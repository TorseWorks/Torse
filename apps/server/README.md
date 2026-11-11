# 服务端

## 独立地形显示范围

`NetworkHub::enable_visual_interest`为连接设置不可变的只读批准范围、显示范围、中心与`VisualTerrainLimits`，初次近景订阅后交付。`set_visual_interest`在批准范围内移动，`refresh_visual_interest`重试缺失、损坏或预算拒绝的来源。库入口默认关闭，主程序不自动扩大视野。

批准区域可用`VisualTerrainApproval::rectangular(min, max)`表达，三个轴均含端点。对于分区边长16、高度区间`[-64, 320)`的世界，角点`(-32, -4, -32)`和`(32, 19, 32)`覆盖101400个坐标，矩形只占固定元数据。`contains`查询单点，`select_window(min, max, limits.max_sections)`返回与批准区域相交的当前窗口，交集超过预算时在枚举前整体拒绝。宿主可逐步请求小窗口，读盘按中心距离优先。原有`ReplicaScope`参数仍可使用，完整矩形会精确压缩，有缺口的范围继续按显式集合校验，显式元数据上限为16384个坐标。

已驻留页从耐久权威投影，非驻留页由每连接一个有界工作线程读取已有`SectionPages`，不触发装载、生成或模拟。只发送地形及所用状态，不发送实体、组件、分配器或运行状态。发送前重查来源标记和意向代数。批准收紧须撤销连接；身份切换关闭已启用显示的连接。关闭等待当前文件读取退出，不提供操作系统I/O取消。

缩小当前窗口时，客户端应用新代重置后移除窗口外页面，旧代异步读结果丢弃。服务端可在缓存预算内保留窗口外页面供回访，复用前重查来源。超出窗口页数预算或批准区域的请求返回错误，当前窗口继续交付。

当前窗口页数、缓存、在途I/O、批次内页面的逻辑字节和每次页数均独立受限。含控制元数据的完整编码消息受网络输出预算限制。共享MemoryPool在启用前预留缓存、I/O、三份批次工作空间和控制元数据。`visual_terrain_diagnostics`报告逻辑缓存和固定预留，不代表RSS。缓存不足时优先保留近中心页，其余明确返回Budget，避免循环逐出重读。客户端默认4096页、64MiB，可在首条Terrain前设置预算。场景与GPU另受自身预算约束。

协议6增加Terrain流，双方须同时升级，存档不变，见[格式](../../crates/protocol/FORMAT.md)。客户端将远景接入派生场景、网格和上传，近景覆盖远景，独立显示不修改权威复制镜像。

`cargo run --release -p torse-server --example visual_terrain_baseline -- NEW_OUTPUT_DIRECTORY`使用公共文件夹具测量24个已保存稠密分区，2个模拟驻留分区与8页显示窗的直行、掉头和回访。CSV记录CPU镜像首批/全批可用时间、最长poll、传输字节和逻辑内存；不等同于显示器呈现时间或RSS。回归另覆盖近景切换、恢复、损坏修复、积压和内存释放。

## 稠密原子安装诊断

`section_install_diagnostics`按最近一次`install_section_batch`记录`page_preparation`（页编码、摘要和引用）、`validation`（结构、内容、预算、恢复准备和完整记录成本）、`commit`（日志暂存编码、原子发布和失效通知，排除刷盘）、`durability_wait`与总`elapsed`。失败阶段同样记录；这些墙钟观察不参与权威决策。另用可选SerialProfiler的Encoding阶段细分最终日志编码，不能把全部commit时间当作纯发布。

批量准备借用原有页序列，保留只含身份和版本的发布令牌，不重复保存完整地形页。快照仍执行原有结构、内容、持久字段和预算校验；符号解析全部保持原ID时直接复制已验证的稠密ID序列，否则逐项重映射；直接建立最终紧凑布局，避免中间稠密统计。稠密wire数组按有界字节块处理，旧字节与条目/拥有量预算保持一致。调色板仅在实际包含需要附属实例的状态时扫描所有单元。完整记录成本由已测页列表和实际空列表记录包络精确合成，保留总编码字节、解码拥有量、条目和帧限制，日志写入仍执行原有完整编码。一次日志记录、一次原子发布和一次耐久屏障不变，提交格式仍为4，存档与协议不因这些优化变化。

