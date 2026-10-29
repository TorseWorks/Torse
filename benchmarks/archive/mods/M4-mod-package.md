# M4前置：包格式、工具链与多包装载

日期：[20261024]
类型：验收
状态：已验收
环境：未记录
范围：P3原型，非M4验收

## 目标

维护：Mod平台侧(T05)。前置：[M4-mod-authority.md](M4-mod-authority.md)。

### 问题

行为Mod需要从"宿主代码装配内容"变为"包目录声明内容"：清单与数据文件的可验证装载、跨包依赖解析与冲突检测、整合包锁文件，以及作者侧的四命令工具链(new/check/pack/test)与无头场景运行器。

## 方法

### 实现

- `crates/mod_host/src/package.rs`(feature `package`)：`mod.toml`清单解析与校验（id必须是合法小写命名空间、`api = "0.2"`必须匹配宿主、side枚举、权限键、依赖版本要求`X.Y`兼容次版本/`X.Y.Z`精确）；`content/*.toml`数据文件（`[[component]]`、`[[state]]`、`[[block]]`，字段类型/持久化/复制映射到引擎描述符）；多包解析（拓扑排序、循环与缺失依赖、版本不满足、同包重复内容）；每包文件级sha256的Merkle摘要与`mod.lock`锁文件（写入与校验）。
- `crates/mod_host/src/scenario.rs`：夹具世界TOML（实体、组件初值、`@name`实体引用、库存槽、种子唤醒、Tick步进、断言）、`BehaviorRuntime`（所有行为包实例共享一个store与权威，trap后仅重建该包实例）、按包诊断统计（派发/提交/拒绝/trap/空计划/停用/耗时）与宿主计量。
- `crates/mod_host/src/bin/torse_mod.rs`:`torse-mod` CLI，`new`生成模板包（清单+内容+夹具+guest骨架+Cargo），`check`校验并可选验锁，`pack`写锁，`test`构建wasm并跑夹具场景。
- `examples/mods/machine`转为真包：`mod.toml`+`content/machine.toml`+`scenario.toml`，内容不再由宿主代码注册。
- 权威桥多包化：AuthorityHost按包持有授权(ActorContext)与自有schema，故障计数与停用按包，唤醒记录携带来源包，派发回到正确的实例。

## 结果

`cargo test -p torse-mod-host --features package --test package`，6项全部通过：

| 测试 | 判定 |
|---|---|
| manifest_and_content_validation | api不匹配、内容越命名空间、孤儿组件（未挂状态模式）全部在装载期报告 |
| dependency_resolution_and_conflicts | 依赖按序解析；版本不满足、缺失依赖、循环依赖、同包重复内容各有定位错误 |
| lock_roundtrip_detects_tampering | 锁往返校验通过；篡改文件后Merkle不匹配被拒绝 |
| machine_package_runs_the_scenario | machine包从目录装载->wasm构建->6 Tick->三项断言全过（进度2、输入空、输出1石），统计6派发6提交0拒绝 |
| new_template_passes_check | `new`模板通过`check`（CLI二进制端到端） |
| version_types_roundtrip | 版本与版本要求语义 |

CLI实测：

```text
$ torse-mod check examples/mods/machine
ok: machine 0.1.0 (5 files, merkle 774be1c40853)

$ torse-mod test examples/mods/machine --scenario examples/mods/machine/scenario.toml
package machine: dispatches=6 committed=6 rejected=0 trapped=0 no-plan=0 deactivated=false wall=635.1µs
commit-seq: 7
assert ok: field machine:field/progress on `machine`
assert ok: slot 0 on `input`: None
assert ok: slot 0 on `output`: Some(ItemStack { item: base:stone, count: 1 })
scenario passed
```

commit-seq为7：六个计划中第四步拆两个事务（机器组件编辑+库存转移），与P2的按来源拆分一致。

默认工作区420项通过；fmt与clippy（默认、component、authority、package四种组合）零告警；依赖新增`toml = "=1.1.8"`、`sha2 = "=0.10.9"`（与内容编译器同版本）。

### 设计决定记录

1. **命名空间即包身份**：包id必须是合法内容命名空间，包内所有声明键的命名空间必须等于包id。跨包同名内容在结构上不可能，重复检测只需覆盖同包多文件。
2. **组件必须显式挂到状态模式**：引擎注册表只经StateSchema收集组件，孤儿组件在装载期报错而非静默丢弃。
3. **多包运行时**：所有实例共享一个store与权威；trap只重建该包的实例（store与权威不受污染），唤醒按来源包路由，故障停用按包。
4. **测试模式授权策略**：`test`命令默认授予清单申请的全部权限(GrantPolicy::AllRequested)并在诊断中呈现；真实服务器传显式策略，二者共用同一装载路径。
5. **模板的路径参数**：`new`生成的guest骨架依赖torse-mod-sdk与WIT的路径，经`TORSE_SDK_PATH`/`TORSE_WIT_PATH`或默认相对路径注入；发布型安装布局属后续。

## 复现

未记录

## 边界

### 已知边界

- 锁文件为目录级（无zip/签名）；`entry-hash`仅在包目录内含产物文件时写入，构建产物默认在target目录。
- 依赖解析要求全部依赖在装载集合内；仓库/注册表发现延后。
- 夹具组件值仅支持u32/i64/bool/@实体引用；text与content引用待需要时加。
- `block-placed/broken`事件仍未派发；能力声明(`[[capability]]`)与动作声明未上TOML，需要时按同一模式扩展。
