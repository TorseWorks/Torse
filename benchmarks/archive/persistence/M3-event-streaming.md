# 事件感知的持久流送

日期：[20261027]
类型：验收
状态：已验收
环境：Windows11、x86_64、Rust1.93.1
范围：局部事件依赖、有界驻留调度、页I/O隔离、生成器身份及包含卸载分区的完整归档

## 目标

未记录

## 方法

### 一、行为

事件可声明固定分区或实体身份，宿主从持久事件与当前实体位置派生逻辑票据。旧事件保持全局驻留语义；显式空声明表示事件与分区无关。每事件最多64项，重复或非规范持久声明会拒绝。恢复校验装卸记录时使用同一声明，不调用处理器。

到期事件缺少分区时，执行停留在Tick起点。装载允许已排队输入，避免输入等待事件、事件等待装载。所有装卸仍在Tick起点提交，卸载还要求没有待结算输入或冻结规划。

调度器合并调用方兴趣与事件依赖，确定加载、生成或卸载目标。驻留数量和页I/O字节均显式配置；派发前核对页预算四倍的保守工作额度。一个Server只持有一个页工作线程、一个在途操作，单次轮询至多派发或发布一次。页读写和生成在工作线程执行，回到主线程后重新验证引用、实体和当前兴趣，再走原日志门槛。

I/O失败可重试，过期结果丢弃。在途页工作阻止页回收，工作线程在世界租约释放前关闭。日志刷盘仍同步执行，关闭可能等待当前文件调用完成。

## 结果

### 二、生成与归档

生成器的内容键、正整数版本及配置摘要保存到独立描述文件，首次启用显式注册。现有描述不匹配时拒绝启用，预检和启动也拒绝损坏描述。装配方可在WorldLease阶段核对预期身份，避免启动后才发现参数变化。摘要应包含种子和全部生成参数，替换语义需要独立迁移。

ResidencyArchive读取全部parked页，在总预算内与驻留快照合并，保留运行态及生成器身份。输出不会改变运行中的驻留集合。恢复时核对生成器描述，源页保持原样；超预算返回错误。它提供有界全量导出，磁盘流式归档仍需单独实现。

### 三、验证

完整工作区792项测试通过，零失败、零忽略。原始逐套计数见[workspace.csv](../data/event-streaming/01-bounded/workspace.csv)，配置与环境见[environment.txt](../data/event-streaming/01-bounded/environment.txt)。

启用Mod的服务端182项测试通过，计数见[mods.csv](../data/event-streaming/01-bounded/mods.csv)。格式、Clippy与相关API文档检查通过。

| 场景 | 结果 |
|---|---|
| 局部远期事件 | 只固定声明分区，独立分区可以卸载并重启恢复 |
| 实体与跨区依赖 | 实体移动后票据跟随位置，双分区声明同时阻止卸载 |
| 到期依赖缺失 | Tick暂停，生成装载后继续，已排队输入不造成互相等待 |
| 低预算与I/O失败 | 拒绝派发时序号不变，坏页修复后可以重试装载 |
| 阻塞生成器 | 主线程轮询不等待生成结果，兴趣变化后的旧结果丢弃 |
| 回收协调 | 在途工作禁止页清理，防止删除尚待发布的引用 |
| 生成描述 | 身份、版本及摘要变化均拒绝，损坏描述的只读预检不改原文件 |
| 完整归档 | 合并卸载页，实体、水位和分区一致，生成器核对及编解码往返通过 |

旧全局事件、已有分区页、装卸失败重试、实际进程退出和旧执行版本读取继续由原流送测试覆盖。

## 复现

### 四、复现

```powershell
$env:CARGO_BUILD_JOBS='1'
cargo test -p torse-sim event_dependencies --locked
cargo test -p torse-server --test residency --test streaming --test archive --test lifecycle --locked
cargo test --workspace --all-targets --locked
cargo test -p torse-server --features mods --all-targets --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
$env:RUSTDOCFLAGS='-D warnings'
cargo doc -p torse-server -p torse-sim --no-deps --locked
```

## 边界

### 五、兼容与边界

事件依赖沿用ComponentFields的保留字段，原事件和执行字段布局不变。旧宿主仍按全局规则拒绝带事件的卸载，新局部卸载历史需要新宿主重放。GenerationIdentity和ResidencyArchive是新增显式格式，原ExecutionArchive、WIT及网络协议不变。格式见[载荷说明](../../../crates/codec/EXECUTION.md)，接口见[服务端](../../../apps/server/README.md)。

可信规则必须完整声明读取范围，未知读取范围的规则继续保留全局语义。原生生成器仍负责遵守工作额度，页预算不能替代分配器限制。没有进行硬件断电、远程文件系统或多平台性能验收；本轮本机只验证Windows。
