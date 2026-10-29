# Execution载荷

组件持久投影不改变本文字段布局。持久宿主过滤世界快照与变更中的Never组件，并一致过滤实体生命周期观察；FrozenTask仍保留操作意图与前置值。恢复比对冻结计划时使用相同投影，自动方块实例生命周期另按格变化和启动配置核对。仅临时组件变化产生的空世界增量仍带有提交位置、执行进度和结果。验证见[内核一致性](../../benchmarks/archive/simulation/M2-core-consistency.md)。

这是B6执行模型和B7应用载荷的显式格式登记。实现真源为src/wire.rs的Wire实现及fields列表；不使用Rust内存布局。外层日志包裹仍按FORMAT.md的版本1，原WorldCodec的WORLD.md不改变。

所有整数小端。应用消息的wire载荷保持u16版本1。ExecutionCodec快照写入版本2、读取1和2，提交写入版本3、读取1至3。随后写入类型约定的字段，不含Rust类型名。Option为u8标签0/1，非法标签拒绝。Vec为u32项数后逐项编码，Map为u32项数后按键严格升序写入键/值。数量在分配前检查最小输入宽度、条目与拥有的字节预算。序列次序有语义，不自动排序输入。

ContentKey、ComponentValue与嵌套组件字段沿用WORLD.md。位置和位移为三个u64位模式；元数据布局沿用FORMAT.md。i32优先级以u32二进制补码表示。RejectCode标签按src/wire.rs的0至9固定映射；ReplyStatus为0已提交、1只读、2拒绝后跟RejectCode。

| 类型 | 按顺序的字段 |
|---|---|
| Call | action、version:u32、target、parameters |
| target | 0 + BlockPos，或1 + EntityId |
| Reply | status、fields、at:CommitCursor |
| Receipt | id:RequestId、input:Call、reply |
| Window | epoch、session、high:u64、receipts |
| EventDraft | tick、rule、payload:ComponentFields |
| TimedEvent | id:EventId、event:EventDraft |
| StableCell | pos、content（0空，1方块键与u16值列表） |
| StableMove | id、previous:[u64;3]、delta:[u64;3]、half:[u64;3] |
| StableSpawn | position:[u64;3]、components:`Map<ContentKey,ComponentFields>` |
| StoredTransaction | id、source、phase:u16、priority:i32、task_key:u64、local:u32、reads、writes、moves、components、spawns、despawns |
| StoredTransactionGroup | id:TransactionId、members:`Vec<StoredTransaction>`，来源严格升序、数量1到4、排序键一致 |
| Accepted | id:RequestId、input:Call |
| FrozenTask | transaction:任务标签、request:Option、consume:`Option<EventId>`、schedule、cancel、status、output |
| PhaseWork | tasks、next:u32 |
| SystemSpec | id、phase:u16、rule_priority:i32、before、after |
| RuntimeState | rules、next_event:u128、events、windows、work:`Option<PhaseWork>` |
| ExecutionSnapshot v1 | world:世界布局3的快照正文、runtime |
| ExecutionSnapshot v2 | world、runtime、parked:`Map<SectionPos,ParkedSection>` |
| ExecutionChanges v1 | world:世界布局3的变更正文、runtime:RuntimeState、outcome:`Option<ReplyStatus>`、cancelled:`Option<Accepted>` |
| ExecutionChanges v2 | world:世界布局3的变更正文、runtime:RuntimeUpdate、outcome:`Option<ReplyStatus>`、cancelled:`Option<Accepted>` |
| ExecutionChanges v3 | v2全部字段、section:`Option<SectionChange>` |
| ParkedSection | digest:32字节、entities:`Vec<EntityId>` |
| SectionChange | page:Snapshot正文、load:bool、fresh:bool、reference:ParkedSection |
| RuntimeUpdate | u8标签0 + 完整RuntimeState，或u8标签1 + RuntimeProgress |
| RuntimeProgress | next:u32、next_event:u128、events、windows |

组件替换ComponentEdit和销毁EntityState沿用世界领域字段顺序，字段值保留嵌套List/Record。运行句柄和进程状态编号不进入StoredTransaction，块前置条件也使用稳定内容。

B9为FrozenTask的transaction字段增加标签2。标签0表示无事务，标签1沿用原StoredTransaction布局，读取时提升为单成员组；标签2随后写组id与成员列表。单成员且组id等于成员id时继续写标签1，保持原字节不变；多成员或独立组id写标签2。其他Option仍只接受0/1。该补充保留提交版本1的原字段顺序；旧读取器遇任务标签2明确拒绝，新读取器可以读取既有单事务日志。

放置占用复核新增事务标签3：随后为组id、u32成员数，每成员写上述StoredTransaction正文，再写clearance列表（计数加StableCell）。整组合计最多256个条件，分配前检查累计数量；条件不得为空占用或重复位置，须与本成员最终写入的状态一致。没有条件仍使用标签1或2，解码旧标签得到空条件。带条件的成员不能单独按旧正文编码，避免条件丢失。新读取器读取0至3，旧读取器明确拒绝3；当前装卸补充使用Execution提交3及快照2，旧无条件任务字节不变。未完成任务恢复后执行空间条件，已提交结果不重跑碰撞。验证见[放置占用复核](../../benchmarks/archive/simulation/M2-placement-clearance.md)。

组成员数在解码分配前限制为1到4。编码、解码与领域恢复均检查来源严格有序、共同排序键、块和组件目标不重复、生命周期不交叠。冻结保存全部成员，恢复执行按原顺序还原后调用prepare_group；已经结算的日志只重放合并ChangeSet，不重跑Guest。组与事件、任务位置共同形成一条日志记录。此前B8排入的转移事件仍可结算，新机器计划不再生成这种延迟转移事件。

