# 应用协议版本6

## 独立显示地形

版本6追加Terrain标签10。Hello拒绝版本1至5，双方须同时升级；codec/wire版本1与存档格式不变。近景仍使用Seed/Batch/Interest。Terrain不包含实体，不授予交互权限。

TerrainUpdate字段依次为Binding、generation:u64、seq:u64、through:CommitCursor、reset:Option<TerrainReset>、pages:Vec<TerrainPage<S>>、unavailable:Vec<(SectionPos,TerrainUnavailable)>。TerrainReset为有序唯一sections和retained:Vec<(SectionPos,Digest)>。TerrainPage为at:CommitCursor、digest:Digest、terrain:S；S含单个分区、使用到的状态字典和空实体列表。Digest为地形投影wire编码的SHA-256内容标记。可用性标签为0 Pending、1 Missing、2 Corrupt、3 Budget。

范围替换提高generation，从seq=1和reset开始；同代seq连续增长，through可相等但不能倒退。新范围只能保留reset列出且标记一致的页；保留证明确认内容在through仍有效。其他旧页清除。近景变化设置显示页版本下界，晚到页低于下界时隐藏，近景始终优先。非法绑定、重复、乱序、无效引用和超额输入原子拒绝，应用丢弃连接镜像并重连，宿主重新批准范围。

显示批准范围由宿主设定，连接内不可收紧；撤销须关闭连接并清理输出。普通移动在批准范围内更换意向，积压时等待输出排空，丢弃过期后台结果。缺失或损坏不伪造空气、不安装权威分区。Terrain不保存事件历史。

每个TCP帧为u32小端正文长度 + 正文。零长度或超出本地frame上限拒绝。正文使用codec/wire版本u16=1，再跟u8消息标签和以下字段。TCP分段不等于应用分片，正文未收齐不解码。

| 客户端标签 | 字段顺序 |
|---|---|
| 0 Hello | protocol_version:u16=6、Compatibility |
| 1 Request | RequestSeq:u64、泛型输入I |
| 2 Subscribe | SectionPos的有界Vec |
| 3 Unsubscribe | SubscriptionId:u128 |
| 4 Close | 无 |
| 5 Ping | 无 |
| 6 Move | SessionId:u128、seq:u64、tick_hint:u64、wish_x:u8、wish_z:u8、jump:bool |
| 7 ControlOffer | api:u16 |
| 8 Control | seq:u64、payload的有界字节Vec |

| 服务端标签 | 字段顺序 |
|---|---|
| 0 Welcome | Binding、SessionId:u128 |
| 1 Admission | RequestSeq、接纳状态 |
| 2 Result | RequestSeq、泛型结果O |
| 3 Seed | ReplicaSeed<S> |
| 4 Batch | ReplicaBatch<C> |
| 5 Rejected | RejectCode |
| 6 Player | SessionId、EntityId、input_seq:u64、tick:u64、position_bits的3个u64、vertical_micros:i64、grounded:bool、reset:bool |
| 7 ControlReady | api:u16 |
| 8 ControlAck | seq:u64、accepted:bool |
| 9 Interest | ReplicaBatch<C>，其中presentation必须为空 |
| 10 Terrain | TerrainUpdate<S>，只读地形独立顺序流 |

接纳状态为0 + target_tick:u64、1表示Duplicate、2 + RejectCode表示Rejected。元数据和容器按codec显式基础编码，应用层本版绑定I=Call、O=Reply、S=ReplicaSnapshot、C=ReplicaChanges，见../codec/EXECUTION.md。

客户端不能提供可信actor、管理员标记或世界绑定。连接的世界、运行代数、session与actor来自宿主已认证记录。Hello必须先成功，未知版本、内容/规则摘要不兼容或非法消息拒绝。新请求序号从1连续增长，重发沿用原值，旧序号由权威窗口判断Duplicate、DuplicateMismatch或SequenceExpired。

