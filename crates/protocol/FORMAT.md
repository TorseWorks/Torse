# 应用协议版本3

每个TCP帧为u32小端正文长度 + 正文。零长度或超出本地frame上限拒绝。正文使用codec/wire版本u16=1，再跟u8消息标签和以下字段。TCP分段不等于应用分片，正文未收齐不解码。

| 客户端标签 | 字段顺序 |
|---|---|
| 0 Hello | protocol_version:u16=3、Compatibility |
| 1 Request | RequestSeq:u64、泛型输入I |
| 2 Subscribe | SectionPos的有界Vec |
| 3 Unsubscribe | SubscriptionId:u128 |
| 4 Close | 无 |
| 5 Ping | 无 |
| 6 Move | SessionId:u128、seq:u64、tick_hint:u64、wish_x:u8、wish_z:u8、jump:bool |

| 服务端标签 | 字段顺序 |
|---|---|
| 0 Welcome | Binding、SessionId:u128 |
| 1 Admission | RequestSeq、接纳状态 |
| 2 Result | RequestSeq、泛型结果O |
| 3 Seed | ReplicaSeed<S> |
| 4 Batch | ReplicaBatch<C> |
| 5 Rejected | RejectCode |
| 6 Player | SessionId、EntityId、input_seq:u64、tick:u64、position_bits的3个u64、vertical_micros:i64、grounded:bool、reset:bool |

接纳状态为0 + target_tick:u64、1表示Duplicate、2 + RejectCode表示Rejected。元数据和容器按codec显式基础编码，应用层本版绑定I=Call、O=Reply、S=ReplicaSnapshot、C=ReplicaChanges，见../codec/EXECUTION.md。

客户端不能提供可信actor、管理员标记或世界绑定。连接的世界、运行代数、session与actor来自宿主已认证记录。Hello必须先成功，未知版本、内容/规则摘要不兼容或非法消息拒绝。新请求序号从1连续增长，重发沿用原值，旧序号由权威窗口判断Duplicate、DuplicateMismatch或SequenceExpired。

Queued不表示动作成功。Result、Seed、Batch都要求其观察/提交位置不超过DurableSeq。只有A7授权源产生复制载荷，不发送完整存档。ReplicaSeq按订阅连续，CommitSeq可跳过隐藏变化。失效控制清空未发队列并关闭连接，客户端丢弃该连接镜像后重建。

首版只支持完整有界消息；没有压缩、任意大分片、登录协议或TLS消息。传输上限、速率、会话、订阅和输出队列策略在本地配置，未知字段或尾随字节不被忽略。

## 玩家移动

Move的水平意向以i8的二进制补码写入两个u8，服务端归一化方向。会话必须与Welcome一致，序号从1增长，与RequestSeq独立；tick_hint只供诊断，不决定步长或调度。客户端不提供实体身份、速度、位置或时间增量。

宿主在Hello前为连接分配已有玩家实体。Player首次下发绑定和输入水位0，此后只确认已持久结算的输入；位置及垂直速度来自当前持久世界。没有输入时仍按50毫秒模拟重力，每Tick每玩家最多一步。垂直速度按百万分之一格每秒编码，绝对值上限64000000，grounded为真时必须为0。实体位置与运动状态继续通过普通组件和EntityMove记录保存、恢复和复制。

队列溢出、速率拒绝或结算失败通过Player.reset要求重新校正；旧输入返回当前确认水位，重复输入不重走。已交执行器但尚未持久结算的序号不会提前确认。客户端只在已订阅镜像中存在玩家实体及碰撞体时预测，保存最多64条未确认意向，在确认后重放剩余意向，超出0.05格容差时跳变。该预测不修改镜像。

断线撤销父会话及内部移动会话，冻结任务在提交前重查绑定、运行代数与批准范围。重连创建新会话；订阅替换保留当前移动序号并建立新代数基线，旧代数增量不能进入新镜像。移动路径及碰撞读取涉及的所有分区必须处于批准范围。

标签0至5及codec/wire版本1保持原布局，但Hello拒绝协议版本1、2和未知版本。协议升级不重写现有存档；存在玩家运动组件的新世界需要支持该组件的读取器。具体预算和验证见[权威移动](../../benchmarks/archive/network/M5-authoritative-movement.md)。

机器UI复用Seed/Batch中的Authorized组件和Request中的Call。机器对象及两个端口分别授权，引用端口身份不扩大读取权限。UI描述版本1是客户端编译期模型版本，独立于应用协议版本；未知描述版本清空视图。按钮ActionID与版本见[执行载荷](../codec/EXECUTION.md)，模型和场景见[机器UI验证](../../benchmarks/archive/mods/M4-machine-ui.md)。

## 会话存活

Ping仅在Hello成功后接纳，没有响应、不占RequestSeq、不写日志或产生请求结果；握手前发送立即关闭连接。客户端每10秒发送一次，由渲染帧和等待钩子共同轮询，同一间隔只发送一次，窗口最小化或没有重绘时继续轮询。

应用时序须满足`idle_ticks * tick_period >= 3 * heartbeat_interval`。演示配置为1200个Tick、50毫秒一个Tick、10秒心跳，空闲上限为60秒。停发后仍按原空闲规则回收连接。未握手连接也受空闲上限约束，Ping不能延长其占位。

Ping在应用协议版本2加入，当前协议版本3在Hello阶段拒绝旧版本混用。codec/wire版本保持1，既有标签顺序、存档及执行记录布局不变；未知消息标签仍拒绝。
