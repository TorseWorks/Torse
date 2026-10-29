# 标准玩法模块

| 目录 | 职责 |
|---|---|
| [inventory](inventory/src/lib.rs) | 物品堆叠、容量、端口与受控转移 |
| [items](items/README.md) | 物品目录、放置消耗、破坏回收及背包交互 |
| [crafting](crafting/src/lib.rs) | 配方描述与原子加工结算 |
| [energy](energy/src/lib.rs) | 能量单位、存储、端口与结算 |
| [ui](ui/README.md) | 可换肤即时模式控件、输入捕获、字体与绘制 |

库存模块已实现有界槽位、物品目录、查询与原子转移，说明见[inventory/README.md](inventory/README.md)。配方与能量仍为空库入口。模块通过公开内容、动作与模拟接口注册，世界内核不依赖具体玩法模块。当前由应用装配代码选择启用的模块，独立的游戏定义配置尚未实现。
