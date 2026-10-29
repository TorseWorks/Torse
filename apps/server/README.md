# 服务端

包名：`torse-server`。提供权威世界的持久执行、文件恢复、授权网络和可选Mod装配，可在无GPU环境中运行。主程序通过ExecutionHost驱动阶段任务，WorldHost保留可信本地事务示例。范围与证据见[执行与网络验证](../../benchmarks/archive/network/B6-B7-execution-network.md)。

明文演示只允许回环地址。对外监听必须配置`--tls-cert`、`--tls-key`、`--client-ca`和`--users`，按验证后的证书指纹选择用户权限、范围和玩家，不能叠加全局演示授权。重复登录、错误或过期凭据及重放的拒绝语义见[用户认证与连接策略](../../benchmarks/archive/network/M5-user-authentication.md)。

主程序通过`--memory-mib N`设置共享逻辑内存上限，默认512MiB。世界、执行、存储、订阅、传输和并行工作使用同一MemoryPool；预算不足暂停可重试工作，提交序号保持不变，关闭控制消息保留专用空间。库宿主在恢复后、注册会话前调用`Server::attach_memory`，并使用`NetworkHub::with_memory`。预留口径、失败释放及不包含的内存范围见[共享内存记录](../../benchmarks/archive/performance/M6-shared-memory.md)。

| 文件 | 职责 |
|---|---|
| [authority.rs](src/authority.rs) | 服务端本地权威、准备项、独立恢复和订阅失效适配接口 |
| [checkpoint.rs](src/checkpoint.rs) | 检查点保留、维护与存储健康状态适配 |
| [reclamation.rs](src/reclamation.rs) | 检查点、日志与分区页引用追踪及回收 |
| [residency.rs](src/residency.rs) | 事件票据、驻留调度及有界页I/O工作线程 |
| [generation.rs](src/generation.rs) | 生成器身份、版本和配置摘要的持久校验 |
| [coordinator.rs](src/coordinator.rs) | 预留、发布、接纳、刷盘、持久确认、检查点与有界关闭 |
| [directory.rs](src/directory.rs) | 世界目录排他锁与运行代数分配 |
| [lifecycle.rs](src/lifecycle.rs) | 文件世界创建、只读恢复检查、激活与日志重开 |
| [world.rs](src/world.rs) | 真实世界的可信本地事务、完整记录、有界快照与独立恢复适配 |
| [execution.rs](src/execution.rs) | 持久阶段工作、定时事件、动作结果、去重及完整执行恢复 |
| [profiling.rs](src/profiling.rs) | 串行执行的可选计时接线 |
| [planning.rs](src/planning.rs) | 冻结前的独立规则并行与稳定归并 |
| [prevalidation.rs](src/prevalidation.rs) | 冻结后的有界移动与组件预验证及串行提交前复核 |
| [network.rs](src/network.rs) | 会话策略、输入适配、持久输出门槛、订阅与失效控制 |
| [bootstrap.rs](src/bootstrap.rs) | 有限演示世界及显式容量配置 |
| [main.rs](src/main.rs) | 非阻塞监听、固定逻辑Tick和有界关闭 |

## 演示物品库存

[20261029]

新建基础世界时，`--players N --editors --fill`为有限玩家提供36槽初始库存。回环连接仅获得所分配玩家的库存实体授权；物品动作把地形和库存一起提交，重试及文件恢复不重新发放物品。没有玩家绑定的演示编辑者保留显式创作编辑策略，访客没有编辑权限。认证连接按用户策略配置库存实体和权限，不能使用全局演示授权。

规则及接口见[物品模块](../../modules/items/README.md)。默认演示的内容与规则身份现已包含物品模块；旧世界需要新目录或显式迁移，不能直接替换摘要装载。存储和网络编码未变。

## 玩家兴趣跟随

`--players N --follow-radius R`按玩家已持久提交的位置更新水平兴趣窗口，`R`范围为0到64；省略`--follow-radius`时保留原固定订阅行为。API使用`NetworkHub::enable_player_follow`，必须在接纳连接前启用，并先装配服务端驻留调度器。窗口始终是用户批准范围的子集，按距离选择垂直分区并受数量上限约束，不扩大actor权限或用户授权。

