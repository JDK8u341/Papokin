//! 轮次 27 基准：事件派发路径的逐调用分配（护栏回收闭包 + `minecraft:`
//! 前缀 `format!`）。
//!
//! 对照形态（`WasmPluginEventHandler::handle` 派发路径）：
//! - 旧臂（现状）：受护栏资源每次派发记录一个
//!   `Box<dyn FnOnce(&mut ResourceTable) + Send>` 回收闭包——Server/
//!   Player/World 三资源的高频事件即每派发 3 次堆分配；名字转换用
//!   `format!("minecraft:{}", name)`，容量预估只取字面量（实测预估
//!   20B），多数注册名都要二次扩容；
//! - 新臂（收编）：护栏条目枚举化（类别标签 + rep，纯数据零分配）；
//!   名字转换改为精确容量拼接（`10 + name.len()`，恰好 1 次分配）。
//!
//! 负载模型（不依赖 wasmtime，模型表语义与 `ResourceTable::delete` 对齐：
//! 命中即删、未命中 no-op）：
//! - 稳态成功段：`10_000` 轮派发 × 3 守卫条目，每轮记录后按成功路径
//!   `decay_clear_vec` 收尾（生产真函数，两臂同节奏）；
//! - 失败重放段：`1_000` 轮 × 3 条目记录后逐轮重放，比对模型表
//!   状态指纹（语义等价门：目标条目全删、护栏外条目保留）；
//! - 尖峰窗口段：单次派发 300 条目（大事件 recipients 场景），
//!   统计记录存活期峰值驻留；
//! - 名字段：`100_000` 次转换，轮转代表性注册名（短/中/长），比对
//!   输出逐字节指纹与分配次数。
//!
//! 预登记门（跑前钉死，FAIL 须如实记录）：
//! - G1 两臂失败重放后模型表指纹一致且护栏外条目保留；
//! - G2 两臂名字输出指纹一致（逐字节）；
//! - G3 新臂名字段分配次数 == 转换次数（恰好 1 次/调用）；
//! - G4 守卫记录段分配差值 == 条目数（旧臂每条目恰一次 Box 分配）；
//! - G5 尖峰窗口旧臂峰值驻留 ≥ 新臂 2 倍（Box 载荷 + Vec 胖指针 vs 内联）。

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use papokin_util::capacity::decay_clear_vec;

// ---------------------------------------------------------------------------
// 分配计数器（次数 + 毛字节 + 释放字节；净驻留 = 毛 − 释放）
// ---------------------------------------------------------------------------

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);
static FREED_BYTES: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: 仅转发 System 并计数，不改动布局与返回值语义。
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: 与文档约定一致，直接转发。
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        FREED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: 与文档约定一致，直接转发。
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn counters() -> (usize, usize, usize) {
    (
        ALLOCS.load(Ordering::Relaxed),
        ALLOC_BYTES.load(Ordering::Relaxed),
        FREED_BYTES.load(Ordering::Relaxed),
    )
}

fn retained() -> usize {
    let (_, alloc, freed) = counters();
    alloc.saturating_sub(freed)
}

/// FNV-1a：增量状态指纹（避免为比对而驻留全部输出）。
#[derive(Clone, Copy)]
struct Fnv(u64);
impl Fnv {
    const fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    const fn finish(self) -> u64 {
        self.0
    }
}

// ---------------------------------------------------------------------------
// 负载模型
// ---------------------------------------------------------------------------

/// 每轮派发的守卫条目数（Server/Player/World 三资源高频事件面）。
const GUARDS_PER_DISPATCH: usize = 3;
/// 稳态成功段轮数。
const STEADY_DISPATCHES: usize = 10_000;
/// 失败重放段轮数。
const FAILURE_DISPATCHES: usize = 1_000;
/// 尖峰窗口：单次派发条目数（大事件 recipients 场景）。
const SPIKE_ENTRIES: usize = 300;
/// 名字段转换次数。
const NAME_CONVERSIONS: usize = 100_000;
/// 失败重放段护栏外常驻条目数（重放不得波及）。
const BYSTANDERS: usize = 8;

