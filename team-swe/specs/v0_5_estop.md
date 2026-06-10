# Spec · v0.5 候选 D — `EStop` 触发 RPC（round 1）

> CodeMap：[`team-swe/codemap/v0_5_estop.md`](../codemap/v0_5_estop.md)（round 1
> amend `f52ccfe` reviewer ACK）。本 Spec round 1 按 reviewer 指示**先拍
> Q1/Q2/Q8 三联动**（决定轻量 publish 路径 or S0 回执路径），再 D4-D8 + AC +
> hard boundaries。**0 代码改动**。

## What

实现 design §5.3 已冻结未 land 的 `rpc EStop (EStopCommand) returns (Ack)`
（`00_design.md:909/:939`：`EStopCommand{ CommandMeta meta; string estop_reason }`，
**无 confirmation** = 安全方向自洽）。proto method **9 → 10 additive**；
**operator+**（`Capability::EStop` 已在 enum，rpc_roles.toml 已映射）；触发
operator+ / 清除 maintainer+ 的分权模型不变。

## 影响范围

- **In**：proto +1 method；gateway `estop` handler + 常驻 EStopEvent publisher
  （专用线程，详 D1 实现形态）；`Method::EStop` idempotency；e2e。
- **Out（hard boundaries 镜像）**：S0 **0 改**（D1 路由 (a) 下消费链全既有）；
  robot-cli 0 改（robotctl `latched=true` 债务仅登记，D3）；event.fbs **wire
  字段 0 改**（仅注释澄清走 CHANGELOG）；ClearEStop / SetEnabled / 既有 9 method
  0 改；design.md 0 改。

## 关键设计决策

### D1（Q1 路由）⚠ **拍板**：(a) gateway **publish estop topic**（design §6.6 字面）

- **采纳 CodeMap §5 分析**：ClearEStop 走 RR 因它是**危险方向**（解锁，需
  TorqueOffConfirmed gate + confirmation + 强 Ack）；EStop 是**安全方向**（上锁，
  无 gate / 无确认 / 任何时刻执行都对）——机制不必对称。design §6.6 字面
  （`RPC EStop → gateway → iceoryx2 /events/estop → S0 latch SAFE + 写磁盘`）
  即此路由；**entry §2.4 "对称 control plane" 表述按 CHANGELOG 勘误**（连同
  entry:143 schema id "0x0405"→`0x0000_0300` 笔误一起登记）。
- **后果（全部入 AC/宣称）**：
  1. **S0 0 改**——poll_estop → emergency drain → safety_stop + latch fsync
     primary-first 链全既有（CodeMap §2）。
  2. **无单飞 / 无 RR deadline 机制**——重复 EStop 无害（latch 单调 true），
     **emergency 绝不被 "AlreadyInProgress" 拒**（RR 路由的反直觉点，CodeMap §5
     表）。
  3. **S0 不在时 RPC 仍 Ack**（publish 到 topic 不依赖订阅者存活）——明文宣称：
     **EStop RPC 不是 S0 死亡场景的救命索**，该场景由硬件 fail-safe 兜底
     （§6.7 #1）；AC-7 用 e2e 锁这个诚实语义。
- **实现形态**（Plan 期细化，此处定骨架）：iceoryx2 0.8 `ipc::Service` 家族
  SingleThreaded policy → publisher handle **`!Send`**（`ipc_thread.rs:18-22`
  模块 doc 同族结论）→ gateway 起**专用 `estop_pub_io_thread`**（镜像
  `rr_io_thread` 形态：handler → crossbeam forward channel → 线程内 publish →
  oneshot 回 publish 成败）。**常驻** publisher（gateway 启动 attach，QoS 4 槽
  预算实测宽裕，CodeMap §2）——不走 robotctl 的 per-call attach（emergency 路径
  不付 attach 延迟）。

### D2（Q2 Ack 语义）⚠ **拍板**：**弱 Ack = published**，`cleared_at_ns` 承载 `published_at_ns`

- `Ack.request_id` = echo；`Ack.cleared_at_ns` = **published_at_ns**（estop_pub
  线程 publish 成功时刻，gateway `RobotClock` mono raw）；`Ack.dry_run` =
  false 恒定（D4 拒 dry_run）。
