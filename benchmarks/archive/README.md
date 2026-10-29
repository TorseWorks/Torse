# 历史验证与测量记录

本目录保存已归档的验证与测量记录，按语义分组。

## 世界、客户端与图形

| 记录 | 内容 |
|---|---|
| [技术实验](foundation/M0-experiments.md) | 内容编译、ECS、Wasmtime与GPU初始化 |
| [世界场景](world/M1-scenario.md) | 分区、生成、碰撞、网格与卸载 |
| [离屏渲染](client/M1-client.md) | 实际GPU管线、网格上传与相机 |
| [窗口交互](client/M1-window.md) | 连续渲染、飞行、流式装载与截图 |
| [方块动作](client/M1-actions.md) | 射线选择、放置、破坏与动作授权 |
| [半砖遮挡与合并](world/M1-slab-completion.md) | 遮挡掩码只声明完整覆盖面、同格半砖合并与编译器校验 |
| [透明方块遮挡规则](world/M1-transparent-occlusion.md) | Mask与Blend材质不声明遮挡，透过玻璃保留邻面 |
| [同材质接触面合并](world/M1-face-merge.md) | 接触面同材质贴面剔除，玻璃相邻无重合闪面 |
| [行走](simulation/M1-walk.md) | 移动事务、重力、碰撞与跳跃 |
| [界面基础](client/M1-ui.md) | HUD、菜单、机器面板预览与输入处理 |
| [界面主题改版](client/M1-ui-style.md) | 程序化深色扁平主题、布局重排与皮肤素材移除 |
| [地形贴图](world/M1-textures.md) | 逐面材质、图集与材质表、透明模式与色调 |
| [帧预算](performance/M6-frame-budget.md) | 按分区上传、流式生成分摊与帧时对照 |
| [按帧伸缩](performance/M6-frame-pace.md) | 基于测量帧时的生成、网格和上传预算 |
| [按行占用位](performance/M6-row-occupancy.md) | 满格行级跳过、慢路径等价、共享临时预算与CPU微基准 |
| [有界执行器](performance/M6-bounded-executor.md) | 生成与网格线程池、取消、背压、过期结果和串行等价 |
| [客户端开发构建性能](client/CLN-debug-performance.md) | 客户端开发构建性能 |
| [客户端大视距与呈现修复](client/CLN-large-view.md) | 客户端大视距与呈现修复 |
| [长途流送与内存压力下的界面呈现](client/CLN-streaming-memory.md) | 长途流送与内存压力下的界面呈现 |
| [帧反馈与网格合并验证](performance/M6-adaptive-meshing.md) | 帧反馈与网格合并验证 |
| [客户端光照连续性与输入修复](performance/M6-client-stability.md) | 客户端光照连续性与输入修复 |
| [光照与透明呈现验证](performance/M6-derived-lighting.md) | 光照与透明呈现验证 |
| [GPU几何驻留与视锥剔除](performance/M6-gpu-residency.md) | GPU几何驻留与视锥剔除 |

## 模拟与事务

| 记录 | 内容 |
|---|---|
| [提交仲裁](simulation/M2-commit.md) | 前置条件、稳定排序与冲突拒绝 |
| [Tick驱动](simulation/M2-tick.md) | 阶段排序、阶段末提交与超预算报告 |
| [逻辑步与呈现帧](simulation/M2-presentation-frame.md) | 固定步长、追赶上限、相机插值、暂停与联网副本保护 |
| [串行基线](performance/M6-serial-baseline.md) | 规划、准备、编码、发布、授权投影与刷盘的分项采样及原始数据 |
| [冻结前并行规划](performance/M6-parallel-planning.md) | 独立规则有界并行、稳定归并、日志等价与冻结阶段恢复 |
| [运行态与批量提交](performance/M6-runtime-commit.md) | 进度记录、共享预算缓存、批量同步、混合版本恢复与预验证成本 |
| [移动预验证与投影](performance/M6-movement-prevalidation.md) | 有界碰撞预计算、串行复核、完整路径测量与授权投影扫描优化 |
| [授权增量投影](performance/M6-incremental-projection.md) | 按提交目标更新订阅、状态引用计数、完整容量校验和对照测量 |
| [组件预验证](performance/M6-component-prevalidation.md) | 复用成员校验批次、可选组件并行、完整路径收益与保守预留 |
| [真实多机器](performance/M6-multi-machine.md) | 实际Wasm负载、事件成本复用、完整日志与恢复对照 |
| [交换载荷](simulation/M2-exchange.md) | 快照、变更记录与重放 |
| [实体移动](simulation/M2-movement.md) | 碰撞结果、准备记录与移动恢复 |
| [动作与库存](simulation/M2-inventory.md) | 统一授权、争用、守恒转移与组件事务 |
| [跨模块事务](mods/M4-cross-module.md) | 多来源原子提交、所有权与整组失败 |
| [内核一致性](simulation/M2-core-consistency.md) | 内核一致性 |
| [模块系统声明执行](simulation/M2-declared-systems.md) | 模块系统声明执行 |
| [放置占用复核](simulation/M2-placement-clearance.md) | 放置占用复核 |

## 持久化与恢复

