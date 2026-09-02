# CodeMap · post-E 通信故障硬化（Pre-Research，不开 Spec/Plan/实现）

> 代码基线：`fed7549`（候选 E 段(10) 收口后）。方法：纯读码 + 已登记证据，
> **零代码改动**。
>
> **范围（Reverse Sync 拍板 2026-09-01，PX-58）**：F1（S0 内预算倒挂）+
> F2（fault 路径零日志）+ F3（Ack 超时三元契约）+ 评估文档 §6.2 P0-2
> （通信故障升级链：连续读写错误阈值 / Fault·断扭语义 / RX 重同步 /
> 有界串口 reopen / 快速错误也须触发）。
>
> **边界（相邻候选不并入，只登记接缝）**：D15 硬件 fail-safe（OPEN）、
> D14 四检测器（post-E 另单，两 P0 前置阻断在案）、跨臂 daemon-independent
> 联锁（评估 §6.2 P1-4）、逐电机降级（§6.2 P2-5）。
>
> 输入文献：`docs/CHANGELOG.md` 2026-09-01 Reverse Sync 条（F1 名义窗口
> 核算表）/ `docs/handoffs/v0.5_demo_run_2026-09-01_summary.md` §3（4L-①
> 取证）/ `docs/handoffs/v0.5_motor_communication_resilience_assessment.md`
> §3.2·§4.2·§6.2（加固前快照）/ Spec P.4 PX-58..60。

## CodeMap amend 记录（round 1；reviewer 复核后）

- **P0-1**：§0.1 原"每层倒挂"结论**方向性错误**——client 4s > 3s > 2s >
  ipc 1s 是正确嵌套；改为"8 行预算栈 + **单一倒挂边界**（ipc 1s →
  serial 无预算/enable 无界）"；gateway 行标注透传非独立层；4L-①
  改"与该路径一致，未唯一归因"。
- **P0-2**：S-2 证据链**整体更正**——`clear_estop_action.rs:222` 是
  测试 fixture；真实链 = 生产初值 true（`s0_daemon.rs:628`）+ 事件
  自置 true（`ipc_tick.rs:388`）+ commit_fault 不动 guards →
  **ClearError 自我授权、恢复门无 adapter 健康校验**；"EStop 绕道
  锁定"结论撤销（demo 绕道系 client finalizer 置 latch 所致）。
  §0.3 #5 / S-2 / Q5 重写。
- **P0-3**：S-1 扩程——阻塞源 = loop 内**任何**同步 IO（confirm /
  normal action / write_command / read_state / safety_stop 自身
  write + confirm read），非仅 enable；方案 C 只消除子步间阻塞、
  在途底层 IO 不可中断；β 不构成即时自收敛；§4.6 <10ms 约束对象
  待 Q11 澄清，**撤"SLA 不成立"断言**。
- **P0-4**：SerialRT 生产 tick = **200 Hz**（`serial_io_thread.rs:133`
  默认 5 ms；CLI `--tick-hz 50` 不进 SerialRT cfg，`s0_daemon.rs:881`）
  ——三处 50 Hz 修正；"连续 N 次 = N×tick 墙钟"不成立 → 阈值时基
  两选项另拍（Q14 新增）。
- 补充：S-3 补拍板项 Q13；S-4 收窄为进程内 packed atomic 约束；
  S-5 标注 last_completed 会被覆盖非持久材料；"仓内唯一模板"收窄为
  "同一 serial 控制路径最接近先例"。
- reviewer 确认成立的主结论：同步 enable 阻塞、F2 四零日志点、
  P0-2 四件套全缺。

## CodeMap amend 记录（round 2；reviewer 复核后）

- **P0-1**：§1 方案矩阵自相矛盾修正——C 列"超时可真中止、副作用继续
  一元消失"改为"后续子步可停止、一元**收窄**（在途底层 IO 跑完、已完成
  子步不回滚）"；"C ⊃ B"改为 B/C **共享基础、可组合、互不包含**；B 列
  "需 wire additive 评审"改为"内部进度回报不改外部 wire，仅暴露到
  RPC/topic 才需"（与 S-4 对齐）；§2 末条"可被消除"同步改"可被收窄"；
  表头 C 由"可中断 enable"改"子步化 + 步间可抢占 enable"。
- **P0-2**：Q11 四件扩**五件**——补"排队/唤醒段"（drain 位于每 tick
  `sleep_until` 之后，生产周期 5 ms + 调度唤醒延迟，<10 ms 判定不可
  省）；S-1 按五段重排。§1 C 列 `T_wait ≤1 s` 伪硬界改"配置预算
  1 s/次、非硬上界"。
- **P1-3**：§0.4 "`--tick-hz` 只进 ipc/rt"改"只进 HardRT
  `RtThreadConfig`（:850），IPC 固定 `poll_interval_ns: 500_000`
  （:638）"；§7 `clear_estop_action.rs:222` 改标"测试 fixture、round 1
  误判来源"，并补 `s0_daemon.rs` 引用行。
- reviewer 确认闭合：预算单一倒挂、ClearError 自我授权、200 Hz、
  S-3/Q13、S-4/S-5 收窄。

## CodeMap amend 记录（round 3；reviewer 复核后）

- **P0-1**：S-1 "请求只是置原子 flag"**不符实现**——`safety_stop()` 在
  client 端同步完成 policy=StopTorque → latch → StopCommanded → drain
  pending → emergency_action 五步（`hard_rt_ctrl.rs:142`）；改为"软件
  门闩立即生效 / 物理断扭仍延迟"两层表述；Q11 起点由"请求入原子槽"
  细分为四个候选点。
