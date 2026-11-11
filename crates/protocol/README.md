# 网络协议

包名：`torse-protocol`。版本6实现兼容握手、请求、结果、订阅、授权复制、提交绑定的呈现事件、同游标兴趣范围增量、独立只读地形范围及玩家移动消息。普通消息使用泛型包裹，应用层绑定Call/Reply及授权世界投影，协议不接收可信actor或管理员标记。

| 文件 | 职责 |
|---|---|
| [messages.rs](src/messages.rs) | `ClientMessage<I>`、`ServerMessage<O,S,C>`及显式消息标签 |
| [FORMAT.md](FORMAT.md) | 长度前缀、字段顺序、版本、接纳状态及身份规则 |

正文使用codec/wire版本1，TCP帧使用u32小端长度前缀。兼容的协议、规则和权威内容摘要是加入前置条件。结构解码拒绝未知标签、截断、超额和尾随字节；ActionService再执行权限、目标与参数检查。

Queued不表示成功，普通结果、查询和复制都等待DurableSeq。ReplicaSeq按订阅连续，不能用全局CommitSeq跳号判断丢包。撤权使用独立控制信号关闭连接并清队列，客户端重建基线。

应用集成位于[network.rs](../../apps/server/src/network.rs)；测试覆盖内存和真实回环TCP双客户端。登录、TLS、压缩和任意大消息分片不在本版范围。

客户端在Hello成功后每10秒发送无字段Ping，服务端只刷新接收活跃时间。Ping不产生回包、请求序号或持久记录。旧协议版本1至3在握手时拒绝，wire版本仍为1。默认60秒空闲窗口保留，停止心跳后连接正常回收。移动序号、确认与版本兼容见[格式说明](FORMAT.md)。
