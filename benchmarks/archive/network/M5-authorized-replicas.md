# A7授权投影与复制源验证

日期：[20261024]
类型：验收
状态：已验收
环境：未记录
范围：A7正确性与A侧接口收尾，非完整M5联机验收

## 目标

Windows本地，Rust1.93.1，默认features。

## 方法

### 实现

- `crates/sim/src/replica.rs`：ReplicaSource独占WorldAuthority，基线与发布共用串行边界；按宿主scope、ActorContext目标授权及组件声明生成规范载荷。
- 复用types的Viewer、Binding与泛型seed/batch包裹。领域载荷不包含私有组件、旧值或身份分配水位；地形符号附稳定内容字典。
- 增量替换变化的Section和实体，跨范围与销毁发送删除身份；每个可见提交一个批次。不可见提交不占复制序号，through保留权威提交位置。
- Publication增加invalidated_replicas。撤权即时返回独立控制信号；超限清空订阅并在发布结果中通知，其他订阅和世界继续推进。
- 库存container声明Authorized，请求参数声明仍为Private。只有宿主授予对象读取访问的Viewer取得实际嵌套槽位。

接入方式与字段由授权复制契约固定。

## 结果

### 场景与结果

16项sim专项测试与2项真实库存集成测试通过：

| 场景 | 判据 |
|---|---|
| 授权者与旁观者 | 同范围内只有授权者收到Authorized组件；Private和ClientPresentation均不发送；范围外实体不出现 |
| 基线后的首个可见提交 | 丢弃准备、隐藏组件修改不产生增量；之后ReplicaSeq=1，through使用实际CommitSeq |
| 混合事务 | 块与多个组件在同一批次出现，私有字段不泄露；组件删除采用最终替换 |
| 客户端重建对照 | 连续应用增量后的镜像逐次等于同一位置重新捕获的授权基线 |
| 完整前缀和小预算 | 按预算只取整批前缀；首批不够返回ResourceLimit，再取仍是原来的序号 |
| 慢订阅 | 只清空超限订阅并报告失效；健康Viewer继续取得连续序号，世界提交成功 |
| 撤权 | 已取出的B侧数据需要独立控制信号清理；A侧剩余队列立即失效，下一Publication重复通知 |
| 旧绑定与重连 | 旧epoch不能读取或关闭当前订阅；close后新编号不复用，基线覆盖当前状态 |
| 预算与隐私字典 | Dense数组计入字节，嵌套槽位计入条目，其他范围内的方块状态不泄露到字典 |
| 加载状态 | Requested/Loading/缺失区域没有伪造空气；空字典不泄露全注册表 |
| 生命周期与范围 | 移出、移入、销毁、重新生成分别输出删除或最终实体，历史身份不重用 |
| 真实库存 | 转移前有权Viewer得到实际槽位，转移后同一批包含两端最终库存；旁观者前后都没有库存数据，物品守恒 |

提交前合并A7与Mod平台P2复查：全工作区400项通过、0失败，其中A7新增18项。全仓格式、默认工作区Clippy与Mod宿主authority特性Clippy通过；P2真实权威机器5项测试通过。前轮警告视为失败的Rustdoc也已通过。测试记录是本地工作树证据，未运行远端CI。

## 复现

### 复跑

```sh
cargo test -p torse-sim --test replica --locked
cargo test -p torse-inventory --test transactions replica:: --locked
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Rustdoc使用`RUSTDOCFLAGS=-D warnings`执行`cargo doc --workspace --no-deps --locked`。本轮只格式化A侧修改的sim与inventory包；全仓格式检查也需要其他并行工作保持格式正确。

## 边界

### 未覆盖

没有网络编码、真实连接、分片重组、客户端协议实现或发送任务；DurableSeq门槛和B侧撤权清队列由B7接线，测试不宣称已验证真实网络撤权。当前LocalWorldModel仍无复制订阅。

世界有限且完全驻留，scope固定；没有流送装卸和逐字段可见性。每提交重新投影并比较，队列和投影有硬上限，临时内存还包含新投影与一批差异。内存预算是领域载荷估算，不包含分配器开销，也不是网络字节承诺。本记录不提供性能、帧率或线程扩展结论。