- **明文非保证**（Spec/AC/proto 注释三处宣称）：Ack **不**证 S0 已消费、**不**证
  latch 已 fsync；`RobotState` 无 `estop_latched` 字段 → **latch fsync 远程不可
  观测**（CodeMap B2）。远程客户端可观测面 = `GetState` **运行态**
  （mode=Safe / safety_state ∈ {StopCommanded, TorqueOffConfirmed}）。
- **(c) observed_safe_at_ns（中档）被否的理由**：仍不证 fsync（不解决 B2 指出的
  鸿沟）+ gateway 确认循环让 emergency 回包变慢 + 失败面增（观测超时算什么？）。
  **(b) latched_at_ns（强）被否的理由**：要 S0 回执路径 = 放弃 D1 全部收益，且
  EStop 的安全性**不**依赖 Ack 强度（§6.7 #1/#4 已由既有链保证）。若 reviewer
  否 D1/D2，回退即 (b) 整套（entry 5-8 commit 口径）。

### D3（Q8 latched 填值）⚠ **拍板**：(i) **`false`** + fbs 注释澄清 + robotctl 债务登记

- gateway RPC 路径 `EStopEvent.latched = false` = "触发请求，**未**确认 latch"
  ——publish 时刻 fsync 不可知，填 true 即 wire 上说尚未发生的事实（CodeMap B1）。
- **消费面实测无破坏**（CodeMap Q8 census）：生产订阅者只有 s0_daemon，
  `poll_estop` 不读该字段；`.latched()` 读者全在测试侧且绑 robotctl 直连路径。
- **fbs 注释澄清**（event.fbs:9 注释从 "S0 已经把 latch 持久化到磁盘" 澄清为
  "S0 侧确认语义；producer 在 publish 时刻不可知时**必须**填 false"）——
  **comment-only，wire 字段 0 改**，走 CHANGELOG 登记（frozen schema 纪律）。
- **robotctl `latched=true` 债务**：CHANGELOG 登记（含 `source="robotctl"` 不在
  §3.5 三枚举内）；**修复不捆绑候选 D**（改 robotctl 行为 + robotctl_smoke
  断言 = 跨 crate scope 蔓延；独立 follow-up）。⚠ 若 reviewer 要求候选 D 内
  顺手修，请在 round 1 回复中明示（会 +1 commit + robotctl_smoke 改动）。

### D4（Q3 dry_run）拍板：**拒** `InvalidArgument`

- EStop 无 precondition gate，dry-run 无可评估物；"would-publish" 无信息量；
  静默吞 = 候选 A "假 dry-run" 洞。`meta.dry_run=true` → 即拒
  `InvalidArgument("EStop does not support dry_run")`，**不** publish、**不**入
  idempotency cache（validate 步即拒，先于 Step 5）。

### D5（Q4 idempotency）拍板：挂统一骨架 `Method::EStop`

- `Method::EStop`（"estop"）+ `CachedResponse::EStop { published_at_ns }`
  （无 dry_run 字段——D4 恒拒，cache 形态不留死字段）。同 key replay 返缓存 Ack
  **不**重复 publish（§6.7 #5 字面满足）；不同 key 重复 EStop = 两次 publish
  全合法（latch 单调，无副作用差异）。fingerprint 字段：source / estop_reason /
  audit_reason / auth_token（沿 set_enabled 分类法；request_id / timeout_ms
  不进）。
- **fingerprint 不含 dry_run**（D4 在 validate 即拒，dry_run=true 永不到
  fingerprint 步）。

### D6（Q5 wire 映射）拍板：`source="rpc"` 固定 + `reason` 透传

- `EStopEvent.reason = EStopCommand.estop_reason`（空串合法——按钮也没 reason；
  **不**强制非空，区别 ClearEStop confirmation）；`source = "rpc"` 固定
  （event.fbs:13 三枚举字面；`meta.source` 是调用方身份，进 gateway 审计 log
  不进 wire source）；`stamp_ns` = gateway clock now（informational，S0 不校验）；
  `latched = false`（D3）。
- envelope（`set_seq` / `set_source_id`）：seq = gateway 内单调计数；source_id
  预留值 Plan 期定（robotctl 用 99；gateway 用独立值避免混淆审计）。

### D7（Q6 robotctl estop-rpc 子命令）拍板：**推后**