范围变化后，网络层替换会话的兴趣范围、重建基线，并把需要的分区合并到驻留意向。预算或来源暂时不可用时等待后续轮询。没有绑定玩家的观察者仍使用显式订阅。默认演示仅在已有有限世界中运行，不会因窗口移动而自动注册无限地形生成器。

## 串行计时

`Server::set_serial_profiler`可接入有界的`torse_diagnostics::SerialProfiler`，默认关闭。规划与准备、复制投影与发布分别计时并扣除嵌套成本，日志另计编码字节与刷盘耗时。计时只服务诊断，不改变规则或持久数据。

```sh
cargo run --release -p torse-server --example serial_baseline --locked -- --output .work/serial-baseline --ticks 30 --warmup 5
```

基准对照无存储sink、保留日志的内存后端和真实文件后端，覆盖空Tick、不同任务数、争用与订阅数量。输出目录必须不存在；原始CSV、边界说明和本机测量见[串行基线](../../benchmarks/archive/performance/M6-serial-baseline.md)。

## 冻结前并行规划

`ExecutionHost::set_parallel_planning(Some(PlanningConfig { workers: 4, max_in_flight: 4 }))`启用有界批次；Server提供同名入口，主程序可传`--planning-workers 4`。默认保留串行执行。规则通过`DurableRule::parallel_planner`返回`Some(self)`显式加入，编译器要求其实现Sync。现有Mod适配器继续在宿主线程执行。

请求与定时事件先按原次序规划，周期规则只在相邻且没有声明依赖时并行。工作线程借用同一阶段起点世界，批次全部结束后按原拓扑次序归并，再使用原稳定事务排序。每批最多max_in_flight项，原生规划器须遵守阶段和运行态预算。冻结缓存、逐项准备、日志接纳、发布、刷盘和恢复均沿用现有路径。

借用批次通过torse-runtime的run_scoped执行，每批创建并回收有限线程；轻规划会增加开销，启用前应实测。`cargo run --release -p torse-server --example parallel_baseline -- --output .work/parallel-baseline`生成串行、1和4线程对照，详见[并行规划记录](../../benchmarks/archive/performance/M6-parallel-planning.md)。

## 移动预验证

`ExecutionHost::set_prevalidation(Some(PrevalidationConfig::default()))`启用冻结任务的只读碰撞预计算，Server提供同名入口。主程序使用`--prevalidate-workers 4`，默认关闭。默认最多4线程、16项在途任务，每项预留8MiB；预计扫掠体积不足512格的轻任务继续串行准备。

只处理相邻、不同实体、单成员且仅含一次移动的任务。显式块前置、块写入、组件、生成、销毁与重复实体会截断批次。工作线程返回不可直接发布的MovementPrevalidation；宿主逐项检查阶段、兼容信息、碰撞读集、实体身份代数和精确前位置，再绑定当前提交序号。过期、超限或执行器失败均回到原串行准备，拒绝原因由原路径确定。

缓存只在内存中存在，已发布任务立即释放，表或阶段变化时清空，文件恢复重新计算剩余冻结任务。日志Busy重试复用缓存；提交、事件结算、撤权、批量刷盘及持久输出继续串行。`prevalidation_stats`报告预计算数、命中、过期、跳过及预留和缓存峰值，命中计数包含未发布重试。

大碰撞体负载有实测收益，小碰撞体默认跳过。完整路径、原始数据及真实单机器Mod对照见[移动预验证与投影](../../benchmarks/archive/performance/M6-movement-prevalidation.md)。

## 组件预验证

组件事务可通过`--prevalidate-workers 4 --prevalidate-components`显式启用，默认关闭。该CLI组合将每项工作及结果预留设为64MiB，默认窗口16项，最大预留1GiB；实际内存不等于预留。API对应`PrevalidationConfig.components`、`min_component_bytes`及`max_job_bytes`。默认跳过前后组件载荷合计不足16KiB的任务。

支持仅含组件修改的单成员或跨模块组，各任务涉及的实体必须互不重叠。显式块前置、块写入、移动、生成和销毁截断窗口。工作线程按原所有权和模式约束准备，宿主提交前复核世界、阶段、兼容信息、身份代数、旧字段与最终组件容量。令牌按所有权移入准备项，避免再复制完整字段树；日志Busy导致未发布准备项丢弃后，该项按原串行路径重试。恢复丢弃缓存并处理剩余冻结任务。

