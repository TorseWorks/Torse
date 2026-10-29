# 运行态与批量提交

日期：[20261025]
类型：测量
状态：已验收
环境：Windows 11 Pro 10.0.26300、Intel Core i7-14700KF、28个逻辑CPU、NTFS、Rust 1.93.1、Release
范围：未记录

## 目标

基线提交：deb744a，包含本轮工作区改动。数据位于[data/runtime-commit](../data/runtime-commit)。

## 方法

未记录

## 结果

### 一、运行态记录

[串行基线](M6-serial-baseline.md)显示，冻结任务反复进入每项任务的完整运行态记录，放大了复制、校验和编码成本。本轮将规则声明及冻结任务列表改为Arc共享。容量推迟需要修改列表时使用写时复制，已缓存的列表保持不可变。

冻结、阶段结束和关闭拒绝仍使用完整运行态；逐项结算默认写RuntimeProgress，包含next、next_event及有界事件和请求窗口的终态替换，复用紧邻前态中的规则与冻结列表。事件与窗口仍为整体替换，没有在本轮改成逐字段差量。

提交载荷写版本2、读版本1和2；读取旧提交时将原运行态提升为Full，允许同一个文件混合两个版本。快照、网络协议与外层日志包裹仍为版本1，世界子体仍为布局3。新读取器无需转换旧文件即可追加新记录；旧版只读版本1的程序不能读取新记录。完整字段登记见[Execution格式](../../../crates/codec/EXECUTION.md)。

不可变列表的编码字节、解码拥有字节及条目成本按Arc身份缓存；字段变化触发写时复制并重新计算。动态事件、窗口和完整运行态预算每次复核，领域事件、请求与进度检查继续执行。预算缓存使用实际有界编码器和解码器计算成本，没有用压缩长度代替拥有的内存。

`set_runtime_optimization(false)`关闭进度记录及预算缓存，作为对照。该模式仍保留内存表共享，因此本轮完整模式是对记录与校验策略的同代码对照，不能解释为原提交每个内部步骤的精确复现。

### 二、批量刷盘

`drive_one`保持每条记录同步。新增drive_batch逐项执行prepare、try_stage、publish、立即accept及失效控制；达到记录上限、Tick边界、空闲或日志容量阻塞后，对已接纳前缀执行一次flush_through，再返回CommitBatch。

| 边界 | 行为 |
|---|---|
| 容量 | 每批1到1024条，仍受日志槽数、累计编码字节及单条编码预算限制 |
| 确认 | 返回成功前，整批已接纳前缀必须通过DurableSeq门槛 |
| 读取 | 可变驱动期间没有权威视图、请求结果、基线或复制出口；返回后再开放持久读取 |
| 撤权 | 每项accept后立即执行失效控制，不等待该批最后的刷盘 |
| Busy | 先同步已接纳部分，再由后续批次重试；未发布准备项不消耗身份或任务位置 |
| 追加或同步故障 | 停止实例，不返回该批成功，读取、结果、复制与检查点入口关闭 |
| 进程退出 | 未同步的内存批次可以丢失；已写出的完整连续前缀由恢复校验，不承诺未确认请求成功 |
| 关闭 | 原有有界排空和检查点路径保留，退出前完成同步 |

Server的Tick驱动默认每批1条；主程序默认16条并设置相应日志槽数，`--commit-batch 1`可回到逐条同步。没有增加异步I/O线程、并行发布或硬件断电保证。

MemoryBackend另改为有硬容量上限的几何扩容，避免每次小追加都精确扩容。已用10003次单字节追加验证内容与容量上限。该实现同时用于本轮各对照模式，本次没有单独测量扩容算法的加速比。

### 三、Release对照

复用serial_baseline的21组矩阵，每组预热5Tick后测30Tick，依次运行以下模式。

| 数据目录 | 运行态 | 每批记录上限 |
|---|---|---:|
| [01-full-runtime](../data/runtime-commit/01-full-runtime) | 完整记录和完整编码/解码校验 | 1 |
| [02-compact-runtime](../data/runtime-commit/02-compact-runtime) | 进度记录及不可变表成本缓存 | 1 |
| [03-batch-16](../data/runtime-commit/03-batch-16) | 进度记录及不可变表成本缓存 | 16 |

三种模式使用相同当前代码和最终状态验证，均经过真实Server、Journal编码、预留、接纳与同步接口。sink丢弃日志，memory保留日志且不执行设备同步，file执行真实文件追加与sync_all。文件场景结束后从初始检查点重放整个日志并逐字节比较最终执行快照。各模式和后端的最终状态摘要相同。

下表为每Tick驱动均值，单位ms，不含最后的验证和恢复。

