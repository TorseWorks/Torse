# 库存标准模块

可选包torse-inventory提供物品目录、固定容量槽位、实例数据和原子操作。世界内核只处理通用组件、身份、模式与提交，不依赖库存模块。

## 装配与执行

先调用register_content注册模式、动作和item_port能力，编译内容后创建ItemCatalog。冻结前调用ItemCatalog::bind_schemas注册嵌套语义校验，对内容兼容信息调用一次ItemCatalog::bind_compatibility，再冻结ComponentSchemas。将同一目录的handlers交给ActionService::bind。校验器规则身份自动进入组件配置摘要。

ActorContext由宿主根据认证身份、范围和访问规则建立。请求只指定意图，不自报权限。处理器通过ActionView读取获准对象，输出事务计划，由WorldAuthority准备和发布。计划成功不代表已经提交或持久化。

交互动作运行于Interaction阶段。原生机器通过plan_transfer和Transfer调用同一路径，可运行于Interaction或Simulation，仍检查权限、范围、容量和守恒。

## 组件与实例

inventory:schema/container包含长度1至64的inventory:field/slots列表。空槽是空Record；非空槽包含item（Content）、count（U32）、data_format（U32，当前为1）、data（Record），字段均使用inventory:field/前缀。数量为正且不超过物品上限。ItemStack::new创建空数据，with_count保留实例字段。

记录按键排序，列表保序，类型和文本精确比较，未知字段原样保存。ItemPolicy可声明已识别U32属性的范围与必填要求，或进一步禁止合并；不隐式补默认值、转换类型或删除属性。

Transfer和Insert按物品键与完整数据合并，Consume和Extract精确匹配实例。拆分、交换保留未知字段。同容器操作最终只生成一次组件替换，同槽转移或交换拒绝。Consume旧值与Insert新值可在拥有者批次内完成变换，分别需要consume和insert权限。所有端点还需要独立授权。

plan_inventory和plan_transfers在私有草稿执行顺序操作，任意失败丢弃整个批次。完整原组件作为previous观察，属性变化也会冲突。提取值为暂定结果，只能在整个事务组持久接受后对外兑现。已有主机请求去重与持久化应答契约继续适用。

## 资源边界

| 范围 | 上限 |
|---|---|
| 实例数据计量字节 | 4096 |
| 实例值节点 | 256 |
| 实例字段值深度 | 4，首层为0 |
| 每个记录或列表成员 | 64 |
| 文本UTF-8字节 | 1024 |
| 属性键或内容引用UTF-8字节 | 128 |
| 容器格数 | 64 |
| 计划操作与容器数 | 各64 |
| 计划逻辑计量 | 8MiB |

validate_instance_data采用component_size口径，计入根计数、键长度与标签。容器封装还须满足通用组件65536计量字节、4096值节点与深度8的边界。计划累计输入、期望观察、草稿、替换和提取值，共享主机保留及事务编辑限制继续独立执行。这些计量不代表线格式大小或进程RSS。字段编码见[世界载荷格式](../../crates/codec/WORLD.md)。

## 恢复与兼容

拥有者校验器经ComponentSet统一执行，覆盖写入、快照恢复、日志重放及授权复制解码。必须在冻结前绑定，冻结后不能加入或覆盖。库存保持Authorized可见性，未授权读取者不能取得实例字段。

容器模式与item-port能力版本为2，inspect、transfer动作API为2，请求参数模式仍为1。新读取器接受旧两字段堆叠并映射为空，新写入器总写四字段，包括空数据。未知格式或不完整封装拒绝，旧读取器不能读取新堆叠。不匹配的存档或客户端身份拒绝接入，不自动改写源文件；本次不提供离线迁移。

ItemCatalog::rules_digest覆盖排序后的物品身份、容量、策略、格式、比较规则与预算。普通实例变化只改变世界状态，呈现信息不改变规则身份。

通用世界、网络协议与WIT值词汇沿用既有类型，不新增浮点、字节数组或U64属性。当前扁平WIT读取接口遇嵌套实例返回Unsupported；原生接口、通用组件复制及真实Wasm物品转移路径保留完整数据。

## 验证入口

```sh
cargo run -p torse-inventory --example competition --locked
cargo test -p torse-inventory --all-targets --locked
cargo test -p torse-items --all-targets --locked
```

测试覆盖规范化和类型区别、未知嵌套字段、精确操作、授权变换、批次失败、并发冲突、预算边界、真实Codec快照与日志恢复、授权seed和delta、重连及外围掉落。

## 后续工作

库存已接入服务端授权请求、持久提交与Mod机器任务。实体生成和销毁由通用事务处理。配方与能量库仍为预留入口，通用文本命令和真实图形库存操作尚待实现。
