# 轮次 28 · Wasmtime 运行时底座共享化（Engine/Linker/缓存线程/epoch ticker）

> 日期：2026-10-11 · 类别：内存（驻留）+ CPU（唤醒/加载） · 前置：轮次 25 报告在册架构级候选（"共享 wasmtime Engine + 内存保留参数调优，每插件一 Engine/一 100ms ticker，留待后续轮次"）
> 基准：`benchmark/src/bin/shared_wasm_runtime.rs`（双臂：真线程/真 tokio 定时任务 + Engine/Linker 结构模型）

## 背景：每插件一套完整 Wasmtime 底座

轮次 25 侦察记录：`PluginRuntime::new` 每次插件加载各建一套——
`WasmPluginLoader::load`（`loader/wasm/mod.rs`）对每个 `.wasm` 文件
调用一次，旧形态开销逐插件复制：

1. **一个 `wasmtime::Engine`**：内部类型注册表与分配基础设施，
   全部插件输入完全相同（同一批常量开关）；
2. **一个缓存工作 OS 线程**：`Cache::new` 无条件
   `thread::spawn`（wasmtime `crates/cache/src/worker.rs:90` 源码
   确认），N 插件 = N 个常驻线程（各自默认栈 VA）；
3. **一份全量 Linker 注册**：`setup_linker` 逐次注册 WASI p2/p3 +
   HTTP + papokin 宿主 API（982 函数 + WASI 族），每次加载重做；
4. **一个 100ms epoch ticker 定时任务**：每插件一个 tokio interval
   任务递增**各自 Engine** 的 epoch 计数器，N 插件 = N 路定时唤醒。

侦察确认共享可行性（红线证据链）：

- **配置等价**：插件发现不递归子目录（`plugin/mod.rs` `read_dir`
  循环 `is_dir() → continue`），所有插件都在 `./plugins/` 顶层，
  缓存目录恒为共享的 `plugins/cache`——各 `PluginRuntime` 的
  Config 输入逐字节相同；
- **Linker 可共享**：wasmtime 组件 `Linker<T>` 为纯拥有数据 +
  `PhantomData<fn() -> T>`（与 T 无关，自动 `Send + Sync`），
  `instantiate_pre`/`instantiate` 均取 `&self`；
- **epoch 语义兼容**：`Store::set_epoch_deadline(ticks)` 是**相对**
   截止（内部换算为「当前 epoch + ticks」），`set_epoch_budget`
   的预算也是增量——共享单一计数器后每插件仍按每 100ms 一拍
   计时，速率与判定逐例等价；
- **缓存兼容**：手工 JIT 缓存（`load_component` 的 hash 命名
  `.cwasm` + `Component::deserialize_file`）按「wasm 哈希 + 包版本」
  键控，0.3.39 版本递增本身已使旧缓存整体失效，无跨版本残留风险。

## 机制

```rust
struct SharedWasmRuntime { engine, linker, ticker: TickerRegistry }
static SHARED_WASM_RUNTIME: OnceLock<Arc<SharedWasmRuntime>>;

struct TickerRegistry { inner: Arc<TickerInner> }   // 计数+启停同一把锁
struct TickerInner { engine: Engine, state: Mutex<{ live: usize, task: Option<AbortHandle> }> }
```

- 首个插件加载时 `get_or_init` 构建共享底座（失败不入缓存、下次
  重试；并发首载败者弃用自建实例）；每插件只克隆 `Arc` + 获取
  ticker 守卫（零分配）；
- **ticker 引用计数**：守卫计数 0→1 启动单个 100ms 任务（全进程
  一份，递增共享 Engine 的 epoch），1→0 中止；之后再加载重新
  启动（热重载「先卸后装」窗口正确）。计数与启停全部在同一把
  互斥锁内，无竞态窗口；
- `WasmPlugin` 持守卫存活（原为持 `AbortHandle` 由 loader 手工
  转交），`PluginRuntime::init_plugin` 内部完成转交，加载失败随
  运行时 Drop 自动释放——与旧语义逐一对应；
- 卸载路径：`Drop for WasmPlugin` 弃置 store 后守卫随字段释放，
  最后一个插件卸载才停表。

### 红线核对（行为/UX 零变化）

- 死循环插件中断：epoch 计时速率不变（100ms/拍）、deadline 相对
  语义不变、每 Store 独立截止——逐例等价；
- 插件隔离：Store/实例/线性内存本就 per-plugin（wasmtime 实例级
  隔离），共享的只是无状态的宿主注册与引擎基础设施；每插件
  `StoreLimits` 512MB 上限不变；
