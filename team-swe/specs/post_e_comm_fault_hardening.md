# Spec · post-E 通信故障硬化（Spec / Plan Approved；分包实施中）

> **当前执行授权（2026-09-08）**：用户要求“对齐本地文档，然后用一个子agent专门评审，另外一个专门开发，一直推进”。
> 本轮由主 agent 维护文档与整合，developer 编制函数级计划并实施，reviewer 独立评审与验收。
> 先完成内部 Plan 评审再改生产代码；后续按评审结果持续回修。下方 2026-09-05 的
> “仅准备 Plan outline”保留为历史记录，其用户授权限制已由本轮指令更新。
> **Plan Approved（2026-09-08）**：独立 reviewer 已批准
> [正式函数级 Plan](post_e_comm_fault_hardening.PLAN.md)，冻结 hash 与进度见
> [HANDOFF](post_e_comm_fault_hardening.HANDOFF.md)；开发分支 `feat/post-e-comm-fault-hardening`。

> CodeMap：[`team-swe/codemap/post_e_comm_fault_hardening.md`](../codemap/post_e_comm_fault_hardening.md)
> （`bd081cb`，reviewer 五轮 ACK）。入口 = CodeMap §6 **Q1–Q15**，本 Spec 逐条
> 落为 D1–D15 + Spec / Plan / 实施期新发现 S-6..S-40；实现前代码基线 `bd081cb`。
>
> 收口口径承接候选 E：本候选闭合评估 §6.2 **P0-2** 中"连续读写错误阈值 / Fault·
> 断扭语义 / 快速错误也须触发"三件 + Reverse Sync **F1/F2/F3** + CodeMap **S-1 的
> P0 部分**；**RX 重同步与有界串口 reopen 两件因协议无事务身份（S-11）移出本候选**；
> **通信信任 = 显式双状态 marker 的一次性授权模型（两阶段 strict 写 + 持久屏障 +
> 非阻塞独占锁）**（D9）；**不**闭合候选 E 的 D15；**不**宣称无人值守。
>
> **审批状态（2026-09-05）**：reviewer round 21 **Spec Approved**；三项回修已闭合。
> 本次仅批准 Spec，允许准备 Plan outline；**不是 Plan Approved，不授权实施**。
> AC-9(o7) 的非阻断措辞随收口同步，不再开启 Spec 回修轮。
>
> **本文件自包含**：正文 = 最终有效规范全文（API、路由矩阵、phase 表、出口矩阵、
> 错误映射、预算、AC），不引用任何未入库的历史轮次；amend 记录只做摘要，摘要与
> 正文不一致时以正文为准。

## H2 Reverse Sync（2026-09-08，PX-56 验收环境）

S-44：C5 的 H2 工具启动 S0 时未装配 `--serial-fifo` / `--cpu` / `--strict-rt`，
也未核验实际线程调度，不能仅凭 sudo 或 Orin 主机身份宣称 AC-12 的 PX-56 环境。
仅修验收工具：物理 H2 的 S0 普通线程继承 CPU0–3 / SCHED_OTHER，串口线程
FIFO70、左 CPU4 / 右 CPU5；首次恢复与使能前，逐 TID 核实际策略、优先级、精确
亲和性与稳定线程集合，失败留证并走既有独立断扭收尾，不进入采样。工具与 gateway
保持普通调度并核 CPU0–3。软件 mock 路径不增加 Orin 限制；不改变 S0 生产接口。
函数级实现与独立反例见 PLAN P.6.10；两端必要软件门与左 H1/H3 实物记录已独立批准。

用户已确认“现在可以现场配合”，现场 H1–H3 按单臂步骤开展；每臂开始前仍须收到
该臂已支撑、断电重上电及物理 USB 拔插完成的现场事实。H2 再按 D9 显式一次性
授权，不自动恢复信任。此授权不等于实测完成，六项常驻服务保持停止、D15 OPEN。

### S-45：完整事务边界采样与显式 idle 前置条件（软件验收通过）

左 H2 在 `a161a7b` 上首次运行 FAIL：三进程实际 RT 门通过；首组 35 次真实在途
分类失配，随后 epoch 10072 缺同 epoch acquire，严格分析器拒绝，正式样本 0。
原始记录无 dropped 或通信 Fault；`final_stop` 确认状态 3，进程全部退出，marker
为 `1/1`。这是采样接缝问题，不能据此宣称通信故障或 H2 通过。旧物理授权已消费。

control 线程不得在任意时刻直接切换 trace epoch。仅 `test-comm-fault-hook` 启用时，
arm/disarm 先发布带唯一 request_seq 的请求，serial 在真实无 normal lease 的边界
落实并发布独立回执；CommandApplied 只代表 control 已处理，不代表请求被接受或 Ready。disarm 的 epoch=0
仍须绑定新 request_seq，不能复用旧零值或旧 Ready。请求、回执与 deadline 必须一致
可读；重复请求、未完成请求覆盖、错误模式/序号均拒绝，不清 ACTIVE、不补造 acquire。
tool 每轮先 Acquire 读取 CommandApplied，再读取一致边界快照；只有本序号已处理且
随后快照仍无本请求才判 control 拒绝，禁止用旧快照搭配后读的新确认字推导拒绝。

idle 组显式建立“t0 时无在途 IO”的测试前置条件，不宣称自然运行的空闲命中率。
Ready 后有界阻止普通 IO admission；不暂停串口线程，真实急停 claim、故障收敛、
Safety/Confirm/AwaitAck、取消、timeout 退休和 stop 始终可推进。只有真实 EStop
已建立权威保护或窗口到期/disarm 才解除测试门；不能在 TopicReceived 的 t0 先解除，
以免 t0→t1 间又开始 normal IO。epoch 独立保留至 AllOff、IPC 事件采集完毕及边界
disarm。inflight 组在边界 Ready 后照常取得真实 lease 并执行既有 U(1,5)ms 载荷。

边界等待与 idle 窗口各固定 1 秒：前者由 control 首次接收时计算绝对截止，后者由
serial Ready 时计算；轮询/重复请求不得续期。到期显式 FAIL 并解除测试门，不自动
重测、授权、恢复信任或发另一测量 EStop；仍执行既有独立最终收尾。gate 无权修改
safety_word，原语调用方无附加原子写，无 hook 生产调度与冻结设计保持不变。

独立验收覆盖真实 acquire→arm 请求→release 的跨边界反例、Ready 后立即急停、
陈旧回执/重复请求、固定截止到期、取消与超时退休绕过 gate、t0→t1 交错、Fault 下
disarm，以及真实 mock 进程完整采集/恢复/收尾。保留不完整事件拒绝、t0 lease 分类、
慢样本、样本数与原 P99 门；不能以 t1 的 ACTIVE 位替代 t0 分类。函数级范围见
PLAN P.6.11；实现与两端软件验收已独立批准，物理 H2 仍待新边界重测。

## C4 Reverse Sync（2026-09-08，已 admission live RR 的急停保护）

S-41：新调度把 PendingReplies 延后到 RR dequeue 后创建，不能因此丢掉旧实现对
IPC 已接收、尚未执行 live 请求的急停保护。采用 EStop 时冻结 rr lane 前缀、随后
按既有配额与第二道 deadline 取消 live 的方案；dry / source channel / gRPC / control
不扩大取消域。SSF only 继续按 Fault 守门。根 agent 已核基线并按 reviewer 给出的
窄边界回写 D13 与 AC-11(aa)，实现前独立复核；不宣称覆盖所有急停前发送的请求。

S-42：SerialRT timeout 预提交或本 tick 刷新后到达的 mailbox 可使 Fault 与缓存的
健康 guard 同时存在，随后 ClearError 错误恢复。独立 reviewer 已 ACK：两个 Fault
提交点同步清 `adapter_connected` / `adapter_fault_cleared`；持续推导先 Acquire
读 `abort_request` 再读 inhibit，未退场请求不准 ready。只收紧已有守门，不增加
原子或更改 serial 对 inhibit / generation 的单写权；D3 / D9 / D11 / AC-11(ab) 同步。
同类发布接缝补充：正常动作错误先经 serial 登记 Fault / inhibit / mailbox，再
Release 发布错误完成，不能让 IPC 先收到 Err 却读到未登记故障的健康原子；
PreemptedByEmergency 保持无 Fault。取消响应在普通 lane 继续前只终结原 RR owner，
后续 admission 新请求不继承旧取消结果。

S-43：C4 总装曾忽略 auto-enable 的 `Disconnected`，且 READY 后不检查 IPC
handle 已退出，standalone 可继续存活。按既有 D13 收口：auto-enable 移至物理
出口 / ACK 后、READY 前；`Full` 日志留 Safe，不重试；`Disconnected` 进入有界
启动清理并退 1。READY 后发现 IPC handle finished 同样提交 Disabled 并清理退出。
此项不扩展看门狗架构。测试独立区分健康 `preflight_finished_ns` 与首次带 gen 的
`first_exit_ns`；前者只观测真实 finish_boot，健康路径后者必须保持零。

## C3 Reverse Sync（2026-09-08，独立 reviewer 方案 ACK）

S-40：EStop 不再丢弃尚未执行请求的身份，SerialRT 在原 pending 槽标取消位，
由 serial 唯一消费者完成零执行取消与记录。没有跨槽搬运，不增加事实槽；权威
EStop 仍由 safety_word 一次成功 CAS 发布。下文 D3 / D13 / AC 已同步；这项批准
不代表实现或验收已完成。C1 中间实现的同步清空语义随 C3 整包修正，HardRT 不变。

## Spec amend 记录（round 20；reviewer 复核后）

- **P0 AwaitAck 保留证据与 ACK 义务**：publisher 已将 confirmed CAS 成
  `StopCommanded`，consumer 不再用当前 word 反推旧观测。`ConvergeRound` 增
  serial 自有 `completed_safety`；`AwaitAck` 先于启动合并规则分派：可信 all-off
  → 保留原 ctx / gen / inhibit / 启动截止，仅重开物理 `StopIssue → Confirm`；
  再次完成仍经 `AwaitAck(n)`，早到 ack 不跳过物理收敛；不可信 → 零 IO 升级 SSF
  新 gen。AC-19(iv) 冻结 ACK 后重开并再次确认仍等 n；AC-23 统一为每物理轮有界，
  明确 AwaitAck 离开后重开的 IO 例外，启动重开不重复 Connect、不延长截止。
- **P1-1 policy 写权收紧**：核基线 serial `:401-402` 与 ctrl `:136-139` 均无条件
  store（S-37）。删除 claim 通用尾写；`StopIssue` 仅在 `estop_bound` 时以 strong
  CAS 将允许普通输出的 policy 收紧为 `StopTorque`，`Disabled` / `StopTorque`
  保留。IPC 负责状态机 policy 提交，Fault 后的 `Disabled` 不被 serial 覆盖；纯
  Fault 收敛不写 policy（AC-5(c)）。AC-9(r) 覆盖 SSF 已 ACK 后 IPC 先处理 EStop、
  serial 后 claim，以及 StopIssue 与 IPC 写 Disabled 的两种顺序。
- **P1-2 mailbox 不被连续 EStop 饿死**：核基线 `ipc_tick.rs:128-141` emergency
  提前 return（S-38）。Step 1 只记 `emergency_handled` 并清 pending，Step 1b
  每 tick 保留一次 mailbox 服务，随后才结束普通处理；SSF 仍优先，ACK 只写实际
  快照 gen。AC-8(j) 连续 4 tick EStop + 非 SSF n 待 ACK；AC-8(k) 锁 Step 0 后
  SSF 升级，≤ 2 tick 处理。
- **非阻断同步**：AC-9(o6) 第二次注入前明确 serial / IPC 各自已完成第一次
  claim，另测 notice 尚未消费时合法合并；AC-22(l16) 改为零业务 worker spawn
  （`boot_supervisor` 已启动）。D1、D9 记为已 ACK；D4、D11–D14 待复核，未进 Plan。

## Spec amend 记录（round 1–19；摘要）

- round 1–4：五预算 + RPC 返回表；SLA 测量目标 + fast lane；typed `CommErrorKind`；
  S-9；gen/ack + inhibit；HardRT 隔离；带 valid 位的 active/completed 记录；
  mailbox；S-10；EventQueue；resolver；liveness lease；DiagSink；40 位 seq；
  retire-and-converge 矩阵；inhibit 位集；emergency 双位；Lamport SPSC；事务不可
  拆抢占；BusOpClass；`FAULT_PENDING`；confirm 契约修订。
- round 5–8：phase 表；撤边界证明与进程内 reopen（S-11）；`rx_trust`；路由矩阵
  全定义；无阈值首次定级；单调升级新 gen；S-12；权威封顶表；`enter_fault` 分流；
  `StopRetry`（`MAX_STOP_ATTEMPTS = 3`）；一次性授权模型；preflight 接入
  READY/auto-enable；`ConvergeContext` 三上下文；EStop 幂等合并。
- round 9：`session_clean` / `rx_trust` 拆分 + `evidence_trusted(ctx)` 封顶；显式
  双状态 marker `1\n`/`0\n` + truth table；`rx_trust.lock`；启动上下文先选定、
  Dirty 单轮收敛、READY 等 ack；`adapter_connected` 语义扩为 enable-ready guard。
- round 10：正文自包含；两阶段 marker 写（rename 线性化点，S-13）+ authorize 退出码
  0/2/3 + `status` / `revoke`；(ConvergeContext × ConfirmOutcome) 出口矩阵；启动
  序列 `StopIssue → Connect → Confirm`（feetech `new()` 预填 entries，S-14；open
  先于消费；`T_boot(N)`）；daemon `LOCK_NB` + 退 87 + unit
  `RestartPreventExitStatus=86 87`。
- round 11：`estop_bound` 义务与 code 升级正交（出口矩阵按 `estop_bound` 分列，
  AC-19 五条序列 (i)–(v)）；`call_deadline` 在 `FeetechAdapter::new` 安装（S-15）；
  收紧一律 backup-first 持久屏障（consume / revoke）；`SerialTrustPolicy`（S-17）；
  wd / external channel 有界（S-16）；seq ≥ 1；`OpenFailed` / `DiscardFailed` 归属。
- round 12：`B_startup_ack`（S-18）；gen = Some 决定 `AwaitAck`（AC-19(vi)）；
  EventQueue 三 lane + 两级 admission + typed `ExternalEvents`（撤裸
  `Sender<Event>`）；authorize canonical `1/1` 前置；SLA-B fast-path / 全路径分列
  （270 / 690 ms）；`SerialTrustPolicy` 精确 kind 表（S-19）；census 改"IPC 入向
  事件槽上限"；锁 errno 分类（87 仅 `EWOULDBLOCK` / `EAGAIN`）。
- round 13：进程级 `T_abs` + 共享 join 截止 + 强制 exit（S-20）；
  `SafetyStopFailedAwaitAck{gen, detail}`（gen = Some 无例外等 ack）；mailbox 独立
  原子槽不进 lane；`Enable(false)` 仅物理豁免（仍发 gen → `AwaitAck`）；外部
  EStop store(Release) 先于断扭；revoke 退 4；`Stop` 仅控制面收尾（S-21）；
  `B_hw_confirm` 525 ms。
- round 14：`boot_supervisor` 独立执法（`CP1` / `CP2`，S-24）；emergency 分路
  （S-22）；`CommErrorKind::PendingDeadline` + `TRUST_CLEARING` 集合；AC-12(c) 进程
  存活判据（S-23 `panic = "abort"`）；五 lane + 两级预算固定优先级；`external_estop`
  计 1 槽。
- round 15：READY 提交点 = datagram 送达后（S-25）；supervisor 自 CLI parse 起
  （S-27）、硬路径仅原子写 + `_exit(88)`、覆盖边界明示；`EmergencyBatch` 分派 +
  状态机优先规则（S-26）；完整 tick 调度表（pending 在飞不 drain lane，S-28）；RR
  上界推导；`StopFlags` 预建七槽；反应 ≤ 100 ms。
- round 16：`try_notify_ready` 三态收紧 + `--notify-mode`（S-29）；`SerialEmergencyCtrl`
  原语（SerialRT `Some` / HardRT `None`）；`EmergencyBatch` = estop 位 + mailbox 单词
  快照（gen / detail 原子绑定），发布顺序 ①②③ + IPC 唯一 ack；RR 8 + 6 = 14 tick、
  撤 "< 2 s"；`CP1` = 全部 worker spawn（S-30）。
- round 17：`safety_word` 单 CAS 原语（CAS 后零写；serial 消费自置 policy）；
  `(Fault(_), EStop) if Guards.rt_serial → 原 Fault`（S-26）；rr lane 第二道
  deadline 检查（S-33）；supervisor 自 `main()` 首语句 + "t_start → READY 上界"
  + pre-main 盲区（S-32）；`CP1` = required worker 全部 `Spawned`（恒七，S-31）；
  隔离改口构造期不变量 + factory 分派测试。
- round 18：`safety_word` 改 level-triggered `estop_pending` 位（撤 `estop_seq`）；
  serial 四检查点重读；classifier 同步 `(Emergency, Disabled)`（S-34）；`N_max =
  MAX_JOINTS = 32` 起算、resolve 后 `N_ATOMIC`（S-35）。
- round 19：`normal_io_active` acquire-CAS 与 EStop CAS 全序；每逻辑 EStop 恰一次
  原语，双位 `serial_estop_pending` / `ipc_estop_notice` 各自在处理开始时 claim，
  撤 `external_estop`；`TooManyJoints` + CLI 上限（S-36）；strong CAS，进展仅称
  lock-free。

## What

> **Plan round 1 Reverse Sync（2026-09-08）**：保留 D1 的所有 IPC 超时均须
> `CommFault + Disabled` 并由 serial 收敛的安全语义，补齐 S-39 未提交请求表示；
> 按 D13 收准周期事务“入口拒绝”与“在途不可拆”的区分。独立 reviewer 已确认
> 两项缺口成立；修订落 D3 / D4 / D13 / AC-4 / AC-6，正式 Plan 复审后才实施。

1. **有界 IO**：`D_call` 单次 transport 调用预算，自 adapter 构造起生效；事务
   （request + response）不可拆；serial 线程单次 transport 调用 ≤ `D_call`；
2. **可中止 action**：仅 `NormalEffectful` 事务在事务边界与 sleep 分片内接受抢占，
   抢占类型化，与带 valid 位的 active 记录绑定；本地 deadline 中止不破坏 RX 信任；
3. **fail-closed 升级链**：通信错误按路由矩阵首次即定级 → `enter_fault` 按 code
   分流 → 统一收敛 phase 机（三上下文 + `estop_bound` 义务 + gen 决定 ack 等待，
   启动前选定）→ mailbox 单词原子发布 / IPC 唯一 ack → 单调升级；IPC 侧 EStop 经
   `safety_word` **单 CAS** 线性化，严重度只升不降、CAS 后零写；Fault 粘滞不被
   EStop 清除（SerialRT）；
4. **诚实扭矩态**：证据可信判据 `evidence_trusted(ctx)` 在权威状态处封顶；
   `rx_trust` 只在可信 preflight all-off 后置 true；显式双状态 marker + 两阶段
   strict 写 + 持久屏障 + canonical 前置 + 非阻塞独占锁的一次性授权；启动
   `StopIssue → Connect → Confirm` 锁在 READY/auto-enable 之前，READY 提交点 =
   datagram 真送达后（standalone 模式除外），**t_start → READY** 由独立 supervisor
   强制有界（明示覆盖边界与 pre-main 盲区）；EStop 义务不随 code 升级降级；
5. **人工恢复 + 可观测**：信任恢复 = 外部信任边界 + 维护者显式授权（`0\n`）+
   启动 preflight；ClearError 守门；`DiagSink`；RR 不变量锁死（含 rr lane 第二道
   deadline 检查）；EStop fast lane；完整 tick 调度表下按事件类型两级 admission +
   固定优先级配额的有界入向事件通路。

## 影响范围

- **In**：
  - `robot-adapters/feetech`：`set_timeout` 契约修正 + additive `discard_input` /
    `drain_until_quiet`；`RealSerialTransport` 分段读写 + 时钟注入 + open 初值
    `D_call`；`FeetechBus::new` 初值 `D_call`；`FeetechAdapter::new` 安装
    `call_deadline`（失败 → 构造失败）；`FeetechBus` 持 `PreemptProbe` +
    `BusOpClass`；`FeetechAdapterConfig` 增 `preempt` / `diag` / `call_deadline`；
    sleep 分片；`FeetechError` 拆分；TX timeout 归 `IoWrite`；`new()` 预填
    `joint_ids` / `safety_stop_entries` + 校准核对（S-14）；热路径 tracing →
    `DiagSink`。
  - `robot-core`：`CommErrorKind`（含 `PendingDeadline`）、`RobotError::Comm(kind)` /
    `Aborted(AbortReason)`、`AdapterErr::{Comm(kind), AbortedByTimeout}` 编码、
    `BusOpClass`、`PreemptProbe`、`DiagSink` / `DiagEvent`、`SafetyStopFailDetail`、
    `NormalAbortTarget::{Submitted{seq}, Unsubmitted}` 与
    `ExecutorCtrlClient::abort_pending_normal(target)`（唯一 additive default）、新 trait
    `SerialEmergencyCtrl { fn estop_monotonic(&self) -> EStopOutcome }`（additive
    trait；`ExecutorCtrlClient` 除上述 abort additive default 外 0 改）、`SEQ_BITS = 40`。
  - **`robot-health`**：additive `try_notify_ready() -> ReadyOutcome { Sent,
    NotConfigured, Err(io::Error) }`：`Sent` 仅当 `send_to` 实际执行且 Ok；abstract
    `@…` → `Err(Unsupported)`；未设置 `NOTIFY_SOCKET` → `NotConfigured`（旧
    `notify_ready` / `send_message` 语义保留 0 改，供其它进程沿用）。
  - `robot-s0`：serial 收敛 phase 机 + `ConvergeRound` / `ConvergeContext` /
    `completed_safety` / `estop_bound` / `SafetyStopFailedAwaitAck` / 启动阶段（Connect）/ 路由矩阵 /
    `enter_fault` 分流 + 单调升级 / mailbox 单词发布顺序 / inhibit 位集 /
    `evidence_trusted` 封顶 / `session_clean` + `rx_trust`（清零 kind 集合）/ 记录槽
    / `liveness_counter`；**`safety_word` packed 权威状态（SerialRT）+
    `HardRtCtrlClient: SerialEmergencyCtrl`（单 strong CAS，发布双位）+ `normal_io_active`
    acquire-CAS + `safety_state()` 按构造期 `ctrl_kind` 选源**；serial claim 零 policy 写、
    `StopIssue` 按 `estop_bound` 调用 `tighten_estop_policy()` 条件 CAS；**`validate_hardware_section`
    增 `TooManyJoints`（`MAX_JOINTS`）+ CLI `--joints` 上限校验**；**`rx_trust` 模块**（两阶段 marker 写 + 持久屏障 truth table
    + canonical 前置 + `rx_trust.lock` 非阻塞锁（errno 分类）+ `rx-trust authorize |
    status | revoke` 子命令 + 退出码 87 / 4）；`SerialTrustPolicy` 穷尽分派；
    EventQueue 五 lane + `EmergencyBatch`（`ipc_estop_notice` claim + mailbox 快照）+
    两级 admission + 两级预算 + 完整 tick 调度表（含 rr lane **第二道 deadline 检查**）+ typed
    `ExternalEvents`（撤裸 `Sender<Event>`）+ wd / external channel 有界；状态机
    `Guards.rt_serial` + 优先规则 `(Fault(_), EStop) if rt_serial → 原 Fault`；
    `CommFault` 状态机规则；ipc fast lane + mailbox 唯一 ack + resolver；ipc_tick
    F2 + abort + ClearError 守门；pub_thread 映射（SerialRT 读 `safety_word.state`）
    + DiagSink 消费；RR 常量 + 入队后建 pending；daemon 顺序（**`main()` 首语句
    `boot_supervisor`**（`N_max` 起算，parse 后重算）→ logging init → CLI parse →
    `StopFlags` 预建 → config resolve → lock → latch → open → 消费 → 七 worker
    spawn → `SPAWN_REACHED` → 协作式等待 → `try_notify_ready` + `--notify-mode`
    → `READY_REACHED`；supervisor 双检查点 `_exit(88)`）+ liveness 观察器接
    `liveness_counter` + `--rx-trust-dir` / `--rx-trust-policy` / `--notify-mode`。
  - `robot-msg`：`joint.fbs` 既有 enum `S0FaultCode` +1（`CommFault = 7`）。
  - `robot-tests`：`comm_fault_e2e`（IO 延迟分布 / kind / NACK / 断连 / 同形旧帧 /
    TX timeout / transport 序列记录 / characterization / 调度探针 / GetState 轮询 /
    marker truth table 与 partial 写 / 目录 fsync 与掉电回退注入 / 非规范初态 / 锁
    竞态与 errno 注入 / 启动序列与 READY 序 / pre-READY 注入（含 config 读盘阻塞、
    READY 四种 socket 情形）/ 连续 EStop / 上下文 × 出口矩阵 / `EmergencyBatch`
    十二格 + `estop_monotonic` 三条 CAS 竞态 + Fault × outcome 全表 + mailbox 发布
    顺序竞态 / rr lane 过期双 RPC / lane 灌满逐类注入 / 多来源持续灌流 / 在途
    SetEnabled + 灌流 / EStop 顺序锁 / factory 分派）。
  - deploy / docs：`scripts/systemd/xlerobot-s0-{left,right}-arm.service`
    `RestartPreventExitStatus=86 87`（88 可重启，注释登记）+ ExecStart 增
    `--notify-mode required`；`README_dimos_replace.md` 授权程序 + 退出码表（1 / 2 /
    4 / 87 / 88）；CHANGELOG 偏离登记；`10_verification.md`（含 supervisor 覆盖边界
    残余 + pre-main 盲区）；`Guards::adapter_connected` / `rt_serial` 字段文档。
- **Out**：进程内 reopen / resync 恢复与事务身份（紧后候选）；候选 E 的 D15 / D14；
  跨臂联锁；逐电机降级；S0 ClearError RR；F1-B；RR 非阻塞派发 (b)；ipc 线程 RT
  class 化；HardRT 行为（含 HardRT 下 EStop 通配转 `Safe`）；gateway / proto /
  client；`00_design.md`；iceoryx2 订阅队列配置；正常关停路径的 `graceful_stop`
  语义（由 systemd `TimeoutStopSec` 兜底）；`Stop` 的物理断扭语义；unit
  `TimeoutStartSec=infinity`（候选 E round 1 P0-2 硬要求，保持）；process-wide
  `SIGSTOP` / 调度饥饿 / 内核级阻塞下的启动上界；pre-main ctor 段；abstract
  notify socket 支持；`OutputSafetyProbe` / `ManipulatorAdapter` / `SerialRtAdapter`
  / `ExecutorCtrlClient`（除既有 additive default）均 0 改；既有 `estop_latch`
  写入/删除语义 0 改；deploy guard 六步 0 改。

## Spec 期新发现（均已核码）

- **S-6**：enable 分支仅 `Ok` 写 `safety_state`（`serial_io_thread.rs:584-604`）；
  逐电机顺序上扭（`adapter.rs:352-402`）。→ D5。
