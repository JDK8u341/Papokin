# 轮次 29 · 实体/玩家空间查询族单遍归约（去 HashMap 收集）

## 背景：高频空间查询的三层浪费

`World` 的空间查询族是 AI 目标选择、刷怪规则、钟共振、僵尸化
猪灵聚合等玩法逻辑的公共高频面：

- `get_nearby_entities(pos, radius)` 旧形态把候选实体灌进
  `HashMap<uuid, entity>` 返回——但候选源（`ChunkedEntityIndex`
  的 `query`/`query_into`，按 `Arc::ptr_eq` 去重；全表回退路径
  每实体恰一项）**天然已去重**，外层 HashMap 的唯一职能
  （按键去重）是纯冗余：每次调用白付一次哈希表增长链
  （3→7→14…段段重分配）+ 逐条目 SipHash 哈希（uuid 128 位），
  而全部 6 个消费方（bell/breed/follow_parent/follow_mob/
  zombified_piglin/……）**只做纯迭代、无一处按 UUID 查找**；
- `get_nearest_entity` / `get_nearest_player` 旧形态先收集
  **全量**容器（HashMap / Vec）再筛选取最近——筛选/瞄准类
  AI 高频调用为「只要一个最近者」付出整容器代价。

## 侦察定案过程（含两项正式否决）

本轮起点为轮次 27 在册的「WIT String 借用参数」候选，先行
正式评估后否决，再扫其他驻留/分配面定案查询族：

- **WIT String 借用参数（否决）**：把宿主→插件的字符串参数从
  所有权 `String` 改为借用 `&str`/borrow 参数确可降到 0 分配，
  但这是 **WIT 接口签名级协议契约变更**——现有全部已发布插件
  的导出函数签名按所有权串编译，换借用参数即破坏 ABI 兼容
  （红线 3「兼容性」）。收益（名字转换 1 次/调用 → 0）不值得
  破约，正式否决并在轮次 27 报告补批注归档；
- **ItemStack.patch Arc-COW（否决）**：补丁语义里
  share-then-mutate 模式占主导（先克隆共享、随即改写），
  Arc 写时复制在「取得 Arc 后立刻改」的路径上净增一次分配
  与原子跳数，收益被抵消；且 ItemStack 序列化路径
  （NBT 落盘/协议编码）要求独占访问，COW 需穿透到编码层
  改动面过大。否决；
- pathfinder（轮次 16 已复用）、照明（持久队列已治理）、
  广播（版本键编码缓存已有）、事件路径剩余 `format!`
  （用户数据本体，无前缀模式可用）——均已治理或结构性不适用；
- `ChunkedEntityIndex` 内部（轮次 17 的 `query_any` 零分配
  早退已覆盖谓词场景）之外，**查询族的容器策略是剩余最大
  未治理分配面**，定案为本轮目标。

## 改动

`crates/papokin/src/world/mod.rs`：

- 新增私有 `nearby_entity_candidates`：收敛分块索引优先/
  大半径全表回退的候选迭代器构造（两路径产出天然去重，
  模块文档注明）；
- `get_nearby_entities`：返回类型 `HashMap<uuid, entity>` →
  `Vec<Arc<dyn EntityBase>>`（候选 → 球形过滤 → 收集）；
- `get_nearest_entity`：单遍归约（距离过滤 → 类型/谓词过滤 →
  `min_by` 就近取小），不再先收集全量 HashMap；
- `get_nearest_player`：对玩家表单遍归约 + `.cloned()`；
  `get_closest_player` 委托它自动受益；
- `get_nearby_players` 文档勘误（陈旧的「返回 HashMap」表述）。

消费方适配（编译器驱动，5 处）：bell 的 `.into_values()` →
`.into_iter()`；zombified_piglin 解构 `(_uuid, other)` →
`other`；breed/follow_parent/follow_mob 的 `.values()` 迭代 →
按引用迭代。

### 红线核对（行为/UX 零变化）

- **去重语义不变**：索引层 `Arc::ptr_eq` 去重先于本次改动
  存在（轮次 17），HashMap 从未真正消过重——直接返回 Vec
  与旧返回的条目集合严格一致（基准 G1 求和指纹全等实证）；
- **等距并列裁决**：旧形态 `min_by` 并列胜者由 HashMap 哈希序
  决定，而 `RandomState` 每进程随机播种——**旧并列行为本就
  跨重启漂移、无可观察契约**；新形态按表序裁决，确定性反而
  更强。基准两轮运行并列差异 3 处/6 处不同（全部等距），
  恰为「旧裁决随进程漂移」的实证。已在两函数 rustdoc 注明；