Queued不表示动作成功。Result、Seed、Batch都要求其观察/提交位置不超过DurableSeq。只有A7授权源产生复制载荷，不发送完整存档。ReplicaSeq按订阅连续，CommitSeq可跳过隐藏变化。失效控制清空未发队列并关闭连接，客户端丢弃该连接镜像后重建。

首版只支持完整有界消息；没有压缩、任意大分片、登录协议或TLS消息。传输上限、速率、会话、订阅和输出队列策略在本地配置，未知字段或尾随字节不被忽略。

## 玩家移动

Move的水平意向以i8的二进制补码写入两个u8，服务端归一化方向。会话必须与Welcome一致，序号从1增长，与RequestSeq独立；tick_hint只供诊断，不决定步长或调度。客户端不提供实体身份、速度、位置或时间增量。

宿主在Hello前为连接分配已有玩家实体。Player首次下发绑定和输入水位0，此后只确认已持久结算的输入；位置及垂直速度来自当前持久世界。没有输入时仍按50毫秒模拟重力，每Tick每玩家最多一步。垂直速度按百万分之一格每秒编码，绝对值上限64000000，grounded为真时必须为0。实体位置与运动状态继续通过普通组件和EntityMove记录保存、恢复和复制。

队列溢出、速率拒绝或结算失败通过Player.reset要求重新校正；旧输入返回当前确认水位，重复输入不重走。已交执行器但尚未持久结算的序号不会提前确认。客户端只在已订阅镜像中存在玩家实体及碰撞体时预测，保存最多64条未确认意向，在确认后重放剩余意向，超出0.05格容差时跳变。该预测不修改镜像。

断线撤销父会话及内部移动会话，冻结任务在提交前重查绑定、运行代数与批准范围。重连创建新会话；订阅替换保留当前移动序号并建立新代数基线，旧代数增量不能进入新镜像。移动路径及碰撞读取涉及的所有分区必须处于批准范围。

外层消息标签及codec/wire版本1保持原布局，Batch正文增加呈现事件。Hello拒绝协议版本1至4和未知版本。协议升级不重写现有存档；存在玩家运动组件的新世界需要支持该组件的读取器。具体预算和验证见[权威移动](../../benchmarks/archive/network/M5-authoritative-movement.md)。

机器UI复用Seed/Batch中的Authorized组件和Request中的Call。机器对象及两个端口分别授权，引用端口身份不扩大读取权限。UI描述版本1是客户端编译期模型版本，独立于应用协议版本；未知描述版本清空视图。按钮ActionID与版本见[执行载荷](../codec/EXECUTION.md)，模型和场景见[机器UI验证](../../benchmarks/archive/mods/M4-machine-ui.md)。

## 会话存活

Ping仅在Hello成功后接纳，没有响应、不占RequestSeq、不写日志或产生请求结果；握手前发送立即关闭连接。客户端每10秒发送一次，由渲染帧和等待钩子共同轮询，同一间隔只发送一次，窗口最小化或没有重绘时继续轮询。

应用时序须满足`idle_ticks * tick_period >= 3 * heartbeat_interval`。演示配置为1200个Tick、50毫秒一个Tick、10秒心跳，空闲上限为60秒。停发后仍按原空闲规则回收连接。未握手连接也受空闲上限约束，Ping不能延长其占位。

Ping在应用协议版本2加入，当前协议版本6在Hello阶段拒绝旧版本混用。codec/wire版本保持1，既有标签顺序、存档及执行记录布局不变；未知消息标签仍拒绝。

## 提交呈现事件

版本4的ReplicaChanges依次为presentation、states、sections、removed_sections、entities、removed_entities。presentation是最多32项的完整有界列表，每项为PresentationId与PresentationDraft。

PresentationId依次为epoch:u128、cursor（WorldId:u128、CommitSeq:u64）、index:u32。epoch与cursor须匹配批次绑定和through，index严格递增且小于32；过滤其他受众的事件后可以跳号。PresentationDraft依次为kind:ContentKey、version:u32、section:SectionPos、`recipient:Option<EntityId>`、fields:ComponentFields。version不得为0。