- **S-7**：S0 `Event::ClearError` 无生产注入点；gateway `ClearError` 目标 S1。→ D11。
- **S-8**：`adapter_connected` 仅启动置 `true`（`s0_daemon.rs:620/634`）。→ D9 推导。
- **S-9**：liveness-gated 喂狗每 1 s 比对四 worker `tick_count`
  （`s0_daemon.rs:1072-1100`，`systemd.rs:248`），`WatchdogSec=2s`；SerialRT
  观察器 = serial `tick_count`（`s0_daemon.rs:1083`）。→ D2 / D3。
- **S-10**：`Fault(SafetyStopFailed)` 仅消费规则（`state_machine.rs:565`），无生产
  点（HardRT / SerialRT 均无）；confirm 首错即终态（`serial_io_thread.rs:445-500`）。
  → D4。
- **S-11**：协议无事务身份（`protocol.rs:12`）；串口链 1a86:55d3 / cdc_acm；
  close/open 无设备复位。→ D4 / D8 / D9。
- **S-12**：`spawn_serial_io_thread` 在**调用方线程**同步 `adapter.connect()`
  （`serial_io_thread.rs:266`），失败 → daemon 退 1（`s0_daemon.rs:899-910`）；
  成功后直写 `TorqueOffConfirmed`（`:269`）无 readback；daemon 随后 auto-enable
  （`s0_daemon.rs:1049`）再 `notify_ready`（`:1071`）；Enable 守卫只查
  `ready_guards_pass`（`set_enabled_action.rs:80`，`state_machine.rs:191-196`）。→ D9。
- **S-13**：`save_single_strict` 先 rename（`estop_latch.rs:362`）再目录 open +
  fsync（`:370-371`）；Err 不能推出目标文件未变。→ D9 两阶段写。
- **S-14**：`safety_stop_fast` 要求 `safety_stop_entries.len() == joint_ids.len()
  > 0`，两者在 `connect()` 末尾填充（`adapter.rs:435-438`、`:460-494`）；connect
  失败则 emergency 广播被拒为 `NotConnected`。→ D4 / D9（`new()` 预填）。
- **S-15**：`FeetechBus::new` 初值 `DEFAULT_TIMEOUT_MS`（`bus.rs:70`），
  `RealSerialTransport::open` 同值（`transport.rs:206`），仅 `connect()`
  （`bus.rs:102`）向 transport 下发 timeout。→ D2 安装点。
- **S-16**：watchdog channel `wd_tx`（`s0_daemon.rs:555`）与 external `event_tx`
  （`ipc_thread.rs:559`）均 `unbounded`；watchdog 只 `try_send(HeartbeatTimeout)`
  （`watchdog.rs:102`）；`ipc.event_sender()` 暴露裸 `Sender<Event>`
  （`ipc_thread.rs:489`，可阻塞 `.send()`）；EventQueue = 单 `VecDeque<Event>`
  （`state_machine.rs:443-444`），IPC 四处 `events.push`（`ipc_thread.rs:682/695/
  699/703`）；daemon 退出 `let _ = event_tx.send(Event::Stop)`（`s0_daemon.rs:1133`）
  后走 `graceful_stop`（`:1412`）；`Stop` 既有语义 = `Stopping → Disabled`
  （`state_machine.rs:392/558`）。→ D13。
- **S-17**：`MockSerialRTAdapter` 上报 `RtClass::SerialRT`（`mock/src/lib.rs:352`）。
  → D9 `SerialTrustPolicy`。
- **S-18**：systemd watchdog 线程在 `notify_ready`（`s0_daemon.rs:1071`）之后
  spawn（`:1097`）；systemd `WatchdogSec` 仅在 start-up 完成后激活；unit
  `TimeoutStartSec=infinity`（`:97`，候选 E 硬要求）→ pre-READY 无执法。→ D9
  `boot_supervisor`。
- **S-19**：adapter kind 注册名 = `feetech_sts3215`（`factory.rs:158`）、
  `mock_serial_rt`（`mock/src/lib.rs:413`）、`mock_hard_rt`（`:278`）。→ D9 分派表。
- **S-20**：`graceful_stop`（`s0_daemon.rs:1412`）逐线程 `request_stop()` + 无超时
  `join()`（`:1437-1462`），任一 worker 不退出即永久阻塞；仓内无 `JoinHandle::
  is_finished` 使用。→ D9 强制关停。
- **S-21**：serial loop `while !stop.load`（`serial_io_thread.rs:373`）退出不调
  `adapter.safety_stop()`（仅 emergency drain `:389` 调）；`set_policy(Disabled)`
  只关 `do_write` 白名单（`:526`）；serial 消费 emergency 时**自行**置
  `safety_latched` 与 policy `StopTorque`（`:390-391`）。→ D13 `Stop` 口径 /
  `estop_monotonic` 不写 policy。
- **S-22**：`ipc_tick` Step 1（`ipc_tick.rs:128-138`）对 `drain_emergency` 返回的
  EStop 与 `AdapterErr(SafetyStopFailed)` 同路处理，**无条件**
  `executor_ctrl.safety_stop()`（`:135`）。→ D13 `EmergencyBatch`。
- **S-23**：workspace release profile `panic = "abort"`（`Cargo.toml:194`）→ 任一
  线程 panic 终止整个进程。→ AC-12(c) 口径。
- **S-24**：`serialport::open`（`transport.rs:204`）同步；`spawn_ipc_thread` 以
  `ready_rx.recv()`（`ipc_thread.rs:595`）阻塞调用方直到 subscribers 就绪。→ D9
  `boot_supervisor`。
- **S-25**：`robot_health::systemd::notify_ready()` 返回 `()`，`let _ =
  send_message("READY=1\n")` 吞掉错误（`systemd.rs:70-72`）。→ D9 READY 提交点。
- **S-26**：状态机 `(_, E::EStop) => Ok(S::Safe)`（`state_machine.rs:553-555`，
  "EStop 压倒一切，包括 FAULT 和 STOPPING"）；`plan` 阶段 EStop → `Emergency` +
  `StopTorque`（`:358-359`）；`HardRtCtrlClient::safety_stop` 五步含 `safety_state
  := StopCommanded`（`hard_rt_ctrl.rs:171-182`）。→ D13 `EmergencyBatch` +
  `estop_monotonic` + 状态机优先规则。
- **S-27**：`parsed.resolve()`（`s0_daemon.rs:471`）经 `config_loader.rs:308`
  `fs::read` 读配置，在 `run()` 之前。→ D9 supervisor 起点。
- **S-28**：`ipc_tick` Step 2 在 pending 在飞时 `return outcome`（`ipc_tick.rs:186`
  / `:209`），Step 3 `plan_transition` 仅在 pending 为空时（`:214`）。→ D13 调度表。
- **S-29**：`send_message`（`systemd.rs:343-349`）在 `NOTIFY_SOCKET` 未设置或为空
  时返 `Ok(())`；`send_message_to`（`:353-356`）对 abstract socket `@…` silent skip
  亦返 `Ok(())`。→ D9 `try_notify_ready` 三态 + `--notify-mode`。
- **S-30**：daemon worker spawn 顺序 = ipc（`s0_daemon.rs:681`，含 ready 握手）→
  ClearEStop rr_server（`:730`）→ SetEnabled rr_server → 心跳 watchdog（`:801`）→
  executor（`:856` HardRT / `:889` SerialRT）→ pub（`:942`）→ io（`:968`）。→ D9
  `CP1`。
- **S-31**：io worker 为占位（`s0_daemon.rs:967-968`），**生产 plan 始终 spawn**；
  故当前 required worker set 恒七个。→ D9 `CP1` 口径。
- **S-32**：`main()`（`s0_daemon.rs:453-477`）在 `run()` 前顺序执行
  `robot_logging::init()`（`:456`）、`Args::parse()`（`:458`）、`parsed.resolve()`
  （`:471`）；robot-logging 另有 pre-main `#[ctor]` 自动 `init()`（`lib.rs:217`）。
  → D9 supervisor 起点 = `main()` 首语句；ctor 段为盲区。
- **S-33**：RR 请求 deadline 检查仅在 source channel drain 时（`ipc_thread.rs:726`
  ClearEStop / `:782` SetEnabled：`clock.now() > deadline_ns` → `DeadlineExceeded`
  不入队）；新增 rr lane 后请求可在 lane 内过期。→ D13 第二道检查。
- **S-34**：`classify_transition`（`state_machine.rs:349`）对所有 `EStop` 先返回
  `(Emergency, StopTorque)`（`:357-359`），独立于 `try_transition` 的目标态；仅改
  `try_transition` 不足以让 Fault 态下的 EStop 得到 `Disabled`。→ D13 classifier
  同步。
- **S-35**：生产电机数来自 `parsed.resolve()` 后选中的 `HardwareSection.motor_ids`
  （`s0_daemon.rs:313-324` 校验并派生 layout）；raw `--joints` 仅 mock adapter
  参数（默认 3，`:173` / `:402`）；结构上限 `MAX_JOINTS = 32`
  （`robot-core/src/adapter.rs:89`）。→ D9 `N_max` 与 `N_ATOMIC` 时点。
- **S-36**：`validate_hardware_section`（`config_loader.rs:160-173`）只校验非空 /
  等长 / 唯一，**无** `len ≤ MAX_JOINTS`；33 个唯一 motor id 可通过 resolve，直到
  feetech `FeetechAdapter::new`（`adapter.rs:159`）才拒绝，mock 则在
  `MockHardRTAdapter::new`（`mock/src/lib.rs:53-57`）`assert!` panic；CLI
  `--joints` 亦无上限校验。→ D9 `N` 上限前置校验。
- **S-37**：serial emergency 兜底以普通 store 写 policy `StopTorque`
  （`serial_io_thread.rs:391-402`）；控制面 `set_policy` 亦为普通 store
  （`hard_rt_ctrl.rs:136-139`）；`ipc_tick.rs:153-156` 已明确 Fault 路径不能以
  `safety_stop()` 覆盖 `Disabled`。→ D4 / D13 撤 claim 尾写，StopIssue 仅允许
  条件 CAS 收紧；终态与 Fault policy 不受 serial 后写覆盖。
- **S-38**：`ipc_tick.rs:128-141` emergency 分支清 pending 后立即 return；若把此
  顺序沿用到新增 mailbox Step 1b，每 tick EStop 会使 mailbox 永不被服务。
  → D13 将提前 return 后移至 Step 1b 之后。
- **S-39（Plan round 1 核码）**：`ipc_tick.rs:149-151` 在 `:170` 的首次
  `request_normal` 之前检查总超时。Step 3 建立 `(plan, None)` 后 IPC 停顿超过
  1 s 即可无 seq 超时；持续 `Busy` 也可走该分支，不能笼统视为不可达。
  → D3 类型化 timeout 请求新增 `Unsubmitted`，不伪造 seq 0 或 EStop，
  保持 D1 的 `CommFault + Disabled`、物理收敛与 gen/ACK 义务。
- **S-40（C3 核码，C1 实现 `fc0c142`）**：`estop_monotonic_impl` 在权威 CAS
  之前 `pending_normal.swap(0)` 丢弃请求词；正常交错为 IPC 已发布 Submitted
  timeout、serial 未消费、EStop 再清 pending，三类记录均不匹配，误进 Unknown。
  → D3 / D13 改原槽取消 tag，保留完整请求直到 serial 消费；不新增跨槽事实搬运。
- **S-41（C4 迁移核码，基线 `beebfa0`）**：旧 `ipc_thread.rs:864–910` 在 source
  drain 建立 live 请求后、实际执行前，对 emergency 移除 ClearEStop event 并回复
  Preempted，避免清除新 latch。P.4 将 PendingReplies 延至 dequeue 后创建，若只
  清它会让已 admission 的 live RR 丢失急停保护。→ D13 冻结 rr 前缀，按配额取消；
  明确只涵盖 IPC 已 admission 的 live 请求，不扩展为所有上游旧请求。
- **S-42（C4 IPC 独立评审）**：timeout 已预提交 CommFault 但 serial 未消费 abort，
  或 tick 开头 guard 刷新之后才发布 mailbox，旧健康 guard 仍可能让同 tick
  ClearError 恢复 Ready。→ Fault 提交点同步清两 guard，持续推导按 abort→inhibit
  的 Acquire 顺序读取；禁止拼接旧 inhibit=0 与新 abort=0。
- **S-43（C4 daemon 独立评审）**：auto-enable 丢弃 channel 结果且 READY 后
  未检测 IPC handle finished，接收端退出后 standalone 可常驻。→ 按 D13 退出
  责任穷尽处理发送结果，并检测真实 IPC 退出；不新增重启或恢复路径。

## 术语

- `N` 电机数（left arm 6，full 13）；`tick` = 5 ms（200 Hz）；`q` = 抢占检查量子
  5 ms；`D_call` = 50 ms；σ_s / σ_i / σ_p = serial / ipc / pub 调度延迟，未知测量项。
- **事务** = 一次 request（write_all）+ 其 response（read_exact）或单次 write-only。
- **信任量**：`session_clean`（进程内自首个启动事务前置 true）与 `rx_trust`（初值
  false，唯一置 true 点见 D4）；**信任清零 kind 集合** `TRUST_CLEARING = {Timeout,
  FrameCorrupt, ResponseIdMismatch, PeerNoResponse, Disconnected, IoRead, IoWrite,
  DiscardFailed, Unknown}`（单向清零）；`PendingDeadline` **不在集合内**。
- **轮次** `ConvergeRound { ctx: ConvergeContext, gen: Option<u32>, estop_bound:
  bool, attempt: u8, some_on_seen: bool, completed_safety: Option<ConvergeCompletion> }`；
  `ConvergeCompletion ∈ {AllOffTrusted, AdapterUncertain{kind: CommErrorKind}}`，
  **仅 serial 持有**：初始 / 重开物理阶段为 None，Confirm / D3 豁免 / 启动 deadline
  出口写 Some，再进入 `AwaitAck`；publisher 不读写此记录。它只记录本轮已完成的证据，
  不替代共享 `safety_word.state`，不受 EStop CAS 改写。
  `ConvergeContext ∈ {Fault, Emergency,
  Preflight}`；**`estop_bound`** = 本轮由 EStop 创建或合并了 EStop，**code / gen
  升级永不改它**；**`gen = Some` 的轮次必经 ack 等待（`AwaitAck` 或
  `SafetyStopFailedAwaitAck`），无例外**；**出口** `ConfirmOutcome ∈ {AllOffTrusted,
  AllOffUntrusted, SomeOnExhausted, ErrExhausted}`。
- **`safety_word: AtomicU32 = {state:8 | serial_estop_pending:1 | ipc_estop_notice:1
  | normal_io_active:1 | 保留:21}`**：SerialRT 权威 `safety_state` 容器 + EStop
  发布位 + normal IO 执行权。`serial_estop_pending` / `ipc_estop_notice` = 由原语
  **一次 CAS 同时置 1** 的两个 level-triggered 请求位，各由其唯一 consumer（serial
  / IPC）在**处理开始时**以 CAS claim（清 0）；`normal_io_active` = serial 经
  acquire-CAS 取得的 normal 事务执行权。一切写入均为保留他人字段的 **strong**
  CAS。初值 {`TorqueUnknown`, 0, 0, 0}，于 ctrl 构造时（早于业务 worker spawn；
  `boot_supervisor` 已启动）。HardRT
  沿用 legacy `safety_state: AtomicU8` + `emergency_action` + EventQueue
  `estop_pending`。诊断计数 `estop_requests: AtomicU32`（Relaxed，非权威）= 原语
  调用次数 = 逻辑 EStop 注入次数。
- **`SerialTrustPolicy ∈ {Marker, Static}`**：SerialRT 计划的信任来源（D9）。
- **admission**：事件按类进入 EventQueue 的规则（D13）；`AdmitError ∈ {Full,
  Disconnected}`。mailbox **不是**事件：独立 level-triggered 原子槽（D4 / D13），
  **一个 u64 词** {gen:32 | code:8 | detail:8 | origin:8}，gen 与 detail 原子绑定。
- **SerialRT pending 取消 tag**：`PENDING_CANCELLED = 1 << 63`；action / arg
  沿用低 16 位，合法 seq 在位 16..55，位 56..62 必须为零。只剥 bit63 后校验
  seq 为 1..SEQ_MASK，禁止宽 mask 吞掉非法位。HardRT 仍用完整 48 位 seq，绝不剥 tag。
- **`EmergencyBatch { estop: bool, ssf: Option<MailboxSnapshot { gen, detail,
  origin }> }`**：IPC 每 tick Step 0 以 CAS claim `ipc_estop_notice`（SerialRT）或
  取走 EventQueue `estop_pending`（HardRT）并快照 mailbox（code ==
  SafetyStopFailed ∧ gen ≠ `fault_ack`）（D13）；**SSF 态** = `S0State ==
  Fault(SafetyStopFailed)`。
- **`estop_monotonic() -> EStopOutcome { StopIssued, Merged, LatchOnly }`**：
  SerialRT 专用 `SerialEmergencyCtrl` 原语，**每个逻辑 EStop 恰调用一次**（唯一
  发布者 = topic `poll_estop` 或 `ExternalEvents::estop()`；IPC Step 1 不调），
  **一次 strong CAS 即整个操作的线性化点**（同时完成严重度规则与两请求位置 1），
  权威状态 CAS 之后零写（D13）。
- **`boot_supervisor`**：进程内独立执法线程；`t_start` = **`main()` 首语句** spawn
  时刻（logging init 与 CLI parse 之前）；检查点 `CP1 = t_start + T_pre`
  （`SPAWN_REACHED` = resolved plan 中全部 required worker 到达 `Spawned`；当前恒
  七个）、`CP2 = T_abs + 1 s`（`READY_REACHED`），`T_abs = t_start + T_pre +
  T_boot(N) + B_startup_ack`（resolve 前以 `N_max = MAX_JOINTS = 32` 计，S-35）；执法 =
  `_exit(88)`（D9）。
  **盲区** = pre-main ctor（S-32），不在任何上界内。
- **notify 模式** `--notify-mode {required | standalone | auto}`：`auto` =
  `INVOCATION_ID` 存在 → required，否则 standalone（D9）。

## 关键设计决策（D1–D15 ↔ Q1–Q15；Spec Approved）

### D1（Q1）预算栈 + 预算模型 + RPC 返回表 + resolver 接线

预算栈（0 改）：client 4 s > grpc 3 s > gateway `timeout_ms + 500 ms` = 2.5 s
（`gateway.rs:986-989`）> wire deadline 2 s（`:936-937`）> ipc `T_pending` 1 s
（`ipc_tick.rs:86`）；有效预算 = min(meta 剩余, `T_pending`)。

| 预算 | 定义 | 数值（N=6，不含 σ） |
|---|---|---|
| **B_success** | `Enable(true)` 在 IPC 判定成功前完成 = `T_pending` = 1 s（nominal ≈ 100 ms） | 越过一律中止（fail-fast，AC-4f） |
| **T_abort_reaction** | 抢占置位 → `Aborted` 返回 ≤ 一个不可拆事务 2·`D_call` + q + σ_s | ≤ 105 ms + σ_s |
| **B_action_retire** | `T_pending` + T_abort_reaction | ≤ 1.105 s + σ_s |
| **B_stop_issue** | 下一事务边界发起断扭广播，**host write 返回**（`transport.rs:25`，不证明 UART 发完或电机执行）；`MAX_STOP_ATTEMPTS = 3`（初始 1 + 重试 2，间隔 1 tick）；`D_call` 自 adapter 构造起已安装（D2） | 3·`D_call` + 2·tick = 160 ms |
| **T_connect(N)** | 启动 `Connect` = N × ping（PING + Model_Number 各 1 事务 ≤ 2·`D_call`） | 4N·`D_call` = 1.2 s（N=13：2.6 s） |
| **B_hw_confirm（证据可信）** | Σ_{k≤5}(tick + σ_s + probe ≤ 2·`D_call`) | nominal ≈ 11 ms；5 次 525 ms + 5σ_s |
| **B_hw_confirm（证据不可信）** | 权威封顶 `AdapterUncertain`；每次 drain ≤ 150 ms + probe ≤ 100 ms + tick，≤ 5 次 | ≤ 1.275 s + 5σ_s（取 1.3 s） |
| **B_boot(N)** | 启动一轮 = B_stop_issue + T_connect + B_hw_confirm（不可信） | ≤ 2.66 s（N=13：4.06 s） |
| **T_pre** | 启动前置预算（自 `t_start`）：logging init + CLI parse + config resolve（`config_loader.rs:308`）+ 锁 + latch + 串口 open + marker 消费（fsync）+ **七 worker spawn**（含 ipc ready 握手 `ipc_thread.rs:595`）；`CP1` 检查 `SPAWN_REACHED` | 5 s |
| **T_boot(N)** | spawn 后等启动出口 deadline = 4N·`D_call` + 2.5 s | 3.7 s（N=13：5.1 s） |
| **B_startup_ack** | 出口后等 IPC commit + `Disabled` + gen ack 的追加预算（S-18：pre-READY 无 watchdog） | 2 s（= `WatchdogSec`） |
| **T_abs** | `t_start + T_pre + T_boot(N) + B_startup_ack`（resolve 前按 `N_max = MAX_JOINTS = 32` = 15.9 s，resolve 后按选中 hardware 的 `motor_ids.len()` 重算只缩短，S-35）；`CP2 = T_abs + 1 s` 检查 `READY_REACHED`（= READY datagram **真**送达，或 standalone 模式 `NotConfigured`），由 `boot_supervisor` 执法（`_exit(88)`） | 10.7 s（N=13：12.1 s；resolve 前 N_max 32：15.9 s） |
| **t_start → READY 上界** | `CP2` + supervisor 反应（轮询 50 ms + 调度 ≤ 50 ms）= `CP2` + 100 ms；**覆盖边界**：主线程 / 任一 worker 停摆；**不覆盖**：pre-main ctor（S-32）、process-wide `SIGSTOP` / 调度饥饿 / 内核级阻塞；**不称"进程启动总上界"** | ≤ 11.8 s（N=13：13.2 s；resolve 前按 N_max：≤ 17.0 s） |
| **RR 端到端（IPC 内；无 emergency 占用 tick）** | source channel → admission ≤ ⌈64/8⌉ = 8 tick；admission → rr lane 处理 ≤ ⌈128/24⌉ = 6 tick；总 ≤ 14 tick；在飞 pending 另 + `B_action_retire`；两道 deadline 检查（source drain 既有 + rr lane dequeue 新增） | 70 ms（无在飞）；不宣称与 wire 2 s 的严格关系（σ 未知） |
| **B_remote_observe** | + pub ≤ 20 ms（`pub_thread.rs:72`）+ 1 ms + σ_p + iceoryx2 + `state_pool` 2 ms（`state_pool.rs:50`）+ RTT + client 周期 | + 23 ms + RTT + client 周期 |

RPC 最终返回（复用 `SetEnabledRrPayload`，0 新变体）：

| 情况 | S0State | RPC | 客户端契约 |
|---|---|---|---|
| B_success 内 Ok | Enabled.Active | `Enabled{..}` | — |
| B_success 内通信错误 | `Fault(CommFault)` | `AdapterError` | 轮询 GetState；`safety_state` 终态 = `TorqueOffConfirmed`（证据可信）/ `AdapterUncertain`（不可信）/ `StopFailed` |
| B_success 内非通信 Err | `Fault(AdapterErr)` | `AdapterError` | 同上 |
| IPC 超时 | `Fault(CommFault)`（预提交；serial retire 补 gen，kind `PendingDeadline`，含 `Enable(false)` 豁免路径） | `AdapterError`（`T_pending` + ≤ 2 ipc tick） | 同上 |
| 停命令未交付 / 观测带扭 / EStop 义务下不可信确认 | `Fault(SafetyStopFailed)` | `AdapterError` | 人工现场确认 |
| 启动阶段未出口 / 出口非 Passed | Safe 或 Fault | `GuardRejected{actual}` | 等 READY 后 GetState |
| rr lane 满 | 不变 | `ResourceExhausted`（无 pending；≤ 8 tick 内决定） | 重试 |
| deadline 已过（source drain 既有检查，或 rr lane dequeue 第二道检查） | 不变 | `DeadlineExceeded`（零副作用） | 重试 |
| emergency / meta 先到期 | 既有 | `PreemptedByEStop` / `DeadlineExceeded` | 既有 |

resolver 接线：`ipc_thread.rs:1086` 由 `Fault(AdapterErr)` 扩为 `Fault(AdapterErr |
CommFault | SafetyStopFailed)` → `AdapterError`（AC-17）。

### D2（Q2）有界 transport：契约修正 + 安装点 + 全量枚举 + 两个上界

- `set_timeout` 语义改"单次高层调用总预算 `D_call`"（签名不变）；
  `RealSerialTransport` 分段 `set_timeout(remaining)` + `read`/`write`，剩余由注入
  `Arc<dyn RobotClock>` 计算；到期 → `TransportTimeout { wanted, got 真值 }`。
  **TX 超时归 `Comm(IoWrite)`，RX 超时归 `Comm(Timeout)`**。`D_call = 50 ms`
  （`FeetechAdapterConfig.call_deadline`；1 Mbps 单包 < 1 ms、13-motor sync_read
  ≈ 4 ms，≥ 10× 余量）；`DEFAULT_TIMEOUT_MS = 1000` 退役。
- **安装点（S-15）**：`FeetechAdapter::new`（`adapter.rs:157`）返回前调
  `bus.set_timeout(call_deadline)` → `transport.set_timeout`；`FeetechBus::new`
  （`bus.rs:70`）与 `RealSerialTransport::open`（`transport.rs:206`）初值一并改
  `D_call`；`connect()`（`bus.rs:102`）再下发为幂等。安装失败（Real
  `port.set_timeout` Err）→ `new()` Err → factory `build_serial_rt` Err → daemon
  退 1，**位于 marker 消费之前、零总线 IO**。由此启动首个 `Safety` 写（D4 启动
  序列首个 IO）已受 `D_call` 约束。
- census（`bus.write` = write_all + read_exact；`sync_read` = write_all + 1
  read_exact；`sync_write` = write_all）：

  | 段 | write_all | read_exact | sleep |
  |---|---|---|---|
  | sync_read Present ×1 | 1 | 1 | — |
  | [sync_write Goal + 20 ms + sync_read Goal] × 3 | 6 | 3 | 60 ms |
  | settle | — | — | 50 ms |
  | per-motor Torque_Enable × N × 3 | 3N | 3N | N × 300 ms |
  | **合计** | **W(N) = 7 + 3N** | **R(N) = 4 + 3N** | **S(N) = 110 + 300N ms** |

  N=6：25 / 22 / 1910 ms；N=13：46 / 43 / 4010 ms。
- 两个上界：无抢占 `B_noabort(N) = (W+R)·D_call + S(N)`：N=6 **4.26 s**、N=13
  **8.46 s**（> client 4 s、> 喂狗 2 s → 放大预算路线不可行）；有抢占退场上界 =
  `B_action_retire`。

### D3（Q3）A-bounded + C-lite：40 位协议 + 事务级抢占 + 操作域 + lease 化 liveness

