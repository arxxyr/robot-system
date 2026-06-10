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
- **AC-4（分权）**：viewer → PermissionDenied；operator **和** maintainer →
  准（触发 = operator+；对照 ClearEStop maintainer-only）。
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

reviewer round 1 复核 → （若有阻断）round 2 → ACK 进 Plan outline（commit 拆分
预估：(1/3) proto + gateway handler + estop_pub_io_thread atomic；(2/3) e2e；
(3/3) summary + CHANGELOG/CLAUDE/README——D1 路由 (a) 砍掉 S0 段后 3 commit
口径；Plan 期再校）。
