# 库存原子操作规划

日期：[20261027]
类型：验收
状态：已验收
环境：未记录
范围：未记录

## 目标

关联：GitHub#15。

## 方法

未记录

## 结果

### 一、接口

plan_inventory接收受限ActionView、冻结ItemCatalog、操作列表和每个目标的预期完整组件，返回InventoryPlan。计划的事务来源固定为库存模块，不修改世界；调用方可读取transaction并参与原有原子事务组，或通过into_parts取出事务和拟提取物品。只有事务提交成功后才能交付提取结果。

| 操作 | 权限 | 行为 |
|---|---|---|
| Insert | inventory:permission/insert | 合并同种物品或写入空槽，拒绝溢出和不同物品 |
| Extract | inventory:permission/extract | 精确物品和数量扣除，返回拟提取物品 |
| Consume | inventory:permission/consume | 精确扣除，不返回提取结果 |
| Swap | inventory:permission/swap | 交换两个槽，支持不同物品和同一容器 |

输入最多64项操作和64个目标，所有目标都要有宿主签发的对象授权。预期组件与当前组件须完全一致，目录、槽位、数量和容量全部检查；失败丢弃私有草稿。批内操作依次作用于草稿，每个容器最后只生成一个带精确旧值的组件替换。

Insert是外部物品进入库存的特权入口，普通transfer权限不包含它。宿主负责外部物品的来源与消耗证明，并把相关世界或实体操作纳入同一事务组。接口不绕过库存所有权，也不把提取计划当作已完成的物品交付。

### 二、验证

新增4项测试覆盖插入、消费、提取、不同物品及同容器交换，拒绝无权限、无目标授权、满槽、超量、未知物品、非法槽、零数量、旧版本和过长批次。失败时世界快照不变。并发提取只有一项成功，快照加提交增量恢复后结果相同，重复重放拒绝。

```sh
cargo test -p torse-inventory --test operations --locked
cargo test --workspace --all-targets --locked
```

全工作区729项通过，零忽略，格式和Clippy通过。库存组件与原有转移接口保持原格式，网络与持久格式不变。

## 复现

未记录

## 边界

未记录