| 场景 | 完整运行态 | 紧凑运行态 | 紧凑运行态，16条一批 |
|---|---:|---:|---:|
| sink，64项，无订阅 | 7.367 | 0.795 | 0.770 |
| sink，128项，无订阅 | 26.784 | 2.156 | 2.053 |
| file，空Tick | 24.464 | 24.726 | 1.999 |
| file，64项，无订阅 | 160.449 | 154.352 | 12.661 |
| sink，64项，8订阅 | 27.671 | 21.235 | 24.659 |

64项无订阅的sink准备耗时由6.214ms降为0.571ms，编码由0.968ms降为0.072ms。每Tick日志从669184字节降为35968字节；128项从2595808字节降为67808字节，主要的冻结列表重复写入已移除。

紧凑记录没有消除文件同步等待：64项逐条同步仍需76次屏障。16条一批后降为5次，Flush均值由151.450ms降为11.060ms；空Tick由12次降为1次，Flush约1.969ms。文件64项完整对照的驱动均值160.449ms，最终批量模式为12.661ms。

授权投影仍是多订阅负载的主要成本。64项8订阅的sink紧凑模式投影约20.290ms，批量模式约23.792ms；队列积累和运行时波动会影响该场景，本轮没有改动投影算法，也没有把它描述为批量模式的收益。

这是同一设备上的一次顺序运行，每场景30个测量Tick，不设跨设备性能承诺。原始数据每目录包含summary.csv、ticks.csv、samples.csv.gz和run.txt。三组分别保留241380、241380、200700项阶段样本，已核对调用数、耗时、峰值及编码字节汇总；每组630行逐Tick结果。

### 四、预验证决策与验证

另用prevalidation_baseline隔离测量冻结事务的restore及prepare_group，分别串行、1线程和4线程。每组预热10轮后测100轮；工作线程只借用同一只读世界，每项预留64KiB工作空间。结果只用于成本实验，不复用准备项进行发布，也未计入独立性分类成本。

| 独立简单事务数 | 串行P50，微秒 | 4线程P50，微秒 |
|---|---:|---:|
| 16 | 13.8 | 103.7 |
| 64 | 50.7 | 107.1 |
| 128 | 100.3 | 106.6 |

该负载尚无并行预验证收益，因此本轮保留串行准备和发布。准备项还绑定观察到的提交序号及阶段，不能把同一起点生成的多份PreparedChange直接按完成次序发布。后续若复杂碰撞或其他真实事务显著增加验证成本，再测分类和最终复核成本。完整结果见[预验证原始数据](../data/runtime-commit/prevalidation.csv)与[汇总](../data/runtime-commit/prevalidation-summary.csv)。

| 验证 | 结果 |
|---|---|
| 完整与进度记录 | 连续40个记录位置逐一恢复，世界和完整运行态相同，覆盖成功和争用拒绝 |
| 格式 | 版本1文件重开后追加版本2，再次重开成功；截断、尾随字节与伪造跳项拒绝 |
| 容量缓存 | 168组条目和字节边界与完整编码/解码结论一致，写时复制使旧缓存失效 |
| 批量背压 | 16条批次遇2槽日志时先同步已接纳前缀，继续推进至相同Tick和序号 |
| 故障 | 批中追加及同步失败不释放输出；只恢复完整连续前缀 |
| 撤权 | 慢订阅在批内失效，失效控制发生在追加和屏障前，不能再取走排队增量 |
| 进程退出 | 发布第8条、接纳前直接退出恢复至0；16条批次同步后退出恢复至16，继续Tick至20且没有重复修改 |
| 组合路径 | 1/4线程规划、批量提交、事件、请求结果、复制与恢复等价测试通过 |

工作区526项、服务端mods特性107项测试通过；运行态/批量与规划组合专项重复20轮，共240次测试通过。工作区和mods特性Clippy、格式检查通过。实际主程序默认批量创建至Tick 2，再以逐条同步重开至Tick 4，DurableSeq从24继续至48。远端CI尚未运行，相关重复测试及批量基准冒烟已加入CI。

## 复现

### 五、复现

```powershell
cargo test -p torse-server --test runtime_commit --test parallel_planning --locked
cargo test --workspace --all-targets --locked
cargo test -p torse-server --features mods --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p torse-server --features mods --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo run --release -p torse-server --example serial_baseline --locked -- `
    --output .work/runtime-commit/01-full-runtime --ticks 30 --warmup 5 --full-runtime
cargo run --release -p torse-server --example serial_baseline --locked -- `
    --output .work/runtime-commit/02-compact-runtime --ticks 30 --warmup 5
cargo run --release -p torse-server --example serial_baseline --locked -- `
    --output .work/runtime-commit/03-batch-16 --ticks 30 --warmup 5 --batch 16
cargo run --release -p torse-server --example prevalidation_baseline --locked -- `
    .work/runtime-commit/prevalidation.csv
```

输出目录和预验证CSV必须尚不存在。执行环境和验证计数见[environment.txt](../data/runtime-commit/environment.txt)。Gzip仅由本次测量CSV生成，本轮没有新增图像或其他媒体素材。

## 边界

未记录