| 记录 | 内容 |
|---|---|
| [文件持久化](persistence/M1-persistence.md) | 世界编码、日志与检查点的早期组合验证 |
| [领域恢复](simulation/M2-recovery.md) | 快照加日志尾重建与续号 |
| [本地世界装配](persistence/M3-local-world.md) | 真实权威接入文件协调器与进程退出 |
| [实体生命周期](persistence/M3-entity-lifecycle.md) | 生成、销毁、分配水位与版本2读取 |
| [文件锁回归](persistence/M3-file-locks.md) | 描述符副本、解锁与跨平台CI |
| [存储回收](persistence/M3-storage-reclamation.md) | 分段日志、manifest整理、引用页回收与中断恢复 |
| [事件感知流送](persistence/M3-event-streaming.md) | 局部事件票据、有界页I/O、生成器身份与完整归档 |
| [阶段进度](persistence/M3-phase-progress.md) | 阶段恢复与有界关闭 |
| [持久执行与网络](network/B6-B7-execution-network.md) | 任务、事件、结果、去重与真实服务端程序 |
| [动作组合与原子提交](persistence/M3-action-composition.md) | 动作组合与原子提交 |
| [动作实体生命周期](persistence/M3-action-lifecycle.md) | 动作实体生命周期 |
| [显式存档迁移](persistence/M3-archive-migration.md) | 显式存档迁移 |
| [原子操作工具链兼容](persistence/M3-atomic-compatibility.md) | 原子操作工具链兼容 |
| [组件配置装配验证](persistence/M3-component-configuration.md) | 组件配置装配验证 |
| [传播光照验证](persistence/M3-emitted-light.md) | 传播光照验证 |
| [库存原子操作规划](persistence/M3-inventory-operations.md) | 库存原子操作规划 |
| [实体坐标边界验证](persistence/M3-position-bounds.md) | 实体坐标边界验证 |
| [只读兼容预检](persistence/M3-readonly-preflight.md) | 只读兼容预检 |
| [分区存储压缩](persistence/M3-section-compression.md) | 分区存储压缩 |
| [持久分区装卸](persistence/M3-section-streaming.md) | 持久分区装卸 |
| [会话范围与旧订阅失效](persistence/M3-session-scope.md) | 会话范围与旧订阅失效 |
| [快照结构校验验证](persistence/M3-snapshot-validation.md) | 快照结构校验验证 |

## 网络与授权

| 记录 | 内容 |
|---|---|
| [授权复制](network/M5-authorized-replicas.md) | 基线、完整增量、对象过滤与失效 |
| [联网客户端](network/M5-net-client.md) | TCP握手、授权镜像、实体与访客拒绝 |
| [联机编辑](network/M5-net-edit.md) | 范围授权、服务端提交与客户端更新 |
| [权威移动](network/M5-authoritative-movement.md) | 玩家绑定、有界输入、持久结算、预测校正与重连 |
| [用户认证](network/M5-user-authentication.md) | 双向TLS、用户授权、玩家分配、重复登录与重放拒绝 |
| [授权验收](network/A8-acceptance.md) | 双客户端一致性、越界拒绝、撤权与重连 |
| [客户端会话心跳](network/B7-heartbeat.md) | 客户端会话心跳 |
| [玩家兴趣范围自动跟随](network/M5-player-follow.md) | 玩家兴趣范围自动跟随 |

授权复制、联机编辑与权威移动已接入协议及TCP，客户端独立预测并按持久结果校正。服务端支持显式范围替换，也支持在批准范围内自动跟随玩家调整兴趣窗口，配置与限制见[服务端说明](../../apps/server/README.md)。

## Mod平台

| 记录 | 内容 |
|---|---|
| [组件边界](mods/M4-mod-component.md) | WIT绑定、查询、计划、燃料与中断 |
| [权威桥](mods/M4-mod-authority.md) | 真实世界、库存和机器行为 |
| [包工具](mods/M4-mod-package.md) | 清单、内容、多包依赖、锁文件与场景工具 |
| [持久Mod执行](mods/M4-mod-durable.md) | 唤醒、请求去重与恢复 |
| [Mod原子执行](mods/M4-mod-atomic.md) | 机器状态、库存计划与唤醒共同结算 |
| [宿主预算边界](mods/M4-mod-budgets.md) | 受检查询、累计Host工作、ABI复制、Store资源及失败恢复 |
| [运行时资源加载验证](mods/M4-cooked-runtime.md) | 运行时资源加载验证 |
| [实体碰撞体](mods/M4-entity-colliders.md) | 实体碰撞体 |
| [Minimal machine UI and authorized actions](mods/M4-machine-ui.md) | Minimal machine UI and authorized actions |
| [Mod动作与规则身份](mods/M4-mod-actions.md) | Mod动作与规则身份 |

现有机器状态与动作模型已供服务端和无GPU客户端验证，图形客户端的真实机器面板仍待接入。开发入口见[Mod示例与工具](../../examples/mods/README.md)。

## 记录要求

共享账户的生命周期、并发峰值和背压验证见[共享内存预算装配](performance/M6-shared-memory.md)。

新增记录应写明代码与依赖版本、系统、硬件、内容配置和工作负载，给出可复跑命令，并保留原始指标。区分功能正确性、进程退出、硬件断电与性能测量，未验证的范围应明确说明。临时世界、日志和截图放入`.work/`，最终画面与必要夹具随对应记录保存。