`prepare_group`同时复用成员已经完成形状校验的ComponentBatch，合并时继续检查重复目标、总量、旧值与最终容量，保留原成员拒绝次序。该优化始终启用。组件并行在完整路径中的收益有限，P95也可能变差，应按实际负载测量后启用。`peak_cached_bytes`对组件令牌记录保守工作计费，不表示实际常驻内存。对照与范围见[组件预验证记录](../../benchmarks/archive/performance/M6-component-prevalidation.md)。

## 授权增量投影

复制源默认根据提交实际修改的分区和实体更新订阅，只重建改变的Section及实体可见组件集合。状态引用计数维护完整可见字典，缓存成本继续检查整个投影的条目和字节上限；队列也在克隆载荷前检查。每个可见提交仍输出一个原子批次，撤权、慢订阅重置和DurableSeq发送门槛保持原语义。

`Server::set_projection_optimization(false)`可切换到完整投影作诊断对照，ExecutionHost及ReplicaSource也提供同名入口。重新启用时从当前订阅基线重建派生缓存。缓存不进入存档或协议；运行期地形装卸由独立的持久驻留接口处理。测量、容量边界和编码一致性验证见[增量投影记录](../../benchmarks/archive/performance/M6-incremental-projection.md)。

## 启动与连接

```sh
cargo run -p torse-server -- .work/execution-world --listen 127.0.0.1:7777
cargo run -p torse-server --example headless_client -- 127.0.0.1:7777
```

第二条在另一终端执行，需保持服务端运行。默认或`--ticks 0`持续运行，指定`--ticks 60`会在60个Tick后保存并关闭；同目录再次启动恢复Tick、事件、结果和任务位置，使用新epoch。演示创建两个Section及两个容器，对新连接使用只读访客PeerPolicy，因而headless_client看到实体但看不到库存组件。`--fill`仅用于新世界，把两个演示分区铺一层石头地面，方便图形客户端连上后有地形可见；已有存档的世界使用该参数会报错。`--editors`对新连接授予订阅分区内的编辑权限（ActorContext分区授权），配合图形客户端可完成经服务端提交的放置与破坏。`--revoke-after N`以首个连接为锚点，N个Tick后撤销全部会话并关闭传输，用于验收撤权与重连；行为与验证见[授权验收](../../benchmarks/archive/network/A8-acceptance.md)。

ExecutionHost独占ReplicaSource，使用纯DurableRule规划系统和事件。阶段先写完整冻结任务，再逐项原子记录世界变化、任务进度、事件和结果，最后推进一个阶段。未来输入的关闭拒绝可以使用独立Stopping记录，不越过截止边界执行下一Tick。旧Simulation保留参考用途，不能在日志外直接执行后再补写进度。

WorkPlan支持携带TransactionGroup。Mod机器自有状态与库存规划器返回的事务按来源组成一个任务，连同新唤醒共同预留、发布和刷盘。冻结任务保存稳定成员，恢复不重跑已规划或已结算的Guest。库存拒绝保留机器完成前的进度，不登记该计划的新唤醒；陷阱重试次数由持久事件载荷记录，停机以没有后续唤醒表示。ModStats只供诊断，不影响恢复后的执行。验证见[Mod原子执行](../../benchmarks/archive/mods/M4-mod-atomic.md)。

持久数据使用ExecutionCodec<ExecutionModel>，格式见[EXECUTION.md](../../crates/codec/EXECUTION.md)。原WorldCodec世界目录不自动迁移；不兼容数据会报错并保留。提交载荷写版本3、读版本1至3，快照写版本2、读版本1和2，应用协议为版本3。冻结列表只在完整记录中保存，后续进度记录替换有界事件与请求窗口。主程序仍使用有限范围，显式装卸接口另见下文，未进行硬件断电验收。

## 运行态与批量刷盘

默认共享不可变规则及冻结任务表，缓存其编码和解码预算成本；动态事件、结果及全运行态上限继续校验。逐任务记录保存进度，恢复仍重新核对终态，不重跑规则。`Server::set_runtime_optimization(false)`可切换到完整运行态记录和完整编码/解码校验作对照，内存表共享仍保留。

