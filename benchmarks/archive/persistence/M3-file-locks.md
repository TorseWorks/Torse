# 文件锁生命周期回归

日期：[20261024]
类型：验收
状态：已验收
环境：未记录
范围：未记录

## 目标

关联运行：[CI第17次](https://github.com/invefa/Torse/actions/runs/39993343682)。

## 方法

未记录

## 结果

### 故障与修正

Ubuntu的complete_manifest_corruption_never_repairs_or_falls_back在重新打开manifest时报告排他锁占用，尚未进入损坏内容检查。原实现依赖File关闭释放锁，描述符被复制或继承时，锁可能继续由同一底层文件描述持有。

新增确定性回归测试保留try_clone产生的描述符，销毁原FileCheckpointBackend后重新打开。同一测试在修复前的Windows环境复现相同WouldBlock错误；显式unlock后通过。检查点、日志与世界目录分别在所属对象退出时解锁，日志扫描与目录账本校验的失败路径也由解锁守卫覆盖。

测试未增加等待或重试，仍保留单写者排他要求。新持有者取得锁后，即使旧副本关闭，也继续拒绝第三方进入。损坏数据校验、CRC32C和持久化顺序保持原语义。

### 验证

```sh
cargo test -p torse-storage -p torse-server --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

上述Windows验证通过。[修复提交4bfe317的CI](https://github.com/invefa/Torse/actions/runs/39995008946)全部通过：Linux与Windows完整检查、Linux连续十轮检查点并发测试，以及macOS实验。该结果覆盖本次软件回归，不构成硬件断电验收。

## 复现

未记录

## 边界

未记录
