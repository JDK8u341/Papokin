//! 轮次 30 基准：容器内容同步全屏扫描的逐槽克隆消除
//! （双份深克隆 → 引用比较 + 条件内克隆）。
//!
//! 对照形态（`ScreenHandler::send_content_updates` /
//! `update_to_client`，玩家屏幕 46+ 槽、拾取/切屏/手持变化等
//! 高频路径）：
//! - 旧臂（现状）：每槽先 `get_cloned_stack()`（组件补丁深克隆
//!   一次），再把 `stack.clone()` 按值传给 `update_tracked_slot`
//!   （第二次深克隆）——而绝大多数槽位未变化，两次克隆只用于
//!   `are_equal`/`is_in_sync` 比较后即丢弃；另有逐槽 `Arc` 克隆；
//! - 新臂（收编）：槽位方法收 `&ItemStack`，克隆延迟到确认
//!   变化之后（未变化槽位只剩一次 `get_cloned_stack` 读取，
//!   变化槽位克隆次数不变）。
//!
//! 负载模型（46 槽玩家屏幕量级：12 槽带 3-5 组件补丁、26 槽
//! 裸物品、8 槽空；`ModelStack.patch` 的 `Vec<(u32, Box<[u8]>)>`
//! 克隆成本与生产 `Vec<(DataComponent, Option<Box<dyn ...>>)>`
//! 同构——Vec 分配 + 逐条目堆分配深拷贝）：
//! - 稳态段：10 000 次全屏扫描，零槽位变化（游戏内大多数
//!   `send_content_updates` 调用的形态——拾取/装备/切屏后的
//!   例行复查）；
//! - 扰动段：10 000 次扫描，每次扫描前轮转修改一个槽位
//!   （数量/组件交替），两臂同付变更成本、差值即扫描面差异。
//!
//! 预登记门（跑前钉死，FAIL 须如实记录）：
//! - G1 稳态段两臂触发序列指纹一致（应为空序列的固定折叠）；
//! - G2 扰动段两臂触发序列指纹一致（变化检测与重发序列相同）；
//! - G3 稳态段旧臂分配次数 ≥ 2×新臂且新臂非零（未变化有补丁
//!   槽旧臂付两次克隆、新臂付一次，结构性 2×比值）；
//! - G4 扰动段两臂触发序列非空（负载有效性：变更确被检测）；
//! - G5 扰动段旧臂分配次数严格大于新臂（变化槽位克隆不变、
//!   未变化槽位省一半，扰动段差值仍为正）。

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

// ---------------------------------------------------------------------------
// 分配计数器（次数 + 毛字节 + 释放字节）
// ---------------------------------------------------------------------------

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static FREED_BYTES: AtomicU64 = AtomicU64::new(0);

struct Counting;