`cargo run --release -p torse-server --example dense_install_baseline -- NEW_OUTPUT_DIRECTORY`初始化25列、每列24页，然后以50ms节拍驱动200次内核tick，在10.9格/秒的虚拟位移下安装30列。固定种子每列16页稠密地形和3个附属实体；使用真实文件日志、600页授权复制、编码/解码和镜像更新，窗口移动后卸载后方页。正常驻留600页、安装边界最多720页，每列断言一次flush请求，最终核对完整授权快照与重开恢复。

该公共夹具是内核复现模型，未包含外部宿主的游戏系统、Socket或原始生成参数。`--uniform`保留均匀页对照；`--unpaced`只供快速正确性检查，不用于定时长尾验收。夹具为稠密输入将runtime条目预算设为200000，并为600页复制和检查点设置2000000条目容量；字节、日志同步和恢复策略保持原文件策略，两版使用相同容量。保留所有样本，单独报告耐久等待；文件同步本身可能超过5ms，结果不构成硬实时承诺。

持久页在通过结构、预算和组件持久化校验后直接进入恢复，避免再次扫描同一快照。两状态分区在调色板更节省空间时，按字打包成一位调色板。后续同机对照保留了仍超出5ms的耐久等待样本，见[稠密分区恢复对照](../../benchmarks/archive/residency/dense-install-tail-20261111.md)。

包名：`torse-server`。提供权威世界的持久执行、文件恢复、授权网络和可选Mod装配，可在无GPU环境中运行。主程序通过ExecutionHost驱动阶段任务，WorldHost保留可信本地事务示例。范围与证据见[执行与网络验证](../../benchmarks/archive/network/B6-B7-execution-network.md)。

明文演示只允许回环地址。对外监听必须配置`--tls-cert`、`--tls-key`、`--client-ca`和`--users`，按验证后的证书指纹选择用户权限、范围和玩家，不能叠加全局演示授权。重复登录、错误或过期凭据及重放的拒绝语义见[用户认证与连接策略](../../benchmarks/archive/network/M5-user-authentication.md)。

主程序通过`--memory-mib N`设置共享逻辑内存上限，默认512MiB。世界、执行、存储、订阅、传输和并行工作使用同一MemoryPool；预算不足暂停可重试工作，提交序号保持不变，关闭控制消息保留专用空间。库宿主在恢复后、注册会话前调用`Server::attach_memory`，并使用`NetworkHub::with_memory`。预留口径、失败释放及不包含的内存范围见[共享内存记录](../../benchmarks/archive/performance/M6-shared-memory.md)。

## 兴趣范围增量

