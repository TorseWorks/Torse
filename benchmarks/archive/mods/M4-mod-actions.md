# Mod动作与规则身份

日期：[20261028]
类型：验收
状态：已验收
环境：未记录
范围：未记录

## 目标

未记录

## 方法

### 实现

WIT升级为0.3.0-draft，增加类型化Interaction、嵌套ValueTree、spawns与despawns。ValueTree是后序节点表，转换器拒绝循环、共享节点、重复键及未使用节点，并执行原生组件的深度、节点和字节限制。宿主预算覆盖新字段的输入、输出和快照复制。

批量转移复用库存草稿，按顺序处理最多64项。中转容器可以被多次引用，最终只输出一次替换；后项失败时全部丢弃。AuthorityHost和持久服务端把机器、库存与生命周期成员作为同一事务组准备。宿主持久唤醒会为确实持有本模块组件的当前对象补充自身授权，允许启动后新增的同模块机器继续运行。

DurableGuest::plan_interaction使用宿主路由与认证调用者，取双方权限及对象范围的交集。Guest只能返回计划，实体身份由原生准备阶段分配。生命周期要求显式权限、范围和组件所有权，不能移除其他模块的库存实体。

## 结果

### 场景

2026-10-28在Windows上验证物品规则身份、批量库存、嵌套WIT数据及实体生命周期。记录反映功能与失败边界，不用于判断高并发性能。汇总数据位于[data/mod-actions/01-typed](../data/mod-actions/01-typed)。

标准装配把ItemCatalog的排序规则摘要并入权威规则摘要，再冻结组件配置。两个目录只有输入顺序不同时摘要相同；物品成员或堆叠上限变化时摘要不同。服务端创建目录并写入检查点后，以不同堆叠上限重开会失败；随后用原规则重开仍成功。

### 验证

工作区831项通过，无失败或忽略。服务端Mod与宿主组件组242项通过，无失败；其中一项发布模式性能测量按既有设置默认忽略，不属于功能验收。

| 场景 | 验证结果 |
|---|---|
| 库存规则身份 | 顺序稳定，成员及容量变化可区分，旧目录拒绝不匹配规则 |
| 共享端点转移 | 草稿中转守恒，后项失败不改变世界 |
| WIT嵌套数据 | List与Record真实穿过Wasm边界并更新组件 |
| 非法节点图 | 循环、重复引用、未使用节点、重复字段及超额值均拒绝 |
| 原生与Wasm一致性 | 相同动作得到完全相同的库存事务 |
| 调用者授权 | 缺权限或缺任一端点授权时拒绝，不借用模块权限 |
| 实体生命周期 | 授权生成与移除成功，越权及其他模块组件拒绝 |
| 恢复 | 原生最终值记录重放与提交后的快照完全相同 |

复跑命令：

```sh
cargo test --workspace --all-targets --locked
cargo test -p torse-server -p torse-mod-host --features torse-server/mods,torse-mod-host/package --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p torse-server -p torse-mod-host --features torse-server/mods,torse-mod-host/package --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo doc --workspace --no-deps --locked
```

### 兼容与范围

包API为0.3，0.2组件需要重新生成绑定并构建。网络和存档字段布局不变；库存目录首次参与规则摘要后，旧规则身份需要显式迁移或新目录。不会自动改写旧世界摘要。

当前提供宿主可装配的Guest动作入口与真实Wasm验收。示例网络监听器和图形面板尚未自动注册这些动作；宿主接入时必须把InteractionPlan中的事务组与唤醒共同持久化，再确认动作。原型FixtureHost没有库存或实体权威，遇到相应计划会整体报错。

## 复现

未记录

## 边界

未记录
