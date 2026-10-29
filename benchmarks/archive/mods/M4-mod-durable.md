# M4前置：Mod机器接入持久执行

日期：[20261024]
类型：验收
状态：已验收
环境：未记录
范围：Mod平台持久化接线，非M4验收

## 目标

本页记录B8历史实现。B9已将机器修改和库存转移改为同一任务的一条持久记录，当前行为与验收见[Mod原子执行](M4-mod-atomic.md)。下文的单来源拆分、下一Tick转移和旧测试输出仅用于说明当时的实现。

维护：Mod平台侧(T05)。前置：[M4-mod-package.md](M4-mod-package.md)。执行宿主说明见[服务端文档](../../../apps/server/README.md)。

### 问题

把P2的进程内唤醒队列迁移到B6持久事件基础设施，使行为包的定时器与计划修改在服务端重启后继续，且结算、失败与恢复遵守执行宿主的记录语义。

## 方法

未记录

## 结果

### 接线设计

- **每包一个`DurableRule`**(apps/server/src/mods.rs,feature `mods`)：spec id `<pkg>:rule/behavior`，Simulation阶段；`plan()`在Tick 0为携带本包schema的实体装填首个唤醒；`event()`按事件载荷分派。
- **计划分解为单来源结算**：guest返回的WIT计划由mod_host的`convert_plan`对真实世界转换：本包组件编辑与块编辑成为一个事务（source=包principal）；至多一个转移编码为**下一Tick的转移事件**（载荷携带两端实体与槽位）；guest唤醒成为持久`EventDraft`。机器完成步的结算因此是：机器事务+（转移事件@T+1，唤醒@T+1）同记录原子落账；转移事件到点后由宿主原生`plan_transfer`产单事务结算。
- **有界快照派发**(crates/mod_host/src/durable_dispatch.rs)：执行宿主独占世界，Store不能借用：每次派发先把目标实体周围5x5x5格、框内实体与自有组件的扁平快照拷入Store数据，guest的查询import从快照应答；计划转换在原生侧持真实世界引用完成。实例跨派发复用，陷阱后重建。无unsafe。
- **陷阱有界重试**：guest陷阱结算为空工作（事件消耗、世界零变化），按`attempt`载荷重试(+8 Tick)，8次后停机；`attempt`计数随事件持久。
- **失败语义**：转移业务拒绝->空屏障终态，机器下一循环自愈；计划验证失败（如越权schema）->空屏障终态；宿主结构性错误不外泄为阶段Fault，单包失败不拖累其他任务。
- 事件载荷字段用包命名空间键(`<pkg>:event/kind|entity|key|attempt|source|...`)。

### 验收结果

`cargo test -p torse-server --features mods --test mod_durability`，6项全过：

| 测试 | 判定 |
|---|---|
| machine_survives_restart_and_keeps_counts_correct | 5 Tick后ready步已结算、转移待发；退出重开后1 Tick完成转移：输入空、输出1、进度续走；再4 Tick第二循环对空输入正确拒绝，计数守恒无复制 |
| settled_tasks_do_not_rerun_the_guest_after_recovery | 重启前4次派发；重开后结算待定转移（原生，零guest调用）+新唤醒仅1次派发 |
| transfer_rejection_leaves_no_partial_changes | 空输入世界7 Tick：转移拒绝≥1次，输入输出零变化，机器继续循环 |
| trapped_guest_parks_bounded_without_side_effects | spin guest 8次陷阱+1次停机；进度0、库存原封；宿主每Tick继续提交屏障记录 |
| failing_package_does_not_stop_the_others | machine（spin克隆）陷阱期间，machine正常完成转移：输出1、输入空、进度续走 |
| task_budget_shortfall_commits_nothing_and_recovers | phase_tasks=1在第6 Tick收集双事件任务时Fault于任何提交之前；重开后进度0、石头未动，续跑完成循环 |

检查：默认工作区462项通过；fmt与clippy（默认与`mods`组合）零告警；P1/P2/P3套件（3/5/6项）回归全绿。复跑：

```sh
cargo test -p torse-server --features mods --test mod_durability
```

## 复现

未记录

## 边界

### 已知边界

- 快照派发的查询面为5x5x5框与框内实体的扁平组件；框外返回NotLoaded，嵌套值不可见（与P2边界一致）。端口估计不参与持久派发。
- 陷阱停机的包没有外部再启动路径（等交互事件或重装）；重试节奏固定+8 Tick、上限8次。
- 转移与机器编辑分两个结算记录（单来源约束所致）：两记录之间崩溃由冻结列表与持久事件在恢复时补齐，计数字段始终保持守恒；两任务间的业务拒绝只在同阶段其他任务改动相同库存时才可能发生，当前场景不存在。
- `mod_world`是演示装配（每包一台机器实体、共享双箱）；正式服务器按GameDefinition装配。