- **槽与位宽**（`SerialIoCtrlInner` 新增；`SEQ_BITS = 40`，`SEQ_MASK`）：
  `active_action` {valid:1 | action:8 | arg:8 | seq:40 | 保留:7}；
  `completed_effect` {valid:1 | success:1 | action:8 | arg:8 | seq:40 | 保留:6}
  （不存 resp 码；错误详情走 DiagSink）；`abort_request: AtomicU64` 为带标签请求：
  `0 = ABORT_NONE`，`1..=SEQ_MASK = Submitted{seq}`，`1<<63 = Unsubmitted`。
  必须先判完整标签再比较序号；仅 Submitted 请求使用
  `slot.valid ∧ seq == slot.seq`，不得把高标签 mask 成 seq 0。非法编码按
  `InvariantViolation / Unknown` fail-closed。**fail-closed**：
  允许最大可编码 seq = 2^40 − 1；`request_normal` 在取号结果 ≥ 2^40 时回
  `NormalReqStatus::Unsupported`（`hard_rt_ctrl.rs:222` 取号处）；既有 48 位槽
  不改；无绕回 → 无 ABA。**seq 永不为 0**：`seq_counter` 初值 1
  （基线 `hard_rt_ctrl.rs:85`）。SerialRT 以 strong CAS 取号：旧值为 0 或超出
  `SEQ_MASK` 则拒绝且不推进计数器，合法旧值才 CAS 加一；Busy 仍允许消耗 seq。
  不能继续对已耗尽计数器 `fetch_add`，否则最终 u64 回绕会重新发出旧 seq。
  HardRT 保留原取号逻辑；`ABORT_NONE = 0` 永不匹配合法 seq（AC-4h）。
- **发布顺序**（serial 单写者）：① 读 `abort_request`（Acquire），Submitted 匹配 seq → 0 IO 回
  `AbortedByTimeout`；② `active{valid=1}`；③ 执行；④ `completed{valid=1,
  success}`；⑤ `active.valid = 0`；⑥ `last_completed`（既有）。跨线程配对：
  `abort_request` IPC Release / serial Acquire；`fault_mailbox` serial Release / IPC
  Acquire（单词）；`fault_ack` IPC Release / serial Acquire；**`safety_word` 双方均 strong CAS（AcqRel / Acquire 失败重读）**：serial 只改
  `state` 字节、claim 清 `serial_estop_pending`、acquire / release
  `normal_io_active`；IPC 原语只按 D13 规则改 `state` 并置两请求位；IPC Step 0
  claim 清 `ipc_estop_notice`；`estop_requests` Relaxed（诊断）；
  `emergency_action`（HardRT 专用）既有 Release / AcqRel swap；`SPAWN_REACHED` / `READY_REACHED` / `N_ATOMIC`
  主线程 Release / supervisor Acquire；`SUPERVISOR_FIRED` supervisor Relaxed
  （仅事后诊断）；`liveness_counter`、`active_action`、`completed_effect` 单写者
  Relaxed（pub_thread 只读观测）。
- **抢占单元与操作域**：`PreemptProbe`（`Send + Sync`：`should_abort(class)` /
  `checkpoint()` / `sleep_or_abort(class, dur)`）下沉到 `FeetechBus`：每个事务在
  **发请求前**检查；request + response **不可拆**（发出请求后必须完成 read_exact
  或超时）；adapter 三处整段 sleep（`adapter.rs:279` 20 ms、`:381` 150 ms、`:629`
  50 ms）改 q = 5 ms 分片、每片后检查。最长未检查区间 = 一个事务（≤ 2·`D_call`）
  或一片 q。**在事务边界的中止不产生 RX 失步**（响应已完整读取或超时已按 kind
  记账），故 timeout lane 的中止以 `PendingDeadline` 记账。`BusOpClass`：

  | class | 成员 | 抢占 |
  |---|---|---|
  | `NormalEffectful` | enable / set_mode / hold_current / 运行期 connect·disconnect 的事务 | emergency 请求（`serial_estop_pending` 位，事务起点 acquire-CAS 拒绝）与匹配的 Submitted timeout 均可抢占；Unsubmitted 由 tick 头走独立收敛入口 |
  | `PeriodicRead` | `read_state`（sync_read） | 不抢占；受 inhibit 门；起点须 acquire-CAS（`serial_estop_pending == 0`） |
  | `PeriodicWrite` | `write_command`（sync_write，无响应） | 不抢占；受 inhibit 门；起点须 acquire-CAS |
  | `Safety` | `safety_stop` 广播（`adapter.rs:638`，write-only） | 不抢占，仅受 `D_call` |
  | `Confirm` | `read_torque_enable_mask` 与其前置 hygiene（`discard_input` / `drain_until_quiet`） | 不抢占，仅受 `D_call` / drain deadline |
  | `Connect` | 启动阶段 `connect()` 握手事务 | 不抢占，仅受 `D_call`（启动期 EStop 按 D4 幂等合并） |

- **抢占类型化**：`RobotError::Aborted(AbortReason::Emergency | Timeout)` →
  `AdapterErr::PreemptedByEmergency`（既有码 4）/ `AdapterErr::AbortedByTimeout`
  （新码 6）。
- **两 lane + retire-and-converge 矩阵**：emergency lane = `safety_word.serial_estop_pending`
  位（HardRT：`emergency_action`）；timeout lane 读 `abort_request`。唯一新增接口为
  `fn abort_pending_normal(&self, target: NormalAbortTarget)`（default no-op，HardRT 保持原行为）；
  IPC 超时按 `seq_opt` 发布 Submitted / Unsubmitted，并预提交 `CommFault + Disabled`。
  预提交同时清两个本地健康 guard，覆盖本 tick 后续 control；mailbox 提交 Fault
  同样清 guard。持续 ready 推导必须先 Acquire 读 abort 再读 inhibit：serial
  登记 inhibit / generation 后以 AcqRel CAS 清 abort，读到清零者继而看到故障登记。
  正常动作错误同样须先由 serial 登记故障再 Release 发布错误完成，消除 response
  先可见而 inhibit 尚为零的接缝；取消结果不制造 Fault，仅终结当时已存在的 RR owner。
  serial 在 tick 头先解标签，事务边界只匹配 Submitted；**下列正常超时路径一律以
  `PendingDeadline`（非 RX 事件，不清信任量）进 `enter_fault`**：

  | 匹配到的记录 | 处置 |
  |---|---|
  | `active.valid ∧ seq` | 事务边界中止（`AbortedByTimeout`）→ `enter_fault(CommFault, PendingDeadline, PendingTimeout)` + 收敛（`rx_trust` 不变 → 可信 all-off 可达 `TorqueOffConfirmed`） |
  | `completed.valid ∧ seq`，`Enable(false)` ∧ `success` | **仅豁免重复物理收敛**（不重发 `Safety`、不 `Confirm`）；**不豁免 Fault / gen / ack**：仍 `enter_fault(CommFault, PendingDeadline, PendingTimeout)` 发布 gen，创建的 `Fault` 轮次跳过 `StopIssue` / `Confirm`，以 N 个电机逐项 status ACK 全数成功（`write_torque_enable_with_retry(false)`，`adapter.rs:352-402`）为观测经封顶表提交：`evidence_trusted`（= `rx_trust`，未被本路径清零）→ `TorqueOffConfirmed`，否则 `AdapterUncertain` → `AwaitAck(gen)`；IPC 预提交的 `Fault(CommFault)` 由该 gen 对账；AC-4i 锁 |
  | `completed.valid ∧ seq`，其它 action × 结果 | `enter_fault(CommFault, PendingDeadline, PendingTimeout)`（含收敛） |
  | 未消费 pending seq | 消费时 0 IO 回 `AbortedByTimeout` + `enter_fault(CommFault, PendingDeadline, PendingTimeout)`（无效果，收敛为幂等广播） |
  | EStop 已标取消的 pending，精确匹配 Submitted seq | 同未消费 pending：零原 action IO，以 PendingDeadline 收敛；独立 EStop 请求仍须 claim / 合并，不因 timeout 结果吞掉急停义务 |
  | `Unsubmitted`，原 action 尚未提交 | 不调用原 action、不构造假 seq；`enter_fault(CommFault, PendingDeadline, PendingTimeout)` + 完整物理收敛 + gen/ACK；不置 EStop latch、不清信任量。持续 Busy 若还残留非本请求的 pending / active，先按单 pending 不变量失败记 `InvariantViolation`，阻止该残留发起普通 IO，并以 `Unknown` 收紧信任后收敛，不误当成本请求完成 |
  | **Submitted 不匹配 / 非法请求编码** | **不变量违反（fail-closed）**：Diag `InvariantViolation{abort_request, active, completed}` + `enter_fault(CommFault, Unknown, InvariantViolation)`（`Unknown` ∈ `TRUST_CLEARING`，清信任量）；"不可能"由 AC-4c 证明，运行时仍 fail-closed |

  timeout 消费后仅以 `compare_exchange(observed_request, ABORT_NONE, AcqRel, Acquire)`
  清本次请求，不能普通 store 覆盖其间到达的不同请求。每个请求的收敛与 gen/ACK
  责任登记完成后才清；同一 IPC pending 超时仅发布一次，不在每 tick 重复发布。

  **已取消 pending 的记录所有权（S-40）**：原槽保持非空，`request_normal` 既有
  `compare_exchange(0, payload)` 继续返回 Busy，不能覆盖被取消请求。serial 唯一
  消费者剥 tag 并严格校验 payload；匹配 Submitted timeout 按上表退场。未匹配
  timeout 的合法取消请求仅零执行完成 `PreemptedByEmergency`，先写
  `completed_effect{success=false, exact action/arg/seq}` 再发布 `last_completed`；
  随后迟到的同 seq timeout 可以精确命中完成记录。取消 tag 自身不创建 Emergency、
  不 claim 请求、不写 policy / gen；即使发布者停在 tag 成功、权威 CAS 之前也如此。
  急停义务只来自 `safety_word` 请求的 claim。Unsubmitted 遇到非本请求的取消槽
  仍属不变量残留，按 Unknown，不因取消标记豁免。IPC 收到取消结果按抢占结果
  处理，不误映射成普通 AdapterErr；EStop notice 到达后独立服务其状态与 latch 责任。

- `T_pending` 保持 1 s；client 栈 0 改。
- **liveness（lease 化）**：`liveness_counter` 在合法 tick 推进时 +1；本 tick 有 IO 时，
  任一调用越过其当前 phase deadline 就不得靠 tick 尾部增加进度，后续调用重设
  deadline 不能抹掉此前越界。无 IO 的 Idle-inhibited / AwaitAck / 终态等待正常
  计进度，不继承上一个 tick 的过期 lease；不新增整轮绝对预算。`checkpoint()` 仅在一次
  transport 调用**已返回**且 `now < phase_deadline` 时 +1（normal action：开始 +
  `B_action_retire`；启动 Connect：开始 + `T_connect(N)`；收敛：按 D4 phase 预算）；
  越界冻结 → 观察器（`s0_daemon.rs:1083` 改接本计数）≤ 1 s 判停滞 → 停喂 →
  `WatchdogSec=2s` 重启。"持续 checkpoint 但越过上界不退场"的错误实现因此必被停喂
  （AC-16 反向）。pre-READY 期间 watchdog 未激活（S-18），启动期由 D9
  `boot_supervisor` 兜底。

### D4（Q4）路由矩阵 + 统一收敛 phase 机（三上下文 + 启动阶段 + `estop_bound` 义务 + gen）+ 出口矩阵 + `enter_fault` 分流 + `evidence_trusted` 封顶

- **inhibit 位集** `inhibit: AtomicU8` = {`COMM_FAULT`（仅 CommFault）,
  `ADAPTER_FAULT`（仅 AdapterErr）, `STOP_FAILED`（仅 SafetyStopFailed）,
  `CONVERGING`, `FAULT_PENDING`, `BOOT`（spawn 起置位，启动出口清）}；任一位 →
  不消费 normal action（pending 立即回对应 Err）、不 `write_command`（无论
  policy）、不 `read_state`（快照沿用 last-known-good + 旧戳）。
- **信任量**：
  - `session_clean`：进程内自首个启动事务前置 true；任一 `TRUST_CLEARING` kind →
    false，单向；
  - `rx_trust`：初值 false；**唯一置 true 点** = `Preflight` 上下文 `Confirm` 得
    all-off 且 `evidence_trusted(Preflight)`；任一 `TRUST_CLEARING` kind → false，
    单向；
  - **`PendingDeadline` 不清任何信任量**（IPC 本地 deadline，无 RX 失步证据）；
  - `evidence_trusted(ctx) := match ctx { Preflight => authorized ∧ session_clean,
    Fault | Emergency => rx_trust }`。
- **权威封顶表**（serial 侧提交 `safety_word.state` 处，CAS 保留请求位与 `normal_io_active`；判据
  = `evidence_trusted(ctx)`；观测来源 = `Confirm` probe，或 D3 豁免路径的 N 个 ACK）：

  | 观测 | 可信 | 不可信 |
  |---|---|---|
  | all-off | `TorqueOffConfirmed`（`Preflight` 下先 `rx_trust := true` 再提交） | `AdapterUncertain`（Diag `UntrustedProbe{all_off}`） |
  | some-on（attempt < 5） | 保持 `StopCommanded` | 同左 |
  | some-on（attempt ≥ 5） | `StopFailed` | `StopFailed`（危险方向原样） |
  | Err（attempt ≥ 5，无 some-on） | `AdapterUncertain` | `AdapterUncertain` |

  pub 投影（`pub_thread.rs:394`，SerialRT 读 `safety_word.state`）与 ClearEStop 门
  （`clear_estop_action.rs:97` `actual != TorqueOffConfirmed` 即拒）无需遮蔽即
  自动正确。**权威值由 serial 侧 CAS 提交；IPC 侧唯一写点 = `estop_monotonic()`
  的单次 CAS，只允许 `{TorqueUnknown, TorqueOn, TorqueOffConfirmed} →
  StopCommanded`，永不降级 `StopFailed` / `AdapterUncertain`，且 CAS 之后零写
  （S-22 / S-26）。**
  **`Preflight` 发布顺序（锁死）**：`authorized` 已消费 → probe all-off ∧
  `session_clean` → `rx_trust := true`（Release）→ `safety_word.state :=
  TorqueOffConfirmed`（CAS）→ `Done{Passed}`；任一步失败 → `rx_trust` 保持
  false、不提交 confirmed。
- **路由矩阵**（全定义；"不可达"格 = 条件 + 锁定测试）：

  | class \ kind | RX Timeout / FrameCorrupt / ResponseIdMismatch / PeerNoResponse | TX timeout / Disconnected / IoRead / IoWrite / DiscardFailed | `StatusError`（设备状态字节） | `InvalidParameter` / 永久配置 | Aborted(Emergency) | Aborted(Timeout) |
  |---|---|---|---|---|---|---|
  | `PeriodicRead` | `enter_fault(CommFault, kind, Periodic)`；信任量清零 | 同左（`DiscardFailed` 不在此类产生） | Diag `DeviceStatus` + `enter_fault(AdapterErr, byte, Periodic)` | `enter_fault(AdapterErr)` | 仅事务入口 acquire 被 EStop 拒绝：零 IO、立即 claim，不计通信错误、不刷新 snapshot；已 acquire 的事务不可拆 | 不可达（周期事务不匹配 timeout 请求） |
  | `PeriodicWrite` | 不可达（无读；锁测：Mock 无法向该类注入 RX 错误） | `enter_fault(CommFault)`；信任量清零 | 不可达（无响应） | `enter_fault(AdapterErr)` | 同上，入口拒绝后本 tick 不再启动其它 normal IO | 不可达 |
  | `NormalEffectful` | `enter_fault(CommFault, kind, NormalAction)` + 收敛；信任量清零 | 同左 | `enter_fault(AdapterErr)` + 收敛 | `enter_fault(AdapterErr)` + 收敛 | `PreemptedByEmergency`（emergency 路径自行断扭，不进 fault） | `AbortedByTimeout` → `enter_fault(CommFault, PendingDeadline, PendingTimeout)` + 收敛（信任量不变） |
  | `Connect`（启动） | `ConnectOutcome::Err(kind)`；信任量清零 → 转 `Fault{gen, ConnectFailed}`（code CommFault） | 同左 | `ConnectOutcome::Err` → 转 `Fault{gen, ConnectFailed}`（code AdapterErr，信任量不变） | `HandshakeWrongModel` / 校准错配 → 同左（AdapterErr） | 不可达（不受抢占；EStop 按幂等合并，锁测） | 不可达（启动期无 pending normal；锁测） |
  | `Safety`（广播，write-only） | 不可达（无读） | `StopRetry`（耗尽 → `SafetyStopFailed{StopNotDelivered{kind}}`）；信任量清零 | 不可达 | `StopRetry`（参数/永久错误 = 本次未交付） | 不可抢占（锁测） | 不可抢占（锁测） |
  | `Confirm`（含 hygiene 步） | attempt++；信任量清零；`Emergency`（gen None）→ **发布 CommFault gen**（mailbox + inhibit，轮次 `gen := Some`），上下文 / phase / attempt / `estop_bound` 不变；`Fault` → 按单调升级；`Preflight` → 不调 `enter_fault`，出口由出口矩阵决定 | 同左（`DiscardFailed` = hygiene 步 ioctl / read 失败，仍继续 probe） | attempt++ + Diag `DeviceStatus`（帧完整，信任量不变） | attempt++ + Diag `InvariantViolation`（固定 addr/len 下不可达；锁测：Mock 返回 `InvalidParameter` 不 panic、按 Err 计） | 不可抢占（锁测） | 不可抢占（锁测） |

  **无阈值**：首次即定级；`comm_level` 仅观测（D6）。
- **`ConvergeContext`**（进入 phase 前选定；同一 bus owner 规则；`estop_bound`
  与 `gen` 另记，出口见出口矩阵；一切"升级 `SafetyStopFailed`"均指进入
  `SafetyStopFailedAwaitAck(gen)`）：

  | 上下文 | 创建条件 | gen | 成功出口（¬estop_bound） | 失败出口 |
  |---|---|---|---|---|
  | `Fault` | `enter_fault(AdapterErr / CommFault)`（含 D3 豁免路径：跳过 `StopIssue` / `Confirm`）；或启动：marker Dirty → `Fault{gen, BootUntrusted}`、Connect 失败 → `Fault{gen, ConnectFailed}`、Preflight 未通过 → `Fault{gen, PreflightFailed}`、deadline → `Fault{gen, PreflightTimeout}` | 有 | `Confirm`（或豁免观测）→ `AwaitAck` | 升级 `SafetyStopFailed{..}`（新 gen）→ `SafetyStopFailedAwaitAck` |
  | `Emergency` | 非收敛态（含 `Idle(Fault 粘滞)`）首次 EStop 请求（claim `serial_estop_pending`）；`estop_bound = true` | 初 None；轮内 `enter_fault` 到达 → `Some`（code 按来者） | `Confirm` 可信 all-off → gen None：`Done`（latch 语义既有）；gen Some：`AwaitAck(gen)` | 升级 `SafetyStopFailed{..}`（gen）→ `SafetyStopFailedAwaitAck`；`enter_fault` 到达 → 只发布 gen，义务不变 |
  | `Preflight` | 启动：`authorized` ∧ 串口已 open | 无 | `Confirm` 可信 all-off → `Done{Passed}` | 转 `Fault{gen, ConnectFailed / PreflightFailed}` 的 `AwaitAck`（不重跑）；some-on ≥ 5 → 升级 `SafetyStopFailed`（gen）→ `SafetyStopFailedAwaitAck` |

- **phase 表**（每 tick 推进一步；attempt 计数属于当前轮次）：

  | phase | owner | 动作 | 成功 → | 失败 → | 预算 |
  |---|---|---|---|---|---|
  | `StopIssue`（attempt 1） | converge | 置 `CONVERGING`、`safety_word.state := StopCommanded`（CAS）；仅 `estop_bound` 时 `tighten_estop_policy()`（D13；保留 `Disabled`，非 EStop 的 Fault 轮零 policy 写）；下一事务边界发 `Safety` 广播（`D_call` 已安装，D2） | 启动首轮：`Connect`；运行期或 AwaitAck 重开：`Confirm` | 写错误 → `StopRetry` | ≤ `D_call` |
  | `StopRetry`（attempt 2..3） | converge | 间隔 1 tick 重发；`MAX_STOP_ATTEMPTS = 3` | 同上 | 第 3 次仍失败 → 升级 `SafetyStopFailed{StopNotDelivered}` → `SafetyStopFailedAwaitAck` | ≤ 2 × (tick + `D_call`) |
  | `Connect`（仅启动） | boot | `adapter.connect()` = N × ping（`BusOpClass::Connect`）；entries 已由 `new()` 预填 | `Confirm` | Err → 上下文转 `Fault{gen, ConnectFailed}` → 仍进 `Confirm` | ≤ 4N·`D_call` |
  | `Confirm` | confirm | ¬`evidence_trusted(ctx)` 先 `drain_until_quiet`；probe；封顶表提交；attempt ≤ 5 | 出口矩阵 | 出口矩阵 | 每次 ≤ drain 150 + probe 100 ms + tick |
  | `AwaitAck`（**gen = Some 的非 SafetyStopFailed 轮次**） | 无 IO | 本轮 `completed_safety = Some`；先处理已 claim 的 EStop（规则 (d)），未重开时才检查 `fault_ack == 当前 gen`（Acquire）；phase 内零 IO | 匹配 ack 且物理轮已完成 → 清 `FAULT_PENDING`（启动：并清 `BOOT`）→ `Idle(Fault 粘滞，原因位保持)` | IPC 停摆 → 一直等；EStop 可按 (d) 离开本 phase 重开物理轮（启动期仍由原截止收口） | — |
  | **`SafetyStopFailedAwaitAck{gen, detail}`** | 无 IO | 进入时按**发布顺序**：① 权威 `safety_word.state`（`StopFailed` / `AdapterUncertain`，CAS 保留请求位与 `normal_io_active`）→ ② `STOP_FAILED` + `FAULT_PENDING`（Release）→ ③ mailbox 词 {gen+1, SafetyStopFailed, detail, origin}（Release，单 u64）；**零总线 IO、不改 latch**；等 `fault_ack == gen`（Acquire） | 清 `FAULT_PENDING`（启动：并清 `BOOT`）→ 终态 `SafetyStopFailed` | IPC 停摆 → 一直等（启动期由 `boot_supervisor` 收口） | — |
  | 终态 `SafetyStopFailed` | 无 IO | `STOP_FAILED` 保持；**零 IO**；粘滞至重启 | — | — | — |

  启动序列 = 上下文选定 → `StopIssue`(+`StopRetry`) → `Connect` → `Confirm` →
  出口；**停命令是本启动首个总线 IO**（S-14 预填 + S-15 安装使其不依赖握手）。
  Dirty 启动缺省只跑这一轮；每物理轮 `Safety` 写 ≤ 3。已完成的 `AwaitAck` 收到
  新 EStop 时按 (d) 重开，为明确例外；不重复 Connect、不刷新启动绝对截止。
- **出口矩阵**（`Confirm` 退出；attempt < 5 的 some-on / Err = 留在 `Confirm`；
  列按 `estop_bound` 与上下文划分，**无重叠**；**gen = Some 时凡"Done"处一律改
  `AwaitAck(gen)`**；"升级 `SafetyStopFailed`" = 进入 `SafetyStopFailedAwaitAck(gen)`
  等 ack 后终态）：

  | `ConfirmOutcome` | `Fault`（¬estop_bound） | `Preflight`（¬estop_bound） | **estop_bound**（`Emergency`，或 `Fault` / `Preflight` 合并了 EStop） |
  |---|---|---|---|
  | `AllOffTrusted`（all-off ∧ 可信） | `TorqueOffConfirmed` → `AwaitAck` | `rx_trust := true` → `TorqueOffConfirmed` → `Done{Passed}` | `TorqueOffConfirmed` → 义务已满足：gen None → `Done`（`Emergency`）/ `Done{Passed}`（`Preflight`）；gen Some → `AwaitAck(gen)`（`estop_bound` 保留） |
  | `AllOffUntrusted` | `AdapterUncertain`（封顶）→ `AwaitAck` | `AdapterUncertain` → 转 `Fault{gen, PreflightFailed}` → `AwaitAck`（不重跑） | 禁止 `Done` / `AwaitAck`：升级 `SafetyStopFailed{AdapterUncertain{kind}}`（gen）→ `SafetyStopFailedAwaitAck` |
  | `SomeOnExhausted`（attempt ≥ 5 ∧ 曾 some-on） | 升级 `SafetyStopFailed{StopFailed{still_on}}`（新 gen）→ `SafetyStopFailedAwaitAck` | 同左（gen） | 同左（gen） |
  | `ErrExhausted`（attempt ≥ 5 ∧ 无 some-on） | `AdapterUncertain` → `AwaitAck` | `AdapterUncertain` → 转 `Fault{gen, PreflightFailed}` → `AwaitAck` | 升级 `SafetyStopFailed{AdapterUncertain{kind}}`（gen）→ `SafetyStopFailedAwaitAck` |

  `kind` = 最后一次 Err 的 `CommErrorKind`（非通信 Err，如 `StatusError`，取
  `Unknown` 并由 Diag `DeviceStatus` 携字节）。estop_bound 轮发布过 CommFault gen
  （RX kind）后 `rx_trust` 已为 false，`AllOffTrusted` 不可达；发布过 `AdapterErr`
  gen 或 `PendingDeadline` gen（均不清 `rx_trust`）后 `AllOffTrusted` **可达** →
  `AwaitAck(gen)`（锁测 AC-19(vi)）。
  **完成记录**：进入非 SSF `AwaitAck` 前，serial 将封顶后的完成结果存入
  `round.completed_safety`：可信 all-off → `AllOffTrusted`；不可信 all-off 或
  Err 耗尽 → `AdapterUncertain{kind}`。D3 豁免路径的 N 个 ACK 也经同一映射。
  启动 deadline 跳过剩余 Confirm 的出口直接记录 `AdapterUncertain{Unknown}`，
  不得以 None 进入 AwaitAck；这是无充分确认的完成结果，不宣称观察到 all-off。
  `kind` 沿用出口的错误信息，无通信错误时取 `Unknown`。此记录与本轮 gen 绑定，
  只在 serial 开始下一物理轮时清 None；EStop 原语不覆盖它。