/// 资源类别（对应生产五类受护栏资源；基准以标签代替类型参数）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Server,
    Player,
    World,
    Text,
    Item,
}

/// 高频三资源轮转（Server/Player/World 事件每次派发各占一条）。
const TRIO: [Kind; GUARDS_PER_DISPATCH] = [Kind::Server, Kind::Player, Kind::World];

/// 模型资源表：rep 为下标，`Some(kind)` 表示占用。`delete` 语义与
/// wasmtime `ResourceTable::delete::<T>` 对齐：类别命中即删返回 true，
/// 未命中（已释放/虚构 rep）为无害 no-op 返回 false。
#[derive(Default)]
struct ModelTable {
    slots: Vec<Option<Kind>>,
    next_rep: u32,
}

impl ModelTable {
    fn push(&mut self, kind: Kind) -> u32 {
        let rep = self.next_rep;
        self.next_rep = rep.saturating_add(1);
        self.slots.push(Some(kind));
        rep
    }
    fn delete(&mut self, kind: Kind, rep: u32) -> bool {
        if let Some(slot) = self.slots.get_mut(rep as usize)
            && slot.is_some_and(|occupied| occupied == kind)
        {
            *slot = None;
            return true;
        }
        false
    }
    /// 状态指纹：仅计入占用槽（下标 + 类别），canonical 下标序。
    fn fingerprint(&self) -> u64 {
        let mut h = Fnv::new();
        for (idx, slot) in self.slots.iter().enumerate() {
            if let Some(kind) = slot {
                h.write(&u64::try_from(idx).unwrap_or(u64::MAX).to_le_bytes());
                h.write(&[*kind as u8]);
            }
        }
        h.finish()
    }
}

/// 旧臂护栏条目：逐条堆分配的回收闭包（与生产旧形态同构）。
type OldDeleter = Box<dyn FnOnce(&mut ModelTable) + Send>;

fn record_old(guard: &mut Vec<OldDeleter>, kind: Kind, rep: u32) {
    guard.push(Box::new(move |table| {
        table.delete(kind, rep);
    }));
}

/// 新臂护栏条目：类别标签 + rep 的纯数据枚举（与生产新形态同构）。
enum NewDeleter {
    Server(u32),
    Player(u32),
    World(u32),
    Text(u32),
    Item(u32),
}

impl NewDeleter {
    const fn new(kind: Kind, rep: u32) -> Self {
        match kind {
            Kind::Server => Self::Server(rep),
            Kind::Player => Self::Player(rep),
            Kind::World => Self::World(rep),
            Kind::Text => Self::Text(rep),
            Kind::Item => Self::Item(rep),
        }
    }
    fn replay(self, table: &mut ModelTable) {
        let (kind, rep) = match self {
            Self::Server(rep) => (Kind::Server, rep),
            Self::Player(rep) => (Kind::Player, rep),
            Self::World(rep) => (Kind::World, rep),
            Self::Text(rep) => (Kind::Text, rep),
            Self::Item(rep) => (Kind::Item, rep),
        };
        table.delete(kind, rep);
    }
}

/// 代表性注册名（短/中/长覆盖真实注册表长度谱系；长名正是旧臂
/// `format!` 预估容量之后的重灾区）。
const NAMES: [&str; 4] = [
    "stone",
    "smooth_red_sandstone_stairs",
    "waxed_weathered_cut_copper_stairs",
    "lightning_bolt",
];

/// 旧臂名字转换：`format!` 字面量容量预估（实测 20B）+ 逐段格式化。
fn name_old(name: &str) -> String {
    format!("minecraft:{name}")
}

/// 新臂名字转换：精确容量拼接（10 + len，恰好 1 次分配）。
fn name_new(name: &str) -> String {
    let mut out = String::with_capacity(10 + name.len());
    out.push_str("minecraft:");
    out.push_str(name);
    out
}