- JIT 缓存：手工 `.cwasm` 缓存路径与键控不变；wasmtime 内部
  Cache 随 Engine 共享（同目录，行为不变）；
- 无 tokio 运行时的同步上下文：不启动 ticker（与旧形态一致），
  守卫计数照常工作。

## 双臂基准（64 插件 ×（Engine 模型 + 缓存线程 + 982 函数 Linker + 100ms ticker））

| 段 | 指标 | 旧臂 | 新臂 | 倍率 |
|---|---|---|---|---|
| 加载 | 分配次数 | 126 529 | **1 978** | **64×** |
| 加载 | 净驻留 | 6 215 168 B | **97 544 B** | **63.7×** |
| 加载 | 耗时 | 30.3 ms | 0.40 ms | **75×** |
| 稳态（1250ms） | 定时唤醒 | 769 次 | **13 次** | **59×** |
| 卸载 | 耗时 | 9.25 ms（join 64 线程） | **6.1 µs**（join 1） | **1516×** |
| 常驻结构 | OS 线程 + tokio 任务 | 64 + 64 | **1 + 1** | 64× |

- 新臂加载段 1 978 次分配全部来自共享底座的一次性构建（982 注册 ×
  （键 String + Box<str>）+ 基础结构 ≈ 2×982+14），每插件追加
  **0** 次（守卫计数与 `Arc` 克隆零分配）；
- 稳态窗口内旧臂 64 路定时器各自唤醒（769 ≈ 64×12 拍）、新臂单
  路唤醒 13 拍——每个插件的截止判定仍按每 100ms 一拍驱动（G1
  语义等价门）；
- Engine/Linker 为结构模型（真字节按模型口径），硬事实是实例数
  64→1；缓存线程与 ticker 为**真**线程/真 tokio 任务实测。

## 闸门

| # | 闸门 | 结果 |
|---|---|---|
| G1 | 稳态唤醒 旧 ≈ 64×拍数、新 ≈ 拍数（每插件计时语义等价） | PASS（769/13） |
| G2 | 新臂加载分配 ≤ 2×982+64（仅共享底座一次性构建，每插件追加 0） | PASS（1978） |
| G3 | 峰值驻留 旧 ≥ 10× 新 | PASS（63.7×） |
| G4 | 卸载完整性：线程全 join（64/64 与 1/1）、守卫计数归零 | PASS |
| G5 | 停表观测：守卫归零后 350ms 零唤醒 | PASS |
| G6 | 生产单测（共享单例 + ticker 引用计数启停/重启） | PASS（2/2 新增） |
| G7 | papokin lib 全量测试 | PASS（429/429，基线 427 + 新 2） |
| G8 | fmt / clippy（lib + bins，pedantic，--all-targets） | PASS |

## 测试（`wasm_host::tests`，新增 2 项）

- `ticker_refcount_starts_stops_and_respawns_shared_task`：首守卫
  启动、存活复用、末守卫中止、再获重启（完整生命周期状态机断言）；
- `plugin_runtime_shares_process_wide_base`：两次 `PluginRuntime::new`
  共享同一 `Arc` 底座（`Arc::ptr_eq` 与进程级单例一致），双守卫
  计数 2、全释放后归零且任务停。

## 负面发现与后续清单

- **轮次 25 提及的「~4GiB VA 预留/实例」不属于 Engine 面**：那是
  on-demand 分配器对每条线性内存（per-Store）的虚拟地址预留 +
  guard 策略（64 位进程 VA 充裕、非 RSS）。调低
  `memory_reservation` 会让增长超限的内存走搬移路径（真实 CPU
  代价）换地址空间节省（伪内存压力），本轮明确**不调**，记录在案
  防重复探查；
- 共享 Engine 的内部 Cache 目录固定为首次加载的目录：插件发现不
  递归，实际恒为同一目录；若未来支持子目录插件，需注意内部缓存
  目录取首载值（内容寻址、跨目录共用仍正确，只是位置不同）；
- epoch ticker 若其宿主 tokio 运行时整体销毁，任务随之消亡——旧
  形态同样如此（且旧形态永不重启）；新形态在计数归零后再次加载
  会重建任务，严格优于旧行为。

## 提交

- `perf(plugin): 共享Wasm运行时底座与引用计数ticker`（生产 + 测试）
- `perf(benchmark): 轮次28共享底座双臂基准`
- `docs(note): 轮次28报告与索引更新`