应用协议5的`Interest`与普通`Batch`共享订阅序号，允许在同一耐久游标连续切换。保留区域的未变地形直接复用，进入区域与实体字段按当前授权投影；重复、缺失、乱序更新返回`ResetRequired`。初次订阅和`NetworkHub::reset_policy`继续发送完整`Seed`。传输未排空时修改策略会关闭连接并要求重连，详情见[协议格式](../../crates/protocol/FORMAT.md#兴趣范围切换)。客户端与服务端须同时升级，不改变存档格式。

公开文件夹具比较256分区、224分区重叠窗口，六个独立release进程交替执行两种顺序，共72次切换。每次客户端镜像都与新生成的完整授权快照逐项相等，两条路径和各进程的状态字节哈希一致。完整Seed每次包含256页及224页交集地形，Interest只包含32页进入地形，交集地形载荷为零。

Windows 26200、Rust 1.93.1上，Seed正文为138208–138551字节，Interest为1856–2167字节。宿主切换调用耗时分别为0.273–0.451毫秒（中位0.346）与0.046–0.136毫秒（中位0.063）。该固定、热缓存夹具包含少量实体和非空地形；内存传输测量不含TCP/TLS帧头、Socket I/O、客户端应用和完整快照对照耗时，不代表复杂实体或冷缓存负载。

原始样本、环境与状态哈希见[测量归档](../../benchmarks/archive/network/interest-20261110/environment.json)。重跑：`cargo build --release -p torse-server --example interest_baseline --locked`，随后`python apps/server/examples/interest_baseline/repeat.py --binary <release-example> --output <new-archive> --work <new-fixtures> --build <build-commit>`。大状态字节只保留在work目录，归档保存样本和哈希。

宿主普通视野移动可调用`NetworkHub::try_change_interest(server, peer, scope)`，仅改变范围并保留actor授权。应用/TLS输出或源复制队列尚未排空时返回`InterestUpdate::Deferred`，不改变旧授权、订阅或序号，也不保存新请求。宿主继续poll并让客户端消费输出后，用当前所需范围重试；成功返回`Applied`并发送一次有序Interest，已完成的相同范围重试不再发送。投影失效返回`ResetRequired`，须显式重置或重连；只支持已建立的显式订阅，不接管自动玩家跟随。未知传输默认不报告空闲，适配器需实现`output_idle`才能使用。强制撤权继续使用`replace_policy`或`revoke`，不能等待该入口排空。

## 队列与进度诊断

库宿主向`Server::queue_diagnostics`、`NetworkHub::queue_diagnostics`或`peer_queue_diagnostics`传入`torse_types::QueueDiagnostics`读取已有账目。默认配置返回`NotEnabled`；显式使用`QueueDiagnostics::enabled(4096)?`启用一次读取，参数限制返回值的固定尺寸，包含外层`Diagnostic`，合法范围为1字节至1MiB。尺寸不足返回`BudgetUnavailable`。这些接口不分配快照缓冲、不保存历史、不增加生产路径计数器或时钟；调用方保留的副本、序列化和导出内存由调用方管理，不属于内核预留。

服务端快照包含待处理请求数、剩余冻结任务数、已调度事件数、复制订阅/批次/items/逻辑字节账、日志已接纳和暂存预留，以及published、accepted、durable进度。请求没有现成字节账，返回`NotCollected`；日志预留包含尚未接纳的暂存令牌，不能视为全都已持久。复制字节沿用保守的自有负载成本，不代表线缆字节或进程内存。定制权威、Journal和Transport未实现观察接口时返回`NotSupported`，未知值不填零。

NetworkHub按已有连接ID读取单个连接，汇总连接数包含尚待正常回收的关闭条目；不建立诊断连接表。连接快照包含待确认请求数、订阅数、已有控制消息负载账和传输队列。各对象依次取样，不能作为跨对象原子切面；共享内存传输尝试加锁，竞争时返回`Busy`而不等待。接口只观察，不poll、不消费、不刷盘。生产Journal与ExecutionHost复用现有读接口；自定义适配器须遵守相同只读约定。

本批不提供Socket实际I/O、驻留时间、累计值、峰值或历史；传输字节口径及TLS边界见[连接与状态复制](../../crates/net/README.md)。

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

范围变化后，已驻留窗口通过Interest保留订阅身份，只传进入分区、移除记录和受影响实体；需要加载新分区时重建基线，并把需要的分区合并到驻留意向。预算或来源暂时不可用时等待后续轮询。没有绑定玩家的观察者仍使用显式订阅。默认演示仅在已有有限世界中运行，不会因窗口移动而自动注册无限地形生成器。

## 串行计时

`Server::set_serial_profiler`可接入有界的`torse_diagnostics::SerialProfiler`，默认关闭。规划与准备、复制投影与发布分别计时并扣除嵌套成本，日志另计编码字节与刷盘耗时。计时只服务诊断，不改变规则或持久数据。

```sh
cargo run --release -p torse-server --example serial_baseline --locked -- --output .work/serial-baseline --ticks 30 --warmup 5
```

基准对照无存储sink、保留日志的内存后端和真实文件后端，覆盖空Tick、不同任务数、争用与订阅数量。输出目录必须不存在；原始CSV、边界说明和本机测量见[串行基线](../../benchmarks/archive/performance/M6-serial-baseline.md)。

## 冻结前并行规划

`ExecutionHost::set_parallel_planning(Some(PlanningConfig { workers: 4, max_in_flight: 4 }))`启用有界批次；Server提供同名入口，主程序可传`--planning-workers 4`。默认保留串行执行。规则通过`DurableRule::parallel_planner`返回`Some(self)`显式加入，编译器要求其实现Sync。现有Mod适配器继续在宿主线程执行。

普通规则先按拓扑次序规划，只在相邻且没有声明依赖时并行；请求规划使用普通输出剩余的phase_tasks槽位，未选中请求继续等待。工作线程借用同一阶段起点世界，全部结束后按原拓扑次序归并，再按既有稳定事务次序冻结普通工作。普通规则输出须符合阶段和运行态预算。到期事件在普通工作结算后独立分批规划，不与周期规划线程重叠。冻结缓存、逐项准备、日志接纳、发布、刷盘和恢复均沿用现有路径。

借用批次通过torse-runtime的run_scoped执行，每批创建并回收有限线程；轻规划会增加开销，启用前应实测。`cargo run --release -p torse-server --example parallel_baseline -- --output .work/parallel-baseline`生成串行、1和4线程对照，详见[并行规划记录](../../benchmarks/archive/performance/M6-parallel-planning.md)。

原生规则还可实现`DurableRule::plan_parallel`，在独占当前规划批次时通过`PlanningContext::map`细分任务。宿主在主线程调用调度入口，一次启动有限计算通道，各通道复用临时空间，按输入序号汇总结果；与其他规则并行时仍调用普通plan，避免嵌套创建线程。保留输入、结果及临时空间的可信估计超过既有规则预留时，map返回None供调用方串行计算。线程异常在全部回收后返回错误，冻结前继续检查任务与编解码预算。

默认掉落物规则已经接入该入口，`--planning-workers 4`也可细分单个物理规则。候选轮转、邻域合并与输出占位先按既定次序确定，随后计算独立运动；至少8项运动、全部使用原生固定碰撞体且预算可容纳时才启动计算通道。激活、其他碰撞体、小批次与预算不足沿用串行路径。`ExecutionHost::native_planning_stats`报告进程内批次数、物体数、峰值通道数和预算回退次数，恢复后重新计数。

掉落物计算视图借用同一阶段权威世界，持有稳定观察及解析状态，规划返回后销毁；事务继续串行校验和提交。实体存储、组件模式、存档和网络格式不变，独立ECS实验尚未接管权威状态。真实物体的完整Tick测量未显示稳定的整体加速，P95也有退化，默认继续串行。复现入口为`cargo run --release -p torse-server --example drop_baseline --locked -- --output .work/drop-baseline`，详见上述记录的原生掉落物补充。

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

ExecutionHost独占ReplicaSource，使用纯DurableRule规划系统和事件。阶段先冻结普通工作，再逐项原子记录世界变化、任务进度、事件和结果；Simulation阶段继续结算全部到期事件后才推进。未来输入的关闭拒绝可以使用独立Stopping记录，不越过截止边界执行下一Tick。旧Simulation保留参考用途，不能在日志外直接执行后再补写进度。

## 到期事件预算与进度

phase_tasks限制每批冻结任务数，事件队列总量另由events限制。普通规则每阶段只规划一次；已有冻结工作在恢复后直接续行。普通工作结算后，到期事件按`(tick, EventId)`升序取有界前缀，每次准备最多调用phase_tasks个事件回调。事件批次共享该批开始时的已提交世界，后续批次可观察前批效果，不重复规划普通规则。空普通输出可以直接冻结首个事件批次。

冻结前检查完整运行态预算，并预检保留任务表及各次成功结算的事件输出；组合超限会缩小批次，未选中事件仍在持久队列中。单个事件计划在当前队列下仍无法容纳，或回调返回非法数据时明确报错。单次可信回调自身的执行成本仍由实现方保证有界，宿主不能抢占任意原生回调。

每批先以Full记录冻结，再以Progress逐项结算。结束后可在同一Tick和阶段冻结下一批；全部到期事件处理完才推进阶段。每条记录继续遵守准备、日志预留、发布、接纳、刷盘的顺序，重开只重放记录，不调用规划器。尚未冻结的回调、以及预算探测后未接纳的回调可能再次调用，必须保持纯函数。

取消尚未规划的事件会直接移除队列项；取消同批已冻结事件时保留其任务位置，以Conflict空结算跳过全部效果。未处理事件的驻留声明持续生效，部分批次期间不能卸载依赖分区。检查点仍只接受Tick起点，批间检查点请求无修改地拒绝；已有检查点加逐项持久日志可恢复批间进度。编码兼容见[Execution载荷](../../crates/codec/EXECUTION.md)。

```sh
cargo test -p torse-server --test event_budget --locked
```

WorkPlan支持携带TransactionGroup。Mod机器自有状态与库存规划器返回的事务按来源组成一个任务，连同新唤醒共同预留、发布和刷盘。冻结任务保存稳定成员，恢复不重跑已规划或已结算的Guest。库存拒绝保留机器完成前的进度，不登记该计划的新唤醒；陷阱重试次数由持久事件载荷记录，停机以没有后续唤醒表示。ModStats只供诊断，不影响恢复后的执行。验证见[Mod原子执行](../../benchmarks/archive/mods/M4-mod-atomic.md)。

持久数据使用ExecutionCodec<ExecutionModel>，格式见[EXECUTION.md](../../crates/codec/EXECUTION.md)。原WorldCodec世界目录不自动迁移；不兼容数据会报错并保留。提交载荷写版本4、读版本1至4，快照写版本2、读版本1和2，应用协议为版本6。冻结列表只在完整记录中保存，后续进度记录替换有界事件与请求窗口。主程序仍使用有限范围，显式装卸接口另见下文，未进行硬件断电验收。

## 运行态与批量刷盘

默认共享不可变规则及冻结任务表，缓存其编码和解码预算成本；动态事件、结果及全运行态上限继续校验。逐任务记录保存进度，恢复仍重新核对终态，不重跑规则。`Server::set_runtime_optimization(false)`可切换到完整运行态记录和完整编码/解码校验作对照，内存表共享仍保留。

事件预算按身份和完整内容复用真实编解码成本，变化后重新测量，删除后移除缓存；事件合法性及完整运行态上限仍每次检查。缓存项受事件数限制，拥有载荷计费不超过运行态字节上限；预算与编码限制变化使缓存失效。进度记录直接构造，避免先复制完整运行态。实际1、16、64台Wasm机器的负载、争用与恢复对照见[真实多机器测量](../../benchmarks/archive/performance/M6-multi-machine.md)。

`drive_one`继续每条记录同步。`drive_batch`逐项执行准备、日志预留、发布、立即接纳及失效控制，在达到记录上限、Tick边界、空闲或容量阻塞时同步已接纳前缀，然后返回CommitBatch。追加或同步失败时停止实例，不返回该批成功，结果、基线、复制与快照入口关闭。容量Busy时先同步已接纳部分，后续可以继续重试。

Server的Tick驱动默认每批1条，可用`set_commit_batch_limit`设置1到1024。主程序默认16条，并设置相应日志槽数；`--commit-batch 1`可回到逐条同步。任何成功返回都要求该批完整前缀通过DurableSeq门槛，未确认的内存批次在进程退出后可能丢失。关闭流程仍有界排空并保存检查点。

复现对照与故障验证见[运行态与批量提交](../../benchmarks/archive/performance/M6-runtime-commit.md)。当前简单事务的并行预验证实验未显示收益，因此继续使用串行事务准备与发布。

NetworkHub通过应用协议版本6接收意图，可信actor由PeerPolicy和会话绑定。结果、查询、基线、增量及移动确认都经过DurableSeq门槛。撤权控制直接清空未发队列并关闭受影响Peer，慢连接不阻塞世界。图形客户端已接入授权镜像、编辑请求、独立移动输入和预测校正。双向TLS与按用户授权已接入，明文演示限制在回环地址。

## 批量驻留

库宿主在`enable_residency`后、I/O空闲时调用`set_residency_batch_pages(N)`启用有限批量，N为1至1024，默认1。调度器按分区坐标次序在单个世界自有Worker中准备加载或生成页；一次派发的页数还受空余驻留槽和`max_io_bytes`约束。沿用每页`4 * SectionPages::budget().max_bytes`的保守I/O预留，总额在派发前申请；共享池不足返回BudgetBlocked，不提高业务预算。配置改变会拒绝尚有在途或保留结果的状态。

权威线程在发布每页前重新核对当前意向、停放引用、修订与实体身份。已解码但暂存槽不足的剩余页保留在原预留内，下次poll继续，避免重读；每次调用最多处理已配置的页数，在日志容量允许时共用一次耐久屏障。默认单页仍返回Published；显式批量返回Batch，其中committed列出已确认分区、durability为该前缀进度，blocked或failure给出终止原因。损坏或过期准备结果可由后续意向重新选择；日志写入或sync错误停止宿主，须重新打开恢复实际完整日志前缀，不能原地重试或宣称整批回滚。

后台生成仍只接受不带实体的地形页。生成游标若因外部提交过期则重试；同一批次自身接受的前缀允许更新页的生成切面，再重算摘要，继续沿用原有逐页校验和恢复格式。带新实体的可信安装可使用单页`install_section`或下述可信批量入口。页面准备不阻塞poll，但接受已准备页会等待该批耐久确认；较大批次可能推迟首批可用时间，不能承诺所有延迟同时改善。

`cargo run --release -p torse-server --example residency_baseline --locked -- .work/residency-run`复用公开文件夹具，对照24次`install_section`/`reload_section`与批量调度，输出同步请求、首批/全部可用时间、最长调用及状态和日志一致性结果。可加`--batch-first`交替顺序。backend_sync_calls留空，表示未测实际后端同步；请求次数不解释为磁盘操作次数。测试覆盖24页、字节预算缩批、兴趣改变、过期结果、损坏页、部分append与sync失败恢复。

[六次独立Release进程的原始结果](../../benchmarks/archive/residency/20261110)交替单页/批量顺序，共24行样本，全部状态及日志哈希一致。构建fd2b084，Rust1.93.1、Windows11 build26200；使用生产文件宿主和默认4条日志容量，24个固定空地形页，I/O逻辑预留上限768MiB，未附加共享池。单位为毫秒，表内为逐进程最小至最大值；同步请求由24次降为6次。

| 场景 | 首批可用 | 全部可用 | 最长单次调用 |
|---|---:|---:|---:|
| 单页安装 | 0.512至0.702 | 5.831至10.336 | 0.511至1.426 |
| 批量安装 | 0.586至0.873 | 1.853至2.509 | 0.437至0.756 |
| 单页重载 | 0.423至0.598 | 11.467至12.771 | 0.647至0.979 |
| 批量重载 | 3.464至4.884 | 4.882至7.149 | 0.327至0.842 |

本夹具中批量减少全量等待，但重载首批延迟明显增加，因为Worker先准备整个派发批次。最长调用区间重叠，不宣称普遍改善尾延迟；复杂地形、带实体生成、冷缓存、其他平台和硬件断电不在测量范围。重跑入口为`python apps/server/examples/residency_baseline/repeat.py --binary <release-example> --output <new-archive> --work <new-fixtures> --build <build-commit>`；原始大状态和日志保留在work目录，归档只保存样本、环境与校验哈希。

## 带实体的可信批量安装

宿主预先准备的页面可通过`begin_section_installs(pages, snapshots, SectionInstallLimits { max_pages, max_bytes })`创建安装批次，再重复调用`install_sections(&mut batch, sink)`。页数上限为1至1024；字节预算覆盖保留输入及工作缓冲的保守逻辑计费，共享MemoryPool会在构造前预留该上限，随批次句柄释放。它不是实际RSS测量。所有输入必须来自当前同一生成切面且位置不重复，后台GenerationSource仍只支持无实体地形，不隐式改变后台调度契约。

输入快照中的实体ID解释为各自页内的非零有序临时符号。每页提交前，权威线程从当前分配器续号，重写实体及嵌套List/Record中的页内引用，再按原有内容、组件、位置和驻留规则校验。输入的next_entity_id不是最终编号来源；跨页或指向既有实体的引用暂不支持并明确拒绝。成功返回的InstalledSection含提交游标和临时ID到持久ID的映射。

日志容量可缩短单次接受前缀；成功调用共享一次耐久屏障，返回installed、durability、remaining及blocked/failure。日志背压保留未接受的原始页，重试时重新依据当前分配器准备，不消耗ID；非法页终止在该页，先确认此前前缀。批次外的权威提交或运行代数改变返回ResetRequired，宿主须明确放弃旧批次并重新准备。into_remaining可取回未接受输入，其后生命周期和预算归调用方。

append/sync失败停止宿主。batch.accepted列出已被内存权威接受的页，batch.durability仅代表最近成功确认的耐久水位；失败时不能把accepted全当作已耐久，也不能假定未确认部分全部回滚。必须重新打开目录，按实际完整日志前缀恢复页面与实体分配器，不把旧句柄接到新宿主上重试。测试覆盖24页背压续传、非法页前缀、外部推进、内存预留释放、部分append、sync确认失败，以及确认4页后强制终止子进程再续传。未进行硬件断电验收，不改变存档或协议格式。

公开streaming实体夹具的六个独立release进程交替执行两种顺序，24页、每页1实体。逐页/批量同步请求为24/6，总确认耗时区间为6.112–9.017/1.885–2.614毫秒，中位为6.999/2.274毫秒；批量总耗时包含句柄构造和输入预检。Windows 26200、Rust 1.93.1、日志暂存上限4条，批次逻辑预算8MiB，未附加共享MemoryPool。完整状态、实体编号、日志字节及重开结果一致，跨进程哈希相同。未测后端真实sync调用，不推广到复杂组件、冷缓存或其他硬件。样本见[可信安装归档](../../benchmarks/archive/residency/trusted-20261110/environment.json)，入口为`cargo build --release -p torse-server --example trusted_install_baseline --locked`及`python apps/server/examples/trusted_install_baseline/repeat.py --binary <release-example> --output <new-archive> --work <new-fixtures> --build <build-commit>`。

## 联网玩家

新世界可加`--players 2`创建两个带碰撞体及持久运动状态的玩家，配合`--fill`提供地面。重开时保留`--players 2`并去掉`--fill`，复用存档中的玩家；请求数量不能超过已有玩家数。演示程序把可用玩家依次分配给连接，断线后可重新分配。这是显式本地策略，正式身份验证另由宿主装配。

应用调用`NetworkHub::assign_player`在Hello前绑定已有实体，客户端不能选择实体。每个联网玩家额外消耗一个内部会话，用普通冻结任务与持久回执保存移动结果，移动序号不消耗普通动作请求序号。默认16个连接对应最多32个执行会话。保留动作标识`core:action/player-step`不接受普通网络Request调用。

服务端每50毫秒最多结算一步，水平速度4.3格每秒，重力28格每平方秒，起跳速度8.4格每秒，终端下落速度64格每秒。方向归一化，客户端时间提示不改变步长；静默输入继续重力，结算拒绝保持原位并要求校正。碰撞体和地形读取冻结为前置条件，位置和垂直速度在同一事务中提交。断线、运行代数变化及批准范围收缩会在冻结任务提交前再校验。

输入、传输、执行任务和回执分别受各自预算限制，存储阻塞不会推进输入确认水位，刷盘失败会关闭输出入口。授权范围更新复用`replace_policy`，空闲连接及已排空的源投影使用增量；显式`reset_policy`或源投影失效使用完整基线。待发传输数据会触发关闭并要求重连，防止撤权后发送旧授权数据。玩家移动序号继续递增。参数、限制及复现结果见[权威移动验证](../../benchmarks/archive/network/M5-authoritative-movement.md)。

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

## Durable flush experiment

`cargo run --release --locked -p torse-server --example flush_baseline -- --output .work/flush-one --backend file`
runs three fixed serial cases: no business edits, one block edit per tick, and sixteen
independent entity movements per tick. The public bootstrap content, normal ExecutionHost,
production Journal encoder/queue, default budgets and default one-record commit batches
are retained. Initial assembly, checkpoint creation, warm-up, verification, recovery and
CSV export are outside each complete `drive_tick` timing. Samples use fixed preallocated
storage (at most 1000 ticks per case). Each file case reopens through the production file
server and verifies the complete recovered state.

For three alternating independent release processes per backend and measurement mode:

```sh
python apps/server/examples/flush_baseline/repeat.py --output .work/flush-results --work .work/flush-worlds
```

The runner refuses existing directories, retains per-tick CSV, process commands, exit
codes, binary SHA256 and final-state SHA256, and reports per-process nearest-rank
percentiles. Measurement-enabled and disabled final states and file journal bytes must
agree. Each child has a 600-second timeout. Working worlds remain under `--work`; only
text measurements go under `--output`. Instrumentation-disabled counters are unavailable
(empty CSV cells), rather than measured zeros. Memory sync makes no crash-persistence promise.

`flush_requests` counts coordinator calls; `durable_hits` counts successful already-covered
targets; `backend_sync_calls` counts actual adapter sync invocations. The file adapter calls
`File::sync_all` for every new durable prefix. `append_records/bytes` count successful
complete framed records; `file_write_calls/written_bytes` count returned file-write results,
including partial writes. File API calls do not prove device-level flush counts.
`append_ns` and `sync_ns` are disjoint children of `flush_ns`, which is contained in
`tick_ns`; these columns must not be added to their parents. Adapter observer bookkeeping
also contributes to the enclosing time. Freeze, advance and task counts explain the
durability work even in the absence of business edits. Invalid/failing runs exit nonzero
and must not be treated as completed timing samples.

The file adapter is explicitly **unsegmented**, equivalent to the legacy FileBackend write
and synchronization operations. It does not measure segmented rotation, reclamation,
checkpoint publication or directory synchronization. SegmentWriter may synchronize during
append on rotation, and its sync calls the directory helper (an OS directory sync on Unix;
currently a no-op on other platforms). Those paths need separate observation; this result
does not justify removing any barrier. Cache, scheduler and filesystem variability remain
part of the measurement; report repeated-process dispersion rather than a stable speedup.

Recorded run [20261105]: [raw samples and per-process summary](../../benchmarks/archive/flush/20261105/summary.csv),
[build and environment identity](../../benchmarks/archive/flush/20261105/environment.json).
Twelve independent release processes all exited zero, with 1080 measured ticks total.
Each backend/mode has three independent repetitions; all final-state SHA256 values agree
within a case, and all file cases reopen successfully. The instrumented process means were:

| Case | Memory tick ms, range of three means | File tick ms, range of three means | Requests / backend syncs per tick |
|---|---:|---:|---:|
| No business edits | 0.0263–0.0308 | 2.8361–2.9213 | 12 / 12 |
| One block edit | 0.0452–0.0668 | 3.0653–3.2528 | 13 / 13 |
| Sixteen entity moves | 0.2553–0.2874 | 7.3308–9.2279 | 28 / 28 |

Already-covered hits were zero in every measured tick. Each tick includes six frozen
phase batches and six phase advances, plus the business task records. These are different
commit positions, not repeated flushes of the same durable prefix. The covered-target
regression separately proves that repeated already-durable requests perform no backend sync.
The observed counts alone do not justify coalescing barriers or changing when success is
confirmed. No optimization was implemented.

File synchronization occupied approximately 91–96% of the instrumented process mean tick
times, including adapter observation overhead. The measured environment was Rust 1.93.1,
Windows 11 build 26200 and NTFS; affinity and power were uncontrolled and caches were not
evicted. These numbers apply only to this fixed experiment. Per-process instrumented-minus-
uninstrumented mean differences ranged from -0.6683ms to +0.6543ms across file cases. They
do not establish stable observer overhead or any speedup. CPU time was not separately sampled.

### 原子安装已生成分区

`Server<ExecutionHost,_,_>::install_section_batch(&pages, batch, &mut sink)`接收有序`Vec<Snapshot>`，将整批作为单条持久记录安装。默认单批上限24，`set_section_install_limit(1..=1024)`调整上限，其他输出、存储、内存与驻留预算保持原配置。每页使用当前权威生成切面；附带实体仍须遵守`install_section`的校验，编号按页顺序连续分配，不在此接口隐式重映射引用。

任何非法页面或总预算不足都拒绝整批，journal压力返回`CommitStep::Blocked`，不会发布前缀或消耗实体编号。调用方需要重试时保留原输入，确认生成切面仍有效；只有`CommitStep::Durable`表示整个批次到达持久位置。终止性存储错误使宿主停止，必须重开判定完整日志。`section_install_diagnostics()`保留最近一批数量、包含准备和持久等待的耗时以及失败原因，停止后仍可读取。

此接口区别于`set_residency_batch_pages`的有界前缀组提交。已有驻留、权限、tick和事件处理接口不变。提交布局版本4及兼容边界见[Execution载荷](../../crates/codec/EXECUTION.md)。复跑安装与恢复基准：`cargo test --release -p torse-server --test atomic_install --locked release_atomic_column_install_comparison -- --ignored --nocapture --test-threads=1`。该夹具使用一列24个均匀石块分区和实际文件后端，并直接统计journal追加与同步调用；不包含生成、传输或客户端呈现耗时。

[20261110]同机实测：Windows 11 10.0.26200，WD Blue SN580 1TB NVMe SSD，Rust 1.93.1，Release，单进程文件后端，交替顺序各六轮。24分区逐个安装中位数8.601ms，原子批次0.7395ms，单分区0.6765ms；整批约为单次1.09倍，较逐个安装快11.63倍。安装吞吐分别约2790和32454分区/秒。实际journal追加及同步调用从24次降为1次；新建checkpoint和输入页构造不在计时区间内，准备、发布、日志追加与同步均在区间内，每轮随后重开核对完整状态。夹具没有生成、传输或50ms tick循环，不能据此推断集成地形列吞吐；未进行硬件断电测试。原始样本、配置和源码指纹见[测量数据](../../benchmarks/archive/residency/atomic-20261110/environment.json)与[逐轮样本](../../benchmarks/archive/residency/atomic-20261110/samples.json)。
