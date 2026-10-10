//! 轮次 29 基准：实体/玩家空间查询族的容器与归约策略
//! （`HashMap` 收集 + 全量再筛选 → `Vec` 收集 / 单遍归约）。
//!
//! 对照形态（`World::get_nearby_entities` / `get_nearest_entity` /
//! `get_nearest_player`，AI 目标选择、刷怪规则、钟共振等高频面）：
//! - 旧臂（现状）：`get_nearby_entities` 把已天然去重的候选（区块
//!   索引按 `Arc::ptr_eq` 去重、全表回退每实体一项）再灌进
//!   `HashMap<uuid, entity>`（桶增长 + 逐条目哈希），而全部消费方
//!   只做纯迭代；`get_nearest_entity` 先收集全量 `HashMap` 再筛选
//!   取最近；`get_nearest_player` 先收集全量玩家 Vec 再筛选取最近。
//!   生产迭代器为 `Box<dyn Iterator>`（无尺寸提示），collect 从
//!   零容量起逐段增长——旧臂以显式插入/推进循环建模该增长链。
//! - 新臂（收编）：`get_nearby_entities` 直接返回 `Vec`（条目宽度
//!   8B，对齐生产 `Vec<Arc>` 指针宽度）；两个 nearest 变体对候选/
//!   玩家表单遍归约（距离过滤 → 谓词过滤 → 就近取小），除索引查询
//!   自身外零分配。
//!
//! 负载模型（500 实体满区散布 160×160×2 层 + 50 玩家 5 平面 ×
//! 10 列，LCG 确定性查询点；候选集为两臂同源公共前置——索引查询
//! 产出（已按球形半径过滤且去重）非本轮改动面，计量从候选集就绪
//! 后开始）：
//! - collect 段：10 000 次 `get_nearby_entities` 形态（半径 16）；
//! - nearest-entity 段：10 000 次 `get_nearest_entity` 形态
//!   （类型过滤 kind==2 + 谓词 id 偶数，半径 24）；
//! - nearest-player 段：10 000 次 `get_nearest_player` 形态
//!   （谓词 id 非 3 倍数，半径 32）。
//!
//! 预登记门（跑前钉死，FAIL 须如实记录）：
//! - G1 collect 段两臂结果指纹一致（逐查询实体集合求和折叠，
//!   求和无序、天然并列无关）；
//! - G2 nearest-entity 段距离剖面指纹逐位一致，且两臂胜者 id
//!   不同的查询全部为等距并列（距离逐位相等）——旧臂并列胜者
//!   由哈希序裁决、随进程随机种子漂移，无任何可观察契约，故
//!   等价判据为「同一最近距离、任一并列成员」；
//! - G3 nearest-player 段同 G2 判据；
//! - G4 collect 段旧臂分配字节 ≥ 2×新臂且新臂非零（旧臂 `HashMap`
//!   条目 16B 且哈希表按 7/8 负载超配，新臂 Vec 条目 8B 倍增
//!   增长，结构性字节比值优势；分配次数两臂同付增长重分配、
//!   无差值保证，不设次数门）；
//! - G5 nearest 两段新臂计量段零分配（单遍归约无中间容器）。
//!
//! 跑前修正记录（均为设计期发现，先于正式记录跑）：
//! 1. 查询点归一化误除 512（应除 2^31），查询点跑出 ±6.7 亿、
//!    候选集恒空、三段读数全 0——由启动健全性计数（分配器工作
//!    正常）与空跑指纹常数定位；
//! 2. 类型过滤 kind==1（⟹ i 奇数）与谓词 id%2==0 互斥，胜者
//!    恒空，改 kind==2；
//! 3. 实体 z 取 i/160 挤在 4 平面、玩家挤 2 平面，改满区散布；
//! 4. G4 初版按分配次数差登记——新臂 Vec 同付增长、次数差无
//!    保证（实测两臂恰好相等），修正为字节比值门；
//! 5. G2/G3 初版按胜者 id 序列折叠——等距并列时旧臂哈希序与
//!    新臂表序可裁决出不同并列成员（语义皆合法），修正为距离
//!    剖面指纹 + 并列审计判据；
//! 6. collect 旧臂初版用精确 `size_hint` 的 collect（单次分配），
//!    与生产无尺寸提示迭代器的增长链不符，改显式循环建模。

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
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