- **P0-2**：§1 (ii) "22 × T_wait + sleep"**不是上界**（22 仅 `read_exact`
  数，`write_all` 未纳入）；(ii) 改为"全量枚举 read/write/重试/sleep +
  每高层调用统一硬 deadline + 推导 action 级硬上界"三步，旧式仅留作
  名义倒挂见证。
- **P0-3**：Q13 "Safe/Disabled"混淆 `S0State` 与 `OutputPolicy`（仓内无
  `S0State::Disabled`，`state_machine.rs:101`）；改为三元组拍板：目标
  S0State + OutputPolicy + adapter 动作/最终扭矩证据，"Safe + Disabled
  只停写命令，不保证断扭"。
- **P1**：§0.4 "串口层持续失败不触发任何 Fault"过宽——normal action Err
  已提交 `Fault(AdapterErr)`（`ipc_tick.rs:199`）；收窄为"周期性
  write_command/read_state 连续失败只计 metrics，不进升级链"。

## CodeMap amend 记录（round 4；reviewer 复核后）

- **P0-1**：S-1 全链遗漏 **IPC 侧排队/调度预算**——EStop 入队
  （`ipc_thread.rs:694`）后还要过 watchdog / 外部事件 / 两路 RR 四个
  `while let Ok` drain（无每 tick 计数上限）才到 `ipc_tick`（:920），
  错过本轮 poll 再加 500 µs sleep（:1121）+ 普通线程调度延迟；五段扩
  六段，Q11 排队段拆为 source → EventQueue → `safety_stop()`/
  emergency_action → SerialRT drain，并新增"RR drain 饥饿上界"核定项。
- **P0-2**："bus write 完成"不是当前可观测终点——生产 `write_all` 仅调
  `std::io::Write::write_all`，无 UART drain（`transport.rs:227`）；终点
  改三选一：host `write_all` 返回（仅调用完成）/ UART on-wire 完成
  （无观测）/ `Torque_Enable` readback 全关（强证据）。
- **P1**：Q2 补"完整调用枚举 + action 级硬上界数值"为必拍子项；Q11
  起点"四选一"改准为六候选（入口与返回是两个点，另拆出 source 到达）。

## CodeMap amend 记录（round 5；reviewer 复核后）

- **P0-1**：RR drain 体内对 bounded(64) response channel 调**阻塞
  `send`**（`ipc_thread.rs:727/:783` 等 12 处）此前未登记；当前不阻塞靠
  隐含不变量 `max_in_flight`=64 ≤ response 容量 64 + 每 request ≤1
  response + 双 channel 独立，三个 64 无共享常量/断言/测试锁定。S-1 ①
  登记证据链，新增 **Q15** 二选一（锁不变量 vs 有界非阻塞），CodeMap
  建议 (a) 锁不变量为本候选 P0 最小件。
- **P0-2**：EStop source 误写"iceoryx2 topic / RR"——S0 RR 只承载
  ClearEStop/SetEnabled；路由改准为 gateway/S1 → `/events/estop` topic →
  `poll_estop` → EventQueue（`00_design.md` §6.6），起点改为七个可观测
  点并给出裁决范围（S0 内从 subscriber/EventQueue 起算；从 gateway RPC
  起算须纳入 `estop_pub_thread` 链路）。
- **P1**：三处 drain 区间 ":697-:781" 改精确 watchdog :697 / 外部 :701 /
  ClearEStop :725–763 / SetEnabled :781–862；非阻塞生产者补
  `set_enabled_rr_server.rs:323`。
- ACK 后非阻断两项：Q11 引用粘连修正；"EStop 路由"收窄为 **topic-based
  软件 EStop 路由**，§6.6 物理 GPIO 与内部 SafetyStopFailed 路径明列本次
  SLA 起点裁决范围外。

## 0. 现状剖面（映射的"源"语义）

### 0.1 SetEnabled demo 路径预算栈（8 行）与单一倒挂边界

| 层 | 预算 | 出处 | 性质 |
|---|---|---|---|
| client 本地硬超时 | 4 s / RPC | supervised_demo `CLIENT_HARD_TIMEOUT`（tokio timeout 包裹） | 真墙钟 |
| grpc-timeout metadata | 3 s | tonic `set_timeout`（仅写 metadata，非客户端计时器，PX-47） | 服务端建议值 |
| `CommandMeta.deadline` | 2 s（demo 惯用） | client 填写 → gateway 校验 | 域级 deadline |
| gateway → S0 RR 转发 | （透传，非独立预算层） | `rr_io_set_enabled` 线程（精确推导 = Q1） | 透传 |
| ipc effectful pending | **1 s** | `ipc_tick.rs:86` `PENDING_TOTAL_TIMEOUT`（A2 头部检查） | 真墙钟 |
| serial normal action | **无预算**（同步 inline 执行到完） | `serial_io_thread.rs` 步 3 `execute_serial_normal` | — |
| adapter enable 全序列 | 名义 23.91 s（22 次高层 `read_exact` + 1.91 s sleep；`write_all` 未纳入）；**无可证明事务级上界** | CHANGELOG 2026-09-01 核算表 | 名义值 |
| 单次底层等待 `T_wait` | 1000 ms | feetech `registers.rs:43`（serialport per-operation，读写共用） | 底层等待 |

