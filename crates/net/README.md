# 连接与状态复制

包名：`torse-net`。提供有界内存传输、非阻塞TCP、会话序号/速率和原子客户端镜像；应用层NetworkHub接入真实权威与持久门槛。

TcpTransport提供双向TLS，证书及应用协议验证完成后才报告对端指纹。记录I/O和帧均有界，关闭时释放预留。配置见[用户认证](../../benchmarks/archive/network/M5-user-authentication.md)。

| 文件 | 职责 |
|---|---|
| [transport.rs](src/transport.rs) | MemoryTransport、TcpTransport、完整帧队列和部分读写 |
| [session.rs](src/session.rs) | 运行期唯一会话、新序号连续性、重传与每Tick限额 |
| [replication.rs](src/replication.rs) | ReplicaMirror基线和完整增量校验、一次替换状态 |
| [transports.rs](tests/transports.rs) | 部分帧、超大前缀、背压和控制关闭验证 |

传输不参与世界发布。TcpTransport的长度检查发生在正文分配前，socket非阻塞，每次poll限制字节和最多64次读/64次写尝试。配置显式限制单帧、排队帧数、累计字节和每poll工作量。Partial/WouldBlock保留到下一次poll，错误只关闭对应连接。

MemoryTransport和TcpTransport都提供独立close，清空尚未发送的数据。NetworkHub还限制连接、空闲Tick、请求与订阅，按宿主PeerPolicy绑定权限；权威ExecutionHost负责持久请求去重。应用协议3保留握手后Ping，客户端每10秒发送，默认空闲上限仍为60秒。心跳由应用连接层驱动。

ReplicaMirror先校验完整新状态再替换，拒绝错误Binding、重复/缺失ReplicaSeq、无效字典、Section尺寸或实体位置。连接关闭后应用应丢弃镜像并重新获取基线，不能继续使用旧权限状态。

端到端见[执行与网络记录](../../benchmarks/archive/network/B6-B7-execution-network.md)及[权威移动](../../benchmarks/archive/network/M5-authoritative-movement.md)。用户策略、移动和持久确认由应用层装配；当前使用固定内容和有限范围，UDP、多通道仍待接入。
