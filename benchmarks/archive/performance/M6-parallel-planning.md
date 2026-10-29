# 冻结前并行规划

日期：[20261025]
类型：测量
状态：已验收
环境：Windows 11 Pro 10.0.26300、Intel Core i7-14700KF、28个逻辑CPU、Rust 1.93.1、Release
范围：未记录

## 目标

基线提交：7dde86b，包含本轮工作区改动。原始数据见[data/parallel-planning](../data/parallel-planning)。

## 方法

### 一、接入位置

`ExecutionHost::collect_phase`在阶段任务持久冻结之前并行执行符合条件的周期规划。默认仍为串行；宿主通过`set_parallel_planning`配置workers和max_in_flight，规则通过`DurableRule::parallel_planner`返回`Some(self)`加入，编译器要求实现Sync。

请求、动作通知和定时事件保留原串行入口。周期规则沿用order_phase的规范拓扑次序，只把相邻、没有直接依赖的已加入规则组成一批；声明的before或after关系、未加入的规则和批次上限都会结束当前批次。批次全部结束后才进入下一批，因此间接依赖同样得到满足。现有Mod适配器持有RefCell及Rc，继续在宿主线程执行。

`torse_runtime::run_scoped`借用同一个阶段起点的只读WorldAuthority，每批创建最多workers个线程，不复制完整世界，不让任务跨过冻结或发布。按原输入序号收回结果，再使用既有稳定事务键排序。相同键保留规则收集顺序，回调完成次序不参与争用裁决。

冻结缓存、逐项prepare、日志预留、publish、accept、授权投影、刷盘和恢复代码沿用既有流程，执行载荷版本仍为1。恢复只消费冻结任务和最终变更，不运行规划器。

## 结果

### 二、容量与失败

| 项目 | 行为 |
|---|---|
| 工作线程 | workers取1到64，每批最多max_in_flight项，后者同样取1到64 |
| 输入 | 共享阶段起点世界，只传借用引用和有限任务索引 |
| 预留 | 每项按运行态字节上限的4倍预留可信规划、稳定捕获及编码校验的工作空间，整批合计受配置约束 |
| 输出 | 每项先检查阶段任务数，再经现有有界编码与解码校验字节、条目和嵌套分配；归并后继续检查全阶段与完整运行态预算 |
| 分配边界 | 原生回调必须自行遵守预算，预留不强制截断任意原生分配；共享世界、已归并任务、线程栈与控制对象另计 |
| 规划故障 | 整批回收后按原规则次序返回第一个故障，不写入冻结任务或消耗事件身份 |
| panic | 当前展开式配置下转换成规划故障；其余批内工作完成并回收，不发布部分结果 |
| 日志阻塞 | 已收集结果继续留在既有冻结缓存，重试不重新调用规划器 |
| 配置切换 | 不替换已缓存或已持久冻结的任务，只影响之后尚未收集的阶段 |

权威规划在整批返回前等待；已有生成与网格固定池继续使用自有输入。此处为安全借用世界采用有作用域的线程批次，没有复用固定池，也没有跨阶段的异步结果和取消。原生规划器须保持纯计算，不能在回调里等待磁盘、网络或同批其他工作。

规划Fault会使Server按既有策略停止实例；日志容量阻塞仍可继续重试。直接使用ExecutionHost时，尚未冻结的纯规划失败不改变权威状态。

### 三、等价与恢复验证

[专项测试](../../../apps/server/tests/parallel_planning.rs)使用真实ExecutionHost和文件服务端，覆盖以下场景。