事件由启动时注册的独立PresentationSchema校验。公开类型要求recipient为空；授权类型要求既有实体、订阅的实体授权及类型声明的permission。两类均要求批准范围包含事件锚点。载荷只有声明的基础字段，最多32字段，不接受List或Record，不从权威组件隐式复制。每项保守计费最多4096字节，每生产者每提交最多16项，整次提交最多32项及32768字节。队列另受ReplicaLimits限制，超额使整个订阅失效并清空待发状态和事件。

同一次事务或原子组的状态与事件共用一个Batch。没有可见状态变化时可以只发事件；没有匹配事件或状态时不产生空批次。普通规则无需RequestId即可产生事件，拒绝及未预留任务不发事件，Result、Batch仍等待DurableSeq。

ReplicaMirror.apply_with_presentation在完整状态与事件校验通过后一次返回该批事件。重复批次返回ResetRequired，不修改镜像或再次释放事件。apply保留只应用状态的用法并丢弃事件。客户端NetEvent::Increment携带经镜像验证的事件，消费者按kind/version选择处理器，未知类型或版本跳过，不转成服务器任务。返回值有界且不存历史；应用须在连接失效、重建基线或退出时丢弃尚未处理的旧连接输出。

Seed只包含状态。重连、订阅重建和新epoch都从新基线开始，不重播已提交事件。已提交但尚未发出的呈现事件是易失输出，重启时丢弃；冻结但未提交的任务保留草稿，恢复后首次提交按新epoch产生身份。音频设备、混音与粒子渲染不属于本通道。

## 兴趣范围切换

宿主的`try_change_interest`仅在旧actor授权下移动视野。待发应用/TLS输出或未消费源批次返回Deferred且不保存请求、不变更授权/订阅/序号，排空后显式重试。失效投影返回ResetRequired。该API不新增协议消息；成功仍使用Interest，强制撤权的关闭屏障不变。

版本5追加Interest标签9，沿用ReplicaBatch封装及ReplicaChanges字段。普通Batch仍要求世界through严格递增；Interest允许through等于前一游标，但禁止倒退，且与普通Batch共用同一订阅的连续ReplicaSeq。Interest不包含呈现事件。重复、缺失、乱序、旧订阅或旧绑定均返回ResetRequired，完整校验失败不修改镜像；应用须丢弃旧连接镜像并重新连接或请求完整Seed。客户端与服务端必须同时升级，Hello拒绝版本1至4，不改变codec/wire版本1或存档布局。

初次订阅和显式NetworkHub::reset_policy使用完整Seed。正常replace_policy在活跃、已取空源队列和空闲传输下保留订阅身份，发送进入分区、离开记录及实体/状态变化；未变交集地形不重新捕获或编码，实体字段仍按新授权逐项过滤。源队列尚有未消费批次或旧投影失效时退回完整Seed，丢弃易失呈现输出。玩家移动时批准权限不变，先按原顺序发送普通编辑，再发送Interest。

修改批准权限前，Transport::output_idle须确认没有未交付的应用/TLS缓冲；MemoryTransport检查共享出站队列，TCP检查未完成帧和TLS待写记录，未知适配器默认返回false。非空、竞争或未知状态直接关闭并清空连接，客户端重连取Seed，不能跳过协议控制帧或截断帧继续运行。已被对端读取或Socket接受的数据无法撤回，空闲判定不表示网络对端已确认接收。库宿主绕过NetworkHub调用change_session_interest时须自行执行同样的下游输出屏障。

范围进入尚未驻留的页、驻留失效、超预算和编码/发送失败可能要求重建基线或关闭；协议不保存兴趣历史，不追溯播放事件。所有成功切换仍受既有快照、复制输出和连接预算限制，未确认世界进度不外发。
