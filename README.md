<p align="center">
  <img src="figures/torse-logo.png" alt="Torse项目标志" width="420">
</p>

# Torse

简体中文 | [English](README.en.md)

Torse是用Rust编写的开放方块世界内核与开发框架，提供世界存储、固定步长模拟、持久化、网络同步和Mod扩展。仓库包含图形客户端、独立服务端和基础游戏示例，供开发者运行并验证这些能力。

Torse仍处于早期开发阶段，公共接口、存档格式和网络协议仍在演进。随仓库提供的联机演示使用有限世界。各项规模与性能数据的适用范围见[验证记录](benchmarks/README.md)。

## 快速开始

安装[Rustup](https://rustup.rs/)，然后克隆仓库。以下命令均在仓库根目录执行。Rustup会按[rust-toolchain.toml](rust-toolchain.toml)选择Rust 1.93.1。Windows原生构建需要MSVC构建工具。图形客户端需要支持Vulkan、DirectX 12或Metal的GPU和驱动。

```sh
git clone https://github.com/TorseWorks/Torse.git
cd Torse
```

### 本地体验

```sh
cargo run -p torse-client -- --window
```

| 操作 | 默认输入 |
|---|---|
| 移动与转向 | WASD和鼠标 |
| 飞行升降 | 空格和左Shift |
| 切换飞行与行走 | F5，行走时按空格跳跃 |
| 破坏和放置方块 | 鼠标左键和右键 |
| 选择快捷槽和打开物品栏 | 数字键1至9和Tab |
| 拾取和丢弃物品 | F和Q |
| 调节速度 | 鼠标滚轮 |
| 菜单和机器面板预览 | Esc和E |
| 调试信息和截图 | F8和F12 |

也可以将一帧保存为PNG。离屏渲染仍需要GPU。

```sh
cargo run -p torse-client -- --capture out.png --width 960 --height 720 --radius 2
```

更多参数和按键配置见[客户端文档](apps/client/README.md)。

### 运行联机演示

在第一个终端启动服务端。命令会创建一个包含两名玩家的本地演示世界。

```sh
cargo run -p torse-server -- .work/demo-world --listen 127.0.0.1:7777 --fill --editors --players 2
```

保持服务端运行，在另一个终端启动客户端。可以再运行一次客户端，在两扇窗口中观察同一世界的编辑结果。

```sh
cargo run -p torse-client -- --window --connect 127.0.0.1:7777 --walk
```

`--fill`只用于创建新世界，重新打开同一目录时去掉该参数。`--editors`授予演示范围内的编辑权限，省略时以只读访客身份连接。明文演示仅监听回环地址。远程连接需要双向TLS和用户授权配置，见[服务端文档](apps/server/README.md)。

在没有GPU的环境中，可用以下示例连接正在运行的服务端。

```sh
cargo run -p torse-server --example headless_client -- 127.0.0.1:7777
```

## 已实现能力

| 领域 | 当前实现 |
|---|---|
| 世界与渲染 | 按分区存储和流式加载地形，支持确定性生成、碰撞、网格生成和贴图。宿主可配置独立的只读远景范围，并同步地形和光照。 |
| 模拟与持久化 | 按固定Tick调度实体和组件更新。世界修改经过授权、准备和原子提交，日志与检查点用于恢复世界、执行进度和请求去重状态。 |
| 联机 | 权威服务端按会话权限同步世界变化，支持多人编辑、服务端行走结算及客户端预测校正。 |
| 内容与玩法 | 用内容定义和可选模块组合基础游戏。有限库存模式下，放置消耗物品，破坏产生受重力和地形约束的掉落物，可拾取回库存。 |
| Mod扩展 | Wasm组件通过类型化接口提交行为计划，宿主校验权限并提交世界、组件和库存修改。机器Mod提供可运行的无界面场景，图形客户端的机器面板为预览。 |
| 资源限制 | 客户端和服务端支持共享逻辑内存预算、背压及可回收缓存，并提供队列与执行进度的诊断接口。 |

## 项目结构

| 路径 | 内容 |
|---|---|
| [apps](apps/README.md) | 图形客户端、独立服务端与开发工具 |
| [crates](crates/README.md) | 世界、模拟、渲染、存储、协议与网络等框架库 |
| [crates/mod_sdk](crates/mod_sdk/src/lib.rs) | Mod开发SDK |
| [modules](modules/README.md) | 库存、物品与可换肤UI等可选模块 |
| [games/base](games/base/README.md) | 基础游戏的内容、规则与素材 |
| [examples/mods](examples/mods/README.md) | 可运行的Wasm Mod示例 |
| [tests](tests/README.md) | 跨模块场景、故障注入与测试夹具 |
| [benchmarks](benchmarks/README.md) | 可复现的验证过程、画面与测量记录 |

## 开发与贡献

修改代码时，提交前运行格式检查、Clippy和工作区测试。

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

可从内容、事务和恢复场景开始验证。

```sh
cargo run -p torse-tools -- content-fixture
cargo run -p torse-tools -- commit-scenario
cargo run -p torse-tools -- recovery-scenario
```

Mod开发可从[机器示例](examples/mods/README.md#运行机器示例)开始。安装Wasm目标后可检查示例包。

```sh
rustup target add wasm32-unknown-unknown
cargo run -p torse-mod-host --features package --bin torse-mod -- check examples/mods/machine
```

欢迎提交问题反馈、文档修订、测试和代码改进。新功能及公共接口、存档或网络格式的变更，请先开Issue讨论。环境准备、可选特性检查和审核流程见[贡献指南](CONTRIBUTING.md)。

## 许可证

Torse采用GNU General Public License v3.0，仅限第3版（`GPL-3.0-only`）。除文件另有说明外，该许可适用于仓库中的原创代码、文档、示例和素材。第三方材料保留各自的许可声明。完整条款见[LICENSE](LICENSE)。
