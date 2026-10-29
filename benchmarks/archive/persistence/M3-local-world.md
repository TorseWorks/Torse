# 真实世界持久化装配验证

日期：[20261024]
类型：验收
状态：已验收
环境：Windows，Rust1.93.1，默认功能
范围：未记录

## 目标

未记录

## 方法

未记录

## 结果

### 范围

WorldHost与WorldHostRecovery把WorldAuthority、完整ChangeSet和WorldCodec接入服务端已有协调器、文件日志、检查点与目录锁。可信本地事务在固定Tick边界执行，可包含块编辑、碰撞解析后的实体移动和受保护组件修改。世界完全驻留，快照保留实体分配水位。

本次属于B6的本地事务装配。Simulation执行循环、实体生成与销毁事务、持久事件、网络请求与去重结果、授权复制仍未接入，完整M3验收尚未完成。

### 验证

```sh
cargo test -p torse-server --test world
cargo test --workspace --all-targets
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Rustdoc检查使用RUSTDOCFLAGS=-D warnings，执行cargo doc --workspace --no-deps。

| 场景 | 判据与结果 |
|---|---|
| 混合事务与库存争用 | 块写入、负坐标跨区移动、两个库存替换共同持久；旧观察竞争事务按Conflict拒绝，位置不增加 |
| 检查点与日志尾 | 快照截止位置1，日志续到2；附加未完成尾部后恢复与在线快照一致，运行代数更新，继续提交到3并正常关闭 |
| 记录超限 | 单记录预算不足返回TooLarge，连续重试后世界、序号、水位不变；恢复到允许预算的实例后同一计划成功 |
| 刷盘失败 | 世界发布后屏障失败，不返回Durable结果，实例停止，只读访问与后续提交均拒绝 |
| 领域恢复拒绝 | 空方块注册表不能解析日志中的石头，快照、manifest与日志尾均未修复；使用正确内容后可恢复 |
| 有界快照 | 稠密格值和嵌套库存参与预算；不足18项或字节不足时拒绝，世界不变，足额预算与普通捕获一致 |
| 恢复位置检查 | 元数据、世界、兼容信息、阶段、步骤、Tick变化、前序与最终位置不符均拒绝 |
| 发布后接纳前进程退出 | 子进程直接退出，重开恢复到位置0，块、实体与库存共同保持初始值 |
| 持久确认后进程退出 | 子进程直接退出，重开恢复到位置1，整组世界变化与预期快照一致 |

专项测试入口共8项，其中1项是子进程入口，7项为父进程验收场景。工作区共356项测试通过，0失败，格式、Clippy和Rustdoc检查通过。

### 可运行示例

```sh
cargo run -p torse-server --example durable_world -- .work/local-world
cargo run -p torse-server --example durable_world -- .work/local-world
```

在新目录执行时，首轮输出epoch=1、commit_seq=1、durable_seq=1，方块由空气变为石头。第二轮输出epoch=2、commit_seq=2、durable_seq=2，方块恢复后变回空气。输出中的运行时方块编号只用于本轮诊断，持久记录仍使用稳定内容身份。

测试覆盖进程退出与文件恢复，未验证硬件断电或目录项的跨平台持久性。文件包裹版本仍为1，世界格式仍为2，本次未改动字节布局。

## 复现

未记录

## 边界

未记录
