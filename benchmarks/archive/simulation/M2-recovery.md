# 恢复场景记录

日期：[20261023]
类型：验收
状态：已验收
环境：Windows 11 x86_64，Rust 1.94.0，debug profile
范围：未记录

## 目标

命令内含断言，快照、日志尾或续号回退时以非零状态退出。

## 方法

### 场景内容

以`WorldAuthority`装配世界，装载三个边长16的分区。提交一个事务后在位置1捕获快照，再提交两个事务作为日志尾。用快照与两条提交变更重建`WorldAuthority`，逐分区比较单元布局，并在恢复后的世界上继续提交。

## 结果

### 输出

```text
snapshot_seq=1
live_seq=3
recovered_seq=3
cells_checked=8193
restored_equal=true
continued_seq=4
```

### 判读

- `snapshot_seq=1`与`live_seq=3`：快照停在位置1，之后两条提交没有进入快照。
- `recovered_seq=3`：快照加日志尾把位置推进到3，与在线世界一致。
- `cells_checked=8193`：两个稠密分区共8192格，加一个未编辑分区的整体表示。
- `continued_seq=4`：恢复后的协调器从3继续，没有重号或跳号。
- 末尾另做一次陈旧读取的提交，确认在恢复世界上仍以冲突拒绝且不改世界。

## 复现

### 运行命令

```sh
cargo run -p torse-tools -- recovery-scenario
```

## 边界

### 未覆盖

- 文件与检查点：本场景在内存中完成，manifest发布与截断留给B3。
- 日志完整性：校验、截断与位翻转由B2的测试覆盖，本场景假设日志尾已被验证。
- 未知内容：恢复拒绝未注册内容，保留原始载荷的只读模式未实现。
- 实体变更：变更集仍只含块写入。