- **EStop 幂等合并（serial 侧；请求来源 = `safety_word.serial_estop_pending`；检查点
  = tick 头、每个 normal 事务起点的 acquire-CAS（`pending == 1` → 拒绝启动）、sleep
  分片；见位 → **处理开始时** CAS claim（清 0），再按下列规则处理；处理期间的
  再次置位 = 新逻辑请求，下一检查点再 claim）**。按 phase 匹配，(d)/(e) 亦适用
  启动，优先于 (b)，五分支无重叠：
  - (a) `Idle` 非收敛（含 `Idle(Fault 粘滞)`）→ 创建 `Emergency`（`gen = None`、
    `estop_bound = true`、`completed_safety = None`）进 `StopIssue`；仅此分支新建
    无 gen 的轮次。
  - (b) 启动的 `StopIssue` / `StopRetry` / `Connect` / `Confirm` → 置 executor
    latch + Diag `EStopMerged{phase}` + **本轮 `estop_bound := true`**，不中止
    Connect，不重置 attempt、不新增写、不另建 `Emergency`。
  - (c) 运行期 `StopIssue` / `StopRetry` / `Confirm`（任一上下文）→ 置 latch +
    Diag + `estop_bound := true`，复用当前收敛，不重置 attempt、不新增写。
  - (d) `AwaitAck(n)` → latch + Diag + `estop_bound := true`，**只按 serial 本轮
    `completed_safety` 判定，不读当前 word 反推旧状态，也不依赖发布者的 outcome**：
    - `Some(AllOffTrusted)` → **保留原 ctx、`gen = Some(n)`、fault code / detail /
      origin、原因位、`FAULT_PENDING` 与启动 `BOOT`**，清 `completed_safety`，
      重置物理 attempt / `some_on_seen`，重开 `StopIssue → Confirm`（不经 (a)，
      不新发 gen，不再 Connect）。再次可信 all-off 后记录完成结果、仍进入
      **`AwaitAck(n)`**；ACK 未到则一直等。ack(n) 即使在重开前或物理阶段已到，
      也只能在再次完成后对账，不能提前清位或到 `Done` / `Idle`。重开中发生升级
      则遵守新 gen 规则，旧 ack(n) 不清新义务。
    - `Some(AdapterUncertain{kind})` → **零 IO 升级**至
      `SafetyStopFailedAwaitAck{AdapterUncertain{kind}}`（origin
      `EStopEscalation`），新 gen；旧 ack 无效。
    - `None` → 内部不变量违例，Diag `InvariantViolation` + 零 IO 升级
      `SafetyStopFailedAwaitAck{AdapterUncertain{Unknown}}`，禁止无证据重开。
    启动重开保留 `t_spawn`、`T_boot(N)`、`T_abs` 与首次出口起算的 ACK 截止；
    READY 等本轮物理完成与对应 ACK 两者，不以已有 ack 单独放行。
  - (e) `SafetyStopFailedAwaitAck` / 终态 → 仅 Diag，**零 IO、零 policy 写、状态
    不变**；无论 IPC 与 serial 的处理顺序如何，均保持此不变量。
- **`enter_fault(code, detail, origin)`** + **单调升级**：severity `AdapterErr(1) <
  CommFault(2) < SafetyStopFailed(3)`。**code / gen 只改 `gen` 与 inhibit 位，
  永不改 `estop_bound`；`gen = Some` 后本轮必经 ack 等待；信任量只按 `detail`
  kind ∈ `TRUST_CLEARING` 清零。**
  - 无活动 fault、无活动收敛：`AdapterErr` / `CommFault` → 置原因位 +
    `FAULT_PENDING`、mailbox `gen+1`、创建 `Fault{gen}` 进 `StopIssue`（D3 豁免
    路径：跳过 `StopIssue` / `Confirm`，直接经封顶表提交观测 → `AwaitAck`）；
    `SafetyStopFailed` → `SafetyStopFailedAwaitAck{gen+1, detail}`。
  - 活动 `Emergency` 轮（gen None）：置原因位 + `FAULT_PENDING`、`gen+1`（轮次
    `gen := Some`），**保留上下文 `Emergency`、phase、attempt、`estop_bound`**（停
    已发出，不重跑 `StopIssue`），出口按 estop_bound 列且凡 Done 处改
    `AwaitAck(gen)`；`SafetyStopFailed` → `SafetyStopFailedAwaitAck`。
  - 活动 `Preflight`：Confirm 类错误不经此入口（出口矩阵转 `Fault{gen,
    PreflightFailed}`）；`Connect` 失败由 boot 逻辑转 `Fault{gen, ConnectFailed}`；
    `Safety` 写耗尽 → `SafetyStopFailedAwaitAck{gen, StopNotDelivered}`；daemon
    deadline → `PreflightTimeout`。
  - 已有活动 fault：`severity ≤ 当前` → 只记 Diag / 更新 detail；`severity > 当前`
    → 立即切入：`CommFault` 覆盖 `AdapterErr` → 置 `COMM_FAULT`、信任量按 kind、
    **phase 自 `StopIssue` 重跑（attempt 重置；单调性保证至多一次；`estop_bound`
    保留）**、新 gen、`FAULT_PENDING` 重新置位；`SafetyStopFailed` →
    `SafetyStopFailedAwaitAck`、新 gen。旧 ack 不清任何位。
- **mailbox（独立 level-triggered 原子槽，不是事件；单 u64 词）**：`fault_mailbox:
  AtomicU64` = {gen:32 | code:8 | detail:8 | origin:8}（**gen 与 detail / code /
  origin 原子绑定，不存在"看到 detail 看不到 gen"**）+ `fault_ack: AtomicU32`。
  gen 0 空、自 1 起单调、达 u32::MAX − 1 后饱和 fail-closed（不再发布新代次，
  Fault 粘滞 + Diag）。**serial 发布顺序（所有 code 统一）**：① 权威
  `safety_word.state` / 记录（CAS）→ ② inhibit 位（Release）→ ③ mailbox 词
  （Release）；IPC Acquire 读词后再读 `safety_word` / inhibit 必 ≥ ① / ②。**唯一
  ack 所有者 = IPC**：code == SafetyStopFailed 的代次在 Step 1
  （`EmergencyBatch.ssf` 快照）commit 后 `fault_ack := 快照 gen`（Release）；其它
  code 在**每 tick 必经**的 Step 1b Acquire 读一次完整词，`gen != fault_ack` →
  `commit_fault(code)` + `set_policy(Disabled)` + 清 pending +
  `fault_ack := 读到的 gen`。同等或更低 code 的 commit 为 no-op，不降级当前 Fault。
  Step 1b 新看到的 SSF 不走非 SSF 分支、不提前 ack，交下一 tick Step 0 / 1；
  Step 1 已处理的 SSF 不重复提交。IPC 只 ack 自己读到的 gen；serial 清 `FAULT_PENDING`
  只在 `fault_ack == 当前 gen`（Acquire）→ 旧 ack 永不清新代次（AC-8(i)）。**不经
  任何 lane、不受 lane 占用、pending 在飞或连续 EStop 影响**：Step 1 命中后先完成
  Step 1b，再结束普通处理。**IPC 持续推进时 ≤ 2 ipc tick**（SSF 若晚于 Step 0
  快照发布，计入下一 tick；tick 数不替代 fsync / 调度墙钟耗时）。IPC 若已由
  `last_completed` 路径提交同等或
  更高 Fault，仍 ack（状态机 no-op）；超时预提交后收到 gen 仍 ack（含 D3 豁免
  路径）。
- **confirm 契约修订**（S-10）：Err 不再首错即终态；上限 5；封顶表；终态按"是否
  出现 some-on"、`estop_bound` 与 gen；wire 三态枚举不变。CHANGELOG 登记。
- `adapter_connected` 推导见 D9；不置 `safety_latched`、不写 estop latch、不动
  policy（§6.7 #3）；**SafetyStopFailed 亦不置 `safety_latched`**（S-22 行为变化
  登记）。

### D5（F3 三元契约）β-scoped：全部 effectful action

① Ack 超时 = IPC 预提交 `Fault(CommFault)` + 请求 retire；serial **一律**补 mailbox
代次（kind `PendingDeadline`；含 `Enable(false)` 豁免路径）；② 副作用 ≤ 一个不可拆
事务 + 已完成子步；③ 收敛 = D3 矩阵 + D4 `enter_fault`：任何 normal action
`Comm(_)` → `CommFault`；effectful action（`Enable` 任一方向 / `SetMode` /
`HoldCurrent` / `Connect` / `Disconnect`）非 Ok → `AdapterErr` + 收敛（端口状态
未知即视为可能带扭）；`Enable(false)` 全 ACK 成功为唯一**物理收敛**豁免（Fault /
gen / ack 不豁免）。修 S-6。IPC 侧"不兜底 `safety_stop()`"注释保持；client
finalizer 保留为第二层。

### D6（Q14）无阈值定级 + `comm_level` 仅观测

任何通信错误按 D4 路由矩阵首次即定级（协议无事务身份，任何失步后 RX 不可信，
"容忍再试"无意义；"快速错误也须触发"天然满足）。`comm_level`（L0 / L1 = 曾有 Diag
级设备状态 / L2 = 活动 CommFault）仅供 metrics / 日志。生命周期：Safe / Ready /
Enabled 一律 `Fault(CommFault)`（新边 Safe→Fault / Ready→Fault 按 v9 `* →
Fault(SafetyStopFailed)` 先例登记 freeze diff）；退出仅 ClearError 守门（失步
会话内不可满足 → 实际 = D9 程序 + 重启）或重启；**SerialRT 下 EStop 不清 Fault**
（D13 状态机优先规则）。

### D7（Q6 / Q9 / Q12）typed 错误分类、一字节编码、`CommFault`

- `CommErrorKind`（`Copy` `#[repr(u8)]`，0..=0x3E，`Unknown = 0x3F`）：`Timeout`、
  `Disconnected`、`IoRead`、`IoWrite`（含 TX timeout）、`OpenFailed`、
  `FrameCorrupt`、`ResponseIdMismatch`、`PeerNoResponse`、`DiscardFailed`、
  **`PendingDeadline`**（IPC 本地 pending deadline；非 RX 事件；∉ `TRUST_CLEARING`）。
  `RobotError::Comm(kind)` + `RobotError::Aborted(AbortReason)`（additive）。
  归属：`OpenFailed` **仅**启动 open（`factory.rs:73`）失败的分类 → daemon 退 1，
  **不进路由矩阵、不进 mailbox**；`DiscardFailed` **仅**在 `Confirm` hygiene 步
  产生，按 `Confirm` 行连接类列处理（attempt++、信任量清零、仍 probe）；
  `PendingDeadline` **仅**由 D3 timeout lane 产生（origin `PendingTimeout`）。
- `FeetechError` 拆 `Handshake(String)` → `HandshakeMissing { id }` /
  `HandshakeWrongModel { id, got }` / `ResponseIdMismatch { expected, got }`；
  `StatusError(u8)` = 设备状态字节（非通信）。
- 映射与处置：

  | 类 | FeetechError | RobotError | 处置 |
  |---|---|---|---|
  | 连接 / IO | `NotConnected`、`TransportRead/Write`、TX timeout | `Comm(Disconnected / IoRead / IoWrite)` | 路由矩阵（CommFault）；信任量清零 |
  | 启动 open | `TransportOpen` | `Comm(OpenFailed)` | daemon 退 1（消费前、零总线 IO）；不进矩阵 |
  | 超时 / 坏帧 / 错位 | `TransportTimeout`、`BadPacket*`、`BadChecksum`、`PacketTooShort`、`ResponseIdMismatch` | `Comm(Timeout / FrameCorrupt / ResponseIdMismatch)` | 路由矩阵；信任量清零 |
  | 对端无应答 | `PingNoResponse`、`HandshakeMissing` | `Comm(PeerNoResponse)` | 启动 Connect 失败（CommFault）；信任量清零 |
  | hygiene | `discard_input` / drain 的 ioctl / read 失败 | `Comm(DiscardFailed)` | `Confirm` 行连接类列 |
  | 本地 deadline | —（IPC 侧，无 FeetechError） | `Comm(PendingDeadline)` | D3 retire 矩阵；信任量不变 |
  | 设备状态 | `StatusError(u8)` | `HardwareFault` + Diag `DeviceStatus{motor, byte}` | `AdapterErr`（候选 E 的 D14 范围） |
  | 永久配置 | `PingModelMismatch`、`HandshakeWrongModel`、Calibration*、Unknown*、Invalid*、Magnitude*、ByteLength* | `InvalidParameter(..)` | `AdapterErr`；Fault 粘滞至重启 |
  | 硬件语义 | `SyncGoalReadbackMismatch` | `HardwareFault(..)` | `AdapterErr` + 收敛 |
  | 抢占 / 占用 | `Aborted` / `AlreadyConnected` | `Aborted(reason)` / `Busy` | 类型化（D3） |

- `AdapterErr` 编码（既有 `Other=0..SafetyLatched=5`）：`AbortedByTimeout = 6`；
  `Comm(kind) = 0x40 | kind`（0x40..=0x7F，含 `PendingDeadline`）；`from_u8`：
  未知 kind → `Comm(Unknown)`，其它未知码 → `Other`（AC-18）。
- **mailbox detail 命名空间**（按 code）：`CommFault` → `CommErrorKind`；
  `AdapterErr` → 设备状态字节或 0；`SafetyStopFailed` → `SafetyStopFailDetail`
  = 高 2 位变体 {0 `StopFailed`, 1 `AdapterUncertain`, 2 `StopNotDelivered`} +
  低 6 位载荷（`StopFailed` → still-on 台数；其余 → `CommErrorKind`）。
- **origin**（8 位）：`Periodic`、`NormalAction`、`PendingTimeout`、
  `InvariantViolation`、`Confirm`、`StopNotDelivered`、`BootUntrusted`、
  `ConnectFailed`、`PreflightFailed`、`PreflightTimeout`、`EStopEscalation`
  （合并规则 (d) 的零 IO 升级）。
- `FaultCode::CommFault` 新码；wire `S0FaultCode` 既有 enum +1（`CommFault = 7`）；
  `pub_thread.rs:363-370` 穷尽映射增 arm；proto 透传 0 改。Q9：normal action
  `Comm(_)` → 直接定级 `CommFault`。

### D8（Q7）RX hygiene（非边界）

- `drain_until_quiet(quiet_required = 50 ms, absolute_deadline = 150 ms)`：以 1 ms
  段 `read` 丢弃字节并计数；quiet 计时逐字节重置；**absolute_deadline 永不重置**，
  到期返回 `DeadlineExceeded`（仍进入下一步，只记 Diag `Drain{bytes,
  deadline_hit}`）；read 本身 Err → `DiscardFailed`（D7 归属）。用途：证据不可信下
  `Confirm` 探测前的 hygiene，**不构成任何隔离 / 边界宣称**（S-11）。
- `discard_input` 保留为 hygiene 原语（Real = `serialport::clear(Input)`；Mock
  no-op）。帧级重同步 / 事务身份 = 紧后候选。

### D9（Q8）显式双状态 marker（两阶段写 + 持久屏障 + canonical 前置）+ 非阻塞独占锁（errno 分类）+ `SerialTrustPolicy` + 启动序列（`boot_supervisor` 自 `main()` 首语句 + READY 真送达提交点）+ `adapter_connected` 推导

- **文件**（默认目录 = `--latch-path` 的父目录；`--rx-trust-dir` 覆盖，供测试）：
  - `rx_trust_marker`（primary）与 `rx_trust_marker.bak`：内容**恰为** `1\n`
    （Dirty/Untrusted，磁盘常态与首次部署值）或 `0\n`（AuthorizedOnce）；
  - `rx_trust.lock`：稳定锁文件，**永不 rename/delete**；内容 = 持有者 pid（仅诊断）。
- **两阶段 marker 写 `write_marker_two_phase(path, byte) -> Result<(), MarkerWriteError>`**
  （S-13；`rx_trust` 模块新 helper，不改 `estop_latch` 语义）：phase 1 = tmp 写 +
  file fsync + `rename(2)`，任一失败 → `NotCommitted(io::Error)`（tmp 已删，目标
  未变）；phase 2 = 父目录 open + fsync，失败 → `CommittedNotDurable(io::Error)`。
  **线性化点 = rename 返回成功**（本地 FS 原子、返回值权威、命名空间立即可见）；
  持久性只由 phase 2 保证。测试 seam `MarkerFs`（rename / dir fsync 可注入失败、
  可注入永久阻塞，并可**模拟掉电回退**：撤销未 fsync 的 rename）。
- **屏障规则**：truth table 只在 `0/0` 授权，故 **untrusted 屏障 = 任一份持久为
  `1\n`**。约定：
  - 放宽（`authorize`）：**前置 = canonical Dirty `1/1`**（两份存在且恰为 `1\n`），
    backup-first `0\n`、primary-last `0\n`——屏障最后移除；非 canonical 初态一律
    拒绝（见 authorize）；
  - 收紧（consume / `revoke`）：**backup-first `1\n` 两阶段完整持久 = 屏障**，未
    持久不宣称收紧；primary 随后规范化。
  - 掉电回退分析：authorize 自 `1/1` 出发，任一步失败或回退后命名空间恒有一份为
    `1`（backup 回退 `0→1`、primary 未动或回退 `0→1`），永不落入 `0/0`，除非 primary
    rename 已成功（= 已生效，退 0 / 3）；收紧后 primary 回退 `1→0` 只在 `.bak = 1`
    已持久时才可能发生，得 `0/1` = untrusted；`.bak` 未持久时收紧不成立（consume
    不 `authorized`、revoke 退 2）。
- **锁协议（errno 分类）**：daemon 启动（`boot_supervisor`、logging init、CLI
  parse、config resolve 之后，`ensure_latch_parent` 之后、读 latch / open 串口 /
  读 marker 之前）`open(rx_trust.lock)` → `fcntl(F_SETFD, FD_CLOEXEC)` →
  `flock(LOCK_EX | LOCK_NB)`（`EINTR` 重试）。分类：`flock` 得 `EWOULDBLOCK` /
  `EAGAIN` → 日志"lock held by pid …" + **退 87**（并发实例；零 marker / 零总线
  副作用）；open / `fcntl` / `flock` 其它 errno（`EIO` / `EPERM` / `ENOLCK` / `ENOSPC`
  …）→ **退 1**（存储 / 运行错误，可重启）。**不照搬** `deploy/counter.rs:231-256`
  （其把所有 flock 失败映成 Conflict）。持有到进程退出（fd 不关闭）。`rx-trust`
  子命令同锁、同分类（并发 → 87；其它 → 1）。unit `RestartPreventExitStatus=86 87`。
  查询 systemd / 进程列表不替代锁。所有计划都取锁。
- **`SerialTrustPolicy`（穷尽表，S-17 / S-19）**：daemon `build_adapter` 按
  `[[hardware]] adapter` 注册名解析（`s0_daemon.rs:1235`）：

  | adapter kind | RtClass | policy | 备注 |
  |---|---|---|---|
  | `feetech_sts3215`（`factory.rs:158`） | SerialRT | `Marker` | 生产；`--rx-trust-policy` 指定任何值 → 退 2 |
  | `mock_serial_rt`（`mock/src/lib.rs:413`） | SerialRT | `Static`（缺省） | `--rx-trust-policy marker` 可改为 `Marker`（测试） |
  | `mock_hard_rt`（`:278`） | HardRT | 无 policy | 无 marker；`--rx-trust-policy` 指定 → 退 2 |
  | 其它 SerialRT kind | SerialRT | **拒绝** | fail-closed 退 2（配置错误） |

  `Marker`：执行 marker 消费与 truth table。`Static`：不读不写 marker（文件即便
  `0/0` 也不动），启动 `authorized = true` → `Preflight`。分类无重叠；AC-14 锁测。
  **同一分派点**决定：SerialRT 计划 → IPC 注入 `Some(Arc<dyn SerialEmergencyCtrl>)`、
  `Guards.rt_serial = true`、`HardRtCtrlClient` 以 `ctrl_kind = SerialRt` 构造
  （`safety_state()` 读 `safety_word`）；HardRT 计划 → `None` / `false` / `HardRt`。
  这是**构造期不变量**（trait 实现于共享类型，非类型系统不可表达），由 factory
  分派测试锁定（AC-14）。
- **authorize（放宽方向）**：`s0_daemon rx-trust authorize --robot-id <id>`，锁内：
  读两份，**非 canonical `1/1`（含 `0/1`、`1/0`、`0/0`、缺失、损坏、非法内容）→
  退 2** 并提示"先 `rx-trust revoke` 规范化"（文件不动）；canonical → 写 `.bak =
  0\n`（两阶段，任一阶段失败 → 退 2，primary 仍 `1`）→ 写 primary `= 0\n`：phase 1
  失败 → 退 2（`1/0`，下次启动 untrusted + 规范化）；phase 2 失败 → **退 3 = 授权
  已生效（命名空间）但持久性未知**（掉电回退 `0→1` = 安全）；全成功 → 退 0。审计
  日志（uid / 时间 / 原内容 / 结果 / 退出码）。
- **revoke（收紧方向，backup-first 屏障；亦为规范化工具）**：`rx-trust revoke`，
  锁内：写 `.bak = 1\n`（两阶段）：phase 1 失败 → 退 2（未撤销）；phase 2 失败 →
  退 2（撤销未确认：命名空间已 `x/1` 但掉电可能回退，**必须重跑**）；成功（屏障
  持久）→ 写 primary `= 1\n`：任一阶段失败 → **退 4 = 安全撤销完成（屏障持久）但
  规范化未完成（`0/1`；每次启动判 untrusted 并自行规范化），须重跑至退 0**；成功
  → **退 0 = 严格 canonical `1/1`**。从任何初态可执行。
- **status**：`rx-trust status` 读两份并打印 truth table 判定与是否 canonical
  （退 0；锁忙退 87）。
- **启动消费（收紧方向，backup-first 屏障；锁内、串口 open 之后、spawn 之前；
  仅 `Marker`）**：读两份 →

  | primary | backup | 判定 | 动作 |
  |---|---|---|---|
  | `0\n` | `0\n` | **授权候选** | 写 `.bak = 1\n`（屏障）：phase 1 失败 → **拒绝启动**（退 1；命名空间仍 `0/0`，下次可消费）；phase 2 失败 → `authorized = false` + Diag `MarkerDurabilityUnknown`，继续规范化 primary `= 1\n`（失败仅日志）→ Dirty（命名空间 `1/1` 或 `0/1`）；成功 → 写 primary `= 1\n`：任一阶段失败 → `authorized = false`（Dirty；屏障已持久）；成功 → `authorized = true` |
  | `1\n` | 任意 | untrusted | 规范化 `.bak = 1\n`（若非 `1\n`；失败仅日志） |
  | `0\n` | `1\n` / 缺失 / 损坏 | untrusted（partial authorize / partial consume / revoke 退 4 残留） | 规范化 `.bak = 1\n`（若非 `1\n`）→ primary `= 1\n`（失败仅日志；`0/1` 本身 untrusted） |
  | 缺失 / 损坏 / 非法内容 | 任意 | untrusted（首次部署或损坏） | 规范化两份 `1\n`（backup-first；失败仅日志） |
  | 读取权限错误 | — | 拒绝启动（退 1） | — |

  **不存在任何"缺失 ⇒ 授权"路径**；`authorized` 仅当前进程有效；一次性：只有
  两份均持久收紧的进程可信使用授权，其余一律 Dirty（不使用），故任一掉电序列
  下可信消费至多一次。
- **READY 提交点（S-25 / S-29）**：robot-health additive `try_notify_ready() ->
  ReadyOutcome`：读 `NOTIFY_SOCKET`；未设置 / 空 → `NotConfigured`；以 `@` 开头
  （abstract）→ `Err(io::ErrorKind::Unsupported)`（**不** silent skip）；否则
  `UnixDatagram::unbound()` + `send_to` → Ok → **`Sent`**，Err → `Err(e)`。**`Sent`
  只表示 datagram 确实执行发送并成功返回。** S0 `--notify-mode {required |
  standalone | auto}`（默认 `auto` = env `INVOCATION_ID` 存在 → `required`，否则
  `standalone`；生产 unit ExecStart 显式 `--notify-mode required`）：

  | 模式 | `Sent` | `NotConfigured` | `Err` |
  |---|---|---|---|
  | `required` | `READY_REACHED := true` | 日志 + 协作式关停退 1（**不置位**；`CP2` 兜底） | 同左 |
  | `standalone` | `READY_REACHED := true`（有 socket 亦发） | `READY_REACHED := true`（无 systemd 等待方） | 日志 + 协作式关停退 1 |

  旧 `notify_ready` / `send_message`（含 abstract silent skip）保留 0 改，供其它
  进程沿用；S0 不再调用。
- **`boot_supervisor`（独立执法，S-18 / S-20 / S-24 / S-27 / S-32）**：**`main()`
  首语句** spawn（在 `robot_logging::init()`、`Args::parse()`、`parsed.resolve()`、
  锁、latch、open、marker、spawn 之前）；线程体 = 循环 `sleep(50 ms)` + Acquire
  读原子位；**不取锁、不分配、不做 IO、不调 trait**；不注册 atexit。`t_start` =
  spawn 时刻。**N 未知期**：以 `N_max = MAX_JOINTS = 32`（S-35）计 `T_abs`；主线程在
  `parsed.resolve()` 成功后以选中 hardware 的 `motor_ids.len()` 执行
  `N_ATOMIC.store(N, Release)` → supervisor 下次轮询重算（只会缩短，不会延长；
  `N ≤ MAX_JOINTS` 由 `TooManyJoints` 与 CLI `--joints` 上限校验前置保证，S-36）；
  mock 计划取 `joint_count`（parse 时校验 `≤ MAX_JOINTS`，违反退 2）。
  - `CP1 = t_start + T_pre`：`SPAWN_REACHED`（主线程在 resolved plan 中**全部
    required worker 到达 `Spawned`** 后 Release 置位；当前生产 plan 恒七个，含占位
    io，S-31）未置 → 执法；
  - `CP2 = T_abs + 1 s`（`T_abs = t_start + T_pre + T_boot(N) + B_startup_ack`）：
    `READY_REACHED`（按 READY 提交点表置位）未置 → 执法；
  - **执法硬路径（wait-free）** = `SUPERVISOR_FIRED.store(cp, Relaxed)` →
    **`libc::_exit(88)`**（立即；不 unwind、不跑析构、不写日志、不 sleep、不遍历
    任何 registry、不等待任何线程）。退出码 **88** = "supervisor 强制退出"（可重启，
    ∉ `RestartPreventExitStatus`），与协作式退 1 区分；`READY_REACHED` 置位后
    supervisor 自行退出。
  - **覆盖边界**：覆盖主线程自 `t_start` 起在任一步（logging init、CLI parse、
    config 读盘、锁、latch、open、marker fsync、adapter 构造、七 worker spawn 含
    `ready_rx.recv()`、监督循环、READY 发送）的停摆与任一 worker 的停摆 / 拒退；
    **不覆盖**：pre-main ctor 段（robot-logging `#[ctor]` `lib.rs:217`，仅 tracing
    subscriber 初始化，无已知阻塞点；登记为盲区）、process-wide `SIGSTOP`、调度
    饥饿使 supervisor 自身不被调度、内核级阻塞——unit `TimeoutStartSec=infinity`
    为候选 E 硬要求保持不变，残余登记 `10_verification.md`，运维靠人工 / 外部监控。
    **宣称口径 = "t_start → READY 上界"，不称"进程启动总上界"。**
