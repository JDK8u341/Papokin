# 轮次 26 · 实体元数据存储扁平化与内联值

> 日期：2026-10-11 · 类别：内存（驻留与分配率） · 前置：无（与前 25 轮无重叠的新驻留面）
> 基准：`benchmark/src/bin/entity_metadata_storage.rs`（双臂分配计数 + 跨臂打包指纹门）

## 背景：最后一个未治理的「每实体 ×N」驻留面

前 25 轮肃清了连接缓冲、编码缓存、调色板/光照容器、tick 各类向量、
长生命周期表桶驻留与插件宿主表，但**逐实体的同步元数据表**
（`SynchedEntityData`）从未被触及。旧形态（`entity/synched_entity_data.rs`）：

```rust
struct DataItem {
    tracked: TrackedData,              // 16B，与 HashMap 键完全重复
    serializer: Box<dyn ErasedSerializer>, // 每项一次堆分配（bool 也是）
    canonical_bytes: Vec<u8>,          // 每项再一次堆分配（变更检测用）
    dirty: bool, is_default: bool,
}
items: Mutex<HashMap<TrackedData, DataItem>>
```

三重代价：

1. **驻留**：每追踪项 = HashMap 桶内条目（16B 键 + 56B 值 + 哈希开销）
   + `Box<dyn>` 堆块（分配器最小 16B）+ canonical `Vec` 堆块（≥16B），
   实测约 235 B/项；典型实体（基础 7 + 生物 5 + mob 1 ≈ 13 项）
   仅元数据表就 2.8 KB+，随全服实体数线性放大；
2. **变更检测的分配税**：`set` 无论值是否变化都先
   `write_canonical` 序列化进一个新 `Vec` 再比较——no-op set
   （tick 内最常见：flags/pose/ambient 反复写同值）每次白付
   一次序列化 + 一次分配 + 一次释放；
3. **哈希查找**：每实体个位到数十条追踪项，HashMap 的分桶
   间接寻址不如扁平数组顺扫的缓存局部性。

## 机制

### 收编形态

```rust
enum TrackedValue {
    Bool(bool), I8(i8), U8(u8), I32(i32), F32(f32),
    VarInt(VarInt), OptionalInt(OptionalInt), BlockPos(BlockPos), // 内联：零堆分配
    Boxed(Box<BoxedValue>),  // 大值回退：序列化器与 canonical 同住一次分配
}
struct DataItem { value: TrackedValue, dirty: bool, is_default: bool } // 键不再重复
items: Mutex<Vec<(TrackedData, DataItem)>>  // 扁平向量，线性查找
```

### 变更检测语义等价论证（红线：行为不变）

旧实现以「canonical 字节相等」判 no-op；新实现按存储形态比较：

- 整数/布尔/`VarInt`/`OptionalInt`/`BlockPos`：值相等 ⇔ 序列化字节相等
  （定宽小端/原样位写，无别名）；浮点用 `to_bits`——IEEE-754 位型与
  序列化字节一一对应，`-0.0` 与 `0.0` 位不同即视为变化、同位 NaN
  （含负载位）视为未变，与旧字节比较逐例一致（单测固化）；
- 装箱族（`TextComponent`/`Option<Uuid>`/`ItemStackSerializer`/
  `VillagerData`/粒子等）：沿用 canonical 字节比较，语义原样。

### 调用面兼容（红线：零 API 破坏）

- `define`/`set` 泛型签名不变，仅增加 `T: IntoTrackedValue` 约束
  （`Entity::set_synced_data` 同步传播）；逐类型显式实现（无 blanket），
  漏掉某类型直接编译失败，保证内联集与真实调用面一致；
- 内联 8 类（全仓 343 个调用点的主力类型）+ 装箱 9 类
  （`TextComponent`/`Option<TextComponent>`/`Option<Uuid>`/
  `Option<BlockPos>`/`ItemStackSerializer<'static>`/`VillagerData`/
  `EffectParticles`/`ParticleMeta`/`Vector3f`/`Quaternionf`）；
