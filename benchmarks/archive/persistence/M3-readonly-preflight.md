# 只读兼容预检

日期：[20261027]
类型：验收
状态：已验收
环境：未记录
范围：未记录

## 目标

关联：GitHub#13。

## 方法

### 一、入口

WorldLease::inspect取得已有目录的排他租约并检查epoch记录，不创建文件、截断尾部或追加epoch。宿主可通过root读取自己的生成与规则描述，再调用FileServer::preflight_file核对预期Compatibility、检查点载荷和日志前缀；成功返回存档SnapshotMeta，其中包含模式、规则与内容身份。

```rust
let lease = WorldLease::inspect(path, world, epoch_limit)?;
let saved = Files::preflight_file(&lease, compatibility, codec.clone(), config)?;
validate_application_descriptors(lease.root(), saved)?;
let directory = lease.start()?;
// Pass directory into the ordinary file recovery entry point.
```

示例中的应用校验函数由宿主实现。所有检查期间持续持有同一文件锁，start消费租约并将锁转交WorldDirectory，期间不会释放排他权。仅start分配新epoch并修复合法的未完成epoch尾部。旧WorldDirectory::acquire保留立即启动行为，调用方需要只读预检时改用两阶段入口。

预检不激活检查点，不修复日志或manifest尾部，不构造可接收请求的权威对象。正式启动后的领域恢复仍走原有隔离恢复路径；该阶段失败可能已经消耗启动epoch。存档格式和网络格式不变。

## 结果

### 二、验证

新增3项测试，覆盖空目录不创建文件、预检重复执行、完整容量但无法启动的epoch账本、未完成尾部保留、三个兼容字段分别不匹配，以及检查期间另一进程被拒绝。兼容预检成功或失败后，world.lock、日志、manifest和检查点的全部文件字节保持原样。只有显式start后epoch从1变为2。

```sh
cargo test -p torse-server --test directory --test lifecycle --locked
cargo test --workspace --all-targets --locked
```

全工作区723项测试通过，零忽略；Clippy和格式检查通过。原进程退出、锁别名、完整损坏拒绝及逐字节尾部恢复测试继续通过。

## 复现

未记录

## 边界

未记录
