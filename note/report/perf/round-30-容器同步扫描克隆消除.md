# 轮次 30 · 容器内容同步全屏扫描的逐槽克隆消除

## 背景：CONTAINER_SET_SLOT 高频路径的双份深克隆

轮次 30 起点为 1.21.11 审计 P1 在册项的内存面落地侦察：

- **advancements**：仓内仅有命令层（`command/commands/advancement.rs`），
  进度系统本体（进度树/触发器/玩家进度数据）未实现——无可优化
  面，P1 项维持「待实现」状态归档；
- **CONTAINER_SET_SLOT / 容器同步**：`sync_inventory_to_client` →
  `ScreenHandler::send_content_updates`，**命中**——全屏扫描的克隆
  策略存在结构性浪费；
- **动态配方进属性集**：仓内无 recipe property set 结构（grep 无
  匹配），配方属性集聚合尚未实现，同样维持待实现归档。

`send_content_updates`（与同构的 `update_to_client`）逐槽执行：

```text
let slot = slots[i].clone();                 // Arc 克隆（原子跳）
let stack = slot.get_cloned_stack();         // 组件补丁深克隆 #1
self.update_tracked_slot(i, stack.clone());  // 深克隆 #2（传参）
self.check_slot_updates(i, stack);           // move #1
```

`ItemStack.patch` 是 `Vec<(DataComponent, Option<Box<dyn ...>>)>`——
每次克隆 = Vec 重分配 + 逐条目堆分配深拷贝。而 `update_tracked_slot`
与 `check_slot_updates` 的第一步都是**比较**（`are_equal` /
`is_in_sync`，均只需 `&ItemStack`），未变化槽位（全屏扫描中的绝大多
数）的两次克隆只用于比较后即丢弃。玩家屏幕 46+ 槽，`sync_hand_slot`、
拾取、切屏、创造模式物品栏等高频路径都会触发全屏扫描。

## 改动

`crates/papokin-inventory/src/screen_handler.rs`：

- `update_tracked_slot(&mut self, slot, stack: ItemStack)` →
  `stack: &ItemStack`：克隆移入「确认变化」分支内
  （`tracked_stacks[slot] = stack.clone()` 与 listener 的
  `on_slot_update(..., stack.clone())` 次数不变）；
- `check_slot_updates(&mut self, slot, stack: ItemStack)` →
  `stack: &ItemStack`：`set_received_stack(stack.clone())` 移入
  `!is_in_sync` 分支内；
- `send_content_updates` 循环体：去掉逐槽 `Arc` 克隆与第二次无条件
  深克隆，单次 `get_cloned_stack()` 后引用传递；
- `update_to_client` 循环体：同构收编。

两方法是 trait 默认实现、全仓无 override，调用点全部在同文件
（`send_content_updates`/`update_to_client` 循环），改动面收口。
`ScreenHandlerListener::on_slot_update` 与 `SyncHandler` 签名零变化
（3 个 listener 实现方零影响）。

### 红线核对（行为/UX 零变化）

- 比较语义不变：`are_equal`/`is_in_sync` 原本就按引用比较；
- 变化槽位的克隆次数不变（跟踪表/监听器/同步处理器的更新时序
  与内容逐项一致——基准 G1/G2 触发序列指纹全等实证）；
- 未变化槽位省掉的是「比较后必然丢弃」的中间克隆，无任何
  可观察效应；
- 新增单测 `rescan_skips_unchanged_slots_and_syncs_changed_slot`
  固化行为不变式：初次扫描非空槽触发、空槽不触发；稳态重复
  扫描零重发；单槽变化恰重发该槽且跟踪表/同步态反映新值；
  清空槽触发一次。

## 双臂基准（46 槽 × 每段 10 000 次扫描）

`benchmark/src/bin/screen_sync_clone.rs`（第 30 个 bin）。
`ModelStack.patch: Vec<(u32, Box<[u8]>)>` 与生产 patch Vec 克隆成本
同构；46 槽中 12 槽带 3-5 组件补丁（工具/武器/盔甲类）、26 槽裸
物品（空 patch，克隆零分配）、8 槽空。稳态段零变化（游戏内大多
数调用的形态）；扰动段每拍轮转修改一槽（数量/组件交替），两臂
同付变更成本、差值即扫描面差异。

### 预登记门与结果（全部 PASS，EXIT=0）

| 门 | 判据 | 结果 |
|---|---|---|
| G1 | 稳态段触发序列指纹一致 | PASS（空序列固定折叠） |
| G3 | 稳态段旧臂分配次数 ≥ 2×新臂且新臂非零 | PASS（2.00×） |
| G2 | 扰动段触发序列指纹一致 | PASS（0xd10f9ce4a8b76154） |
| G4 | 扰动段触发序列非空（负载有效） | PASS（两臂 10 000 拍全非空） |
| G5 | 扰动段旧臂分配次数 > 新臂 | PASS（1 422 381 > 792 429） |

### 数据

| 段 | 旧臂 | 新臂 | 收益 |
|---|---|---|---|
| 稳态（零变化） | 1 200 000 allocs / 32.24 MB / 42.07 ms | 600 000 / 16.12 MB / 22.37 ms | 分配与字节 **2.00×**、耗时 **1.88×** |
| 扰动（每拍一变） | 1 422 381 allocs / 39.21 MB / 48.37 ms | 792 429 / 22.01 MB / 29.01 ms | 分配 **1.79×**、字节 1.78×、耗时 1.67× |

稳态 2.00× 精确落位结构预期（未变化有补丁槽旧臂付两次克隆、
新臂付一次）；扰动段差值来自未变化槽省半 + 变化槽克隆不变。

## 负面发现与后续清单

- advancements 本体与动态配方属性集**未实现**（P1 项归档为
  待实现，实现时应带分配面设计评审）；
- `Inventory::get_stack` 按值返回（trait 方法）：全屏扫描每槽
  仍付一次读取克隆——进一步消除需要 borrow 变体
  （`with_stack(impl FnOnce(&ItemStack))`），动全部 Inventory
  实现方，改动面大、收益限于有补丁物品，暂不立项；
- `prop_vec`（属性变更暂存）空时不分配，已有零成本快路径。

## 提交

- `perf(inventory)`：槽位同步方法引用化 + 循环去双克隆 + 行为
  不变式单测；
- `perf(benchmark)`：`screen_sync_clone` 双臂基准（第 30 bin）；
- `docs(note)`：本报告 + 索引；
- `chore(release)`：版本 0.3.41 dist。
