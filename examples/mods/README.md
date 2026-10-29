# Mod示例与工具

Torse通过Wasm组件执行行为Mod。Mod返回世界修改、组件更新、库存转移和唤醒计划，由宿主检查权限、准备并提交。包清单与内容使用TOML，行为边界定义于[behavior.wit](../../crates/mod_api/wit/behavior.wit)。

| 示例 | 用途 |
|---|---|
| [roundtrip](roundtrip/src/lib.rs) | 验证WIT查询、计划返回和组件调用边界 |
| [machine](machine/mod.toml) | 可装载的机器包，包含内容定义、四步加工逻辑与无界面场景 |

SDK与工具尚未提供独立发布安装布局，以下命令使用本仓库的Cargo工作区。SDK和示例遵循仓库的[GPL-3.0-only许可](../../LICENSE)。

## 运行机器示例

先在仓库根目录安装Wasm目标：

```sh
rustup target add wasm32-unknown-unknown
```

检查包，再构建行为代码并运行场景：

```sh
cargo run -p torse-mod-host --features package --bin torse-mod -- check examples/mods/machine
cargo run -p torse-mod-host --features package --bin torse-mod -- test examples/mods/machine --scenario examples/mods/machine/scenario.toml
```

成功时会输出各包的派发、提交、拒绝与陷阱统计，并以`scenario passed`结束。场景读取[scenario.toml](machine/scenario.toml)中的世界、实体、初始库存、唤醒和断言。测试模式授予清单申请的权限，实际服务端仍由宿主策略决定授权。

生成目录内容锁文件并验证：

```sh
cargo run -p torse-mod-host --features package --bin torse-mod -- pack examples/mods/machine -o .work/machine.lock
cargo run -p torse-mod-host --features package --bin torse-mod -- check examples/mods/machine --lock .work/machine.lock
```

`pack`当前生成锁文件，记录目录文件摘要，不生成安装压缩包。包检查与锁定工具的实现见[torse_mod.rs](../../crates/mod_host/src/bin/torse_mod.rs)。

## 创建自己的包

`new`可生成清单、组件内容、行为代码和场景。当前模板需要显式设置SDK与WIT的路径；将包放在`examples/mods/my-machine`时，PowerShell示例如下：

```powershell
$env:TORSE_SDK_PATH = "../../../crates/mod_sdk"
$env:TORSE_WIT_PATH = "../../../crates/mod_api/wit"
cargo run -p torse-mod-host --features package --bin torse-mod -- new examples/mods/my-machine
Remove-Item Env:TORSE_SDK_PATH, Env:TORSE_WIT_PATH
```

其他Shell设置同名环境变量即可，路径相对于生成的Cargo.toml与源码所在crate解释。生成后将`examples/mods/my-machine`加入根Cargo.toml的`workspace.members`；清单中的`build.workspace = "../../../"`指向仓库根目录。使用其他目录时，需同步调整这些路径。

随后按机器示例的方式执行`check`与`test`，将包目录和场景路径替换为新目录。提交贡献前补充包用途、授权需求与验证结果，要求见[贡献指南](../../CONTRIBUTING.md)。

## 接入服务端

启用`mods`特性后可加载机器包：

```sh
cargo run -p torse-server --features mods -- .work/mod-demo --mod examples/mods/machine --machine-operators
```

保持服务端运行，在另一终端连接匹配内容的无GPU客户端：

```sh
cargo run -p torse-server --features mods --example headless_client -- 127.0.0.1:7777 --mod examples/mods/machine
```

`--machine-operators`是显式演示策略，授予机器及输入、输出端口的读取和库存转移权限；默认不授予。创建与重开世界时需使用相同内容。现有图形客户端仍使用基础内容装配，尚未加载匹配Mod内容或接入真实机器面板。

机器进度、库存计划与新唤醒在持久任务中共同结算。包可声明绑定方块行为；动作成功时，放置与破坏通知和世界修改共同持久化。机器UI公共模型位于[mod_api](../../crates/mod_api/src/lib.rs)，图形显示与操作仍待接入。

## 验证与限制

```sh
cargo test -p torse-mod-host --features package --all-targets --locked
cargo test -p torse-server --features mods --all-targets --locked
```

包格式、跨包依赖、Merkle锁与工具验证见[包工具记录](../../benchmarks/archive/mods/M4-mod-package.md)；持久唤醒与恢复见[持久Mod执行](../../benchmarks/archive/mods/M4-mod-durable.md)，跨模块共同提交见[Mod原子执行](../../benchmarks/archive/mods/M4-mod-atomic.md)。这些记录描述各次验证时的范围，当前实现以代码和本说明为准。

包API、WIT与内容格式仍在演进，没有通用兼容承诺。依赖包需要一同提供给装载器，尚无远程包发现或注册表。SDK通过仓库路径引用，模板暂不能直接作为独立安装包使用。

## WIT动作边界

当前包API为0.3，WIT包为`torse:behavior@0.3.0-draft`。0.2行为组件需要用新WIT重新生成绑定并构建，装载器会拒绝旧API声明。生命周期字段属于行为ABI变更，网络消息与已有世界记录的字段布局不变；库存目录参与规则摘要后，旧规则身份仍需显式迁移。

Atom保留标量，并通过Tree传递嵌套List和Record。树节点按后序排列，子索引必须小于父索引，每个非根节点只能被引用一次。宿主拒绝循环、共享节点、未使用节点、重复字段、超深及超额数据，转换后继续执行原生组件限制。

DurableGuest::plan_interaction接收宿主配置的InteractionRoute、认证调用者和类型化ActionRequest。Guest可见调用者身份、动作版本、目标和参数。宿主取模块授权与调用者授权的交集，限定查询范围，并用同一权限交集规划结果。返回的InteractionPlan只含未提交的事务组和唤醒，宿主负责把二者共同持久化并在成功后确认请求。该接口用于宿主动作适配，示例网络监听器尚未自动注册任意Guest动作。

Plan可同时返回组件编辑、最多64次库存转移，以及最多256次实体生成和移除。生成身份由原生准备阶段分配；Guest不能指定身份。生成需要spawn权限和目标格授权，移除需要despawn权限、实体授权及组件所有权。所有成员在同一个事务组中准备和发布。

机器示例每轮使用共享端点的两次转移。其交互分支另有transfer、set、spawn和despawn动作，真实Wasm测试覆盖嵌套状态、权限拒绝、原生计划一致性与重放。图形面板和网络动作路由仍需由具体宿主装配。