**结构性事实（round 1 修正）**：client 4 s > metadata 3 s > meta 2 s >
ipc 1 s 是**正确的外大内小嵌套**，不是倒挂。**唯一的倒挂边界**在
`ipc pending 1 s → serial action 无预算 / enable 名义 23.91 s 且无
可证明上界`——外层监督者比它所监督的动作小一个数量级以上，且被
监督者无界。demo 4L-① 的 `AdapterError` **与该倒挂路径一致，但未
唯一归因**（summary §3 候选路径口径：`AdapterResp::Err` /
`SafetyLatched` / `Unsupported` 同落 `Fault(AdapterErr)`）。

### 0.2 serial tick 闭环与同步 action 执行

`run_serial_io_loop`（`serial_io_thread.rs:351`）每 tick 顺序：

```
emergency drain（swap 原子）→ confirm_torque_off（限 StopCommanded）
  → consume normal action（单槽 swap，execute_serial_normal 同步 inline）
  → policy gate + write_command → read_state（事务戳提交）→ publish snapshot
```

- **normal action 是同步执行到完**：enable 在步 3 内联跑完整个 adapter
  序列（名义 23.91 s）。执行期间**整个循环停转**——emergency drain、
  confirm、read_state、snapshot 全部停摆（→ 系统性发现 S-1）。
- 单槽 `pending_normal` / 单槽 `last_completed`（`Busy` 语义 =
  槽被占）——分步 ack 类方案的结构约束（→ S-4）。
- `read_state` 失败保留 last-known-good 状态**与旧戳**（段(7) 事务式
  提交；消费方 stale 检测是当前唯一防线）。

### 0.3 Ack 协议与"超时后世界线"（F3 现状事实链）

`ipc_tick.rs` Step 2（A2/A3 决策注释在案）：

1. pending 超 1 s → `commit_fault(AdapterErr)` + `policy = Disabled` +
   `retry.clear()`。**明文不兜底 `safety_stop()`**（A3：AdapterErr 非
   emergency 路径；safety_stop 会把 policy 覆盖成 StopTorque）。
2. serial 线程内的 enable **继续执行**（无中断入口）；完成后
   `safety_state` 照常 store `TorqueOn`（`execute_serial_normal`），
   resp 落 `last_completed`——但 pending 已清，seq 不匹配 → **late ack
   被静默丢弃**。
3. 终态：`state = Fault(AdapterErr)` + `policy = Disabled`（不写 goal）+
   硬件**可能带扭** + `safety_state = TorqueOn`（**wire 可观测**，
   S0SafetyState 已在 state topic / GetState 上）。
4. **S0 侧无自收敛断扭路径**：
   - `Disable`：Safe/Fault 下是 Pure no-op，**不碰 adapter**
     （`state_machine.rs:392` + 候选 C RS1）；
   - `ClearError`：`Fault(_) → Ready`（**非 Safe**），Pure 不碰 adapter，
     守卫 `!estop_latched && adapter_fault_cleared`
     （`state_machine.rs:648`）；
   - 唯一物理断扭入口 = emergency `safety_stop`（EStop /
     SafetyStopFailed 路径，broadcast Torque_Enable=0 + confirm 三态）。
5. **`ClearError` 守卫在实践中形同虚设——自我授权**（round 1 证据链
   修正；原"EStop 绕道锁定"结论**错误**，`clear_estop_action.rs:222`
   是测试 fixture 非生产置位点）：
   - 生产初值就是 `adapter_fault_cleared: true`（`s0_daemon.rs:628`）；
   - `Event::ClearError` 自己的 guard 副作用在 plan 之前把该 guard 置
     `true`（`ipc_tick.rs:388` `apply_event_guard_side_effect`）；
   - `commit_fault` 只改 state 不动 guards。
   三者合成：Fault(AdapterErr) 下发 ClearError **必然通过守卫**（唯一
   实际拦截者是 `estop_latched`）。**真实缺陷 = 恢复门没有任何 adapter
   健康校验**（Q5）。demo 4L-① 后走 EStop→ClearEStop 是因为 client
   finalizer 已置 latch，与本守卫无关。
6. client 侧现行契约（demo bin）："ack 失败 ≠ 未上扭" → cancel →
   EStop（fail-closed finalizer）。**client 死亡则无人收敛**（D15 OPEN
   下硬件无 fail-safe，扭矩驻留在最后 Goal）。

### 0.4 错误处理现状（P0-2 四件套逐项核缺）

| P0-2 要求 | 现状 | 证据 |
|---|---|---|
| 连续读写错误阈值 | **无**。`read_errors` / `write_errors` 仅 metrics 计数，无阈值、无升级、不进状态机 | `serial_io_thread.rs` 步 4（write err `metrics.write_errors`）/ read_state Err 分支 |
| Fault / 断扭语义 | **无**（收窄表述）。周期性 `write_command` / `read_state` 连续通信失败目前只累计 metrics，**不进入 Fault / 断扭升级链**。对比：normal action 的 `AdapterResp::Err` 已提交 `Fault(AdapterErr)`（`ipc_tick.rs:199`），safety_stop / confirm 亦有 StopFailed / AdapterUncertain / SafetyStopFailed 失败态——缺口仅在周期性通信路径 | 同上；state_machine 无周期通信失败事件源 |
| RX 重同步 | **无**。协议层 `read_exact` 定长读，错位后无帧头扫描 / flush 机制 | `bus.rs` `parse_status` 定长路径 |
| 有界串口 reopen | **无**。`NotConnected` 后无重开尝试；唯一 reopen = 重启 daemon（guard 连败预算 0..=5 兜底） | transport `real` 模块无 reopen 入口 |
| 快速错误也须触发 | **不满足**。拔线类 NotConnected 立即返回 → **200 Hz** 空转计数（SerialRT 生产 tick = 5 ms，`serial_io_thread.rs:133` 默认；CLI `--tick-hz 50` 只进 HardRT `RtThreadConfig`（`s0_daemon.rs:850`），IPC 自身固定 `poll_interval_ns: 500_000`（`s0_daemon.rs:638`），两者均**不进** SerialRT cfg（`s0_daemon.rs:881` 用 `..default()`））、stamp 冻结、systemd liveness 恒绿（线程活着）| 评估 §4.2 判定与代码一致 |