- 候选 D 不捆绑 CLI；既有 `robotctl estop` 直连覆盖本地紧急；gateway 路径的
  CLI 入口留候选 E 前 operator workflow 沉淀时一并评估。

### D8（Q7 e2e 拓扑）拍板：沿 `set_enabled_e2e` harness，观测面分两层

- **远程层** = `GetState` 运行态（mode / safety_state）；**latch 文件 "1" 断言 =
  e2e harness 本地读文件**（远程客户端做不到，B2 收窄口径）。
- case 集（详 AC）：Safe 态 EStop（最小）+ Enabled 态 EStop（heartbeat 源，证
  压倒）+ viewer 拒 / operator 准 + S0-dead 仍 Ack（D1 后果 3 诚实语义锁）+
  idempotency replay + dry_run 拒。

## Acceptance Criteria（round 1）

- **AC-1（additive）**：proto append `EStop`（method 9→10）；既有 9 method
  diff=0；既有 22 e2e（task 6 + state 6 + set_enabled 10）+ 4 smoke 0 退化；
  fbs 0 新 schema + schema_lint 过。
- **AC-2（Safe 态 happy）**：operator `EStop` → Ack（`cleared_at_ns` =
  published_at_ns > 0，`dry_run=false`）→（harness 观测）S0 mode=SAFE +
  safety_state ∈ {StopCommanded, TorqueOffConfirmed} + **latch 文件 "1"
  （harness 本地读）** + estop latch 后续 enable 被拒（LatchBlocked，与候选 C
  AC-4 闭环）。
- **AC-3（Enabled 态压倒）**：heartbeat 源 + auto-enable 到 ENABLED →
  `EStop` → mode=SAFE（Enabled→Safe Emergency 压倒）+ latch "1" + torque-off
  （safety_state）。
- **AC-4（分权）**：viewer → PermissionDenied；**operator+ 全准**（operator /
  maintainer / admin —— rpc_roles.toml 三者都有 `estop`；对照 ClearEStop
  maintainer-only）。
- **AC-5（幂等，§6.7 #5）**：同 `idempotency_key` 二发 → 返缓存 Ack（同
  published_at_ns），**不**重复 publish（e2e 以 estop topic 订阅计数或 S0
  metrics 证一次）；不同 key 二发 → 两次都 Ack。
- **AC-6（dry_run 拒，D4）**：`dry_run=true` → `InvalidArgument`，**不**
  publish、状态 0 变化、cache 0 写入。
- **AC-7（S0-dead 诚实语义，D1 后果 3）**：kill s0 后 `EStop` → **仍 Ack**
  （published 语义）；Spec/proto 注释明文该场景硬件 fail-safe 兜底（§6.7 #1）。
- **AC-8（wire 字段，D3/D6）**：harness 订阅 estop topic 验
  `EStopEvent{ reason=estop_reason, source="rpc", latched=false }`。
- **AC-9（latched 语义文档，D3）**：event.fbs 注释澄清 + CHANGELOG 登记
  （robotctl true 债务 + entry 两处勘误一并）。
- **AC-Q**：fmt + clippy(-D warnings) + `cargo test --workspace`（release
  rebuild 在前，RS2 流程）+ 4 smoke + cargo deny baseline + schema_lint。

## hard boundaries（实施期不破）

1. §6.7 全保（#3 强化：新增显式审计 latch 源；清除仍唯一 ClearEStop）。
2. 既有 9 method wire 0 改（第 10 additive only）。
3. **S0 0 改**（D1 路由收益；若实施期发现必须改 S0 = 触发 Reverse Sync 重拍 D1）。
4. robot-cli 0 改（D3 债务仅登记）。
5. event.fbs wire 字段 0 改（注释澄清 only，走 CHANGELOG）。
6. ClearEStop / SetEnabled handler + S0 rr_server 0 改。
7. design.md 0 改（entry 勘误走 CHANGELOG）。

## 后续（RIPER）

reviewer round 1 复核 ✅ ACK（0 阻断；2 非阻断钉进 Plan §P.3 PD1/PD2 + AC-4
文案修正已落）→ Plan outline（下）→ Plan round → 实施。

---

# Plan（outline draft v1）