- **daemon 顺序（协作式快路径；硬保证由 `boot_supervisor` 提供）**：
  ⓪ `main()` 首语句 spawn `boot_supervisor`（记 `t_start`，`N_max = 32`）→
  `robot_logging::init()` → raw CLI parse → 预建 `StopFlags`
  （7 个 `Arc<AtomicBool>` 固定槽位：ipc / executor / pub / io / 心跳 watchdog /
  ClearEStop rr_server / SetEnabled rr_server，传入各 spawn，crate 内部签名）→
  `parsed.resolve()`（config 读盘，S-27；`validate_hardware_section` 含 `TooManyJoints`：
  `motor_ids.len() > MAX_JOINTS` → 退 2，S-36）→ `N_ATOMIC.store(motor_ids.len(),
  Release)`（S-35）→ ① `ensure_latch_parent` → 取锁（并发
  退 87 / 系统错误退 1）→ ② `load_estop_latch`（既有）→ ③ `build_adapter` = open
  串口（`factory.rs:73`）+ `FeetechAdapter::new` 安装 `call_deadline`（D2）+ 解析
  `SerialTrustPolicy` 与 `ctrl_kind` / `rt_serial` / `Option<SerialEmergencyCtrl>`
  （失败均退 1 / 2，**marker 未动、授权保留、零总线 IO**）→ ④ marker 消费
  （`Marker` 计划）→ ⑤ **七 worker 按既有顺序 spawn**（S-30 / S-31）：ipc
  （`:681`，含 `ready_rx.recv()` 握手）→ ClearEStop rr_server（`:730`）→ SetEnabled
  rr_server → 心跳 watchdog（`:801`）→ executor（`:856` / `:889`；**spawn 调用方
  线程不再 connect**；`safety_word` 初值 {`TorqueUnknown`, pending 0}（ctrl 构造时已置））→ pub（`:942`）
  → io（`:968`，占位，始终 spawn）；任一 spawn 失败 → 既有退 1 路径（其
  `graceful_stop` 无超时 join 由 supervisor `CP1` / `CP2` 兜底）→ 全部 `Spawned`
  → 置 `SPAWN_REACHED`，记 `t_spawn` → ⑥ serial 启动序列（D4：上下文选定 →
  `StopIssue` → `Connect` → `Confirm` → 出口）→ ⑦ 主线程协作式等待（1 ms 轮询）：
  - 出口未到且 `now ≥ t_spawn + T_boot(N)` → 置 `boot_deadline_expired`（serial
    **best-effort**：于下一事务边界，若本启动尚无 Safety 写则先 `StopIssue`，跳过
    剩余 Confirm，封顶 `AdapterUncertain`，记录
    `completed_safety = Some(AdapterUncertain{Unknown})`，转 `Fault{gen, PreflightTimeout}`
    `AwaitAck`；`estop_bound` → `SafetyStopFailedAwaitAck{AdapterUncertain}`）；
  - 出口带 gen（`AwaitAck` / `SafetyStopFailedAwaitAck`）→ 等 IPC commit +
    `set_policy(Disabled)` + gen ack，截止 = min(t_exit + `B_startup_ack`, `T_abs`)；
    `t_exit` 取本启动首次带 gen 的物理出口，AwaitAck 被 EStop 重开不刷新截止；
  - 出口 `Done{Passed}`，或当前物理轮已完成且对应 gen 的 ack 已到 → ⑨；
    AwaitAck 重开后，即使旧 ack 已到也须等再次物理完成，不能提前 READY；
  - `now ≥ T_abs` 且 READY 条件未满足 → 主线程协作式关停：日志 →
    `set_policy(Disabled)` → 置 `StopFlags` 全部已 spawn 槽位 → 轮询
    `JoinHandle::is_finished` 至 `T_abs + 1 s` → 退 1（若主线程本身停摆，由
    `boot_supervisor` 在 `CP2` 以 88 执法）。
  ⑨ 推导 guard → auto-enable（仅 `Done{Passed}`）→ **`try_notify_ready()` 按 READY
  提交点表**：置位路径 → `READY_REACHED.store(true, Release)` → systemd watchdog
  线程（既有 `:1097`）；失败路径 → 日志 + 协作式关停退 1（**不置位**；`CP2` 仍
  兜底）。期间 `SetEnabled` dry/live → `GuardRejected{actual}`。既有顺序无超时
  `graceful_stop`（`:1412`）**仅保留于正常关停路径**（Out；systemd
  `TimeoutStopSec` 兜底）。
- **启动上下文选定**（open 成功后、spawn 前由 daemon 传入；serial 首 tick 生效）：
  `authorized` → `Preflight`；否则 → `Fault{gen, BootUntrusted}`（`enter_fault(
  CommFault, Unknown, BootUntrusted)`，mailbox 立即发布）。两者缺省只跑**一轮**；
  已完成的 AwaitAck 收到新 EStop 时按 D4(d) 重开物理轮，Connect 始终只执行一次。
  Connect 失败：`Preflight` → 转 `Fault{gen, ConnectFailed, detail}`（code 按
  D7 类）；Dirty → 同级只记 Diag `ConnectResult{kind}`。**授权在 ④ 之后即视为
  已消费，Connect 失败不复用**。
- **`adapter_connected` 推导（enable-ready guard）**：ipc 每 tick 由 serial 原子
  推导 `adapter_connected = abort_request == 0 ∧ boot_outcome == Passed ∧ rx_trust ∧ inhibit 位集 = 0`；
  先 Acquire 读 abort 再读取 inhibit，不得以反序拼出虚假的双零（S-42）。
  既有 SAFE→READY 守卫（`state_machine.rs:191-196`）与 `SetEnabled` dry/live
  （`set_enabled_action.rs:80`）因此天然拒。HardRT / SoftRT 保持静态 true。**字段
  语义扩展登记**（`Guards` 文档 + CHANGELOG：非纯物理连接状态；`Guards` 另增
  `rt_serial: bool`）。
- **runbook 授权程序（维护者显式授权）**：① 电机总线断电重上电；② USB 拔插并
  记录枚举信息（`/sys/bus/usb` devnum / serial）；③ 确认 S0 已停（`authorize` 锁
  忙退 87 即拒）；④ `rx-trust status`；非 canonical → `rx-trust revoke` 至退 0
  （退 4 = 再跑一次；退 2 = 未撤销，必须重跑）；⑤ `s0_daemon rx-trust authorize
  --robot-id <id>`；退 0 / 3 = 已生效（退 3 另记"持久性未知"，不重跑物理程序；如非
  本意用 `revoke` 至退 0）、退 2 = 未授权（按提示处理后重跑）；⑥ 启动 S0 → 日志
  确认"授权消费 + 写回 + preflight Passed + READY Sent"；runbook 记录操作者 /
  时间 / 设备枚举 / 退出码 / 结果。退出码表：1 协作式失败（可重启；含 READY
  `NotConfigured` / `Err` under required）/ 2 配置 / 4 revoke 未规范化 / 86 guard
  停止态 / 87 锁并发（不重启）/ 88 supervisor 强制退出（可重启）。
- 明文：普通 `systemctl restart` 不恢复信任；软件 USB reset 不在程序内；unit
  启用期间禁止手工起 daemon（退 87 保护的是并发，不是替代 unit）；紧后候选落地
  前无自动恢复。

### D10（Q10）`DiagSink`（Lamport SPSC drop-newest）+ pub_thread 代发

- `robot-core::DiagSink: Send + Sync`，`record(&self, DiagEvent)`；`DiagEvent`
  `Copy` `#[repr(C)]` 16 B = {stamp_ns: u64, site: u8, kind: u8, a: u8, b: u8,
  seq_lo: u32}。robot-s0 实现 = Lamport 单生产者单消费者环（256 槽；`head` 生产者
  Release 写 / 消费者 Acquire 读，`tail` 反之；**满则丢弃最新**并 `dropped += 1`，
  不覆盖最旧、不触碰对方索引）。生产者 = serial 线程唯一；消费者 = pub_thread
  唯一；feetech 经 `FeetechAdapterConfig.diag: Option<Arc<dyn DiagSink>>` 持有。
- site 集合（替换热路径 tracing；重试后成功亦留证据）：`ReadStateFail{kind}`、
  `WriteCmdFail`、`GoalReadbackRetry{attempt}` / `GoalReadbackRecovered`、
  `TorqueWriteRetry{motor, attempt}` / `TorqueWriteRecovered`、
  `TorqueWriteFail{motor}`、`DeviceStatus{motor, byte}`、`Discard{ok}`、
  `Drain{bytes, deadline_hit}`、`Converge{state}`、`AbortObserved{reason}`、
  `EnterFault{code, origin}`、`Escalation{from, to, gen}`、`PhaseTransition{from,
  to}`、`StopRetry{attempt}`、`UntrustedProbe{all_off}`、`RxTrustLost{kind}`、
  `RxTrustLatched`、`InvariantViolation`、`EStopMerged{phase}`、
  `EStopBound{ctx}`、`EStopClaimed{phase}`（claim `serial_estop_pending`）、`RetireExempt{seq}`、
  `BootContext{kind}`、`ConnectResult{kind}`、`PreflightDone{result}`、
  `BootTimeout`、`BootAckTimeout`、`MarkerState{p, b}`、`AuthorizeConsumed{ok}`、
  `MarkerDurabilityUnknown`、`LockAcquired`、`TrustPolicy{kind}`、`Dropped{n}`。
- pub_thread 每 poll 消费 → `tracing::warn!/error!`（字段：site, kind, a, b, seq,
  stamp）+ 1 Hz 摘要；IPC 三落点直接 tracing（低频；含 `estop_monotonic` outcome
  与 READY outcome）；daemon 启动路径与主线程协作式关停日志直接 tracing（不在
  serial loop）；`boot_supervisor` 硬路径**无日志**（归因靠退出码 88 + 主线程最后
  日志）。

### D11（Q5 / Q13）ClearError 守门三元组 + 拒 auto-heal

- 目标态 `Ready` + `ReadyHold` + 前置（`safety_state == TorqueOffConfirmed` ∧
  `comm_level == L0` 持续 ≥ 500 ms ∧ `adapter_connected` ∧ inhibit 位集 = 0 ∧
  `rx_trust`）；拒绝 → `GuardRejected{actual}`。
- `adapter_fault_cleared` 推导按 RtClass：SerialRT → serial 原子推导，初值 `false`；
  HardRT / SoftRT → 保持现语义（初值 `true`）；两类都删 `ipc_tick.rs:388` 自置。
- SerialRT timeout 预提交与 mailbox 对账提交 Fault 的同一控制路径，立即将
  `adapter_connected` / `adapter_fault_cleared` 置 false；不得仅等 tick 尾刷新。
  abort 未退场时两个 guard 保持 false，读序按 D9；不改变 serial 故障登记所有权。
- 恢复触发面：不新增 S0 ClearError RR（S-7，紧后候选）；失步会话内前置不可满足
  → 实际恢复 = D9 程序 + 重启；拒 auto-heal（S0State 粘滞；SerialRT 下 EStop 亦
  不清 Fault，D13）；`SetEnabled(false)` 作 ack 的替代不推荐。

### D12（Q11）EStop fast lane；SLA = 测量目标 + 枚举上界（fast-path / 全路径分列）

- 范围：S0 内 topic-based 软件 EStop 路由；GPIO / gateway 段不纳入。
- **fast lane（SerialRT：一个逻辑 EStop = 恰一次 `estop_monotonic()`；HardRT = 既有
  `set_estop()` + `safety_stop()`）**：
  - topic 路径：`poll_estop` 取得样本（**t0**）→ SerialRT：`estop_monotonic()` 单
    CAS 同时置 `serial_estop_pending` 与 `ipc_estop_notice` → **t1**（CAS 返回）；
    HardRT：`events.set_estop()` → `safety_stop()` → t1；
  - 外部路径（signal / CLI / 测试）：`ExternalEvents::estop()` = 同一原语恰一次
    （**t0 = 调用进入，t1 = CAS 返回**；无独立"意图"原子位，意图与动作即同一 CAS，
    故不存在两步之间的暂停 / 提前返回窗口）；
  - IPC Step 0 以 CAS claim `ipc_estop_notice` → `EmergencyBatch.estop` → Step 1
    状态提交 + latch fsync（**不再调原语**）；serial 在检查点 claim
    `serial_estop_pending` → 物理收敛。**不是 process-abort 保证**：release `panic =
    "abort"`（S-23）下调用线程 panic = 进程终止 = S0 dead → 硬件 fail-safe OPEN
    （候选 E D15）。
- SLA-A（软件门闩）= t0 → t1 **P99 ≤ 2 ms**（测量目标；lock-free：无锁、无阻塞
  等待；strong CAS 重试次数为 AC-12 测量项，不据单次失败宣称进展）。**t1 语义**：
  自 t1 至本轮物理收敛完成，normal 事务不再启动：未 claim 时由同 word CAS 拒绝，
  claim 后由 phase / inhibit 拒绝；之前已 acquire 的事务完成 ≤ 2·`D_call`
  （`T_abort_reaction`）。健康 Emergency 完成后的采样沿既有 policy / inhibit 规则；
  `StopTorque` 继续禁 `write_command`，不把此门闩解释为永久禁止状态采样。
- SLA-B（物理断扭，终点 readback 全关）：

  | 段 | 空载 | 在途 IO |
  |---|---|---|
  | 唤醒 | ≤ 1 tick + σ_s | + 在途事务 ≤ 2·`D_call` + q |
  | broadcast write（fast-path：首发成功） | ≤ `D_call`（nominal ≈ 1 ms；§4.6 判据 < 10 ms） | 同 |
  | broadcast write（全路径：含 `StopRetry` ≤ 3 次） | ≤ `B_stop_issue` = 160 ms | 同 |
  | 下 tick probe（`:433` 跳过本 tick） | 1 tick + σ_s + ≤ 2·`D_call` | 同 |
  | 重试 | × ≤ 5，每次 + tick + σ_s | 同 |
  | **测量目标** | **P99 ≤ 20 ms** | **P99 ≤ 75 ms** |
  | **fast-path 参考值（首 probe / 5 次）** | 160 ms + 2σ_s / 580 ms + 6σ_s | 各 + 105 ms |
  | **全路径上界（首 probe）** | 5 + 160 + 5 + 100 = **270 ms** + 2σ_s | + 105 ms |
  | **全路径上界（5 次 probe）** | 5 + 160 + 5 × 105 = **690 ms** + 6σ_s | + 105 ms |

  三次 `Safety` 写全失败 → **不存在物理断扭上界**，只进入 `SafetyStopFailed{
  StopNotDelivered}`（D4）；fast-path 值仅供健康路径参考，**不得称全路径上界**。
- §4.6 原文对象 = adapter 一次广播调用本身（< 10 ms），单测 / 基准判据。

### D13（Q11 ②）EventQueue 五 lane + `EmergencyBatch`（notice claim + mailbox 快照）+ `estop_monotonic()` 单 CAS 双位发布 + `normal_io_active` 执行权 + 状态机 / classifier 优先规则 + 完整 tick 调度表（含 rr lane 第二道 deadline 检查）+ 两级 admission + typed `ExternalEvents` + 有界上游 channel

- **`SerialEmergencyCtrl::estop_monotonic() -> EStopOutcome { StopIssued, Merged,
  LatchOnly }`（P0-1 / P0-2）**：robot-core additive trait，实现于共享类型
  `HardRtCtrlClient`；daemon 仅在 SerialRT 计划注入 `Some(Arc<dyn
  SerialEmergencyCtrl>)`，HardRT 为 `None`（**构造期不变量**，factory 分派测试
  锁定；HardRT 走既有 `safety_stop()`）。**唯一发布者**：topic `poll_estop`（IPC
  ingress）或 `ExternalEvents::estop()`；IPC Step 1 **不调**。原语（全部 atomic，
  无锁）：
  1. `safety_latched.store(true, Release)`；对 SerialRT `pending_normal` 的非空
     未取消词 strong CAS 置 `PENDING_CANCELLED`，保留完整 payload，失败重读；
     空槽 / 已取消槽仅退出 pending 标记步骤，继续计数与权威 CAS，
     不造空 tag、不丢弃原词；随后
     `estop_requests.fetch_add(1, Relaxed)`（诊断，非权威）——均先于权威 CAS；
  2. **一次 strong CAS 循环于 `safety_word`**：`old = load(Acquire)`；
     `new.state = if old.state ∈ {TorqueUnknown, TorqueOn, TorqueOffConfirmed} {
     StopCommanded } else { old.state }`；`new.serial_estop_pending = 1`；
     `new.ipc_estop_notice = 1`；`new.normal_io_active = old.normal_io_active`；
     `compare_exchange(old, new, AcqRel, Acquire)`：成功 → outcome：
     `old.serial_estop_pending == 1` → **`Merged`**；否则 `old.state` startable →
     **`StopIssued`**；`StopCommanded` → **`Merged`**；`StopFailed` /
     `AdapterUncertain` → **`LatchOnly`**；失败（word 已变）→ 重读 `old` 重判；
  3. **权威状态 CAS 之后零写**：不写 policy（物理 `StopIssue` 仅按下述权限做
     条件 CAS 收紧）、不写 `emergency_action`（SerialRT 不再使用）。
  - **线性化点 = 该 CAS**：同时完成"严重度只升不降"、"发布两个 consumer 各自的
    请求位"、并与 serial 的 acquire-CAS 全序。**无 ABA**（level 位）；**无双发**
    （每逻辑 EStop 恰一次调用；重复调用只在真有第二次逻辑 EStop 时发生，合并为
    `Merged`）；**无初始化缺口**（`safety_word` 初值全 0 位于 ctrl 构造时，早于
    ipc / executor spawn；pre-spawn EStop 置位后由 serial 首 tick claim，启动规则
    (b)）。进展性质 = lock-free；重试次数 = 测量项。
- **normal IO 执行权（P0-1）**：serial 启动**每个** `PeriodicRead`（`read_state`）/
  `PeriodicWrite`（`write_command`）/ `NormalEffectful` 事务前，必须 strong CAS
  **`serial_estop_pending == 0 ∧ normal_io_active == 0 → normal_io_active := 1`**
  （其余字段保留）；事务完成（Ok / Err / 超时）后 CAS `normal_io_active := 0`。
  acquire 因 `serial_estop_pending == 1` 失败 → **不启动该事务**，转入 claim 与
  消费。EStop 原语的 CAS 与 acquire-CAS 在同一 word 上**全序**：原语在前 → 其后
  claim 前任何 acquire 失败，claim 后由收敛 phase / inhibit 门继续禁入 →
  **t1 至本轮收敛完成零新 normal IO**；acquire 在前 → 该事务为 t1 前
  在途，完成 ≤ 2·`D_call`（`T_abort_reaction`，D1），期间原语不等待
  `normal_io_active`（非阻塞）。`Safety` / `Confirm` / `Connect` 不需 acquire
  （它们是收敛路径本身，serial 单线程顺序执行）。`PreemptProbe::begin_transaction`
  在事务边界执行 acquire-CAS，成功后由 RAII lease 在所有退出路径恰一次
  `finish_transaction` 归还；`should_abort` 仅供 sleep 纯读检查，不取得执行权。
  `checkpoint` 只记已返回的 transport 调用进度，不能释放 request+response 共持的 lease。
  周期类的 `Aborted(Emergency)` 仅表示入口未获许可，不表示在途响应被截断；立即
  claim 后本 tick 不继续普通 IO，不把沿用旧快照的新时间戳伪装成成功采样。
- **serial 消费（claim）**：检查点 = tick 头、每次 acquire 失败处、`sleep_or_abort`
  分片。见 `serial_estop_pending == 1` → **处理开始时** CAS 清 0（claim；保留他位）
  → 按 D4 规则 (a)–(e)（latch、Diag、`estop_bound`、创建 `Emergency` 或合并）；
  **没有通用 policy 尾写**（撤基线 `serial_io_thread.rs:401-402`）。处理期间
  生产者再次置 1 = **新逻辑请求**
  → 下一检查点再 claim（不丢；phase 规则 (b)/(c)/(d) 合并，`Idle` 下规则 (a) 创建
  新轮 = 正确语义）。
- **SerialRT policy 写入权限（S-37）**：IPC 按 `PlannedTransition` 或
  `commit_fault` 提交 policy；只有控制面可恢复可输出策略，Fault 提交必须写
  `Disabled`，daemon 既有关停路径只写 `Disabled`。serial 不做无条件 policy
  store；唯一写点 = `StopIssue ∧ estop_bound` 调用 crate 内部
  `fn tighten_estop_policy(policy: &AtomicU8)`：Acquire 读原始字节，`match`
  为 `ActiveForward` / `HoldCurrent` / `ZeroCommand` / `ReadyHold` 时以 strong
  `compare_exchange(old, StopTorque, AcqRel, Acquire)` 收紧；失败重读重判；
  `Disabled` / `StopTorque` 直接返回、**不写**。未知编码按既有 `from_u8` 的
  `Disabled` 语义保留，不新增放宽路径。两种全序：serial CAS 先成功 → IPC 的
  `Disabled` 后写获胜；IPC 先写 `Disabled` → serial CAS 失败重读并保留它。
  禁止 load 判 Fault 后普通 store（有检查后写入竞态）。`StopRetry` / `Confirm` /
  `AwaitAck` / SSF 相关 phase / claim 均零 policy 写；纯 Fault（¬estop_bound）
  `StopIssue` 亦零写（AC-5(c)）。`Safety` / `Confirm` 由 phase owner 执行，不以
  policy 必须等于 `StopTorque` 为前置；`Disabled` 下仍可完成物理收敛。
- **EventQueue lane**（`state_machine.rs:443` 单 `VecDeque` 重构）：
  - **emergency**：SerialRT → Step 0 以 CAS claim `ipc_estop_notice`（清 0）→
    `EmergencyBatch.estop`；HardRT → EventQueue `estop_pending` 位（legacy，
    `set_estop()` 置位）。**无 SSF 位**——SSF 唯一来源 = mailbox 词快照（Acquire
    读一次 u64；code == SafetyStopFailed ∧ gen ≠ `fault_ack` →
    `Some(MailboxSnapshot{gen, detail, origin})`）→ **`EmergencyBatch { estop,
    ssf }`**；无容量概念、预算外。
  - **mailbox 槽**：每 tick Step 1b 一次服务机会，非 SSF code O(1) 处理，预算外
    （D4）；Step 1 命中 EStop 也不跳过。
  - **link lane**：有序 `VecDeque<LinkEvent>`（`Heartbeat` / `HeartbeatTimeout`）
    定长 64，入队与队尾相同则合并（连续重复；顺序证据 `state_machine.rs:593-632`）；
    **Full → 丢弃来者并置 `link_overflow`**；drain 时按序回放后，若 `link_overflow`
    → 追加一条合成 `HeartbeatTimeout`（未知链路态 ⇒ 超时，fail-closed）并清位 +
    计数。`HeartbeatTimeout` 因此**永不因队列满而丢失**。
  - **rr lane**：有序 `VecDeque<RrEvent>` 定长 128（ClearEStop / SetEnabled 请求，
    携 `deadline_ns`）；Full → 来者立即回 `ResourceExhausted`，**不建立 pending**。
  - **control lane**：有序 `VecDeque<Event>` 定长 128（ClearError / Enable /
    Disable / Stop / 其它外部控制事件），连续重复合并；Full → `AdmitError::Full`。
- **状态机 + classifier 优先规则（S-26 / S-34）**：`Guards` 增 `rt_serial: bool`
  （daemon 按 plan 置）；`try_transition` 新增置于 `(_, EStop) → Safe` 之前的规则
  **`(Fault(_), EStop) if guards.rt_serial → 原 Fault`**（`AdapterErr` /
  `CommFault` / `SafetyStopFailed` 均保持）；**`classify_transition`（`:349`）
  同步**：`ev == EStop ∧ matches!(new_state, Fault(_))` → `(Emergency, Disabled)`，
  其后保留通用 EStop 分支 `(Emergency, StopTorque)`（HardRT 通配转 `Safe` 时
  `new_state` 非 Fault → 仍 `StopTorque`，0 行为改）。guard `estop_latched` 仍置位
  + latch fsync；EStop latch 与物理收敛为正交义务。freeze diff：撤 SerialRT 下
  "EStop 压倒 FAULT"。
- **`EmergencyBatch` 分派矩阵（Step 1；S-22 / S-26）**：

  | batch | SerialRT（`Some` 注入） | HardRT（`None`） |
  |---|---|---|
  | none | — | — |
  | EStop only（已 claim notice） | `estop_latched` 守卫 + latch fsync（**始终**）→ `plan_transition`：非 Fault 态 → `Safe` + `StopTorque`（既有）；Fault 态 → **原 Fault** + `Disabled`（try + classifier 双同步）；**不调原语**（物理侧已由唯一发布者的 CAS 置 `serial_estop_pending`，serial 自行 claim 收敛） | 既有：守卫 + latch fsync + `safety_stop()`（五步）+ `Safe` |
  | SSF only（快照 gen g） | `commit_fault(SafetyStopFailed)` + `set_policy(Disabled)` + **`fault_ack := g`**（IPC 唯一 ack）；不改 latch | 不可达（HardRT 无 SSF 生产点，S-10；若出现按同列处理） |
  | both（快照 gen g） | `estop_latched` 守卫 + latch fsync → `commit_fault(SafetyStopFailed)`（高严重度获胜）+ `set_policy(Disabled)` + `fault_ack := g`；**不调原语** | 同 SSF only 列 + 既有 EStop 路径 |

  命中任一非 none → 清 in-flight pending（既有 A1）+ 记局部
  `emergency_handled = true` → **仍执行一次 Step 1b mailbox 服务** → 才结束
  本 tick（不执行 Step 2–5）。Step 1b 以新读到的完整词决定是否处理非 SSF，不能
  因本 tick 已处理 EStop 跳过，也不循环追赶生产者。`estop_latched` 守卫与 latch
  文件在所有含 EStop 的情形**始终持久化**。
- **已 admission RR 的 EStop 取消范围（S-41）**：仅 `EmergencyBatch.estop == true`
  时，Step 1 以 O(1) 记录当时 rr lane 长度为取消前缀（含本 tick Step 0 新 admission）。
  之后每次 rr pop 都消耗一个前缀计数并占 rr24 配额，包含 dry 与过期请求。
  **先执行第二道 deadline**：过期一律 DeadlineExceeded；未过期且在取消前缀内的
  live ClearEStop 回 PreemptedByEmergency，live SetEnabled 回 PreemptedByEStop，
  零 evaluator / latch / Event / PendingReplies。dry 仍只读评估；后续新 admission
  不受旧前缀影响。重复 EStop 更新前缀为当时 rr.len，不扫描队列。
  SSF only 不新增前缀取消：queued 请求按当前 Fault 守门，尚未执行的 Disable
  保持幂等 Disabled；已在飞 Set 的 SSF 仍为 AdapterError，both 保持 EStop 响应优先。
  **已在飞** PendingReplies 的既有 emergency 优先于观察 deadline 不变；前缀规则
  只服务尚未执行的 RrEvent。源 channel / gRPC / control lane 不在取消域，不能
  宣称取消所有旧意图，也不能把 SSF 等同新 EStop。
