# 用户认证与连接策略

日期：[20261028]
类型：验收
状态：已验收
环境：未记录
范围：未记录

## 目标

双向TLS先验证双方证书，服务端再按客户端叶证书的SHA-256指纹选择用户、动作权限、复制范围和玩家。Hello与普通请求不携带可自选的用户身份。协议保持3，存档及执行记录没有格式变化。

## 方法

未记录

## 结果

### 身份与会话

CertificateUsers实现可替换的IdentityVerifier，原生宿主可接入其它已验证凭据来源。验证器及TransportSecurity均属于受信适配器边界，客户端消息不能制造该状态。内置传输只有在TLS握手、证书链、有效期、服务端名称及应用协议校验完成后才报告MutualTls。

每个用户同时只允许一个连接，重复登录拒绝后来者。玩家由用户配置指定，不能由连接顺序或输入消息选择；重复用户、证书和玩家分配会被配置校验拒绝。断线撤销父会话、移动子会话和订阅，重新连接得到新会话。旧连接的输入和密文不能在新会话重放，普通请求仍保留原有持久去重及速率限制。

证书有效期在每次完整握手时校验。配置关闭TLS恢复和早期数据，不使用旧恢复状态跳过新连接的凭据验证。已建立连接的可信撤权通过NetworkHub.revoke或replace_policy执行；修改用户配置文件需要重新加载宿主，当前程序不提供账户注册、密码登录或证书在线签发。

未完成认证的连接15秒后关闭，已建立会话默认60秒无应用消息关闭，使用单调时钟，不依赖模拟Tick能否继续推进。空闲上限至少容纳三个心跳周期，客户端仍每10秒发送Ping。字节、队列、每次轮询和请求速率上限继续生效，TLS有界缓冲也接入共享预留。

### 主程序配置

未配置认证时，服务端及图形客户端的明文演示仅允许回环地址。对外监听必须提供TLS和用户配置，不能同时使用全局演示编辑或机器操作授权。任一凭据文件缺失、解析失败或验证失败均关闭对应入口，没有明文回退。

以下工具只生成本地验收材料，目录必须不存在，私钥不入库：

```sh
cargo run -p torse-net --example tls_fixture --locked -- .work/auth-keys
```

将输出的alice_certificate_sha256填入自行创建的`.work/users.toml`：

```toml
[[users]]
id = "user:alice"
certificate_sha256 = "REPLACE_WITH_64_HEX_DIGITS"
permissions = ["world:permission/edit"]
sections = [[0, 0, 0], [1, 0, 0]]
player = 3
```

用户最多64项。权限、实体与分区列表分别最多32、64、64项，实际分区数还受宿主复制上限约束；当前演示上限4。`entities`可显式授予实体目标，省略权限表示只读。玩家必须已由宿主创建、驻留在批准范围内并具有运动状态与碰撞体。配置文件最多64KiB，单个PEM文件最多1MiB。

```sh
cargo run -p torse-server -- .work/auth-world --listen 127.0.0.1:40124 --fill --players 1 --ticks 180 --tls-cert .work/auth-keys/server.pem --tls-key .work/auth-keys/server.key --client-ca .work/auth-keys/ca.pem --users .work/users.toml
cargo run -p torse-client -- --window --connect 127.0.0.1:40124 --walk --autoplay --frames 180 --tls-ca .work/auth-keys/ca.pem --tls-cert .work/auth-keys/alice.pem --tls-key .work/auth-keys/alice.key --server-name localhost
```

两条命令在不同终端运行。客户端可重复传`--section X,Y,Z`选择用户获准的子集，省略时仍请求演示的两个分区。远程部署由管理员提供实际证书、信任根及用户配置，服务端名称须与证书一致。TLS由[rustls连接接口](https://docs.rs/rustls/latest/rustls/enum.Connection.html)和[客户端证书验证器](https://docs.rs/rustls/latest/rustls/server/struct.WebPkiClientVerifier.html)实现。

### 验证

工作区827项、服务端mods配置197项测试通过，零失败、零忽略。两名用户同时通过真实双向TLS连接图形窗口，各运行241帧和24个行走步，均收到36个复制批次。编辑用户发送3个请求，最终画面显示第3项已提交；只读用户发送11个请求，退出前收到10个拒绝，最后一项尚未确认。服务端完成240个Tick并有界关闭，持久序号达到2949。

两个客户端的共享逻辑预留峰值分别320163228及320163176字节，服务端为389062864字节，均低于各自512MiB预算。公开记录保留汇总，临时证书、私钥、世界和截图不发布。

专项测试覆盖分片收发、共享预留关闭释放、错误名称、错误私钥、过期双方证书、不受信客户端、匿名客户端、明文误接TLS端口、密文重复记录、用户权限及订阅隔离、重复登录、未验证凭据、重连旧会话及无Tick进度时的认证超时。

```sh
cargo test -p torse-net --all-targets --locked
cargo test -p torse-server --test authentication --locked
cargo test --workspace --all-targets --locked
cargo test -p torse-server --features mods --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

工作负载、环境及实际程序汇总见[原始记录](../data/user-authentication/01-mutual-tls)。功能及拒绝路径验证不等同于互联网规模、外部渗透或证书运维体系的验收。

本机首次链接完整调试类型的TLS与Wasm组合时出现MSVC的LNK1318、PDB LIMIT(12)。开发配置改为`debug = "limited"`，测试继承该设置，断言和溢出检查仍启用，Release配置不变。该选项保留模块及源码调试位置，省略类型和局部变量级信息，定义见[Cargo调试信息配置](https://doc.rust-lang.org/cargo/reference/profiles.html#debug)。随后按默认命令重新构建和验证。

## 复现

未记录

## 边界

未记录