事件预算按身份和完整内容复用真实编解码成本，变化后重新测量，删除后移除缓存；事件合法性及完整运行态上限仍每次检查。缓存项受事件数限制，拥有载荷计费不超过运行态字节上限；预算与编码限制变化使缓存失效。进度记录直接构造，避免先复制完整运行态。实际1、16、64台Wasm机器的负载、争用与恢复对照见[真实多机器测量](../../benchmarks/archive/performance/M6-multi-machine.md)。

`drive_one`继续每条记录同步。`drive_batch`逐项执行准备、日志预留、发布、立即接纳及失效控制，在达到记录上限、Tick边界、空闲或容量阻塞时同步已接纳前缀，然后返回CommitBatch。追加或同步失败时停止实例，不返回该批成功，结果、基线、复制与快照入口关闭。容量Busy时先同步已接纳部分，后续可以继续重试。

Server的Tick驱动默认每批1条，可用`set_commit_batch_limit`设置1到1024。主程序默认16条，并设置相应日志槽数；`--commit-batch 1`可回到逐条同步。任何成功返回都要求该批完整前缀通过DurableSeq门槛，未确认的内存批次在进程退出后可能丢失。关闭流程仍有界排空并保存检查点。

复现对照与故障验证见[运行态与批量提交](../../benchmarks/archive/performance/M6-runtime-commit.md)。当前简单事务的并行预验证实验未显示收益，因此继续使用串行事务准备与发布。

NetworkHub通过应用协议版本3接收意图，可信actor由PeerPolicy和会话绑定。结果、查询、基线、增量及移动确认都经过DurableSeq门槛。撤权控制直接清空未发队列并关闭受影响Peer，慢连接不阻塞世界。图形客户端已接入授权镜像、编辑请求、独立移动输入和预测校正。双向TLS与按用户授权已接入，明文演示限制在回环地址。

## 联网玩家

新世界可加`--players 2`创建两个带碰撞体及持久运动状态的玩家，配合`--fill`提供地面。重开时保留`--players 2`并去掉`--fill`，复用存档中的玩家；请求数量不能超过已有玩家数。演示程序把可用玩家依次分配给连接，断线后可重新分配。这是显式本地策略，正式身份验证另由宿主装配。

应用调用`NetworkHub::assign_player`在Hello前绑定已有实体，客户端不能选择实体。每个联网玩家额外消耗一个内部会话，用普通冻结任务与持久回执保存移动结果，移动序号不消耗普通动作请求序号。默认16个连接对应最多32个执行会话。保留动作标识`core:action/player-step`不接受普通网络Request调用。

服务端每50毫秒最多结算一步，水平速度4.3格每秒，重力28格每平方秒，起跳速度8.4格每秒，终端下落速度64格每秒。方向归一化，客户端时间提示不改变步长；静默输入继续重力，结算拒绝保持原位并要求校正。碰撞体和地形读取冻结为前置条件，位置和垂直速度在同一事务中提交。断线、运行代数变化及批准范围收缩会在冻结任务提交前再校验。

输入、传输、执行任务和回执分别受各自预算限制，存储阻塞不会推进输入确认水位，刷盘失败会关闭输出入口。授权范围更新复用`replace_policy`重建基线，玩家移动序号继续递增。参数、限制及复现结果见[权威移动验证](../../benchmarks/archive/network/M5-authoritative-movement.md)。

## 本地世界示例

WorldHost消耗一个初始化完成的WorldAuthority，后续只提供只读world访问。Server::commit_transaction接收借用的可信本地事务，准备时生成完整ChangeSet，仍由通用协调器执行预留、发布、接纳和刷盘。LocalCommit分别表示业务拒绝、存储预留受阻或持久提交。预留受阻时世界、序号、实体分配水位和调用方事务均保持原状，可重新调用。

可信本地事务现已支持实体生成与销毁，可与其他实体的移动、组件修改及块编辑成组提交。生成身份在准备阶段按顺序提出，发布时才消耗；销毁保存完整旧状态并复核组件所有权。相同实体不能在一条记录中同时生成或销毁并移动或编辑组件。旧的块专用、无模式入口会拒绝生命周期工作，不能静默丢弃。