/// 实体表规模（全表回退路径量级）。
const N_ENTITIES: u32 = 500;
/// 玩家表规模。
const N_PLAYERS: u32 = 50;
/// 每段查询次数。
const N_QUERIES: u64 = 10_000;
/// collect 段查询半径。
const COLLECT_RADIUS: f64 = 16.0;
/// nearest-entity 段查询半径。
const NEAREST_ENTITY_RADIUS: f64 = 24.0;
/// nearest-player 段查询半径。
const NEAREST_PLAYER_RADIUS: f64 = 32.0;
/// nearest 段未命中哨兵（两臂一致折叠）。
const NO_WINNER: u32 = u32::MAX;

/// 模型实体：满区网格散布，类型 0-3 轮转（nearest 段过滤 kind==2：
/// i%4==2 ⟹ i 为偶数，与谓词 id%2==0 可同时满足）。
#[derive(Clone, Copy)]
struct ModelEntity {
    id: u32,
    uuid: u64,
    x: f64,
    y: f64,
    z: f64,
    kind: u8,
}

fn squared_distance(entity: &ModelEntity, query: (f64, f64, f64)) -> f64 {
    let (dx, dy, dz) = (entity.x - query.0, entity.y - query.1, entity.z - query.2);
    dx * dx + dy * dy + dz * dz
}

/// 确定性查询点（LCG），两臂共用同一序列。
struct QueryPoints {
    state: u64,
}

impl QueryPoints {
    const fn new() -> Self {
        Self {
            state: 0x2545_F491_4F6C_DD1D,
        }
    }
    fn next(&mut self) -> (f64, f64, f64) {
        let mut mix = || {
            self.state = self
                .state
                .wrapping_mul(6364_1362_2384_6793)
                .wrapping_add(1_442_695_040_888_963_407);
            self.state
        };
        let a = mix();
        let b = mix();
        let c = mix();
        // 归一化：`>> 33` 取高 31 位（值域 [0, 2^31)），除以 2^31
        // 才落到 [0,1)，再映射到实体散布区 [-80, 80]；y 取 8 位映射
        // 到实体 y 值域 [0, 8)。
        (
            ((a >> 33) as f64 / 2_147_483_648.0) * 160.0 - 80.0,
            ((b >> 40) & 0xFF) as f64 * (8.0 / 256.0),
            ((c >> 33) as f64 / 2_147_483_648.0) * 160.0 - 80.0,
        )
    }
}

fn entity_table() -> Vec<ModelEntity> {
    (0..N_ENTITIES)
        .map(|i| {
            let big = u64::from(i);
            ModelEntity {
                id: i,
                uuid: big.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
                // 满区散布：20 列 × 20 排（步长 8）覆盖 [-80, 80]²，
                // y 贴地层（0/1）
                x: (big % 20) as f64 * 8.0 - 80.0,
                y: (big / 400) as f64,
                z: ((big / 20) % 20) as f64 * 8.0 - 80.0,
                kind: (i % 4) as u8,
            }
        })
        .collect()
}

fn player_table() -> Vec<ModelEntity> {
    (0..N_PLAYERS)
        .map(|i| {
            let big = u64::from(i) + u64::from(N_ENTITIES);
            ModelEntity {
                id: N_ENTITIES + i,
                uuid: big.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
                // 5 个 z 平面 × 10 列，满区散布
                x: (big % 10) as f64 * 16.0 - 72.0,
                y: 0.0,
                z: ((big / 10) % 5) as f64 * 32.0 - 64.0,
                kind: 0,
            }
        })
        .collect()
}

/// 公共前置：候选集（模拟区块索引查询产出：已按球形半径过滤且
/// 去重）——两臂同源，不属本轮改动面，计量从其就绪后开始。
fn candidates_for(entities: &[ModelEntity], query: (f64, f64, f64), radius: f64) -> Vec<u32> {
    let radius_squared = radius * radius;
    entities
        .iter()
        .filter(|e| squared_distance(e, query) <= radius_squared)
        .map(|e| e.id)
        .collect()
}

/// 段读数：结果指纹 + 计量段分配 + 总耗时。
struct Report {
    fp: u64,
    delta: Delta,
    ns: u128,
}

/// nearest 段逐查询审计：(胜者 id 或哨兵, 胜者距离平方的位模式；
/// 未命中折叠为 0)。收集在计量段之外，不产生计量内分配。
type WinnerAudit = Vec<(u32, u64)>;

/// 距离剖面折叠值：未命中 0，命中取距离平方位模式（逐位可比）。
fn winner_bits(winner: u32, entities: &[ModelEntity], query: (f64, f64, f64)) -> u64 {
    if winner == NO_WINNER {
        0
    } else {
        squared_distance(&entities[winner as usize], query).to_bits()
    }
}