**同一 serial 控制路径上最接近的先例**：`confirm_torque_off` 的三态
升级机构（`StopCommanded` → 连续 `confirm_max_attempts` 次仍 on →
`StopFailed`；`DeadlineExceeded` → `AdapterUncertain`）
（`serial_io_thread.rs:445-500`）。

**错误分类现状**：`FeetechError → RobotError` 映射里
`TransportTimeout → DeadlineExceeded` 已被 confirm 路径特判（部分回包
≠ hard fail）；其余（NotConnected / StatusError / Transport*）落
HardwareFault 类。阈值设计需要完整映射表精查（Q6）。

### 0.5 F2 可观测性现状

零日志 fault 点（定位 4L-① 时只能代码排除法）：

1. `ipc_tick.rs` Step 2 头部 1 s 超时分支（`commit_fault` 前后无
   tracing）——4L-① 候选路径本体；
2. 同 Step 2 `Unsupported` 分支；
3. 同 Step 2 `AdapterResp::Err(_)` 分支（错误值 `_e` 被丢弃）；
4. serial 侧 read/write 错误（仅计数无内容；错误值被 `is_err()` /
   `Err(_)` 丢弃）。

## 1. 勘探项 ①：F1 预算结构（三路比较，不拍板）

**共同前提**（无论选哪路都必须先解决）：enable 全序列**当前无可证明
上界**（PX-58）。可界化本身有两个正交子选项：
- (i) transport 层加**事务级 deadline**（`read_exact`/`write_all` 收
  一个总预算参数，底层循环内检查）——改 trait 契约，影响 mock/real 两实现；
- (ii) **调用计数上界**推导，须三步齐备：精确枚举完整 enable 事务内
  **全部**高层 `read_exact` **与 `write_all`**、重试及 sleep（PX-58 的
  22 仅是 `read_exact` 数，`write_all` 未纳入 23.91 s 名义核算）；每个
  高层调用先取得**统一硬 deadline**（否则字节滴流可把单次高层调用拖过
  T_wait，"每高层调用至多一次底层等待打满"当前不成立）；再据完整调用数
  推导 action 级硬上界。现有"22 × T_wait + sleep"**只能保留为名义倒挂
  见证，不能成为任何上界方案**；per-call 硬 deadline 即 (i) 的事务级
  deadline，故 (ii) 不是 (i) 的替代，而是 (i) 落地后的上界推导步骤。

| | 方案 A：放大外层预算 | 方案 B：分步 ack | 方案 C：子步化 + 步间可抢占 enable |
|---|---|---|---|
| 形态 | `PENDING_TOTAL_TIMEOUT` 改为"推导上界 + 余量"（前提 (i) 先落） | enable 拆阶段（sync_goal / settle / per-motor），serial 侧逐步汇报进度，ipc 每步小预算 | serial normal action 子步化 / tick 化执行，emergency 与超时可在步间抢占 |
| 改动面 | 最小（一常量 + 推导注释 + transport deadline） | 中（ack 协议 + 单槽结构 S-4 扩展 + ipc pending 状态机） | 最大（adapter enable 重构为可恢复状态机 + serial 循环重排） |
| 解 S-1（emergency 阻塞）？ | **否**——enable 仍同步占线程 | 部分（步间可插 emergency drain；在途底层 IO 仍阻塞） | **部分**——仅消除子步间阻塞；在途单次底层 IO（`T_wait` = 配置预算 1 s/次、非硬上界；无事务 deadline）仍不可中断，彻底解需 transport 层可中断/更小底层预算配合 |
| 客户端栈连带 | 外层 >4 s 后 client 4s/3s/2s 全要重排（倒挂上移到 client） | serial↔ipc 内部进度回报不改外部 wire；仅当进度暴露到 RPC/topic 才需 additive wire 评审（同 S-4） | 外部 wire 同样不必变；需 adapter 行为语义评审（可抢占中间态） |
| F3 契约配合 | 仍需独立拍（超时仍可能发生） | 天然给出"进行到哪一步"证据，F3 三元判定材料变强 | 超时后**后续子步可停止**，"副作用继续"一元被**收窄**而非消失：在途底层 IO 仍会跑完、已完成的 enable 子步（含已上扭电机）不回滚 |
| 风险 | 长预算掩盖真故障（24 s 内无诊断） | 协议复杂度 + 兼容 | 防飞序列（goal=present）被打断的中间态安全性要重证 |

