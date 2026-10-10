//! 轮次 26 基准：实体同步元数据存储形态（HashMap+逐项装箱 → 扁平 Vec + 内联值枚举）。
//!
//! 对照形态（`SynchedEntityData` 的驻留表示与读写路径）：
//! - 旧臂（现状）：每实体一个 `Mutex<HashMap<TrackedData, DataItem>>`，
//!   `DataItem` 内重复存 16B 键 + `Box<dyn ErasedSerializer>`（每项一次
//!   堆分配）+ `canonical_bytes: Vec<u8>`（每项再一次堆分配，且每次
//!   `set` 无论值是否变化都要重新序列化产出以做相等比较）；
//! - 新臂（收编）：`Mutex<Vec<(TrackedData, DataItem)>>` 扁平向量，
//!   值为内联枚举（bool/u8/i8/i32/f32/VarInt/OptionalInt/BlockPos 按
//!   位比较零分配；TextComponent/粒子等大值装箱回退）。追踪项数
//!   为个位到数十条，线性扫描胜过哈希查找且驻留无桶开销。
//!
//! 负载模型（1000 实体 × 12 追踪项，僵尸类典型追踪面）：
//! - 加载阶段：define 全部项，统计分配次数/字节；
//! - 稳态阶段：每实体 3 次变化 set + 3 次同值 no-op set
//!   （旧臂 no-op 也要分配 canonical Vec；新臂零分配零序列化）；
//! - 打包阶段：两客户端版本 `pack_dirty`，统计分配并比对
//!   两臂输出字节（语义等价指纹门）；
//! - 卸载阶段：drop 全部实体，验证净驻留归零。
//!
//! 观测：分配计数器（次数+毛字节+释放字节，净驻留 = 毛 − 释放）、
//! 逐版本打包输出逐字节相等。

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use bytes::BufMut;
use papokin_data::meta_data_type::MetaDataType;
use papokin_data::tracked_data::{TrackedData, TrackedId, entity, living_entity, mob, player};
use papokin_protocol::codec::var_int::VarInt;
use papokin_protocol::java::client::play::{Metadata, MetadataSerializer};
use papokin_util::version::JavaMinecraftVersion;

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

#[derive(Clone, Copy)]
struct Delta {
    allocs: usize,
    bytes: usize,
    retained: usize,
}

fn measure(stage: &str, mut f: impl FnMut()) -> Delta {
    let (a0, b0, _) = counters();
    let r0 = retained();
    f();
    let (a1, b1, _) = counters();
    let r1 = retained();
    let d = Delta {
        allocs: a1 - a0,
        bytes: b1 - b0,
        retained: r1.saturating_sub(r0),
    };
    let (allocs, bytes, ret) = (d.allocs, d.bytes, d.retained);
    println!("{stage:<10} 分配 {allocs:>9} 次 {bytes:>11} B    净驻留 +{ret:>10} B");
    d
}

// ---------------------------------------------------------------------------
// 负载模型
// ---------------------------------------------------------------------------

const ENTITIES: usize = 1000;

/// 每隔多少个实体有 1 个带自定义名字（装箱族占比 10%）。
const TEXT_ENTITY_INTERVAL: usize = 10;

const V26_3: JavaMinecraftVersion = JavaMinecraftVersion::V_26_3;

