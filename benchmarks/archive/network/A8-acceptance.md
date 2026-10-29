# A8联网编辑验收记录

日期：[20261024]
类型：验收
状态：已验收
环境：Windows 11 x86_64，Rust 1.93.1，debug profile，Vulkan 后端（Intel RaptorLake 核显），本机TCP回环
范围：未记录

## 目标

对照既有A8验收条目，四条均有实跑证据；验收语义由范围授权契约固定。

## 方法

未记录

## 结果

### 一、有权限玩家编辑后两个客户端都更新

编辑者与旁观者同时连接同一服务端（旁观者先连1秒）：

```text
旁观者: session=1 batches=12 rebuilds=13 uploads=12 vertices=4528
编辑者: session=2 batches=12 rebuilds=13 uploads=12 vertices=4528 requests_sent=27 requests_rejected=15
```

编辑者12个提交产生的批次，两个客户端逐一收到并同样重建、同样顶点数。首次尝试中旁观者晚于全部编辑连接，其基线直接包含编辑结果（基线顶点4536），同样证明一致性；正式取证采用重叠运行。

### 二、访客与越界请求被拒绝

```text
# 访客（无--editors），自动演示25个请求（对照记录M5-net-client.md）
requests_sent=25 requests_rejected=22 batches=0
# 访客单发越界探针
requests_sent=1 requests_rejected=1 batches=0 vertices=4528
# 编辑者（有编辑权限）单发越界探针，目标(200,1,1)在批准分区之外
requests_sent=1 requests_rejected=1 batches=0 vertices=4528
```

编辑者的越界拒绝证明分区边界本身生效，与权限无关。领域层的边界语义另有5项sim测试（`crates/sim/tests/section_grants.rs`）。

### 三、拒绝不修改世界或镜像

上述全部拒绝运行中，两个世界的镜像批次均为零、顶点数保持基线值；服务端`--fill`地面未被改动。编辑者会话中被拒的15个请求（业务冲突）也只占序号不产生批次。

### 四、撤权使旧订阅失效，重连取得新授权基线

```text
服务端: revoked_sessions=1
被撤客户端: client failed: connection failed: InvalidState: transport closed
重连: session=2 baseline_sections=2 vertices=4520 batches=0
```

撤权在锚定Tick触发，旧连接的传输被服务端关闭，客户端以连接终止退出；重连注册新会话编号2并取得新基线，基线包含被撤客户端此前的提交（顶点4520对4528基线的差异即其净编辑），旧会话编号与订阅不复用。

### 数量预算

ActorContext分区授权上限`MAX_ACTOR_SECTIONS=1024`，会话注册时由执行宿主拒绝超限actor（resource_limit）；该预算与复制订阅的`replicas.max_sections`相互独立。上限按全高驻留世界取值：16格边长分区、24层垂直范围、5×5列共600个授权，原值64只够半径0的单列世界。构造器保持不可失败，超限仅在注册时生效，1项sim测试覆盖。

## 复现

### 运行命令

```sh
# 双客户端与撤权各用独立新世界；客户端命令见各节
torse-server <dir> --listen 127.0.0.1:7806 --fill --editors
torse-server <dir> --listen 127.0.0.1:7810 --fill --editors --revoke-after 30
torse-client --window --connect <addr> [--autoplay] [--probe-outside] [--frames N]
```

`--probe-outside`在连接后发送一个目标为`(200,1,1)`的破坏请求，远在批准scope之外；`--revoke-after N`以首个连接为锚点，N个Tick后撤销全部会话并关闭传输。

## 边界

### 未覆盖

- `--editors`为演示策略，所有连接同一编辑者身份；真实认证与按玩家授权待应用层。
- 撤权演示撤销全部会话；按会话选择性撤权已有`hub.revoke(peer)`入口，未在本场景展开。
- CI不执行窗口与网络运行；结果需本机复跑。
