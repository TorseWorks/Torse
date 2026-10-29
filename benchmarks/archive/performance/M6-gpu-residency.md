# GPU几何驻留与视锥剔除

日期：[20261028]
类型：测量
状态：已验收
环境：未记录
范围：未记录

## 目标

未记录

## 方法

未记录

## 结果

### 范围

地形及实体顶点、索引缓冲共用独立逻辑预算，默认64MiB，最多4096条目。按与相机中心分区的距离保留近处几何，实体槽固定。拒绝替换时保留旧缓冲，窗口保留CPU网格及待上传修改，镜头返回后重新请求缺失条目。

绘制前对实际世界空间顶点包围盒做六面保守裁剪，透明面继续在所有可见条目中统一按深度排序。上传选择仍受每帧计数和8MiB载荷硬限约束。

### 验证

工作区894项测试通过，1项已有行计时默认忽略；整工作区Clippy以-D warnings通过，格式检查通过。两项实际GPU像素测试在NVIDIA GeForce RTX 4060 Laptop GPU的Vulkan后端通过。

Vulkan实际GPU测试将预算限制为两份四边面载荷，接纳近处和远处缓冲后上传较近条目，验证远处淘汰。更远条目、非法索引和超大替换被拒绝，旧缓冲保持。中心改变后重传被淘汰条目，实体槽保持固定，全部释放后逻辑载荷和CPU收费均为0。

实际帧包含一个视锥内白色四边面和一个视锥外四边面，绘制统计分别为1项可见、1项剔除；中心像素亮度大于240。原有透明交换顺序、亮度及像素测试通过。CPU测试另覆盖六个裁剪面、近裁面穿越、包围相机、非有限变换和暂缓修改不丢失。

窗口以1MiB几何上限运行241帧，4次流送，222次网格重建及199次上传。退出时GPU载荷1031856字节，8条目，累计淘汰4次；最后一帧4项可见、4项剔除。CPU逻辑峰值145138269字节。离屏640x400帧含36分区及4213面，载荷1247048字节。详细数据见[指标](../data/gpu-residency/01-budgeted/metrics.csv)。

## 复现

```sh
cargo test -p torse-client --features gpu-tests gpu_ -- --nocapture
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo run -p torse-client -- --capture .work/gpu-capture.png --width 640 --height 400 --radius 1
cargo run -p torse-client -- --window --width 640 --height 400 --radius 2 --frames 240 --autoplay --gpu-memory-mib 1
```

--gpu-memory-mib控制逻辑载荷，支持1至4096MiB；--memory-mib仍控制共享CPU收费。离屏一次上传的完整几何超过预算时返回错误。

## 边界

预算是已发布顶点和索引载荷之和，包含实体槽。GPU分配器、纹理、UI、驱动、替换期间新旧缓冲重叠和在途命令引用均不在此逻辑计账内；真实释放由wgpu所有权管理，统计不代表实际VRAM。

透明排序保持四边面中心深度规则，不拆分相交几何。过小预算可导致远处几何持续等待；只有当前可见且可能接纳的条目会展开顶点。无存档或协议版本变化。