> Spec round 1 ACK（`a278529`）后授权。3 feat/docs commit 口径（D1 路由 (a)
> 砍掉 S0 段）。reviewer 2 个非阻断已钉 §P.3 PD1/PD2。

## Plan round 1 amend 记录（reviewer 复核后）

3 个可执行性阻断，已回修：

| # | 阻断 | 修订 |
|---|---|---|
| PB1 | **gateway ready ≠ estop publisher ready**：原 Plan 写 estop_pub 在 6 rr_io **之后** spawn 且 "ready marker 不动"——既有 e2e 等 `"rr_io_threads started"` marker，无法证明 estop_pub 已 attach（AC-7 顺序失去前置保证）| **选修法 (a)**：estop_pub spawn 移到 marker log 行**之前**（spawn 同步 ready 握手 → marker 打出时 publisher 已 attach）；marker 行文本扩注 estop_pub（count-independent 前缀 `"rr_io_threads started"` 稳定，既有 e2e 兼容）；AC-7 顺序口径改 "gateway ready marker（**蕴含** estop publisher attached）→ kill s0 → EStop"。详 §P.2.1 |
| PB2 | **Full→ResourceExhausted 缺 channel 容量定义**：forward channel 未锁 bounded/容量，实现成 unbounded 则 Full 分支永不可测 | 锁 **`bounded::<EStopPubForward>(64)`**（沿 6 条 rr forward 64 先例）+ **新增 unit** `estop_forward_channel_full_returns_resource_exhausted`（bounded(1) + 预塞满 + 不消费 → 第二发 ResourceExhausted）；unit 数 9→10。详 §P.2.1 |
| PB3 | **AC-4 e2e 漏 maintainer**：Spec AC-4 = operator/maintainer/admin 全准，Plan case 3 只测 operator+admin | case 3 改名 **`viewer_denied_operator_maintainer_admin_allowed`**，maintainer token 纳入（common/client.rs `MAINTAINER_TOKEN` 现成）。详 §P.2.2 |

## P.0 工作基线

- Spec 定稿 = `a278529`（round 1 ACK，0 阻断）；CodeMap = `f52ccfe`。
- 全程 pinned `nightly-2026-06-08`；每 commit 前 `cargo fmt --all` +
  `cargo clippy --workspace --all-targets -- -D warnings` 零警告。
- **0 新依赖**；**Quality Gate 顺序硬性 = `cargo build --release --workspace`
  在前 → workspace test → 4 smoke**（候选 C RS2 流程教训）。

## P.1 commit 节奏（2 feat + 1 docs + summary）

| 段 | 内容 | 主要文件 |
|---|---|---|
| (1/3) | **proto + gateway atomic**：proto append `EStop`（9→10）+ `EStopCommand` + `Method::EStop` / `CachedResponse::EStop` + `estop.rs`（validate 含 dry_run 拒 + fingerprint）+ `estop_pub_thread.rs`（常驻 publisher 专用线程）+ gateway handler + 启停 wiring | robot-rpc |
| (2/3) | **cross-process e2e**：`estop_e2e.rs`（s0 + gateway + 按需 heartbeat + **harness estop-topic 订阅者**）6 case 覆盖 AC-2..8 | robot-rpc tests |
| (3/3) | **docs**：event.fbs 注释澄清（comment-only）+ CHANGELOG（实现登记 + entry 两处勘误 + robotctl latched 债务）+ summary + CLAUDE + README | docs + event.fbs 注释 |

## P.2 工作流（executable）

### P.2.1 [commit (1/3)] proto + gateway atomic

- **`robot_control.proto`**：append `rpc EStop (EStopCommand) returns (Ack);`
  （service 末尾，method 9→10）+ `message EStopCommand { CommandMeta meta = 1;
  string estop_reason = 2; }`（design :909 字面）；header 注释 9→10。
- **`idempotency.rs`**：`Method::EStop`（as_str `"estop"`）+
  `CachedResponse::EStop { published_at_ns: u64 }`（**无 dry_run 字段**，D4/D5）。
