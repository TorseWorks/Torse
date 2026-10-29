# 原子操作工具链兼容

日期：[20261027]
类型：验收
状态：已验收
环境：未记录
范围：未记录

## 目标

未记录

## 方法

未记录

## 结果

### 说明

关联：GitHub#14。

光照缓存的实例身份分配改用compare_exchange_weak循环，移除新工具链报告弃用的fetch_update调用。使用Relaxed顺序，只保证唯一身份分配，不承担数据发布同步。达到u64上限时拒绝，不回绕，不改计数器。

新增并发分配与耗尽两项测试：8个线程各分配100个身份，结果严格为1至800，无重复；末尾可用身份分配后，重复申请均拒绝且计数器不变。原8项光照回归继续通过。

固定Rust1.93.1与本机stable的Rust1.98.1均通过28项测试和Clippy。Rust1.99.1安装的分发端连接超时，未完成该版本实跑；实现使用两套已验证工具链均支持的稳定比较交换API。内存身份语义、持久格式及网络格式不变。

## 复现

```sh
cargo test -p torse-world-services --all-targets --locked
cargo clippy -p torse-world-services --all-targets --locked -- -D warnings
cargo +stable test -p torse-world-services --all-targets --locked
cargo +stable clippy -p torse-world-services --all-targets --locked -- -D warnings
```

## 边界

未记录
