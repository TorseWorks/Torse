# 可换肤游戏UI库

`torse-ui`是可选的界面模块，不依赖客户端、世界或模拟权威。宿主声明当帧控件、传入输入、处理返回事件，再绘制当前状态。控件只报告交互，不直接修改玩法数据。

## 装配

默认启用`gpu`特性，提供WGPU叠加层。关闭默认特性时仍可使用输入捕获、贴图、字体和CPU画布，也可以把画布交给其他渲染器。

```toml
torse-ui = { path = "modules/ui", default-features = false }
```

| 入口 | 用途 |
|---|---|
| `Pointer<K>`、`Control<K>` | 使用宿主稳定键声明按钮和滑动条，处理按下、释放与拖动 |
| `Event<K>` | 激活按钮或返回滑动数值，复选框由宿主翻转对应设置 |
| `Rect` | 统一逻辑矩形和命中范围 |
| `Canvas::try_new` | 分配有界画布，可接入共享预算 |
| `Canvas::clear` | 清零已有画布，复用像素存储及对应预算 |
| `Sprite::decode` | 按预先声明的尺寸解码图片 |
| `Text::from_bytes` | 使用宿主提供的字体，不要求系统字体 |
| `Text::load` | 使用平台字体或`TORSE_UI_FONT`指定的字体 |
| `Theme` | 根据PNG图集和区域描述绘制窗口、控件、槽位及提示 |
| `Overlay` | 将画布叠加到已有WGPU目标，需启用`gpu` |

控件键在同一组声明中保持唯一，布局变化后重新声明即可。按钮要求按下与释放命中同一目标，滑动条拖出矩形仍保留捕获并约束数值。失焦、切换模态或销毁界面时调用`Pointer::cancel`；被拖动控件从声明中消失也会取消。宿主将物理坐标转换成与画布一致的逻辑坐标后再传入。

## 贴图与默认皮肤

内置紧凑深色和蓝色面板两种皮肤，均为IMGUI风格，包括窗口、分组、按钮三态、字段、槽位、进度、复选框、滑块、关闭图标和准星。蓝色面板使用黑灰底、蓝色标题带及控件。PNG与TOML描述位于[assets](assets)，全部像素由仓内脚本原创生成，采用本仓库GPL-3.0-only许可。重建命令：

```sh
python modules/ui/assets/make-skin.py
```

`Theme::load_default()`加载紧凑深色，`Theme::load_blue()`加载蓝色面板。`Theme::from_png`接受PNG字节、TOML区域描述和宿主资源标识。18个基础角色各声明一次，可额外声明一个`title`角色绘制标题带；旧描述无需补充该角色，调用时不绘制。区域可以重新排布，九宫格边距保留边角并拉伸中部。文字不在图集中，便于本地化和自定义字体。[20261029]

公开枚举`Part`新增`Title`变体，宿主若对该枚举做完整匹配，需要补充分支；旧18角色贴图描述保持兼容。

`Theme::load_dir`读取目录中的`default.png`与`default.toml`。客户端在设置的“界面”页选择内置皮肤并保存偏好，设置`TORSE_UI_SKIN`后启动时优先加载该目录，也可在游戏内切回内置皮肤；目录文件命名保留为这两个名称，图像和区域内容均可替换：

```powershell
$env:TORSE_UI_SKIN = ".work/my-ui"
cargo run --release -p torse-client -- --window
```

非法皮肤在构建时返回错误，不以部分角色覆盖已有皮肤。客户端替换前完成解码和预算申请，失败保留当前主题和选择；启动时没有可用旧主题则回到内置深色并显示原因。环境变量每次启动优先于保存值，移除后使用保存的选择。

## 预算与生命周期

皮肤源PNG最多8MiB，区域描述最多32KiB，图像边长最多1024。解码前校验声明尺寸和图片尺寸，拒绝重复、缺失、溢出及越界区域。字体源最多32MiB，画布最多64MiB；较大画布和压力下分配使用`Canvas::try_new`处理失败。

皮肤和图标的CPU像素通过资源目录持有，`attach_memory`接入宿主共享预算；字体与字形缓存也可接入，字形缓存有数量限制并可清理。带预算的画布在分配前申请费用，整个持有期间保留计费，销毁时归还。客户端复用相同物理尺寸和缩放的画布，每次重绘先清零；尺寸变化时先释放旧画布，再申请新画布。新画布申请失败时保留上一份GPU图像，释放字形缓存并在后续重试。WGPU设备及目标由宿主管理，GPU叠加纹理不计入地形几何载荷限额。

## 验证

```sh
cargo test -p torse-ui --locked
cargo test -p torse-ui --no-default-features --locked
cargo test -p torse-client --features gpu-tests --locked
cargo run -p torse-ui --no-default-features --example widgets -- .work/ui-preview.png
cargo run -p torse-ui --no-default-features --example widgets -- .work/ui-blue.png blue
```

回归覆盖九宫格边角、缩放、匹配释放、拖动与取消、自定义贴图生效、非法区域、预算拒绝及生命周期归零。客户端保留场景装配与具体界面内容，不保留私有画布、字体、GPU叠加层或控件捕获实现。
