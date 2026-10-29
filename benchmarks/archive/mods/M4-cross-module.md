# 跨模块原子提交场景记录

日期：[20261024]
类型：验收
状态：已验收
环境：Windows 11 x86_64，Rust 1.93.1，debug profile
范围：未记录

## 目标

跨模块提交语义由契约固定。

## 方法

### 实现

`torse_sim::TransactionGroup`（`MAX_GROUP_MEMBERS=4`）把多个来源的事务组成一项原子任务：构造时按source排序、校验来源唯一与组内写入不重叠（同格子、同实体同模式、生命周期与移动或组件编辑交叉），成员统一采用组的OrderingKey。`CommitCoordinator::prepare_group`对每个成员执行与单事务相同的校验（生命周期、所有权按成员各自source、组件形状、块前置、碰撞按成员自身写入解析），全部通过后合并为一个PreparedChange：一个序号、一条元数据、一份合并ChangeSet；`WorldAuthority::commit_group`提供无日志单跳提交，发布走既有publish路径。

## 结果

### 判读

7项测试覆盖契约语义，全工作区462项通过：

| 测试 | 语义 |
|---|---|
| group_commits_both_sources_and_consumes_one_sequence | 两来源共同提交，序号恰前进1，合并ChangeSet携带两个组件编辑且按成员序 |
| member_ownership_violation_rejects_the_whole_group | 机器成员写库存模式（唯一目标），整组PermissionDenied，世界与序号不变 |
| stale_member_observation_rejects_the_whole_group | 重复加工任务前置过期，整组拒绝，机器进度不前进（并发争用走查） |
| block_precondition_of_any_member_rejects_the_group | 任一成员的块前置失效即整组Conflict |
| constructor_rejects_structurally_invalid_groups | 重复来源、重叠目标、空组与超4成员在构造时拒绝 |
| member_order_is_sorted_by_source | 成员按source排序并统一OrderingKey |
| group_rejects_unresolvable_component_shape | 未知字段的形状校验失败整组拒绝 |

## 复现

### 运行命令

```sh
cargo test -p torse-sim --test cross_module
```

## 边界

### 未覆盖

- WorkPlan、冻结任务存储与恢复重组属B侧接线，待契约评审；恢复重放沿用合并ChangeSet，现有Recovery无需改动。
- 组内顺序可见性与批量转移扩展不在首版（契约第九节）。