**交叉关系**：B（阶段进度回报）与 C（action 子步化 + 步间可抢占）共享
"enable 拆子步"基础、可组合，但**互不包含**——B 不要求子步可抢占，
C 不要求进度回报；A 与 B/C 不互斥（A 可作过渡）。
**不预钉**（Reverse Sync 拍板原文）；Spec 需按上表 + Q2/Q3 拍。

## 2. 勘探项 ②：F3 三元契约（选项枚举）

三元 = ① Ack 超时语义 / ② 副作用是否继续 / ③ 最终扭矩态谁收敛。

- **选项 α（现状显式化）**：超时 = "结果未知"；副作用继续；收敛责任
  = client（EStop finalizer 契约已在 demo bin 落地）。S0 侧只补观测：
  `Fault(AdapterErr)` + `safety_state=TorqueOn` 组合已足以在 wire 判读
  "faulted 但带扭"——文档化 + gateway/robotctl 呈现即可，S0 零行为改动。
  残余：client 死亡无人收敛（依赖 D15 闭合）。
- **选项 β（S0 侧收敛）**：超时 Fault 时附带 `safety_stop()`（推翻 A3
  "不兜底"决策——其注释理由是 policy 覆盖语义，需重议而非绕过）。
  收敛变 S0 本地、有 confirm 三态兜底；代价 = "读慢一次就断扭"的
  误伤面（与 P0-2 阈值联动可缓解：只有升级链定级到位后才断扭）。
  **S-1 约束（round 1）**：safety_stop 的物理执行同样要等 serial loop
  下次 drain——若超时正是被在途同步 IO 拖出来的，β 的断扭同被同一
  IO 阻塞，**单独不构成即时自收敛**，须与 S-1 的解联动评估。
- **选项 γ（侦察后收敛）**：超时不立即 Fault，转"结果侦察"态——等
  late resp 落地（或加一次 readback）后按真实终态 commit；给 late ack
  赋语义（现状被静默丢弃）。代价 = 状态机新态 + 侦察自身也要预算。
- 与勘探项 ① 耦合：B/C 路线下三元里的"副作用继续"可被细粒度化/收窄
  （不能消除：在途 IO 与已完成子步的副作用仍在），
  α/β/γ 的比较基面随之变化——Spec 拍板顺序建议先 ①后 ②（Q3）。

## 3. 勘探项 ③：P0-2 升级链（四件套设计输入）

- **计数器落点**：serial 线程是唯一串口 IO 执行点（read_state /
  write_command / normal action 三路同一 adapter）——单一计数器
  自然落 serial 循环；按"错误类别 × 连续窗口"计（快速错误天然被
  tick 节拍计数，满足"快速返回也触发"）。
- **阈值时基（round 1 修正——"连续 N 次 = N×tick 墙钟"不成立**：
  同步串口调用会拉长 tick，200 Hz 只是空载节拍）：两选项另拍（Q14）——
  (a) 单调时钟 elapsed 阈值；(b) 连续样本数 + **最大采样间隔上限** +
  重置语义（间隔超限时计数如何处理）。`confirm_torque_off` 先例用
  "连续 + saturating"，同受 tick 拉长影响。
- **升级终态**：定级 → `Fault(AdapterErr?/新码)` + 是否附带
  safety_stop = 与 F3 选项 β 同一题（**必须一次拍清**，Q4）；
  新 FaultCode（如 `CommFault`）为 additive 枚举扩展。
- **RX 重同步**：错位后策略 = flush RX + 帧头（FF FF）扫描重对齐 vs
  整帧丢弃重发；feetech 定长协议下最小实现是"错误后 flush + 下轮
  重发"；bus 层现无 flush 入口（transport trait 需 additive 方法，Q7）。
- **有界 reopen**：by-id 路径重开（open 语义已有）+ 次数上界 + 每次
  间隔 + 终态（reopen 耗尽 → 升级终态）；与 guard 连败预算（daemon
  级 0..=5）的分层关系要写清：串口 reopen 是**进程内**第一层，
  daemon 重启是第二层，两层预算不可互相吞并（Q8）。
- **触发面完备性**：normal action 失败（enable 内部 Err）是否计入
  同一升级链（现状 `AdapterResp::Err` 只走 ipc Fault，不进计数）——
  归一 or 分立（Q9）。

## 4. 勘探项 ④：F2 结构化 ERROR 落点

| # | 落点 | 建议字段（Spec 定稿） |
|---|---|---|
| 1 | ipc Step 2 超时分支 | elapsed_ns / seq（有无）/ 请求类型（Enable(bool) 等）/ 当前 state |
| 2 | ipc `Unsupported` 分支 | 同上 + 拒因文本 |
| 3 | ipc `AdapterResp::Err` 分支 | 同上 + `AdapterErr` 变体（现被 `_e` 丢弃） |
| 4 | serial read/write 错误 | 错误变体 + 连续计数值（与勘探项 ③ 计数器共源）；限速（200 Hz 生产 tick 下防日志风暴，如每 N 次/每状态变化打一条） |

F2 与 F1/③ 同候选落地（拍板原文）；日志限速策略本身是 Spec 项（Q10）。

## 5. 跨面系统性发现（登记；本候选范围裁决 = Q11）

