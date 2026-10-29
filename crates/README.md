# 框架库

本目录保存Torse的通用库，涵盖世界、模拟、数据交换、资源生命周期、派生光照与有界诊断等能力。窗口、GPU绘制和界面装配位于[客户端](../apps/client/README.md)，持久执行与网络会话装配位于[服务端](../apps/server/README.md)。

| 目录 | 职责与当前范围 |
|---|---|
| [types](types/src/lib.rs) | 身份、坐标、时间、结果、持久位置、复制与内存账本等公共类型 |
| [runtime](runtime/README.md) | 有界CPU执行器、预留、优先级与取消，另含可选ECS实验 |
| [content](content/src/lib.rs) | 内容契约、编译、摘要与基础内容夹具 |
| [world](world/src/lib.rs) | 方块状态、分区、实体、组件与快照、变更载荷 |
| [sim](sim/src/lib.rs) | 固定步长、阶段排序、事务仲裁、动作授权、持久任务、授权复制与行走领域逻辑 |
| [actions](actions/src/lib.rs) | 动作声明、命令冲突与注册检查 |
| [modules](modules/src/lib.rs) | 模块依赖、服务与注册解析 |
| [world_services](world_services/src/lib.rs) | 确定性地形生成、AABB碰撞、有界天空光与发光传播、调用方提供通行策略的格点寻路 |
| [mod_api](mod_api/src/lib.rs) | 公共Mod接口，行为边界定义于[behavior.wit](mod_api/wit/behavior.wit) |
| [mod_sdk](mod_sdk/src/lib.rs) | Guest端绑定与确定性随机数辅助，供Mod代码依赖 |
| [mod_host](mod_host/src/lib.rs) | Wasm组件宿主、权威桥、包解析、场景运行与持久派发，按特性启用 |
| [codec](codec/README.md) | 记录包裹、世界载荷、执行状态与网络载荷的显式编码 |
| [storage](storage/README.md) | 有界日志、检查点、文件恢复扫描与故障处理 |
| [protocol](protocol/README.md) | 兼容握手、请求、结果、订阅和授权复制消息 |
| [net](net/README.md) | 会话、速率、有界内存与TCP传输、原子客户端镜像 |
| [render](render/src/lib.rs) | CPU网格、逐面材质槽与局部贴图坐标、遮挡剔除、依赖修订校验、按焦点距离驱逐的网格缓存与内存账本汇报，另含可选GPU初始化实验 |
| [assets](assets/src/lib.rs) | 资源身份、句柄、依赖与生命周期、图集PNG解码、材质包校验及mipmap生成；具体资源声明由应用或模块提供 |
| [diagnostics](diagnostics/src/lib.rs) | 可选、有界的串行分项计时；其他指标由各实现提供 |

## 依赖与执行

`world`保存领域状态，`sim`负责动作规划、事务准备和提交。准备项在发布前可以被拒绝或丢弃，世界状态与提交序号在成功发布后才推进。

服务端协调日志预留、发布、接纳和刷盘，达到持久位置后才释放相应结果与复制输出。存储层检查记录结构与连续性，恢复构建器检查世界语义。网络层传输授权投影，客户端接收完整基线或增量后更新镜像。

`world`不依赖渲染和网络，渲染层通过动作接口发起编辑。`crates/modules`负责通用模块装配，根目录[modules](../modules/README.md)负责可选玩法。新增跨层行为时，请明确状态归属和提交边界，避免产生另一条未经授权或无法恢复的写入路径。

## 可选特性

Mod宿主的`component`启用Wasm组件调用，`authority`增加权威世界桥，`package`进一步启用包工具与场景运行器。服务端的`mods`特性启用完整Mod接入。具体命令见[Mod示例](../examples/mods/README.md)，提交要求见[贡献指南](../CONTRIBUTING.md)。
