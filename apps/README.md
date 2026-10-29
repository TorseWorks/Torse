# 应用入口

| 目录 | 职责 | 运行方式 |
|---|---|---|
| [client](client/README.md) | 图形渲染、本地交互、联网镜像与编辑请求 | `--window`打开窗口，`--capture`输出PNG，`--connect`连接服务端 |
| [server](server/README.md) | 持久世界、阶段执行、授权网络与可选Mod装配 | 指定世界目录启动；支持恢复、有限Tick运行及无GPU客户端示例 |
| [tools](tools/src/main.rs) | 内容检查、世界场景、提交与恢复验证、技术实验 | 使用`content-fixture`、`m1-scenario`等子命令 |

底层库不依赖应用入口。服务端可在无图形环境中运行；仓内`apps/client`装配公开领域模型、资源与UI模块，提供内核验证用的图形客户端。其他宿主也可以使用这些公开接口自行装配。

开发工具还提供`commit-scenario`、`tick-scenario`、`exchange-scenario`、`recovery-scenario`、`movement-scenario`与`persistence-scenario`。`ecs-probe`、`wasm-probe`和`gpu-probe`需要显式启用`experiments`特性。

```sh
cargo run -p torse-tools -- m1-scenario
cargo run -p torse-tools --features experiments -- wasm-probe
```

基础内容的PNG身份、材质槽、状态投影和语言表可离线生成：

```sh
cargo run -p torse-tools --locked -- cook-base games/base/assets .work/base.tcook
cargo run -p torse-tools --locked -- run-cooked .work/base.tcook games/base/assets
```

输入清单、格式和当前客户端接入范围见[运行时表说明](../crates/content/COOKED.md)。
