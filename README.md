<p align="center">
  <img src="figures/torse-logo.png" alt="Torse项目标志" width="420">
</p>

# Torse

Torse是一个用Rust编写的开放方块世界内核与开发框架，提供世界存储、模拟执行、持久化、网络同步和Mod扩展能力，并通过基础游戏验证这些能力如何共同工作。

项目将通用框架与具体玩法分开：内核负责世界状态和执行规则，玩法由内容定义、可选模块与Mod组合。客户端和服务端共用领域模型，世界修改经过统一授权与事务提交，再交给存储和网络层处理。

[快速开始](#快速开始) | [项目结构](#项目结构) | [开发与验证](#开发与验证) | [参与贡献](CONTRIBUTING.md) | [许可证](LICENSE)

## 开发状态

Torse目前处于早期开发阶段，已有可运行的图形客户端、独立服务端与Mod示例。现阶段主要验证世界编辑、持久恢复和联机交互，接口与存档格式仍在演进。

客户端与服务端Demo承担内核运行测试、能力边界展示和外围设施装配验证。官方外围设施将按可装配模块提供，组件和内容模板也属于可复用预设的范围。例如半砖模板需要统一声明状态、渲染、碰撞、选择、放置及合并语义，再配合不同材质使用。现有演示内容用于验证这些契约，正式预设接口仍需逐项确定。

| 领域 | 已实现的范围 | 验证记录 |
|---|---|---|
| 世界与地形 | 方块状态、分区存储与流式装载、确定性地形生成、碰撞检测和网格生成 | [世界场景](benchmarks/archive/world/M1-scenario.md) |
| 模拟与事务 | 固定步长Tick、阶段调度、实体与组件、动作授权、库存转移和跨模块原子提交 | [Tick执行](benchmarks/archive/simulation/M2-tick.md)、[跨模块事务](benchmarks/archive/mods/M4-cross-module.md) |
| 图形客户端 | 窗口与离屏渲染、图集贴图与逐面材质、飞行与行走、方块选择与编辑、HUD、菜单和截图 | [窗口交互](benchmarks/archive/client/M1-window.md)、[地形贴图](benchmarks/archive/world/M1-textures.md) |
| 持久化 | 日志、检查点、实体生命周期恢复、持久任务与定时事件、请求结果与去重 | [世界恢复](benchmarks/archive/persistence/M3-local-world.md)、[执行与网络](benchmarks/archive/network/B6-B7-execution-network.md) |
| 联机 | 双向TLS与用户授权、TCP连接、授权复制、多人编辑、越界拒绝、撤权与重连 | [用户认证](benchmarks/archive/network/M5-user-authentication.md)、[授权验收](benchmarks/archive/network/A8-acceptance.md) |
| Mod扩展 | Wasm包、类型化交互、嵌套状态、批量库存与实体生命周期，共同准备和提交 | [Mod动作](benchmarks/archive/mods/M4-mod-actions.md)、[Mod持久执行](benchmarks/archive/mods/M4-mod-durable.md) |
| 内存预算 | 主程序共享世界、资源、复制、编解码、任务和缓存预留，支持背压、重试及生命周期释放 | [共享内存](benchmarks/archive/performance/M6-shared-memory.md) |

联网移动已接入服务端固定Tick结算、持久运动状态与客户端预测校正。服务端可在批准范围内自动跟随玩家调整兴趣窗口，也可由宿主显式替换范围并重建订阅基线。物品模块已连接放置消耗、破坏回收与库存原子提交，回收产物直接进入背包，尚无地面掉落实体。当前图形机器面板使用示例数据，真实Mod机器面板仍待装配。默认服务端演示使用有限世界，不代表已经验证大规模世界或高并发性能。

## 快速开始

以下命令均在仓库根目录执行。项目通过[rust-toolchain.toml](rust-toolchain.toml)固定使用Rust 1.93.1，并在[Cargo.lock](Cargo.lock)中记录依赖版本。使用Rustup管理工具链时会按配置选择版本；图形客户端还需要可用的Vulkan、DirectX 12或Metal后端。

### 本地体验

启动交互窗口：

```sh
cargo run -p torse-client -- --window
```

| 操作 | 输入 |
|---|---|
| 移动 | WASD |
| 转向 | 捕获状态下移动鼠标；捕获失败时可按住左键或右键拖动 |
| 飞行升降 | 空格 / 左Shift |
| 切换飞行与行走 | F5，行走时空格跳跃 |
| 破坏 / 放置方块 | 短按鼠标左键 / 右键 |
| 选择快捷槽 / 打开物品栏 | 数字键1到9 / Tab |
| 调试信息 | F8，可在按键设置中修改 |
| 调节速度 | 鼠标滚轮 |
| 菜单 / 机器面板预览 | Esc / E |
| 截图 | F12 |

也可以直接生成一张离屏截图：

```sh
cargo run -p torse-client -- --capture out.png --width 960 --height 720 --radius 2
```

离屏模式仍需要GPU。更多参数、字体配置与交互说明见[客户端文档](apps/client/README.md)。

### 联机编辑

先创建一个演示世界并启动服务端：

```sh
cargo run -p torse-server -- .work/demo-world --listen 127.0.0.1:7777 --fill --editors
```

保持服务端运行，在另一个终端启动客户端：

```sh
cargo run -p torse-client -- --window --connect 127.0.0.1:7777
```

可以再启动一个客户端，观察同一世界中的编辑结果。`--fill`只用于首次创建世界，重新打开同一目录时需去掉该参数。`--editors`会向所有连接授予演示范围内的编辑权限；省略时，客户端以访客权限连接，编辑请求会被拒绝。该策略仅用于回环地址的本地演示，远程连接改用双向TLS及用户配置，见[用户认证](benchmarks/archive/network/M5-user-authentication.md)。

无需图形环境时，可在服务端运行期间使用无GPU客户端示例：

```sh
cargo run -p torse-server --example headless_client -- 127.0.0.1:7777
```

服务端默认持续运行；指定`--ticks 60`可在运行60个Tick后保存并退出。持久目录、恢复行为与Mod加载方式见[服务端文档](apps/server/README.md)。

## 项目结构

仓库采用Cargo工作区组织。`crates`承载通用能力，根目录的`modules`承载可选玩法，`games/base`负责基础游戏的组合与资源。

| 路径 | 内容 |
|---|---|
| [apps](apps/README.md) | 图形客户端、独立服务端与开发工具 |
| [crates](crates/README.md) | 世界、模拟、渲染、编码、存储、协议与网络等框架库 |
| [crates/mod_sdk](crates/mod_sdk/src/lib.rs) | 面向Mod开发者的SDK |
| [modules](modules/README.md) | 库存、物品与可换肤UI模块，以及预留的合成与能量入口 |
| [games/base](games/base/README.md) | 基础游戏的内容、规则与素材 |
| [examples/mods](examples/mods/README.md) | 通过公开SDK实现的Mod示例 |
| [tests](tests/README.md) | 跨模块场景、故障注入与测试夹具 |
| [benchmarks](benchmarks/README.md) | 可复现的验证过程、画面与测量记录 |

世界与模拟层负责权威状态及提交语义，存储与网络层通过公共契约接收结果。渲染层读取世界投影，编辑操作通过动作服务提交。修改公共类型或行为时，需要同步检查调用方、编码格式和恢复路径。

## 开发与验证

提交前执行格式、静态检查与工作区测试：

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

开发工具提供可独立复跑的场景，例如：

```sh
cargo run -p torse-tools -- content-fixture
cargo run -p torse-tools -- m1-scenario
cargo run -p torse-tools -- commit-scenario
cargo run -p torse-tools -- recovery-scenario
cargo run -p torse-tools -- persistence-scenario
```

ECS、Wasmtime与图形初始化实验需显式启用`experiments`特性：

```sh
cargo run -p torse-tools --features experiments -- ecs-probe
cargo run -p torse-tools --features experiments -- wasm-probe
cargo run -p torse-tools --features experiments -- gpu-probe
```

CI配置见[ci.yml](.github/workflows/ci.yml)。窗口交互与实际GPU画面需在本地验证；各项功能的复现命令、结果和未覆盖范围记录在[benchmarks](benchmarks/README.md)中。


## 参与贡献

问题反馈、文档修订、测试和代码改进均可通过Issue或Pull Request提交。新功能、公共接口以及存档或网络格式的调整，请先开Issue讨论。环境准备、验证命令与审核要求见[贡献指南](CONTRIBUTING.md)，Mod开发从[示例与工具说明](examples/mods/README.md)开始。

## 许可证

Torse采用GNU General Public License v3.0，仅限第3版（`GPL-3.0-only`）。除文件中另有说明外，该许可适用于仓库中的原创代码、文档、示例和素材；第三方依赖与材料保留各自的许可声明。

GPL-3.0允许商业使用和收费分发。分发受该许可覆盖的软件时，需要遵守相应源码提供、许可声明和再分发要求。完整条款见[LICENSE](LICENSE)，版权归各自贡献者所有。
