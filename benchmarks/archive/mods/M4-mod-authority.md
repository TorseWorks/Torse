# M4前置：行为Mod接入真实权威

日期：[20261024]
类型：验收
状态：已验收
环境：未记录
范围：P2原型，非M4验收

## 目标

维护：Mod平台侧(T05)。前置：[M4-mod-component.md](M4-mod-component.md)。

### 问题

把组件边界从夹具宿主接到真实权威：import读真实`WorldAuthority`，返回的计划经既有事务接口提交，并用一台加工机器示例验证加工、原子性、权限与故障路径。

## 方法

### 实现

- `crates/mod_host/src/authority.rs`(feature `authority`)：`AuthorityHost`持有`WorldAuthority`，实现`query`/`diagnostics`/`schedule`三类import；`ModInstance`封装实例与调用循环，trap后丢弃实例并从同一宿主数据重建；`machine_scenario`组装内容（库存、方块动作、机器schema）、权威、三实体（机器/输入箱/输出箱）与授权上下文。
- `examples/mods/machine`：机器guest。每次唤醒推进私有进度，第四步在同一计划中转移一颗石头（输入箱槽0->输出箱槽0）并归零进度；`spin`/`intrude`特性分别供trap与越权测试。
- 计划到事务：块编辑按读集规则生成前置；组件编辑检查`owner_module == 包身份`；转移经库存`plan_transfer`产出独立事务。跨模块无法共事务（协调器按source校验组件所有权），计划按来源拆分后**顺序prepare->publish**，前置检查各事务目标不相交。
- WIT升到`torse:behavior@0.2.0-draft`：组件与转移改实体挂靠（A侧组件在实体上，库存转移按实体+槽位），唤醒绑定实体，新增`unsupported`错误码。

## 结果

`cargo test -p torse-mod-host --features authority --test authority_machine`，5项全部通过：

| 测试 | 判定 |
|---|---|
| machine_processes_and_transfers | 6个Tick：进度1->2->3->转移并归零->1->2；输入箱空、输出箱一颗石头；序号随提交推进 |
| failed_transfer_leaves_no_partial_processing | 空输入时整个计划被拒：进度停在3、序号不变、输出为空；补货后重调度的唤醒完成加工并归零 |
| foreign_schema_ownership_is_rejected | guest试图改库存拥有的schema:prepare前即拒，零提交 |
| consecutive_failures_deactivate_the_handler | 3次提交后连续8次拒绝触发停用，后续派发返回Deactivated |
| trap_rebuilds_the_instance_from_committed_state | spin guest燃料耗尽trap，权威零残留、失败计数+1、唤醒重排；重建后的实例可再次执行 |

边界成本复测（WIT v2，与P1同法）：实例化139.1µs，组件3.18–3.21µs/调用，原生约0.33µs，约9.6–9.7倍，与P1一致，无回归。

默认工作区`cargo test --workspace --all-targets` 400项通过；`cargo fmt`与`cargo clippy`（默认与`authority`/`component`特性组合）零告警。

### 发现与决定

1. **跨模块原子性**：协调器强制组件编辑归属事务source，机器进度（机器schema）与库存转移（库存schema）必须分属两个事务。落地语义：一次计划->多个按来源拆分的事务，前置检查目标不相交，顺序prepare->publish；任何首个prepare拒绝即整计划丢弃，后续prepare在目标不相交下不可能因前序发布失效，若仍拒绝按不变量故障传播。设计文档DM02据此细化。
2. **prepare捕获提交位置**：全部prepare后再publish会撞上"prepared change is not at the current commit position"不变量，必须每个事务prepare后立刻publish。
3. **转移拒绝是业务拒绝**：`plan_transfer`的Conflict等映射为计划级Rejected（整计划丢弃+失败计数），不作为宿主错误。
4. **唤醒必须绑定对象**：派发需要self对象，唤醒登记与事件载荷携带实体身份。
5. 首版计划至多一个转移；`plan_transfer`逐个转移产出事务，同对象多转移会因previous失效互相冲突，批量库存原语属A侧后续。

## 复现

未记录

## 边界

### 已知边界

- 唤醒队列仍是进程内，不持久；等B6定时事件记录。
- 库存槽位是嵌套List(Record)，不可过扁平atom边界；机器经`plan_transfer`原语操作库存，不直读槽位，嵌套表示仍为开放问题。
- `block-placed/broken`事件已定义未派发；场景用宿主直接构造实体（P2不涉及实体生成事务）。
- 机器的schema/实体由宿主场景装配；数据包与清单装载是P3工具链范围。
