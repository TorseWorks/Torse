# 贡献指南

Torse仍处于早期开发阶段，欢迎提交问题报告、文档修订、测试和代码改进。提交前请先阅读[项目介绍](README.md)，了解已实现的功能与当前限制。

## 开始之前

小范围的文档修正或明确的Bug修复可以直接提交Pull Request。新功能、架构调整、公共接口，以及存档、网络协议或Mod边界的变更，请先在[Issues](https://github.com/TorseWorks/Torse/issues)说明用途和方案，确认范围后再开始实现。

报告问题时，请提供使用的提交版本、操作系统、运行命令、预期结果和实际结果。涉及恢复问题时，保留原始存档，使用副本复现。日志和附件中请去除口令、访问令牌和个人信息。

贡献可以从修正文档、补充复现用例或改进错误提示开始。若不确定某项工作是否有人在做，可以在相关Issue下说明计划。

## 准备环境

仓库使用Rustup管理Rust工具链，[rust-toolchain.toml](rust-toolchain.toml)固定版本为1.93.1，并包含Rustfmt与Clippy。Windows原生构建需要MSVC构建工具。GPU探测需要可用的Vulkan、DirectX 12或Metal后端；无GPU的环境可以运行服务端和无界面场景。

先Fork仓库，再克隆自己的副本并创建工作分支：

```sh
git clone https://github.com/YOUR_ACCOUNT/Torse.git
cd Torse
git remote add upstream https://github.com/TorseWorks/Torse.git
git switch -c fix/short-description
rustup show
cargo check --workspace --locked
```

在仓库根目录执行后续命令，保留提交的Cargo.lock。Mod行为代码还需要安装Wasm目标：

```sh
rustup target add wasm32-unknown-unknown
```

运行方式见[服务端](apps/server/README.md)与[Mod示例](examples/mods/README.md)。各库职责见[框架库说明](crates/README.md)。

### 编辑器分析

rust-analyzer需要工具链的标准库源码，先在仓库根目录执行`rustup component add rust-src`。安装中断后出现`missing manifest in toolchain`时，应修复对应版本的安装，再核对`rustc --version`、`rustup component list --installed`和`cargo check --workspace --locked`，不要通过关闭诊断掩盖问题。

新版分析器与旧标准库可能不兼容。[rust-analyzer安装说明](https://rust-analyzer.github.io/book/installation.html)说明其标准库支持范围。本机已验证Rust 1.99.1可用于当前编辑器分析，仓库命令行构建仍使用固定的1.93.1。需要分开配置时，先安装分析工具链并查询其标准库目录：

```sh
rustup toolchain install 1.99.1 --profile minimal --component rust-src,rustfmt,clippy
rustup run 1.99.1 rustc --print sysroot
```

在本机工作区设置中配置下列项；已有环境变量保留并合并。这些绝对路径不提交到仓库。

| 设置 | 值 |
|---|---|
| `rust-analyzer.server.extraEnv` | `RUSTUP_TOOLCHAIN`设为`1.99.1` |
| `rust-analyzer.cargo.extraEnv` | `RUSTUP_TOOLCHAIN`设为`1.99.1` |
| `rust-analyzer.cargo.sysroot` | 上述查询命令的输出目录 |
| `rust-analyzer.procMacro.server` | 该目录内的`libexec/rust-analyzer-proc-macro-srv`，Windows文件名带`.exe` |

打开多个工程的父目录时，通过`rust-analyzer.linkedProjects`列出正式的`Cargo.toml`，并用`rust-analyzer.files.exclude`排除`.work`等临时目录。分析可选Mod工具和示例时启用`rust-analyzer.cargo.features = "all"`，否则未启用特性的目标可能缺少依赖。修改工具链、标准库或宏服务后重启分析器；宏动态库与服务须来自同一编译器版本。编辑器分析通过不代替固定工具链的提交前检查。

## 修改约定

一次PR集中处理一个问题。格式整理、重构和行为修改尽量分开，避免审核者需要从无关改动中寻找实际变化。遵循相邻代码的命名与组织方式，使用仓库现有的Rustfmt配置。

源码、注释、日志、错误消息和提交信息使用英文。用户界面文案放入资源文件；文档可以使用中文，表达以准确、直接为准。说明当前行为时，请核对实现，避免将计划中的能力写成已完成。

修改行为时应提供能说明问题的验证。Bug修复宜有能复现原问题的回归测试；涉及授权、事务、持久化或恢复时，需覆盖拒绝和失败路径。纯文档或简单格式调整不要求增加测试代码。

## 测试与验证

提交代码前执行基础检查：

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

默认工作区测试不会启用所有可选特性。修改Mod宿主、包工具或服务端Mod接入时，还需执行对应检查：

```sh
cargo test -p torse-mod-host --features package --all-targets --locked
cargo clippy -p torse-mod-host --features package --all-targets --locked -- -D warnings
cargo test -p torse-server --features mods --all-targets --locked
cargo clippy -p torse-server --features mods --all-targets --locked -- -D warnings
```

这些Mod测试会调用Cargo构建Wasm示例，需要安装前述Wasm目标。窗口、输入和GPU表现需在本地运行验证，记录复现命令，并按需要提供截图。现有CI执行基础检查与部分技术实验，不代替全部可选特性和图形交互验证。

可复跑的场景入口见[测试说明](tests/README.md)，已有验证过程见[验证记录索引](benchmarks/README.md)。运行产生的世界、日志和截图放入`.work/`，固定测试夹具除外。

## 接口与数据兼容

世界修改经过动作授权、事务准备与统一提交，渲染和网络层通过公开接口读取结果。修改时应保持权限检查、失败无副作用和持久确认等既有语义。

公共类型变化需要检查调用方与对应测试。修改存档、执行记录、协议或WIT边界时，同时更新版本处理、格式说明、固定字节样例和恢复验证。需要破坏兼容性时，应在Issue和PR中明确影响，说明旧数据会被拒绝、迁移还是继续读取。

相关格式说明：

- [记录包裹](crates/codec/FORMAT.md)与[世界载荷](crates/codec/WORLD.md)。
- [执行状态与网络载荷](crates/codec/EXECUTION.md)。
- [检查点格式](crates/storage/FORMAT.md)与[应用协议](crates/protocol/FORMAT.md)。
- [Mod接口定义](crates/mod_api/wit/behavior.wit)。

## 提交与审核

将工作分支推送到自己的Fork，向本仓库的`main`分支提交PR。说明解决的问题、修改后的行为和验证结果，并关联已有Issue。未完成的工作可以先提交Draft PR讨论。

维护者会检查实现、测试、兼容性和项目方向，并可能要求调整。修改继续提交到原分支即可更新PR。存在合并冲突时，请同步上游并重新执行受影响的检查。

PR标题与提交信息使用简洁的英文，正文可以使用中文。涉及界面、性能或数据格式的改动，请附上相应证据；没有执行的检查应如实说明。CI通过后仍需维护者审核，合并由维护者完成。

## 文档维护

README与使用指南描述当前实现。功能变化时，请同步更新参数、示例命令和限制，检查相对链接与图片是否有效。

benchmarks中的记录保存各次验证时的环境、结论与未覆盖范围。后续实现完成时，更新索引或补充新的记录，保留原始结果，避免把历史测量改写成当前承诺。

## 贡献许可

提交供本项目合入的原创代码、文档、示例或素材，表示你有权提供这些内容，并同意按本项目的`GPL-3.0-only`许可分发。贡献的版权仍归原作者所有。

引入第三方内容时，请注明来源与许可证，保留要求的版权声明，并在PR中说明与项目许可的兼容性。无法确认来源或授权的内容，请先讨论。许可全文见[LICENSE](LICENSE)。