/// 玩家段变体：玩家 id 自 `N_ENTITIES` 起编，表索引须减去偏移。
fn winner_bits_player(winner: u32, players: &[ModelEntity], query: (f64, f64, f64)) -> u64 {
    if winner == NO_WINNER {
        0
    } else {
        squared_distance(&players[(winner - N_ENTITIES) as usize], query).to_bits()
    }
}

// ---------------------------------------------------------------------------
// 段 1：collect 形态（get_nearby_entities）
// ---------------------------------------------------------------------------

fn collect_old(entities: &[ModelEntity]) -> Report {
    let mut points = QueryPoints::new();
    let mut fp = 0xcbf2_9ce4_8422_2325u64;
    let t0 = Instant::now();
    let mut allocs = 0u64;
    let mut bytes = 0u64;
    for _ in 0..N_QUERIES {
        let query = points.next();
        let candidates = candidates_for(entities, query, COLLECT_RADIUS);
        let start = snapshot();
        // 旧臂：灌入 HashMap（uuid 键）。显式插入循环建模生产
        // 无尺寸提示迭代器 collect 的零容量起步增长链（3→7→14…）。
        let mut map: HashMap<u64, u32> = HashMap::new();
        for &id in &candidates {
            map.insert(entities[id as usize].uuid, id);
        }
        let mut sum = 0u64;
        for id in map.values() {
            sum += u64::from(*id);
        }
        let d = delta_since(start);
        allocs += d.0;
        bytes += d.1;
        black_box(&map);
        fp = fp.wrapping_mul(31).wrapping_add(sum);
    }
    Report {
        fp,
        delta: (allocs, bytes),
        ns: t0.elapsed().as_nanos(),
    }
}

fn collect_new(entities: &[ModelEntity]) -> Report {
    let mut points = QueryPoints::new();
    let mut fp = 0xcbf2_9ce4_8422_2325u64;
    let t0 = Instant::now();
    let mut allocs = 0u64;
    let mut bytes = 0u64;
    for _ in 0..N_QUERIES {
        let query = points.next();
        let candidates = candidates_for(entities, query, COLLECT_RADIUS);
        let start = snapshot();
        // 新臂：直接 Vec 收集（条目宽度 8B，对齐生产 Vec<Arc> 的
        // 指针宽度）。显式推进循环同样建模零容量起步增长链
        // （4→8→16…）。
        let mut collected: Vec<u64> = Vec::new();
        for &id in &candidates {
            collected.push(u64::from(id));
        }
        let mut sum = 0u64;
        for id in &collected {
            sum += *id;
        }
        let d = delta_since(start);
        allocs += d.0;
        bytes += d.1;
        black_box(&collected);
        fp = fp.wrapping_mul(31).wrapping_add(sum);
    }
    Report {
        fp,
        delta: (allocs, bytes),
        ns: t0.elapsed().as_nanos(),
    }
}

// ---------------------------------------------------------------------------
// 段 2：nearest-entity 形态（get_nearest_entity）
// ---------------------------------------------------------------------------

fn nearest_entity_old(entities: &[ModelEntity]) -> (Report, WinnerAudit) {
    let mut points = QueryPoints::new();
    let mut fp = 0xcbf2_9ce4_8422_2325u64;
    let t0 = Instant::now();
    let mut allocs = 0u64;
    let mut bytes = 0u64;
    let mut audit: WinnerAudit = Vec::new();
    for _ in 0..N_QUERIES {
        let query = points.next();
        let candidates = candidates_for(entities, query, NEAREST_ENTITY_RADIUS);
        let start = snapshot();
        // 旧臂：先收集全量 HashMap（uuid 键、零容量起步增长），
        // 再类型/谓词筛选取最近
        let mut map: HashMap<u64, u32> = HashMap::new();
        for &id in &candidates {
            map.insert(entities[id as usize].uuid, id);
        }
        let winner = map
            .values()
            .filter(|&&id| entities[id as usize].kind == 2)
            .filter(|&&id| id % 2 == 0)
            .min_by(|&&a, &&b| {
                squared_distance(&entities[a as usize], query)
                    .total_cmp(&squared_distance(&entities[b as usize], query))
            })
            .copied()
            .unwrap_or(NO_WINNER);
        let bits = winner_bits(winner, entities, query);
        let d = delta_since(start);
        allocs += d.0;
        bytes += d.1;
        fp = fp.wrapping_mul(31).wrapping_add(bits);
        audit.push((winner, bits));
    }
    (
        Report {
            fp,
            delta: (allocs, bytes),
            ns: t0.elapsed().as_nanos(),
        },
        audit,
    )
}

