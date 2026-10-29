# M0技术实验记录

日期：[20261023]
类型：实验
状态：已验收
环境：macOS aarch64，Apple M4 Max，Rust 1.93.1
范围：未记录

## 目标

构建：debug profile

## 方法

未记录

## 结果

### 内容编译夹具

```text
blocks=3
state_schemas=3
capabilities=1
actions=3
modules=1
module_order=base:core
authority_digest=7a9d4c47e35be56414ec05254cd4d471a527cdcaa64b397e7eb169a8939f5dd7
presentation_digest=f84ad4585c5a7be8d0a1c6daca6975b271718994c1d3bde675e7259c70011eec
```

石头、玻璃半砖和植物夹具通过编译。测试同时覆盖乱序输入摘要一致、重复定义、未知引用、形状越界和状态组合上限。

### ECS适配

版本：`bevy_ecs 0.18.1`

```text
entities=10000
ticks=100
elapsed=119.524334ms
checksum=55994400
```

两组系统由独立调度器执行，覆盖位置积分和生命值更新。此结果只证明独立ECS可以装配并运行，不代表最终世界调度性能。

### Wasm批量调用

版本：`wasmtime 45.0.0`

```text
calls=10000
iterations_per_call=100
elapsed=12.661792ms
checksum=56995000
```

调用进入真实Cranelift编译的Wasm函数，不使用空函数吞吐替代逻辑。下一步需要在完整配方逻辑上比较批处理大小、宿主复制量和实例重建成本。

### 图形窗口与设备

版本：`wgpu 30.0.1`、`winit 0.30.12`

```text
adapter=Apple M4 Max
backend=Metal
device_type=IntegratedGpu
surface_format=Bgra8UnormSrgb
elapsed=19.462417ms
```

探针创建不可见原生窗口、`wgpu` surface、adapter和device，然后立即退出。没有提交网格或渲染画面，因此不构成帧时间数据。

### 结论

M0所需的三项技术路线都已经有可编译、可运行的最小原型。当前证据只支持“技术可以接入”，不支持“已经达到架构候选性能门槛”。正式性能比较应在Release构建、固定场景和重复采样下进行。

## 复现

### 运行命令

```sh
cargo run -p torse-tools -- content-fixture
cargo run -p torse-tools --features experiments -- ecs-probe
cargo run -p torse-tools --features experiments -- wasm-probe
cargo run -p torse-tools --features experiments -- gpu-probe
```

## 边界

未记录