- **S-1（安全级，round 1 扩程 + round 2 补排队/唤醒段 + round 3 改正
  client 语义）**：`safety_stop()`（`HardRtCtrlClient`；SerialRT 路径的
  `SerialIoCtrlClient` 是同一类型别名）在 client 端**同步**完成五步
  atomic 写（`hard_rt_ctrl.rs:142`）：policy=StopTorque →
  safety_latched=true → safety_state=StopCommanded → drain 尚未被消费的
  pending normal（回填 `PreemptedByEmergency`）→ emergency_action=
  SAFETY_STOP。因此必须分两层：
  - **软件门闩立即生效**（调用返回即成立）：后续 tick 的 policy gate
    不再 write_command；新 `Enable(true)` 被 client 端 latch 拒；尚未被
    serial 线程取走的 normal action 被清。
  - **物理断扭仍延迟**：已被 serial 线程 swap 走或正在执行的 IO 无法
    中断；真正的 `adapter.safety_stop()` 要等 serial loop 下一次 drain
    （`serial_io_thread.rs:384-389`）。
  请求 → 断扭全链六段（round 4 拆出 IPC 排队段；round 5 补路由 + 阻塞
  send）：① **IPC 排队**——**topic-based 软件 EStop 路由** = gateway/S1 →
  iceoryx2 `/events/estop` topic → S0 `poll_estop` → EventQueue
  （`docs/00_design.md` §6.6；S0 RR 只承载 ClearEStop/SetEnabled，**不承载
  EStop**）。范围外（本次 SLA 起点裁决不覆盖，避免七个起点被误读为全部 EStop 来源）：§6.6 物理 GPIO 按钮路径（仓内无 S0 GPIO 中断实现（仅 feetech adapter 注释提及））与 S0 内部 `AdapterErr(SafetyStopFailed)` emergency 路径（起点在 S0 内，`ipc_tick.rs:125`；共享 ②–⑥ 后段）。从
  subscriber 到 `safety_stop()` 不是零成本：`poll_estop` 以 `latest()` 取得
  样本（`ipc_thread.rs:694`）入 EventQueue（:695）后，本轮还要依次 drain
  watchdog :697 / 外部 :701 / ClearEStop :725–763 / SetEnabled :781–862 四个
  `while let Ok(..) = try_recv()` 循环（**无每 tick 计数上限**；watchdog
  channel `unbounded`（`s0_daemon.rs:555`）、外部事件 channel `unbounded`
  （`ipc_thread.rs:559`）、两路 RR `bounded(64)`（`s0_daemon.rs:673/678`）
  且生产者 `try_send` 不阻塞（`rr_server_thread.rs:447` /
  `set_enabled_rr_server.rs:323`）——RR 侧任一时刻积压 ≤64 但 drain 中可被
  并发补入，饥饿上界须核定），到 :920 才进 `ipc_tick`；若 EStop 错过
  本轮 poll，再加 500 µs `thread::sleep`（:1121）+ 普通优先级线程调度
  延迟。**更硬的一点**：两个 RR drain 体内对 bounded(64) response channel
  调的是**阻塞 `send`**（ClearEStop :727/:750/:755，SetEnabled
  :783/:801/:806/:813/:819/:839；resolve 路径 :900/:1041/:1109 同样阻塞）。
  当前不阻塞靠**隐含不变量**：rr_server `max_in_flight`=64
  （`s0_daemon.rs:734/777` 字面量；`pending.len() >= max_in_flight` 即拒新
  wire 请求，`rr_server_thread.rs:297` / `set_enabled_rr_server.rs:197`）
  ≤ response 容量 64（`s0_daemon.rs:674/679` 字面量），每个 request 至多
  生成一个 response（drain 内分支互斥；in-flight 由 `take()` 后单次回复，
  :900 / :1041 / :1109），两路 RR channel 独立。三个 64 之间**无共享常量、
  无 const assert、无测试锁定**——配置漂移或不变量破坏后，IPC 会在执行
  EStop 的 `ipc_tick` 之前卡在 response send（处置 = Q15）；
  ② **软件门闩**——
  `ipc_tick` Step 1 调 `executor_ctrl.safety_stop()`（`ipc_tick.rs:135`）
  → 返回即成立；③ **SerialRT 排队/唤醒**——即便 loop 空载，drain 也位于
  每 tick 开头的 `sleep_until` 之后（`serial_io_thread.rs:378-380`），故
  要等本 tick 剩余等待（生产周期 5 ms）+ 线程调度唤醒延迟，对 <10 ms
  判定属同量级、不可省；④ **在途同步 IO**——loop 内**任何同步操作**都
  会推迟下一次 drain：`confirm_torque_off` probe、normal action（enable
  最长，名义 ~24 s 无上界）、`write_command`、`read_state`，每一项都是
  不可中断的底层串口 IO（单次底层等待配置预算 `T_wait`=1 s、非硬上界，
  无事务级 deadline）；⑤ `adapter.safety_stop()` 自身的 `write_all`；
  ⑥ **终点三选一**——(a) host `write_all` 返回：生产 `RealSerialTransport`
  仅调 `std::io::Write::write_all`，无 UART drain / 物理应用确认
  （`transport.rs:227`），只证明调用完成，**不能称"bus write 完成"**；
  (b) UART on-wire 完成：当前无独立观测机制；(c) 电机 `Torque_Enable`
  readback 全关：现有唯一强证据终点（= confirm 三态）。因此：
  勘探项 ① 方案 C 也**只消除子步之间的阻塞，不能中断在途底层 IO**；
  F3 选项 β 的 safety_stop 同受本约束，**单独不构成即时自收敛**。§4.6
  "`safety_stop_fast` < 10 ms" SLA 的约束对象（adapter 调用本身 vs
  请求→断扭全链）需先澄清（Q11），**在澄清前不宣称违反或满足**。
  demo 期间每次上扭窗口该暴露真实存在。