- **完整 tick 调度表（含 effectful pending 位置，S-28）**：

  | Step | 条件 | 动作 | 预算 |
  |---|---|---|---|
  | 0 ingress | 每 tick | topic polls（heartbeat 1、estop → SerialRT 调原语恰一次 / HardRT `set_estop()`）；SerialRT claim `ipc_estop_notice`；mailbox 词快照（SSF → `EmergencyBatch.ssf`）；wd → link ≤ 8；external → control ≤ 8；ClearEStop RR → rr ≤ 8；SetEnabled RR → rr ≤ 8（**第一道** deadline 检查在 source drain，既有 `ipc_thread.rs:726 / 782`：过期不入 lane、回 `DeadlineExceeded`；rr Full → 立即 `ResourceExhausted`） | 预算外配额 |
  | 1 emergency batch | 每 tick | `EmergencyBatch` 矩阵；EStop 另冻结 rr 取消前缀；命中 → 清 pending + `emergency_handled = true`；**继续 Step 1b** | 预算外 O(1) |
  | 1b mailbox | 每 tick（含 Step 1 命中） | Acquire 读一次完整词；非 SSF code 且 `gen != fault_ack` → 单调 commit + `Disabled` + 清 pending + `fault_ack := 读到的 gen`；新 SSF 留下一 tick Step 0 / 1，不提前 ack；完成服务后 `emergency_handled` 为 true → 本 tick 结束 | 预算外 O(1) |
  | 2 pending | 未命中 emergency 且在飞 | poll resp / deadline → retire（≤ `B_action_retire`）；**仍在飞 → 本 tick 结束，lane 不 drain**（既有 `ipc_tick.rs:186/209` 语义） | — |
  | 3 link | 无在飞 | ≤ 16（回放 + `link_overflow` 合成） | 16 |
  | 4 rr | 无在飞 | ≤ 24；**每条 dequeue 后、任何 dry/live evaluator / latch 修改 / Event 入队 / pending 创建之前，第二道 deadline 检查（新增，S-33）：`clock.now() > deadline_ns` → 立即回 `DeadlineExceeded`、零副作用**；未过期 live 取消前缀按 S-41 回复 Preempted；某请求创建 effectful pending → 本 tick 后续 rr / control 停止 | 24 |
  | 5 control | 无在飞 | ≤ 24 | 24 |

  固定顺序 emergency → mailbox → link → rr → control；emergency 命中只在 mailbox
  服务后结束普通处理。配额**按 lane 保留、不跨
  lane 转移**；lane → 状态机总量 ≤ 64。**在飞 pending 期间的交互（≤
  `B_action_retire` = 1.105 s + σ）**：
  - `HeartbeatTimeout`：留在 link lane（合并）；retire 后**同一 tick** 紧随 Enable
    commit 应用 → `Enabled.CtrlLostHold`（或对应 Lost 态）先于任何发布快照出现
    `Active`；**既不抢占也不阻止** enable 收口（AC-11(z)）；
  - `Stop`：daemon 已直接 `set_policy(Disabled)`（控制面收尾完成），状态机
    STOPPING 延后 ≤ `B_action_retire`；
  - 新 RR：ingress 照常进 rr lane（Full → 立即 `ResourceExhausted`），处理延后 ≤
    `B_action_retire`，等待期间过期者由第二道检查拒绝；
  - EStop / SafetyStopFailed / mailbox：**不被 pending 或 lane 延后**（Step 1 /
    1b 清 pending；SSF 晚于 Step 0 的发布在下一 tick Step 1 处理）。
- **推导上界（P1-1）**：RR source channel → admission ≤ ⌈64/8⌉ = 8 tick（拒绝
  `ResourceExhausted` 与第一道 deadline 检查均在此点决定）；admission → rr lane
  处理 ≤ ⌈128/24⌉ = 6 tick；**总 ≤ 14 tick（70 ms）**，在飞另 + `B_action_retire`；
  普通 lane 上界以没有 emergency 占用 tick 为前提；持续 EStop 下只保证 emergency
  与 mailbox 服务，不保证普通 lane 公平。第二道 deadline 检查保证过期请求不被执行；
  **不宣称**与 wire 2 s 的严格不等式
  （σ 未知）。`HeartbeatTimeout` 语义生效 ≤ 2 tick（合并使首条即代表全部），FIFO
  排空 ≤ ⌈64/8⌉ + ⌈64/16⌉ = 12 tick。
- **两级 admission（按事件类型）**：

  | 事件 | 入口 | lane / 通路 | Full 语义 |
  |---|---|---|---|
  | `EStop`（topic `poll_estop`） | SerialRT：`estop_monotonic()` 恰一次（置双位）；HardRT：`set_estop()` → `safety_stop()` | `safety_word` 双位（SerialRT）/ emergency 位（HardRT） | 不适用（无容量） |
  | `EStop`（外部：signal / CLI / 测试） | `ExternalEvents::estop()` = 同一原语恰一次（SerialRT）/ `set_estop()` + `safety_stop()`（HardRT） | 同上；不经 channel | 不适用 |
  | `AdapterErr(SafetyStopFailed)` | serial 按发布顺序发布 mailbox 词；IPC Step 0 快照 → Step 1 矩阵 → `fault_ack := g` | mailbox 词快照（`EmergencyBatch.ssf`） | 不适用 |
  | `AdapterErr(AdapterErr / CommFault)`（mailbox） | **每 tick IPC Step 1b 直接 Acquire 读一次完整 mailbox 词**（含 EStop tick）→ 单调 commit + `Disabled` + 清 pending + ack 该快照 gen | 独立原子槽，**不进任何 lane** | 不适用（level-triggered） |
  | `Heartbeat`（IPC 自身 `poll`）/ `HeartbeatTimeout`（wd channel） | IPC | link | 丢弃 + `link_overflow` → 合成 `HeartbeatTimeout` |
  | RR 请求（ClearEStop / SetEnabled） | rr_server → RR channel → IPC ingress（第一道 deadline）→ rr lane（第二道 deadline） | rr | **仅入队成功后建立 pending**；Full → 立即回 `ResourceExhausted`（`RpcStatus` 既有），无 pending |
  | `Enable`（daemon auto-enable，`s0_daemon.rs:1049`） | `ExternalEvents::control` | control | 日志 + 不重试，留 Safe（fail-closed） |
  | `Stop`（daemon 退出，`:1133`） | 先 `executor_ctrl.set_policy(Disabled)`（= `Stopping → Disabled` 既有语义，`state_machine.rs:392/558`，幂等）再 `ExternalEvents::control(Stop)` | control | **控制面收尾已完成**（后续 `write_command` 被 `Disabled` 门关闭，`serial_io_thread.rs:526`）；**物理断扭不保证**（serial 退出不调 `safety_stop()`，S-21；受候选 E D15 OPEN 约束）；入队仅供状态机进 STOPPING；`graceful_stop`（`:1412`）不依赖该入队 |
  | signal / CLI 其它控制事件 | `ExternalEvents::control` | control | 返回 `Full` 给调用方（日志 + 计数），不阻塞 |

- **typed `ExternalEvents`**（撤裸 `Sender<Event>`，`ipc_thread.rs:489`）：
  `estop(&self)`（如上，永不失败、不经 channel、恰一次原语）；`control(&self, ev)
  -> Result<(), AdmitError>`：经 `bounded(RR_CHANNEL_CAPACITY)` external channel
  `try_send`，`Full` / `Disconnected`（接收端已退出）；**无阻塞 `send` 方法**；
  `Event::EStop` 不接受经 `control` 提交（类型上排除）。`Disconnected`：daemon →
  IPC 已死，转 `graceful_stop` 退出路径；watchdog 线程 → 日志 + 停止；测试 → 断言。
  auto-enable 在启动物理出口 / ACK 后、READY 前发送；断连时不提交 READY。
  READY 后主循环检查 IPC handle finished，同样进入既有清理退出路径。
- **wd channel**：`wd_tx`（`s0_daemon.rs:555`）改 `bounded(RR_CHANNEL_CAPACITY)` +
  `try_send`；只承载 `HeartbeatTimeout`（`watchdog.rs:102`）→ channel Full ⇒ 已有
  ≥ 1 条同类事件排队 ⇒ 丢弃即合并（无信息损失），计数 `wd_full`；IPC ingress 后进
  link lane（其 Full 语义如上）。
- **IPC 入向事件槽上限**（不称"端到端"）：覆盖 = `safety_word` 三位
  （`serial_estop_pending` / `ipc_estop_notice` / `normal_io_active`）+ HardRT
  EventQueue `estop_pending` 1 位 + mailbox 1 槽 + link 64 + rr 128 + control 128 +
  wd channel 64 + external channel 64 + ClearEStop RR request 64 + SetEnabled RR
  request 64（`s0_daemon.rs:673/678`）；每 tick lane → 状态机 ≤ 64。**不覆盖**
  （各自已有独立上界）：两组 RR response channel（`:674/679`，bounded 64，消费者 =
  rr_server 线程）、两组 server pending deque（≤ `max_in_flight` 64，
  `set_enabled_rr_server.rs:180/197`）、iceoryx2 订阅队列（由 service 配置固定，
  Out）。

### D14（Q15）锁不变量 (a)；(b) P1

`pub const RR_CHANNEL_CAPACITY: usize = 64` 共源（`s0_daemon.rs:734/777` 字面量与
D13 两 channel 改引用）+ `const _: () = assert!(..)` + 灌满 64 / 卡死 server 单测；
RR pending 仅在 rr lane 入队成功且通过第二道 deadline 检查后建立（D13）；(b) 非
阻塞派发登记 P1。

### D15 观测面与测试基线

wire：无新 method / schema / message / field，既有 enum +1。`comm_fault_e2e`：
mock transport 可注入每次 IO 延迟分布 / `CommErrorKind` / 第 k 台 NACK / 断连段落
/ 同形旧帧 / TX 阻塞 / `set_timeout` Err / open 永久阻塞；transport 调用序列记录器
（时序 + class）；调度延迟探针线程（σ_est）；GetState 轮询 client（10 ms，实测
RTT）；marker `MarkerFs` seam（rename / 目录 fsync 注入失败 / 永久阻塞 + 掉电回退
模拟）；锁 errno 注入 seam；config 读盘阻塞 seam；notify seam（`NOTIFY_SOCKET` 指向
path socket / abstract / 不存在路径 / 未设置；`INVOCATION_ID` 有无）；IPC / serial
冻结开关、worker 拒退开关、subscribers_factory 阻塞开关、主线程停摆开关（spawn
后 / READY 发送前两处）；`safety_word` CAS seam（原语 load 与 `compare_exchange`
之间暂停；CAS 成功之后暂停；serial acquire-CAS 之前 / 之后暂停；claim 之后处理
期间暂停）+ ctrl 原子写序列记录器（含全序时间戳，断言权威状态 CAS 后零写与两
CAS 顺序）；policy CAS seam（serial load 后 / CAS 前暂停，与 IPC Disabled 写交错）
与按线程归属的 policy 写记录器；AwaitAck 的 ACK 冻结 / 物理重开 / 完成记录探针；
pre-spawn EStop 注入（ctrl 构造后、executor spawn 前）；mailbox 发布
顺序 seam（① / ② / ③ 两两之间暂停生产者）；rr lane 等待期 deadline 注入（入
lane 时有效、处理前过期）；factory 分派探针（IPC 是否持有 `SerialEmergencyCtrl`、
`Guards.rt_serial`、`ctrl_kind`）；`PlannedTransition` 直接断言接口；config /
CLI 电机数越界夹具（33）；启动序列断言；lane 灌满逐类注入；多来源持续灌流；在途
SetEnabled 注入。

## Acceptance Criteria（round 20）

- **AC-1** additive / 0 退化：实现前按 Cargo `--list` 冻结全 workspace 具名
  integration targets、用例与 required feature，既有 e2e + smoke + `sd_watchdog`
  全绿。历史“12/12”按 `dimos_workflow_e2e` 的 11 个主体测试 + 1 个公共 subprocess
  测试溯源，当前以实际清单为准；不以固定总数代替覆盖。禁止通过删测试、增加 ignore、
  关闭 feature 或零测试满足本项；批准的启动语义变更须给旧覆盖到新 AC 的映射。fbs 仅
  enum +1；HardRT `rt_thread` 测试全绿；robot-health 旧 `notify_ready` /
  `send_message` 0 改；`ExecutorCtrlClient` 仅允许已批准的
  `abort_pending_normal(NormalAbortTarget)` additive default，不新增 EStop 方法；HardRT EStop 通配转 `Safe`
  + `StopTorque` 不变。
- **AC-2** 有界 transport + 信任不变量：(a) 滴流 → `D_call ± 5 ms` 返回，`got`
  真值；(b) `write_all` 同；(c) 任一 RX 失步后注入同形旧帧（全 off 训）→
  `safety_word.state` 权威值 `AdapterUncertain`（内部断言）、ClearEStop dry-run
  与真调用均拒（`clear_estop_action.rs:97` 路径）、wire `AdapterUncertain`、该会话
  内 `TorqueOffConfirmed` 0 次；(d) characterization（Orin，非判据）：1a86:55d3 /
  cdc_acm / kernel / 响应延迟直方图入 `10_verification.md`；(e) TX timeout：Mock
  write 阻塞 > `D_call` → `Comm(IoWrite)` → 按矩阵定级；(f) `TRUST_CLEARING` 穷举：
  集合内每个 kind 注入一次 → `session_clean` / `rx_trust` 皆 false；
  `PendingDeadline` 注入 → 两者不变。
- **AC-3** census 锁（常量计算 W/R/S 与 D2 表相等，N=6/13）+ 预算数值登记
  （105 ms / 1.105 s / 160 ms / 525 ms / `T_connect` / `T_pre` / `T_boot` /
  `B_startup_ack` / `T_abs`（`N_max = 32` 初值 15.9 s 与实际 N 两值）/ SLA-B 全
  路径 270 / 690 ms / RR 8 + 6 = 14 tick）。
- **AC-4** 抢占：(a) 在途 + EStop → ≤ 2·`D_call` + q 返回 `PreemptedByEmergency`；
  (b) 空闲 EStop → ClearEStop → enable 正常；(c) 超时 abort 三态（在途 / 已完成 /
  未消费）均以 `enter_fault(CommFault, PendingDeadline, PendingTimeout)` 发布
  mailbox 且 `rx_trust` 不变；三态之外构造"记录不匹配"注入 → `InvariantViolation`
  + `enter_fault(.., Unknown, ..)` + 收敛 + 信任量清零，正常路径下不匹配不出现
  （序列覆盖证明）；(d) seq+1 抢占不影响 seq；(e) 结束后无残留（`active.valid =
  0`）；(f) fail-fast；(g) 事务不可拆：write_all 之后置抢占 → read_exact 仍完成，
  抢占在下一事务边界（acquire-CAS）生效；sleep 中置抢占 → ≤ q 返回；(h) seq
  fail-closed：注入 `seq_counter = 2^40 − 2` → 成功、成功、第三次 `Unsupported`；
  seq ≥ 1：初值断言 1、注入 `seq_counter = 0` → 取号 `Unsupported`，`abort_request =
  ABORT_NONE` 永不匹配任何 valid 槽；(i) 矩阵 + 豁免收窄（可信分支可达）：
  `rx_trust = true` 会话中 `Enable(false)` N 个 ACK 全成功被超时命中 → 序列记录
  **零** `Safety` 写、零 `Confirm` probe、mailbox 发布 gen n（CommFault /
  `PendingDeadline` / PendingTimeout）、Diag `RetireExempt`、`rx_trust` 仍 true、
  `safety_state = TorqueOffConfirmed`、轮次进 `AwaitAck(n)`、IPC 预提交的
  `Fault(CommFault)` 以 ack(n) 对账后 `FAULT_PENDING` 清；注入 `rx_trust = false`
  → `AdapterUncertain`；任一 ACK 缺失 → 全收敛；`SetMode` Ok / `HoldCurrent` Ok
  被超时命中 → 全收敛；在途 `Enable(true)` 被超时命中并在事务边界中止 → 收敛 +
  `rx_trust` 不变 → 可信 all-off → `TorqueOffConfirmed`；(j) 操作域：动作中 EStop
  → `Safety` 广播事务确实发出且未被 Aborted；`Confirm` / `Connect` 事务在
  `serial_estop_pending` 位或 Submitted timeout 有效时照常执行（无需 acquire）；
  (k) `(plan,None)` 创建后无 Busy、IPC 停顿超过 1 s → 原 action 零执行，发布
  Unsubmitted、预提交 CommFault/Disabled、serial PendingDeadline 收敛并等 gen/ACK，
  rx_trust 与 EStop latch 不变；(l) 持续 Busy 与残留 pending/active 分列，残留槽
  禁止发新普通 IO并记录 Unknown 不变量失败；(m) Unsubmitted 与 EStop/mailbox
  交错不吞请求、不跳物理确认与 ACK，清请求 CAS 不覆盖不同新请求，高标签不得误匹配 seq 0。
  (n) Submitted timeout 已发布、EStop 标取消、serial 后消费：原 action 零执行，
  PendingDeadline 保信任、gen / ACK 与独立急停义务均完成；标记 CAS 败给 serial
  消费的反序亦成立；(o) 发布者在 tag 成功、权威 CAS 前暂停，serial 只取消动作，
  不提前创建 Emergency / 写 policy / 发布 gen；发布者继续后恰一个权威请求；
  (p) 两发布者重复取消不覆盖 payload，取消完成后迟到同 seq timeout 仍精确匹配；
  非法保留位、空 tag、seq 0 均 Unknown，HardRT 48 位高值不得被剥成取消 tag。
- **AC-5** 三元 + S-6 + 禁写空窗：(a) 扭矩阶段第 k 台 NACK → 收敛；(b) 性质"从不
  在任一台 on 时报 `TorqueOffConfirmed`"；(c) 非 Comm 失败后冻结 IPC 至 confirm
  之后 → 期间 policy write 计数恒 0，`FAULT_PENDING` 直至 ack 才清。
- **AC-6** 路由矩阵：每个可达格一条测试（含周期事务 acquire 败给 EStop 时零 IO、
  立即 claim、无通信错误与新 snapshot；反向 acquire 先赢时完成整个事务再释放；
  `PeriodicWrite` TX timeout；`Safety`
  TX timeout / 参数错误 → `StopRetry`；`Confirm` `StatusError` → attempt++ 且信任量
  不变；`Confirm` `InvalidParameter` → attempt++ 不 panic；`Confirm` hygiene
  `DiscardFailed` → attempt++ 仍 probe；`Connect` 各 kind → `Fault{ConnectFailed}`
  且 code 按 D7；`NormalEffectful` Aborted(Timeout) → `PendingDeadline` 且信任量
  不变）；每个不可达格一条锁定测试；Safe 态掉线 → `Fault(CommFault)` 且
  `s0_fault_code = 7`；单次瞬态 Timeout 不再被容忍（明文）。
- **AC-7** wire 映射穷尽（`s0_fault_code = 7`；`pub_thread` 映射；SerialRT 读
  `safety_word.state`、HardRT 读 legacy 字节）。
- **AC-8** mailbox + 升级：(a) 定级后 ≤ 2 ipc tick commit + Disabled 后 ack——
  **在任一 lane 满、effectful pending 在飞、连续 EStop 时同样成立；同 tick 有
  EStop batch 仍执行 Step 1b**（前提 IPC 持续推进）；(b) IPC 冻结时零写；
  (c) 同级重入不重复；(d) IPC
  ack CommFault(n) → some-on × 5 → `SafetyStopFailed(n+1)` 进
  `SafetyStopFailedAwaitAck`、`STOP_FAILED` 置位、`FAULT_PENDING` 重置，旧 ack(n)
  不清任何位，ack(n+1) 后才清并进终态；(e) `AdapterErr` 活动中来 CommFault →
  升级、信任量按 kind、phase 自 `StopIssue` 重跑（恰一次）；(f) gen 0 空 / 自 1 /
  注入 `u32::MAX − 1` → 饱和 fail-closed 不绕回；(g) 超时预提交后收到 gen 仍 ack；
  (h) mailbox 不经任何 lane（序列记录：commit 发生于 lane drain 之前）；(i) 发布
  顺序竞态：seam 让生产者分别暂停于 ① 与 ② 之间、② 与 ③ 之间，IPC 持续 tick：
  ③ 之前 IPC 不见新 gen（`fault_ack` 不变、无 commit）；③ 之后 IPC 读到的
  `safety_word` / inhibit 必已含 ① / ②；IPC 只 ack 读到的 gen；随后生产者再发布
  gen+1（升级）→ 旧 ack(gen) 不清 `FAULT_PENDING`，serial 仅在 `fault_ack == gen+1`
  清位；gen 与 detail 同词，任何时刻不存在"detail 新 gen 旧"的观测；
  **(j) 连续 EStop 不饿死非 SSF**：预置 CommFault gen n 待 ack（fault_ack = n−1），
  所有 lane 满、effectful pending 在飞；确定性调度连续 **4 tick**，每 tick
  Step 0 前注入一个新逻辑 EStop → 每 tick 序列均为 Step 1 → Step 1b → return，
  普通 lane 零 drain；首 tick Step 1b 必 commit `Fault(CommFault)` + `Disabled`
  + ack(n)，后续 EStop 保持原 Fault / Disabled，n 不重复提交；两 consumer 的
  claim 与 `estop_requests` 按实际注入记账。
  **(k) Step 0 后升级 SSF**：初始 CommFault(n) 已 commit + ACK，`fault_ack = n`；
  Step 0 已快照无 SSF，Step 1 处理 EStop；在 Step 1b
  之前 serial 发布 SSF(n+1) → Step 1b 不以非 SSF 分支 commit / ack，ack 仍为 n；
  下一 tick 继续注入 EStop → both 格先持久化 latch，再 commit SSF + `Disabled`
  + ack(n+1)，总 ≤ 2 tick；之后 Step 1b 不重复 ack，旧 ack(n) 不清新义务。
- **AC-9** phase 表：每转移一条序列测试；单 tick 单 owner；`Confirm` 不可信下先
  `drain_until_quiet`（absolute_deadline 命中仍 probe）；attempt 上限 5；(m) 初始
  Safety 写失败：attempt 1 失败 → `StopRetry` 2 次 → 第 3 次失败 →
  `SafetyStopFailedAwaitAck{StopNotDelivered}`（gen 发布、`STOP_FAILED` 置位）→
  ack → 终态；序列记录总写次数恰 3、间隔 1 tick、之后零普通 IO 与零再广播；首次
  `enter_fault(SafetyStopFailed)` 直接 `SafetyStopFailedAwaitAck` 不进 `StopIssue`；
  **(n) `EmergencyBatch` 十二格**：{none, EStop, SSF, both} × serial 所处 {收敛中
  （`StopIssue` / `Confirm`）, `AwaitAck`, `SafetyStopFailedAwaitAck` / 终态}，
  每格断言：所有含 EStop 格 → `estop_latched = true` 且 latch 文件 `1`、
  `ipc_estop_notice` 由 IPC claim 恰一次、`serial_estop_pending` 由 serial claim 恰
  一次、**IPC Step 1 零原语调用**；含 SSF 或处于 SSF 态的格 → Safety 写计数不增、
  `safety_word.state` 保持 `StopFailed` / `AdapterUncertain`（不被 `StopCommanded`
  覆盖）、`S0State == Fault(SafetyStopFailed)`（不落 Safe）、policy `Disabled`、
  原语（由发布者调）返回 `LatchOnly`（或位已置时 `Merged`）；EStop-only 且非
  Fault 态且 `state ∈ {TorqueUnknown, TorqueOn, TorqueOffConfirmed}` 格 →
  `StopIssued`、CAS 至 `StopCommanded` + 双位置 1、`Safe`；EStop-only 且 `state ==
  StopCommanded` → `Merged`（不重写 state）；both 格 → 单次 commit 为
  `Fault(SafetyStopFailed)`、`fault_ack := g`；`SafetyStopFailedAwaitAck` 期间 ack
  前 `FAULT_PENDING` 保持；**IPC 在原语 CAS 之后零权威原子写**（ctrl 写序列记录器
  断言；`estop_requests` Relaxed 计数在 CAS 之前）；**(o) `estop_monotonic` 竞态
  与所有权**：(o1) load 后 / CAS 前暂停发布者，并发 serial 提交 `StopFailed` → CAS
  失败重读 → `LatchOnly`、双位置 1、serial 零 IO；(o2) CAS 成功（`StopIssued`）后
  暂停发布者，并发 serial 提交 SSF（其 CAS 保留请求位）→ 序列记录断言发布者在 CAS
  之后**零**权威原子写；serial claim 时已处 SSF 相关 phase → 零 IO、状态不变；
  (o3) `StopCommanded` 分支：load 后暂停，并发 serial 提交 `StopFailed` → CAS 失败
  → 重读 → `LatchOnly`（**不是** `Merged`）；(o4) **pre-spawn EStop**：ctrl 构造后、
  executor spawn 前调原语 → 双位置 1、`state` 按初值 `TorqueUnknown` →
  `StopCommanded`（`StopIssued`）；serial 首 tick 按启动规则 (b) claim，启动
  `StopIssue` 仍为首个总线 IO、总 Safety 写 ≤ 3；**(o5) serial 夹在快路径与 Step 1
  之间**：发布者 CAS → seam 让 serial 在 IPC Step 1 之前 claim 并开始收敛 → IPC
  Step 0 claim `ipc_estop_notice`、Step 1 仅处理、**零原语调用**、`estop_requests` = 1、serial
  无第二轮 `StopIssue`；**(o6) 处理期间再次注入**：第一次发布后，先安排 serial
  claim 与 IPC Step 0 claim 各完成一次，再用 seam 暂停二者处理；发布者再调原语
  （第二个逻辑 EStop）→ 双位重新为 1、`estop_requests = 2` → 二者在下一检查点
  各完成第二次 claim，serial 按当时 phase 合并或重开，IPC 状态机幂等。对照序列：
  第二次发布早于 IPC 第一次 claim → 两个未消费 notice 合并，IPC 仅 claim 一次
  合法，`estop_requests` 仍为 2；不得由注入次数推导每个 consumer 的 claim 次数。
  **(o7) 单次外部 API 调用
  恰产生一个权威请求**：`ExternalEvents::estop()` 一次 → ctrl 写序列记录恰一次
  原语成功 CAS、`estop_requests` +1、双位各置 1 一次；**原语成功 CAS 后零写**，
  CAS 前的 latch 写、pending 取消标记 CAS 与诊断计数按 D13 执行；“恰一次”指
  safety_word 的权威成功 CAS，pending 标记 CAS 单独计数；上述各项均：
  `safety_word.state` 不降级、`estop_latched` 已持久化、SSF gen 在下一 tick 正常
  commit + ack；**(p) EStop 不清粘滞 Fault 全表 + classifier**：{`Fault(AdapterErr)`,
  `Fault(CommFault)`, `Fault(SafetyStopFailed)`} × 原语 outcome {`StopIssued`,
  `Merged`, `LatchOnly`}（各以对应 `safety_word` 预置构造）→ SerialRT：
  `plan_transition` 返回的 `PlannedTransition { new_state == 原 Fault, new_policy ==
  Disabled, kind == Emergency }`（**直接断言 plan**）、`S0State` 不变、
  `estop_latched` 置位 + latch 文件 `1`、serial 侧按本轮记录走规则 (a)/(c)/(d)/
  (e)（serial 分支依据本轮 phase / completed_safety，不由 outcome 反推）；
  HardRT（`rt_serial = false`）：`PlannedTransition { new_state == Safe,
  new_policy == StopTorque, kind == Emergency }`（0 行为改锁测）；**(q) normal IO
  执行权全序（两种 CAS 顺序，含 `read_state`）**：(q1) seam 让 serial 在 acquire-CAS
  之前暂停，发布者原语 CAS 先完成（t1）→ serial 恢复后 acquire 失败 → **t1 至本轮
  收敛完成零 normal 事务启动**（序列记录：`read_state` / `write_command` / normal 事务均无新
  起点），立即 claim 并消费；(q2) serial acquire-CAS 先成功并开始事务，发布者原语
  CAS 后完成 → 该事务完成 ≤ 2·`D_call`（序列记录其起点 < t1），随后 release、下一
  acquire 失败；两序均以 ctrl 写序列的全序时间戳断言；对 `PeriodicRead`、
  `PeriodicWrite`、`NormalEffectful` 各一组；反向：绕过 acquire-CAS 直接启动 IO 的
  错误实现被锁测拒绝（t1 后出现新起点）；**(r) policy 写权与交错**：
  (r1) SSF 已 commit + Disabled + ack，serial 已到终态 → 注入新 EStop → IPC 先
  claim 并处理（保持 Fault / Disabled）→ serial 后 claim 走 (e)：零总线 IO、
  零 serial policy 写，最终及后续 policy 始终 Disabled；交换两 consumer 顺序，
  并将终态换成 `SafetyStopFailedAwaitAck` 再各测一次。
  (r2) `StopIssue ∧ estop_bound`，serial load ActiveForward 后暂停 → IPC
  commit Fault + Disabled → serial CAS 失败重读、零写；反序 serial CAS 先成功
  收紧 StopTorque → IPC 后写 Disabled，最终 Disabled；对其它三种可输出 policy
  同测。(r3) 六个合法 policy × `estop_bound` 两值：纯 Fault 一律零 policy 写；
  EStop 仅四种可输出 policy 可 CAS 至 StopTorque，Disabled / StopTorque 零写。
  (r4) IPC 暂停、policy 初始 ActiveForward，serial 完成 Emergency → StopIssue
  已收紧 StopTorque，完成后零 `write_command`，不能恢复旧 ActiveForward。