- `ErasedSerializer`/`Box<dyn>` 对外可见类型保留（plugin-api 未触碰）；
- 迭代序：旧 HashMap 本身不定序、客户端按每条目自带的索引解码
  （顺序无关紧要），新插入序是另一种合法序，用户体验无差异。

## 双臂基准（1000 实体 × 13 追踪项，10% 实体带自定义名装箱族）

| 阶段 | 指标 | 旧臂 | 新臂 | 倍率 |
|---|---|---|---|---|
| 加载（define 全项） | 分配次数 | 29 009 | 5 109 | **5.7×** |
| 加载 | 分配字节 | 4 819 952 B | 2 360 952 B | 2.0× |
| 加载 | **净驻留/实体** | **2 818 B** | **1 363 B** | **2.07×** |
| 稳态 set（3 变化 + 3 同值） | 分配次数 | 9 000 | **0** | ∞ |
| 稳态 set | 分配字节 | 57 000 B | **0 B** | ∞ |
| 打包（两版本） | 分配次数/字节 | 9 010 / 332 208 B | 9 010 / 332 208 B | 1.0× |
| 打包指纹 | 两版本 × 全实体折叠 | `0x98cfec43…` | `0x98cfec43…` | **全等** |

- 稳态 9 000 → 0：旧臂 6 次 set/实体 = 6 个 canonical `Vec`（no-op 也付）
  + 3 个 `Box`（变化项重装）；新臂 no-op 按位比较零成本、变化项换值零分配；
- 打包字节 1.0× 与指纹全等：出站路径语义零变化；
- 卸载后净驻留增量归零（两臂对称，无泄漏）。

## 闸门

| # | 闸门 | 结果 |
|---|---|---|
| G1 | 跨臂打包输出折叠指纹全等（规范序：按版本索引排序；生产旧序本身不定） | PASS |
| G2 | 加载净驻留 ≤ 旧臂 1/2 | PASS（2.07×） |
| G3 | 稳态 set 零分配 | PASS |
| G4 | 生产 7 项单元测试（打包序/清脏窗口/no-op/浮点位级/装箱 canonical/define 重置/缺键插入） | PASS |
| G5 | papokin lib 全量测试 | PASS（426/426，含基线 421 + 新 5） |
| G6 | fmt / clippy（lib+bins，pedantic） | PASS |

## 测试（`synched_entity_data::tests`，7 项）

- `pack_dirty_for_versions_packs_all_versions_and_clears`（存量）
- `set_after_pack_marks_dirty_again`（存量，并发清脏窗口语义）
- `equivalent_floats_match_canonical_byte_semantics`（新：`-0.0`/`0.0`/NaN 位级）
- `inline_scalars_skip_noop_sets`（新：bool/u8/VarInt 同值不置脏）
- `boxed_custom_name_compares_by_canonical_bytes`（新：装箱族字节比较）
- `define_resets_existing_entry`（新：define 整项重置，is_dirty 粘滞语义）
- `set_inserts_missing_keys_and_pack_contains_all`（新：缺键插入 + 多条目打包）

## 负面发现与后续清单

- `VillagerData`（12B Copy）走装箱而非内联：保持值枚举与游戏类型解耦，
  村民数量级小，不立项；
- 迭代序从「不定序」变为「插入序」：客户端语义无关（条目自带索引），
  但若未来有插件依赖 `HashMap` 迭代序（本就不该依赖）需注意；
- 脑记忆 `MemoryStore`（`Vec<Option<MemorySlot>>`）已天然扁平，无治理空间
 （侦察确认，防重复探查）。

## 提交

- `perf(entity): 实体元数据表扁平化与内联值存储`（生产 + 测试）
- `perf(benchmark): 轮次26 实体元数据存储双臂基准`
- `docs(note): 轮次26 报告与索引更新`
