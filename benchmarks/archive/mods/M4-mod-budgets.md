# Mod宿主预算边界

日期：[20261027]
类型：验收
状态：已验收
环境：Windows、x86_64、Rust1.93.1、Wasmtime45.0.0
范围：行为组件的调用预算、编组复制、Store资源限制及失败恢复

## 目标

未记录

## 方法

### 一、行为

测试宿主、权威桥和持久派发共用受检区域计数。先检查所有轴是否为空或反向，再计算无符号跨度与受检乘积，并在进入任一层循环前核对单次和累计预算。跨度超过i64有符号差值范围时也不会溢出。空范围立即返回空结果。

每轮调用分别累计Host调用次数、原生工作量及输入输出估算。查询失败不会解除累计限制；Guest忽略预算错误后返回的计划仍被拒绝。日志条数按本轮计数，诊断统计跨轮保留并使用饱和计数。

实体扫描在进入迭代前按全部已分配槽位预留工作量，包含空槽。组件列表、查询结果和待唤醒列表在复制前核对相应工作与结果额度。持久派发构建只读快照的工作和副本也计入本轮预算。

所有组件Store都设置Wasmtime的`set_hostcall_fuel`。该额度在单次规范ABI提升时检查列表和字符串复制，重复指向同一Guest字符串的别名仍分别计数。它与Guest指令fuel独立；应用侧累计输入预算继续约束多次小调用。

## 结果

### 二、默认值

默认值来自`CallBudget::prototype()`，宿主可以显式配置。

| 项目 | 默认上限 | 生命周期 |
|---|---|---|
| Guest指令fuel | 200000000 | 每轮调用 |
| 单次方块体积、实体页长度 | 4096 | 每次查询 |
| Host导入调用 | 1024 | 每轮调用 |
| Host工作单位 | 16384 | 每轮调用 |
| 输入估算、单次ABI复制 | 1MiB | 累计输入按轮，复制按次 |
| 输出及快照估算 | 1MiB | 每轮调用 |
| 接收的日志条数 | 64 | 每轮调用 |
| Guest线性内存总量 | 32MiB | 每个Store |
| Guest表元素总量 | 32768 | 每个Store |
| 核心实例数 | 64 | 每个Store |
| 线性内存数、表数 | 各8 | 每个Store |

内存和表元素限制统计同一Store内全部对象的总量。超过限制的增长会产生陷阱，初始实例化同样受限。包集合共享Store时，共享这些额度。

### 三、恢复与接口

单包实例在陷阱后重建，持久派发继续保持每轮新建Guest的语义。多包场景运行时在失败需要重建时丢弃整个旧Store，重新实例化各包，避免已失效实例继续占用Store的资源和数量额度。已提交的权威世界、包失败计数和调度队列保留；各包根据下一次调用携带的已提交组件状态继续执行。

`CallBudget`新增显式额度字段。使用完整结构体字面量的调用方应补齐新字段，或使用`..CallBudget::prototype()`继承默认值。`EntityStore::allocated_slots()`提供包含空槽的扫描计数。WIT、网络消息和存档格式均未改变。

字节估算覆盖边界值及可见快照副本，不能替代分配器或统一进程预算。Wasmtime的编译器分配、原生容器管理开销及长时间保留的世界数据仍由宿主的其他预算管理。

### 四、验证

边界回归覆盖4294967296乘4294967296的体积、i64两端坐标、巨大空范围、多次小查询耗尽、输入输出超限、最大游标、稀疏实体槽位、真实Wasm内存及表增长，以及重复字符串别名的ABI复制限制。

`budget-probe`测试Guest按不同Tick触发体积、累计查询、日志输入、内存增长和返回计划超限，并继续尝试提交计划。权威实例、持久派发和共享运行时均验证拒绝后无世界修改，随后正常调用仍可执行。

```sh
cargo test --workspace --all-targets --locked
cargo test -p torse-mod-host --features package --all-targets --locked
cargo test -p torse-server --features mods --all-targets --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p torse-mod-host --features package -p torse-server --features mods --all-targets --locked -- -D warnings
cargo doc -p torse-mod-host --features package -p torse-world --no-deps --locked
```

全工作区默认测试760项通过，零忽略。Mod宿主启用package后的测试35项通过，原有性能测量保留1项显式忽略。服务端启用mods后的测试172项通过，零忽略。

测试结果逐目标汇总见[原始结果表](../data/mod-host-budgets/01-bounded/tests.csv)，运行环境见[环境记录](../data/mod-host-budgets/01-bounded/environment.txt)。

### 五、未覆盖范围

本次未重新运行跨设备GPU测量及长时间进程内存压力场景。编译器内存、整进程分配器与包文件装载上限不由本次调用预算保证；1项性能测试保持显式忽略。本次不以测试耗时作为性能结论。

## 复现

未记录

## 边界

未记录