- **AC-10** DiagSink：字段断言；serial 线程 0 tracing（按线程名）；`*Retry` +
  `*Recovered`；drop-newest + `dropped` 正确、旧记录保留。
- **AC-11** RR 锁 + 队列 + admission + 调度：const assert；灌满 64 / 卡死；65 →
  64 + 1；EStop 与 64 RR 同 tick → 原语 CAS 先于 drain；EStop 与 SafetyStopFailed
  同 tick → 按矩阵 both 格处理；"Heartbeat, Timeout" 与 "Timeout, Heartbeat" 终态
  不同；pending 1 s 期间队列长度有界；**(x) 先灌满各 lane 再逐类注入**：冻结 IPC
  的 drain → 灌满 link（64 次 H/T 交替）、rr（128）、control（128）与 wd / external
  / 两 RR channel（各 64）→ 逐类注入：外部 EStop → 原语单 CAS 立即置双位（不受
  任何满影响），解冻后 IPC claim 处理；mailbox SSF 词 → 解冻后 Step 1 处理并 ack；
  `HeartbeatTimeout` → wd Full 丢弃 + `wd_full`，解冻后回放 + 合成
  `HeartbeatTimeout` 落地（状态机终态 = Lost）；mailbox CommFault：lane 全满 +
  effectful pending 在飞 → 解冻后 ≤ 2 tick commit + `Disabled` + ack，早于任何 lane
  drain；RR 请求 → 立即 `ResourceExhausted` 且 pending 计数不增；daemon `Enable`
  → `Full` + 留 Safe；`Stop` → `set_policy(Disabled)` 已调且进程正常退出；
  `Disconnected`（kill IPC 接收端）→ daemon 走退出路径；有界断言 = 每个有界结构
  `len ≤ cap` 硬锁 + warmup 后持续注入 10 s 的 RSS 高水位 ≤ warmup 高水位 + 1 MiB；
  **(y) 多来源持续灌流（无在飞 pending、无 emergency batch）**：external channel、两 RR channel、wd
  channel 各以 ≥ 64 条/tick 速率持续注入 ≥ 2 s → 每 tick 序列记录断言：link 推进
  min(len, 16)、rr 推进 min(len, 24)、control 推进 min(len, 24)，顺序 emergency →
  mailbox → link → rr → control；链路超时语义在 ≤ 2 tick 内生效、wd FIFO 排空 ≤
  12 tick；每个 RR 请求 ≤ 8 tick 得 admit 或 `ResourceExhausted`，admit 者自
  admission 起 ≤ 6 tick 被处理（总 ≤ 14 tick）；无 lane 连续 2 tick 非空却零推进；
  **(y′) rr lane 等待期过期（双 RPC 反向测试）**：两条 RPC 在 source drain 时均未
  过期（第一道通过、入 rr lane），设置极短 `deadline_ns` 使其在 rr lane 等待期
  （以在飞 pending 或 lane 前置负载制造 ≥ 2 tick 等待）过期 → 第二道检查各回
  `DeadlineExceeded`；断言零副作用：无 dry/live evaluator 调用、latch 文件与
  `estop_latched` 不变、无 Event 入队、pending 计数不增、`safety_word` 不变；同批
  未过期的第三条 RPC 正常处理；**(z) 在途 SetEnabled + 多源灌流**：`Enable(true)`
  在飞期间同上灌流 + 注入 `HeartbeatTimeout` 与 `Stop` → 在飞期间 link / rr /
  control 零 drain、emergency 与 mailbox 照常；retire（Ok）后同一 tick：Enable
  commit → 紧随应用 `HeartbeatTimeout` → 状态 `Enabled.CtrlLostHold`（Lost），发布
  快照序列中**不出现** `Active`；enable 未被 timeout 中止（`Enable(true)` 事务
  完整）；`Stop` → `set_policy(Disabled)` 在注入 ≤ 1 ipc tick 内已生效，STOPPING
  在 retire 后 ≤ 1 tick；期间新 RR 得 `ResourceExhausted` 或延后处理 ≤
  `B_action_retire` + 14 tick（等待中过期者由第二道检查拒绝）。
  **(aa) 已 admission 急停取消前缀**：EStop 前已 admission live Clear / Set →
  相应 Preempted 且零执行；同前缀 dry 继续只读；标记后过期优先 DeadlineExceeded；
  每 pop（含 dry / expired）均扣前缀，源 channel 尚未 admission 与后续新 admission
  不被旧前缀误杀；重复 EStop 扩大为当前长度。SSF only 下 queued Disable 幂等
  Disabled、既在飞 Disable 返回 AdapterError；both 维持 EStop 响应优先。
  **(ab) timeout / mailbox 与 ClearError 同 tick**：Submitted / Unsubmitted
  pending 超过 1 s，serial 尚未消费时，同 tick 及后续 tick 注入 ClearError / Enable，
  快照始终 CommFault、policy 始终 Disabled、两 guard 为 false，无新 normal 提交；
  abort 先读非零后 serial 登记并清零、以及 serial 完成退场后才刷新两序均不 ready。
  初始 guard 刷新后注入真实 mailbox，再处理 ClearError：仍 ACK 对应 generation，
  不出现 Ready / ReadyHold。此项不新增 S0 ClearError RR。
  正常动作 Comm / HardwareFault 的完成发布点亦运行 IPC 两 tick 并注入 ClearError，
  必须已观察到 inhibit / mailbox、保持 Fault；取消时原 normal 无 RR owner，
  同 tick 后续新 RR 不得误回 Preempted，其 request_id 与动作生命周期一一对应。
  **(ac) daemon 断连退场**：auto-enable=true 时，在真实发送点前请求 IPC stop 并
  等待其 handle finished，再由真实 channel 返回 Disconnected；断言未 READY、
  Disabled、已启动七 worker 均请求 stop、退出 1。正常 READY 后通过测试控制器
  仅请求真实 IPC stop，断言注入已消费、daemon 退出 1、无 supervisor 假超时。
  测试 POD kind=4 `RequestWorkerStop` 只置预建 worker stop 标志；
  `ROBOT_TEST_IPC_EXIT_BEFORE_AUTO_ENABLE` 仅冻结上述窄交错，二者不伪造退出码。
- **AC-12** SLA 口径：(a) 每场景 1000 样本 / 100 warmup；负载 U(1,5) ms IO、背景 RR
  20/s；判据 P99；报告 P50 / P99 / max、σ_est 与**原语 / acquire / claim 的 CAS
  重试次数分布**，并分别对照 fast-path 参考值与全路径上界；Orin PX-56 为判据；
  (b) t0 定义：topic = 样本取得，外部 = `ExternalEvents::estop()` 进入；t1 = 原语
  CAS 返回；(c) 单原语原子性锁测：`ExternalEvents::estop()` 与 topic 路径各恰一次
  原语 CAS、无独立"意图"原子写、无第二次原语（ctrl 写序列记录）；调用线程在原语
  返回后立即提前返回或被 seam 暂停 → IPC 与 serial 各自 claim 并完成处理（进程
  存活判据）。明文：**不覆盖调用线程 panic**（`panic = "abort"`，S-23 → 进程终止，
  属 D15 OPEN 范围）。(d) H2 采样采用 S-45 的真实事务边界握手；idle 组以有界
  普通 admission 测试门建立无在途前提，inflight 组仍走真实事务与载荷，不宣称自然
  idle 命中率。边界回执、门解除/超时与完整 epoch 原始证据必须留存；不完整即 FAIL。
- **AC-13** §4.6 广播 ≤ 10 ms（Mock 计时 + Orin 实测登记）。
- **AC-14** 隔离（分类无重叠，按 S-19 注册名；**factory 分派探针**）：
  `mock_hard_rt` 除 `rx_trust.lock` 文件创建外 0 行为变化（探针断言：IPC 持有
  `None`、`Guards.rt_serial = false`、`ctrl_kind = HardRt`；EStop 走既有
  `set_estop()` + `safety_stop()` 五步并通配转 `Safe` + `StopTorque`），
  `adapter_fault_cleared` / `adapter_connected` 语义不变，`--rx-trust-policy` 指定
  → 退 2；`mock_serial_rt` 走 `Static` + 启动序列（AC-20），marker 文件即便 `0/0`
  也不读不写且 `authorized = true`，探针断言 `Some` / `true` / `SerialRt`；
  `mock_serial_rt` + `--rx-trust-policy marker` → 消费 marker；`feetech_sts3215` →
  `Marker` 消费 + `Some` / `true` / `SerialRt`，`--rx-trust-policy` 任何值 → 退 2；
  未知 SerialRT kind → 退 2。
- **AC-15（信任模型，逐步断言）**：(a) 授权启动序列 `authorized = true` →
  `StopIssue` Safety 写 → `Connect` Ok → probe all-off ∧ `session_clean` →
  `rx_trust = true` → `safety_word.state = TorqueOffConfirmed` → `Done{Passed}` →
  `adapter_connected = true` → `Event::Enable`（若 auto）→ `try_notify_ready` →
  `READY_REACHED`，每步顺序由原子快照记录锁定；`rx_trust = true` 之前
  `safety_state` 从未为 `TorqueOffConfirmed`；(b) 运行中失步 → 无落盘动作，
  `rx_trust = false`、`session_clean = false`；`SIGKILL` 重启 → marker `1\n` →
  Dirty；(c) Dirty → `Fault{BootUntrusted}`：`AdapterUncertain` + `fault_code = 7`；
  auto-enable / `SetEnabled` 均拒；(d) preflight some-on × 5 → `SafetyStopFailed{
  StopFailed}`；(e) preflight 期间失步（授权已消费）→ all-off 不可信 → 不提交
  confirmed、转 `Fault{PreflightFailed}` `AwaitAck`，不重跑；(f) EStop latch 与
  marker 相互独立。
- **AC-16** liveness lease：(a) 在途 40 ms IO → 计数每 ≤ 50 ms 推进；(b) 反向：
  持续 checkpoint 越过 `B_action_retire` 不返回 → 冻结 → 下一周期判停滞；(c) 单次
  IO 卡死 > 1 s → 停喂；(d) 启动 `Connect` 全程计数推进。
- **AC-17** RPC 接线（cross-process）+ 两种起算：(i) t0 = mailbox 发布 → GetState
  可见 `fault_code = 7` ≤ 2 ipc tick + 23 ms + RTT_实测 + 10 ms；(ii) t0 = 首次通信
  错误 → 上式 + 1 tick；在途 IO +105 ms；三种 Fault 下 SetEnabled 在 `T_pending` +
  2 ipc tick 内回 `AdapterError`。
- **AC-18** 编码：`AdapterErr` 全变体（含 `Comm(PendingDeadline)`）round-trip；256
  字节 decode→encode→decode 幂等；未知 kind → `Comm(Unknown)`，未知码 → `Other`。
- **AC-19（上下文 × 出口矩阵，按 `estop_bound` 分列）**：对 `Fault`（¬estop_bound）/
  `Preflight`（¬estop_bound）/ estop_bound（`Emergency`、`Fault` + 合并 EStop、
  `Preflight` + 合并 EStop）各注入 {5× `StatusError`、5× 通信错误、不可信 all-off、
  some-on × 5、可信 all-off} → 断言出口矩阵每格：Fault 二者 → `AdapterUncertain` +
  `AwaitAck` 无新 gen；Preflight 二者 → `Fault{PreflightFailed}` `AwaitAck`、无
  `Done{Passed}`、`rx_trust` 保持 false；estop_bound 二者 → `SafetyStopFailed{
  AdapterUncertain{kind}}` 带 gen、无 `Done` / `AwaitAck`、经
  `SafetyStopFailedAwaitAck` 等 ack 后终态；some-on × 5 三列 → `StopFailed`（同经
  `SafetyStopFailedAwaitAck`）；可信 all-off：Fault → `AwaitAck`、Preflight →
  `Done{Passed}`、Emergency（gen None）→ `Done`。序列：(i) `Emergency` 轮 Confirm
  通信错误 → CommFault gen n 发布、IPC commit `Fault(CommFault)`、phase / attempt
  保留、无第二次 `StopIssue`、出口 `SafetyStopFailedAwaitAck`（gen n+1）→ ack →
  终态；(ii) `Fault` 轮合并 EStop 后不可信 all-off → `SafetyStopFailed{
  AdapterUncertain}`（而非 `AwaitAck`）；(iii) `AwaitAck` 中 EStop +
  `AdapterUncertain` → 原语 `LatchOnly`（`serial_estop_pending` 置 1）→ serial
  claim 后零 IO 升级至 `SafetyStopFailedAwaitAck`（origin `EStopEscalation`），
  `S0State` 保持 Fault；**(iv) AwaitAck 重开保留 n**：可信 all-off 完成记录 +
  `AwaitAck(n)`，冻结 IPC ACK（`fault_ack = n−1`）→ EStop 原语 `StopIssued`
  （共享 word 已变 `StopCommanded`）→ serial claim 按本地 `AllOffTrusted` 记录
  命中 (d)，保留 ctx / `gen = Some(n)` / 原因位 / `FAULT_PENDING`，仅重开物理
  `StopIssue → Confirm`，不新增 gen → 再次可信 all-off → **仍 AwaitAck(n)**，
  持续冻结期间零后续 IO、不可 `Done` / `Idle`；解冻 ack(n) 后才清位并 Idle，
  `S0State` 保持原 Fault（可先以超时预提交 Fault 构造）。再测 ack(n) 在已 claim
  EStop 后、重开前或 `StopIssue` / `Confirm` 期间到达：不能提前清位或跳过物理
  收敛，完成后显式经 AwaitAck(n) 对账。重开失败升级 n+1 时，旧 ack(n) 不清
  新义务。启动 AwaitAck 也走 (d)：Connect 计数不增，BOOT / 原绝对截止保持，
  READY 不因旧 ack 提前发送。**(v)**
  estop_bound 轮发布过 RX-kind CommFault gen 后 `AllOffTrusted` 不可达（锁测）；
  (vi) EStop → `Emergency` → `enter_fault(AdapterErr)` gen n（`rx_trust` 不变）→
  可信 all-off → `TorqueOffConfirmed` + `AwaitAck(n)`（不 `Done`）→ ack(n) →
  `Idle(Fault 粘滞)`，`estop_bound` 全程保持；(vii) 任何 gen = Some 的轮次在 ack
  前不得到达 `Done` / `Idle` / 终态（全矩阵扫描断言）。
- **AC-20（启动行为变化 + `D_call` 安装）**：(a) 既有启动测试更新：spawn 时
  `safety_word = {TorqueUnknown, 0, 0, 0}`；经 `StopCommanded` 到
  `TorqueOffConfirmed`（mock 全 off + `Static` 或授权）而非直写；spawn 调用方线程
  零总线 IO（Mock 计数）；(b) 首个 Safety 实际预算 = `D_call`：Mock 令首个 write
  阻塞 > `D_call` → 启动 `StopIssue` 在 `D_call ± 5 ms` 内得 `Comm(IoWrite)` →
  `StopRetry`；序列记录首个 IO 之前 transport 已收到 `set_timeout(D_call)`；(c)
  安装失败：Mock `set_timeout` 注入 Err → `FeetechAdapter::new` Err → daemon 退 1，
  marker 仍 `0/0`、Mock 零 write / read。
- **AC-21（双状态 truth table + 两阶段写 + 屏障 + canonical 前置）**：每行一测：
  `0/0` 完整消费 → authorized；`0/0` `.bak` phase 1 失败 → 拒绝启动、命名空间仍
  `0/0`、下次可消费；`0/0` `.bak` rename 成功 + 目录 fsync 失败 → `authorized =
  false`、Dirty、Diag `MarkerDurabilityUnknown`、继续规范化 primary → 命名空间
  `1/1`（primary 成功）或 `0/1`（失败）；`0/0` `.bak` 持久 + primary 任一阶段失败
  → `authorized = false`；`1/*` → untrusted；`0/1`、`0/缺失`、`0/损坏` → untrusted
  且两份规范化；双缺失 → untrusted + 创建 `1/1`；非法内容 → untrusted；读权限错误
  → 拒绝启动；partial authorize（`.bak = 0` 后中断）→ untrusted；partial consume
  （`.bak = 1` 后中断）→ `0/1` → untrusted；authorize 非 canonical 初态穷举
  （`0/1`、`1/0`、`0/0`、`1/缺失`、`缺失/1`、双缺失、任一损坏、非法内容）→ 退 2 且
  两份字节不变，尤其 `0/1` 初态下永不出现 `0/0`；authorize `1/1` + `.bak` 任一阶段
  失败 → 退 2，primary 仍 `1`；authorize primary phase 1 失败 → 退 2，`1/0` → 下次
  untrusted；authorize primary rename 成功 + 目录 fsync 失败 → 退 3，随后启动消费
  成功（证明退 3 = 已生效）；revoke `.bak` phase 1 失败 → 退 2、初态不变；revoke
  `.bak` phase 2 失败 → 退 2，seam 模拟掉电回退 → 初态恢复（`0/0` 时下次启动可
  消费，证明退 2 必要）；revoke `.bak` 持久 + primary 任一阶段失败 → 退 4，
  `status` 报非 canonical（`0/1`），seam 回退 primary → 仍 `0/1` → 启动 untrusted
  并规范化为 `1/1`，再跑 revoke → 退 0；revoke 退 0 ⇔ 两份字节恰为 `1\n`（自每种
  非 canonical 初态各一测）；consume `.bak` 持久后 seam 回退 primary → `0/1` →
  untrusted；任一路径不得 `authorized = true` 除 `0/0` 两份均持久收紧。
- **AC-22（启动序列与 READY 序，t_start → READY 由 supervisor 强制有界）**：(a)
  `try_notify_ready` 与 auto-enable 均在启动出口之后；Dirty / Connect 失败时 READY
  前已观察到对应 Fault commit + `Disabled` + gen ack（序列断言）；(b) 启动期间并发
  `SetEnabled` dry/live → `GuardRejected`，`Done{Passed}` 后接受；(c) `auto_enable
  = true` + 非 Passed → 不发 `Event::Enable`；(d) 注入 serial 短暂停滞越过
  `T_boot(N)` 后恢复 → `Fault(CommFault, PreflightTimeout)`、本启动至少一次 Safety
  写、`completed_safety = Some(AdapterUncertain{Unknown})`、ACK 后 READY 仍发出；
  对照：在其 AwaitAck 冻结 ACK 并注入 EStop → 零 IO 升级 SSF，不记
  `InvariantViolation`；(e) `Preflight` 成功出口不进 `AwaitAck`、无 gen；(f) Dirty 仅
  一轮（无 AwaitAck 后新 EStop 重开）：总 Safety 写 ≤ 3、Confirm 一组、无第二次
  `StopIssue`；(g) 启动序列 = Safety
  写先于任何握手事务（序列记录）；(h) Connect 失败（各 kind）→ Safety 已写、
  Confirm 仍执行、`Fault{ConnectFailed}` code 按 D7、`adapter_connected = false`、
  READY 等 ack；(i) 启动期 EStop（StopIssue 前 / Connect 中 / Confirm 中）→ 只置
  latch + Diag + `estop_bound`，Connect 不中止、无 `Emergency` 上下文、总 Safety 写
  ≤ 3，且不可信出口为 `SafetyStopFailed`；(j) 串口 open 失败 → 退 1 且 marker 仍
  `0/0`（授权保留）；(k) Connect 失败后 marker 已 `1/1`（不复用）；**(l) supervisor
  执法注入**（各自独立，进程级 harness 观测退出码与时刻；"及时" = 检查点后 ≤
  100 ms；`t_start` = `main()` 首语句，harness 以进程 exec 时刻 + ctor 段近似，
  ctor 段单独计时并断言 ≤ 10 ms 作盲区大小登记）：(l-1) logging init / CLI parse
  阶段永久阻塞（seam）→ `CP1`（按 `N_max = 32` 计）后及时退 88；(l0) config 读盘
  永久阻塞（seam）→ `CP1` 后及时退 88、marker 未动；(l1) 串口 open 永久阻塞（Mock
  open seam）→ 同上；(l2) marker 两阶段写 fsync 永久阻塞（`MarkerFs` seam）→ 同上；
  (l3) `spawn_ipc_thread` 的 subscribers_factory 永久阻塞（`ready_rx.recv()` 不
  返回）→ 同上；(l3′) 七 worker 中任一（逐个，含占位 io）spawn 失败 → 既有退 1
  路径，或其无超时 `graceful_stop` 卡住时 `CP1` 后及时退 88；`SPAWN_REACHED` 仅在
  七者全部 `Spawned` 后置位；(l4) 主线程在 `SPAWN_REACHED` 之后永久停摆 → `CP2`
  后及时退 88；(l5) serial 永久冻结 → `CP2` 后：主线程协作式关停在 `T_abs + 1 s`
  内退 1，或（主线程亦停摆）supervisor 退 88；(l6) IPC 永久冻结 → 同 (l5)；(l7)
  任一 worker 不响应 stop 标志 → 主线程 join 截止后退 1，或 supervisor 于 `CP2`
  退 88；(l8) 正常启动 → `READY_REACHED` 后 supervisor 退出、无执法，且
  `parsed.resolve()` 后 `T_abs` 已按选中 hardware 的 `motor_ids.len()` 缩短
  （`N_ATOMIC` 断言；resolve 前 = `N_max = 32` 值）；(l8′) 以 N = 32 配置（结构
  上限）正常启动 → `T_abs` 不变仍为 15.9 s，supervisor 不提前执法；(l9) 主线程在
  READY 发送之前停摆 → `CP2` 后及时退 88，systemd harness 断言从未收到 `READY=1`；
  (l10) READY 发送失败（`NOTIFY_SOCKET` 指向不存在路径）+ `required` →
  `try_notify_ready` 得 `Err` → `READY_REACHED` 不置位 → 协作式退 1（≤ `CP2`），
  未 active；(l11) READY 发送成功 → `READY_REACHED` 置位 → supervisor 无执法退出、
  进程存活；(l12) path socket（harness 绑定 `NOTIFY_SOCKET` 路径 datagram）→
  harness 收到 `READY=1` 且进程 `Sent`；(l13) abstract socket（`NOTIFY_SOCKET=@x`）
  + `required` → `Err(Unsupported)` → 退 1、不置位；(l14) 纯 standalone（未设
  `NOTIFY_SOCKET`、未设 `INVOCATION_ID`、`--notify-mode auto` 或 `standalone`）→
  `NotConfigured` → `READY_REACHED` 置位、进程存活；(l15) systemd 环境缺 socket
  （`INVOCATION_ID` 设置、`NOTIFY_SOCKET` 未设置、`--notify-mode auto` 或
  `required`）→ `NotConfigured` → 退 1、不置位、从未 READY；**(l16) 电机数上限**：
  config 33 个唯一 motor id → `validate_hardware_section` `TooManyJoints` → 退 2，
  **零业务 worker spawn**（`boot_supervisor` 已在 `main()` 首句启动）、无总线
  IO、`N_ATOMIC` 仍为 32；CLI `--joints 33`（mock）→ parse 退 2，同样零业务
  worker spawn / 零总线 IO / `N_ATOMIC = 32`；N = 32 → 通过；所有执法情形：
  退出码 ∈ {1, 88}、∉ {86, 87}；协作式路径
  `set_policy(Disabled)` 被调且 `StopFlags` 已 spawn 槽位恰置一次；supervisor 硬
  路径零日志、零 join。
- **AC-23（EStop 幂等合并）**：分别验证活动物理轮与完成后的新请求：
  (a) 每 tick 连续注入逻辑 EStop 贯穿一次 `StopIssue` / `StopRetry` / `Connect`
  / `Confirm`，到首次出口前停止注入：总 `Safety` 写 ≤ `MAX_STOP_ATTEMPTS`
  （`AdapterErr → CommFault` 升级重跑除外，另 ≤ 3）、合并不重置 attempt、不延长
  单物理轮预算。(b) `AwaitAck` phase 内零 IO；新 EStop 命中 D4(d) 的可信完成
  记录时，**先离开 AwaitAck 再重开物理轮**，允许该轮 Safety / Confirm IO，
  保留 n 并按 AC-19(iv) 再次等 ACK；不可信记录零 IO 升级 SSF。不得把所有
  AwaitAck 新请求都断言为零后续 IO。(c) `SafetyStopFailedAwaitAck` / 终态收到
  任意次 EStop 均零总线 IO、零 serial policy 写、状态不变。(d) 跨 AwaitAck / Idle
  边界的 R 次有效重开分别计物理轮，每轮独立有界；不宣称无限请求流的总 IO 或
  总时长仅等于一轮。启动重开仍服从原绝对截止，Connect 总计一次。(e) `Idle`
  首次 EStop 按 (a) 创建 `Emergency`；AwaitAck 重开保留 ctx / gen；
  `estop_requests` 每次逻辑注入恰 +1（= 原语调用次数），serial / IPC 各自每检查点
  至多 claim 一次，次数各自 ≤ 注入次数（未消费的 level 位允许合并）。
- **AC-24（锁 + errno）**：(a) 两个 daemon 并发启动（同 robot-id、tmpdir）→ 第二
  实例**立即退 87**，marker 内容不变、Mock transport open 计数 0、零总线 IO；(b)
  daemon 运行中 `authorize` / `status` / `revoke` → 退 87、marker 不变；(c)
  `SIGKILL` 后锁由内核释放，`authorize` 可成功；(d) `authorize` 成功后 daemon 启动
  消费 → `authorized = true`；(e) unit 文件含 `RestartPreventExitStatus=86 87`
  且**不含** 88、ExecStart 含 `--notify-mode required`（deploy 单测读取 unit 文本
  断言）；(f) errno 分类：seam 注入 open `EACCES` / `flock` `EIO` / `ENOLCK` /
  `fcntl` 失败 → 退 1（非 87）且 marker 不变；`flock` `EWOULDBLOCK` → 87；`EINTR`
  一次后成功 → 正常启动。
- **AC-Q** PX-44 gate 纪律。

## hard boundaries（实施期不破）