| 场景 | 结果 |
|---|---|
| 串行、1线程、4线程 | 完整最终执行快照、文件日志字节、请求结果和授权复制载荷一致 |
| 完成乱序 | 强制相邻规则逆序完成，相同事务键仍由规范顺序中的第一项获胜 |
| 阶段起点 | 所有规划器读取修改前的同一格值，规划期间没有提交 |
| 并发与背压 | 4线程配置、最多2项在途，实际并发达到2且不超过2，后续规则分批进入 |
| 依赖与兼容 | before和after均在前驱完成后执行；持有Rc的旧规则在宿主线程执行且与工作线程隔离 |
| 事件与请求 | 冲突失败不登记事件，只有获胜计划的定时事件消费一次，未知动作拒绝结果一致 |
| 冻结中恢复 | 在冻结后及结算1、5项任务后重开文件世界，回调禁止执行仍可完成剩余阶段 |
| 故障与容量 | 规划Fault、原生panic、阶段任务数及字段字节超限均不改变权威状态；直接使用ExecutionHost时可重新收集纯规划 |
| 预留重试 | 丢弃准备记录模拟日志拒绝，再次准备得到相同记录，规则调用数不增加 |

`run_scoped`另验证借用输入、真实并发、输入序号归并、任务与字节预检查、输出超限和panic隔离。工作区517项测试、服务端mods特性99项测试通过；Runtime与规划专项联合重复20轮，共260次测试通过。工作区及服务端mods特性Clippy、格式检查通过。主程序4线程配置创建世界至Tick 2、1线程配置重开至Tick 4，DurableSeq从24继续至48。此启动场景无已加入并行的规则，规划并发由上述专项和基准验证。

本轮在本机运行；Linux与Windows远端CI尚未验证。CI已加入规划专项重复执行及配置入口启动检查。未增加断电保证、并行提交或Guest并发承诺。

### 四、Release测量

使用8个纯原生周期规则，每项生成一个独立或成对争用的格值事务。轻负载只生成事务；CPU负载每规则额外执行200000次确定性只读格值查询与校验计算，用于控制规划成本。三种模式均预热3Tick后测20Tick，使用执行阶段分项计时。

后端为不保留日志的sink，追加时计算流式CRC32C，不执行磁盘I/O；其他编码、准备、发布及驱动流程仍经过真实Server。下表规划耗时是六阶段Planning样本每Tick之和的均值，驱动耗时包含该Tick完整20条记录的处理。结果不是实际玩法吞吐或文件持久性能。

| 负载 | 串行规划，ms | 1线程规划，ms | 4线程规划，ms | 串行驱动，ms | 4线程驱动，ms |
|---|---:|---:|---:|---:|---:|
| 轻负载，独立修改 | 0.016 | 0.079 | 0.136 | 0.271 | 0.434 |
| 轻负载，成对争用 | 0.013 | 0.067 | 0.125 | 0.259 | 0.419 |
| CPU负载，独立修改 | 10.552 | 10.704 | 2.898 | 10.835 | 3.180 |
| CPU负载，成对争用 | 10.609 | 10.764 | 3.004 | 10.898 | 3.278 |

CPU独立负载的规划约为串行的3.64倍速度，完整驱动约为3.41倍；轻负载多出的线程启动和校验成本超过计算收益。默认串行及规则显式加入因此保留。测量只含一次运行，没有用该合成结果推算现有Mod或整体服务端的加速比。

每组跨三种模式逐字节比较完整最终执行快照，并校验流式日志CRC32C一致。独立组的状态摘要为dd6844a1、日志摘要为c157f16b；争用组分别为a3391504、b72433b4。日志逐字节等价另由文件回归测试验证。逐Tick耗时、P50/P95和参数分别保存在[ticks.csv](../data/parallel-planning/ticks.csv)、[summary.csv](../data/parallel-planning/summary.csv)和[run.txt](../data/parallel-planning/run.txt)。

## 复现

### 五、复现

```powershell
cargo test -p torse-runtime --lib --locked
cargo test -p torse-server --test parallel_planning --locked
cargo test --workspace --all-targets --locked
cargo test -p torse-server --features mods --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p torse-server --features mods --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo run --release -p torse-server --example parallel_baseline --locked -- --output .work/parallel-planning/01-measurement
```

输出目录必须不存在。`--ticks`允许1到1000，`--warmup`允许0到100；默认分别为20和3。workers为0的CSV行表示原串行参考，1和4表示已启用的借用批次，max_in_flight固定为8。[validation.txt](../data/parallel-planning/validation.txt)记录本轮验证计数与启动结果。

## 边界

未记录