- **S-2（round 1 证据链更正）**：ClearError 恢复门**自我授权**——
  guard 生产初值 `true`（`s0_daemon.rs:628`）+ 事件自置 `true`
  （`ipc_tick.rs:388`）+ `commit_fault` 不动 guards → 守卫从不拦截
  ClearError；**恢复门没有任何 adapter 健康校验**（通信仍断时也能
  Fault→Ready）。是否加校验、校验形态 = Q5。
- **S-3**：`ClearError` 直达 **Ready** 且 `ReadyHold` 在 serial
  `do_write` 名单——Fault 期硬件带扭 + 旧 goal 时，恢复即写命令的
  接管语义未定义（写的是 S1 最新 command slot；若 S1 未跑 = 旧值）。
  拍板 = Q13 三元组（S0State + OutputPolicy + 扭矩证据），状态名不等于
  物理安全。
- **S-4**：单槽 `pending_normal`/`last_completed` 是分步 ack（①-B）
  的**进程内 packed atomic 布局**约束；本身不涉外部 wire——仅当 B 要
  把阶段进度暴露到 RPC/topic 层时才引入 wire 变化。
- **S-5**：late ack 被静默丢弃（seq 不匹配）；其值短暂留在
  `last_completed` 槽但**会被下一次 completion 覆盖**——非可靠持久
  侦察材料。γ 选项若采纳需显式持久化 late 结果，不能依赖该槽。

## 6. Spec 拍板 Q 表（Pre-Research 出口 → Spec round 1 入口）

| Q | 问题 | 关联 |
|---|---|---|
| Q1 | gateway RR 转发对 SetEnabled 的精确 deadline 推导（meta 透传处的截断/放大） | §0.1 |
| Q2 | 可界化 (i)+(ii) 三件必拍：(a) transport 事务级 deadline 的 trait 契约形态（参数 vs builder vs per-call）；(b) 完整 enable 事务的调用枚举（全部高层 `read_exact` / `write_all` / 重试 / sleep，按电机数 N 参数化）；(c) 据 (a)(b) 推导并登记 action 级硬上界**数值**——Spec 只拍 API 不拍预算结果 = 未闭合 | ①前提 |
| Q3 | F1 三方案（A/B/C）拍板 + 与 F3（α/β/γ）的拍板顺序 | ①② |
| Q4 | 升级链终态是否附带 safety_stop（= F3-β 同题，一次拍清） | ②③ |
| Q5 | ClearError 恢复门是否加 adapter 健康校验（现状自我授权：guard 生产初值 true + 事件自置 true，守卫从不拦截）；校验形态（probe / 通信健康计数 / 显式确认） | S-2 |
| Q6 | `FeetechError → RobotError` 完整映射表 + 哪些类别计入升级链计数 | ③ |
| Q7 | RX 重同步的 transport additive 方法形态（flush vs 帧扫描） | ③ |
| Q8 | 串口 reopen 预算与 guard 连败预算的分层数值 | ③ |
| Q9 | normal action 失败是否并入升级链计数 | ③ |
| Q10 | F2 日志限速策略 | ④ |
| Q11 | S-1 范围裁决与 SLA 定义六件：① SLA 起点——**topic-based 软件 EStop 路由** = gateway/S1 → iceoryx2 `/events/estop` topic → S0 `poll_estop` → EventQueue（`docs/00_design.md` §6.6；S0 RR 不承载 EStop）；范围外（本次 SLA 起点裁决不覆盖，避免七个起点被误读为全部 EStop 来源）：§6.6 物理 GPIO 按钮路径（仓内无 S0 GPIO 中断实现（仅 feetech adapter 注释提及））与 S0 内部 `AdapterErr(SafetyStopFailed)` emergency 路径（起点在 S0 内，`ipc_tick.rs:125`；共享 ②–⑥ 后段）；可观测候选点七个：producer `publish()` 返回 / S0 subscriber `latest()` 取得样本（`ipc_thread.rs:694`）/ EventQueue 入队（:695）/ `safety_stop()` 调用入口 / emergency_action 发布（`hard_rt_ctrl.rs:142` 末步 store）/ `safety_stop()` 返回（软件门闩成立点）/ serial 端 `adapter.safety_stop()` 调用开始（`serial_io_thread.rs:389`）。裁决范围：若只裁 S0 内 SLA，从 subscriber `latest()` 或 EventQueue 入队起算；若从 gateway RPC 起算，必须把 gateway `estop_pub_thread` 链路（gRPC handler → bounded(64) forward channel `try_send`（`robot-rpc/src/gateway.rs:2032` / `robot-rpc/src/gateway.rs:1248` handler `try_send`）→ `gw_estop_pub` 线程 → `publish()`）纳入预算 ② IPC 排队段——source → EventQueue → `safety_stop()`/emergency_action 之间：四个 `while let Ok` drain（`ipc_thread.rs`：watchdog :697 / 外部 :701 / ClearEStop :725–763 / SetEnabled :781–862）是否有饥饿上界（watchdog/外部事件 `unbounded`，两路 RR `bounded(64)` + 生产者 `try_send`；无每 tick 计数上限，须核定或加上限）+ drain 体内阻塞 response `send`（Q15）+ 错过本轮 poll 的 500 µs sleep（:1121）+ 普通线程调度延迟，是否计入 SLA ③ SerialRT 排队/唤醒段（本 tick 剩余 `sleep_until` 等待，生产周期 5 ms，+ 线程调度唤醒延迟；是否计入 SLA）④ 在途底层 IO 如何中断或界内化 ⑤ safety_stop write 预算 ⑥ 终点三选一——host `write_all` 返回（仅证明调用完成，`transport.rs:227` 无 UART drain）/ UART on-wire 完成（无观测机制）/ `Torque_Enable` readback 全关（强证据）+ confirm read 预算；并澄清 §4.6 "<10 ms" 原文约束对象——澄清前不判合规。是否纳入本候选 P0 范围（建议：是） | ⑤ |
| Q12 | 新 FaultCode（如 CommFault）additive 扩展 vs 复用 AdapterErr | ③ |
| Q13 | ClearError 后目标态须一次拍清**三元组**：目标 `S0State`（Safe / Ready；仓内无 `S0State::Disabled`，`state_machine.rs:101`）+ `OutputPolicy`（Disabled / ReadyHold / …）+ adapter 动作与最终扭矩证据（是否执行 `Enable(false)`，或走 safety_stop + confirm 三态）。"Safe + policy Disabled"只停止写命令，**不保证已带扭硬件断扭**，不能当物理安全结论。若保持 Ready，则 ReadyHold 语义二选一（写"当前位"快照 vs 写最新 command slot）——现状 = Ready + ReadyHold 写最新 slot（S1 未跑则旧值），硬件扭矩沿用 Fault 期状态 | S-3 |
| Q14 | 升级链阈值时基：单调时钟 elapsed vs 连续样本数 + 最大采样间隔上限 + 重置语义（"N×tick=墙钟"因同步 IO 拉长 tick 不成立） | ③ |
| Q15 | RR response 派发阻塞 `send` 的处置二选一：(a) 证明并**锁住**不变量——response 容量 ≥ `max_in_flight`、每 request ≤1 response、双 RR channel 独立（共享常量 + const assert + 单测三件）；(b) response 派发改**有界非阻塞**（`try_send` + Full 计数 / 超时）。CodeMap 判定：现状不变量成立（证据 S-1 ①）但**未锁**；建议 (a) 为本候选 P0 最小件，(b) 在 Q11 起点拍在 IPC 之前时升级为必要件 | S-1 / Q11 |

