# B6持久执行与B7网络验收

日期：[20261024]
类型：验收
状态：已验收
环境：Windows本地，Rust1.93.1，默认features，真实文件系统与回环TCP
范围：未记录

## 目标

范围是有限世界与同步日志的正确性，非性能承诺。

## 方法

### 入口

```sh
cargo test -p torse-server --test execution --locked
cargo test -p torse-net --test transports --locked
cargo test --workspace --all-targets --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

RUSTDOCFLAGS=-D warnings下运行`cargo doc --workspace --no-deps --locked`。

## 结果

新增17项server测试入口（含1项子进程辅助）与3项net测试，全工作区420项通过。格式、Clippy和Rustdoc通过。源码在apps/server/src/execution.rs、network.rs、crates/sim/src/durable.rs、crates/codec/src/wire.rs及crates/net/src下。

| 验证 | 实际结果 |
|---|---|
| 请求与结果原子性 | 真实库存转移后Receipt与两端库存共同恢复；同身份重发不再执行，不同参数返回DuplicateMismatch |
| 部分阶段恢复 | P3事件输出冻结后直接关闭，再恢复；新进程规则回调计数为0，执行剩余任务后只得到一次结果 |
| 定时事件 | 未来事件及身份水位进入快照；确认和新唤醒共同记录，关闭不执行未来事件 |
| 世界发布后、日志accept前进程退出 | 子进程exit(73)；恢复到冻结阶段，重新结算一次库存转移，没有重复扣料 |
| 持久屏障后、结果释放前进程退出 | 子进程exit(74)；恢复直接取得原结果，不重跑已完成任务 |
| 预留与屏障故障 | Busy不消费输入和序号；注入flush失败后世界、结果和复制读取门槛全部关闭 |
| 非法领域恢复 | 规则声明改变和伪造阶段进度拒绝；规则拒绝前后日志原字节不变 |
| 预算与编码 | 执行快照往返、所有截断位置、尾随字节、超额输出回滚、巨大集合计数和未知标签拒绝 |
| 内存双客户端 | 授权者得到两端槽位最终值，旁观者没有库存；重复请求返回原结果，through不越过DurableSeq |
| 真实TCP双客户端 | 一次库存转移和一次方块放置后，两个镜像的地形一致，只有授权者收到库存；ReplicaSeq可跳过隐藏提交对应的CommitSeq |
| 撤权 | B侧已排队的私有增量和结果立即清空，连接关闭；重连使用新订阅和当前基线 |
| 非法输入 | 未兼容握手、未经批准范围、无权动作和附加伪造actor字节均拒绝或关闭对应连接 |
| 慢Peer | 仅关闭输出队列超限连接，健康连接继续获得持久结果，世界保持运行 |
| 去重窗口与配额 | 结果窗口淘汰后旧序号返回SequenceExpired；超额输入/待接纳队列不消耗可重试的新序号 |
| 有界关闭 | 当前接纳请求结算后保存；截止范围外请求持久记录Stopping，未来事件保留 |
| TCP边界 | 每poll只读写3字节时仍完整重组200字节帧；超大长度前缀拒绝；内存控制关闭清空未发数据 |

### 可执行程序

```sh
cargo run -p torse-server -- .work/execution-world --listen 127.0.0.1:7777 --ticks 60
cargo run -p torse-server --example headless_client -- 127.0.0.1:7777
```

第一条单独启动服务端，第二条在另一终端连接。--ticks 0表示持续运行，有限Tick完成后关闭并保存。演示使用只读访客策略。

真实程序复跑结果：新目录运行2 Tick，epoch=1、初始tick=0、durable_seq=24；同目录重开再运行2 Tick，epoch=2、初始tick=2、durable_seq=48。另一次60 Tick运行完成到durable_seq=720，独立headless_client取得session=1、epoch=1、2个Section、2个实体、0个可见组件集合。基线序号随连接时点变化，此次为252。

## 复现

未记录

## 边界

没有GPU客户端输入、预测、互联网登录或TLS。固定范围完全驻留，没有运行期脏区装卸。原生规则使用纯规划接口，Mod平台P2的WIT唤醒派发仍需适配此持久事件接口。

原WorldHost工具继续支持旧本地格式，新增执行世界使用独立执行载荷版本1，旧数据不会自动迁移。完整RuntimeState随每条记录替换，空Tick产生12条同步记录；单文件日志继续保留，未做压缩和吞吐优化。进程退出测试不等同于硬件断电保证。本轮未运行远端CI，已把主程序smoke加入现有Linux/Windows工作流。
