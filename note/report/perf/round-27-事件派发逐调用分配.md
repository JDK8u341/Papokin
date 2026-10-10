# 轮次 27 · 事件派发逐调用分配治理（护栏闭包枚举化 + `minecraft:` 前缀精确拼接）

> 日期：2026-10-11 · 类别：内存（分配率）+ CPU · 前置：轮次 25 遗留候选（派发路径逐调用 `format!` String 分配）
> 基准：`benchmark/src/bin/dispatch_guard_replay.rs`（双臂分配计数 + 跨臂语义指纹门）

## 背景：WASM 事件派发的两类逐调用分配

轮次 25 治理了事件订阅表（派发查找零分配），报告遗留了派发路径的
逐调用 `format!` 候选。侦察确认派发热路径上有两类可治理分配：

1. **派发护栏的回收闭包**（`wasm_host/state.rs`）。每次事件派发，
   `WasmPluginEventHandler::handle` 进入护栏后 `add_server`/`add_player`
   /`add_world` 等各压入一条资源并记录一个
   `Box<dyn FnOnce(&mut ResourceTable) + Send>` 回收闭包——访客 trap 时
   按记录逐条回收兜底。Server/Player/World 三资源的高频事件
   （玩家移动/交互/方块事件族）**每次派发 3 次堆分配**，全在护栏
   生命周期内（成功路径随 `decay_clear_vec` 丢弃）；
2. **`minecraft:` 前缀 `format!`**（`events/mod.rs` 两助手）。
   `to_wasm_block_name`/`to_wasm_entity_type` 把注册表短名（`stone`）
   拼成带命名空间全名。WIT `String` 字段是所有权边界、单次分配不可
   避免，但 `format!` 的容量预估只来自字面量（实测预估 15-23B 区间：
   `minecraft:stone` 共 15B 不扩容、`minecraft:lightning_bolt` 共 24B
   扩容），**多数注册名（方块名普遍 20-45B）都要二次扩容**——
   每次转换 2 次分配事件 + 逐段格式化机器开销。

## 机制

### 护栏条目枚举化（分配次数 3→0/派发）

```rust
// 旧：逐条堆分配的回收闭包
pub type ResourceTableDeleter = Box<dyn FnOnce(&mut ResourceTable) + Send>;
// 新：类别标签 + rep 的纯数据枚举
pub enum ResourceTableDeleter { Server(u32), Player(u32), World(u32),
                                TextComponent(u32), ItemStack(u32) }
```

- 5 个变体恰好覆盖全部 5 个既有护栏记录点（`add_server`/`add_player`
  /`add_world`/`add_text_component`/`add_item_stack`），不增不减；
- `replay` 语义与各 `add_*` 现场闭包逐一等价：**同类型、同 rep** 的
  `table.delete::<T>`（命中即删，未命中无害 no-op），单测固化；
- 驻留：旧 `Vec<Box<dyn>>` 每槽 16B 胖指针 + 每条目 8B 堆闭包 =
  **24B/条目**；新枚举 8B/条目（u32 rep + 判别式），**3× 缩减**；
- 公共 API（`begin/end_dispatch_guard_*`、字段类型别名消费点）零变化，
  仓内无外部构造 `ResourceTableDeleter` 的调用点（全仓 grep 复核）。

### `minecraft:` 前缀精确容量拼接（1.75→1.0 次分配/调用）

```rust
let mut name = String::with_capacity(10 + block.name.len());
name.push_str("minecraft:");
name.push_str(block.name);
```

- 分配数守恒为恰好 1 次（WIT 所有权边界下限）、无重分配；
- 输出字节与 `format!` 逐字节一致（基准指纹门）；
- 14 个调用点（`events/block.rs` 10 + `events/player.rs` 2 +
  `events/entity.rs` 2）自动受益，无需逐点改动。

## 双臂基准

负载：稳态 10 000 轮派发 × 3 守卫条目（Server/Player/World 三资源）；
失败 1 000 轮；尖峰单派发 300 条目；名字 100 000 次转换（4 个
代表性注册名：stone / lightning_bolt / smooth_red_sandstone_stairs /
waxed_weathered_cut_copper_stairs）。

| 段 | 指标 | 旧臂 | 新臂 | 倍率 |
|---|---|---|---|---|
| 稳态成功 | 分配次数 | 30 014 | **14** | **2143×** |
| 稳态成功 | 记录耗时 | 911.7 µs | 65.9 µs | **13.8×** |
| 失败重放 | 分配次数 | 5 009 | 2 009 | 2.5× |
| 失败重放 | 表状态指纹 | `0xfac1f48b…` | `0xfac1f48b…` | **全等** |
| 尖峰窗口 | 分配次数 | 308 | 8 | 38.5× |
| 尖峰窗口 | **峰值驻留** | **10 592 B** | **4 096 B** | **2.6×** |
| 名字转换 | 分配次数 | 175 000 | **100 000** | 1.75× |
| 名字转换 | 转换耗时 | 8.37 ms | 5.28 ms | **1.58×** |
| 名字转换 | 输出指纹 | `0x76abca3c…` | `0x76abca3c…` | **全等** |
| 合计 | 分配次数 | 210 331 | 102 031 | **2.06×（省 108 300 次）** |