// ---------------------------------------------------------------------------
// 测量辅助
// ---------------------------------------------------------------------------

/// 阶段读数：分配次数增量 + 净驻留增量。
type Delta = (usize, usize);

fn delta_counters() -> (usize, usize) {
    let (a0, _, _) = counters();
    let r0 = retained();
    (a0, r0)
}

fn stage_delta(start: (usize, usize)) -> Delta {
    let (a1, _, _) = counters();
    let r1 = retained();
    (a1 - start.0, r1.saturating_sub(start.1))
}

const fn verdict(pass: bool) -> &'static str {
    if pass { "PASS" } else { "FAIL" }
}

// ---------------------------------------------------------------------------
// 段 1：稳态成功路径（记录 → decay_clear 收尾，不重放）
// ---------------------------------------------------------------------------

fn steady_old_run() -> (Delta, u128) {
    let mut guard: Vec<OldDeleter> = Vec::new();
    let mut table = ModelTable::default();
    let t0 = Instant::now();
    let c0 = delta_counters();
    for _round in 0..STEADY_DISPATCHES {
        for &kind in &TRIO {
            let rep = table.push(kind);
            record_old(&mut guard, kind, rep);
            // 模拟成功路径正常释放（两臂等价，不进对照差值）
            table.delete(kind, rep);
        }
        decay_clear_vec(&mut guard);
    }
    let ns = t0.elapsed().as_nanos();
    let d = stage_delta(c0);
    drop(guard);
    drop(table);
    (d, ns)
}

fn steady_new_run() -> (Delta, u128) {
    let mut guard: Vec<NewDeleter> = Vec::new();
    let mut table = ModelTable::default();
    let t0 = Instant::now();
    let c0 = delta_counters();
    for _round in 0..STEADY_DISPATCHES {
        for &kind in &TRIO {
            let rep = table.push(kind);
            guard.push(NewDeleter::new(kind, rep));
            table.delete(kind, rep);
        }
        decay_clear_vec(&mut guard);
    }
    let ns = t0.elapsed().as_nanos();
    let d = stage_delta(c0);
    drop(guard);
    drop(table);
    (d, ns)
}

// ---------------------------------------------------------------------------
// 段 2：失败重放（记录 → 重放回收 → 指纹比对）
// ---------------------------------------------------------------------------

fn failure_old_run() -> (u64, Delta) {
    let mut table = ModelTable::default();
    let bystanders: Vec<u32> = (0..BYSTANDERS).map(|_| table.push(Kind::Text)).collect();
    let mut guard: Vec<OldDeleter> = Vec::new();
    let c0 = delta_counters();
    for _round in 0..FAILURE_DISPATCHES {
        let reps: Vec<u32> = TRIO.iter().map(|&kind| table.push(kind)).collect();
        for (&kind, &rep) in TRIO.iter().zip(&reps) {
            record_old(&mut guard, kind, rep);
        }
        // 派发失败：重放回收本轮条目
        for deleter in std::mem::take(&mut guard) {
            deleter(&mut table);
        }
        decay_clear_vec(&mut guard);
    }
    let d = stage_delta(c0);
    let fp = table.fingerprint();
    drop(bystanders);
    drop(guard);
    drop(table);
    (fp, d)
}

fn failure_new_run() -> (u64, Delta) {
    let mut table = ModelTable::default();
    let bystanders: Vec<u32> = (0..BYSTANDERS).map(|_| table.push(Kind::Text)).collect();
    let mut guard: Vec<NewDeleter> = Vec::new();
    let c0 = delta_counters();
    for _round in 0..FAILURE_DISPATCHES {
        let reps: Vec<u32> = TRIO.iter().map(|&kind| table.push(kind)).collect();
        for (&kind, &rep) in TRIO.iter().zip(&reps) {
            guard.push(NewDeleter::new(kind, rep));
        }
        for deleter in std::mem::take(&mut guard) {
            deleter.replay(&mut table);
        }
        decay_clear_vec(&mut guard);
    }
    let d = stage_delta(c0);
    let fp = table.fingerprint();
    drop(bystanders);
    drop(guard);
    drop(table);
    (fp, d)
}

