# M4前置：组件模型往返与调用成本

日期：[20261024]
类型：验收
状态：已验收
环境：未记录
范围：P1原型，非M4验收

## 目标

维护：Mod平台侧(T05)。

### 问题

DM01要求验证Wasmtime组件模型作为行为Mod边界的可行性：接口用WIT定义，guest用Rust SDK构建，宿主实现import并计量。需要回答三件事：边界能否端到端跑通；失控guest能否被终止且不残留；每次调用的成本相对原生实现是多少。

## 方法

### 实现

- `crates/mod_host/src/component.rs`：夹具宿主实现三类import（查询、诊断、调度），每调用重置fuel，宿主侧独立计量查询、返回格数、解析与日志；`run_native`为同逻辑原生对照。
- `examples/mods/roundtrip`：每次唤醒解析符号、查询3x3x3邻域、按种子做确定性混合、计数加一并安排下一Tick唤醒，每8 Tick翻转上方方块，每64 Tick输出日志；`spin`特性替换为死循环供终止测试。
- `crates/mod_sdk`：确定性splitmix64随机流，绑定再导出。
- 测试：`crates/mod_host/tests/roundtrip.rs`，全部置于`component`特性之后，默认工作区运行不编译。

## 结果

### 条件

- 宿主：`wasmtime = "=45.0.0"`，features `cranelift, runtime, std, wat, component-model`；fuel与epoch中断开启。
- 编码：`wit-component = "=0.262.0"`，core module转component，验证开启。
- Guest:`torse-mod-roundtrip`，`wit-bindgen = "=0.63.0"`，目标`wasm32-unknown-unknown`，release构建。
- 接口真源：`crates/mod_api/wit/behavior.wit`（包`torse:behavior@0.1.0-draft`，world `mod-behavior`）。
- 硬件与系统：本机Windows开发机，与M0实验的Apple M4 Max不同，数值不可跨设备比较。

正确性与安全：

```text
roundtrip_correctness          通过:100次调用全部返回计划,计数100,唤醒100,
                               查询100次共2700格,日志恰好1条
epoch_kills_runaway_guest      通过:interrupt trap,无提交残留
fuel_kills_runaway_guest       通过:all fuel consumed trap,无提交残留
```

成本（release与guest同机构建）:

```text
instantiate                    143.6µs
calls=10000  component 3.267µs/call  native 0.330µs/call  ratio 9.9x
calls=50000  component 3.270µs/call  native 0.349µs/call  ratio 9.4x
```

每次调用含：构造invocation、穿越组件边界、guest执行（resolve+查询+随机混合+计划构造）、宿主import往返、计划应用到夹具。原生对照含同等工作，差值即边界成本，约2.9µs。

按候选验收场景估算：8192台每Tick一次唤醒约26.8ms，在20 Tick/s、P99 50ms预算内但占比过半；1024台约3.3ms。M0实验（core module无载荷调用，另一台设备）为1.27µs/call，本结果与之量级一致。

### 判定

DM01不触发否决：组件模型保留为行为边界。备选优化（按需启用，不改变接口）：批处理入口减少穿越次数、实例池与`InstancePre`复用降低重建成本、计划合并提交。上述数值是原型起点，机器示例完成后的完整配方逻辑成本按04-mods 3.2节补测。

## 复现

### 复跑

```sh
cargo test -p torse-mod-host --features component --test roundtrip
cargo test -p torse-mod-host --features component --release --test roundtrip \
    component_cost_versus_native -- --ignored --nocapture
```

## 边界

### 已知边界

- 夹具宿主不接真实权威；计划应用是夹具级模拟，P2接入WorldAuthority与协调器。
- 唤醒队列为进程内，不持久；等B6定时事件记录。
- 值表示限扁平原子字段表，嵌套结构未上边界（见设计文档开放问题）。
- trap原因在错误链中而非Display首行，匹配终止原因需遍历chain。