- **兼容性**：查询族为 crate 内部 API（无 WIT 面），全部
  调用点本仓收口，5 处适配即全覆盖（编译器穷尽验证）；
- **功能/UX**：返回条目集合、过滤条件、半径语义逐项未动，
  仅容器与归约策略变化。

## 双臂基准（500 实体满区散布 + 50 玩家，每段 10 000 查询）

`benchmark/src/bin/entity_query_collect.rs`（第 29 个 bin）。
候选集为两臂同源公共前置（模拟索引查询产出：已球形过滤且去重），
计量从候选就绪后开始；计数 GlobalAlloc 与既有基准同构；
启动健全性计数（3 allocs）钉死「分配器生效」前提。

### 跑前修正记录（设计期发现，先于正式记录跑）

1. 查询点归一化误除 512（应除 2³¹），查询点跑到 ±6.7 亿、
   球形过滤恒空、候选集恒空——空迭代器 collect 零分配，三段
   读数全 0。由健全性计数（分配器正常）+ 空跑指纹常数定位，
   属负载模型缺陷而非生产问题；
2. 类型过滤 kind==1（⟹ i 奇数）与谓词 id%2==0 互斥，胜者
   恒空——改 kind==2；
3. 实体 z 挤 4 平面/玩家挤 2 平面，改满区 20×20×2 层 /
   5 平面 × 10 列散布；
4. G4 初版按分配次数差登记——新臂 Vec 同付零容量增长链、
   次数差无保证（实测两臂次数相近），修正为字节比值门；
5. G2/G3 初版按胜者 id 序列折叠——等距并列时哈希序与表序可
   裁出不同并列成员（皆合法），修正为距离剖面指纹 + 并列
   审计判据（胜者 id 不同的查询必须距离逐位相等）；
6. collect 旧臂初版用带精确 size_hint 的 collect（单次分配），
   与生产 `Box<dyn Iterator>`（无尺寸提示）的零容量增长链
   不符——改显式插入/推进循环建模（hashbrown 3→7→14… /
   RawVec 4→8→16…）。

### 预登记门与结果（全部 PASS，EXIT=0）

| 门 | 判据 | 结果 |
|---|---|---|
| G1 | collect 段两臂求和指纹一致 | PASS（0xc18936b17ab04e4c） |
| G4 | 旧臂分配字节 ≥ 2×新臂且新臂非零 | PASS（2.42×） |
| G2 | nearest-entity 距离剖面指纹逐位一致 + 胜者差异全部等距 | PASS（6 处差异全等距） |
| G5a | nearest-entity 新臂计量段零分配 | PASS（0） |
| G3 | nearest-player 同 G2 判据 | PASS（0 处差异） |
| G5b | nearest-player 新臂零分配 | PASS（0） |

### 数据

| 段 | 旧臂 | 新臂 | 收益 |
|---|---|---|---|
| collect（r=16） | 31 314 allocs / 6 269 736 B / 9.95 ms | 30 640 allocs / 2 593 792 B / 6.82 ms | 字节 **2.42×**、耗时 **1.46×** |
| nearest-entity（r=24） | 43 118 allocs / 14 650 560 B / 16.5 ms | **0** allocs / **0** B / 7.00 ms | 分配 −100%、耗时 **2.36×** |
| nearest-player（r=32） | 16 413 allocs / 365 216 B / 1.47 ms | **0** allocs / **0** B / 0.98 ms | 分配 −100%、耗时 **1.50×** |

collect 段分配次数两臂相近（31 314 vs 30 640）符合结构预期：
双方同付零容量增长链，差距在条目宽度（HashMap 16 B/条 +
7/8 负载超配 vs Vec 8 B/条）与哈希开销；nearest 两段新臂
全程零分配（除公共前置的索引查询自身）。

## 负面发现与后续清单

- WIT String 借用参数：**正式否决**（协议契约变更破坏插件 ABI
  兼容，红线 3），除非未来整体升 API 大版本；
- ItemStack.patch Arc-COW：**否决**（share-then-mutate 主导 +
  编码层穿透改动面过大）；
- 查询族生产侧无单测（World 构造重）：语义等价由基准指纹门
  承担，消费方适配由编译器穷尽验证；
- 轮次 28 已明确的 ~4GiB VA/线性内存 per-Store 面维持不动。

## 提交

- `perf(world)`：查询族单遍归约 + 5 消费方适配；
- `perf(benchmark)`：`entity_query_collect` 双臂基准（第 29 bin）；
- `docs(note)`：本报告 + 索引 + 轮次 27 WIT 候选批注；
- `chore(release)`：版本 0.3.40 dist。
