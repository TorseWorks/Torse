# 连接与状态复制

`ReplicaMirror::state()`只读访问已验证快照；原公开可写`state`字段已收为私有，调用方须从`ReplicaSeed`构造镜像并通过有序增量修改，独立编辑可显式克隆快照。该Rust API变化不改变网络或存档格式。增量先借用校验所有输入、顺序和受删除符号影响的保留地形引用，再预留元数据容量并就地提交。未变地形缓冲不克隆，空Interest且符号未删除时不扫描保留地形；新页、实体和状态字典仍完整校验。仅替换状态符号的定义、保留其ID时，保留单元格按新字典解释。

Release合成测量使用64/1024个16³密集页，在同一耐久游标连续应用空Interest。六个独立进程交替执行顺序，每组10次预热、100次采样，共2400行，逐次比完整预期状态并检查就地路径的缓冲身份。Windows 26200、Rust 1.93.1上，显式“克隆快照并全量校验”参考路径中位为0.432/5.369毫秒，就地路径为0.0002/0.0004毫秒；小值受时钟分辨率和固定开销影响。每次保留单元格的逻辑复制量从524288/8388608字节降为0，不包括元数据、分配器流量或校验读取；删除符号仍可能要求扫描保留地形。参考路径复现原机制，并非历史二进制。该结果不代表带大量新页的更新、渲染或整帧耗时。

样本和环境见[镜像归档](../../benchmarks/archive/network/mirror-20261110/environment.json)。重跑：`cargo build --release -p torse-net --example mirror_baseline --locked`，然后`python crates/net/examples/mirror_baseline/repeat.py --binary <release-example> --output <new-archive> --work <new-fixtures> --build <build-commit>`。

包名：`torse-net`。提供有界内存传输、非阻塞TCP、会话序号/速率和原子客户端镜像；应用层NetworkHub接入真实权威与持久门槛。

TcpTransport提供双向TLS，证书及应用协议验证完成后才报告对端指纹。记录I/O和帧均有界，关闭时释放预留。配置见[用户认证](../../benchmarks/archive/network/M5-user-authentication.md)。

| 文件 | 职责 |
|---|---|
| [transport.rs](src/transport.rs) | MemoryTransport、TcpTransport、完整帧队列和部分读写 |
| [session.rs](src/session.rs) | 运行期唯一会话、新序号连续性、重传与每Tick限额 |
| [replication.rs](src/replication.rs) | ReplicaMirror基线和完整增量校验、一次替换状态 |
| [transports.rs](tests/transports.rs) | 部分帧、超大前缀、背压和控制关闭验证 |

传输不参与世界发布。TcpTransport的长度检查发生在正文分配前，socket非阻塞，每次poll限制字节和最多64次读/64次写尝试。配置显式限制单帧、排队帧数、累计字节和每poll工作量。Partial/WouldBlock保留到下一次poll，错误只关闭对应连接。

MemoryTransport和TcpTransport都提供独立close，清空尚未发送的数据。NetworkHub还限制连接、空闲Tick、请求与订阅，按宿主PeerPolicy绑定权限；权威ExecutionHost负责持久请求去重。应用协议5保留握手后Ping，客户端每10秒发送，默认空闲上限仍为60秒。心跳由应用连接层驱动。

`Transport::queue_diagnostics`默认关闭，传入显式`QueueDiagnostics`读取固定尺寸值，定制传输默认返回`NotSupported`。MemoryTransport报告两端共享队列的负载字节，分别try_lock，竞争返回`Busy`；两次取样不保证原子性。TCP已完成入站帧按负载计字节，未完成输入单独报告，出站沿用已有剩余帧字节账（含尚未写出的长度前缀），不再次扣减部分写偏移。TLS沿用应用层明文帧账，已经交给TLS的内部缓冲、密文记录和Socket收发字节不在此范围。观察不驱动poll或握手，也不消费帧；关闭和正常排空后沿用现有队列状态，不提供累计流量或驻留时间。预算与进度语义见[服务端](../../apps/server/README.md#队列与进度诊断)。

ReplicaMirror先校验完整新状态再替换，拒绝错误Binding、重复/缺失ReplicaSeq、无效字典、Section尺寸或实体位置。连接关闭后应用应丢弃镜像并重新获取基线，不能继续使用旧权限状态。

端到端见[执行与网络记录](../../benchmarks/archive/network/B6-B7-execution-network.md)及[权威移动](../../benchmarks/archive/network/M5-authoritative-movement.md)。用户策略、移动和持久确认由应用层装配；当前使用固定内容和有限范围，UDP、多通道仍待接入。
