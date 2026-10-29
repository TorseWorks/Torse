# 组件配置装配验证

日期：[20261027]
类型：验收
状态：已验收
环境：未记录
范围：可信启动配置、权威兼容性与恢复

## 目标

未记录

## 方法

### 一、统一入口

`ComponentSchemas::assemble(content, configuration, base)`同时装配组件持久策略和方块初始字段，返回冻结的模式集与最终Compatibility。宿主使用返回的Compatibility创建世界、打开存档及建立网络绑定。

| 配置 | 校验 |
|---|---|
| PersistenceSelection | 模式存在、登记方为所有者、策略为OwnerDefined |
| BlockDefault | 方块存在并声明该模式、登记方为所有者、字段满足类型及预算 |
| 整体配置 | 无重复绑定，所有方块初始组件的必需字段齐全 |

未显式提供字段时按空字段集验证，不推测实体引用等初始值。配置总数最多4096项，初始字段的component_size合计不超过8MiB；每个组件继续受原有64KiB、节点和深度限制。失败不会交付部分配置。

## 结果

### 二、规则摘要

有效配置按稳定身份排序，使用带版本的SHA-256域，将基础rules_digest、选择不持久的OwnerDefined模式以及非空初始字段计入最终rules_digest。字段使用类型标签、长度和固定小端整数编码，列表保留顺序，记录按键排序。

登记顺序不影响结果。显式选择保存与默认保存相同，显式空字段与隐式空字段相同。没有有效覆盖时保留基础摘要，既有空配置世界可继续使用原元数据。修改有效策略或字段自动改变规则摘要；authority_schema和content_digest沿用基础兼容记录。

WorldAuthority::new也会冻结原有增量登记接口产生的模式集并计算摘要。已经冻结的模式集只能配合它返回的Compatibility使用，重复绑定该值不会重复散列，后续登记返回ConfigurationFrozen。

### 四、验证结果

```sh
cargo test -p torse-world --test configuration --locked
cargo test -p torse-sim --test core_consistency --locked
cargo test -p torse-server --test core_consistency --locked
cargo test --workspace --all-targets --locked
```

新增6项配置测试、1项隔离恢复测试和1项真实文件重开测试。覆盖登记顺序、有效默认等价、全部嵌套值类型、长度分隔、摘要变化、冻结、所有权、未知方块、未声明模式、重复项、必需字段与容量限制。

文件测试先持久放置带组件的方块，再分别改变初始字段和保存策略。两次重开均拒绝，journal.log和全部检查点文件字节不变；恢复原配置后，实例、字段与分配水位正确，并可继续推进Tick、再次重开。日志兼容记录被修改时，隔离恢复拒绝应用，地形和提交位置不变。

Windows全工作区706项测试通过，零忽略；格式、Clippy、Rustdoc和mods编译通过。

## 复现

未记录

## 边界

### 三、持久与网络边界

恢复、组件快照捕获和增量应用核对冻结配置的Compatibility。日志重放还要求每条记录的兼容记录与当前恢复状态一致，拒绝中途切换规则。文件宿主和网络继续使用原Compatibility字段，不增加存档或协议版本。

旧的非空配置若此前没有计入规则摘要，新入口会拒绝直接打开，须用原宿主或离线迁移核实并转换；不重写元数据来绕过校验。使用基础ComponentSchemas::build进行离线、无绑定交换的旧入口仍存在，生产恢复应传入统一装配后冻结的模式集。

服务端普通启动与Mod演示启动已切换到统一入口。现有持久组件和方块实例的真实进程退出回归也使用该入口。