- **`estop.rs`**（新，沿 `set_enabled.rs`）：`ValidatedEStop { request_id,
  idempotency_key, source, estop_reason, audit_reason, timeout_ms, auth_token }`
  （**无 dry_run 字段**——validate 即拒）+ `EStopValidationError` 沿
  SetEnabled 集合 **+ `DryRunUnsupported`**（`meta.dry_run=true` →
  `InvalidArgument`，D4）+ `validate_estop_command` + `estop_idempotency_
  fingerprint`（source / estop_reason / audit_reason / auth_token 进；
  request_id / timeout_ms 不进；**estop_reason 空串合法**，D6）。
- **`estop_pub_thread.rs`**（新）：`spawn_estop_pub_thread(cfg, clock, factory,
  forward_rx) -> EStopPubThreadHandle`（镜像 `rr_io_thread` spawn/ready/stop
  范式；factory 线程内 `open_pub::<markers::EStopEvent>`〔`!Send`〕+ leak Node）。
  - forward 类型 `EStopPubForward { body_bytes: Vec<u8>, stamp_ns: u64,
    oneshot_tx: oneshot::Sender<Result<u64, EStopPubError>>, request_id: String }`
    （handler build body；线程持 seq 单调计数 + `GATEWAY_ESTOP_SOURCE_ID = 98`
    〔robotctl=99，独立值审计区分〕+ loan/set/publish；成功回
    `Ok(published_at_ns = clock.now())`）。**channel = `bounded::<EStopPubForward>
    (64)`（PB2 锁死，沿 6 条 rr forward 64 先例；禁 unbounded——Full 分支必须
    可测）**。
  - **PD2 失败映射表**（reviewer 钉 2）：
    | 失败点 | 行为 |
    |---|---|
    | 启动 attach | `open_pub` open_or_create——**不依赖 S0 subscriber 存在**；失败 = gateway 启动 fail-fast（沿 rr_io spawn 回滚先例）|
    | forward `try_send` Full | `tonic::ResourceExhausted`（沿 ClearEStop Step 9）|
    | forward Disconnected（线程死）| `tonic::Unavailable`（沿 ClearEStop "RR I/O thread stopped"）|
    | 线程内 loan / publish Err | oneshot 回 `Err` → handler `tonic::Internal`（gateway 内部 publish 基建失败）|
    | oneshot 等待超时（timeout_ms + grace）| `tonic::DeadlineExceeded`（gateway 侧；无 S0 deadline——publish 路径无 wire deadline）|
- **`gateway.rs` handler `estop`**（沿 12 步骨架裁剪）：validate（含 dry_run
  拒）→ auth → authz `Capability::EStop` → fingerprint → cache.decide
  （Replay 返缓存 Ack **不**重 publish）→ shutdown gate → `build_estop_event
  (estop_reason, stamp_ns, "rpc", false)`（**latched=false**，D3）→ forward →
  await oneshot → Ok(published_at_ns) → commit cache + `Ack { request_id,
  cleared_at_ns: published_at_ns, dry_run: false }`；Err 路径按 PD2 表。
  **无 Step 11/11.5**（publish 无 wire response → 无 decode / 无 request_id-
  mismatch 防线——该防线属 RR 回包路径）。
- **`run_gateway_with_rr_io`**（PB1 修法 (a)）：spawn estop_pub thread 在第
  6 条 rr_io 之后、**`"rr_io_threads started"` log 行之前**（spawn 同步 ready
  握手〔sync_channel，沿 rr_io_thread 范式〕→ **marker 打出时 estop publisher
  已 attach**）；marker 行文本扩注（如 "rr_io_threads started (6 RR I/O threads
  + estop_pub; …)"——count-independent 前缀稳定，既有 e2e 兼容）+
  `GatewayHandlerState.estop_pub_tx` + shutdown 停（rr_io 同阶段）；spawn 失败
  回滚先例。
- **unit**（预估 10）：estop.rs 5（happy / dry_run 拒 / 空 estop_reason 合法 /
  fingerprint 排除 request_id+timeout / fingerprint 区分 reason）+ gateway 5
  （viewer 拒 / publish-ok → Ack + cache commit / forward Disconnected →
  Unavailable / **`estop_forward_channel_full_returns_resource_exhausted`
  〔PB2：bounded(1) + 预塞满不消费 → 第二发 ResourceExhausted〕** / 同 key
  replay 返缓存 Ack 不二次 forward〔mock 端 forward_rx 计数〕）。

### P.2.2 [commit (2/3)] cross-process e2e