// SAFETY: 仅转发 System 并计数，不改动布局与返回值语义。
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(
            u64::try_from(layout.size()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        // SAFETY: 与文档约定一致，直接转发。
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        FREED_BYTES.fetch_add(
            u64::try_from(layout.size()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        // SAFETY: 与文档约定一致，直接转发。
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// 阶段读数：分配次数增量 + 毛字节增量。
type Delta = (u64, u64);

fn snapshot() -> (u64, u64) {
    (
        ALLOCS.load(Ordering::Relaxed),
        ALLOC_BYTES.load(Ordering::Relaxed),
    )
}

fn delta_since(start: (u64, u64)) -> Delta {
    let end = snapshot();
    (end.0 - start.0, end.1 - start.1)
}

const fn verdict(pass: bool) -> &'static str {
    if pass { "PASS" } else { "FAIL" }
}

// ---------------------------------------------------------------------------
// 负载模型
// ---------------------------------------------------------------------------

/// 屏幕槽位数（玩家屏幕量级）。
const N_SLOTS: usize = 46;
/// 带组件补丁的槽位数（工具/武器/盔甲类，克隆即逐条目堆分配）。
const PATCHED_SLOTS: usize = 12;
/// 每段扫描次数。
const N_SCANS: u64 = 10_000;

/// 模型物品堆：`patch` 的克隆成本与生产 `ItemStack.patch`
/// （`Vec<(DataComponent, Option<Box<dyn DataComponentImpl>>)>`）
/// 同构——Vec 重分配 + 逐条目深拷贝。
#[derive(Clone)]
struct ModelStack {
    count: u8,
    id: u32,
    patch: Vec<(u32, Box<[u8]>)>,
}

impl ModelStack {
    const fn bare(count: u8, id: u32) -> Self {
        Self {
            count,
            id,
            patch: Vec::new(),
        }
    }

    fn with_patch(count: u8, id: u32, components: usize) -> Self {
        Self {
            count,
            id,
            patch: (0..components)
                .map(|c| (c as u32, vec![7u8; 8 + c].into_boxed_slice()))
                .collect(),
        }
    }

    fn are_equal(&self, other: &Self) -> bool {
        self.count == other.count
            && self.id == other.id
            && self.patch.len() == other.patch.len()
            && self
                .patch
                .iter()
                .zip(&other.patch)
                .all(|(a, b)| a.0 == b.0 && a.1 == b.1)
    }
}

/// 模型屏幕：槽位 + 跟踪态（复刻 `tracked_stacks` 与
/// `previous_tracked_stacks` 的比较语义）。
struct ModelScreen {
    slots: Vec<ModelStack>,
    tracked_stacks: Vec<ModelStack>,
    previous_tracked_stacks: Vec<Option<ModelStack>>,
}

impl ModelScreen {
    fn new(slots: Vec<ModelStack>) -> Self {
        let n = slots.len();
        Self {
            slots,
            tracked_stacks: (0..n).map(|_| ModelStack::bare(0, 0)).collect(),
            previous_tracked_stacks: (0..n).map(|_| None).collect(),
        }
    }

    /// 触发指纹折叠：返回本轮扫描的更新序列指纹。
    fn fingerprint(slot: usize, stack: &ModelStack) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        h ^= u64::from(slot as u32);
        h = h.wrapping_mul(0x1000_0000_01B3);
        h ^= u64::from(stack.count);
        h = h.wrapping_mul(0x1000_0000_01B3);
        h ^= u64::from(stack.id);
        h = h.wrapping_mul(0x1000_0000_01B3);
        h
    }
}

/// 模拟槽位读取（`get_cloned_stack`：按值返回，即一次深克隆）。
fn get_cloned_stack(screen: &ModelScreen, slot: usize) -> ModelStack {
    screen.slots[slot].clone()
}

// ---------------------------------------------------------------------------
// 旧臂：按值传参（每槽两次深克隆）
// ---------------------------------------------------------------------------

// 旧臂刻意按值收参并在比较分支内克隆——复刻生产旧形态的
// 浪费模式（比较只需引用却付了传参克隆），非坏代码。
#[allow(clippy::needless_pass_by_value, clippy::redundant_clone)]
fn update_tracked_slot_old(screen: &mut ModelScreen, slot: usize, stack: ModelStack) {
    if !screen.tracked_stacks[slot].are_equal(&stack) {
        screen.tracked_stacks[slot] = stack.clone();
    }
}

#[allow(clippy::needless_pass_by_value, clippy::redundant_clone)]
fn check_slot_updates_old(screen: &mut ModelScreen, slot: usize, stack: ModelStack) {
    let Some(prev) = &screen.previous_tracked_stacks[slot] else {
        screen.previous_tracked_stacks[slot] = Some(stack.clone());
        return;
    };
    if !prev.are_equal(&stack) {
        screen.previous_tracked_stacks[slot] = Some(stack.clone());
    }
}

fn scan_old(screen: &mut ModelScreen) -> u64 {
    let mut fp = 0xcbf2_9ce4_8422_2325u64;
    for i in 0..screen.slots.len() {
        let stack = get_cloned_stack(screen, i);
        let before = screen.tracked_stacks[i].are_equal(&stack);
        update_tracked_slot_old(screen, i, stack.clone());
        if !before {
            fp ^= ModelScreen::fingerprint(i, &stack);
        }
        check_slot_updates_old(screen, i, stack);
    }
    fp
}

// ---------------------------------------------------------------------------
// 新臂：引用传参（未变化槽位只付一次读取克隆）
// ---------------------------------------------------------------------------

fn update_tracked_slot_new(screen: &mut ModelScreen, slot: usize, stack: &ModelStack) {
    if !screen.tracked_stacks[slot].are_equal(stack) {
        screen.tracked_stacks[slot] = stack.clone();
    }
}

fn check_slot_updates_new(screen: &mut ModelScreen, slot: usize, stack: &ModelStack) {
    let Some(prev) = &screen.previous_tracked_stacks[slot] else {
        screen.previous_tracked_stacks[slot] = Some(stack.clone());
        return;
    };
    if !prev.are_equal(stack) {
        screen.previous_tracked_stacks[slot] = Some(stack.clone());
    }
}

fn scan_new(screen: &mut ModelScreen) -> u64 {
    let mut fp = 0xcbf2_9ce4_8422_2325u64;
    for i in 0..screen.slots.len() {
        let stack = get_cloned_stack(screen, i);
        let before = screen.tracked_stacks[i].are_equal(&stack);
        update_tracked_slot_new(screen, i, &stack);
        if !before {
            fp ^= ModelScreen::fingerprint(i, &stack);
        }
        check_slot_updates_new(screen, i, &stack);
    }
    fp
}

// ---------------------------------------------------------------------------
// 段执行
// ---------------------------------------------------------------------------

/// 段读数：结果指纹 + 计量段分配 + 总耗时。
struct Report {
    fp: u64,
    triggers: u64,
    delta: Delta,
    ns: u128,
}

/// 构建两臂同源的初始槽位表。
fn build_slots() -> Vec<ModelStack> {
    let mut slots = Vec::with_capacity(N_SLOTS);
    for i in 0..N_SLOTS {
        let slot = if i < PATCHED_SLOTS {
            ModelStack::with_patch(
                u8::try_from(1 + i % 3).unwrap_or(1),
                100 + i as u32,
                3 + i % 3,
            )
        } else if i % 3 == 0 {
            ModelStack::bare(0, 0)
        } else {
            ModelStack::bare(u8::try_from(8 + i % 32).unwrap_or(8), 500 + i as u32)
        };
        slots.push(slot);
    }
    slots
}

/// 扰动脚本：第 k 次扰动修改的槽位与新值（两臂同源）。
fn perturbation(k: u64, screen: &mut ModelScreen) {
    let slot = (k as usize) % PATCHED_SLOTS;
    let variant = (k / PATCHED_SLOTS as u64) % 4;
    let base = &screen.slots[slot];
    let mut next = base.clone();
    match variant {
        0 => next.count = next.count.wrapping_add(1).max(1),
        1 => next.count = next.count.saturating_sub(1).max(1),
        2 => next.patch.push((99, vec![1u8; 12].into_boxed_slice())),
        _ => {
            next.patch.pop();
            if next.patch.is_empty() {
                next.patch.push((7, vec![3u8; 9].into_boxed_slice()));
            }
        }
    }
    screen.slots[slot] = next;
}

fn steady_segment(scan: fn(&mut ModelScreen) -> u64) -> Report {
    let mut screen = ModelScreen::new(build_slots());
    // 预热一次使跟踪态就位（初次全量同步不计入稳态计量）
    black_box(scan(&mut screen));
    let mut fp = 0xcbf2_9ce4_8422_2325u64;
    let t0 = Instant::now();
    let start = snapshot();
    for _ in 0..N_SCANS {
        fp ^= scan(&mut screen);
    }
    let delta = delta_since(start);
    Report {
        fp,
        triggers: 0,
        delta,
        ns: t0.elapsed().as_nanos(),
    }
}

fn perturbed_segment(scan: fn(&mut ModelScreen) -> u64) -> Report {
    let mut screen = ModelScreen::new(build_slots());
    black_box(scan(&mut screen));
    let mut fp = 0xcbf2_9ce4_8422_2325u64;
    let mut triggers = 0u64;
    let t0 = Instant::now();
    let start = snapshot();
    for k in 0..N_SCANS {
        perturbation(k, &mut screen);
        let scan_fp = scan(&mut screen);
        if scan_fp != 0xcbf2_9ce4_8422_2325u64 {
            triggers += 1;
        }
        fp ^= scan_fp;
    }
    let delta = delta_since(start);
    Report {
        fp,
        triggers,
        delta,
        ns: t0.elapsed().as_nanos(),
    }
}

fn print_segment(title: &str, old: &Report, new: &Report) {
    println!(
        "{title} 段 旧 {:>8} allocs / {:>10} B / {:>10} ns",
        old.delta.0, old.delta.1, old.ns
    );
    println!(
        "{title} 段 新 {:>8} allocs / {:>10} B / {:>10} ns",
        new.delta.0, new.delta.1, new.ns
    );
}

fn main() {
    let mut gates_pass = true;

    println!("== 轮次 30 双臂基准：容器内容同步全屏扫描克隆消除 ==");
    println!(
        "负载：{N_SLOTS} 槽（{PATCHED_SLOTS} 带补丁），每段 {N_SCANS} 次扫描（稳态零变化 / 扰动每拍一变）\n"
    );

    // 健全性：全局分配器必须在 main 之前已计数
    let boot = snapshot();
    println!("启动健全性：{} allocs / {} B", boot.0, boot.1);

    // 段 1：稳态（零变化）
    let old = steady_segment(scan_old);
    let new = steady_segment(scan_new);
    print_segment("稳态", &old, &new);
    let g1 = old.fp == new.fp;
    println!(
        "G1 稳态段触发序列指纹一致（{:#x} vs {:#x}）→ {}",
        old.fp,
        new.fp,
        verdict(g1)
    );
    gates_pass &= g1;
    let g3 = new.delta.0 > 0 && old.delta.0 >= 2 * new.delta.0;
    println!(
        "G3 稳态段旧臂分配次数 ≥ 2×新臂（{} vs {} = {:.2}×）→ {}",
        old.delta.0,
        new.delta.0,
        old.delta.0 as f64 / new.delta.0 as f64,
        verdict(g3)
    );
    gates_pass &= g3;

    // 段 2：扰动（每拍一变，两臂同源变更）
    let old = perturbed_segment(scan_old);
    let new = perturbed_segment(scan_new);
    print_segment("扰动", &old, &new);
    let g2 = old.fp == new.fp;
    println!(
        "G2 扰动段触发序列指纹一致（{:#x} vs {:#x}）→ {}",
        old.fp,
        new.fp,
        verdict(g2)
    );
    gates_pass &= g2;
    let g4 = old.triggers > 0 && new.triggers > 0;
    println!(
        "G4 扰动段触发序列非空（旧 {} 拍 / 新 {} 拍非空折叠）→ {}",
        old.triggers,
        new.triggers,
        verdict(g4)
    );
    gates_pass &= g4;
    let g5 = old.delta.0 > new.delta.0;
    println!(
        "G5 扰动段旧臂分配次数 > 新臂（{} vs {}）→ {}",
        old.delta.0,
        new.delta.0,
        verdict(g5)
    );
    gates_pass &= g5;

    println!(
        "\n门禁汇总：{}",
        if gates_pass {
            "全部 PASS"
        } else {
            "存在 FAIL（见上）"
        }
    );
    std::process::exit(i32::from(!gates_pass));
}