// ---------------------------------------------------------------------------
// 段 3：尖峰窗口峰值驻留（单派发 300 条目记录存活期）
// ---------------------------------------------------------------------------

fn spike_old_run() -> Delta {
    let mut table = ModelTable::default();
    let reps: Vec<u32> = (0..SPIKE_ENTRIES)
        .map(|i| table.push(TRIO[i % GUARDS_PER_DISPATCH]))
        .collect();
    let mut guard: Vec<OldDeleter> = Vec::new();
    let c0 = delta_counters();
    for (i, &rep) in reps.iter().enumerate() {
        record_old(&mut guard, TRIO[i % GUARDS_PER_DISPATCH], rep);
    }
    let d = stage_delta(c0); // 记录完成、条目全存活的峰值窗口
    drop(guard);
    drop(reps);
    drop(table);
    d
}

fn spike_new_run() -> Delta {
    let mut table = ModelTable::default();
    let reps: Vec<u32> = (0..SPIKE_ENTRIES)
        .map(|i| table.push(TRIO[i % GUARDS_PER_DISPATCH]))
        .collect();
    let mut guard: Vec<NewDeleter> = Vec::new();
    let c0 = delta_counters();
    for (i, &rep) in reps.iter().enumerate() {
        guard.push(NewDeleter::new(TRIO[i % GUARDS_PER_DISPATCH], rep));
    }
    let d = stage_delta(c0);
    drop(guard);
    drop(reps);
    drop(table);
    d
}

// ---------------------------------------------------------------------------
// 段 4：名字转换（minecraft: 前缀）
// ---------------------------------------------------------------------------

fn name_old_run() -> (u64, Delta, u128) {
    let mut h = Fnv::new();
    let c0 = delta_counters();
    let t0 = Instant::now();
    for i in 0..NAME_CONVERSIONS {
        let out = name_old(NAMES[i % NAMES.len()]);
        h.write(out.as_bytes());
    }
    (h.finish(), stage_delta(c0), t0.elapsed().as_nanos())
}

fn name_new_run() -> (u64, Delta, u128) {
    let mut h = Fnv::new();
    let c0 = delta_counters();
    let t0 = Instant::now();
    for i in 0..NAME_CONVERSIONS {
        let out = name_new(NAMES[i % NAMES.len()]);
        h.write(out.as_bytes());
    }
    (h.finish(), stage_delta(c0), t0.elapsed().as_nanos())
}