## 7. 引用

- `crates/robot-s0/src/ipc_tick.rs`（:86 预算 / Step 1-3 机构 / :135 Step 1 `safety_stop()` / :199 normal action Err → Fault(AdapterErr) / A2·A3 注释 / :388 ClearError 事件自置 guard）
- `crates/robot-s0/src/hard_rt_ctrl.rs`（:142 client 端 `safety_stop()` 五步同步 atomic 写；`SerialIoCtrlClient` = 同类型别名，`serial_io_ctrl.rs:57`）
- `crates/robot-s0/src/serial_io_thread.rs`（:133 默认 5 ms / :351 主循环 / :378-380 tick `sleep_until` / :384-389 emergency 消费 → `adapter.safety_stop()` / :584 execute_serial_normal / confirm 三态）
- `crates/robot-s0/src/state_machine.rs`（:101 `S0State` 五主态、无 Disabled / :341 classify / :648 ClearError 守卫 / :181 缺省 guard）
- `crates/robot-s0/src/clear_estop_action.rs`（:222 `adapter_fault_cleared: true` 为**测试 fixture**，round 1 误判来源，非生产置位点）
- `crates/robot-s0/src/bin/s0_daemon.rs`（:628 guard 生产初值 / :638 ipc `poll_interval_ns` 固定 500 µs / :850 `--tick-hz` 仅进 RtThreadConfig / :881 SerialRT cfg `..default()`）
- `crates/robot-s0/src/ipc_thread.rs`（:694 EStop `latest()` / :695 入队 / 四个 while-let drain：watchdog :697 / 外部 :701 / ClearEStop :725–763 / SetEnabled :781–862，无每 tick 上限 / 阻塞 response send :727,:750,:755,:783,:801,:806,:813,:819,:839,:900,:1041,:1109 / :920 ipc_tick / :1121 500 µs sleep）
- `crates/robot-s0/src/rr_server_thread.rs`（:112 `max_in_flight` 默认 64 / :297 overflow 拒 / :447 `try_send`）+ `set_enabled_rr_server.rs`（:197 overflow 拒 / :323 `try_send`）+ `s0_daemon.rs`（:555 watchdog unbounded / :673-679 RR bounded(64) ×4 / :734,:777 `max_in_flight: 64` 字面量）
- `crates/robot-rpc/src/estop_pub_thread.rs`（gateway EStop 路由 D1 (a)：handler → bounded(64) forward `try_send` → `gw_estop_pub` → `publish()`；`robot-rpc/src/gateway.rs:2032` / `robot-rpc/src/gateway.rs:1248` handler `try_send`）+ `docs/00_design.md` §6.6 EStop 来源
- `crates/robot-adapters/feetech/src/{adapter.rs, bus.rs, transport.rs, registers.rs}`（enable 序列 / 定长协议 / 契约注释〔round 3 勘正后〕/ T_wait / `transport.rs:227` 生产 `write_all` 无 UART drain）
- `docs/CHANGELOG.md` 2026-09-01 两条；Spec P.4 PX-56..60；demo summary §3/§7；评估文档 §3.2/§4.2/§6.2