生成位置须在可读区域，坐标每轴不超过正负1048576格；生成记录不隐含碰撞体。每事务生成和销毁合计上限256，生命周期保守载荷上限8MiB，实际日志预算仍可更低。ActionService在动作权限扩展前拒绝处理器输出生命周期计划，ActionOutput::Mutation现持有Box<Transaction>。

Server::advance_phase在可信调用方完成当前阶段工作后，经相同日志门槛持久推进一个完整阶段。阶段0到5顺序推进，发布阶段完成后进入下一Tick的输入阶段，next_step保持0。进度记录使用空ChangeSet；普通世界事务保持当前恢复位置。预留受阻不改变进度，刷盘失败停止实例并禁止读取。该接口不调用Simulation或玩法系统，调用方仍负责确认阶段工作已完成。

WorldHostRecovery允许从日志恢复到Tick中间的完整阶段边界，拒绝跳阶段、倒退、非零步骤及同条记录同时推进和修改世界。检查点仍只在Tick起点捕获，关闭时把当前Tick剩余阶段依次写入日志，再保存下一Tick起点。LocalWorldModel的网络输入、输出和复制载荷使用Infallible，网络接纳入口无法构造请求。TransactionId只供诊断，不提供请求去重语义。待执行事件、动作结果与请求去重由独立的ExecutionHost处理，会话与订阅由NetworkHub管理；此本地模型不接入网络。

capture_snapshot_with_budget先借用世界检查条目和字节预算，再捕获完整快照。条目包括根、状态、分区、实体、组件、字段和嵌套集合项；字节包括结构、集合元素、字符串和稠密格值，宿主另计封装元数据。预算不包含分配器开销和临时捕获索引，编码预算与文件容量继续单独检查。世界全部驻留，运行期没有可变存储或卸载入口。

在实现仓库运行以下命令，再运行两次可验证恢复、销毁和身份续号。示例只使用基础内容夹具，每次切换同一格的石头与空气，并生成或销毁一个空组件实体，成功后保存检查点。新写入采用世界格式3，读取继续接受版本2数据。

```sh
cargo run -p torse-server --example durable_world -- .work/local-world
```

[真实世界测试](tests/world.rs)覆盖混合事务与库存争用、检查点和日志尾恢复、超限后重试、刷盘失败门槛、领域拒绝时保持文件尾部，以及两个进程中止位置。详细结果见[验证记录](../../benchmarks/archive/persistence/M3-local-world.md)。

阶段推进的逐阶段恢复、受阻重试、有界关闭、非法进度及进程退出验证见[阶段记录](../../benchmarks/archive/persistence/M3-phase-progress.md)。

## 提交与访问

Server拥有权威实例、日志、检查点和目录锁。HostAuthority是应用层适配接口，通过WorldAuthority、Recovery及公共契约接入领域实现。准备项必须拥有完整ModelCommit，丢弃后保留输入、事件、序号与持久身份，不能只提供CommitMeta。

drive_one依次准备、预留日志、发布、立即接纳日志、处理订阅失效、刷盘，最后返回DurablePublication。日志预留的Busy、TooLarge、InvalidRecord发生在发布前，返回Blocked，调用方可以处理背压或拒绝原因后重新准备。Stopped、发布不变量故障、接纳失败、失效处理失败和刷盘失败停止实例，不再次发布，也不返回持久确认。

刷盘结果必须匹配完整Binding，覆盖目标，且不超过权威位置。发布位置、恢复位置与准备记录也必须一致。服务端不提供可变权威引用；只读authority访问要求实例正常且当前位置已持久。submit先拒绝无关世界和旧运行代数。ExecutionHost与NetworkHub已负责会话身份、权限、速率及持久去重；Queued仍不构成动作成功承诺。

订阅失效通过独立InvalidationSink处理，发生在释放输出之前。没有复制连接时使用NoReplicas，收到非空失效列表会停止实例，避免漏处理。ExecutionHost/NetworkHub已接入普通结果和复制队列，原LocalWorldModel继续使用NoReplicas。FlushMetrics单独记录日志屏障调用次数及耗时，包含无须追加的调用，不混入权威执行时间。

## 文件启动

