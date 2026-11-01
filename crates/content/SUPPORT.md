# 独立面支撑

FaceSupport描述规则可以要求的静态面支撑条件。它与视觉透明、碰撞体、选中形状和遮挡无推导关系。一个透明表面可以显式提供支撑，一个完整碰撞盒也可以不提供支撑。

## 声明与边界

首版使用固定的SupportCategory词汇，不开放运行时类别注册。

| 类别 | 字段 | 含义 |
|---|---|---|
| Attachment | attachment | 规则可以将对象附着于该面 |
| Structural | structural | 规则可以把该面作为结构支撑条件 |

两个类别分别使用u8掩码，六位顺序与Face::ALL一致：-x、+x、-y、+y、-z、+z。其余位非法。类别之间不隐含包含关系；Structural不声明承重数值，也不会自动提供Attachment。它们是规则谓词，不授予放置、消耗、修改或访问世界的权限。

BlockDefinition::face_support保存默认声明。StateProfile::face_support用Option区分继承与替换：None继承定义，Some(FaceSupport::NONE)取消全部支撑，其他Some值替换两个完整掩码。覆盖按有限字段的规范顺序匹配精确状态，不做逐类别合并。

```rust
use torse_content::{Face, FaceSupport, SupportCategory};

let support = FaceSupport {
    attachment: 1 << Face::PosY.index(),
    structural: 0,
};
assert!(support.supports(Face::PosY, SupportCategory::Attachment));
assert!(!support.supports(Face::PosY, SupportCategory::Structural));
```

每个定义或覆盖只有两个固定长度掩码，没有额外可变类别表。既有每方块4096种有限状态及全局稠密状态空间上限继续适用；编译器拒绝非法掩码、重复或非法状态组合，排序后再计算摘要。

旧原生构造点与当前作者格式适配器显式使用FaceSupport::NONE，预设不根据形状生成支撑。当前TOML适配器没有新增支撑字段解析；宿主通过原生内容声明编译本能力，其他导入器需要显式接入。不会根据内容名称推断支撑。

## 注册表与授权读取

BlockStateRegistry::face_support借用当前状态的有效声明。空占用返回空声明，未知状态返回None。这是公开静态元数据查询，不授予任何世界单元的访问权。

ActionView::face_support接收单元坐标与未发布Transaction，先检查目标授权，再读取地形。Ready单元返回借用的声明，并把观察到的状态加入事务读条件；重复读取相同值不增加条件，已有不同值条件返回Conflict。

FaceSupportQuery分别保留Loaded、NotLoaded、OutOfBounds和Exterior。Loaded空单元仍记录空状态读条件；未加载区域、封闭外部和已知空外部均不提供正支撑。调用者需要区分这些结果来决定拒绝、等待或执行自己的规则，不能把未加载数据当成空气。

准备和发布均验证保留的地形读条件。源单元改变或卸载时，依赖它的写入不发布。读条件随现有延迟事务和跨模块原子组保留，不新增存档或协议字段。读取支撑不会修改世界，也不会取消既有动作权限与组件所有权检查。

## 摘要与缓存

编译器在权威字节流中追加标记0xf2、版本1的支撑段。按方块键排序，写入定义的两个掩码，再按精确状态顺序写入显式替换；省略覆盖与显式空覆盖保持可区分。不含支撑声明的内容保持此前编译摘要。支撑信息不进入呈现字节流。

[离线表版本3](COOKED.md)保存每个展开状态的有效掩码，读写两端均校验。支撑修改改变权威兼容身份，纯支撑修改保持呈现身份。版本1和版本2缓存明确拒绝，须从源重新生成。已有世界不能绕过权威兼容检查使用改变后的规则。
