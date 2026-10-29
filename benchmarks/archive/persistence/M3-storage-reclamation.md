# 长期存储回收

日期：[20261027]
类型：验收
状态：已验收
环境：Windows11、x86_64、Rust1.93.1
范围：日志分段、manifest整理、分区页引用回收及清理中断后的恢复

## 目标

### 一、目标

旧实现持续追加单文件日志和manifest，分区页只发布、不回收。即使快照文件已清理，另外三处仍会随运行时间增长。本次按保留检查点定义恢复下限，并在删除前持久发布相应引用。

## 方法

### 二、实现

manifest整理先验证拟保留快照，将这些引用写入并同步临时文件，原子替换manifest并同步目录，随后删除旧快照。整理后的首条记录使用版本2，可以从非零提交位置开始；版本1读取仍支持。稳定的命名空间锁在替换前后保持同一身份。

首次日志回收发布64字节保留点，后续追加转入按字节及文件数限制的段。仅删除完全被最早保留检查点覆盖的段，相交段继续保留。旧日志有效前缀在需要时继续参与恢复，覆盖后缩为格式标记。旧读取器会拒绝该标记，避免静默读取部分历史。固定头、段文件和迁移语义见[存储格式](../../../crates/storage/FORMAT.md)。

服务端先保存当前世界，再按指定代数保留检查点和日志，追踪保留快照中的parked页以及重放装卸记录中的引用。删除前检查所有引用页的完整性和目录名，随后删除不可达页及中断发布文件。当前驻留分区已经写入完整快照，待处理装卸计划会阻止回收。

## 结果

### 三、验证

完整工作区783项测试通过，零失败、零忽略。逐套原始计数见[workspace.csv](../data/storage-reclamation/01-bounded/workspace.csv)，环境与配置见[environment.txt](../data/storage-reclamation/01-bounded/environment.txt)。

启用Mod的服务端175项测试通过，计数见[mods.csv](../data/storage-reclamation/01-bounded/mods.csv)。格式、Clippy及相关API文档检查通过。

| 场景 | 配置与结果 |
|---|---|
| 日志循环 | 256次提交，每4次按保留点清理；清理后至多2个段，续号与重开一致 |
| manifest循环 | 128次后续快照发布，manifest上限504字节；可保留两代并继续发布 |
| 日志恢复 | 旧日志迁移、相交段、部分旧段残留、最终段不完整及只读扫描通过 |
| 损坏处理 | 缺段、完整帧损坏、坏保留点、缺失保留点、非末段不完整均拒绝 |
| 清理中断 | manifest替换前后注入失败；替换后和删除快照后实际进程退出，当前快照均可恢复 |
| 分区页 | 保留快照及重放引用不删除；孤立页和中断发布文件可清理，重试无额外删除 |
| 只读启动 | 旧目录缺少稳定锁时，预检和领域拒绝不创建锁；成功激活后补建 |

段文件从配置上限读取后再扫描，单帧继续受编解码预算约束。目录枚举、引用集合和快照列表均有显式上限，容量不足时拒绝继续扩张。

## 复现

### 四、复现

```powershell
$env:CARGO_BUILD_JOBS='1'
cargo test -p torse-storage --locked
cargo test -p torse-server --test lifecycle --test streaming --locked
cargo test --workspace --all-targets --locked
cargo test -p torse-server --features mods --all-targets --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
$env:RUSTDOCFLAGS='-D warnings'
cargo doc -p torse-storage -p torse-server -p torse-types --no-deps --locked
```

## 边界

### 五、接口与边界

JournalConfig新增`max_segment_bytes`与`max_segment_files`，完整结构体字面量须补齐。CommitJournal增加默认保留全部历史的回收入口；文件实现启用分段回收。自定义检查点后端须实现原子`replace_manifest`，默认实现会拒绝整理。网络、领域快照与提交载荷格式不变。整理或回收后的目录需要支持新布局的读取器，降级应使用维护前的完整备份。

本次验证进程退出与可注入的I/O失败，没有进行硬件断电、设备缓存或特定文件系统的持久性验收。Unix实现同步目录，尚未在本轮本机执行Unix测试。删除检查点发布之后新增的整个持久日志后缀，仍需要后续检查点的恢复预期才能检测。世界运行代数记录的容量、异步I/O、自动驻留调度和统一进程内存预算不在本次范围。