1. §6.7 #2–#5 全保；#1 软件部分全保，硬件部分 OPEN。#3：不置 `safety_latched`、
   不写 estop latch、不新增清 latch 路径（marker 与 estop latch 相互独立；marker
   无运行期软件放宽路径，仅离线 `authorize`；SafetyStopFailed 亦不置任何 latch；
   EStop 的 `estop_latched` 守卫与 latch 文件在所有情形始终持久化）。#4：marker
   写两阶段 strict + 屏障。
2. `00_design.md` 0 改；偏离（Safe/Ready → Fault(CommFault)、`SafetyStopFailed`
   生产点 + `SafetyStopFailedAwaitAck` + `EmergencyBatch` 分派 + `safety_word`
   单 strong CAS 双位发布 + `normal_io_active` acquire-CAS + claim 语义 + 状态机 /
   classifier 优先规则 `(Fault(_), EStop) if rt_serial → 原 Fault + Disabled`、
   confirm 契约修订、EventQueue 五 lane 与 admission / 预算 / 调度表 + rr lane
   第二道 deadline 检查、mailbox 每 tick 服务 / 单词发布顺序与 IPC 唯一 ack、
   AwaitAck 完成记录与保留 gen 的物理重开、serial policy 条件 CAS 写权、封顶语义、
   `PendingDeadline` 分型、启动序列 `StopIssue → Connect → Confirm`、
   `adapter_connected` 语义扩展、daemon 顺序与退出码 87 / 4 / 88、
   `boot_supervisor` 自 `main()` 首语句强制 `_exit(88)` + `N_max = 32` 起算 +
   `TooManyJoints` / CLI 上限 + pre-main 盲区、READY 真送达提交点 +
   `--notify-mode`）走 CHANGELOG + `10_verification.md`。
3. `ManipulatorAdapter` / `OutputSafetyProbe` / `SerialRtAdapter` 0 改；
   `ExecutorCtrlClient` 仅既有 additive default（`abort_pending_normal`），不加
   任何 latch-only 方法；新 trait `SerialEmergencyCtrl` 为 robot-core additive，
   实现于共享类型 `HardRtCtrlClient`，隔离靠 daemon `Option` 注入（构造期不变量，
   factory 分派测试锁）；`SerialTransport` additive 方法 + `set_timeout` 语义修正；
   feetech `new()` 预填与 `call_deadline` 安装为 crate 内部改动；`ipc_thread` 外部
   API 由裸 `Sender<Event>` 改 typed `ExternalEvents`（crate 内部）；`Guards` 增
   `rt_serial`（crate 内部）；`HardwareSectionError` 增 `TooManyJoints`（crate
   内部）；robot-health 仅 additive `try_notify_ready`（旧 API 0 改）。
4. wire：无新 method / schema / message / field；既有 enum +1；proto / gateway /
   client 预算栈 0 改；本地子命令与 `--notify-mode` 不属 wire；unit
   `TimeoutStartSec=infinity` 保持，`RestartPreventExitStatus=86 87`（88 可重启），
   ExecStart 增 `--notify-mode required`。
5. HardRT `rt_thread` 0 行为改；`mock_hard_rt` 0 行为改（`mock_serial_rt` 走
   `Static` + 启动序列，AC-20；HardRT 无 `SerialEmergencyCtrl` 句柄、EStop 走既有
   `set_estop()` + `safety_stop()` 并通配转 `Safe` + `StopTorque`、沿用 legacy
   `safety_state` 字节 / `emergency_action` / EventQueue `estop_pending`）；既有
   `estop_latch` 语义 0 改；deploy guard 六步与退出码 86 语义 0 改；正常关停路径
   `graceful_stop` 与 `Stop` 物理语义 0 改。
6. 不并入：进程内恢复 / 事务身份、候选 E 的 D15 / D14、跨臂联锁、逐电机降级、
   S0 ClearError RR、F1-B、RR 非阻塞派发 (b)、ipc 线程 RT class 化、iceoryx2
   订阅队列配置、`Stop` 物理断扭、process-wide `SIGSTOP` / 调度饥饿下的启动上界、
   pre-main ctor 段、abstract notify socket 支持、配置层收紧 N ≤ 13（不做；以
   `MAX_JOINTS = 32` 为上界并在 resolve 前置校验）。
7. S0 永不因通信故障主动退出。启动期例外：任何总线 IO 之前——锁并发退 87
   （不重启）、配置 / 电机数越界退 2、锁系统错误 / 串口 open 失败 / `call_deadline`
   安装失败 / marker `.bak` 收紧 phase 1 或读取权限失败 / 任一 worker spawn 失败
   退 1；与总线 IO 前后无关——协作式 pre-READY 超时 / READY `NotConfigured`
   （required）/ READY `Err` 退 1、`boot_supervisor` 在 `CP1` / `CP2` 的
   `_exit(88)`（进程内部停摆，非通信故障；S-18 / S-20 / S-24 / S-25 / S-27 / S-29
   / S-32）。停喂只由 lease 化 liveness 判定的真停滞触发。
8. 不宣称任何 RX 隔离边界；失步会话内权威状态永不 `TorqueOffConfirmed`；信任恢复
   不靠重启；**缺失 / 损坏永不构成授权；authorize 只从 canonical `1/1` 出发且其
   Err 不蕴含"未生效"；revoke 退 0 严格 canonical、退 4 须重跑；consume 未持久屏障
   不宣称收紧；EStop 义务不随 code 升级降级；`gen = Some` 的轮次无例外必经 ack
   等待；`Enable(false)` 豁免不豁免 Fault / gen / ack；本地 deadline 不破坏 RX
   信任、RX kind 必破坏；SLA-B fast-path 值不称全路径上界；`Stop` 只宣称控制面
   收尾；外部 EStop 不是 process-abort 保证；supervisor 上界口径为 "t_start →
   READY"、不覆盖 pre-main ctor 与 process-wide 停摆、resolve 前按 `MAX_JOINTS`
   上界且该上界由前置校验保证；IPC 侧 `safety_word.state` 严重度经单 strong CAS
   只升不降、权威状态 CAS 后零写；每逻辑 EStop 恰一次原语、两 consumer 各自
   claim；t1 至本轮收敛完成无任何 normal 事务启动（`read_state` 在内；claim 前
   acquire-CAS、claim 后 phase / inhibit 门连续覆盖）；
   SerialRT 下 EStop 不清粘滞 Fault 且 plan 给出 `Disabled`，serial 仅以条件 CAS
   收紧 policy、绝不覆盖 `Disabled`；AwaitAck 重开保留 ctx / gen / ACK 义务与启动
   绝对截止；连续 EStop 下每 tick 仍服务一次 mailbox，ack 只绑定实际快照；`Sent` 只表示
   datagram 真发送成功，`NotConfigured` 只在 standalone 模式算 READY；RR 上界不
   宣称与 wire 2 s 的严格关系，且过期请求在 rr lane 第二道检查零副作用拒绝；CAS
   进展为 lock-free，不据单次失败宣称进展、不称 O(1)**。
9. 实施期若须改 `ManipulatorAdapter` 签名或 design 语义 → Reverse Sync。

## 拍板清单（reviewer round 21：Spec Approved）

| # | 项 | 推荐 | round 21 状态 |
|---|---|---|---|
| 范围 | 进程内 reopen/resync 移出；信任 = 显式双状态一次性授权 + 两阶段写 + 持久屏障 + canonical 前置 + 非阻塞锁 | 是 | 已 ACK |
| D1 | 预算模型 + `T_connect` / `T_pre` / `T_boot(N)` / `B_startup_ack` / `T_abs`（`N_max = MAX_JOINTS = 32` 起算，resolve 后按 `motor_ids.len()` 缩短，上限由 `TooManyJoints` / CLI 校验前置保证）；"t_start → READY 上界"口径 + pre-main 盲区；RR 8 + 6 = 14 tick + 第二道 deadline；`MAX_STOP_ATTEMPTS = 3`；`B_hw_confirm` 525 ms | 是 | 已 ACK |
| D2 | `D_call` 50 ms 自 `FeetechAdapter::new` 安装；TX timeout 归 `IoWrite` | 是 | 已 ACK |
| D3 | 事务级抢占 + 六类操作域 + 矩阵（`Enable(false)` 仅物理豁免；timeout lane 以 `PendingDeadline` 记账；normal 类事务起点 acquire-CAS）；seq ≥ 1；lease 化 liveness | 是 | 已 ACK（normal IO acquire-CAS 已闭合） |
| D4 | 启动序列 `StopIssue → Connect → Confirm`；出口矩阵按 `estop_bound` 分列且 gen 决定 ack 等待；`SafetyStopFailedAwaitAck`；mailbox 单词 + 发布顺序 ①②③ + IPC 唯一 ack；`safety_word` CAS 提交（保留请求位与 `normal_io_active`）；EStop claim 语义；`TRUST_CLEARING` 集合；`evidence_trusted` 封顶 | 是 | 已 ACK（AwaitAck 完成记录 / 保留 gen、policy 写权、mailbox 调度闭合） |
| D6 / D8 | 无阈值首次定级；drain 仅 hygiene | 是 | 已 ACK |
| D9 | backup-first 持久屏障 + authorize canonical 前置 + 退出码 0/2/3/4 + `status`/`revoke`；`LOCK_NB` errno 分类 + 退 87/1；`SerialTrustPolicy` 穷尽表 + 同点分派（构造期不变量，已 ACK）；open 先于消费；`boot_supervisor` 自 `main()` 首语句、`N_max = 32` 起算、resolve 后 `N_ATOMIC`（`TooManyJoints` 前置）、wait-free `_exit(88)`、`CP1` = required worker 全部 `Spawned`（恒七，已 ACK）；READY 提交点 = `try_notify_ready` 三态 + `--notify-mode`；`adapter_connected` enable-ready 语义 | 是 | 已 ACK |
| D11–D14 | 守门三元组（SerialRT 下 EStop 不清 Fault）；fast lane（每逻辑 EStop 恰一次原语；t1 = CAS 返回 = 软件门闩）+ SLA-B 分列；EventQueue 五 lane + `EmergencyBatch`（notice claim + mailbox 快照）+ `estop_monotonic` 单 strong CAS 双位发布（CAS 后零写、lock-free）+ `normal_io_active` acquire-CAS 全序禁写 + 状态机 / classifier 优先规则 + 完整 tick 调度表（含 rr lane 第二道 deadline 检查，已 ACK）+ 两级 admission + typed `ExternalEvents`；`Stop` 仅控制面收尾；RR 入队后建 pending | 是 | 已 ACK（policy 写权 / 连续 EStop mailbox 服务闭合；AC-9(o7) 措辞随收口同步） |

## 后续（RIPER）

Spec Approved → Plan outline 待审 → 函数级 Plan round → 明确 Plan Approved 后实施。
当前仅准备规划材料，未批准 Plan，未进入 Execute。

## Plan outline（待审；未获 Plan Approved）

### P.0 交付边界与组织

- 输入：上文已批准的 D1–D15、AC-1–24 / AC-Q 与 hard boundaries；代码基线
  `bd081cb`。本节只拆实施顺序与验收边界，不重新选择方案。当前所有工作包均为
  **未执行**；正式 Plan 须展开完整签名、调用点、测试文件及逐步退出条件。
- 目标交付：拟在 `feat/post-e-comm-fault-hardening` 完成一套可整体评审、整体回滚
  的变更。按下列工作包组织提交；代码接线未齐、验证未通过时不部署任何中间版本。
  当前未创建分支、未 commit、未部署。
- 依赖方案：沿现有 workspace 依赖，预期不新增外部 crate、不变更 wire 方法或字段；
  schema 只加既定枚举值。新增内部依赖 / feature 若确有需要，正式 Plan 列出并附
  timing 验证要求，不在实现时临时引入。
- 文件所有权：Dev-core 管公共类型 / 共享 ctrl / serial；Dev-adapter 管 feetech；
  Dev-ipc 管 IPC / 状态机 / pub / RR；Dev-ops 管 daemon / config / health / 信任
  文件 / unit / runbook；QA 管独立验收夹具与报告。共用 `lib.rs` / `Cargo.toml`
  由 Tech Lead 串行整合；同一文件始终只有一名修改 Owner。跨工作包复用文件仍由
  原 Owner 修改，不按阶段更换写者。安全复核贯穿原子协议、信任落盘与上线回滚。

### P.1 公共契约、原子槽与诊断基础（Dev-core；先行）

- 现有落点：`crates/robot-core/src/error.rs`、`executor_ctrl.rs`、`factory.rs`、
  `lib.rs`；`crates/robot-s0/src/hard_rt_ctrl.rs` 的 `HardRtCtrlInner` /
  `HardRtCtrlClient`、`serial_io_ctrl.rs` 的共享别名；`crates/robot-msg/schemas/joint.fbs`
  的 `S0FaultCode`；既有 `crates/robot-msg/build.rs` 生成链路沿用。
- 落 `CommErrorKind` / `AbortReason` / `SafetyStopFailDetail`、编码表、40 位有效
  seq 约束、active / completed / abort 槽、单词 mailbox / ack、inhibit 与
  `safety_word`；穷尽同步所有受新增 enum 影响的 match，不留下可编译性断点。
- 已定接口：`SerialEmergencyCtrl::estop_monotonic(&self) -> EStopOutcome`；
  `ExecutorCtrlClient::abort_pending_normal(NormalAbortTarget)` 的 additive default 例外按
  hard boundaries 第 3 条展开；其返回型和全体实现 / mock 转发在正式 Plan 钉死。
  EStop 不给 ExecutorCtrlClient 增加方法。`safety_state()` 按构造期 ctrl_kind
  选源；HardRT legacy 行为保持。
- `DiagSink` / `DiagEvent` 放 robot-core，Lamport SPSC 实现拟放
  `crates/robot-s0/src/diag.rs`，serial 单生产者、pub 单消费者、满时 drop-newest。
  新模块路径均为规划，当前不存在实现。
- 退出证据：AC-7/18 编码与 wire 映射、AC-4(h) seq 边界、AC-8(i) mailbox 发布
  顺序、AC-9(o/q/r) 原子交错、AC-10 环协议；独立 QA 先锁可重复调度 seam。
  风险：共享 ctrl 同时服务 HardRT；必须先验证构造分派与 legacy 读写源隔离。

### P.2 有界 transport、事务抢占与工厂接线（Dev-adapter；依赖 P.1）

- 现有落点：`crates/robot-adapters/feetech/src/transport.rs` 的 `SerialTransport`
  / `RealSerialTransport::{open, read_exact, write_all, set_timeout}`；`bus.rs`
  的 `FeetechBus::{new, connect, read, write, sync_read, sync_write}`；`adapter.rs`
  的 `FeetechAdapterConfig`、`new`、enable / hold / safety / probe 路径；
  `factory.rs::FeetechFactory::build_concrete`、`error.rs`、`registers.rs`。
- 安装 `D_call` 的三个构造落点一起生效；分段读写累计同一次调用 deadline，保留
  `got` 真值与 TX/RX 分类。新增 discard / drain 仅作 hygiene；不增加 reopen。
  `PreemptProbe` 按六类操作域作用于事务起点与分片 sleep，已发请求的响应不能拆断。
- 工厂当前 `AdapterBuildContext` 只有配置借用；正式 Plan 须给出 additive 的
  构造期服务载体，将同一 clock / preempt / diag / call_deadline 从 daemon 送到
  transport / bus / adapter。顺序是先建共享 ctrl 与服务，再 open / 构造 adapter，
  最后消费 marker、spawn 业务 worker；不采用运行期 downcast 或新 adapter 方法。
- `new()` 预填 joint_ids / safety_stop_entries 并核校准，保证首个 Safety 写不依赖
  connect 成功；所有安装失败均在 marker 消费前退出。改 mock transport 与既有
  `tests/integration_smoke.rs` 夹具，QA 另维护故障序列验收。
- 退出证据：AC-2/3/4/6/13/20 的 transport、census、不可拆事务、信任清零 kind
  与构造失败路径。风险：legacy timeout 语义改变影响所有调用点，需全量 census。

### P.3 serial 收敛、retire 与 ACK 生命周期（Dev-core；依赖 P.1/P.2）

- 现有落点：`crates/robot-s0/src/serial_io_thread.rs` 的
  `spawn_serial_io_thread`、`run_serial_io_loop`、`execute_serial_normal`；
  纯 phase 逻辑拟抽到 `serial_convergence.rs`，ctrl 协议仍由 P.1 的 Owner 维护。
- 移走 spawn 调用方的同步 connect 与假定 confirmed；按 D4 建完整 phase / 路由 /
  出口矩阵，覆盖 Preflight / Fault / Emergency。完成记录、estop_bound、fault
  severity / gen / ack 分别管理；AwaitAck 重开保留旧义务和启动绝对截止。
- normal 事务 acquire / release 覆盖读、写与 effectful action；超时 retire 按
  四记录分支对账，物理豁免不豁免 Fault / gen。`enter_fault` 先更新权威状态 /
  inhibit，再发 mailbox；SSF 两 phase 均零 IO / 零 policy 写。
- 已定内部签名：`fn tighten_estop_policy(policy: &AtomicU8)`；仅 StopIssue 且
  estop_bound 时调用。清 claim 位不附带 policy 尾写；纯 Fault 收敛零 policy 写。
  接入有上限的 liveness lease 与 serial Diag 生产点。
- 退出证据：AC-4/5/6/8/9/16/19/20/23 的逐转移与交错矩阵；先用注入时钟、冻结
  ACK 和事务序列器确定性验证，再接真实 IPC。风险：误丢旧 gen、提前 READY、
  旧 ack 清新代次、重试刷新截止；这些反例作为本包退出门。

### P.4 IPC 状态机、事件调度与观测（Dev-ipc；依赖 P.1，与 P.3 联调）

- 现有落点：`crates/robot-s0/src/state_machine.rs` 的 `Guards` / `EventQueue` /
  `try_transition` / `classify_transition`；`ipc_tick.rs::ipc_tick` 与 guard
  副作用；`ipc_thread.rs` 的 `spawn_ipc_thread`、ingress / RR drain / resolver；
  `set_enabled_action.rs`、`clear_estop_action.rs` 的 guard 接缝；`pub_thread.rs`。
- 落五 lane / 两级 admission 与完整 Step 0–5 调度；topic 或 ExternalEvents
  恰一次发布，IPC Step 0 claim notice，Step 1 emergency 后仍必经 Step 1b。
  SSF 与非 SSF 按快照代次 ACK；单调 Fault commit 后 Disabled；旧 pending 不复活。
- typed ExternalEvents 替换裸 Sender；RR source / lane 两道 deadline、容量常量 /
  断言 / 入队与 pending 创建顺序同步两 RR server（`rr_server_thread.rs`、
  `set_enabled_rr_server.rs`）；不并入非阻塞派发方案。
- ClearError / adapter_connected 守门依 serial 原子推导；resolver 补三类 Fault。
  pub 的 `publish_joint_state` 当前直接读 legacy safety_state，须同步改选源，
  不能只改 client accessor；`map_state_to_wire` 补 CommFault，消费 Diag 且不做 adapter IO。
- 退出证据：AC-7/8/9/10/11/14/17；独立验证连续 EStop、迟到 SSF、Fault EStop
  classifier、过期 RR 零副作用与满 lane / 在飞 pending。风险：提前 return 饿死
  mailbox、错误 ACK 或观测读旧源；本包与 P.3 联调通过后才有运行期完整闭环。

### P.5 启动监督、信任文件与 READY（Dev-ops；辅助模块可与 P.2/P.3 并行）

- 现有落点：`crates/robot-s0/src/bin/s0_daemon.rs` 的 `main`、`Args::{parse,
  resolve}`、`run`、`build_adapter`、`ExecutorHandle::make_tick_observer`；
  `config_loader.rs::validate_hardware_section`；`crates/robot-health/src/systemd.rs`。
  拟新增 `robot-s0/src/rx_trust.rs` / `boot_supervisor.rs`，并在 lib.rs 注册。
- `try_notify_ready() -> ReadyOutcome` 是 additive；旧 notify API 保留。main 首句
  supervisor、MAX_JOINTS 前置校验、N_ATOMIC 只缩短、CP1/CP2、退出码、notify
  三模式按 D9 接线。信任锁、两阶段 marker 与 authorize/status/revoke 共用文件
  后端；`MarkerFs` 测试缝与真实文件后端分离，原 estop_latch 实现不改。
- 七槽 StopFlags 必须在业务 spawn 前预建，逐一贯穿 IPC、两个 RR、心跳 watchdog、
  executor、pub、io 的 spawn / handle。各文件由原 Owner 改签名；不能在取得
  handle 后才登记 stop，IPC ready 握手可能先阻塞。既有健康 monitor、测试 freeze
  辅助线程、boot_supervisor、READY 后的 systemd watchdog 不加入 CP1 七 worker。
- 同一 SpawnPlan 分派 ctrl_kind / Guards.rt_serial / Option<SerialEmergencyCtrl> /
  SerialTrustPolicy；open 与 D_call 安装先于授权消费，启动物理轮先于 READY /
  auto-enable；提前 ACK 不能绕过重开的物理轮。正常关停原路径保留。
- 完整接线依赖 P.3/P.4；退出证据为 AC-14/15/20/21/22/24，覆盖 partial rename /
  fsync / 锁 errno / 永久阻塞 / worker 拒退 / READY 真发送及启动重开。风险：
  marker 掉电回退与不可回收线程；必须同时有文件故障注入和子进程退出证据。

### P.6 验证总装与质量门（QA；覆盖 P.1–P.5）

- 现有基线：`crates/robot-s0/tests/serial_rt_e2e.rs`、`rt_guard_soak.rs`、
  `deploy_units.rs` / `deploy_guard_kill.rs`、`robot-rpc/tests` 的 EStop / SetEnabled /
  state / workflow E2E、robot-health watchdog 测试、robot-tests 既有 smoke。
  P.0 实施基线先登记实际测试名、feature、工具链与退出码，不照抄历史测试计数；
  AC-1 的既有 integration / smoke 须在正式 Plan 映射到具名清单与 feature；
  `dimos_workflow_e2e` 的历史 12 项包含公共 subprocess 测试。
- 拟新增 `crates/robot-tests/src/bin/comm_fault_e2e.rs` 与独立夹具模块；本机 mock
  与 Orin 测量分开入口，具体 feature / CLI / required-features 在正式 Plan 钉死。
  新 target 当前不存在；注册时同步 Cargo 活清单、feature 单独构建检查与 CI 对应
  入口，不能仅靠 all-features 编译掩盖缺依赖。
- 分层验收：纯类型 / phase / 原子交错用 MockClock 和 seam；Linux mock 子进程验
  mailbox→RPC/wire、启动 supervisor / READY / 文件与锁；Orin 验 AC-2(d) 的设备
  characterization、AC-12 的 P99 / CAS 重试分布（每场景 100 warmup + 1000 样本）、
  AC-13 的真实广播。systemd harness 需要可用的 user systemd；环境缺失不记 PASS。Mock 通过不
  替代 Orin 数据，characterization 不冒充硬门槛。
- 每 AC 输出 `.verify/<AC>/result.json` 与所需日志 / 序列 / 延迟分布，汇总到
  `.verify/summary.json`；状态区分 PASS / FAIL / NEEDS_HUMAN。QA 独立维护验收
  场景，Dev 不以删减 AC 或只断实现内部值替代外部行为证据。
- 正式执行质量顺序：fmt → clippy → test → build；依赖 release 子进程产物的
  E2E / smoke 在显式构建完成后再验。当前只是列门禁，未运行任何 Cargo 检查：

  ```bash
  cargo +nightly fmt --all --check
  cargo +nightly clippy --workspace --all-targets --keep-going -- -D warnings
  cargo +nightly clippy -p robot-tests --all-features --all-targets --keep-going -- -D warnings
  CARGO_BUILD_WARNINGS=deny cargo +nightly test --workspace --all-features --lib --bins
  CARGO_BUILD_WARNINGS=deny cargo +nightly test -p robot-transport --no-default-features --features mock
  CARGO_BUILD_WARNINGS=deny cargo +nightly build --release --workspace
  ```

  PX-44：clippy 前强制重新检查本地编译单元，以进程退出码判定，不数告警文本；
  不清全局 Cargo 缓存。正式 Plan 补显式 release feature/bin 清单，再运行完整
  workspace / feature / transport 隔离测试、cross-process E2E 和 smoke；它们需要
  的产物必须来自当前 checkout，不能命中旧 release。既有 PX-24 九个单 feature
  gate 加新 feature；每次 commit 前实际执行 fmt + clippy。依赖或 feature 变动
  追加 `--timings` 报告；deny / allocation guard / schema compatibility 列为最终
  交付门，不把 CI 尚未配置的任务称为已在 CI 执行。

### P.7 部署、回滚与文档收口（Dev-ops + QA；依赖全部软件验收）

- 文件：`scripts/systemd/xlerobot-s0-{left,right}-arm.service`、
  `README_dimos_replace.md`、`docs/CHANGELOG.md`、`docs/10_verification.md`；
  单元模板测试复用 deploy_units。unit 增 required notify 与退出码 87，保留 88
  可重启、原 guard 六步与 TimeoutStartSec=infinity。
- 迁移清单包含双臂各自 marker / lock 目录与运行身份、旧部署产物和 unit 的可核验
  备份、一次性授权程序、READY/guard 拒绝检查、监测与失败退出条件；目录权限、
  安装命令及每臂顺序在函数级 Plan 落实。首次缺失 marker 必须 Dirty，不能为了
  部署便利自动生成授权。真机动作不因 Spec Approved 自动获得授权。
- **回滚单元为完整候选**：开发失败回到工作包前提交；不得局部撤掉新 guard /
  mailbox 却保留上层策略。部署失败先停止新实例并由现场确认断扭，再整体恢复已
  验证的二进制、unit 与配置；marker 不删除、不复制旧授权快照，重新启用候选须
  执行 D9 授权程序。旧版本不理解 marker，不能把“回退到旧版”称为信任恢复。
- 软件回滚不能撤回已发生的硬件副作用；D15 OPEN，旧栈 unit 保留可回滚，正式退役 /
  跨臂联锁 / reopen 均不并入。CHANGELOG 与验证文档登记已批准偏离，00_design.md
  不改。完成实际验收后才补 Execute / Review 记录、更新 CodeMap，并生成
  `_human.md` / `_llm.md` Archive；当前不生成实施完成记录。

### P.8 正式 Plan 需要补齐的具体项

1. **接口与构造链**：把每个新增方法完整签名、字段内存序与所有调用点列齐；按
   hard boundaries 第 3 条解释 abort additive default 例外，AC-1“无新方法”的
   文字须同步限定该例外，不能据此给 ExecutorCtrlClient 增加 EStop 方法。
2. **原子改动边界**：钉 P.1 新 enum 的所有 match 消费者、P.2 工厂注入载体、P.3
   事务 lease 的取得 / 归还与异常退出、P.4/P.5 七 worker 参数接线；逐组可编译，
   跨文件联动未齐时不切换生产入口。
3. **验收可运行性**：AC 到具名测试的一一映射、comm_fault_e2e feature/CLI、所有
   release 子进程产物及 PX-24 清单、Orin 采样流程、证据路径和通过条件。
4. **上线与回滚**：精确安装 / 停启 / 备份恢复命令、双臂串口与身份匹配、marker
   权限与人工授权步骤、回滚验证与故障处置；不把现场手动项标成自动 PASS。

以上为可评审的提纲与正式 Plan 输入；**尚无 Plan Approved，全部实施工作包未执行**。