/// 每实体的追踪面（僵尸类典型：基础 6 + 生物 5 + mob 1）。
/// (`TrackedData`, 值生成函数)；`i` 为实体序号（用于让值有差异）。
#[derive(Clone)]
enum ModelValue {
    Bool(bool),
    U8(u8),
    I8(i8),
    F32(f32),
    VarInt(i32),
    /// 装箱族（自定义名字 / 药水粒子等大值）
    Text(Option<&'static str>),
}

fn tracked_face() -> Vec<(TrackedData, ModelValue)> {
    vec![
        (entity::DATA_SHARED_FLAGS_ID, ModelValue::U8(0)),
        (entity::DATA_AIR_SUPPLY_ID, ModelValue::VarInt(300)),
        (entity::DATA_CUSTOM_NAME_VISIBLE, ModelValue::Bool(false)),
        (entity::DATA_SILENT, ModelValue::Bool(false)),
        (entity::DATA_NO_GRAVITY, ModelValue::Bool(false)),
        (entity::DATA_POSE, ModelValue::VarInt(0)),
        (entity::DATA_TICKS_FROZEN, ModelValue::VarInt(0)),
        (
            entity::DATA_CUSTOM_NAME,
            ModelValue::Text(Some("等级观察者")),
        ),
        (living_entity::DATA_LIVING_ENTITY_FLAGS, ModelValue::I8(0)),
        (living_entity::DATA_HEALTH_ID, ModelValue::F32(20.0)),
        (player::DATA_PLAYER_ABSORPTION_ID, ModelValue::F32(0.0)),
        (living_entity::EFFECT_AMBIENCE_ID, ModelValue::Bool(false)),
        (mob::DATA_MOB_FLAGS_ID, ModelValue::I8(0)),
    ]
}

fn as_text(v: &ModelValue) -> Option<papokin_util::text::TextComponent> {
    match v {
        ModelValue::Text(Some(s)) => Some(papokin_util::text::TextComponent::text(*s)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 旧臂：HashMap + 逐项装箱（复刻生产旧形态）
// ---------------------------------------------------------------------------

trait ErasedSerializer2 {
    fn write(
        &self,
        index: TrackedId,
        r#type: MetaDataType,
        writer: &mut dyn std::io::Write,
        version: JavaMinecraftVersion,
    ) -> Result<(), papokin_protocol::ser::WritingError>;
    fn write_canonical(&self, index: TrackedId, r#type: MetaDataType) -> Vec<u8>;
}

struct Holder<T> {
    value: T,
}

impl<T: MetadataSerializer + Clone + Send + Sync + 'static> ErasedSerializer2 for Holder<T> {
    fn write(
        &self,
        index: TrackedId,
        r#type: MetaDataType,
        writer: &mut dyn std::io::Write,
        version: JavaMinecraftVersion,
    ) -> Result<(), papokin_protocol::ser::WritingError> {
        Metadata::new_raw(index, r#type, &self.value).write(writer, &version)
    }
    fn write_canonical(&self, index: TrackedId, r#type: MetaDataType) -> Vec<u8> {
        let mut buf = Vec::new();
        let _ = Metadata::new_raw(index, r#type, &self.value).write(&mut buf, &V26_3);
        buf
    }
}

struct OldDataItem {
    tracked: TrackedData,
    serializer: Box<dyn ErasedSerializer2>,
    canonical_bytes: Vec<u8>,
    dirty: bool,
    is_default: bool,
}

struct OldSynched {
    items: Mutex<HashMap<TrackedData, OldDataItem>>,
    is_dirty: AtomicBool,
}

impl OldSynched {
    fn new() -> Self {
        Self {
            items: Mutex::new(HashMap::new()),
            is_dirty: AtomicBool::new(false),
        }
    }

    fn define<T: MetadataSerializer + Clone + Send + Sync + 'static>(
        &self,
        tracked: TrackedData,
        value: T,
    ) {
        let holder = Holder { value };
        let canonical_bytes = holder.write_canonical(tracked.id, tracked.r#type);
        self.items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                tracked,
                OldDataItem {
                    tracked,
                    serializer: Box::new(holder),
                    canonical_bytes,
                    dirty: false,
                    is_default: true,
                },
            );
    }

    fn set<T: MetadataSerializer + Clone + Send + Sync + 'static>(
        &self,
        tracked: TrackedData,
        value: T,
    ) -> bool {
        let holder = Holder { value };
        let new_canonical = holder.write_canonical(tracked.id, tracked.r#type);
        let mut items = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(item) = items.get_mut(&tracked) {
            if item.canonical_bytes == new_canonical {
                return false;
            }
            item.canonical_bytes = new_canonical;
            item.serializer = Box::new(holder);
            item.dirty = true;
            item.is_default = false;
        } else {
            items.insert(
                tracked,
                OldDataItem {
                    tracked,
                    serializer: Box::new(holder),
                    canonical_bytes: new_canonical,
                    dirty: true,
                    is_default: false,
                },
            );
        }
        self.is_dirty.store(true, Ordering::Release);
        true
    }

    fn pack_dirty_for_versions(
        &self,
        versions: &[JavaMinecraftVersion],
    ) -> Vec<(JavaMinecraftVersion, Box<[u8]>)> {
        let mut items = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out = Vec::new();
        for version in versions {
            let mut buf = Vec::new();
            let mut has_any = false;
            // 生产旧形态按 HashMap 迭代序（不定序，客户端按条目自带索引
            // 解码）；为跨臂逐字节比对，此处按版本索引排序为规范序。
            let mut sorted: Vec<&OldDataItem> = items.values().filter(|item| item.dirty).collect();
            sorted.sort_by_key(|item| item.tracked.id.get(version));
            for item in sorted {
                {
                    let before_len = buf.len();
                    if item
                        .serializer
                        .write(item.tracked.id, item.tracked.r#type, &mut buf, *version)
                        .is_ok()
                        && buf.len() > before_len
                    {
                        has_any = true;
                    }
                }
            }
            if has_any {
                buf.put_u8(255);
                out.push((*version, buf.into_boxed_slice()));
            }
        }
        for item in items.values_mut() {
            item.dirty = false;
        }
        self.is_dirty.store(false, Ordering::Release);
        out
    }
}

// ---------------------------------------------------------------------------
// 新臂：扁平 Vec + 内联值枚举（复刻收编形态）
// ---------------------------------------------------------------------------

struct BoxedValue2 {
    serializer: Box<dyn ErasedSerializer2>,
    canonical_bytes: Vec<u8>,
}

enum NewValue {
    Bool(bool),
    U8(u8),
    I8(i8),
    F32(f32),
    VarInt(VarInt),
    Boxed(Box<BoxedValue2>),
}

impl NewValue {
    fn equivalent(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::U8(a), Self::U8(b)) => a == b,
            (Self::I8(a), Self::I8(b)) => a == b,
            (Self::F32(a), Self::F32(b)) => a.to_bits() == b.to_bits(),
            (Self::VarInt(a), Self::VarInt(b)) => a == b,
            (Self::Boxed(a), Self::Boxed(b)) => a.canonical_bytes == b.canonical_bytes,
            _ => false,
        }
    }

    fn write(
        &self,
        index: TrackedId,
        r#type: MetaDataType,
        writer: &mut dyn std::io::Write,
        version: JavaMinecraftVersion,
    ) -> Result<(), papokin_protocol::ser::WritingError> {
        match self {
            Self::Bool(v) => Metadata::new_raw(index, r#type, v).write(writer, &version),
            Self::U8(v) => Metadata::new_raw(index, r#type, v).write(writer, &version),
            Self::I8(v) => Metadata::new_raw(index, r#type, v).write(writer, &version),
            Self::F32(v) => Metadata::new_raw(index, r#type, v).write(writer, &version),
            Self::VarInt(v) => Metadata::new_raw(index, r#type, v).write(writer, &version),
            Self::Boxed(b) => b.serializer.write(index, r#type, writer, version),
        }
    }
}

fn to_new_value(v: &ModelValue, tracked: TrackedData) -> NewValue {
    match v {
        ModelValue::Bool(b) => NewValue::Bool(*b),
        ModelValue::U8(u) => NewValue::U8(*u),
        ModelValue::I8(i) => NewValue::I8(*i),
        ModelValue::F32(f) => NewValue::F32(*f),
        ModelValue::VarInt(i) => NewValue::VarInt(VarInt(*i)),
        ModelValue::Text(_) => {
            let holder = Holder { value: as_text(v) };
            let canonical_bytes = holder.write_canonical(tracked.id, tracked.r#type);
            NewValue::Boxed(Box::new(BoxedValue2 {
                serializer: Box::new(holder),
                canonical_bytes,
            }))
        }
    }
}

struct NewDataItem {
    value: NewValue,
    dirty: bool,
    is_default: bool,
}

struct NewSynched {
    items: Mutex<Vec<(TrackedData, NewDataItem)>>,
    is_dirty: AtomicBool,
}

impl NewSynched {
    const fn new() -> Self {
        Self {
            items: Mutex::new(Vec::new()),
            is_dirty: AtomicBool::new(false),
        }
    }

    fn define(&self, tracked: TrackedData, value: NewValue) {
        let mut items = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match items.iter_mut().find(|(key, _)| *key == tracked) {
            Some((_, item)) => {
                *item = NewDataItem {
                    value,
                    dirty: false,
                    is_default: true,
                };
            }
            None => items.push((
                tracked,
                NewDataItem {
                    value,
                    dirty: false,
                    is_default: true,
                },
            )),
        }
    }

    fn set(&self, tracked: TrackedData, value: NewValue) -> bool {
        let mut items = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, item)) = items.iter_mut().find(|(key, _)| *key == tracked) {
            if item.value.equivalent(&value) {
                return false;
            }
            item.value = value;
            item.dirty = true;
            item.is_default = false;
        } else {
            items.push((
                tracked,
                NewDataItem {
                    value,
                    dirty: true,
                    is_default: false,
                },
            ));
        }
        self.is_dirty.store(true, Ordering::Release);
        true
    }

    fn pack_dirty_for_versions(
        &self,
        versions: &[JavaMinecraftVersion],
    ) -> Vec<(JavaMinecraftVersion, Box<[u8]>)> {
        let mut items = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out = Vec::new();
        for version in versions {
            let mut buf = Vec::new();
            let mut has_any = false;
            // 与旧臂同规范序（按版本索引排序）以做逐字节比对。
            let mut sorted: Vec<&(TrackedData, NewDataItem)> =
                items.iter().filter(|(_, item)| item.dirty).collect();
            sorted.sort_by_key(|(tracked, _)| tracked.id.get(version));
            for (tracked, item) in sorted {
                let before_len = buf.len();
                if item
                    .value
                    .write(tracked.id, tracked.r#type, &mut buf, *version)
                    .is_ok()
                    && buf.len() > before_len
                {
                    has_any = true;
                }
            }
            if has_any {
                buf.put_u8(255);
                out.push((*version, buf.into_boxed_slice()));
            }
        }
        for (_, item) in items.iter_mut() {
            item.dirty = false;
        }
        self.is_dirty.store(false, Ordering::Release);
        out
    }
}

// ---------------------------------------------------------------------------
// 双臂驱动
// ---------------------------------------------------------------------------

const VERSIONS: [JavaMinecraftVersion; 2] = [
    JavaMinecraftVersion::V_1_21_11,
    JavaMinecraftVersion::V_26_3,
];

/// 把全部打包输出折叠为 64 位指纹（FNV-1a），用于跨臂语义比对。
fn fold_fingerprint(packs: &[(JavaMinecraftVersion, Box<[u8]>)]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (version, buf) in packs {
        for &b in &**buf {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let v = *version as u64;
        for shift in [0, 8, 16, 24, 32, 40, 48, 56] {
            h ^= (v >> shift) & 0xFF;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

type PackSample = (JavaMinecraftVersion, Box<[u8]>);

struct ArmReport {
    load: Delta,
    steady: Delta,
    pack: Delta,
    pack_fingerprint: u64,
    per_entity_bytes: usize,
}

fn main() {
    println!(
        "轮次 26：实体同步元数据存储（{ENTITIES} 实体 × 12 追踪项）
"
    );
    let face = tracked_face();

    // ---- 旧臂 ----
    println!("=== 旧臂（HashMap + 逐项装箱） ===");
    let (old, _old_samples) = run_old(&face);
    println!("    每实体加载驻留 ≈ {} B", old.per_entity_bytes);

    println!();

    // ---- 新臂 ----
    println!("=== 新臂（扁平 Vec + 内联值） ===");
    let (new, _new_samples) = run_new(&face);
    println!("    每实体加载驻留 ≈ {} B", new.per_entity_bytes);

    // ---- 汇总 ----
    println!(
        "
=== 对比 ==="
    );
    println!(
        "{:<10} {:>12} {:>14} {:>14}   倍率",
        "阶段", "指标", "旧臂", "新臂"
    );
    let rows = [
        ("加载", &old.load, &new.load),
        ("稳态set", &old.steady, &new.steady),
        ("打包", &old.pack, &new.pack),
    ];
    for (name, o, n) in &rows {
        let ratio_a = o.allocs as f64 / n.allocs.max(1) as f64;
        let ratio_b = o.bytes as f64 / n.bytes.max(1) as f64;
        let (cnt, byt) = ("分配次数", "分配字节");
        println!(
            "{name:<10} {cnt} {:>14} {:>14}   {ratio_a:.1}×",
            o.allocs, n.allocs
        );
        println!(
            "{name:<10} {byt} {:>14} {:>14}   {ratio_b:.1}×",
            o.bytes, n.bytes
        );
    }
    println!(
        "{:<10} {:>12} {:>14} {:>14}   {:.2}×",
        "加载驻留",
        "B/实体",
        old.per_entity_bytes,
        new.per_entity_bytes,
        old.per_entity_bytes as f64 / new.per_entity_bytes.max(1) as f64
    );

    // 指纹门：两臂逐版本打包输出折叠指纹必须全等
    let old_fp = old.pack_fingerprint;
    let new_fp = new.pack_fingerprint;
    println!(
        "
指纹门（两版本 × 全实体打包折叠）：旧 0x{old_fp:016x} vs 新 0x{new_fp:016x} → {}",
        if old_fp == new_fp {
            "PASS 全等"
        } else {
            "FAIL"
        }
    );
    assert_eq!(old_fp, new_fp, "两臂打包输出必须逐字节一致");
}

/// 旧臂：返回各阶段增量与「实体 0 + 实体 999」的打包样本（跨臂比对用）。
fn run_old(face: &[(TrackedData, ModelValue)]) -> (ArmReport, Vec<PackSample>) {
    let mut all: Vec<Box<OldSynched>> = Vec::new();
    let load = measure("加载", || {
        for i in 0..ENTITIES {
            let s = Box::new(OldSynched::new());
            define_face_old(&s, face, i);
            all.push(s);
        }
    });
    let per_entity = load.retained / ENTITIES;

    let steady = measure("稳态set", || {
        for s in &all {
            s.set(living_entity::DATA_HEALTH_ID, 19.5f32);
            s.set(entity::DATA_POSE, VarInt(1));
            s.set(living_entity::DATA_LIVING_ENTITY_FLAGS, 1i8);
            // 同值 no-op：旧臂每次仍分配 canonical Vec
            s.set(entity::DATA_SILENT, false);
            s.set(entity::DATA_NO_GRAVITY, false);
            s.set(living_entity::EFFECT_AMBIENCE_ID, false);
        }
    });

    let mut packs_all: Vec<PackSample> = Vec::new();
    let pack = measure("打包", || {
        for s in &all {
            packs_all.extend(s.pack_dirty_for_versions(&VERSIONS));
        }
    });

    measure("卸载", || {
        all.clear();
    });

    // 样本取首尾实体（其余已在指纹中聚合）
    let samples: Vec<PackSample> = packs_all
        .iter()
        .take(2)
        .chain(packs_all.iter().skip(packs_all.len().saturating_sub(2)))
        .cloned()
        .collect();
    let fingerprint = fold_fingerprint(&packs_all);

    (
        ArmReport {
            load,
            steady,
            pack,
            pack_fingerprint: fingerprint,
            per_entity_bytes: per_entity,
        },
        samples,
    )
}

fn define_face_old(s: &OldSynched, face: &[(TrackedData, ModelValue)], i: usize) {
    for (tracked, value) in face {
        if matches!(value, ModelValue::Text(_)) && !i.is_multiple_of(TEXT_ENTITY_INTERVAL) {
            continue;
        }
        match value {
            ModelValue::Bool(b) => s.define(*tracked, *b),
            ModelValue::U8(u) => s.define(*tracked, *u),
            ModelValue::I8(v) => s.define(*tracked, *v),
            ModelValue::F32(f) => s.define(*tracked, *f),
            ModelValue::VarInt(v) => s.define(*tracked, VarInt(*v)),
            ModelValue::Text(_) => s.define(*tracked, as_text(value)),
        }
    }
}

/// 新臂：同负载模型（Text 项同占比走装箱回退）。
fn run_new(face: &[(TrackedData, ModelValue)]) -> (ArmReport, Vec<PackSample>) {
    let mut all: Vec<Box<NewSynched>> = Vec::new();
    let load = measure("加载", || {
        for i in 0..ENTITIES {
            let s = Box::new(NewSynched::new());
            for (tracked, value) in face {
                if matches!(value, ModelValue::Text(_)) && !i.is_multiple_of(TEXT_ENTITY_INTERVAL) {
                    continue;
                }
                s.define(*tracked, to_new_value(value, *tracked));
            }
            all.push(s);
        }
    });
    let per_entity = load.retained / ENTITIES;

    let steady = measure("稳态set", || {
        for s in &all {
            s.set(living_entity::DATA_HEALTH_ID, NewValue::F32(19.5));
            s.set(entity::DATA_POSE, NewValue::VarInt(VarInt(1)));
            s.set(living_entity::DATA_LIVING_ENTITY_FLAGS, NewValue::I8(1));
            // 同值 no-op：新臂零分配
            s.set(entity::DATA_SILENT, NewValue::Bool(false));
            s.set(entity::DATA_NO_GRAVITY, NewValue::Bool(false));
            s.set(living_entity::EFFECT_AMBIENCE_ID, NewValue::Bool(false));
        }
    });

    let mut packs_all: Vec<PackSample> = Vec::new();
    let pack = measure("打包", || {
        for s in &all {
            packs_all.extend(s.pack_dirty_for_versions(&VERSIONS));
        }
    });

    measure("卸载", || {
        all.clear();
    });

    let samples: Vec<PackSample> = packs_all
        .iter()
        .take(2)
        .chain(packs_all.iter().skip(packs_all.len().saturating_sub(2)))
        .cloned()
        .collect();
    let fingerprint = fold_fingerprint(&packs_all);

    (
        ArmReport {
            load,
            steady,
            pack,
            pack_fingerprint: fingerprint,
            per_entity_bytes: per_entity,
        },
        samples,
    )
}