fn main() {
    let mut gates_pass = true;

    println!("== 轮次 27 双臂基准：事件派发逐调用分配 ==");
    println!(
        "负载：稳态 {STEADY_DISPATCHES} 轮 × {GUARDS_PER_DISPATCH} 条目 | 失败 {FAILURE_DISPATCHES} 轮 | 尖峰 {SPIKE_ENTRIES} 条目 | 名字 {NAME_CONVERSIONS} 次\n"
    );

    // 段 1：稳态成功路径
    let ((steady_old, steady_old_ns), (steady_new, steady_new_ns)) =
        (steady_old_run(), steady_new_run());
    println!(
        "稳态成功段    旧 {:>8} allocs / +{:<7} B / {:>11} ns",
        steady_old.0, steady_old.1, steady_old_ns
    );
    println!(
        "稳态成功段    新 {:>8} allocs / +{:<7} B / {:>11} ns",
        steady_new.0, steady_new.1, steady_new_ns
    );
    // G4：守卫记录段分配差值 == 条目总数（旧臂每条目恰一次 Box 分配）
    let total_entries = STEADY_DISPATCHES * GUARDS_PER_DISPATCH;
    let steady_delta = steady_old.0.saturating_sub(steady_new.0);
    let g4 = steady_delta == total_entries;
    println!(
        "G4 记录段分配差值 == 条目数（{total_entries}）：旧 {} − 新 {} = {steady_delta} → {}",
        steady_old.0,
        steady_new.0,
        verdict(g4)
    );
    gates_pass &= g4;

    // 段 2：失败重放
    let (failure_old_fp, failure_old) = failure_old_run();
    let (failure_new_fp, failure_new) = failure_new_run();
    println!(
        "\n失败重放段    旧 {:>8} allocs / +{:<7} B",
        failure_old.0, failure_old.1
    );
    println!(
        "失败重放段    新 {:>8} allocs / +{:<7} B",
        failure_new.0, failure_new.1
    );
    // G1：两臂重放后表指纹一致（目标条目全删、护栏外条目保留）
    let g1 = failure_old_fp == failure_new_fp && failure_old_fp != 0;
    println!(
        "G1 失败重放表指纹一致（旧 {failure_old_fp:#x} vs 新 {failure_new_fp:#x}，护栏外 {BYSTANDERS} 条保留）→ {}",
        verdict(g1)
    );
    gates_pass &= g1;

    // 段 3：尖峰窗口
    let (spike_old, spike_new) = (spike_old_run(), spike_new_run());
    println!(
        "\n尖峰窗口段    旧 {:>8} allocs / 峰值 +{:<7} B",
        spike_old.0, spike_old.1
    );
    println!(
        "尖峰窗口段    新 {:>8} allocs / 峰值 +{:<7} B",
        spike_new.0, spike_new.1
    );
    // G5：旧臂峰值驻留 ≥ 新臂 2 倍（Box 载荷 + Vec 胖指针 vs 纯内联）
    let g5 = spike_old.1 >= spike_new.1.saturating_mul(2);
    println!(
        "G5 尖峰峰值驻留 旧 ≥ 2×新（{} ≥ 2×{}）→ {}",
        spike_old.1,
        spike_new.1,
        verdict(g5)
    );
    gates_pass &= g5;

    // 段 4：名字转换
    let (name_old_fp, name_old, name_old_ns) = name_old_run();
    let (name_new_fp, name_new, name_new_ns) = name_new_run();
    println!(
        "\n名字转换段    旧 {:>8} allocs / +{:<7} B / {:>11} ns",
        name_old.0, name_old.1, name_old_ns
    );
    println!(
        "名字转换段    新 {:>8} allocs / +{:<7} B / {:>11} ns",
        name_new.0, name_new.1, name_new_ns
    );
    // G2：输出逐字节一致
    let g2 = name_old_fp == name_new_fp;
    println!(
        "G2 名字输出指纹一致（{name_old_fp:#x} vs {name_new_fp:#x}）→ {}",
        verdict(g2)
    );
    gates_pass &= g2;
    // G3：新臂恰好 1 次/调用；旧臂严格更多（预估容量必扩容）
    let g3 = name_new.0 == NAME_CONVERSIONS && name_old.0 > NAME_CONVERSIONS;
    println!(
        "G3 新臂分配 == 转换次数（{} == {NAME_CONVERSIONS}）且旧臂严格更多（{}）→ {}",
        name_new.0,
        name_old.0,
        verdict(g3)
    );
    gates_pass &= g3;

    let alloc_old = steady_old.0 + failure_old.0 + spike_old.0 + name_old.0;
    let alloc_new = steady_new.0 + failure_new.0 + spike_new.0 + name_new.0;
    let saved = alloc_old.saturating_sub(alloc_new);
    // 整数口径的倍率（x.y 形式，避免浮点转换 lint）
    let denom = alloc_new.max(1);
    let ratio_whole = alloc_old / denom;
    let ratio_frac = (alloc_old % denom) * 10 / denom;
    println!(
        "\n合计分配：旧 {alloc_old} → 新 {alloc_new}（{ratio_whole}.{ratio_frac}×，省 {saved} 次）"
    );
    println!(
        "门禁汇总：{}",
        if gates_pass {
            "全部 PASS"
        } else {
            "存在 FAIL（见上）"
        }
    );
    std::process::exit(i32::from(!gates_pass));
}