fn nearest_entity_new(entities: &[ModelEntity]) -> (Report, WinnerAudit) {
    let mut points = QueryPoints::new();
    let mut fp = 0xcbf2_9ce4_8422_2325u64;
    let t0 = Instant::now();
    let mut allocs = 0u64;
    let mut bytes = 0u64;
    let mut audit: WinnerAudit = Vec::new();
    for _ in 0..N_QUERIES {
        let query = points.next();
        let candidates = candidates_for(entities, query, NEAREST_ENTITY_RADIUS);
        let start = snapshot();
        // 新臂：单遍归约，零中间容器
        let winner = candidates
            .iter()
            .copied()
            .filter(|&id| entities[id as usize].kind == 2)
            .filter(|&id| id % 2 == 0)
            .min_by(|&a, &b| {
                squared_distance(&entities[a as usize], query)
                    .total_cmp(&squared_distance(&entities[b as usize], query))
            })
            .unwrap_or(NO_WINNER);
        let bits = winner_bits(winner, entities, query);
        let d = delta_since(start);
        allocs += d.0;
        bytes += d.1;
        fp = fp.wrapping_mul(31).wrapping_add(bits);
        audit.push((winner, bits));
    }
    (
        Report {
            fp,
            delta: (allocs, bytes),
            ns: t0.elapsed().as_nanos(),
        },
        audit,
    )
}

// ---------------------------------------------------------------------------
// 段 3：nearest-player 形态（get_nearest_player）
// ---------------------------------------------------------------------------

fn nearest_player_old(players: &[ModelEntity]) -> (Report, WinnerAudit) {
    let mut points = QueryPoints::new();
    let mut fp = 0xcbf2_9ce4_8422_2325u64;
    let t0 = Instant::now();
    let mut allocs = 0u64;
    let mut bytes = 0u64;
    let mut audit: WinnerAudit = Vec::new();
    let radius_squared = NEAREST_PLAYER_RADIUS * NEAREST_PLAYER_RADIUS;
    for _ in 0..N_QUERIES {
        let query = points.next();
        let start = snapshot();
        // 旧臂：先收集半径内全量玩家 Vec（filter 链无尺寸下界，
        // 零容量起步增长——与生产形态一致），再谓词筛选取最近
        let nearby: Vec<u32> = players
            .iter()
            .filter(|p| squared_distance(p, query) <= radius_squared)
            .map(|p| p.id)
            .collect();
        let winner = nearby
            .iter()
            .copied()
            .filter(|&id| id % 3 != 0)
            .min_by(|&a, &b| {
                squared_distance(&players[(a - N_ENTITIES) as usize], query).total_cmp(
                    &squared_distance(&players[(b - N_ENTITIES) as usize], query),
                )
            })
            .unwrap_or(NO_WINNER);
        let bits = winner_bits_player(winner, players, query);
        let d = delta_since(start);
        allocs += d.0;
        bytes += d.1;
        fp = fp.wrapping_mul(31).wrapping_add(bits);
        audit.push((winner, bits));
    }
    (
        Report {
            fp,
            delta: (allocs, bytes),
            ns: t0.elapsed().as_nanos(),
        },
        audit,
    )
}

fn nearest_player_new(players: &[ModelEntity]) -> (Report, WinnerAudit) {
    let mut points = QueryPoints::new();
    let mut fp = 0xcbf2_9ce4_8422_2325u64;
    let t0 = Instant::now();
    let mut allocs = 0u64;
    let mut bytes = 0u64;
    let mut audit: WinnerAudit = Vec::new();
    let radius_squared = NEAREST_PLAYER_RADIUS * NEAREST_PLAYER_RADIUS;
    for _ in 0..N_QUERIES {
        let query = points.next();
        let start = snapshot();
        // 新臂：单遍归约，零分配
        let winner = players
            .iter()
            .filter(|p| squared_distance(p, query) <= radius_squared)
            .filter(|p| p.id % 3 != 0)
            .min_by(|a, b| squared_distance(a, query).total_cmp(&squared_distance(b, query)))
            .map_or(NO_WINNER, |p| p.id);
        let bits = winner_bits_player(winner, players, query);
        let d = delta_since(start);
        allocs += d.0;
        bytes += d.1;
        fp = fp.wrapping_mul(31).wrapping_add(bits);
        audit.push((winner, bits));
    }
    (
        Report {
            fp,
            delta: (allocs, bytes),
            ns: t0.elapsed().as_nanos(),
        },
        audit,
    )
}