WorldSnapshot包裹先写SnapshotMeta，再写ExecutionSnapshot；CommitRecord先写CommitMeta，再写ExecutionChanges。包裹与world子体元数据必须一致。世界子体没有再次添加世界版本前缀，两个提交版本与快照版本1均固定解释为布局3。

## 运行态进度记录

冻结、阶段结束与关闭拒绝使用Full；逐任务结算默认使用Progress。后者复用紧邻前态中的规则声明和完整冻结任务列表，只保存有界事件、请求窗口的终态替换及任务位置。恢复要求已有冻结工作、next恰好增加1、阶段不变、有outcome且无cancelled，再按原结算规则重新计算终态并校验世界变更。缺少冻结列表、跳项、重复进度、伪造事件或结果均拒绝。读取版本1时把完整运行态提升为Full，允许同一追加日志混合两个版本。

当前快照写版本2，外层日志包裹保持版本1。新读取器可以重开旧快照和提交文件，继续追加提交版本3；旧程序不能读取新版本。

内存中的规则与任务列表使用Arc共享，编码仍写完整规范列表，不保存地址。解码另计共享容器及计数控制区的拥有字节。宿主可缓存不可变列表的完整编码、解码预算成本；事件和窗口每次重新检查，合并后的完整RuntimeState仍须满足原字节和条目上限。该缓存不改变磁盘字段或领域恢复规则。

授权复制使用相同基础词汇，ReplicaSeed按binding、subscription、baseline、state，ReplicaBatch按binding、subscription、seq、through、changes。ReplicaSnapshot和ReplicaChanges字段顺序按A7登记；Dense在此使用带计数的符号列表，因此网络编码条目预算可能比A7内存预检更严格。客户端必须通过state表解析符号，不能直接使用远端运行编号。

decode要求消耗完整输入，拒绝未知版本、标签、超额集合、非法内容键和尾随字节。StorageCodec执行包裹元数据核对，ExecutionRecovery执行规则、阶段、事件、结果和世界领域校验。不能把编码成功当作授权或持久确认。

## 机器UI与方块通知

B10沿用上述字段布局。UI只消费授权ReplicaSnapshot/ReplicaChanges，按钮发送既有Call：inventory:action/transfer、version=1、target为输入实体，parameters为source_slot=0、destination=输出实体、destination_slot=0、count=1。相同意图重发沿用原RequestId，不新建扣料请求。

动作通知保存在FrozenTask.schedule，成功结算时才分配EventId并进入RuntimeState.events。payload使用包命名空间的event/kind（Text，block-placed或block-broken）、event/block（Content）、event/values（`List<U32>`，各项为u16范围）、event/x、event/y、event/z（I64）。定义和有限值记录发生变化的原状态，不保存进程BlockStateId；破坏后不从当前空单元反推旧状态。通知与世界修改、结果和任务位置共记录，拒绝不登记，恢复不重复产生。

## 分区装卸

快照2追加未驻留页索引，旧快照1读入空映射。提交3追加可选装卸字段，旧提交1和2解释为None。bool使用u8的0或1，其他值拒绝。装卸记录的世界变更为空，运行态必须保持原值，阶段仍在同一Tick起点，禁止同时结算任务或请求。未驻留页保留实体ID，正常全局分配水位不回退。

分区页文件为八字节ASCII魔数TORSEPG1，随后是wire版本1的Snapshot。文件名使用整文件的SHA-256摘要和.page后缀；页只含一个分区，实体必须位于该分区。持久页省略Never组件。来源世界、兼容信息、页摘要、水位和最终索引均在开放世界前核对。验证和边界见[持久装卸](../../benchmarks/archive/persistence/M3-section-streaming.md)。

## 事件驻留声明

EventDraft字段布局不变。可选保留字段`torse:event/dependencies-v1`使用ComponentValue::List，各项仍是List，按以下形式编码。

| 依赖 | 列表项 |
|---|---|
| 固定分区 | `U32(0), I64(x), I64(y), I64(z)` |
| 实体所在分区 | `U32(1), Entity(id)`，id不得为0 |

每事件最多64项，分区项先按坐标升序，实体项随后按身份升序，不得重复。空列表声明事件与世界分区无关；省略整个字段保持保守的全局驻留语义。类型错误、非法身份和非规范顺序均拒绝。冻结任务与持久事件共同检查此字段，恢复只解析声明，不调用事件处理器。可信规则负责完整声明将要读取的分区；无法限制读取范围的规则继续省略声明。

宿主依据当前位置或未驻留实体索引派生票据，装卸记录不得卸载事件引用的分区。到期依赖缺失时在Tick起点等待。旧宿主仍会保守拒绝带有未来事件的卸载记录，因此包含局部卸载的新历史需要支持此声明的宿主重放；快照2、提交3与应用协议2的字段布局保持不变。

## 生成器与流送归档

`generation.bin`最多512字节：ASCII标记TORSEGN1、wire版本1编码的GenerationIdentity，最后追加前述全部字节的CRC32C。GenerationIdentity依次为ContentKey、version:u32及configuration:32字节；version必须大于0，摘要应覆盖种子及全部生成参数。先同步`generation.next`，再原子发布正式文件；草稿不参与预检或恢复。

ResidencyArchive是新的显式归档类型。wire外层版本1后写归档版本u16=1，再写完整ExecutionArchive和`Option<GenerationIdentity>`。其中世界快照合并当前驻留数据与全部引用页，保持实体身份、分配水位、事件及请求窗口。恢复前核对生成器描述，未知归档版本拒绝。原ExecutionArchive布局不变；内容迁移不能自动授权生成器语义变化。