先校验FileServerConfig，再用WorldDirectory取得已存在根目录的排他锁。调用方负责首次创建根目录及其上级目录项的持久发布。Unix分支会同步已有根目录中的新文件与子目录项，Windows使用文件sync_all，硬件断电保证仍待平台验收。

create_file拒绝既有日志或检查点目录，创建并同步空日志，再保存位置0的完整初始快照。初始化成功后才返回可接纳输入的Server，失败不会开放半初始化世界。

open_file先检查文件与兼容信息，用Checkpoints::inspect读取完整manifest及快照。独立恢复构建器应用快照，再扫描旧日志或分段日志中仍需保留的连续记录，仅向构建器重放快照之后的部分。日志必须覆盖快照位置及manifest中已知的DurableSeq，持久保留点不得晚于可信快照。构建器finish校验预期位置，返回的权威实例也要匹配新Binding和扫描位置。

宿主同时保留最终ResumePoint：存在快照后的完整记录时取最后一条，否则取快照值。finish返回的Tick、阶段和步骤必须全部匹配，再激活存储，避免只恢复正确序号却重置执行进度。

领域恢复通过后才activate检查点，并重开日志完成不完整尾部修复与同步。领域拒绝、完整记录损坏、配置错误及预期位置不足时，快照、manifest与日志内容保持原状；本次尝试已消耗的运行代数仍保留。激活期间I/O失败可能留下已修复的合法尾部，实例仍不会开放。恢复不调用玩法处理器，不发送成功通知。

## 检查点维护

checkpoint_predecessors规定每次发布新快照前保留最近多少代已发布快照，至少为1。FileServerConfig要求快照文件容量严格大于该值，为新快照预留一个文件位置。每次捕获并核对快照后，先验证保留快照、原子整理manifest、清理旧快照与孤立文件，再将最早保留位置交给日志回收，最后发布新一代。当前检查点与其恢复所需后缀始终保留，发布成功后至多保留前代数加1份快照。

HostCheckpointStore是应用本地维护适配接口，沿用共同CheckpointStore的发布签名。prune_checkpoints提供目录锁保护下的显式清理入口，同时回收被全部保留检查点覆盖的日志段。首次回收启用分段布局，此后按`max_segment_bytes`与`max_segment_files`限制磁盘段，应用示例显式设置16MiB与1024个段。世界载荷和网络协议不变，布局及迁移见[存储格式](../../crates/storage/FORMAT.md)。

健康存储在写入前报告ResourceLimit时，checkpoint返回错误，Server仍可运行或维护后重试。若容量错误发生在部分写入之后，存储已停止，宿主仍按终止性故障处理。manifest整理释放历史引用占用，但保留记录加新记录仍须满足显式上限；关闭时无法发布检查点仍返回失败，不宣称保存成功。

文件版ExecutionHost提供`collect_section_pages(&pages, retain)`，要求空闲Tick边界及至少一代保留。它按指定保留代数保存当前快照并整理日志，逐份读取保留快照与连续日志后缀，追踪parked页与装卸记录引用，最后删除不可达页和中断发布留下的`.part`。当前驻留数据已完整进入快照，待处理装卸计划会阻止回收。删除前验证全部引用页及目录名，缺页、坏页和意外文件会拒绝清理；删除中断只留下不可达文件，重试安全。页目录必须专属于同一世界，并由调用方沿用该世界的排他租约。

## 目录与运行代数

```text
world/
    world.lock
    journal.log
    journal.log.segments/
        head
        0000000000000001.seg
    checkpoints/
        manifest.lock
        manifest.log
        checkpoint-0000000000000001.bin
```

world.lock同时是持有整个运行期的排他锁和追加式运行代数记录。文件保持原位，关闭后也不删除或替换。每次取得锁后验证记录，分配上一个代数加1，写入并同步后才返回WorldDirectory。使用同一世界目录的多进程互斥，普通路径别名也锁定同一个文件。目录锁是协作协议，不防御绕过锁的外部修改或同时运行的世界目录副本。

每条记录固定48字节，代数从1开始，各整数为小端。

| 偏移 | 字节数 | 内容 |
|---|---|---|
| 0 | 8 | magic与版本，ASCII文本TORSEEP1 |
| 8 | 16 | WorldId |
| 24 | 16 | WorldEpoch |
| 40 | 4 | CRC32C，覆盖前40字节 |
| 44 | 4 | 结束标记，ASCII文本EEND |