- **`tests/estop_e2e.rs`**（新，沿 `set_enabled_e2e` env：s0 + gateway + 按需
  FakeHeartbeatPublisher）+ **harness estop-topic 订阅者**（PD1 核心件）：
  - **PD1 可执行判据**（reviewer 钉 1）：每 case 唯一 `robot_id`；订阅者在
    **第一次 RPC 之前**建好（env bring_up 内 attach + ready 确认）；消费用
    **drain 循环逐条 `recv`/`try_recv` 计数**（**禁 `latest()`**——会塌缩多条
    事件造成假阳性）；同 key 两发 → settle window 后**恰好 1 条** EStopEvent；
    不同 key 两发 → **恰好 2 条**。
  - case 集（6）：
    1. `estop_from_safe_happy`（AC-2）：Ack（published_at_ns>0, dry_run=false）
       + mode=SAFE + safety_state ∈ {StopCommanded, TorqueOffConfirmed} +
       **latch 文件 "1"（harness 本地读）** + 后续 SetEnabled(true) →
       FailedPrecondition（LatchBlocked，与候选 C AC-4 闭环）+ **AC-8 wire
       字段断言**（订阅者验 reason / source="rpc" / latched=false）。
    2. `estop_from_enabled_overrides`（AC-3）：heartbeat + auto-enable →
       ENABLED → EStop → SAFE + latch "1"。
    3. `viewer_denied_operator_maintainer_admin_allowed`（AC-4 / PB3）：
       viewer → PermissionDenied；operator → Ack；**maintainer → Ack**；
       admin → Ack（四 token 各自独立 key；operator+ 全准口径）。
    4. `idem_replay_single_publish`（AC-5/PD1）：同 key ×2 → 两 Ack 同
       `published_at_ns` + 订阅者恰好 1 条；换 key 再发 → 恰好 2 条。
    5. `dry_run_rejected`（AC-6）：`dry_run=true` → InvalidArgument + settle
       后订阅者 **0 条** + mode 不变。
    6. `s0_dead_still_acks`（AC-7/PD2/PB1）：**顺序硬性 = gateway ready
       marker（PB1 修法 (a) 后**蕴含** estop publisher 已 attach）→
       `s0.shutdown()`（SIGTERM 退场）→ EStop → 仍 Ack**——证 Ack 只依赖
       publish 语义，不依赖订阅者存活。
- AC-9（docs）在 (3/3)；AC-1 = 既有 22 e2e + 4 smoke 0 退化（AC-Q 统跑）。

### P.2.3 [commit (3/3)] docs 收口

- `event.fbs:9` 注释澄清（comment-only：latched = S0 侧确认语义，producer
  不可知时必须填 false；`//` 注释不进 flatc codegen，wire 0 改）。
- CHANGELOG：候选 D 实现登记 + entry 两处勘误（"对称 control plane" 表述 +
  schema id 0x0405 笔误）+ robotctl `latched=true` / `source="robotctl"` 债务
  登记（follow-up 不捆绑）。
- summary（`docs/handoffs/v0.5_estop_summary.md`）+ CLAUDE 状态块 + handoffs
  README 行。

## P.3 Plan 拍板项

| # | 项 | 拍板 |
|---|---|---|
| PD1 | AC-5 可执行判据（reviewer Spec round 1 钉 1）| 唯一 robot_id + 订阅者先于第一次 RPC + drain 循环计数（禁 latest()）+ 同 key→恰 1 条 / 异 key→恰 2 条 |
| PD2 | estop_pub 失败映射（reviewer Spec round 1 钉 2）| §P.2.1 表：attach 不依赖 S0 / Full→ResourceExhausted（channel bounded(64)，PB2）/ Disconnected→Unavailable / loan·publish Err→Internal / oneshot 超时→DeadlineExceeded；AC-7 顺序 = gateway ready marker（蕴含 publisher attached，PB1）→ kill s0 → RPC |
| PD3 | AC-4 文案 | "operator+ 全准"（operator/maintainer/admin），Spec 文本已修 |
| PD4 | gateway envelope source_id | `GATEWAY_ESTOP_SOURCE_ID = 98`（robotctl=99 区分）|

## P.4 Plan-Execute diff 登记（实施期填）

（占位：沿候选 C §P.4 体例。）
