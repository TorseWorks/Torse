# 世界载荷格式

版本：3。实施阶段：B5、B6。本文规定WorldCodec写入记录包裹的领域载荷，包裹本身见[记录包裹与元数据](FORMAT.md)。字段来源为A登记的Snapshot、ChangeSet与EntityLifecycle，分别位于torse-world的exchange与lifecycle模块。

版本3在变更集末尾增加实体生命周期记录，包含生成、销毁和分配器前置条件。快照主体沿用版本2的布局，仍保存实体分配水位。读取器继续接受版本2快照与变更集，旧变更集解释为无生命周期字段；新写入统一使用版本3。版本1缺少分配历史，继续拒绝。该格式仍为临时实现，尚未承诺正式存档兼容。

## 通用约定

整数均为小端，i64为二进制补码。载荷开头是2字节的世界格式版本，写入值为3，读取只接受2或3。其他值返回Incompatible，不猜测布局。该版本只描述领域布局，与包裹版本及Compatibility.authority_schema各自维护。解码必须恰好消费全部载荷，多余字节按数据错误拒绝。

| 记号 | 编码 |
|---|---|
| 计数 | u32，后接相应个数的元素 |
| 内容键 | u8长度加`namespace:path`的ASCII文本，读取后经`ContentKey::parse`校验 |
| 实体身份 | u128，不得为0 |
| 位置 | 三个u64，依次为x、y、z的IEEE-754位模式 |
| 文本 | u32长度加UTF-8字节，长度不超过`max_field_bytes` |
| 布尔 | 1字节，只允许0或1 |

内容键当前最长193字节，一字节长度足够。键长上限超过255时必须升级格式版本，不能直接放宽该字段。

## 快照

记录类型2。

| 顺序 | 字段 | 编码 |
|---|---|---|
| 1 | version | u16，写入3，读取接受2或3 |
| 2 | meta | 110字节SnapshotMeta，布局同B1 |
| 3 | side | u32，范围1到64 |
| 4 | min_y、max_y | 两个i64，要求`min_y < max_y` |
| 5 | states | 计数加StateEntry |
| 6 | sections | 计数加SnapshotSection |
| 7 | entities | 计数加SnapshotEntity |
| 8 | next_entity_id | u128，非0；最大值表示耗尽 |

| 结构 | 字段与顺序 |
|---|---|
| StateEntry | id:u16，非0；block:内容键；values:计数加u16 |
| SnapshotSection | x、y、z各为i64；cells标签u8，0为Uniform后接u16，1为Dense后接`side^3`个u16 |
| SnapshotEntity | id:实体身份；position_bits:位置；components:计数加SnapshotComponent |
| SnapshotComponent | schema:内容键；fields:字段表 |

next_entity_id保存包含已删除身份的分配历史，必须大于所有存活身份；u128最大值作为耗尽状态允许存活最大身份。恢复不能只按存活实体推导水位。外层记录包裹版本仍为1。

Dense不带长度前缀，单元数由side确定，单元按A登记的线性序`x + side * (z + side * y)`排列。状态表不登记编号0，该编号固定为空占用。

## 变更集

记录类型1。

| 顺序 | 字段 | 编码 |
|---|---|---|
| 1 | version | u16，写入3，读取接受2或3 |
| 2 | meta | 118字节CommitMeta，布局同B1，读写都检查`cursor = previous + 1` |
| 3 | edits | 计数加ChangeEdit，保持提交顺序 |
| 4 | entity_moves | 计数加EntityMove，保持提交顺序 |
| 5 | component_edits | 计数加ComponentEdit，保持提交顺序 |
| 6 | entity_lifecycle | 仅版本3：存在标签u8，0为None，1后接EntityLifecycle |

| 结构 | 字段与顺序 |
|---|---|
| ChangeEdit | x、y、z各为i64；content标签u8，0为Empty，1为Block后接内容键与计数加u16 |
| EntityMove | id:实体身份；previous_bits:位置；position_bits:位置 |
| ComponentEdit | id:实体身份；schema:内容键；previous与fields各为存在标签u8，0为None，1后接字段表 |

同一记录内实体最多移动一次，同一实体与模式最多编辑一次，读写两侧都检查。编码发生在发布之前，违反时事务不会提交，日志里不会出现恢复无法接受的记录。

## 实体生命周期

| 结构 | 字段与顺序 |
|---|---|
| EntityLifecycle | previous_next_id:u128；next_id:u128；spawned:计数加EntityState；despawned:计数加EntityState |
| EntityState | id:实体身份；position_bits:位置；components:计数加组件，每项为schema内容键与字段表 |

spawned保持请求顺序，身份必须从previous_next_id连续分配。next_id恰好等于previous_next_id加生成数量，溢出拒绝。水位最大值表示耗尽，不能再生成；销毁不回退水位。despawned保存原位置的精确位模式及原组件，持久宿主按下述策略投影组件，用于重放前置校验。组件和字段按内容键严格升序。

持久宿主在编码前执行组件持久投影：Never组件从快照、组件增量及生成和销毁观察中省略，Always保留，OwnerDefined采用启动装配时由所有者选定的策略，未选时保留。Codec自身仍可编码完整交换载荷。Recovery对旧完整记录采用相同投影，空投影也保留提交位置。