// ---------------------------------------------------------------------------
// 门禁与主流程
// ---------------------------------------------------------------------------

/// 并列审计：两臂胜者 id 不同的查询里，距离剖面必须逐位相等
/// （等距并列换成员属无契约范围）；返回（差异数, 非等距差异数）。
fn tie_audit(old: &WinnerAudit, new: &WinnerAudit) -> (usize, usize) {
    let mut diff = 0usize;
    let mut non_tie = 0usize;
    for ((old_id, old_bits), (new_id, new_bits)) in old.iter().zip(new) {
        if old_id != new_id {
            diff += 1;
            if old_bits != new_bits {
                non_tie += 1;
            }
        }
    }
    (diff, non_tie)
}

fn print_segment(title: &str, old: &Report, new: &Report) {
    println!(
        "{title} 段 旧 {:>7} allocs / {:>9} B / {:>10} ns",
        old.delta.0, old.delta.1, old.ns
    );
    println!(
        "{title} 段 新 {:>7} allocs / {:>9} B / {:>10} ns",
        new.delta.0, new.delta.1, new.ns
    );
}

fn main() {
    let mut gates_pass = true;

    println!("== 轮次 29 双臂基准：实体/玩家空间查询族 ==");
    println!(
        "负载：{N_ENTITIES} 实体 + {N_PLAYERS} 玩家，每段 {N_QUERIES} 次查询（候选集为两臂同源公共前置）\n"
    );

    let entities = entity_table();
    let players = player_table();
    // 健全性：全局分配器必须在 main 之前已计数（表构建本身
    // 即有分配），若为 0 说明计数器未生效，后续读数全部无效。
    let boot = snapshot();
    println!("启动健全性：{} allocs / {} B", boot.0, boot.1);

    // 段 1：collect 形态
    let old = collect_old(&entities);
    let new = collect_new(&entities);
    print_segment("collect", &old, &new);
    let g1 = old.fp == new.fp;
    println!(
        "G1 collect 段结果指纹一致（{:#x} vs {:#x}）→ {}",
        old.fp,
        new.fp,
        verdict(g1)
    );
    gates_pass &= g1;
    let g4 = new.delta.1 > 0 && old.delta.1 >= 2 * new.delta.1;
    println!(
        "G4 collect 段旧臂分配字节 ≥ 2×新臂（{} B vs {} B = {:.2}×）→ {}",
        old.delta.1,
        new.delta.1,
        old.delta.1 as f64 / new.delta.1 as f64,
        verdict(g4)
    );
    gates_pass &= g4;

    // 段 2：nearest-entity 形态
    let (old, old_audit) = nearest_entity_old(&entities);
    let (new, new_audit) = nearest_entity_new(&entities);
    print_segment("nearest-entity", &old, &new);
    let g2_profile = old.fp == new.fp;
    let (diff, non_tie) = tie_audit(&old_audit, &new_audit);
    let g2 = g2_profile && non_tie == 0;
    println!(
        "G2 nearest-entity 距离剖面指纹一致（{:#x} vs {:#x}），胜者差异 {diff} 处全部等距（非等距 {non_tie}）→ {}",
        old.fp,
        new.fp,
        verdict(g2)
    );
    gates_pass &= g2;
    let g5a = new.delta.0 == 0;
    println!(
        "G5a nearest-entity 新臂计量段零分配（实际 {}）→ {}",
        new.delta.0,
        verdict(g5a)
    );
    gates_pass &= g5a;

    // 段 3：nearest-player 形态
    let (old, old_audit) = nearest_player_old(&players);
    let (new, new_audit) = nearest_player_new(&players);
    print_segment("nearest-player", &old, &new);
    let g3_profile = old.fp == new.fp;
    let (diff, non_tie) = tie_audit(&old_audit, &new_audit);
    let g3 = g3_profile && non_tie == 0;
    println!(
        "G3 nearest-player 距离剖面指纹一致（{:#x} vs {:#x}），胜者差异 {diff} 处全部等距（非等距 {non_tie}）→ {}",
        old.fp,
        new.fp,
        verdict(g3)
    );
    gates_pass &= g3;
    let g5b = new.delta.0 == 0;
    println!(
        "G5b nearest-player 新臂零分配（实际 {}）→ {}",
        new.delta.0,
        verdict(g5b)
    );
    gates_pass &= g5b;

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