- 稳态段两臂共同的 14 次分配 = 守护向量增长/收缩（`decay_clear_vec`
  滞回后容量稳定，与生产同节奏）；差值 30 000 恰为条目数（每条目
  恰一次 Box 分配），即护栏记录本身的分配被完全消除；
- 失败段两臂共同 2 009 次 = 负载模型自身的 `reps` 向量等装置，差值
  3 000 = 1 000 轮 × 3 条目的 Box 分配；重放路径本身零分配（两臂同）；
- 尖峰峰值驻留解析：旧臂 = `Vec<Box<dyn>>` 512 容量槽 × 16B 胖指针
  （8 192 B）+ 300 个堆闭包 × 8B（2 400 B）；新臂 = 512 × 8B 枚举槽
  （4 096 B），与理论 24B→8B/条目一致；
- 名字段旧臂 1.75 次/调用：4 个代表名中 3 个（24/37/44B）超出
  `format!` 预估容量各扩容一次、`stone`（15B）恰好不扩容；
- 名字段净驻留 +0 B：字符串即产即弃，收益全部在分配次数与 CPU
  （省去 fmt 机器与重分配拷贝，1.58×）。

## 闸门

| # | 闸门 | 结果 |
|---|---|---|
| G1 | 失败重放后模型表指纹两臂全等，护栏外 8 条常驻条目保留 | PASS |
| G2 | 名字转换输出指纹两臂全等（逐字节） | PASS |
| G3 | 新臂名字段分配次数 == 转换次数（恰好 1 次/调用） | PASS（100 000 == 100 000） |
| G4 | 稳态记录段分配差值 == 条目数（每条目恰一次 Box 分配） | PASS（30 014 − 14 == 30 000） |
| G5 | 尖峰峰值驻留 旧 ≥ 2× 新 | PASS（10 592 ≥ 2×4 096） |
| G6 | 生产护栏测试（含新增 replay 语义测试） | PASS（3/3） |
| G7 | papokin lib 全量测试 | PASS（427/427，含基线 426 + 新 1） |
| G8 | fmt / clippy（lib + bins，pedantic，--all-targets） | PASS |

## 测试（`wasm_host::state::tests`）

- `dispatch_guard_success_keeps_failure_replays`（存量：成功保留/
  失败重放/护栏外不受波及——公共 API 驱动，枚举化后原样通过）
- `dispatch_guard_enum_replay_matches_closure_semantics`（**新**：
  枚举 replay 与闭包语义逐一等价——同类型同 rep 命中即删、重复重放
  与虚构 rep 无害 no-op、绝不波及相邻条目）
- `dispatch_guard_decay_reclaims_spike_capacity_next_round`（存量：
  尖峰容量衰减，填充物改为枚举条目）

## 负面发现与后续清单

- `format!` 治理只覆盖两助手（固定前缀模式）。事件路径其余 `format!`
  （聊天消息等用户数据本体）非前缀拼接、内容每次不同，精确容量无从
  预估，不适用本手法；
- WIT `String` 所有权边界使名字转换分配下限为 1 次/调用；若未来 WIT
  接口引入借用字符串参数可降到 0，属协议契约变更，另行评估
  （2026-10-10 轮次 29 批注：已正式否决——借用参数属 WIT 签名级
  协议契约变更，破坏现有已发布插件的 ABI 兼容（红线 3），除非
  API 整体升大版本不再评估，见 round-29 报告侦察定案一节）；
- 枚举是编译期封闭集合：未来若有新的 `add_*` 需要护栏，须扩枚举
  变体 + `replay` 分支（换来的是零分配与数据局部性；当前 5 变体
  恰好等于护栏的完整调用面）；
- 44 个 `add_*` 中仅 5 个有护栏记录，其余资源在事件路径压入时
  由 `cleanup_event` 显式释放（轮次 25 前即如此，非本轮变化）。

## 提交

- `perf(plugin): 派发护栏回收闭包枚举化零分配`（生产 + 测试）
- `perf(plugin): 事件名字转换精确容量拼接`（生产）
- `perf(benchmark): 轮次27 事件派发分配双臂基准`
- `docs(note): 轮次27 报告与索引更新`