完整记录损坏、世界不符或代数不连续均拒绝，不回退。末尾未写完整的分配尚未对外返回，可以验证前缀后修复。max_epoch_bytes限制记录总长度，容量耗尽明确失败，不循环复用，不自动压缩或重置。校验复用`crc32c = "=0.6.8"`。

## 关闭

shutdown消耗Server，先停止接纳，通知权威适配器按包含截止Tick的规则处理已接纳输入。截止点之后的未执行输入返回Stopping，未来机器事件仍留在世界中。适配器必须把必要的Tick收尾推进形成日志，并在输入处理完且到达Tick边界时报告shutdown_ready。

关闭推进受shutdown_max_steps限制。持续Busy、无法预留、无进展、越过截止边界及其他故障都会返回失败。成功路径先完成日志屏障，再捕获完整快照并发布检查点，最后释放权威、日志和检查点句柄，目录锁最后释放。失败也会释放资源，不返回保存成功。

允许推进到截止Tick之后的第一个Tick起始位置，禁止进入该Tick内部的阶段或非初始步骤。HostAuthority::is_tick_start按锁定规则图判断候选ResumePoint，宿主在准备项发布前执行检查，避免关闭报错之前已经写入超出截止范围的变化。

## 验证与后续

[协调器测试](tests/coordinator.rs)检查调用次序、预留拒绝后的重试、失败后的持久门槛、输入身份、订阅失效、快照与有界关闭。[目录测试](tests/directory.rs)检查跨进程互斥、退出后解锁、逐字节截断与位翻转、代数递增和容量。[文件生命周期测试](tests/lifecycle.rs)覆盖恢复后续号、尾部保留、整条后缀缺失、初始化拒绝，以及发布后、接纳前突然退出。

首次回收前校验完整旧日志，之后按持久保留点校验仍需保留的段。回收、只读预检、旧布局和清理中断验证见[存储回收记录](../../benchmarks/archive/persistence/M3-storage-reclamation.md)。ExecutionHost和NetworkHub已补齐持久阶段任务、事件、请求结果、去重与授权连接，见[执行测试](tests/execution.rs)。页I/O与驻留调度见下文，日志刷盘继续同步执行，硬件断电保证仍需单独验收。

## Mod机器与UI模型

启用`mods`后可用`--mod DIRECTORY`重复加载包；创建和重开均须使用相同内容。显式`--machine-operators`演示策略对所有新连接授予机器及输入、输出端口读取和库存转移权限，默认不授予。图形客户端还需加载匹配内容并消费`torse_mod_api::ui`模型；双图形客户端的真实机器操作仍待接入。

```powershell
cargo run -p torse-server --features mods -- .work/machine-ui-world --mod examples/mods/machine --machine-operators
cargo run -p torse-server --features mods --example headless_client -- 127.0.0.1:7777 --mod examples/mods/machine
```

以上两条命令分终端运行；首次使用前按[Mod说明](../../examples/mods/README.md)安装Wasm目标。重新打开同一世界时须保留相同包与内容。

公共模型从授权镜像构建进度与库存摘要，按钮复用`inventory:action/transfer`。`DurableRule::action_events`在冻结动作时规划方块通知，通知与动作共同持久结算，冲突和重发不重复派发。绑定方式为包数据的`[[block]] behavior = true`。复现、真实进程退出验证及限制见[机器UI验证](../../benchmarks/archive/mods/M4-machine-ui.md)。

## 客户端心跳

握手后Ping只刷新接收活跃时间，不回复、不占请求序号，也不写入世界日志。握手前Ping关闭对应连接，旧应用协议版本1在Hello时拒绝。wire版本与存档布局保持不变。

图形客户端每10秒发送一次，从渲染帧和等待钩子驱动，最小化窗口仍轮询网络。演示启动检查`idle_ticks * tick_period >= 3 * heartbeat_interval`：默认1200个Tick、50毫秒一个Tick，空闲窗口60秒。NetworkLimits::validate_timing供其他真实时间宿主在装配时核对配置；使用虚拟时钟的测试可自行安排节奏。静默连接继续回收。验证见[心跳记录](../../benchmarks/archive/network/B7-heartbeat.md)。

