# 运行时资源加载验证

日期：[20261028]
类型：验收
状态：已验收
环境：未记录
范围：未记录

## 目标

未记录

## 方法

未记录

## 结果

### 范围

CookedResources消费版本2离线表，校验外部PNG后驻留RGBA。状态、材质及语言直接查询表内行；派生CPU资源沿用AssetCatalog的代际及依赖保护。无窗口示例核对原生注册表，不读取作者TOML和语言文本。

默认32MiB预算成功消费2387字节的表，读取4张PNG，共驻留14592字节RGBA。查询获得8个状态、3个材质及两种语言，英语消息样本为Stone。驻留计账33125字节，峰值16809319字节，释放后为0。1MiB预算在源读取前拒绝Codec预留。

工作区855项测试通过，无失败和忽略。资源测试覆盖共享依赖、自引用与后加回边拒绝、重复GUID、卸载重装、陈旧句柄、源摘要失败、第二张图片失败整体回滚、分类预算及超大尺寸预检。内容和生产器测试继续覆盖确定性与权威、呈现摘要边界。格式和工作区Clippy检查通过。

## 复现

在仓库根目录执行：

```sh
cargo run -p torse-tools --locked -- cook-base games/base/assets .work/base.tcook
cargo run -p torse-tools --locked -- run-cooked .work/base.tcook games/base/assets
cargo run -p torse-tools --locked -- run-cooked .work/base.tcook games/base/assets --memory-mib 1
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

第三条预期失败。输出及环境见[data/cooked-runtime/01-budgeted](../data/cooked-runtime/01-budgeted/metrics.csv)。

## 边界

收费是共享逻辑预算，包含保守表展开和PNG临时空间，不是RSS测量或分配器限制。有界源读取回调由宿主提供，借用的编码表由调用者持有和计账。未加载图片保留表内身份，可显式重装；当前不提供热更新、异步流式或GPU资源管理。

默认图形客户端仍使用嵌入资源。无窗口宿主验证了通用读取和生命周期，运行时表仍只是完整内容注册表的投影。世界存档、网络和离线表版本均未改变。