方块附属实例复用EntityState与普通组件字段，不增加布局字段。保留模式`core:schema/block_instance`包含`core:field/block`内容引用及`core:field/x`、`core:field/y`、`core:field/z`三个I64坐标，绑定始终持久保存。显式生成在前，按最终格坐标排序的自动生成在后，共用一个水位区间。验证见[内核一致性](../../benchmarks/archive/simulation/M2-core-consistency.md)。

生成与销毁合计不得超过256个目标，完整生命周期载荷的保守预算上限为8MiB，包含销毁观察中的组件。一个身份只能出现一次，生命周期目标不能同时参与本条记录的移动或组件编辑。读写两侧检查结构、重复、分配算式及目标交叠；实际水位、销毁观察、组件模式和生成位置由世界层检查。

版本2变更集在component_edits后结束，不带存在标签，读入时entity_lifecycle为None。版本3即使没有生命周期工作也必须写入标签0。版本2和版本3记录可以位于同一连续日志中，新快照统一写入版本3。

## 字段表与值

字段表为计数加若干组`内容键, 值`，键严格升序。SnapshotValue与ComponentValue共用以下标签：

| 标签 | 类型 | 后续内容 |
|---|---|---|
| 0 | Bool | 布尔 |
| 1 | U32 | u32 |
| 2 | I64 | i64 |
| 3 | Content | 内容键 |
| 4 | Entity | 实体身份 |
| 5 | Text | 文本 |
| 6 | List | 计数加值，保持原顺序 |
| 7 | Record | 字段表 |

未知标签按数据错误拒绝。标签编号属于持久格式，退役后不复用。

List与Record每层计一级嵌套，读写都受`max_depth`限制。世界接受的组件最多含9层嵌套容器，`WorldCodec::new`拒绝小于`MIN_WORLD_DEPTH`的配置，避免权威已接受的记录无法编码。

## 规范顺序

排序集合读写两侧都要求严格升序，相等视为重复并拒绝。比较规则沿用A登记的顺序：状态按运行编号，分区按x、y、z，实体按身份，组件、字段与Record条目按内容键。内容键先比较namespace再比较path，与整串`namespace:path`的字典序不同。

同一写入版本下，相同快照产生相同字节，解码后再编码得到原字节。读取版本2后重编码会使用版本3。重复键不会在解码时被映射表静默合并。

## 预算

| 项 | 计入内容 |
|---|---|
| 条目 | 记录本身，以及每个计数集合的每个元素：状态、分区、实体、组件、字段、List元素、块编辑、移动、组件编辑、生成和销毁记录 |
| 编码字节 | 新增载荷长度 |
| 解码字节 | 顶层结构，`Vec`按元素数乘元素大小，内容键与文本按长度；不含分配器开销 |

Dense单元与状态值是标量数组，只计字节，不计条目。读取计数后先核对剩余输入至少能容纳该数量的最小元素，再计预算和分配，伪造的大计数不会触发相应大小的分配。

解码字节通常大于编码字节，值较多的组件尤其明显。检查点发布前会按同一预算预检解码，预算不足时发布前失败。

## 校验边界

codec只检查结构。状态编号是否在状态表中、内容键能否解析、组件是否符合模式、位置是否在世界范围内，由A的`Snapshot::restore`与`ChangeSet::apply_with_components`在全部校验通过后才落地。未注册内容在恢复时报错，当前不保留原始载荷。

## 固定样例

| 文件 | 样例 |
|---|---|
| [world_snapshot.hex](tests/fixtures/world_snapshot.hex) | 边长2，Uniform与Dense分区各一，一个实体，覆盖全部值标签与嵌套 |
| [world_changes.hex](tests/fixtures/world_changes.hex) | 两个块编辑，一次移动，组件替换、新建与删除各一次 |
| [world_lifecycle.hex](tests/fixtures/world_lifecycle.hex) | 上述变更加生成、销毁、水位及嵌套初始组件 |
| [world_snapshot_v02.hex](tests/fixtures/world_snapshot_v02.hex) | 保留版本2快照字节，用于旧数据读取验证 |
| [world_changes_v02.hex](tests/fixtures/world_changes_v02.hex) | 保留版本2变更字节，用于无生命周期后缀的读取验证 |

快照与普通变更样例逐行标注字段名。[格式测试](tests/world_format.rs)用独立于编码器的逐字段规格核对编码结果、样例与解码结果，并核对元数据段与B1的snapshot.hex、commit.hex一致。测试还覆盖全部截断位置、预算不足、乱序与重复、非法标量、伪造计数、嵌套上限和未知版本。

[生命周期格式测试](tests/world_lifecycle.rs)独立拼接新增字段，验证精确字节、全部截断位置、嵌套条目预算、失败回滚、非法水位与身份、未知存在标签及版本2读取。

同一版本内的布局变化属于兼容破坏，须同时升级版本号。确认变化后，设置环境变量`TORSE_BLESS_FIXTURES=1`运行格式测试即可重写样例。