## 持久分区装卸

ExecutionHost已接入Server::unload_section、reload_section和install_section。调用方先持有世界目录锁，再装配SectionPages；恢复器使用with_section_pages接入同一页目录。页先同步发布，装卸记录再走原日志门槛；受影响的授权Peer通过独立控制失效，未相交连接保持。

装卸限于没有冻结规划的Tick起点，卸载还要求没有待结算输入。装载允许已排队输入，避免到期事件等待分区时形成互相等待。旧事件保守阻止全部卸载；通过`EventDraft::with_dependencies`声明Section或Entity依赖后，只固定相关分区，实体票据跟随已提交位置。宿主从持久事件派生逻辑票据，既有SectionStore票据仍生效。到期依赖尚未驻留时暂停在Tick起点，装载完成后继续，不提前运行处理器。

未驻留页保留实体身份和持久字段，活动表移除对象，重新装回后水位不变。分区页与未驻留索引均有容量限制，孤立页可通过`collect_section_pages`清理。备份须包含日志、检查点、生成器描述及被引用的sections页。

可运行durable_stream示例逐次验证park、load和grow，命令、格式与场景证据见[持久分区装卸](../../benchmarks/archive/persistence/M3-section-streaming.md)。

## 驻留调度与生成

`enable_residency`接收ResidencyConfig、同世界SectionPages、可选GenerationSource及初始兴趣分区。`set_residency_interest`更新兴趣，调度器合并全部事件依赖后确定驻留集合。`drive_tick`自动轮询，也可调用`poll_residency`；每次至多派发或发布一个操作。驻留上限显式配置，单工作线程只允许一个在途操作，派发前预留页预算四倍的输入、编解码和交接空间。额度不足、驻留容量不足和没有可用来源分别返回明确状态。

页读写和生成在独立工作线程执行，主线程不等待结果。返回结果在发布前重新核对当前兴趣、世界绑定、页引用、实体身份及准备状态；过期结果丢弃，I/O失败保留权威状态并允许重试。工作线程由Server拥有，世界租约释放前会完成并关闭；文件系统调用不能强制取消，因此关闭可能等待当前操作。存在在途结果时拒绝页回收，避免删除即将发布的引用。显式手工装卸接口继续提供同步入口。

GenerationIdentity在`generation.bin`保存生成器内容键、正整数版本及参数摘要，种子和全部地形参数应进入摘要。已有描述不匹配时拒绝启用，普通启动和只读预检也校验描述文件完整性。装配方应在WorldLease阶段用`GenerationIdentity::verify`核对预期生成器，再开始运行代数；缺少描述时首次启用是显式注册。修改生成语义需要独立世界迁移，不覆盖现有描述。草稿文件不参与恢复。

旧`capture_archive`继续要求全部分区驻留。`capture_residency_archive`在持久Tick边界读取未驻留页，按总预算生成完整ResidencyArchive，保留生成器身份，不改变运行中驻留状态。恢复时须提供匹配的生成器描述；内容或规则迁移仍使用既有显式迁移入口。输出超预算返回ResourceLimit，源世界保持原状。事件声明、生成器描述与归档格式见[载荷说明](../../crates/codec/EXECUTION.md)，验证见[事件感知流送](../../benchmarks/archive/persistence/M3-event-streaming.md)。

生成页可以包含新实体及其初始组件。`Server::install_section`通过生成页准备路径调用`WorldAuthority::prepare_generated_section`，检查连续实体ID、分配水位和分区归属，将地形、实体、组件及新水位作为同一份装载记录提交。坏页和预算拒绝不会提前消耗实体ID；卸载后的普通页重载沿用旧身份。装卸记录的`SectionChange.fresh`区分新生成与既有页，恢复时沿用对应校验路径，不改变记录格式。

## 实体碰撞体

core:schema/collider提供实体轴对齐碰撞盒或关闭碰撞的声明。EntityCollider保存以百万分之一格表示的半尺寸，移动与放置共用声明，字段按普通组件进入日志、检查点、分区页和公开复制。创建时可设置初始声明，后续修改由core:module/physics来源提交，并复核最终位置和地形。无声明对象保留兼容行为，验证及限制见[实体碰撞体](../../benchmarks/archive/mods/M4-entity-colliders.md)。
