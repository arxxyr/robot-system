# CHANGELOG

> 本文件记录 `docs/00_design.md` 冻结后的语义讨论与决定，以及 target 文档硬契约的增删改。
> 三类条目：
> 1. **偏离登记**：允许临时偏离设计文档（冻结原则下）；
> 2. **回写登记**：用户拍板后，通过 v0.1.2 "冻结后文档卫生" 机制（附录 E）修改设计文档正文，同步回写 dimos 侧源版本 `docs/09_robot_framework.md`；
> 3. **Target 契约登记**：`targets/<name>.md` §2《训练 / 部署契约》节的新增 / 修改 / 删除；**仅该节**——同文件的 §1 硬件清单 / §3 上游实现参考 / §4 映射 / §5 adapter 需求 / §6 风险 / §7 参考，以及 `_migration.md` 路线图 / `_<subsystem>_<commit7>.md` 源码快照，全部**不走本文件**，PR 直接改。登记规则详见 [`targets/README.md`](targets/README.md)。
>
> 每条记录：日期 + 涉及小节 + 状态（偏离 / 已回写 / target-contract）+ 理由 + 约束 + 影响面 + 审阅人。

---

## 2026-09-08 · C3 运行时收敛验收细化（状态：分包实现与独立验收通过）

独立验收直接驱动生产 SerialTick 和 SerialConvergence，脚本仅提供设备回答，覆盖
正常动作在途 / 已完成 / 尚未消费的超时退场、ACK 世代、急停合并及状态冻结。
评审核出 tick 尾部无条件推进 liveness 会掩盖重复超期 IO：现按已批准 Plan 的
phase deadline 限制 tick 进度，本 tick 任一调用越界不得因后续换 phase 重新计数；
无 IO 的合法等待不继承旧 lease。Spec 同步这一精确口径，不增加整轮预算。
启动 deadline 的 EStop-bound 分支同样须先写不可信完成记录再进入 SSF。

失败证据保留：实际调度曾在普通读故障后同 tick 多发 Safety，破坏已完成 Disable
的迟到超时豁免；现仅登记义务，后续 tick 执行收敛。取消 tag 与匹配 Submitted
同时到达时结果必须优先 AbortedByTimeout，不能把正常超时覆盖成 Preempted。
另两项夹具误预排五次全断扭确认，已按 D4 首次 AllOffUntrusted 即退场修正。
独立评审已 C3 Approved；S0 337（含独立34）、workspace 1428 项通过，
最终 fmt / 默认与全 feature clippy / build 完整零警告。证据见 C3.json，
本包不宣称整个候选完成；daemon 接线仍在 C4。

## 2026-09-08 · C3 超时与急停交错保留请求身份（状态：Reverse Sync；方案已独立 ACK）

S-40 核码：C1 的 EStop 原语同步清空 pending，可能丢弃一个已被 Submitted timeout
引用、尚未执行的请求，随后被错误判为 Unknown 并清除 RX 信任。SerialRT 改为在
原 pending 词 bit63 标取消，保留 40 位 seq 与 action / arg，serial 唯一消费者
零执行取消并发布完整完成记录；匹配 timeout 按 PendingDeadline 收敛并保留 ACK
和独立 EStop 义务。不新增跨槽事实搬运，HardRT 48 位协议不变。

tag 本身不创建 Emergency / 写 policy / 发 gen，权威请求仍由 safety_word 一次
成功 CAS 发布；发布者暂停于两者之间时亦成立。非法保留位不得被 mask 吞掉。
独立 reviewer 已批准该修正，Spec D3 / D13 / AC-4 / AC-9 已回写；C3 实现及
确定性交错验收现已执行通过，随 C3 分包获批。冻结设计未改。

## 2026-09-08 · post-E 通信故障硬化：Plan 核码澄清（状态：Reverse Sync；实现前登记）

用户授权对齐文档并由固定开发、独立评审持续推进。2026-09-05 的 Spec round 21
批准继续有效；正式计划见
[`post_e_comm_fault_hardening.PLAN.md`](../team-swe/specs/post_e_comm_fault_hardening.PLAN.md)，
内部审批与实施进度记在同目录 HANDOFF。冻结设计 `00_design.md` 未修改。

- **S-39 / D3：IPC 超时未必已有 seq**。核 `ipc_tick.rs` 的总超时判断早于首次
  `request_normal`：IPC 停顿即可产生未提交请求超时，不要求先发生 Busy。以
  `NormalAbortTarget::{Submitted { seq }, Unsubmitted}` 区分，并在唯一 additive
  default 接口 `abort_pending_normal(target)` 发布带标签请求。全部正常超时仍预提交
  `CommFault + Disabled`，由 serial 物理收敛、发布 gen 并等 ACK；不构造假 seq。
  Unsubmitted 若另有非本请求的 pending / active 残留，按 `InvariantViolation`
  转 `Unknown`、清信任并收敛，阻止残留普通 IO。消费后 CAS 清确切请求。
- **D4 / D13：入口拒绝与在途抢占分开**。周期读写在取得事务执行权前败给 EStop，
  合法返回 `Aborted(Emergency)` 且零 IO，不计通信故障、不刷新快照；已经取得执行权
  的完整 request / response 由 RAII 释放。checkpoint 只记进度，不释放执行权。
- **AC-1：回归范围从 Cargo 实际枚举冻结**。保留全部原用例，历史 workflow 12/12
  包含公共 subprocess 测试；唯一 abort default 扩展不改变 HardRT 行为。验收包含
  integration、显式 ignored soak、隔离 smoke 和当前源码构建的子进程。

上述澄清由开发与独立评审核码、主 agent 回写；不扩大候选范围。D15 保持 OPEN，
RX 重同步、reopen 与跨臂联锁仍不纳入。新增验收项尚未执行，不能据此宣称实现完成。

**C1 实施细化与复核**：SerialRT 耗尽后不再递增序号，以 strong CAS 限定合法取号，
避免持续 `fetch_add` 最终 u64 回绕；HardRT 原逻辑不变。独立 reviewer 已批准 C1，
core 79 / S0 298（含独立 14）/ schema compat 16 项通过，后续 phase / IPC 验收尚未落地。
复核发现此前 clippy 返回 0 的日志仍含 Cargo manifest 提示，基线记录已更正：删除
未被继承的三条 workspace 依赖声明，保留现有 snake_case 二进制接口，并为这项
既定命名约定显式配置 Cargo lint；不降低 Rust / clippy 诊断门。全 feature 的
feetech logging 依赖提示按 C2 原计划处理。

**C2 分包验证**：整包读写共用调用级绝对截止，短读写保留累计进度，下一次调用
恢复完整预算；总线请求 / 响应由同一 RAII 执行权覆盖。adapter 构造期安装预算并
填充安全输出对象，首 Safety / Confirm 不依赖 connect 成功，普通 IO 仍要求已连接；
通信错误与可重试设备拒绝分开，hygiene 错误保留事实，运行诊断改固定事件。
`robot-logging` 仅供工具 example，移至 dev-dependencies 后全 feature 静态门零警告。
workspace lib / bins 1389、Feetech 独立 integration 12、schema compat 16 项通过，
真实硬件未执行。独立验收曾把 checkpoint 过约束为必须持有事务执行权，已修正：
discard / drain 返回也记录进度，真实 W/R 才要求执行权连续；失败日志保留供追溯。
独立评审已 **C2 Approved**；诊断缺 motor 的 P2 随包修复并获 ACK，单条事件保留
设备 ID 与状态字节 / 通信类别。默认 build 另发现 tracing 仅在 real-hw factory
使用，将该依赖改为由 real-hw 显式启用，保持默认和真硬件构建的诊断门。

## 2026-09-01 · 候选 E 段(10-demo) 真机执行补录 + F1–F3 / F7 / F8 Reverse Sync 拍板（**状态：执行记录 ×1 + 拍板 ×3；本轮不实施代码、不改 00_design.md**）

### 段(10-demo) 真机执行补录（2026-09-01，操作者全程在场逐步放行）

- **AC-E12 三步真机全过**：handoff 17 电机 ensure-safe → read-state →
  双侧 clear-estop（AC-E21 状态链实证）→ 单臂左/右（+0.0399 / +0.0430
  rad 运动证据）→ 双臂（ack_skew 10.75 µs）→ 双夹爪。收口宣称唯一口径
  = **"机器人控制执行面受监督功能 demo 跑通"**（D15 保持 OPEN）。
- **4L-① FAIL（fail-closed 正确，零运动）**：SetEnabled → AdapterError；
  **候选路径**（非唯一）= `ipc_tick.rs` `PENDING_TOTAL_TIMEOUT` 1 s 总
  超时分支；归因开放（相机 hub 竞争 vs 首次上扭瞬态，后者已有两反例）。
  停相机重测通过，此后全部上扭 phase 零超时。
- 发现登记 F1–F8：F1–F3/F7/F8 本条拍板（下述）；F4/F5 已随 runbook
  修正 closed；F6（wrist_roll ~0.006 rad/轮单向残余，容差内）观察项。
- 同日 RT 三层加固与 §4.6 偏离见上一条；资源基线（六进程 CPU 峰
  ≤13.4% / RSS ≤12 MiB）与全部证据 = Orin `~/deploy_p10/` +
  `docs/handoffs/v0.5_demo_run_2026-09-01_summary.md`（权威执行记录）。

### Reverse Sync ① F1–F3 合并 → post-E "通信故障硬化"候选输入

**F1 预算倒挂核算**（拍板：CodeMap 必须从本核算出发，**禁止直接拍一个
经验 timeout**）：

| 项 | 值 | 出处 |
|---|---|---|
| 单次底层等待预算 `T_wait` | 1000 ms | feetech `registers.rs:43` `DEFAULT_TIMEOUT_MS`（serialport open 直传；底层读**与写**的每次操作共用） |
| 外层 ack 预算 | 1 s | `ipc_tick.rs:86` `PENDING_TOTAL_TIMEOUT` |
| 高层 `read_exact` 调用数（N=6 臂） | 1 + 3 + 6×3 = **22** | sync_read Present ×1 + Goal readback ×`SYNC_GOAL_MAX_ATTEMPTS`(3) + Torque ACK ×N×`TORQUE_ENABLE_MAX_ATTEMPTS`(3)；`bus.write` 逐包等 6 字节 ACK |
| 固定 sleep | 3×20 + 50 + 6×2×150 = **1910 ms** | `SYNC_GOAL_READBACK_WAIT` / `ENABLE_SYNC_SETTLE` / `TORQUE_ENABLE_RETRY_DELAY` |
| **名义窗口合计（非上界）** | `T(N) = T_wait×(1+A_goal+N×A_tq) + A_goal×20ms + 50ms + N×(A_tq−1)×150ms` → N=6 时 **= 23.91 s（名义值）** | 22 次高层 `read_exact` **各按一次底层等待名义计** + 固定 sleep；重试打满 |
| 排队前置 | 两次高层 `read_exact`；**名义计 2 s，无已证明硬上界** | action 在 serial 线程排队于当前 tick 的 read_state 之后（Present_Position + Present_Velocity） |

**核算性质（P0 修正，codex Reverse Sync round 2/3）**：上表是**名义窗口
核算 / 倒挂见证**，**不是可证明的最坏时长**——`read_exact` 与
`write_all` 都可能包含**多次底层带超时操作**（`T_wait` 只约束每次底层
等待；字节滴流下单次高层调用可超过 1 s 而不触发超时），且**没有覆盖
整包或事务的统一 deadline**。`write_all` **未纳入 23.91 s 核算**（其
底层 write 同样按 per-operation timeout 等待可写，同样无事务级
deadline；round 3 勘正原"write_all 无超时"错误表述，transport.rs
契约注释同步修正）→ **当前不存在可证明的事务级硬上界**，这本身就是
CodeMap 必须解决的输入之一。

**结构性结论**（不受上述修正影响，反而更强）：倒挂不是"重试尾巴"
问题——**单次合法慢调用（或排队前置的两次高层 `read_exact`）即可击穿
1 s 外层预算**，此时序列仍在合法执行中；且由于无事务级上界，外层 1 s
预算在任何意义上都不成立。

**F3 三元契约（CodeMap 必答，不预钉答案）**：① Ack 超时的语义（超时 ≠
未执行）；② 副作用继续（enable 序列在 serial 线程不可中断地跑完）；
③ 最终扭矩态（硬件可能已上扭而状态机在 `Fault`——由谁、以何种动作
收敛：EStop / disable / readback 校验）。三者的契约必须一次拍清。

**F2**：`PENDING_TOTAL_TIMEOUT` 分支 `commit_fault` 零日志 → 补结构化
ERROR（elapsed / seq / 请求类型），**与 F1 同候选落地**。

**不预钉**："仅放大 1 s timeout"、"必须可中断"均不预设——放大预算 /
分步 ack / 可中断 enable 三路留 post-E 通信故障硬化 **CodeMap 比较**。

### Reverse Sync ② F7 baseline 判据

- **拍板：现有 `iceoryx2_rt_baseline` p99 ≤ 100 µs / p999 ≤ 500 µs
  阈值在发布频率 ≥ 50 Hz 下无效，即日起不再作为调度 gate**——变频判别
  （hz=10/50/100）已证尾部为速率相关错拍伪影（三种调度配置同形、max ≈
  恰一个发布周期、hz=10 时与样本缺口同时消失）。
- 原始数据全部保留（`~/deploy_p10/evidence_latency_*.log` ×3 + 判别
  实验；summary 附录 A/A.1）。
- **新判据须等 baseline harness 错拍机制定位后另拍**；本轮**不私设
  新阈值**。→ 独立"baseline 判据小件"。

### Reverse Sync ③ F8 iceoryx2 运行用户运维契约

- **拍定：同一部署内所有 iceoryx2 进程必须使用同一个非 root 运行用户**
  （本机 = `loosqk`）；**sudo/root 运行 baseline（及任何 iceoryx2 程序）
  明确禁止**。
- 依据（2026-09-01 实际事故）：root 跑 baseline 创建 root 属主
  `/tmp/iceoryx2/`（750）与 `/dev/shm/iox2_*_global_mgmt`（700）→ 此后
  非 root 结点创建裸报 `InternalError`（trace 级无细节）→ 新栈 s0 每侧
  连崩 5 次。
- 污染处置步骤（停栈 → 清理 → 重启）写入 runbook §10。
- **是否增加程序级 root fail-fast = 后续独立小件**，不在本轮实现。

### 出口与后续硬序

- F1–F3 → **post-E 通信故障硬化 CodeMap**（与评估文档 §6.2 P0-2 合并
  输入）；F7 → **独立 baseline 判据小件**；F8 → **运维契约先闭合**，
  代码防线另立。
- 后续硬序：**① 通信故障硬化 → ② D15 退役决策 → ③ robot-viz Rerun 桥
  → ④ F7 baseline 判据小件**；在 F1/F3 副作用语义拍清前**不开通信
  实现**。
- 本条已按 Reverse Sync round 2/3 修正折入，随段(10) 统一 docs 收口 commit 提交。

---

## 2026-09-01 · SerialRT 线程调度类 opt-in FIFO（XLeRobot 部署）——§4.6 偏离登记 + D-BI3 r5 旧栈 disable 拍板（**状态：偏离 ×1 + 决策修订 ×1 + 认知修正 ×1**）

### 偏离 1：SerialRT 线程调度类 opt-in FIFO（不改 00_design.md）

- **设计原文**（§4.6）：SerialRT adapter 挂 `s0_serial_io_thread`
  （**普通优先级** + select! 主循环）；"SoftRT / SerialRT 试图挂 RT
  线程即拒绝启动"。执法点针对 **adapter→线程分配**——本偏离**未动**
  （SerialRT 仍只挂 `s0_serial_io_thread`，不挂 `s0_rt_thread`）。
- **偏离内容**：`s0_serial_io_thread` 自身的调度类由钉死"普通"改为
  **opt-in 可配**：`SerialIoThreadConfig` += `sched_fifo_priority` /
  `cpu_affinity` / `strict_rt`（`s0_daemon --serial-fifo <1..=99>` +
  既有 `--cpu` / `--strict-rt` 透传）。**缺省不传 = 普通优先级，
  与设计逐字一致**；启用面仅 XLeRobot 双臂部署 unit
  （`--serial-fifo 70 --cpu 4|5 --strict-rt`，配合 Orin
  `isolcpus=4,5`）。commit `48cc4a9`。
- **依据**：用户拍板 RT 加固（2026-09-01"①②③都做"）+ 三组 600 s
  基线对照 p50 10.2 → 4.1 µs（2.5×）。**p99/p999/max 的 10–20 ms
  尾部不作依据**——经变频判别（hz=10/50/100）证为速率相关错拍伪影
  （三种调度配置同形、max ≈ 恰一个发布周期、hz=10 时消失）；曾误判
  "调度尾延迟"，commit body 已 amend 更正（codex round 1 阻断 3）。
- **验证与回滚口径**：落 `10_verification.md` 附录（ps -L 验
  FF70@4|5 / read-state 回归 / 回滚 = unit 撤三 flag 即回普通优先级）。
- **Spec 登记**：P.4 PX-56 / PX-57。

### 认知修正（codex round 1 阻断 1）：CPUAffinity 继承掩码不是上限

- 原 unit 写 `CPUAffinity=0-5`，依据"进程掩码不含目标核则线程
  `pthread_setaffinity` EINVAL"——**该认知错误**：继承掩码是初值不是
  上限，线程可 `sched_setaffinity` 越出（`taskset -c 0 taskset -c 1
  true` 返回 0 实证；本 session 基线进程继承 0-3 掩码仍成功钉 4/5）。
  硬上限只有 cgroup cpuset（systemd `AllowedCPUs=`）。
- 修正：unit `CPUAffinity=0-3`（普通线程圈定杂务核），serial 线程
  自行钉 4|5；**刻意不设 `AllowedCPUs=`**（cpuset 是硬上限，会真拦
  serial 线程进隔离核）。

### 决策修订：D-BI3 r5——旧栈 coordinator/web `systemctl disable`（用户拍板覆盖 r3）

- **r3 原文**：demo 跑通后旧栈仍 stop-only；disable / 删除 / 破坏回滚
  的禁令延长到"post-E 生产安全硬化候选闭环前"。
- **触发事实**：2026-09-01 isolcpus 重启后旧栈按 enabled 状态开机自启
  占用双串口——r3 的 stop-only 假设了无重启场景；每次重启需人工
  stop 才能恢复受监督运行。
- **用户拍板（2026-09-01）**：`systemctl disable
  xlerobot-coordinator.service xlerobot-web.service`。**覆盖 r3 的
  disable 禁令**；unit 文件保留、不删除、不 mask。
- **回滚路径显式收窄（本条即登记，不以"unit 保留"含糊）**：回滚由
  r3 时代的"直接 `systemctl start`"变为**"显式 `systemctl enable`（如
  需自启）+ `systemctl start`"**——多一步显式操作。
- **不变项**：正式退役时点不变（post-E 生产安全硬化候选闭环后）；
  **D15 保持 OPEN**；收口宣称口径仍为"受监督功能 demo 跑通"。
- 同步：`v0.5_dimos_buyin_decision.md` r5 修订块。
- 审阅人：codex（round 1 阻断 4 要求登记）/ loosqk（拍板）。

---

## 2026-08-29 · 候选 E 段(9)：部署件段——handoff / guard / preflight / provision + systemd unit ×7（**状态：契约落地登记〔D2 第 1 条 / D12 / D13〕+ 形态偏差登记 ×1 + 实现口径补充 ×4**）

### 为什么需要部署件（D13 根因，非新增语义）

arm-only S0 **不驱动也不能 `EStop`** head / base 电机，旧栈退出只
best-effort 断扭且吞失败——切换窗口内 head / wheels 的扭矩与速度状态
**未知**。更关键的是 `auto_enable = false` **只防新 S0 自动上扭，不消除
崩溃残留扭矩**：Feetech `connect()` 只建连接 / 翻译 id / 建缓存，**不写
`Torque_Enable = 0`**；S0 在 Enabled 崩溃后本臂 ID 1-6 在新 S0 起来时仍
带扭。故真机启动任一 arm-only S0**之前**必须对该 bus 全部电机 ensure-safe，
失败即 **fail-closed 禁止切换**。

### 落地形态

- **`robot-s0::deploy`**（新模块，5 文件）：`types`（`DeployArm` +
  D13 精确 ID 集：左 bus1 ID 1-8 仅断扭 / 右 bus2 ID 1-6 断扭 + 7-9 先零速
  再断扭；写条数 左 8 / 右 12 / handoff 20）、`counter`（连败预算 0..=5 +
  稳定 inode `.lock`）、`provision`（latch 三分支 + 统一 dir fsync）、
  `preflight`（`DeployTransport` trait + `ensure_safe_arm` /
  `ensure_safe_handoff`）、`guard`（pre-increment 六步 + 退出码表）。
- **`xlerobot_deploy` bin**（`required-features = ["xlerobot-real"]`）：
  `guard` / `preflight-arm` / `preflight-handoff` / `provision`。
- **systemd unit ×7** + runbook `scripts/systemd/README_dimos_replace.md`。

### 硬契约（本段新锁）

1. **写序有序且逐条可断言**：左 `[T1..T8]`、右
   `[T1..T6, V7,T7, V8,T8, V9,T9]`——**每轮零速严格先于同轮断扭**；单项
   失败继续、末尾聚合；一侧串口构造失败**不阻断**另一侧完整执行。
2. **guard 是 `ExecStart` 主进程**（unit **无 `ExecStartPre`**）：
   `RestartPreventExitStatus=86` 只作用于主进程。
3. **计数 = 未成功 guard 尝试数**（pre-increment）：递增早于任何 pre 步，
   慢 preflight 中被 SIGKILL 也保留已递增值。
4. **锁打在稳定 `.lock` 上，永不 rename / 删除**：`flock` 绑定 inode，
   锁在会被 rename 的 `.count` 上时竞争进程能同时"持锁"（反向复现已入
   单测）。
5. **R10-1 config 单源**：guard 自动追加 `--config <config_path>`；`--`
   之后自带 `--config` = 配置错误退 86。三处引用同一绝对路径，
   `crates/robot-s0/tests/deploy_units.rs` 用**真的 `build_s0_args`**
   跑一遍锁死（非字符串比对）。
6. **86 是 `s0_daemon` 永久保留退出码**（AC-E3）：`exec` 后
   `RestartPreventExitStatus=86` 同样作用于 daemon，误用即失去 watchdog
   重启。源级回归锁断言 `s0_daemon` 退出码表 = {0, 1, 2}。

### 形态偏差登记（Spec P.4 PX-30）

Spec P.2.9 写"robot-s0 lib 新模块（`xlerobot-real` feature 域内）"。实施
**只把真串口实现与 bin 放进该 feature**（`preflight::feetech` /
`[[bin]] xlerobot_deploy`），**纯逻辑不加 feature 门**——否则
AC-E16 / E21 / E22 的 unit 判据在默认 gate（`cargo test --workspace`，
default = `xlerobot`，不含 `real-hw`）里根本不编译、更不会跑到。真硬件
依赖（serialport / libudev）仍只由 `xlerobot-real` 拉入。

### 实现口径补充（Spec 未定，PX-31..PX-33 + PX-35）

- **前置校验归属**：`s0_bin` 非绝对 / 不存在、`--` 后重复 `--config` →
  归 `GuardExit::ExecFailed`（Spec 已定义其为"配置级、不可自动恢复 →
  86"同族），**不新增 enum 变体**；且两项校验放在**取锁之前**——必然
  重现的配置错误不该消耗连败预算，86 本身已停自动重启。
- **普通失败码定值**：preflight **70** / provision **71**（避开 86 与
  `s0_daemon` 0/1/2、S1 3/4/5）。
- **latch 路径单源**：guard provision 的目标**跟随 s0 args 的
  `--latch-path`**，缺省才按 `robot_id` 回填
  `/var/lib/robot/<id>/estop_latch`（与 `s0_daemon` 默认规则逐字一致）
  ——保证 provision 的正是 daemon 将读的那一份。
- **readback 覆盖面**：D13 第 3 步"每电机每安全寄存器读回"——写失败的项
  **仍执行 readback**（写失败已单独记账，readback 只作补充证据）；
  feetech 真实现 `connect(false)` **不做 handshake**，因为 handshake 会在
  任一电机失联时整体失败，与"单项失败继续"冲突。

### 边界（本段**不**做的事）

- **不执行段(10-demo) 的任何真机动作**：本段只交付部署件、unit 与
  runbook；上电、`SetEnabled(true)`、单臂 / 双臂动作全部留在段(10-demo)。
- **旧栈 demo 期间 stop-only**：正式退役 / `disable` / 删除 / LCM 随仓
  退出全部等 post-E 生产安全候选闭环（D-BI3 r3）。
- **D15 仍 OPEN**：AC-E25 不属于 demo gate；`§6.7 #1` 的 S0-dead 硬件
  半句仍按 D15-R (c1+) 显式 demo 例外。

### round 1 回修（reviewer 复核，2026-08-30；3 P0 + 3 P1 + 1 同轮修正）

| # | 缺陷 | 修法 |
|---|---|---|
| P0-1 | **`arm` 与 config 未绑定 → 可能静默操作错 bus**：`--arm` 决定安全写序、config 决定物理串口，两者互不校验；`--arm left --config …right_arm.toml` 打开 bus2 却跑左侧 `[T1..T8]`，干扰在跑的右臂且漏掉 wheel ID 9 | 新增 **`deploy::identity`**（D2 身份表代码侧唯一副本）：**打开串口之前**逐字校验 `[robot].id` / `[[hardware]].id` / adapter / by-id 串号 / `motor_ids` / joints / `auto_enable=false`；guard 另加 argv 侧校验（`robot_id` 与转发给 `s0_daemon` 的 `--robot-id` / `--hardware-id` 必须存在且 == D2）→ **argv 身份 == D2 == config 身份**三方闭环，两级都在取锁之前 |
| P0-2 | **有限 `TimeoutStartSec` 中途截断 fail-closed 聚合**：`180s` / `120s` 与 D13"可证明总时限"**未采**的拍板冲突，可能在部分电机已写时杀进程 | handoff + 两条 arm S0 显式 **`TimeoutStartSec=infinity`**，`deploy_units` 锁定 |
| P0-3 | **latch 单源在重复参数下不成立**：`s0_daemon` 对重复取**最后一个**且不认等号写法，guard 取**第一个**且接受等号 → provision A、daemon 读 B | guard 与 daemon **同 tokenize 规则**，并对 `--latch-path` / `--robot-id` / `--hardware-id` **拒绝重复 / 缺值 / 等号写法**；`latch_path()` → `resolve_latch_path() -> Result`，在取锁前先行解析 |
| P1-4 | **生产 S1 / gateway 误用测试 watchdog 值 2s**（D12 拍板 5s，PX-16 已区分） | 四份生产 unit 改 **5s**；S0 arm 与 `*-freeze-test` 模板保持 2s，正反双锁 |
| P1-5 | **AC-E21 dir-fsync 重试序列未被测**：原测试第一段在写文件阶段就失败，第二段只测成功侧 | 抽 **`ProvisionIo` seam**（生产实现全委托 `estop_latch`，测试实现在**真文件系统**上按次注入失败）→ 补两条序列：dir-fsync 失败后重试命中 `Untouched` **仍必须再 fsync 且 0 写**；backup 写失败后重试补齐且 primary inode 不变 |
| P1-6 | **AC-E22 kill-guard 判据缺失**：同进程单测证明不了 SIGKILL 之后的性质 | 新集成 target `deploy_guard_kill`：父测试用 `current_exe()` 重执行本测试二进制作子进程（**不新增 bin**），在"已递增、未完成"窗口 `SIGKILL` ×5 → 计数逐次保留 + flock 每次由内核释放 + 第 6 次退 86，`s0_daemon` **全程从未 exec** |
| 同轮 | `read_count` 的 `trim().parse()` 放行空白 / 多余换行 / 前导零 / 无换行裸值，与"含多余内容即 malformed"注释自相矛盾 | **有界读 ≤ 3 字节** + 只接受 writer 唯一输出形态 `N\n`（6..=9 归 `OutOfRange`，其余 `Malformed`） |

reviewer 同轮 PX 裁决：**PX-30 / PX-31 / PX-33 / PX-34 / PX-35 ACK**；
**PX-32 方向 ACK、实现按 P0-3 收紧**（见 Spec P.4 PX-38）。

### round 2 回修（reviewer 复核，2026-08-30；2 P0 + 1 文档冲突）

| # | 缺陷 | 修法 |
|---|---|---|
| P0-1 | **D2 身份闭环仍不完整**：串口只做 `address.contains(bus_serial)`——**伪后缀**（`…_5A7C121427-if01`）、**更长伪串号**（`…_5A7C1214270-if00`）都能过；且**完全没校验 `calibration_paths`**。后者住 `extras`，`s0_daemon` 的读法把「缺失 / 非数组 / 元素非字符串」一律静默折叠成空数组，而空数组在 feetech factory 里等价于「无标定照常启动」——整条链零报错 | `ArmIdentity` 增 `address`（**完整 by-id 路径逐字相等**）与 `calibration_file`；`calibration_paths` 必须**恰好一项、绝对路径、文件名 == 本臂标定文件**（D2 arm-only 明确不需要 head/base.json）。新增反向：伪 by-id 路径 4 档 + `calibration_paths` 7 档 |
| P0-2 | **「clippy 全绿」无法复现**：判据用 `grep -cE '^(error\|warning)'`，而 `cargo clippy` 对**已 fresh 的编译单元不重放诊断**——缓存命中输出为空、计数恒 0。实测 rustc 1.100.0-nightly (2026-08-29) 下 workspace 有 **21 处新 lint**（`assert_is_empty` ×20 + `needless_range_loop` ×1） | 21 处机械 fix-forward（既有路径 17 处走**独立 🎨 commit**，段(9) 自有 4 处折回本段）；**永久 gate 口径**：clippy 判定一律**以退出码为准** + 强制重新检查 + `--keep-going` 一次汇总全部 crate |
| P1-3 | **PX-32 与 PX-38 给出相反的有效契约**：PX-32 仍称接受 `--latch-path=<p>`、不新增禁止项，并引用已不存在的 `GuardConfig::latch_path` | PX-32 标注 **已被 PX-38 取代**，原文划删除线并写明不成立的理由；当前有效契约与 API（`resolve_latch_path() -> Result`）统一指向 PX-38 |

reviewer 同轮确认闭合：校验位于取锁前、`TimeoutStartSec=infinity`、生产
watchdog 5 s、真文件系统 fsync 重试、SIGKILL 计数保留、canonical counter。

### 测试增量

`robot-s0` lib **223 → 277**（+54：types 3 / counter 8 / provision 8 /
preflight 10 / identity 7 / guard 18）+ 新集成 target `deploy_units`
**15** 与 `deploy_guard_kill` **2**（后者含一个父进程模式空转的子进程入口
case）。`systemd-analyze verify` 七份 unit 全部无语法告警。

审阅人：reviewer（段(9) round 3 复核通过，2026-08-31；D2 闭合 + PX-32 废止
确认 + 强制重检三面 clippy exit 0 + 基线锁定 1452 PASS / 0 FAIL）；
登记人：Claude。

---

## 2026-08-29 · 候选 E 段(7)：双臂协议 + workflow cross-process e2e（**状态：契约落地登记〔D4/D5/D6/D9〕+ 既有缺陷发现与修复登记〔S0 采样时刻〕+ 新硬契约 ×2**）

### 双臂成对提交协议落地（Spec D4/D5 + AC-E14）

- 新 `robot_rpc::client::dual_arm_workflow`（PD-E3 共享语义单源：e2e 与真机
  验收 bin **同向消费**，禁止各自复制）。RPC 通路经 `ArmEndpoint` trait 注入
  ——**不依赖 tonic `client` feature**（RB4 边界延续），协议分支全部可在无
  进程环境下确定性单测（25 条）。
- D5 八条逐条落位：并发 join 提交 + **各侧在自己 future 内打点**的 Ack 偏差
  （join 后统一取 now 会把偏差抹成 0）/ 终态精确集 `Completed·Failed·
  Cancelled` + pair success = 两侧 `Completed(Success)` / 提交期部分失败 →
  cancel 已接收侧 / 执行期不对称 → cancel 幸存侧（成功侧先到则**不** cancel
  → partial completion）/ stream 断开走 `GetTaskStatus` 轮询收敛 / 双拒
  **不重试** / Ack 偏差为**唯一** gate 量（`Started.at_ns` 仅诊断）。
- **安全语义（reviewer round 1-4 收紧后的不变量，Spec P.4 PX-28）**：单一
  安全入口 `run_chunk_pair`；两级硬超时（协议 `rpc_timeout` + 端点 transport，
  层级内→外递增）；**协议预算 + 独立收尾预算**（`cleanup_budget` 为 EStop
  预留固定份额，总上限 `run_chunk_bound()`）；**超时 ≠ 明确拒绝**（仅
  `DEFINITE_REJECTION_CODES` 白名单证明未启动；`DeadlineExceeded` /
  `Cancelled` / `Unavailable` / `Internal` / `Unknown` 与 transport 失败一律
  `IndeterminateSubmit` → **先双 EStop** 再 cancel 已知 action）；每次收尾
  **至多一次**双 EStop；纯判据 `AckSkewViolation` 不伪造"已收尾"错误。

### test-only fault backend（QA6 + R3-B1）

- 新 `robot-s1` feature `test-fault-backend` + `FaultInjectBackend`
  （`fault_inject`；分发键 `DwellMs + NO_PAYLOAD_SCHEMA_ID` = **0 新 wire**；
  3 tick 后 `ActionFailure::InternalError("fault_inject")`；cancel 优先于
  failure）。生产 backend 无执行期 failure 触发器（Scripted 运行期只
  Continue/Completed、IK 错误全在 start 阶段），AC-E14 执行期不对称终态 /
  AC-E18 failure 分支靠它驱动。**冲突消除**：启用它的 profile 必须移除
  `mock_dwell`（分发键重合，dispatch 取首个命中），由
  `DualCtxOptions::validate()` 在 bring-up 前拦 + 单测锁两种排列。

### 六进程 e2e（D9"三真同堂首例"）

- `tests/common/dual_ctx.rs`：**2×(s0+s1+gateway)** bring-up（PD-E3 全套：
  `dimos_e2e_<pid>_<seq>_{left,right}` / 固定 `left_arm`・`right_arm` /
  两端口 `127.0.0.1:0` 断言不同 / `GLOBAL_LOCK` 串行 / latch provision /
  s0 marker 后再起 s1 / 逆序停机 + **停机后** forensic dump）。
- `tests/dimos_workflow_e2e.rs` 11 case 覆盖 AC-E2/E4/E6/E7/E9/E11/E14/E15/
  E18（含 E18 矩阵四分支各自独立 ctx、E6 SIGSTOP 真 s0、E2 独立 mini 拓扑 +
  raw 注入器 + 正向对照）。

### **既有缺陷发现与修复**：S0 采样时刻（Spec P.4 PX-26）

- **现象**：真 S0 + 真 S1 首次同堂时，S1 启动锚定必失败——
  `fatal: startup anchor failed: S0 state sample is stale: stamp_ns=0`。
- **根因**：`JointStateBuf.stamp_ns` 契约规定由**调用方**填（adapter 不持
  时钟），但 S0 两条 tick 路径（`rt_thread` / `serial_io_thread`）在
  `adapter.read_state()` 后**完全漏填** → `pub_thread` 拿它当 body 采样时刻。
  **旧行为准确口径**：S0 未提交权威采样时刻——Feetech 及 mock 启动期为 0；
  旧 mock 在命令生效后还可能**回显命令戳**（`write_command` 把
  `cmd.stamp_ns` 抄进 state）。两者都不是可信采样时刻。修复后仅在成功读取
  时提交采样开始时刻，失败保留最后成功戳。既有 e2e/smoke 用
  `FakeStatePublisher`（自己 stamp）或 `mock_s1`（不锚定）顶替 S0，故长期
  未暴露。
- **修法（事务式提交）**：读进临时副本，仅 `Ok(())` 时打 `clock.now()` 并
  整体提交；`Err` 只计数并**保留 last-known-good 的状态与旧戳**——读失败若
  仍打新戳，会把持续串口故障伪装成新鲜数据；且 trait 不保证失败时不写
  `out`，临时副本同时挡住"失败前部分写入"污染快照。
- **配套**：`ManipulatorAdapter::read_state` 契约固化（stamp 归属调用方 /
  失败输出不得当新样本）；`MockHardRTAdapter` 对齐（`read_state` 保留调用方
  戳、`write_command` 不再抄命令戳，`MockSerialRTAdapter` 委托同源）；两条
  tick 路径各一条 `read_failure_freezes_state_and_stamp` 回归锁 + mock
  `read_state_preserves_caller_stamp`。
- **影响面（部署侧留意）**：所有 S0 部署的 `JointState` body `stamp_ns` 从
  **缺失或错误语义**（0 / 命令戳）变为**采样开始时刻**；仓内 grep 无依赖
  旧语义（恒 0 或命令戳回显）的代码。

### 新硬契约 ×2（Spec P.4 PX-27 / PX-26）

1. `ActionBackend::start`：**`Err` ⇒ 零副作用**（不产生执行状态 / 不发布
   `JointCommand` / 不触碰硬件）——`FailedPrecondition` 被列入"可证明未启动"
   白名单、据此**不**触发双臂 EStop，该分类的长期正确性依赖本契约。
2. `ManipulatorAdapter::read_state`：adapter 不负责 `stamp_ns`；调用方只在
   **成功读取后**提交；失败输出不得被当作新样本。

### 边界

- 0 新 wire（fault backend 复用既有分发键；IK/Scripted payload 均既有
  schema）；robotctl 0 改；design.md 0 改；`run_gateway_with_ready` 与既有
  四个 e2e 的 bind-ready 语义 0 改。
- IK 分支只在**右臂**执行且只验 D3 reset 规则（PX-25 宣称边界）。
- 段(7) 未做：真机验收 bin（段(9)/(10)）；mid-chunk 抢占（非 E 目标）。
- gate：cargo fmt / clippy workspace `--all-targets` + robot-tests
  `--all-features` 0 警告；**PX-24 八项独立 feature check 全过**（+ 默认）；
  release 重建（workspace + 6 smoke feature bins + hook 产物
  `robot-s1 --features test-freeze-hook,test-fault-backend` /
  `robot-rpc --features test-freeze-hook`）；workspace test
  **1381 PASS / 0 FAIL**（1390 listed − 9 ignored；段(8) 基线 1337 + 44 =
  dual_arm 协议 25 + e2e case 11 + `common::subprocess` dump 测试在 5 个
  e2e target 各跑一次 5 + S0 读失败冻结锁 2 + mock stamp 契约锁 1；**新
  e2e 随 `cargo test --workspace` 默认跑到**）。`fault_inject` 的 7 条**不在**
  默认 workspace 计数内（`test-fault-backend` 默认未启用）——单列 feature
  gate 复核：`cargo test -p robot-s1 --features test-fault-backend`
  **330 passed / 0 failed**；七 smoke 全绿（crash 5/5 +
  `--use-mock-s1` 2/2 / scripted 5/5 / ik 5/5 / pubsub all〔c3=false〕/
  clear_estop all / robotctl 12/12 / freeze_systemd 1/1）+
  `sd_watchdog_smoke --phase all` **8/8 ×2** + `dimos_workflow_e2e`
  **12/12**（单跑 42.5 s）。依赖边：**0 新 crate**（robot-rpc /
  robot-s1 / robot-tests 顶层直接边与段(8) 后一致，本段只加
  robot-s1 内部 feature 与 test target）；`cargo build -p robot-rpc
  --timings` 252 units / 4.5 s；`cargo deny check` 与基线**逐条一致**
  （licenses FAILED 既有 baseline + advisories FAILED
  `RUSTSEC-2026-0258` h2 既有 + bans ok〔3 条 duplicate warning 既有〕），
  **0 新 violation**。hook 产物跑完后 `cargo build --release --workspace`
  恢复。

审阅人：reviewer（段(7) 复核通过，2026-08-29；基线锁定 1381 PASS / 0 FAIL）；
登记人：Claude。

---

## 2026-08-27 · 候选 E 段(8)：sd_notify 段——S1 / gateway liveness-gated systemd watchdog + 单一实现迁移 + 双向 smoke（**状态：偏离登记〔B1 决策 #7 解锁〕+ 依赖边登记 + PX-4 闭合**）

### B1 决策 #7 显式解锁（S1 / gateway sd_notify；Spec D12 + P.2.7）

- v0.3 B1 冻结 "#7 S1 不做 sd_notify"（`s1_daemon.rs` 模块 doc 决策表）。
  段(8) **显式解锁**：`s1_daemon` 全部 worker 就绪（锚定 + state cache +
  4 rr server 线程 + tick loop metrics 即将由主线程推进）后 `READY=1`；
  liveness-gated `WATCHDOG=1`（7 observer：tick loop / heartbeat pub /
  rr task_invoke / task_cancel / task_status / clear_error / state cache，
  各自**独立 `loop_tick_count`**、空闲亦推进——D12 r3 T4 第 1 条，不用请求
  计数）；shutdown 首步 `STOPPING=1` → 停 watchdog → 既有 stop 链原样。
  `NOTIFY_SOCKET` 未设 = no-op（既有 e2e / smoke 0 感知）。生产 S1 unit
  属段(9) 部署件。
- gateway（`robot_rpc_gateway`）同段落地：bind-ready **原样转发**既有
  `ready_tx`（4 个 cross-process e2e 语义 0 改，255 / 255 PASS）；
  **READY=1 门槛 = tonic loopback 探针首成**（`systemd_probe.rs`：500 ms
  一次 `GetState(hardware_id="")` 期望 `InvalidArgument`，QA4——只经
  transport + handler 入口，不依赖 S0 / S1 / cache 首帧；path 带 package
  前缀〔R5-4〕；`tonic::client::Grpc<Channel>` + `tonic_prost::ProstCodec`
  + 既有 prost message，**不开 `client` feature**〔RB4〕；首成上限 10 s →
  超时 = 新 `GatewayError::ReadyProbeTimeout` exit 1）；observer = 6
  rr_io + estop_pub + **每 hardware 一条** `state_pool:<hardware_id>`
  （R6-5 禁聚合求和）+ 探针 `success_count`；停机 `STOPPING=1` → 停
  watchdog / 探针 → 内部 serve-shutdown → join server（D12 第 4 条）。

### 单一实现迁移（PD-E6(c)）+ PX-4 闭合

- S0 `systemd_notify.rs` 迁入 `robot-health::systemd`（R3-B2 API：
  `notify_ready / notify_stopping / notify_watchdog / notify_status` +
  `LivenessObserver { label, tick }` + `WatchdogConfig { thread_name,
  startup_grace_ping }` + `spawn_watchdog(observers, cfg) ->
  Option<WatchdogHandle>` + `WatchdogHandle::stop(self)`）；S0 传
  `"s0_systemd_wd"` 原值，ping 节奏 / grace / 判停 **bit 等价**；S1
  `"s1_systemd_wd"`、gateway `"gw_systemd_wd"`。
- **PX-4 闭合**：watchdog 内核拆为纯函数 `evaluate_cycle(observers, prev)
  -> bool` + `WatchdogIo { ping, sleep_interval }` trait（生产 `SdNotifyIo`
  分片 sleep ≤ 50 ms 响应 stop）；单测以 ScriptedIo 逐周期驱动，判停 /
  grace / 恢复全部确定性，不依赖真 socket + 50 ms 墙钟窗（原
  `watchdog_withholds_ping_when_observer_stalled` 满载 flake 根因消除；
  workspace test 可满载跑）。

### test-only freeze hook（R5-5 wire / 生命周期契约落地）

- robot-s1 / robot-rpc 各新增 feature `test-freeze-hook = []`（默认关，
  生产 release 0 编译）：cfg 守门 `liveness::freeze_check`（loop 顶单次
  load，非 0 且未过期 busy-spin；tonic 探针 task 走 5 ms 分片 sleep 不占
  runtime worker）+ `freeze_control` 线程（**独立 topic** S1
  `/<robot_id>/s1/test/freeze` / gateway `/<robot_id>/gateway/test/freeze`，
  QoS 沿 S0 字面 1 pub / 1 sub / history 1 / buffer 2 / safe_overflow）；
  `#[repr(C)] TestFreezeCommandV2 { target_pid: u32, kind: u8, index: u8,
  pad: [u8; 2], duration_ms: u64 }` + `WorkerKind`（S1 Tick /
  HeartbeatPub / RrServer(0..4) / StateCache；gateway RrIo(0..6) /
  EstopPub / StatePool(i < hardware 数) / TonicProbe）；`target_pid` 不匹配
  → "stale freeze ignored (pid mismatch)" warn 忽略；kind / index 非法 →
  warn 忽略。
- 新 smoke `sd_watchdog_smoke`（robot-tests，feature `sd-watchdog-smoke`，
  required-features）+ 两份 user-service 模板
  `scripts/systemd/xlerobot-{s1,gateway}-freeze-test.service`
  （`WatchdogSec=2s`，沿 S0 模板边界）：每 (daemon, worker) 一 case——READY
  → 注入（target_pid = PID0）→ withholding → 真重启 PID 切换 + is-active
  → journal 硬断言（withholding ≥ 1 / banner ≥ 2 / "READY=1 sent" ≥ 2 /
  "freeze_control thread ready" ≥ 2 / applied ≥ 1）→ **反向步**（PID0
  定向命令 → PID1 stale-ignored、applied 不变）→ **空闲 3 × WatchdogSec**
  不重启 / 无二次 withholding（AC-E19 双向）；默认 8 case（S1 ×4 +
  gateway ×4，含 R6-5 双 hardware 冻 `StatePool(1)` 单线程）。S1 case 进程
  内 FakeStatePublisher（派生 layout，50 Hz）供锚定 + state cache。

### 依赖边登记（P.0 "依赖边变更段" gate）

- 工作区内部边 **robot-s1 → robot-health**、**robot-rpc → robot-health**
  （PD-E6(c) 已拍两条；0 新外部 crate）；robot-tests += optional
  `robot-s1`（path + `default-features = false` + `test-freeze-hook`）与
  feature `sd-watchdog-smoke = ["dep:robot-s1", "dep:robot-rpc",
  "robot-rpc/test-freeze-hook"]`；P.0 活清单三条 release build
  （robot-s1 / robot-rpc `test-freeze-hook` + robot-tests
  `sd-watchdog-smoke`）自本段生效。
- `cargo tree -p robot-tests --edges normal,no-proc-macro`：默认 feature 顶层
  **0 新边**（10 条既有）；`--features sd-watchdog-smoke` 顶层 +2 直接边
  （`robot-s1` 新 optional + `robot-rpc` 既有 optional 被本 feature 拉入）；
  `cargo tree -p robot-s1 / -p robot-rpc --depth 1` 各含 `robot-health`
  内部边。`cargo build -p robot-tests --timings`：默认 feature **177 units**
  （与段(6) 基线同，0 新 unit）；`--features sd-watchdog-smoke` 274 units
  （+97 = robot-s1 / robot-rpc / tonic 链，仅该 smoke feature 承担）。
  `cargo deny check`：与段(6) 基线**逐条一致**（licenses FAILED 既有
  baseline〔assert_no_alloc / serialport MPL / unescaper parse-error〕+
  advisories FAILED `RUSTSEC-2026-0258` h2 既有 + bans ok〔duplicate
  warning base64 / hashbrown / wit-bindgen 既有〕；Cargo.lock 仅增
  robot-tests → robot-s1 workspace 边，0 版本变更），**0 新 violation**。

### round 2 回修（reviewer 段(8) round 1：阻断 1-3 + 拍板 ×2 + 非阻断 1）

- **Reverse Sync ①——继承缺陷修正（阻断 1，P0）**：`robot-health::systemd`
  （沿 v0.2 S0 `systemd_notify.rs` 原实现）grace ping 后先 sleep 一个
  `usec / 2` 才取初始 snapshot、再 sleep 一个才首次 enforce——首个 ping 间隔
  = `WATCHDOG_USEC` 整，注释宣称的 "50% 余量" 从未成立，健康进程启动期可能
  被 systemd 误杀（v0.2 task 24-29 / v0.3 A2 freeze smoke 均未暴露：只看
  withholding 路径，不看首个间隔）。修正：cycle 0 grace 喂 + **即刻**
  snapshot，cycle 1 起每 `usec / 2` 比较 → ping；`ping_interval = max(usec
  / 2, 1 µs)`（`WATCHDOG_USEC = 1` 不再 busy-loop）。**S0 行为随之变化**
  （首个 enforce ping 提前一个间隔）——上文 "S0 bit 等价" 宣称**撤回**，改为
  "判停语义等价 + ping 节奏修正"；单测改记录每次 ping 所在 cycle（grace@0 /
  首次 enforce@1 回归锁）。`5510dc3`；Spec P.4 PX-21。
- **阻断 2（P0）探针有界性**：`ATTEMPT_TIMEOUT` 400 ms 包 `ready()` +
  `unary()` 并与 stop `select!`（原仅 connect 超时——服务端 accept 后不响应
  即永挂）；`ProbeHandle::join` 两级有界（正常 2 s → abort → 确认 500 ms → detach，
  round 3 补正）；gateway `await_bind_ready`
  / `ready_gate` 监听外部 `shutdown_rx`——READY 前停机 → 内部 serve-shutdown
  → 有界收 probe + server → `Ok(())` 正常退出（不发 READY、不起 watchdog；
  exit 0）；`join_server` ≤ `SERVER_JOIN_TIMEOUT` 5 s 否则 abort + `ServerTask`
  （运行期停机同用；两处 join 的 abort 确认上限见下 round 3）。新单测 4：
  hung-acceptor（只 accept 不回 h2 preface）
  attempt 经超时计 failure、stop → join < 1 s；`ready_gate` deadline 1 s →
  `ReadyProbeTimeout{1000}` 总耗时 < 4 s 且 server 已 join；READY 前外部
  shutdown → `Ok(None)`；bind 前 shutdown → `Ok(None)` + `ready_tx` 收
  `ServerTask`。`798f0c2`；Spec P.4 PX-22。
- **阻断 3（P1）smoke 握手**：publish 前有界等 journal "freeze_control
  thread ready"（PID0 ≥ 1 / PID1 ≥ 2；`--freeze-ready-wait-ms` 默认 5 s）——
  spawn 返回 / READY 都不等于 subscriber attach，history_size=1 + 固定
  sleep 非确定性。`6ddbe33`；Spec P.4 PX-23。
- **Reverse Sync ②——候选 A 边界收窄（PX-19 拍板 = feature 隔离，不下移
  helper）**：候选 A summary / Spec P.4 E2 "robotctl_smoke 只 dep:tokio +
  serde_json、不引 robot-rpc/tonic" 自段(4) 起已被 `robot_rpc::client::
  layout` 使用打破（单 feature 构建 E0433，gate 合并 feature 集掩蔽）。本次
  撤 optional `robot-rpc` 声明上的全局 `client` feature，四个真用 tonic
  client 的 smoke feature 各自显式 `robot-rpc/client`，`robotctl-smoke` 补
  `dep:robot-rpc`。**准确口径（round 2 复核事实更正——"不拉 tonic" 不成立：
  `dep:robot-rpc` 的普通依赖本身会编译 tonic server 栈，`cargo tree -p
  robot-tests --features robotctl-smoke` 实证）**：robotctl-smoke **不显式
  启用 `robot-rpc/client`、也不直接启用 `dep:tonic`**；**单 feature 构建**不
  生成 `RobotControlClient`，源码不调用 tonic client；合并 feature 构建可能因
  feature unification 生成 client——边界由**单 feature gate（PX-24）+ 源码
  纪律**共同锁定。候选 A summary + `v0_5_operator_cli.md` P.4 E2 行追加同口径
  Reverse Sync 注；`robot-rpc/Cargo.toml` 中"`sd-watchdog-smoke` 同时启用
  `client`"的旧述一并更正。8 feature 各自独立 `cargo check --all-targets`
  全过，并按 **PX-24 写入 P.0 永久 gate**。`38412ec`。
- **拍板 ②**：测试模板 `WatchdogSec=2s` / 生产 5 s 口径保留（PX-16），前提
  = 阻断 1 已修（daemon 侧永远从 `WATCHDOG_USEC` 派生）。
- **非阻断登记（PX-20）**：gateway 每 hardware 一次 `Box::leak` 生成
  `state_pool:<hardware_id>` label（`LivenessObserver.label: &'static str`，
  R3-B2 签名锁）——进程内有界，但**不**视为"可忽略"；撤销条件 = label 改
  `Cow<'static, str>` / `String`（R3-B2 签名修订，独立 commit）。

### round 3 回修（reviewer 段(8) round 2：P1 ×3；watchdog / probe / 握手 / PX-19 方向已确认闭合）

- **P1-1 `abort()` 后仍是无界 await**：tokio `abort()` 是协作取消——任务未
  再次 yield 时 `JoinHandle` 不会完成，故 round 2 的 "join ≤ 2 s / 5 s" 并非
  硬上界。改为**两级有界**：正常 join 上限（probe `JOIN_TIMEOUT` 2 s /
  server `SERVER_JOIN_TIMEOUT` 5 s）→ `abort()` → **取消确认上限**（新
  `ABORT_JOIN_TIMEOUT` / `SERVER_ABORT_JOIN_TIMEOUT`，均 500 ms）→ 仍未确认
  则**严重日志 + detach 返回，不再阻塞退出**（server 侧错误串区分 aborted /
  detached）。两处各抽可注入超时的内部版（`join_with_timeouts` /
  `join_server_with_timeouts`）并加回归单测：任务体为**阻塞** sleep（永不
  yield）时 join 必须在两窗之和量级返回。Spec P.4 PX-22 改写为"正常 join 上限
  + abort 确认上限 + 硬上界 = 两者之和"。
- **P1-2 Reverse Sync 未覆盖生效中的权威文本**：`v0_5_dimos_replace.md`
  P.2.7 与 `s0_daemon.rs` 的 systemd 调用点仍写 "S0 行为 bit 等价"——两处改为
  "**调用面单源迁移**：判停语义保持，首次 enforce 节奏按 PX-21 修正"。
  CHANGELOG round 1 历史记述保留（round 2 已明文撤回）。
- **P1-3 "不拉 / 不依赖 tonic" 与依赖树不符**：见上 Reverse Sync ② 的准确
  口径（`cargo tree` 实证 `dep:robot-rpc` 必编 tonic server 栈）；同时更正
  `robot-rpc/Cargo.toml` 中 "`sd-watchdog-smoke` 同时启用 `client`" 的旧述
  （该 feature **不**启用 client）。**新 PX-24 = 8 项独立 feature check 写入
  P.0 永久 gate**（合并 feature 集会掩蔽单 feature 构建失败，PX-19 根因；
  新增 feature 必须同步入列）。
- round 3 gate（reviewer 指定范围，未重跑全部 smoke）：cargo fmt / clippy workspace `--all-targets` + robot-tests
  `--all-features` 0 警告；**PX-24 8 项独立 feature check 全过**（+ 默认）；
  robot-rpc 232 PASS（default / test-freeze-hook）· 261（`--features
  client`，4 cross-process e2e 0 回归）/ robot-health 66 / robot-s0 231；
  workspace test **1337 PASS / 0 FAIL**（round 2 1335 + 2 = 两处 abort
  确认上限回归锁）。按 reviewer 指定范围**未重跑 smoke**（本轮无行为面
  改动触及 smoke 路径：probe / server join 仅在超时分支加第二级上限）。

### 边界

- 0 新 wire（V2 POD 仅 test-only topic，不进 robot_msg / frozen schema）；
  S0 仅调用面迁移 + round 2 ping 节奏修正（见上 Reverse Sync ①）；robotctl
  0 改；design.md 0 改；`run_gateway_with_ready` 既有 bind-ready 语义 0 改。
- 既有缺陷 PX-19（robot-tests `robotctl-smoke` 单 feature 不可编译）已按
  reviewer 拍板以独立 🐛 `38412ec` 修复（见上 Reverse Sync ②）。
- round 2 gate：fmt / clippy workspace（re-check health·rpc·s1·s0·cli）+ robot-tests
  `--all-features` 0 警告；8 smoke feature 各自独立 `cargo check` 全过；
  release 重建 ×2；workspace test **1335 PASS / 0 FAIL**（round 1 1328 +
  7：watchdog cycle 锁 3 + 探针 / ready_gate 有界性 4）；七 smoke 全绿
  （crash 5/5 + mock_s1 2/2 / scripted 5/5 / ik 5/5 / pubsub all〔c3=false〕
  / clear_estop all / robotctl 12/12 / freeze_systemd 1/1〔S0 新 ping 时序
  下 withholding=2〕）+ **`sd_watchdog_smoke --phase all` 8/8 ×2**（每
  case 先命中 freeze_control ready 握手再注入）；tree / timings（默认 177
  units、sd-watchdog-smoke 274）/ deny 与 round 1 逐条一致。
- round 1 gate：cargo fmt / clippy workspace `--all-targets` + robot-tests
  `--all-features`（PX-5）0 警告；release 重建（workspace + 6 smoke feature
  bins 含 `sd-watchdog-smoke`）；workspace test **1328 PASS / 0 FAIL**
  （基线 1305 + 23：robot-health watchdog 确定性单测 + S1 / gateway
  liveness · freeze_control · probe 单测 + gateway bin）；七 smoke 全绿
  （crash 5/5 + `--use-mock-s1` 2/2 / scripted 5/5 / ik 5/5 / pubsub all
  PASS〔c3=false〕/ clear_estop all / robotctl 12/12 / freeze_systemd 1/1）
  + **新 `sd_watchdog_smoke --phase all` 8/8 PASS ×2**（debug 首跑 +
  release bin 复跑；每 case PID 真切换 + is-active、withholding 1-2 /
  banner 2 / READY sent 2 / freeze_ready 2 / applied 1，反向步 stale
  0→1 且 applied 不变，空闲 6 s 无重启无二次 withholding）；hook 产物
  跑完后 `cargo build --release --workspace` 恢复。

审阅人：待 reviewer 段(8) 复核；登记人：Claude。

---

## 2026-08-27 · 候选 E 段(6)：转换 helper + 基线导入 shim + feetech 换算公共 API（**状态：契约落地登记〔D7 / D8 / D11〕+ 依赖边登记**）

### 单位契约分层落地（Spec D11：helper rad-native + 基线导入 shim；换算权威唯一）

- **E 运行时 helper** `robot_rpc::client::trajectory`（D7 (iii) client 侧
  模块）：`TrajectoryInput { joint_names, points[{ time_from_start_s,
  positions }] }` → `Vec<Waypoint>`（全向量 rad；`dwell_ms = 0`；
  `segment_duration_ms = round(Δt × 1000)`；`velocities` / `timestamp`
  丢弃——两侧皆分段线性，CodeMap §3.1）。**rad-native、不 clamp**（硬件保护
  clamp 唯一权威 = S0 calibration clamp，client 不复制第二道）；NaN / Inf、
  长度不匹配、未知 / 重名关节、时间非有限 / 非单调、段 < 1 ms 或溢出 u32
  → typed `ConvertError` 拒绝（整条轨迹不提交，不降级）。name→index 权威
  = config 派生 `HardwareLayout::joints`（D1 链，禁裸固定索引）。
- **首点语义**（D7 承接 D3）：`points[0]`（提交时当前位形）**丢弃**（S1
  段(5) `scripted_start_pose` 自 state cache 承接起点）；首点时刻**必须严格
  == 0.0**（冻结基线 `executor.py:99` current @ t=0；非零 →
  `ConvertError::FirstPointTimeNonZero` 拒绝——dimos `JointTrajectory.py:87`
  对非零首点会 hold 到该时刻，丢弃即吞掉该 hold 并缩短总时长；PX-13）；逐
  命名关节断言 `|points[0][j] − current[idx_j]| ≤ FIRST_POINT_TOLERANCE_RAD
  = 0.05 rad`（**边界含等**，回归锁 current=0 / 首点=0.05 接受、+1e-12 拒；
  AC-E10 定值，Spec P.4 PX-10；依据：STS3215 hold 噪声 ±10 tick ≈ 0.015
  rad 的 3× 余量，且小于 demo 小幅运动上界 0.087 rad〔AC-E12 ③〕）。
- **缺位填充**（D8）：layout 中未在 `joint_names` 出现的关节，每个
  waypoint 填同 chunk 一次读的 `current[idx]`（只动夹爪 = 5 臂关节填当前
  位，反之同理）。
- **基线导入 shim** `robot_rpc::client::baseline_shim`（**测试侧**，QA5
  typed 化）：`raw12_to_rad(&[f64; 12], left (lo, hi), right (lo, hi)) ->
  Result<Shim12 { values, clamp_count }, ShimError>`——臂 `deg × π/180`；
  夹爪 `[0,100]` 越界 max/min clamp（镜像 dimos adapter 写入前最终 clamp
  `_manipulator_base.py:546`——冻结入口 `executor.py:63` 仅 `/100`，dimos
  两处均不告警）+ 本 shim **新增** `clamp_count` 可观测返回供调用方告警 →
  `/100`（`[0,1]` 仅内部归一化中间值，非输入）→ `rad_lo + u × (rad_hi −
  rad_lo)`；左右夹爪范围**必填**且 `lo < hi`
  （否则 `RangeInverted` fail-fast），NaN / Inf reject；`Shim12::{left_arm,
  right_arm}` 拆 D2(i) 6 维臂向量。"度" / `[0,100]` **只存在于 shim 输入
  层**，E 运行时全程 rad。
- **换算权威唯一**：feetech `raw_to_rad` / `rad_to_raw` / `RAD_PER_TICK`
  自 `adapter.rs` 私有 fn 提升为 `convert.rs` **pub 纯函数**（adapter 热路径
  改调同一份实现；`cargo test -p robot-adapter-feetech` 126 / 126，新增
  0..=4095 逐 tick 往返恒等锁）；robot-rpc **不复制公式**（shim 只收调用方
  算好的 rad 范围；真机验收 bin 经公共 API 从标定 JSON `range_min/max`
  派生）。
- `client::trajectory::build_scripted_payload` 与 robotctl `payload.rs` 的
  fbs 构造同构**并存**（robotctl 0 改硬边界；PX-12）。

### 依赖边登记（P.0 "依赖边变更段" gate）

- robot-tests += `robot-adapter-feetech`（optional），新 feature
  `real-hw = ["dep:robot-adapter-feetech", "robot-adapter-feetech/real-hw"]`
  （段(9) preflight / 真机验收 bin 宿主；PX-11）。
- `cargo tree -p robot-tests`：默认 feature **0 新边**；`--all-features`
  +1 直接边（子树 serialport 4.9 / libudev / nix 0.26 均已在 lock 内——
  robot-s0 `xlerobot-real` 先例）。
- `cargo build --timings`：默认 feature 177 units / 1.8 s（无新 unit）；
  `--features real-hw` 188 units / 7.1 s（+11 = serialport 链，仅真机
  feature 承担）。
- `cargo deny check`：与段前基线**逐条一致**（licenses FAILED 既有
  baseline〔assert_no_alloc / serialport MPL / unescaper parse-error〕+
  advisories FAILED `RUSTSEC-2026-0258` h2 既有〔Cargo.lock 未升级任何
  版本，本段仅记录 optional 边〕），**0 新 violation**。
- `cargo clippy -p robot-tests --all-features` 永久 gate（PX-5）自本段起
  覆盖 serialport 编译面（本机 dev 已验可编）。

### 边界

- `dual_arm_workflow` **0 stub**（QA5：段(7) 原子落）；robotctl 0 改；wire /
  S0 / S1 / config / design.md 0 改；IK / kinematics 0 改。
- unit 侧 AC-E10 全路径：trajectory 17 + shim 9 + convert 7；跨进程
  E9 / E11 / E15 落段(7)。
- gate：workspace test 1305 PASS / 0 FAIL（基线 1272 + 33）+
  七 smoke 全绿（crash 5/5 + mock_s1 2/2 / scripted / ik / pubsub /
  clear_estop / robotctl / freeze_systemd）。

审阅人：待 reviewer 段(6) 复核；登记人：Claude。

---

## 2026-08-27 · 候选 E 段(5)：G1 语义闭环——Idle hold target 源显式解锁 + 末点发布保证 + Scripted 起点改源（**状态：偏离登记〔B1 等价语义收窄版解锁〕+ 行为登记**）

### B1 等价语义显式解锁（Spec D3 第 3 条，收窄版）

- v0.3 B2 reviewer 阻断 4 锁定 "no-active-action 路径必须 sample-by-sample
  等价 B1"（`coordinator.rs` Idle hold ≡ `build_hold_command(anchor_bias)`）。
  本段**显式解锁**（CodeMap Q12 (c) / Spec D3）：`Idle` hold target 源改为
  `Coordinator::idle_hold_target`——初值 = 启动 `anchor_bias`（boot 后首个
  Success-Scripted 之前与 B1 **bit 等价**，`g1_hold_target::
  boot_idle_hold_bit_equivalent_to_anchor` 锁）；**唯一**更新路径 =
  `(backend == Scripted) ∧ Completed(Success) ∧ 末 waypoint target 已发布`
  → `idle_hold_target = 末 waypoint`；其余所有 backend × 终态（Scripted
  cancel / failure、IK / HoldPose / DwellMs 一切结局、未来 backend 缺省）
  显式 reset = `anchor_bias`。终态数据通路 = `ActionInstance::
  successful_hold_target()` 默认 `None`、仅 `ScriptedInstance` override
  （穷尽式单一规则由类型承担；coordinator 只在 `Success ∧ !pending_cancel`
  时问 backend）。形态（`build_hold_command` 路径）不变。
- **Degraded 不解锁**：`run_action_tick_loop` Idle / Degraded 分臂，
  Degraded 永远 `anchor_bias`（Idle 与 Degraded 自此分源）。HoldPose 0 改
  （每 tick 仍读 `ctx.anchor_bias`；AC-E8 bit 等价回归锁）。

### 末点发布保证（Spec D3 第 2 条，S1 阻断根修；行为登记）

- v0.4 候选 1 现状：`dwell_ms == 0`（及大步推 clock 跳过 Dwell）时末
  waypoint 完成即 `Completed(Success)`，该 tick 不产命令帧，coordinator 对
  Completed 帧不 publish → 末 target 可能从未上线。段(5) 起：末 waypoint
  完成且末 target 尚未发布 → 先返一帧 `Continue { target = 末 waypoint,
  progress = 1.0 }`（`Phase::FinalEmitted`），**下一 tick** 才
  `Completed(Success)`；dwell>0 正常节奏路径 Dwell 帧已发末 target（首个
  dwell 帧置 `final_published`）→ dwell 完成直接 Completed，**行为不变**。
- **可观测差异**：dwell=0 / 跳过 Dwell 路径的 Completed 事件晚一个 cmd
  周期（20 ms）且多一帧末 target 命令；`idle_hold_target` 提交严格晚于
  末 target 上线（G1 回拉半闭合，无跳变）。

### Scripted 起点改源 + 拒绝语义（Spec D3 第 1 条）

- `ScriptedBackend::start` segment0 `from` = `ctx.scripted_start_pose`
  （提交时当前位，新 `ActionContext` 字段）而非启动期 `anchor_bias`；来源
  = 新 S1 **常驻** state cache 线程（`robot_s1::state_cache`，同
  `/<r>/s0/<hw>/state` topic，keep-last body bytes，convert-on-read）。
  `Coordinator::submit` 的 Scripted 路径经 `StateCacheHandle::latest_fresh
  (now)` 三重校验（joint_count / `layout_id == config 派生值`〔D1 联动〕/
  `max_stale_ms` 新鲜度，沿既有 `[s1].max_stale_ms`，不新增配置面）；无帧
  / stale / layout 不匹配 / joint count 错位 / 未注入 cache →
  `SubmitError::BackendStartFailed(ActionStartError::Rejected)`（既有 wire
  映射 `FailedPrecondition`），**绝不 fallback** 到 `anchor_bias`。
- **运行时可观测变化**：S0 state 不新鲜（S0 停发 > 200 ms）或 layout 与
  S1 config 不一致时，`InvokeTask(Scripted)` 从"接受并从启动锚点重放"
  变为 `FailedPrecondition` 拒绝；HoldPose / DwellMs / IK 提交不受影响。

### 线程卫生与启动 / 关停（AC-E20）

- `s1_daemon`：锚定成功后 spawn state cache 线程（线程内建 iceoryx2 node +
  subscriber，`sync_channel` 同步 ready 握手；`READY_TIMEOUT` = 5 s **仅为
  握手超时检测点，不是 spawn 返回的硬上限**——ready / error 在超时前到达
  则立即返回；若先触发超时，则置 stop 并等待 factory / 线程结束后 join，
  因此超时路径可能超过 5 s，不承诺返回时延硬上限），**先于**
  `rr_server_threads ready` marker；spawn 失败 = 启动失败 fail-fast（exit
  1 = node / subscriber open / 握手超时族；exit 5 = 线程 spawn 族，沿
  rr_server / reaper 先例；Spec P.4 PX-6）。shutdown 序列新增 step 6：
  `take_state_cache → stop → join`（在 RR / drain 线程 join 之后）。
  subscriber 与 node 线程内按 RAII 正常 Drop（iceoryx2 0.9.3），**无**
  `mem::forget` / `Box::leak`。线程 poll 为**标称** 2 ms cadence（调度无
  硬上界，不构成新鲜度保证）；数据安全由 `stamp_ns` + `max_stale_ms` 在
  `latest_fresh` 守门。
- state topic QoS `max_subscribers=8`（同一 S0State service，所有订阅方
  共享）：S1 锚定 subscriber 在锚定函数返回时已释放；S1 cache **长期占 1
  个 slot**，gateway `StatePool` 常驻另占 1 个，robotctl watch 临时占用——
  当前仍有 headroom（段(5) 前 S1 无长期 slot，本段净增 1）。

### 边界（本段不动）

- 启动锚定 `read_anchor_from_s0_state` **不**校 layout（`expected_layout =
  None`，语义 0 改；共享校验 fn `check_state_frame` 抽取）——layout 不一
  致在段(5) 起由 Scripted submit 拒绝面暴露、由 S0 command gate（段(3)）
  丢弃面暴露；锚定期 fail-fast 未纳入 AC-E3 exit 码表，登记为 post-E 观测
  面候选（Spec P.4 PX-9）。
- wire 0 改 / S0 0 改 / robot-rpc 0 改 / config 0 改 / design.md 0 改；
  robot-kinematics 与 IkBackend 0 改（`ik.rs` 终态仍走缺省 `None` → reset）。
- 影响面：robot-s1（`action.rs` / `coordinator.rs` / `state_cache.rs`
  〔新〕/ `action_backend/scripted.rs` / `bin/s1_daemon.rs` +
  `tests/g1_hold_target.rs`〔新〕）。gate：workspace test 1272 PASS /
  0 FAIL（基线 1222 + 新增 50）+ 六 smoke 全绿 + crash
  `--use-mock-s1 --iterations 2`。

审阅人：reviewer（段(5) 复核通过，2026-08-27；ACK 链 `5ceaf61` → `f01da8b` → 回修）；登记人：Claude。

---

## 2026-07-17 · 候选 E 段(4)：段(3)→(4) layout 过渡窗口正式关闭（**状态：闭合登记**）

- **S1 / mock_s1 改为 config 派生**：`s1_daemon` 撤 `--layout-id` 占位
  默认（`0xCAFE_BABE_DEAD_BEEF`），layout 一律由 config 派生
  （`compute_layout_id`，PD-E5(a) 唯一权威）；`mock_s1` `--config` 必填
  化（PC4），joints 数与 layout 同源派生。
- **显式 `--layout-id` 仅保留在 `s1_daemon` 作一致性断言**（≠ 派生 →
  exit 2）；四个 smoke harness（scripted / ik / pubsub_cleanup /
  robotctl）的进程级 `--layout-id` **整体删除**——`compute_layout_id`
  含 per-case robot_id，进程级单值不可能同时匹配 `--phase all` 的多个
  case（不可执行接口，reviewer 段(4) 复核 P1-1）。
- **client layout 硬校验落地**：`robot_rpc::client::layout`
  （`load_hardware_layout` + `validate_state_layout`，`LayoutMismatch`
  硬失败不得降级；AC-E4 e2e `layout_validate_mismatch_hard_fail`）。
- **段(3) 条目"过渡窗口"段自本条起正式关闭**（历史原文保留不改）：
  S1/mock_s1/harness stamping 全部派生同源，config 模式 S0 的 command
  gate 不再丢弃自家 S1 命令帧。

审阅人：reviewer（段(4) 复核意见闭合）；登记人：Claude。

---

## 2026-07-16 · 候选 E 段(3)：`JointState.layout_id` 值语义 + §3.5.1 部分落地边界（**状态：偏离登记〔解读/部分落地〕**）

### 值语义变化

- `JointState.layout_id` / command gate 的权威值从占位
  `0xCAFE_BABE_DEAD_BEEF` 变为 **config 派生真值**：
  `robot_msg::joint_layout::compute_layout_id(robot_id, hardware_id,
  zip(joints, motor_ids))`（Spec D1.1 长度前缀规范编码，
  `u64::from_le_bytes(blake3[0..8])` 端序显式 LE；**config 顺序敏感**）。
  wire 字段既有（v0.1.2 冻结 `ulong`），**仅值语义变化**——本条即该登记。
- **§3.5.1 字面解读**：design 原文 "sorted_joint_names+order" 自相含混，
  可执行化解读 = **名字 + motor id + config 顺序三成分入编码**（不排序；
  排序会丢 order 成分）。design.md 0 改，解读差异以本条为准。

### 部分落地边界（D1 拍定口径）

- **command 侧 gate（S0 `ipc_thread::poll_command`）**：帧
  `layout_id != authoritative` → **丢弃不写 `LatestCommandSlot`** +
  `unknown_layout_count` `AtomicU64` +1 + 限频 warn（首条必报 + 每 64
  条一报）。**不做**：阈值→状态升级、`JointLayout` 广播/RR 握手
  （Spec "不在范围" 登记，post-E）。
- **CLI-only 边界**：无 `--config`（无 name/id 表）时 authoritative 保留
  既有占位 `0xCAFE_BABE_DEAD_BEEF`（行为与段(3) 前 bit 等价）。
- **段(3)→段(4) 过渡窗口**：S1 / mock_s1 默认仍发 `--layout-id` 占位
  （`s1_daemon.rs` 默认 `0xCAFE_BABE_DEAD_BEEF`），config 模式 S0 的
  authoritative 为派生真值 → **其命令帧在 gate 处被丢弃**（既有 e2e /
  smoke 不断言 S0 命令消费，全绿实测；hold/action 命令的 S0 侧生效在
  段(4) S1 config 派生落地后闭合）。窗口内 **CLI-only 路径**（无 config）
  不受影响。

### 影响面与审阅

- 影响面：robot-s0（`pub_thread` 值源 / `ipc_thread` gate /
  `s0_daemon` resolve 派生）；robot-msg 0 改（段(1) 已 land 编码权威）；
  S1 / gateway 段(4) 接。
- 审阅人：reviewer（候选 E Plan Approved 范围内段(3) 执行）；登记人：
  Claude。

---

## 2026-07-15 · 候选 E 受监督 demo 例外（§6.5.1 硬件 fail-safe）+ D15-R (c1+) 拍定（**状态：偏离登记**）

### 背景与能力核实

候选 E Plan round 6 完成 D15 ① 能力核实：**STS3215 无已文档化、可配置的
command watchdog，且当前真机失联持续带扭**。证据链三锚点：dimos-deploy
`feetech_io/registers.py`（表头自证完整表在 lerobot 源码）无该类寄存器 /
lerobot `STS_SMS_SERIES_CONTROL_TABLE` 完整 58 项逐一核对无该类寄存器
（`Protection_Time(35)` / `Over_Current_Protection_Time(38)` 为过载/过流
保护时长非通信看门狗）/ 真机失联 hold-last-position 持续带扭行为；≥10 s
台架观察保留为行为证据。依候选 E Spec D15 已拍"不支持"分支回 Spec 重拍 →
**D15-R 拍 (c1+)（reviewer，E Plan round 7，2026-07-15）：受监督 demo
豁免 + fail-safe 接口预留**——按最新目标收缩：先跑通受监督功能 demo，
生产级安全闭环后置。

### 偏离内容（demo 例外）

- **偏离对象**：`00_design.md` §6.5.1（`:1107` 区块）规则 3 "不支持硬件
  timeout 的电机不得进入 `ENABLED.ACTIVE` 自动控制" + `:1114` CI 验收
  "kill -9 → <500ms fail-safe"；**以及 §6.7 核心不变量 #1 的后半句**
  （round 8 补点名："S0 不在时靠硬件 fail-safe"——S0-dead 硬件保证部分
  作 demo 例外并保持 OPEN；**#1 的 S0-alive 软件保证不变，#2-#5 全保**。
  后续 summary/CLAUDE 不得再写"§6.7 全保"，只能用本拆分口径）。
- **例外范围**：仅候选 E **受监督功能 demo**（AC-E12），执行前置 = 安全
  边界五条全满足：① 机械隔离、工作区无人、无负载，操作者全程在场且可
  立即切断总电源；② 新 unit 不设开机自动控制（权威 config
  `auto_enable = false`），每次上扭均显式 `SetEnabled(true)`；③ 先单臂
  低速/小幅再双臂（round 8 可判定数值：单臂探针 |target−anchor| ≤
  0.087 rad、轨迹最大斜率 ≤ 0.14 rad/s——既有 ±5°、0.25 Hz 真机验证
  量级；撤"低扭矩"宣称，无可 readback 的 torque/current 限制面），
  单侧异常立即 cancel 双侧并双发 EStop；④ 旧栈保留可恢复启动路径；
  ⑤ kill -9 后持续带扭不得描述成通过——仅残余风险证据。
- **不改 `00_design.md`**；`10_verification.md:100` / `:103` 两行 ⏳
  原样保留不销。

### 约束

- **D15 保持 OPEN 不得写成闭环**；AC-E25 <500ms 保持未完成，**不计入
  候选 E demo 完结 gate**。
- **D-BI3 正式退役推迟**：demo 期间旧 unit 可 `stop`，不得 disable/
  删除/破坏回滚（`v0.5_dimos_buyin_decision.md` r3 同步修订）。
- 收口宣称唯一口径 = **"机器人控制执行面受监督功能 demo 跑通"**；不得
  宣称生产替换 / 无人值守 / 硬件 fail-safe 完成。
- **无人值守生产化前必须补硬件 fail-safe 并撤销本例外**（post-E 硬件
  CodeMap → (b) 断电 watchdog / (c2) 独立监督进程再拍）。
- 同步收缩（同轮）：D14 四检测器（deadline/on_timeout/stall/overrun）
  移出候选 E，归档 post-E "生产安全硬化候选"，开工前须先解两 P0 前置
  阻断——① CtrlLostStop 已断扭后 heartbeat 恢复走 Pure 转移回
  `Enabled.Active` 不重新 `Enable(true)`（`state_machine.rs:621-636`，
  "状态 Active、硬件断扭"）；② deadline Fault 消费后 `ClearError` 可借
  `cmd_deadline_valid` guard（`state_machine.rs:202` +
  `ipc_tick.rs:456` 硬编码 true）再进 Active 而信号抑制又禁止旧命令重
  Fault——须锁"新鲜、未过期命令才重新武装并允许 Ready→Active"。
- 预留接口 = **仅文档级集成契约**（arm / heartbeat(kick) /
  request_safe(disarm) / status+readback；集成点 = handoff/guard 与
  允许 `SetEnabled(true)` 之间；缺失/失联/readback 失败 fail-closed）；
  不写假 no-op watchdog trait。
- **配套硬化（round 8）**：`auto_enable=false` 不消除崩溃残留扭矩
  （Feetech `connect()` 不写 `Torque_Enable=0`，`adapter.rs:475-514`）
  → per-arm guard **每次启动/restart 对本 bus 全量电机 ensure-safe**
  （左 bus1 ID 1-8 断扭；右 bus2 ID 1-6 断扭、7-9 先零速后断扭；全部
  readback；仍只碰本臂 bus）；demo **应急协议** = Enabled 期间任一 S0
  死亡/重启 → 立即人工切总电源并中止本轮（不得只依赖 EStop 弱 Ack——
  S0-dead 时 Ack 不证明事件已消费）；kill -9 带扭 ≥10s 观察移出 demo
  必经步（机械隔离 + 关自动 restart 的一次性 characterization，不计
  gate，已有证据可不重跑）。

### 影响面与审阅

- 影响面：候选 E Spec/Plan（round 7 范围收缩，执行序收敛
  (1)→(2)→(3)→(4)→(5)→(6)→(8)→(7)→(9)→(10-demo)）、
  `v0.5_dimos_buyin_decision.md` D-BI3（r3）、后续 summary / CLAUDE
  宣称口径统一"受监督功能 demo"。
- 审阅人：reviewer（E Plan round 7 复核意见原文拍定）；登记人：Claude。

---

## 2026-07-13 · v0.5 候选 E premise 变更 + dimos buy-in 决策单拍板（**状态：premise 变更登记 + 拍板登记**）

### 背景

reviewer 2026-07-10 两条口头输入改变候选 E premise：**(1)** dimos-deploy 为
reviewer 本人仓库、仅 demo 验证使用、后续不用、以当前为基础；**(2)** 系统目标 =
完全替换 dimos-deploy。原 workflow handoff §3 "dimos-deploy 团队 buy-in"框架
（外部团队）**消解**，三类输入改由
[`handoffs/v0.5_dimos_buyin_decision.md`](handoffs/v0.5_dimos_buyin_decision.md)
D-BI0~3 逐条拍板。决策单经 **r1**（reviewer 3 阻断：D-BI0 范围收窄 + 撤"10 RPC
够用"预设 / D-BI2 真机 gate + 退役顺序 / §5 同步范围五项）+ **r2**（reviewer 3
执行级：拍板表权威同步 / disable 措辞收紧 / 撤"gripper additive"预钉）两轮修正
后，2026-07-13 按 reviewer 建议值拍板。

### premise 变更登记

- **dimos-deploy 冻结**：master `9ece6bd` 冻结 = 只读移植参考源 + 功能基线；
  不收 PR / 不跑回归；**跨仓 0 PR 永久**（entry §5 禁止项 #6 边界由"E 重启
  之前"升为永久，已就地增强标注）。
- **候选 E 语义**："dimos integration（给 dimos-deploy 接 gateway）"→
  **替换机器人控制执行面**（原生移植；冻结基线 =
  `src/xlerobot_dimos/policy/executor.py:72` `execute_step()` 一圈：12 维
  action〔5/11 位为夹爪〕→ 双臂 trajectory〔traj_left_arm / traj_right_arm，
  各 5 臂关节〕→ 双夹爪独立下发）。**policy 推理 / 相机采集 / Web UI 不在
  E**，纳入须另行列单拍板。"完全替换"为系统级长期目标，非 E 单候选范围。
- **过时文档处理**：`v0.5_entry.md` 为活文档，沿其 §2.4 勘误惯例就地标注
  （§2.5 六 cell + §3.4 前置 #3 + §5 禁止项 #6 增强 / #4 核对）；
  `v0.4_dimos_research.md` §5 Python client 三选项**作废**（不改本体，本条
  登记）；workflow handoff §3.1/§3.4 buy-in premise 过时（不改本体，指回
  决策单）。

### 拍板登记（2026-07-13，reviewer 建议值）

| 决策 | 值 |
|---|---|
| D-BI0 | **确认**（r1/r2 修正后）：E = 替换机器人控制执行面 |
| D-BI1 | **(i)** E 不含 Python client（推迟至 robot-s2-bridge 真实需求，届时住 robot-system 仓内） |
| D-BI2 | **(i)** Rust cross-process e2e 自动 gate + **Orin 真机"读状态→双臂→双夹爪"完结 gate**（纯 Rust 不能替代真机验收） |
| D-BI3 | **确认**：维护归一 + 冻结声明 + **退役顺序锁**（真机完结 gate 通过后退役；验收前不执行 systemctl disable、不删除 unit、不做其他破坏性操作） |

### 影响面

- **候选 E 前置 #3 ✅ → 三前置齐（#1 B/C/D ✅ / #2 workflow handoff ✅ /
  #3 本条 ✅）→ E CodeMap Pre-Research 可开工**（实现仍走 RIPER 逐段 ACK）。
- gripper 缺口 = E CodeMap 必答：**比较复用 InvokeTask/Scripted vs 新增
  Gripper RPC，不预钉**；若新增 method/schema 仅能 additive（禁止项 #4）。
- `JointTrajectory → ScriptedPayload` adapter 归属 robot-system 侧（D-BI3），
  形态 = E CodeMap 定位题。LCM = 非目标（控制执行面替换下不接，随仓退役）。
- pure docs：0 code / 0 proto / 0 §6.7 / 0 dimos-deploy 改动 / 0 design.md。

### 审阅人

reviewer（r1 三阻断 + r2 三执行级修正逐条验收；"修完上述三点后即可按建议值
拍板并 push"，2026-07-13）。

## 2026-07-12 · 基建：iceoryx2 0.9.3 + 全依赖升级（**状态：实现登记 + 债务解除 ×3 + 偏离登记 + 教训修订**）

### 背景

iceoryx2 `=0.8.1` → `=0.9.3` + `cargo upgrade --incompatible` 全依赖升级。
commit 🔨 `fd58e67`（build + fix + remove 混提，body 分节 ①-⑤ 明文标注；
Cargo.lock 在各变更集间不可分割且每个中间态须独立过 fmt/clippy/test gate）
+ 🔨 `08dc594`（子 crate 版本上收 workspace 根单源，纯声明移动 lock 0 变化）。
业务调用面 0 改（0.9.0 九项 rename 级 breaking 本仓 grep 0 命中）。2026-06-24
"登记 4" 与候选 B PE3 约定的 iceoryx2-upgrade 复核清单本条**全部闭环**。

### 解除 1：toolchain pin `nightly-2026-06-08`（闭 2026-06-24 登记 4）

- 解除条件达成：iceoryx2 0.9.3 在 rustc 1.99.0-nightly (af3d95584 2026-07-09)
  全 workspace 绿，0.8.1 时代 E0599（iceoryx2-bb-container `StaticString` 无
  `as_str`）消失。`rust-toolchain.toml` 回 `channel = "nightly"`。
- 新 nightly 带 clippy `result_large_err` ×11：10 处 tonic OUT_DIR 生成代码走
  既有 module allow 列表 +1，1 处 gateway 测试 helper 针对性 allow（tonic
  签名固定，返回类型不可 Box）。

### 解除 2 + 3：`mem::forget(sub)` workaround ×2 撤销（闭候选 B PE3 + 候选 D PE4）

- 根因（iceoryx2 0.8.1 subscriber `Drop` 在 publisher 先退时访问已释放 shm →
  SIGSEGV）已被上游 #1629 修复（0.9.0 起）。
- 撤销两处：`state_pool.rs` subscriber 线程退出路径（候选 B PE3 登记的故意
  有界泄漏）+ `estop_e2e.rs` `EStopTopicCounter`（候选 D PE4 同根因）。
  estop_e2e 处的正常 drop 兼作"publisher 先退"回归探针。
- 复核实测：estop_e2e ×5 + state_rpc_e2e ×3 + 全 workspace 0 SIGSEGV。
- **robotctl `_exit` 跳 Drop 模式（第 3 锚点）不撤**：兼有短命 CLI 快退出的
  独立理由；是否回归 graceful Drop 另行决策（登记为 open，非本条范围）。

### 登记：robot-transport 并发 `open_or_create` 竞态有界重试（additive）

- 两进程同时首建同一 service 时输家瞬报 `ServiceInCorruptedState` /
  `HangsInCreation`（合成复现 iter 0 即中；0.8.1 / 0.9.3 双实测同在，**非
  0.9 回归**）。`build_service` / `build_rr_service` 加 10ms×50 有界重试；
  瞬态错穷举 match 分类（上游加 variant 编译期报警），耗尽按原错上抛带
  attempts。
- 新增 `robot-transport/tests/iox2_sub_first_repro.rs`：sub-first / pub-first
  / 裸 API / 30 轮并发竞态 4 条锁测 + 1 条 `#[ignore]` 诊断（spawn 真
  s1_daemon）。

### 登记：s1 events_drain terminal grace 修复（pubsub A1 千轮压测根因）

- **根因**：iceoryx2 只在 publish / `update_connections()` 时向新连 subscriber
  补发 history（上游 port 模块文档明文）；terminal 是最后一次 publish，之后
  连上的 gateway subscriber 永远收不到 terminal event。0.8.1 靠惰性 service
  清理意外兜底；0.9.3 及时清理后 pubsub_cleanup A1 在 ~116s 相位漂移处稳定
  复现超时（iter 849/858/862）。
- **修复**：`Iceoryx2Publisher::update_connections()` 透传（robot-transport
  additive API）+ events_drain terminal grace 100ms → 1s（稳态 registry ≈
  7-9，在 C1 金丝雀阈 `events_registry_len > 10` 预算内）+ 分片 20ms 可被
  stop 打断 + 每片调 `update_connections`。
- pubsub_cleanup_smoke --phase all 两轮 all PASS（0 conditional AC）。

### 登记：依赖白名单更新（tonic 0.14 拆分 + 兼容位）

- 延伸 2026-04-22 "workspace runtime 依赖白名单机制" + 2026-04-27 robot-rpc
  白名单：tonic 0.12 → 0.14 把 prost 集成剥离——robot-rpc **新增 runtime dep
  `tonic-prost`**（codec，替代 tonic 的 `prost` feature）+ build-dep
  `tonic-build` → **`tonic-prost-build`**（codegen）；tonic 需显式 `router`
  feature（`Server::add_service` 拆出）。scope 不变：仍仅 robot-rpc /
  robot-cli link tonic 家族。
- `TcpIncoming::from_listener` → `From<TcpListener>` + `with_nodelay` builder
  （语义保持 nodelay=true）；prost 0.13 → 0.14；ctor 0.10 → 1.0
  （`#[ctor(unsafe)]` 显式确认 pre-main 语义，行为不变，robot-logging）；
  arc-swap 1.7→1.9 / tempfile 3.10→3.27 / uuid 1.10→1.23 / trybuild
  →1.0.117。flatbuffers `=25.12.19` / iceoryx2 `=0.9.3` 已为最新。
- 版本单源：assert_no_alloc ×4 + trybuild ×1 子 crate 直写版本上收根
  `[workspace.dependencies]`（`08dc594`，纯声明移动）。

### 偏离登记：deny.toml licenses allow + `Zlib`（foldhash，build-dep only）

- **涉及**：deny.toml 许可证白名单（2026-04-22 白名单机制条的机械闸门）。
- **理由**：tonic-prost-build → prost-build → petgraph → hashbrown 0.15 →
  foldhash（Zlib）。build-dependency only，不进任何运行时 binary；Zlib 为
  OSI 宽松许可（zlib 系），与 Apache-2.0 兼容。
- **复审触发**：foldhash（或其他 Zlib crate）进入 runtime 依赖树时重审本条。
- deny 终态：未新增 violation；licenses FAILED 为既有 baseline
  （assert_no_alloc BSD-1-Clause + serialport MPL-2.0 + unescaper
  parse-error）。

### 教训修订（RS2 家族）：release rebuild 必须含 required-features smoke bins

- 候选 C RS2 写"release rebuild 必须先于 e2e gate"，本轮补一刀：robot-tests
  的 smoke bin 带 `required-features`，`cargo build --release --workspace`
  **不会**重建它们。升级后 4 个 smoke 曾因 6 月 8 日 0.8.1 陈旧二进制与
  0.9.3 daemon 混版本 IPC 假失败（0.8.1 node 无 node_monitor_context →
  0.9.3 侧判 `ServiceInCorruptedState`）；另 robotctl_smoke 缺省
  bin_dir=target/debug 同吃陈旧 debug robotctl。
- **修订后处方**：任何 smoke gate 前必须执行
  `cargo build --release -p robot-tests --features
  "pubsub-cleanup-smoke,scripted-backend-smoke,ik-backend-smoke,robotctl-smoke"`
  （robotctl_smoke 另需重建 debug robotctl）。

### 影响面 / Gate

- S0 daemon 0 改；S1 仅 events_drain terminal grace（行为参数，wire 0 改）；
  gateway 仅 TcpIncoming builder 迁移（行为等价）；B4/C/D frozen wire 全 0
  改；§6.7 五条全保。
- fmt/clippy 零警告（nightly 1.99.0）+ cargo test --workspace **1185 PASS /
  0 FAIL**（+4 竞态锁测）+ 6 smoke 全绿（crash 5/5 + pubsub all PASS ×2 +
  scripted 5/5 + ik 5/5 + robotctl 12/12 + clear_estop all）。

### 审阅人

reviewer（2026-07-12 session："iceoryx2 按两闸门开跑" + "按 (a) 先把 pubsub
A1 修掉再合入" + 依赖全量升级 + 版本上收规则逐条指令；`fd58e67` + `08dc594`
reviewer 本人 push origin/master）。

## 2026-06-24 · v0.5 候选 D EStop 触发 RPC（**状态：实现登记 + 勘误登记 + 债务登记**）

### 背景

候选 D 在候选 C 后的 9-method proto 上 **additive** 实现 design §5.3 已冻结但
未 land 的 `rpc EStop (EStopCommand) returns (Ack)`（method 9 → 10，
`EStopCommand{ CommandMeta meta; string estop_reason }`，operator+
`Capability::EStop`）。经 RIPER CodeMap(+amend) + Spec round 1 + Plan
outline+round 1 reviewer ACK + 2 段 feat + 1 段 docs land。**路由 (a)：gateway
publish `EStopEvent` 到 `/<robot>/events/estop` topic → S0 既有 `poll_estop`
消费链**（design §6.6 字面）——**S0 0 改，proto 形态无 design-freeze diff**。
封存详 [`docs/handoffs/v0.5_estop_summary.md`](handoffs/v0.5_estop_summary.md)
+ 审计全表 `team-swe/specs/v0_5_estop.md` §P.3/§P.4。

### 实现登记：第 10 method（additive，无 design-freeze diff）

- proto `EStop` 9 → 10 + `EStopCommand`；gateway 专用**常驻** `estop_pub`
  线程（`GATEWAY_ESTOP_SOURCE_ID=98`）+ 12 步精简 handler（**无 wire 回执步**——
  publish 非 RR）；`Method::EStop` + `CachedResponse::EStop{published_at_ns}`
  idempotency（第 6 method）；`robot-transport::Iceoryx2Subscriber::try_recv`
  inherent additive（PE1，e2e 逐条计数用，trait/mock 0 改）。
- **弱 Ack**（D2）：`Ack.cleared_at_ns = published_at_ns`，**明文不证** S0 已消费 /
  latch 已 fsync（`RobotState` 无 `estop_latched` 字段，latch fsync 远程不可观测）。
  S0 不在时 RPC 仍 Ack——`EStop RPC` **不是** S0 死亡场景救命索（该场景靠硬件
  fail-safe，§6.7 #1），AC-7 e2e 锁此诚实语义。
- `dry_run=true` → 拒 `InvalidArgument`（D4）；`source="rpc"` 固定 + `estop_reason`
  透传（空合法，D6）。

### 登记：`event.fbs:9` 注释澄清（comment-only，wire 0 改）

- `EStopEvent.latched` 字面 = "S0 已把 latch fsync 到磁盘"，但 producer 在
  publish 时刻**不可知** S0 是否已 fsync（publish 早于 S0 消费）。澄清注释明确：
  **producer 不可知时必须填 `false`**（语义 = "触发请求，未确认 latch"）。
- gateway RPC 路由（候选 D）即填 **`latched=false`**（Q8 (i)）——消费面实测无破坏：
  生产订阅者**只有** s0_daemon `poll_estop`，**不读** `.latched()`（只 parse 计数）；
  `.latched()` 读者全在测试侧。`//` 注释不进 flatc codegen，**wire 字段 0 改**。

### 债务登记：robotctl 直连 `latched=true` / `source="robotctl"`（既有，只登记不修）

- `robotctl.rs:351` 直连 publish 路径填 `latched=true`（publish 早于 S0 消费/fsync，
  同样不可知）+ `source="robotctl"`（**不在** event.fbs:13 "button/rpc/s1_detect"
  三枚举内）= **既有语义债**，先于候选 D 存在。
- 候选 D gateway 路由选**诚实** `latched=false` + `source="rpc"`，**不**沿用
  robotctl 现状；**robot-cli 0 改**（D7：robotctl `estop-rpc` 子命令推后，直连
  `estop` 已覆盖本地紧急）。robotctl 债务收敛为 follow-up，**不捆绑**本候选。

### 勘误：`docs/handoffs/v0.5_entry.md` §2.4 三处（实现期发现）

1. **schema_id 笔误**：entry:143 写 "fbs body 复用既有 EStop event schema（已
   frozen schema_id 0x0405）"——`0x0405` 系笔误（= `SCHEMA_GET_TASK_STATUS_RESPONSE`
   /B4，不属 EStopEvent）。正确 = **`0x0000_0300`**（`schema_id.rs:53-54`）。已就地
   校正 + 标 **勘误**。
2. **"对称 control plane" 表述**：entry:142/144 写 EStop 触发"沿 A1 ClearEStop
   同款 / 对称的 control plane"——与 D1 拍板**不符**。EStop 走 **route (a) gateway
   publish**（非 RR control plane）；安全方向（上锁，无 gate/确认）不必与危险方向
   （ClearEStop 解锁，需 gate+confirmation+强 Ack）对称。已就地校正 §2.4 两 cell
   的 control-plane 表述。

3. **触发角色（reviewer (3/3) 阻断，§P.4 PE5）**：entry:142 范围 cell 把 EStop 触发角色误写为 **maintainer+**，与本候选
   实现的 **operator+**
   （`Capability::EStop`；触发 operator+ / 清除 maintainer+ 的分权模型）冲突——
   属**安全/权限语义**（会误导候选 E 接班），**已就地校正** maintainer+ →
   operator+。

### 约束 / 影响面

- **S0 0 改**（route (a) 下 poll_estop → emergency drain → latch fsync 链全既有）；
  **§6.7 全保**（#3 EStop 不可清为失忆态**增强**：多一条 operator+ RPC 触发入口，
  清除仍唯一走 ClearEStop maintainer+）；**B4 frozen + 既有 9 method wire 0 改**；
  **ClearEStop/SetEnabled rr_server 0 改**；**S1 0 改**；**design.md 0 改**；
  **robot-cli 0 改**；**0 新依赖**。
- 影响面：`robot-rpc`（proto + gateway + estop.rs + estop_pub_thread.rs +
  idempotency）+ `robot-transport`（try_recv additive）+ `robot-msg/event.fbs`
  （comment-only）+ `robot-rpc/tests/estop_e2e.rs`（6 case）。
- Quality Gate：workspace **1180 PASS / 0 FAIL**（estop_e2e 6 + 既有全绿）+ 4 smoke
  + clear_estop_e2e 5/5 + robotctl_smoke 12/12（PE3）。`cargo deny check` 未新增
  violation，overall licenses FAILED 为既有 baseline（与候选 D 无关）。

### 审阅人

reviewer（用户）✓ —— RIPER 全程 ACK-gated（CodeMap `f52ccfe` / Spec `a278529` /
Plan `67b1eea` / feat (1/3) `4c8a6b3` + (2/3) `27cc80b`）。(2/3) reviewer 阻断
（`EStopTopicCounter` `!Send` subscriber Drop 撞 publisher-gone SIGSEGV → 加
`mem::forget(sub)`，PE4）闭环后 push `27cc80b`。

---

## 2026-06-09 · v0.5 候选 C SetEnabled（**状态：实现登记 + 偏离登记**）

### 背景

候选 C 在候选 B 后的 8-method proto 上 **additive** 实现 design §5.3
`SetEnabled`（method 8 → 9，`rpc SetEnabled (SetEnabledCommand) returns (Ack)`，
operator+）。经 RIPER CodeMap(+amend) + Spec round 1-4 + Plan round 1-5 reviewer
ACK + 5 段 feat + 1 Reverse Sync land。**D1 保留 frozen `bool enabled` 不加
hardware_id（范围 = 单 S0 daemon）→ proto 形态无 design-freeze diff**。封存详
[`docs/handoffs/v0.5_set_enabled_summary.md`](handoffs/v0.5_set_enabled_summary.md)
+ 审计全表 `team-swe/specs/v0_5_set_enabled.md` §P.4。

### 偏离 1：state machine 事件集 §1.4 12 种 → +`Event::Disable`（13 种）

- **涉及小节**：design §1.4 状态机事件表（12 种）。
- **偏离**：加 `Event::Disable` = `SetEnabled(false)`：`(Enabled, Disable)→Safe`
  （Effectful `Enable(false)` 真 torque-off，与 enable 上扭对称）/
  `(Ready, Disable)→Safe`（Pure）/ `(Safe/Fault, Disable)` no-op 幂等。**不**碰
  `estop_latched`（disable 不查不清 latch；清 latch 仍唯一走 `ClearEStop`，
  §6.7 #3 不变）。
- **Reverse Sync r1 收紧**（实施期 bug 修回已拍 Spec）：no-op 幂等的 live RR 形态
  = `state ∈ {Safe, Fault(_)}`（含 `Fault(AdapterErr)`）时 S0 **即时**回
  `Disabled`(Ok)，不动 adapter / 不清 Fault / 不建 pending；`Ok` 仅表"已非
  enabled"。
- **影响面**：robot-s0 state_machine / ipc_thread；S1 0 改；design.md 文字不改。

### 登记 2：config `auto_enable` typed 化 + `build_adapter` matched-section（B2/B6）

- `HardwareSection` 加 typed `auto_enable: Option<bool>`（additive，不再走
  untyped extras）；**缺省（None）= true** 保既有无条件启动 auto-enable 行为。
- `resolve_hardware_section(cfg, hardware_id)` 单一 helper：`build_adapter` 与
  启动 auto-enable 改用**同一** matched `[[hardware]]` 段（修既有 `.first()`
  错配）；CLI `--hardware-id` 无匹配段 → 启动 fail-fast exit 2。
- 单 hardware config（first == matched）0 行为变化；e2e
  `auto_enable_false_stays_safe`（双 hardware decoy-first）+
  `cli_hardware_id_no_match_fail_fast` 锁住。

### 登记 3：S0 第 2 个专用 RR server + gateway 第 6 条 rr_io_thread

- topic `/<robot_id>/control/set_enabled`（D5：**专用** rr_server，ClearEStop
  rr_server 0 改）；wire = `set_enabled.fbs`（schema id `0x040D`/`0x040E`，
  `SetEnabledStatus` 10-variant domain enum；`RpcStatus` A1 共享 enum **不扩**）。
- SetEnabled 是**多 tick** control action（区别 ClearEStop 单 tick）：D8 Ack ⟺
  观测到 `Enabled.Active`（enable，heartbeat 依赖）/ `Safe`（disable）；deadline
  过期不回滚副作用。dry_run = precondition-only（D7，进 S0 action layer 真评估，
  gateway 不短路）。

### 登记 4：toolchain pin `nightly-2026-06-08`（基建，非 design 语义）

- channel="nightly" 2026-06-09 滚到 rustc 1.98.0-nightly (beae78130) 后
  **iceoryx2 0.8.1 编译失败**（iceoryx2-bb-container `StaticString<256>` 无
  `as_str`，E0599；上游 dep 不可改）。按既定处方（pin 具体日期 + 原地回滚上一
  已验证日期）pin `nightly-2026-06-08`（rustc f20a92ec0）。
- **解除条件**：iceoryx2 升级适配新 nightly 后复核回 `channel = "nightly"`；与
  候选 B PE3（`state_pool` `mem::forget` workaround）同入 iceoryx2-upgrade
  复核清单。

## 2026-06-08 · v0.5 候选 B GetState/StreamState（**状态：偏离登记**）

### 背景

候选 B 在 B4 frozen 6-RPC schema 上 **additive** 实现 design §5.3 的两条**只读**
RPC（`GetState` / `StreamState`），method 6 → 8。经 RIPER Spec round 1-3 + Plan
round 1-2 reviewer ACK + 4 段 feat land。封存详
[`docs/handoffs/v0.5_get_stream_state_summary.md`](handoffs/v0.5_get_stream_state_summary.md)。

### 偏离 1：design §5.3 `GetState(Empty)` → `GetState(GetStateRequest{hardware_id})`

- **涉及小节**：design §5.3 frozen RPC schema（`rpc GetState (Empty) returns
  (RobotState);`）。
- **偏离**：实现期按**多 hardware 语义**把 `Empty` 改为**显式 hardware selector**
  `GetState(GetStateRequest{ string hardware_id = 1; })`（必填非空）。多 hardware
  下 `Empty` 歧义（返哪个 hardware？）；显式 selector 无歧义。
- **约束**：`docs/00_design.md` §5.3 **文字不改**（走"要改走 CHANGELOG"路径）；实现
  以本条 + `team-swe/specs/v0_5_get_stream_state.md` 为准。`StreamState` 同理
  （`StreamRequest{hardware_id, min_interval_ms}`）。
- **影响面**：proto additive（既有 6 method 0 改）；robotctl 0 改（候选 B 不加
  get-state/stream-state 子命令）。

### 偏离 2：gateway `--config <PATH>`（additive，D10）

- gateway 加**可选** `--config`（robot config TOML）→ minimal local 解析
  `[[hardware]].id` 建常驻 state 订阅池（GetState/StreamState 读源）。**缺省 = 空
  state 池**（非空 hardware_id 请求 `NotFound`，缺/空 `InvalidArgument`）→ **B4
  既有 clear-estop-only gateway 调用 0 破**。`[robot].id != --robot-id` 启动
  fail-fast exit 2。**不** dep robot-s0、**不**抽共享 crate（守 S0/S1 daemon 0 改）。

### 非安全保证延续（C1）

- `GetStateRequest` / `StreamRequest` **不含 auth 字段**；服务端 **不读不校验**任何
  metadata token（沿 TaskHandleRef read RPC 现状）。**无 token 与任意 token 结果
  完全一致**。**不得**宣称 read RPC 已鉴权；`read_state` cap（design §5.4）**本版
  不实现该安全属性**；后续 metadata auth 是独立候选 / round。

### follow-up（PE3）

- `crates/robot-rpc/src/state_pool.rs` subscriber 线程退出 `std::mem::forget(sub)`
  **故意有界泄漏** iceoryx2 subscriber handle（iceoryx2 0.8.1 subscriber `Drop`
  在 publisher 先退时访问已释放 shm SIGSEGV 的 workaround；每 config hardware 一个，
  非 per-RPC；线程仍 stop+join）。**iceoryx2 升级后复核是否撤销**。

### 不变量

§6.7 五条**全保**（GetState/StreamState 只读，0 改任何 safety state / latch /
EStop / output mode / fault）。B4 frozen 6-RPC wire schema 0 改。

### 审阅人

reviewer（RIPER：Spec round 1-3 + Plan round 1-2 + (1/4)-(4/4) 逐段 ACK；PE1/PE2/
PE3 逐条登记复核）。

---

## 2026-05-29 · v0.5 候选 A (1/5) robot-cli tonic client 依赖白名单更新（**状态：登记**）

### 背景

v0.5 候选 A operator CLI（`robotctl` 扩 7 个 tonic gRPC 子命令）经 Spec
round 1-3 + Plan round 1-2 reviewer ACK（AC-1 列定 7 dep）。(1/5)
脚手架 commit 给 `crates/robot-cli` 加 client-side RPC 依赖。本条登记
**超越** 2026-04-27 "A1-1 (3/3) robot-rpc / tonic / prost / uuid /
blake3 依赖白名单登记"中 "仅 robot-rpc crate link tonic/prost/tokio；
其它 crate（…robot-cli…）一律不允许直接 link" 的约束——该约束写于 v0.3
（robot-rpc 是唯一 RPC 参与者时），候选 A 显式让 robot-cli 成为
client-side consumer。

### Scope

- **robot-cli 新增直接 dep**：`robot-rpc`（`features = ["client"]`）+
  `tonic` + `tokio`（features `macros` / `rt-multi-thread` / `signal` /
  `io-util`）+ `flatbuffers` + `uuid` + `serde`（derive）+ `serde_json`。
- robot-cli 只作 **client 端 consumer**：import robot-rpc `client` feature
  的 `RobotControlClient` + 生成 proto 类型（`CommandMeta` / `AuthContext`
  / `InvokeTaskCommand` / `TaskHandleRef` 等）；**不**接 server /
  gateway / handler / dispatcher，**不**改任何 frozen FlatBuffers schema
  / proto / wire 契约 / `rpc_roles.toml`。
- **precedent**：`robot-tests` 已在 `client` / `*-smoke` feature 下 link
  tonic / tokio（A1-3 (3/3-a) `clear_estop_e2e` + B4 / 候选 1/2 smoke）；
  candidate A 把 robot-cli 纳入同类 client consumer。
- **生产 release `s0_daemon` / `s1_daemon` binary 不受影响**：robot-cli 是
  独立 binary（`robotctl`）；s0/s1 的 release 构建 0 robot_rpc / tonic /
  prost 符号边界不变（A1-1 已验证，本条不动 s0/s1）。

### 白名单更新后约束（candidate A 后生效）

- **允许 link tonic / prost / tokio 的 crate**：`robot-rpc`（server +
  client）+ `robot-tests`（client/smoke feature）+ **`robot-cli`
  （client feature；本条新增）**。
- 其它 crate（`robot-msg` / `robot-transport` / `robot-s0` / `robot-s1`
  / `robot-core` / `robot-adapters/*` / `robot-kinematics*` 等）**仍**
  一律不允许直接 link tonic / prost / tokio / 跨 crate 复用 RPC 类型。
- candidate A land 后任何"在上述允许清单之外的 crate link tonic/prost"
  的 PR 仍视为白名单违反，按 reviewer 复核流程否决。

### 不引入

- mTLS / OIDC / JWT / keychain token store（candidate A 接受 fake
  token；推 v0.5+ 真 auth 单独 outline）。
- `clap` —— robotctl 沿既有**手写** parser（Plan round 1 P1-2）；候选 A
  **不**引 clap。
- `whoami` / `rand` / `getrandom` 直接 dep（round 2 amend #2；`source`
  走 `std::env`，`idempotency_key` 复用 `uuid` 已生成的 request_id；
  `uuid` 的 `v7` feature 传递引入的 `getrandom` 是既有 workspace dep
  子树，非候选 A 新增直接依赖）。

### 影响面

- 不改 `docs/00_design.md` §4.2 robot-core 零 runtime 依赖原则。
- 不改 frozen FlatBuffers schema / proto / iceoryx2 服务 attribute 体系。
- `crates/robot-cli/Cargo.toml` 顶部注释跟本条登记互相引用。
- 既有 `robotctl` 3 个 iceoryx2 子命令（`status` / `watch` / `estop`）
  行为 0 改动（(1/5) parser 不扩新子命令；新名仍 → unknown subcommand
  exit 2）。

### 审阅人

reviewer（用户）— v0.5 候选 A Plan round 2 Approved（进入 (1/5) 实施）。

---

## 2026-05-27 · v0.4 候选 2 round 2 `robot-kinematics-xlerobot` impl + `IkBackend` 第三个真 action backend + S1 wiring + `ik_backend_smoke` cross-process e2e（**状态：偏离登记**）

### 背景

v0.4 候选 2 **round 1**（trait crate first release）完结后（详
2026-05-19 候选 2 round 1 §1.1；
[`docs/handoffs/v0.4_robot_kinematics_round1_summary.md`](handoffs/v0.4_robot_kinematics_round1_summary.md)），
按 round 1 §6 接班 10 步 checklist 开 **round 2** = impl crate +
`IkBackend` 第三个真 action backend + S1 coordinator solver wiring +
cross-process e2e（**独立** reviewer round + 独立 CodeMap / Spec /
Plan，沿 round 1 拍板节奏）。本日志记录 **round 2 完结**。

### 行为落地（round 2）

**3 docs/spec/plan 前置 + 4 实施 commit + 1 summary = 8 commit**
（详 [`docs/handoffs/v0.4_robot_kinematics_round2_summary.md`](handoffs/v0.4_robot_kinematics_round2_summary.md)
§2 commit chain）：

| commit | 段 | 内容 |
|---|---|---|
| `a42ef73` | docs | CodeMap：v0.4 候选 2 round 2 IK backend pre-research（含 Q-IK1..Q-IK15 + dimos `arm_ik.py` 移植对照 + XLeRobot 5-DOF 数学推导）|
| `7fd0756` | docs | Spec round 2 land：AC-1..AC-7 + Q-IK1..Q-IK17 reviewer 拍板表 + 14 路径 hard boundary（含 §"严禁改动" #1 round 1 trait crate 0 改动）|
| `266191f` | docs | Plan outline draft v1 + reviewer Plan round 1/2 amend 合并 land：P.0..P.4 + §P.3.2 12 拍板项 + §P.3.3 重型 smoke 拍板 (a) 5-phase full `--phase all` 硬 gate |
| `19a3f43` | feat | (1/4) `robot-kinematics-xlerobot` impl crate first release land |
| `ce2f498` | feat | (2/4) IK schema 注册链 + 三层 `ActionKind::Ik` atomic land |
| `13ecde8` | feat | (3/4) `IkBackend` + S1 coordinator solver wiring |
| `c0ef8e1` | feat | (4/4) `ik_backend_smoke` cross-process e2e |

**round 2 落地范围**（Q-IK1..Q-IK17 ACK 锁定）：

- **新 workspace member** `crates/robot-kinematics-xlerobot`（(1/4)
  `19a3f43`）：`Cargo.toml`（22 L）workspace 继承 + description 含
  AC-6 双写锚句 + 唯一 runtime dep `robot-kinematics = { workspace =
  true }`；`src/lib.rs`（763 L）= 5-DOF 闭式 IK 沿 dimos `arm_ik.py`
  + `Side { Right, Left }` + `XLeRobotArmIkSolver` struct + `ArmIkSolver`
  trait impl + 9 lib unit（含 `solve_yaw_nonzero_rejects` Q-IK6 (a)
  + `solve_out_of_reach_returns_ok_clamped` Q-IK16 (a)）
- **IK schema 注册链 6 步 atomic**（(2/4) `ce2f498`）：`SCHEMA_IK_
  PAYLOAD = 0x0000_040C` + `IkPayload` fbs body + `build.rs current_
  schemas [&str; 13] → [&str; 14]` + `BodyMarker` impl + `SizeTier::
  Control` 映射 + `build_helpers` IkPayload helper
- **三层 `ActionKind::Ik` enum append**（同 (2/4) atomic）：proto
  `ACTION_KIND_IK = 4` + fbs `ActionKindFb::Ik = 4` + lib `ActionKind
  ::Ik` + 三个 variant count 锁测同步 4 → 5
- **`IkBackend` + S1 coordinator solver wiring**（(3/4) `13ecde8`）：
  `crates/robot-s1/Cargo.toml` 新 `backend-ik` feature 进 default +
  `robot-kinematics` 永久 link（零 runtime dep）+ `robot-kinematics-
  xlerobot` optional gate；`ActionContext::solver: Option<&dyn
  ArmIkSolver>` 字段（Q-IK3 (a)）；`Coordinator` 加 `solver: Option<
  Box<dyn ArmIkSolver>>` 字段 + `Coordinator::new` 新增参数 4 处
  callsite；新 `validate_solver_for_profile` + `ProfileError::Missing
  Solver` variant（Q-IK10b (i)）；`crates/robot-s1/src/action_backend/
  ik.rs`（1066 L）= `IkBackend` 8 步 semantic check + `IkInstance`
  single-shot + `map_ik_error_to_start_error` 5 variant exhaustive +
  `inventory::submit!` + 17 lib unit；s1_daemon feature-gated
  `XLeRobotArmIkSolver::new(Side::Right)` 构造 + 注入
- **`ik_backend_smoke` cross-process e2e**（(4/4) `c0ef8e1`）：
  `configs/ik_backend_smoke.toml` 5-DOF XLeRobot 配置；`crates/robot-
  tests/Cargo.toml` 新 `[[bin]] ik_backend_smoke` + feature `ik-backend-
  smoke`；`crates/robot-tests/src/bin/ik_backend_smoke.rs`（1410 L）
  5-phase 跨进程 e2e（selftest / happy Q-IK17 (a) single-shot /
  monotonic B3 §3.7 / unreachable_clamp Q-IK16 (a) / invalid_input
  iva yaw≠0 + ivb garbage payload）；**reviewer 拍板"smoke 不抽公共
  helper"** —— 沿 candidate 1 / 3 同款 inline 复制（不动既有 smoke
  binary）

### Quality Gate 实测

- `cargo fmt --all --check` clean
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo clippy -p robot-tests --bin ik_backend_smoke --features
  ik-backend-smoke -- -D warnings` clean
- `cargo build --workspace --release` 10.90s（incremental；首次
  23.13s）
- `cargo build --release -p robot-tests --bin ik_backend_smoke
  --features ik-backend-smoke` 19.78s
- `cargo run -p robot-msg --bin schema_lint` 15 .fbs 全过（14 active
  + 1 v0 deprecated；本轮新加 `ik_payload.fbs`）
- `cargo test --workspace` **989 PASS / 0 FAIL**（既有 940+ baseline
  + 候选 1 + 候选 3 + round 1 + round 2 新增；本 round 2 净加 30
  test：17 IkBackend lib unit + 4 validate solver + 9 xlerobot lib
  unit）
- `crash_restart_smoke --iterations 5`（real-s1 硬 gate）**5/5 PASS**
- `crash_restart_smoke --use-mock-s1 --iterations 5` **5/5 PASS**
  （informational only；沿 round 1 §7 diff #2 同款，本 round 2
  实测不退化）
- `scripted_backend_smoke --phase all`（候选 1 regression）**5/5
  PASS**
- `pubsub_cleanup_smoke --phase all`（候选 3 regression）**all PASS
  + `triggered_c3 = false`**
- **`ik_backend_smoke --phase all`（本 round 2 硬 gate）5/5 PASS**
- `cargo tree -p robot-kinematics-xlerobot --edges normal,no-proc-
  macro`：唯一 normal dep `robot-kinematics`（trait crate）；trait
  crate 唯一 dep `thiserror v2.0.18`（沿 round 1 hard boundary）
- `cargo deny check`：**未新增 violation；overall fail 为既有
  baseline**（`assert_no_alloc` `BSD-1-Clause` + `serialport` `MPL-
  2.0` 两条 `error[rejected]`；`unescaper` `GPL-3.0/MIT` 一条
  `warning[parse-error]`；3 处全部跟 round 2 本轮改动无关；reviewer
  ACK 路径 (a) follow-up 独立处理；详 round 2 summary §4.1 + §7 diff
  #1）

### 关键契约（round 2 引入 / 维护的硬约束）

详 round 2 summary §3：

1. **round 1 trait crate `crates/robot-kinematics/` 0 改动**（Spec
   §"严禁改动" #1；`git diff round-1-end..(4/4) -- crates/robot-
   kinematics/` empty diff）
2. **`IkBackend::start()` 8 步顺序 semantic check**：payload_schema
   _id / solver injected / deadline pre-guard / fbs decode / 字段长
   度 / hardware joint count / solver.solve(None) / deadline post-
   guard；短路顺序锁定语义不拆 helper
3. **`ActionStartError` 3 类映射严格分离**：`PayloadDecode` (fbs verify
   fail) / `HardwareMismatch` (solver joint_count != hardware.joints.
   len()) / `Rejected` (其它全部含 5 个 `IkError` variant)；全部
   → tonic `Code::FailedPrecondition`（沿 wire.rs:340 B4 既有路径）
4. **`source_task_id == ctx.command_seq` bit-equivalent 锁**（B3 §3.7
   + 候选 1 scripted 同款）：lib unit `tick_emits_command_with_source
   _task_id_eq_ctx_command_seq` body bytes 全字节级 `assert_eq!` +
   cross-process smoke `monotonic` phase 27+ unique seq 严格单调
5. **Q-IK10 三层 fail-fast**：(a-i) `validate_profile_section`
   `UnknownBackend`（feature off + profile 启用）/ (b-i) `validate_
   solver_for_profile` `MissingSolver`（feature on + profile 启用 +
   solver 漏注入）/ (a-ii) `BackendRegistry::dispatch` `NoBackend
   ForKind`（feature on + solver 注入 OK + profile 不含 `"ik"` 但客
   户端 InvokeTask kind=Ik）
6. **Q-IK16 (a) clamp 路径不预钉 trait crate 语义**：XLeRobot impl
   选路径 A 走 `Ok(clamped)`（沿 dimos `arm_ik.py`）；trait crate
   AC-6 四处双写防漂移沿 round 1 不动；cross-process smoke `unreach
   able_clamp` phase 终态 = Completed（**不**是 Failed）
7. **deadline 单一权威 = backend pre/post guard**：solver 收 `None`
   永不返 `IkError::DeadlineExceeded`（Plan §P.3.2 #12 (a)）；3 处
   独立 check（`ActionRequest::validate` + IkBackend pre-guard +
   IkBackend post-guard）
8. **frozen schema / B4 wire 契约不动**：`action_status.fbs` /
   `clear_estop.fbs` 0 改动；`action_request.fbs` 仅 `ActionKindFb`
   enum **append** `Ik = 4`（enum append 是 B4 outline §6 允许的兼
   容路径）；gateway 5 task method wire 字段 0 改动；**0 新增 IK 专
   用 byte cap**（Plan §P.3.2 #7 沿 Control tier 8 KiB wire gate 2
   兜底）

### §6.7 5 不变量

**0** 降级 / **0** 升级 / **0** 偏移：

| # | 不变量 | round 2 改动路径 |
|---|---|---|
| 1 | S0 存活 = 软件层安全充分条件 | 0 触（IK 在 S1，不进 S0/adapter） |
| 2 | systemd 是最外层看门狗 | 0 触 |
| 3 | EStop 不可被软件清除为失忆态 | 0 触 |
| 4 | S0 重启不失忆 | 0 触 |
| 5 | 幂等写 RPC | 0 触（沿 B4 既有 idempotency 路径） |

round 2 净改动 16 文件；全部落在 S1 coordinator / backend / wire enum
/ IK 专用 fbs body / S1 daemon binary / smoke binary / config TOML /
Cargo.toml + Cargo.lock；不进 S0 / adapter / EStop / fsync 双写 /
idempotency cache 路径。

### Plan-Execute diff 5 项

详 round 2 summary §7：

1. **`cargo deny check` overall FAIL = 既有 baseline 不退化** 沿
   round 1 §7 diff #1 同款；本 round 2 **0** 新增 license violation；
   3 处 baseline reject / parse warning 跟 round 2 本轮改动无关；
   reviewer ACK 路径 (a) follow-up 独立处理；措辞约定**严禁** 写
   "`cargo deny check` 过"
2. **mock-s1 `crash_restart_smoke --use-mock-s1` 实测 5/5 PASS** 沿
   round 1 §7 diff #2 同款 informational only；real-s1 路径仍硬 gate
   （5/5 PASS）；本 round 2 不退化反向不退化
3. **`Coordinator::new` 签名加 `solver` 参数 4 处 callsite 同步更新**
   （s1_daemon + action_dispatch.rs × 2 + rr_server/handler.rs +
   coordinator.rs tests）；hard boundary §"严禁改动" #1 round 1 trait
   crate 0 改动守住
4. **28 处 `ActionContext { ... }` 测试 site 补 `solver: None` 字段**
   （hold_pose / mock_dwell / scripted 三个既有 backend test 文件）
   + 4 处 `Coordinator::new(..., None)` 既有 backend 行为 **0** 改
   动（`solver` 字段不读 → 不感知）
5. **(4/4) `happy` / `unreachable_clamp` phase `progress_count == 0`
   触发 → informational only** 沿候选 1 `Started` 是 informational
   同款路径；single-shot + per-ActionId pubsub latest-sample 可能
   skip 早期非终态事件；bit-level `progress = 1.0` 由 ik.rs lib unit
   严格锁住

### 下一候选触发条件

候选 2 round 2 完结后下一步走 **v0.4 候选 4 (dimos 切换 light
Research)** 或 **v0.5 入口**（待 v0.4_entry §3.1 reviewer 拍板）。
触发：本 round 2 summary push（本 commit）+ reviewer 候选 4 entry
ACK。**不**捆绑本 round 2 trait crate / impl crate 改动；不改候选
1/3 既有 smoke binary 行为（本 round 2 (4/4) 已守住此边界）。

### 影响面

- 新 workspace member `crates/robot-kinematics-xlerobot` workspace
  lint 基线沿用（`[lints] workspace = true`）；后续如有其它机器人
  impl（`robot-kinematics-piper` 等）沿同款模板
- `Coordinator::new` 签名加 `solver` 参数 = breaking change（仅 lib
  内部；workspace 外无 dep）；4 处 callsite 同步更新 + 测试 site 28
  处 `solver: None` 字段已闭环；未来 v0.5+ 如继续扩 `Coordinator::
  new` 参数沿同款"运行时所需 → 可选"参数顺序原则
- `ActionContext::solver: Option<&dyn ArmIkSolver>` 字段为 `Action
  Context` struct 的不向后兼容扩展（既有 3 个 backend 不读，行为不
  变；未来扩 backend `IkBackend` / 后续机器人 backend 可读取）
- AC-Q `cargo deny check` 措辞约定沿 round 1 + 候选 1 同款：**严禁**
  写"过"；统一写"未新增 violation；overall fail 为既有 baseline；
  follow-up 独立处理"
- AC-Q mock-s1 `crash_restart_smoke` 沿 round 1 + 候选 1 同款 baseline
  regression 表述；本轮单次正 reversal **不**改写 Spec AC-Q

### 审阅人

reviewer ACK Plan round 1+2 + (1/4)..(4/4) 全部 ACK + summary 待 ACK
（本 commit）。

---

## 2026-05-19 · v0.4 候选 2 round 1 `robot-kinematics` `ArmIkSolver` trait crate first release（**状态：偏离登记**）

### 背景

v0.4 候选 1 (Scripted backend) 完结后（详 2026-05-14 候选 1 §1.1；
[`docs/handoffs/v0.4_scripted_backend_summary.md`](handoffs/v0.4_scripted_backend_summary.md)），
按 v0.4_entry §3.1 优先级 3 → 1 → 2 → 4 排序开候选 2（robot-
kinematics + IK trait）。v0.4_entry §3.3 reviewer 拍板候选 2 **拆两
轮 reviewer round**：**round 1** = trait crate first release（trait +
纯类型，零 runtime 依赖；零 robot-s1 改动）；**round 2** =
`robot-kinematics-xlerobot` impl + `IkBackend` 第三个真 action backend
+ S1 wiring + cross-process e2e（**独立** reviewer round + 独立
CodeMap / Spec / Plan）。本日志记录 **round 1 完结**。

### 行为落地（round 1）

**3 docs/spec/plan 前置 + 1 实施 commit + 1 summary = 5 commit**
（详 [`docs/handoffs/v0.4_robot_kinematics_round1_summary.md`](handoffs/v0.4_robot_kinematics_round1_summary.md)
§2 commit chain）：

| commit | 段 | 内容 |
|---|---|---|
| `557e354` | docs | CodeMap：v0.4 候选 2 robot-kinematics pre-research（8 段 + Q1-Q20 待拍板表）|
| `64cf312` | docs | Spec round 1 land：AC-1..AC-7 + AC-Q + Q1-Q10 reviewer 拍板表 + 14 路径 hard boundary + reviewer 审查重点 4 条 design-in 双写 |
| `98b6033` | docs | Plan outline draft v1 + reviewer Plan round 2/3 amend 合并 land：P.0..P.4 + §P.3 7 拍板项 + §P.3.3a 重型 smoke 拍板 #3 (b) 5 条命令 |
| `6caf0ee` | feat | (1/1) `robot-kinematics` `ArmIkSolver` trait crate first release land |

**round 1 落地范围**（Q1-Q10 ACK 锁定）：

- **新 workspace member** `crates/robot-kinematics`：`Cargo.toml`
  workspace `members` 按字母序 append（`robot-health` 之后；
  `Cargo.toml:25` "未来 crate" 注释段 `robot-kinematics` 行同 commit
  清理）+ `Cargo.lock` 新增 `[[package]] name = "robot-kinematics"`
  stanza
- **`crates/robot-kinematics/Cargo.toml`**（23 L）：`[package]`
  workspace 继承 + `description` 含 AC-6 双写锚句 "trait 层不预钉
  XLeRobot clamp 行为（AC-6）" + `[dependencies] thiserror = {
  workspace = true }` 唯一 runtime dep + `[lints] workspace = true`
- **`crates/robot-kinematics/src/lib.rs`**（313 L 单文件）：5 共享
  类型（`Pose` / `Twist` / `Jacobian` / `KinematicInfo` /
  `IkSolution`；自带 POD 零 nalgebra）+ `JointLimit` POD（嵌套于
  `KinematicInfo::joint_limits: Vec<JointLimit>`）+ `IkError`
  5 variant
  （`InvalidInput { reason }` + `NoSolution` + `OutOfReach
  { distance_m }` + `SingularityTooClose { condition_number }` +
  `DeadlineExceeded`；`thiserror::Error` derive；exhaustive match
  锁）+ `ArmIkSolver: Send + Sync` trait（`info()` + `solve(target:
  &Pose, seed: Option<&[f64]>, deadline_ns: Option<u64>)`；严格
  dyn-safe；多臂靠 `Box<dyn ArmIkSolver>` 多 instance）+ 4 lib unit
  （exhaustive + POD constructable + `assert_dyn_safe::<dyn
  ArmIkSolver>()` 编译期 monomorphize + trait object callable stub）

### Quality Gate 实测

- `cargo fmt --all --check` clean
- `cargo clippy --workspace --all-targets -- -D warnings` 双路（含 /
  不含 `robot-rpc/client`）clean
- `cargo build --workspace --release` 全过（既有 12 crate + 新
  `robot-kinematics`）
- `cargo build -p robot-kinematics --release` 1.07s
- `cargo test -p robot-kinematics --lib` 4 PASS / 0 FAIL
- `cargo test --workspace --tests --features "robot-rpc/client" --
  --test-threads=1` **963 PASS / 0 FAIL**（既有 940+ baseline + 4
  new robot-kinematics lib unit + 19 prior delta）
- `cargo tree -p robot-kinematics --edges normal,no-proc-macro`：
  直接 dep == `{thiserror v2.0.18}`
- `cargo doc -p robot-kinematics --no-deps` clean，**0** warning
- `cargo run -p robot-msg --bin schema_lint` 过（**0** schema 改动；
  14 .fbs 沿用既有锁测）
- `crash_restart_smoke --iterations 5`（real-s1 硬 gate）**5/5
  PASS** ✅
- `crash_restart_smoke --use-mock-s1 --iterations 5` **5/5 PASS**
  （informational only；实测优于 Spec AC-Q baseline regression 预
  期；**不**升级为新硬 gate；详 round 1 summary §7 diff #2）
- §P.3.3a (i)-(v) 5 命令（拍板 #3 (b) compile gate + selftest only）
  全过：pubsub_cleanup_smoke + scripted_backend_smoke 双 compile
  gate + 双 selftest PASS + per-bin `cargo test --bin` 6 + 0 PASS
- `cargo deny check`：**未新增 violation**；overall `licenses
  FAILED` 是**既有 baseline**（`assert_no_alloc` `BSD-1-Clause` /
  `serialport` `MPL-2.0` / `unescaper` 标识符 warning；全部跟
  `robot-kinematics` round 1 无关；baseline 已在 HEAD = `98b6033`
  存在；reviewer ACK 路径 (a) follow-up 独立处理；详 round 1
  summary §4.3 + §7 diff #1）

### 关键契约（round 1 引入 / 维护的硬约束）

详 round 1 summary §3：

1. **trait dyn-safe**：方法签名不含 generic param / 不返 `Self`；
   `assert_dyn_safe::<dyn ArmIkSolver>()` 编译期 monomorphize 锁；
   违反编译 fail
2. **`IkError` exhaustive match**：5 variant `match` 漏 variant 编
   译 fail（沿候选 1 `action_kind_three_v04_variants` 同款）
3. **不预钉 `XLeRobot` clamp 行为**：`IkSolution` 最小字段（不含
   `clamped: bool` / `iterations` / `residual_error` metadata）+
   `IkError::OutOfReach` trait 层可选返回；四处双写防漂移
   （`Cargo.toml` `description` + lib.rs 模块 doc + trait doc +
   Spec AC-6）
4. **`deadline_ns: Option<u64>`**：跟 `robot-core::time::MonoRawNs
   (u64)` + `Deadline(MonoRawNs)` + `ActionRequest.deadline_ns: u64`
   仓库统一时间模型对齐；trait crate **不** dep `robot-core`，仅值
   语义对齐避免 signed conversion 损失精度
5. **零 runtime 依赖**：仅 `thiserror`；trait crate **永远不**扩
   `[dependencies]`；新 dep 走独立 reviewer round
6. **零 `robot-s1` 改动 + 零 wire schema 改动**：`git diff
   98b6033..6caf0ee --name-only` 仅 4 文件（`Cargo.toml` /
   `Cargo.lock` / `crates/robot-kinematics/Cargo.toml` /
   `crates/robot-kinematics/src/lib.rs`）；`crates/robot-s1/**` /
   `robot-rpc/**` / `robot-msg/**` / `robot-s0/**` / `robot-adapters/
   **` / `robot-tests/**` / `configs/**` / `docs/00_design.md`
   全部 **0** 触

### §6.7 5 不变量

**0** 降级 / **0** 升级 / **0** 偏移（trait crate first release 零
runtime 代码路径 + 零 wire schema + 零 s0/s1 改动）。

### round 2 触发条件

候选 2 **round 2** 范围（`robot-kinematics-xlerobot` impl + `IkBackend`
+ S1 wiring + `ik_backend_smoke` cross-process e2e）必须走**独立
reviewer round + 独立 CodeMap / Spec / Plan**，**不**直接开实现。
触发：本 round 1 summary push（本 commit）+ reviewer round 2 entry
ACK。详 round 1 summary §6 接班 10 步 checklist。

### 影响面

- 新 workspace member `crates/robot-kinematics` workspace lint
  基线沿用（`[lints] workspace = true`）；后续 round 2
  `robot-kinematics-xlerobot` impl crate 沿同款模板
- AC-Q `cargo deny check` 措辞约定：**严禁**写"过"；统一写"未新增
  violation；overall fail = 既有 baseline；follow-up 独立处理"
- AC-Q mock-s1 `crash_restart_smoke` 沿候选 1 同款 baseline
  regression 表述（与 (1/3) `d8ba207` 实测 0/5 FAIL 锚点对齐）；本
  轮单次正 reversal **不**改写 Spec AC-Q

### 审阅人

reviewer ACK Plan round 1+2+3 + (1/1) ACK + summary 待 ACK
（本 commit）。

---

## 2026-05-14 · v0.4 候选 1 第二个真 action backend `ScriptedBackend` + 新 fbs body `ScriptedPayload` (schema_id 0x040B) + gateway 两道 byte gate（**状态：偏离登记**）

### 背景

v0.4 候选 3 (per-ActionId pubsub cleanup + AC-C1 reap_finished) 完结
后（详 2026-05-13 候选 3 §1.1；
[`docs/handoffs/v0.4_pubsub_cleanup_summary.md`](handoffs/v0.4_pubsub_cleanup_summary.md)），
按 v0.4_entry §3.1 优先级 3 → 1 → 2 → 4 排序开候选 1：第二个真
action backend = Scripted（joint-space 线性插值 + waypoint dwell；
reviewer v0.4_entry §3.2 拍板插值策略；不选直接跳点 / 不选 cubic）。
B3 `HoldPoseBackend` 是第一个真 backend，本轮加 Scripted 是第二个。

### 行为落地

**6 docs/spec/plan 前置 + 3 实施 commit + 1 summary = 10 commit**（详
[`docs/handoffs/v0.4_scripted_backend_summary.md`](handoffs/v0.4_scripted_backend_summary.md)
§2 commit chain）：

| commit | 段 | 内容 |
|---|---|---|
| `9d91260` | docs | CodeMap：v0.4 候选 1 scripted backend pre-research |
| `5efe987` | docs | Spec round 1+2 amend：影响范围 / 风险 / AC-1..AC-Q / Q1-Q17 reviewer 拍板表 |
| `737fde6` | docs | Plan outline draft v1（commit 节奏 + P.0..P.4） |
| `eca84e4` | docs | Plan round 1 amend（5 阻断 + 2 非阻断） |
| `910c8a4` | docs | Plan round 2 amend（3 阻断 + 1 非阻断：两层 error / `anchor_bias` snapshot / `BackendRegistry::dispatch` 路径 / Q6 only byte cap） |
| `8c39f9b` | docs | Plan round 3 amend（Q6 count-check 表述清零；2 处反弹清理） |
| `d8ba207` | 候选 1 (1/3) | **schema 注册链 6 步 atomic land** + **三层 ActionKind append** + 下游映射 + variant count 锁测（atomic 一个 commit；reviewer Plan round 1 阻断 1） |
| `5c07206` | 候选 1 (2/3) | **`ScriptedBackend` lib impl** + 13 lib unit + **gateway 两道 byte gate** (`MAX_SCRIPTED_PAYLOAD_BYTES` + `CONTROL_TIER_MAX_BYTES` 单源绑定) + 3 lib unit + `backend-scripted` Cargo feature 默认 on (Q17(a)) |
| `70dcb25` | 候选 1 (3/3) | **`scripted_backend_smoke` feature-gated binary** + 5 phase cross-process e2e (selftest / happy_multi_waypoint AC-4 / source_task_id_monotonic AC-5 / cancel_mid AC-6 终态 Cancelled only / payload_boundary AC-7 4 子 case) + `configs/scripted_backend_smoke.toml` 新 + `S1Section.profile` Option<toml::Value> Phase B2 (3/3) 潜伏 bug 修复 |

### schema 注册链（robot-msg 5 必改 + 1 可选）

1. `crates/robot-msg/schemas/scripted_payload.fbs`（新；`robot.msg`
   namespace；Q1(a) 最小集字段 = `waypoints[{ positions: [double],
   dwell_ms: uint, segment_duration_ms: uint }]`）
2. `crates/robot-msg/build.rs:57` `current_schemas: [&str; 12]` →
   `[&str; 13]` + append `"scripted_payload.fbs"`
3. `crates/robot-msg/src/schema_id.rs` 加 `pub const
   SCHEMA_SCRIPTED_PAYLOAD: u32 = 0x0000_040B;` + `SCHEMA_REGISTRY`
   升序数组 append `(SCHEMA_SCRIPTED_PAYLOAD, "robot.msg.ScriptedPayload")`
4. `crates/robot-msg/src/markers.rs` 加 `pub struct ScriptedPayload;`
   + `impl BodyMarker`（`SCHEMA_NAME = "robot.msg.ScriptedPayload"` +
   `BLOB_EMBEDDED = false` + `type Root<'a> = msg::ScriptedPayload<'a>`
   + 默认 `read_blob_desc`；沿 `JointState` / `InvokeTaskRequest`
   同款风格）
5. `crates/robot-msg/src/size_tier.rs:62-95` `default_tier_of` arm
   加 `SCHEMA_SCRIPTED_PAYLOAD => SizeTier::Control`（payload 落
   8 KiB Control tier；穷举 test `every_registered_schema_has_a_tier`
   自动覆盖）
6. （可选）`crates/robot-msg/src/build_helpers.rs`：Q5(a) 拍板**不**
   抽公共 builder，smoke / backend 各自 inline 用 `FlatBufferBuilder`

### 三层 ActionKind enum append + 下游映射（atomic 同 commit）

- `crates/robot-rpc/proto/robot_control.proto:119` `ActionKindProto`
  enum append `ACTION_KIND_SCRIPTED = 3`（prost 生成名
  `ActionKindScripted`）
- `crates/robot-msg/schemas/action_request.fbs:38` `ActionKindFb` enum
  append `Scripted = 3`
- `crates/robot-s1/src/action.rs:148` `ActionKind` enum append
  `Scripted` variant
- `crates/robot-rpc/src/task_handlers.rs` `action_kind_proto_to_fb`
  arm 加 `ActionKindScripted => ActionKindFb::Scripted`
- `crates/robot-s1/src/rr_server/handler.rs:174-193`
  `ActionKindFb::Scripted => ActionKind::Scripted` arm（other catch-
  all 保留 forward-compat）
- 3 variant count 锁测（exhaustive + ENUM_VALUES HashSet equality +
  no-backend 路径 NoBackendForKind → Unimplemented）

### `ScriptedBackend` lib（`crates/robot-s1/src/action_backend/scripted.rs` 新）

- 零字段 struct + `inventory::submit!`；`name = "scripted"` +
  `supported_kinds = &[ActionKind::Scripted]` + `supported_schemas =
  &[SCHEMA_SCRIPTED_PAYLOAD]`（**不**含 `NO_PAYLOAD_SCHEMA_ID`）
- `start()` 返**裸** `ActionStartError`（reviewer Plan round 2 阻断 1
  拍板两层澄清：lib 返裸，coordinator `submit()` 通过
  `SubmitError::BackendStartFailed(#[from] ActionStartError)` 转）；
  四项 semantic check：payload_schema_id 匹配 / `flatbuffers::root`
  verify / waypoints 非空 / 每段 positions.len == hardware.joints.len
  + segment_duration_ms > 0
- `ScriptedInstance`：cancelled flag + `decoded_waypoints` (owned) +
  `initial_pose` snapshot (`ctx.anchor_bias.to_vec()`；reviewer Plan
  round 2 阻断 2 拍板 segment 0 from 来源) + `current_idx` +
  `Phase { Interp | Dwell }` + `phase_started` + `tick_buf` 复用
  (Q7)
- tick() phase 状态机关键 invariant（详 summary §3.2）：
  * cancelled 短路返 `Completed(PartialSuccess { warning:
    "cancelled_at_waypoint_{idx}" })` (Q4(a))
  * Interp 完 → 切 Dwell with `start_ns = original_interp_start +
    segment_duration_ns`（用 Interp 自然完成时刻，**不**用 `now_ns`；
    大步推 clock 时 Dwell elapsed 自然累计 → 下一 tick 立即看到
    Dwell 超时就 advance）
  * 复用 B1 `build_hold_command(ctx.config, ctx.layout_id, &tick_buf,
    ctx.command_seq, ctx.clock.now().0)` 写 body；`source_task_id ==
    ctx.command_seq` 沿 B3 §3.7 单调契约（**禁** backend 私有 counter）

### gateway 两道 byte gate（`crates/robot-rpc/src/task_handlers.rs`）

- `MAX_SCRIPTED_PAYLOAD_BYTES = 4 * 1024`（Q6 拍死 only byte cap）
- `CONTROL_TIER_MAX_BYTES = robot_msg::size_tier::SizeTier::Control.
  max_bytes()`（**单源**绑定；**严禁**硬编码 8192；reviewer Plan
  round 2 阻断 1 锁定）
- 编译期 `const _GATE_BOUNDARY_CONST` 断言 Gate 1 < Gate 2
- Gate 1 (step 1.5)：`kind == Scripted && payload.len() >
  MAX_SCRIPTED_PAYLOAD_BYTES → tonic::Code::InvalidArgument`
- Gate 2 (step 8.5)：`wire_bytes.len() > CONTROL_TIER_MAX_BYTES →
  drop insert_handle + InvalidArgument`（兜底外层 CommandMeta +
  FlatBuffers overhead 撞 Control 8 KiB）
- 新 `TaskValidationError` variants：`ScriptedPayloadTooLarge { len,
  max }` + `InvokeWireBytesTooLarge { len, max }`（均映
  `InvalidArgument`）

### Cargo feature wiring

- `crates/robot-s1/Cargo.toml`：`backend-scripted = []`；`default =
  ["backend-mock-dwell", "backend-hold-pose", "backend-scripted"]`
  (Q17(a))
- `crates/robot-s1/src/action_backend/mod.rs` 加 `#[cfg(feature =
  "backend-scripted")] pub mod scripted;`
- `crates/robot-tests/Cargo.toml`：加 `[[bin]] scripted_backend_smoke`
  + `required-features = ["scripted-backend-smoke"]`；
  `scripted-backend-smoke = ["dep:robot-rpc", "dep:tonic", "dep:tokio"]`
  （沿 `pubsub-cleanup-smoke` / `clear-estop-e2e` 同款）

### Plan-Execute diff 3 项（详 summary §7）

- **#1**：`crash_restart_smoke --use-mock-s1 --iterations 5` 0/5
  FAIL 既有 baseline regression（在 `8c39f9b` 候选 1 (1/3) 之前的
  `cac4515` Phase B 完结 commit 上同款 0/5 FAIL）→ 留 follow-up，**不**
  阻塞候选 1；real-s1 路径 5/5 PASS 沿候选 3 同款回归门槛保留
- **#2**：`S1Section.profile: Option<toml::Value>` 字段（Phase B2
  (3/3) 以来潜伏 bug：`deny_unknown_fields` 让 `[s1.profile]` 段存
  在时 `parse_s1_section_from_toml` 报 unknown field；现有 Phase B
  配置都没用过 `[s1.profile]` → bug 未触发；候选 1 (3/3) smoke 必须真
  启用 scripted backend → 触发并修复；S1 parser **不**消费 profile 内
  容仅放行）
- **#3**：AC-4 Started event 改 informational only（per-ActionId
  pubsub latest-sample 模型 client open `StreamTaskEvents` 后才订
  阅 → 早期 Started 可能被 Progress / 终态覆盖）+ AC-7 (7c-ii)
  cross-process 不重复（由 (2/3) lib unit `invoke_task_wire_over_
  control_tier_returns_invalid_argument` 端到端覆盖；§P.4 预期
  diff #4 命中）

### 验收证据封存（详 summary §4）

- `cargo fmt --all --check` ✅
- `cargo clippy --workspace --all-targets -- -D warnings` 双路 clean
  （含 / 不含 `scripted-backend-smoke` + `robot-rpc/client` feature
  组合）
- `cargo test --workspace --tests --features "robot-rpc/client" --
  --test-threads=1` 全过：940 base + schema variant count 3 +
  ScriptedBackend 13 lib unit + gateway 3 byte gate lib unit +
  `invoke_scripted_without_backend` 1
- `cargo run -p robot-msg --bin schema_lint` ✅（`SCHEMA_SCRIPTED_
  PAYLOAD = 0x040B` 唯一 + 升序 + 未与 deprecated 冲突）
- `./target/release/scripted_backend_smoke --phase all` ✅ 5/5 PASS
  exit 0
- `./target/release/pubsub_cleanup_smoke ... --phase selftest` ✅
  不退化
- `./target/release/crash_restart_smoke --iterations 5` ✅ 5/5 PASS
  (real-s1 path)

### §6.7 5 条不变量全保（**0** 降级 / **0** 升级 / **0** 偏移）

候选 1 是 append-only schema + 第二个真 action backend 落地；不触
S0 / systemd watchdog / EStop latch / RPC idempotency 任一路径。

### 审阅人

reviewer ACK 候选 1 (1/3) (2/3) (3/3) 三段全 push；mock-s1 baseline
regression 同意留 follow-up。

---

## 2026-04-30 · v0.3 B3 第一个真 action backend `HoldPoseBackend` + `ActionContext.command_seq` lib API 扩展（**状态：偏离登记**）

### 背景

B2 (1/3) → (3/3) 全部 land 后（详 2026-04-29 B2 §1.5 状态机扩展登
记），Phase B 进度推到"至少一个真 action backend"门槛（详
[`docs/handoffs/v0.3_entry.md`](handoffs/v0.3_entry.md) §2 Phase B
路线）。B3 选 `HoldPose` 作第一个真 backend：复用 B1 落地的
`build_hold_command(cfg, layout_id, anchor_bias, source_task_id,
now_ns)`，hold-until-cancel 语义；走 EXECUTING 路径但 wire 输出跟
Idle 状态 hold loop 字节级等价。

reviewer round 2 阻断 1 锁定核心契约（详
[`docs/handoffs/v0.3_b3_summary.md`](handoffs/v0.3_b3_summary.md)
§3）：HoldPose body 的 `source_task_id` **必须**等于
`run_action_tick_loop` 当前 `TickState.cmd_seq`，**不**得 backend
私有计数。证据链：

- `crates/robot-s0/src/ipc_thread.rs:903`：`buf.seq =
  fb.source_task_id();` —— S0 把 fbs body 的 `source_task_id` 直接
  写成 `JointCommandBuf.seq`（**不**是 FrameHeader.seq）。
- `crates/robot-core/src/adapter.rs:119-120`：`pub seq: u64` "命令
  seq 号，单调递增；rt_thread 判 CONTROL_LOST → ACTIVE 恢复时用"。
- 私有 counter 反例：daemon 先 Idle hold N 帧（seq 0..N），进入
  EXECUTING（私有 counter 0..M），S0 看到 `..., N, 0` → 破坏单调
  契约 + rt_thread 误触 CONTROL_LOST 恢复路径风险。

reviewer round 2 §3.4 显式放行 B3 做最小 lib API 调整：
`ActionContext` 加 `command_seq: u64` 字段 + `Coordinator::tick_action`
签名加 `command_seq: u64` 参数。不解锁 trait 方法签名 / enum variant
扩展边界（B2 round 1 reviewer 锁定的两条仍按 B2 边界走）。

reviewer 拍板形态（B3 outline §7 round 2 表）：

1. #1=A `HoldPose` / #2=hold-until-cancel / #3=每 tick 读 ctx /
   #5=不扩 `ActionKind` enum / #7=空 payload + `NO_PAYLOAD_SCHEMA_ID`
   / #9=unit + 2 integration + 双路 smoke / #10=progress 永远 0.0 +
   doc 不语义化 / #11=两 commit 节奏。全部 round 2 ACK。
2. #4 source_task_id：round 1 默认"backend 私有 counter"被 round 2
   阻断 1 打回；改 "复用 `TickState.cmd_seq` via ActionContext +
   tick_action 签名"。
3. commit 拆分：outline 2 + 实现 2 + reviewer round 1 fmt fix 1 =
   总 5 commit。

### 行为落地（B3 commit chain 概览）

| commit | 段 | 内容 |
|---|---|---|
| `916202e` | B3 outline | round 1 草稿 + 11 待拍板项 + 默认推进 |
| `2e9fc79` | B3 outline | round 2 三处 follow-up（阻断 1 source_task_id + 中度 bit-equivalent 收紧 + 低度 feature 口径） |
| `cc6b2df` | B3 (1/2) | **`HoldPoseBackend` 落地** + `ActionContext.command_seq` 字段 + `Coordinator::tick_action` 签名加参数 + `backend-hold-pose` Cargo feature 默认 on + 8 条 lib 单测 |
| `f4561bf` | B3 (2/2) | `tests/action_dispatch.rs` 加 2 条 integration test（happy_hold_pose bit-equivalent + cancel_hold_pose cross-segment monotonicity）+ 双路 crash_restart_smoke 5/5+5/5 PASS |
| `d1f5031` | reviewer round 1 fix | rustfmt `clippy::too_many_lines` 注释位置（attribute 后 inline + 续 2 行被 fmt 顶列首产生孤儿；改 leading comment block） |

### lib API 扩展点（reviewer round 2 §3.4 显式放行）

**(i)** `crates/robot-s1/src/action.rs::ActionContext`：

```diff
 pub struct ActionContext<'a> {
     pub clock: &'a dyn RobotClock,
     pub layout_id: u64,
     pub hardware: &'a HardwareSection,
     pub config: &'a CoordinatorConfig,
     pub anchor_bias: &'a [f64],
+    pub command_seq: u64,
 }
```

**(ii)** `crates/robot-s1/src/coordinator.rs::Coordinator::tick_action`：

```diff
 pub fn tick_action(
     &self,
     clock: &dyn RobotClock,
+    command_seq: u64,
 ) -> Option<JointCommandBody>;
```

**(iii)** `run_action_tick_loop::Executing` 分支调用点（当前行号
≈ `coordinator.rs:873`，按符号 `coordinator.tick_action(clock,
state.cmd_seq)` 定位；行号后续重构可能漂，审计时 grep 符号名）：
`coordinator.tick_action(clock, state.cmd_seq)` —— `state.cmd_seq`
同时作 publish frame 的 `FrameHeader.seq`，跨 backend / hold loop 用
同一 counter。

**(iv)** HoldPose backend tick 直接复用：
`build_hold_command(ctx.config, ctx.layout_id, ctx.anchor_bias,
ctx.command_seq, ctx.clock.now().0)`。

**`submit` / `cancel` 路径**：构造 `ActionContext` 时填 `command_seq:
0`（这两路径不在 publish frame seq 流上；`backend.start` /
`backend.cancel` 通常不产 body bytes）。

### Cargo feature wiring

- `crates/robot-s1/Cargo.toml`：`[features]` 加 `backend-hold-pose
  = []`；`default = ["backend-mock-dwell", "backend-hold-pose"]`。
- 整 `hold_pose.rs` 文件 `#[cfg(feature = "backend-hold-pose")]` gate；
  feature off 时 `mod hold_pose` 不 link → `BackendRegistry::from_inventory`
  收不到 `hold_pose` → `[s1.profile.backends = ["hold_pose"]]` 启动期
  fail-fast（符合 entry §2 "配置与编译 profile 不匹配启动期 fail-fast"
  硬约束）。

### 验收证据封存

- `cargo fmt --all -- --check` ✅
- `cargo clippy --all --all-targets -- -D warnings` ✅ 零警告
- `cargo test -p robot-s1 --all-targets` → **161/161 PASS**
  - lib 134（B2 (3/3) 收尾 126 → +8 hold_pose 单测 = 134）
  - mock_s1 bin 6 / s1_daemon bin 8（不变）
  - action_dispatch integration **10**（B2 (3/3) 8 → +2 hold_pose
    integration test = 10）
  - tick_loop_in_process integration 3（不变）
- feature-off 基线（reviewer 额外验）：
  - `cargo test -p robot-s1 --no-default-features --lib` ✅ 116/116 PASS
  - `cargo test -p robot-s1 --no-default-features --bin s1_daemon` ✅ 8/8 PASS
- `./target/release/crash_restart_smoke --iterations 5`（默认 s1_daemon
  路径）→ **5 PASS / 0 FAIL**
- `./target/release/crash_restart_smoke --use-mock-s1 --iterations 5`
  → **5 PASS / 0 FAIL**
- 双路 smoke 互证："no-active-action 路径外部可观测行为完全等价 B2
  (3/3)"（reviewer 阻断 4 锁定的 wiring 回归门槛沿用；B3 wiring 接入
  `HoldPoseBackend` inventory + Cargo feature 默认 on，**不**退化既
  有路径）。

### §6.7 不变量保持

5 条 invariant **全部不动**（v0.3 Phase A 收尾后状态延续 B2 → B3 不变）：

| # | 状态 | 说明 |
|---|---|---|
| #1 | ✅ | S0 存活 = 软件层安全充分条件（B3 不触 S0） |
| #2 | ✅ | systemd 看门狗（A2 freeze hook 真锁；B3 不触） |
| #3 | ✅ | EStop 不可清为失忆态（A1-5 撤 `--allow-empty-latch`；B3 不触） |
| #4 | ✅ | S0 重启不失忆（v0.2 fsync 双写；B3 双路 smoke 5/5 沿用 B2 证据形状） |
| #5 | ✅ | 幂等写 RPC（A1 IdempotencyCache；B3 仍走 in-process Rust API，无 RPC 路径） |

**额外观察**（B4 接班需注意）：B3 引入的"`source_task_id` 必须 ==
`TickState.cmd_seq`"虽不属 §6.7 invariant，但属 wire 协议契约。B4
做 RPC adapter 时不得在 gateway 端重写 frame seq（详
[`docs/handoffs/v0.3_b3_summary.md`](handoffs/v0.3_b3_summary.md)
§3.7）。后续如需提升为 §6.7 第 6 条不变量，走 errata + reviewer 拍板。

### 影响面

- **`docs/00_design.md` 不动**：§1.5 状态机 5 variant B2 已扩 + 登
  记；B3 仅在 EXECUTING 路径加第一个真 backend，跟设计文档既有描述
  对齐。
- **下一阶段 B4 接班点**：B4 robot-rpc gateway 起 adapter thread 把
  `ActionTicket.events: std::sync::mpsc::Receiver<ActionEvent>` 桥
  到 `tokio::sync::mpsc::Sender` → tonic `StreamTaskEvents`；B4 涉
  及 wire / 状态机 / 错误码映射等单独走 B4 outline + reviewer 拍板。
- **CLAUDE.md 状态块同步**：`实现阶段` 改 "v0.3 Phase B —— B1 + B2 +
  B3 全部 land。B4 准备开"；`Phase B 进度` 段 B3 ⬜ 改 ✅，加 commit
  chain 锚点 + 关键契约引用 + B4 wire 契约提醒。

### 审阅人

reviewer ACK：
- B3 outline round 1 ACK + round 2 三处 follow-up 全收尾 @ 2026-04-29。
- B3 (1/2) ACK + feature-off 基线额外验 @ 2026-04-29。
- B3 (2/2) reviewer round 1（rustfmt）阻断 + fix `d1f5031` ACK @ 2026-04-29。
- B3 总 5 commit（outline 2 + 实现/test 2 + reviewer round 1 fmt fix
  1；commit chain `916202e` → `d1f5031`）push 到 origin/master @
  2026-04-30；其中 B3 代码侧 = 实现 2 + fix 1 = 3 commit。

详细 commit chain + 关键契约审计 + reviewer 决策表见
[`docs/handoffs/v0.3_b3_summary.md`](handoffs/v0.3_b3_summary.md)。

---

## 2026-04-29 · v0.3 B2 §1.5 状态机扩展真落地（IDLE / EXECUTING / DEGRADED / BOOTING / SHUTTING_DOWN）（**状态：偏离登记**）

### 背景

`docs/00_design.md` §1.5 定义 S1 状态机包含 IDLE / EXECUTING / DEGRADED
（+ 启动 BOOTING / 停机 SHUTTING_DOWN）共 5 个 variant；v0.2 + v0.3 B1
期间 S1 仅落地"hold + heartbeat"单态闭环（隐式 IDLE），其余 4 个 state
留作设计文档冻结概念，**未**进 Rust 类型系统。B2 outline 拍板（详
[`docs/handoffs/v0.3_b2_outline.md`](handoffs/v0.3_b2_outline.md)
§5 + §10 决策 #11）："CHANGELOG 仅登记 §1.5 状态机扩展（IDLE /
EXECUTING / DEGRADED 真落地）；**不**动 §6.7 invariant"。

B2 (1/3) → (3/3) 12 commit 全部 land + push 到 origin/master 之后
（终点 commit `4b17e29`），登记本条偏离：5 个 state variant 已经在
`crates/robot-s1/src/coordinator.rs::CoordinatorState` 显式建模，
runtime 端 transition / submit 拒绝路径 / clear_error / tick_action
state-aware dispatch 全部按 §1.5 设计执行。

reviewer 拍板形态（B2 outline §10 决策 #10d + 阻断 2）：DEGRADED 在
B2 是 **S1 内部 degraded**（继续 hold + heartbeat，拒新 submit，等
`clear_error()` 解除），**不**触发 S0 control-lost / 不宣称 §6.7 #2
回退。这跟 `docs/00_design.md` §1.5 / §6.3 既有 DEGRADED 描述一致
（DEGRADED 不等于 control-lost），无 errata 需求。

### 行为落地（B2 commit chain 概览）

| commit | 段 | 内容 |
|---|---|---|
| `4b9d4de` | B2 (1/3) | `ActionId` / `ActionKind` / `ActionRequest` / `ActionBackend` / `ActionInstance` / `JointCommandBody` / 错误枚举 lib types |
| `54521ed` + `8508c3d` | B2 (1/3) reviewer fix | `NO_PAYLOAD_SCHEMA_ID` 哨兵 + `ActionTickOutput::Continue::command` doc override 语义修订 |
| `ae1c4cc` | B2 (2/3) | `SubmitError::NotReady` + `BackendStartFailed` + `ActionTicket` / `ActionEvent` / `ActionStatus` + `BackendRegistry` + `MockDwellBackend` |
| `89628b9` | B2 (2/3) | **`CoordinatorState` 5-variant 显式建模** + `Coordinator` 4 API + `tick_action` + `ProfileSection` L1+L2 校验 |
| `3c9eaae` + `2841c44` | B2 (2/3) reviewer fix | `submit()` state-priority + `NoEnabledBackends` profile fail-fast |
| `96fb757` | B2 (3/3) | `run_action_tick_loop` state-aware tick dispatch + visibility 升级 + dead_code 清空 |
| `8d39afa` | B2 (3/3) | `s1_daemon` binary wiring（profile + registry + Coordinator + run_action_tick_loop） |
| `4b17e29` | B2 (3/3) | `tests/action_dispatch.rs` 8 路径 in-process integration 验收 |

### CoordinatorState 5 variant 落地映射

| §1.5 / outline §5.2 状态 | Rust enum variant | 入口转移 | 出口转移 | tick 期间行为 |
|---|---|---|---|---|
| BOOTING | `Booting` | `Coordinator::new()` 构造期 | `mark_idle()` → Idle | 防御性不发 cmd（anchor 完应已 mark_idle） |
| IDLE | `Idle` | mark_idle / Executing 终态 / clear_error 出口 | submit OK → Executing；mark_shutting_down → ShuttingDown | B1 hold cmd 50Hz + hb 50Hz |
| EXECUTING | `Executing { action_id, started_at_ns }` | submit Idle → OK | tick_action Completed → Idle；Failed Pre/Deadline → Idle；Failed Safety/Adapter/Internal → Degraded；mark_shutting_down → ShuttingDown | hb 50Hz；cmd = `tick_action()` 输出（Some publish；None 跳过本帧） |
| DEGRADED | `Degraded { reason }` | tick_action 收 Failed Safety/Adapter/Internal | clear_error → Idle；mark_shutting_down → ShuttingDown | 续 B1 hold cmd 50Hz + hb 50Hz（**S1 内部 degraded**，不触 S0 watchdog） |
| SHUTTING_DOWN | `ShuttingDown` | 任意状态 mark_shutting_down | 终态（B2 不复活） | tick loop 检测到此状态 break；coord drop current → ticket 端 RecvError |

### submit / cancel / clear_error / status API 落地

- `submit(req, clock)`：状态优先 4 路拒（`NotReady` / `BusyExecuting(id)`
  / `CoordinatorDegraded` / `ShuttingDown`），仅 Idle 才走 validate /
  dispatch / start。reviewer round 3 阻断 1 拍板把全部 4 路放 lock
  内一气呵成（不让坏请求 / unknown backend 抢先返 `InvalidRequest` 等
  掩盖状态语义）。
- `cancel(action_id, clock)`：cooperative —— 调 backend `cancel()` +
  set `pending_cancel`；状态保持 Executing 等下次 tick 收敛
  （Cancelled / Failed），**不**双发 Completed + Cancelled。
- `clear_error()`：仅 Degraded → Idle 合法；其它 4 状态返
  `ClearError::NotDegraded(state_label)`。
- `status(action_id)`：先查当前 Executing，再查 cap=128 finished
  ring；找不到返 None。reviewer 拍板**不**做无界 map（防 long-running
  daemon 慢漏内存）。

### 验收证据封存

- `cargo test -p robot-s1 --all-targets` → **151/151 PASS**（lib 126 +
  mock_s1 bin 6 + s1_daemon bin 8 + action_dispatch integration 8 +
  tick_loop_in_process integration 3）。
- `cargo clippy --all --all-targets -- -D warnings` → 零警告。
- `cargo fmt --all -- --check` → clean。
- `./target/release/crash_restart_smoke --iterations 5`（默认 s1_daemon）
  → **5 PASS / 0 FAIL**（latch primary='1' bak='1' + restart daemon
  initial estop_latched latched=true 证据形状不变）。
- `./target/release/crash_restart_smoke --use-mock-s1 --iterations 5`
  → **5 PASS / 0 FAIL**。
- 双路 smoke 互证："no-active-action 路径外部可观测行为完全等价 B1"
  （reviewer 阻断 4 锁定的 wiring 回归门槛）。
- `rg -n "allow\(dead_code\)" crates/robot-s1/src` → 0 实代码命中
  （仅 1 注释引用说"已批量移除"）。
- `rg "tokio|tonic|ActionKind::Stop|MoveToPose|FollowTrajectory|PolicyAction" crates/robot-s1/src` →
  全部命中在注释 / doc 的禁止语境（"不引"/"不放"/"B4 时桥接"）。

### §6.7 不变量保持

5 条 invariant **全部不动**（v0.3 Phase A 收尾后状态延续）：

| # | 状态 | 说明 |
|---|---|---|
| #1 | ✅ | S0 存活 = 软件层安全充分条件（B2 不触 S0） |
| #2 | ✅ | systemd 看门狗（A2 freeze hook + freeze_systemd_smoke 真锁；B2 不触） |
| #3 | ✅ | EStop 不可清为失忆态（A1-5 撤 `--allow-empty-latch`；B2 不触） |
| #4 | ✅ | S0 重启不失忆（v0.2 fsync 双写；B2 双路 smoke 5/5 沿用 B1 证据形状） |
| #5 | ✅ | 幂等写 RPC（A1 IdempotencyCache；B2 仍走 in-process Rust API，无 RPC 路径） |

### 影响面

- **`docs/00_design.md` 不动**：§1.5 状态机文字版已经提了 IDLE /
  EXECUTING / DEGRADED 等 variant；B2 实现层只是把它们从概念变成 Rust
  enum，无 errata 需求。
- **下一阶段 B3 / B4 接班点**：B3 加新 backend 时 `ActionKind` enum
  按需扩 variant（B2 仅 `DwellMs` / `HoldPose`）；B4 robot-rpc gateway
  起 adapter thread 把 `ActionTicket.events` 桥到 `tonic::StreamTaskEvents`
  （B2 lib 已用 `std::sync::mpsc::Receiver` 埋好接口）。
- **CLAUDE.md 状态块同步**：`实现阶段` 改 "v0.3 Phase B —— B1 + B2 全
  部 land。B3 准备开"；`Phase B 进度` 段加 B2 commit chain 锚点 +
  B2 summary handoff 路径。

### 审阅人

reviewer ACK：
- B2 (1/3) ACK push 3 commit @ 2026-04-28（B1 收尾后第 2 天）。
- B2 (2/3) ACK push 4 commit @ 2026-04-29（含 reviewer 2 阻断 + 1 注
  释 fix）。
- B2 (3/3) ACK push 3 commit @ 2026-04-29（双路 crash_restart_smoke
  本机 5/5+5/5 实跑通过；nit 修留下次 docs 整理）。

详细 commit chain + 拍板表见
[`docs/handoffs/v0.3_b2_summary.md`](handoffs/v0.3_b2_summary.md)。

---

## 2026-04-28 · v0.3 A1-5 撤 `--allow-empty-latch` + 升级 §6.7 #3 真宣称（**状态：偏离登记**）

### 背景

A1-1 → A1-4 完整 land + A1-4 milestone handoff（`docs/handoffs/v0.3_a1_4_clear_estop_smoke.md`）
封存 5/5 PASS 实证之后，Phase A 还差最后一步：撤 v0.2 期间临时引入的
`--allow-empty-latch` opt-in 守门，让 strict fail-closed 成为 latch 缺失
的唯一行为，闭合 §6.7 #3 真宣称（v0.2 收尾时降级宣称的两条之一；另一
条 #2 已由 A2 freeze hook + `freeze_systemd_smoke` land 时升级，详
2026-04-26 round 4 / 5 / 6 + `docs/handoffs/2026-04-26_6.md`）。

reviewer 拍板形态（A1-5）：

1. `MissingLatchPolicy` enum 整个删（不留 `Strict`-only 单 variant 假
   接口；`--allow-empty-latch` 撤掉就把逃生门收干净）。
2. 历史 handoff（`2026-04-26.md` / `_3.md` / `_5.md` /
   `_bus2_runbook*.md` 等）保留作 pre-A1-5 时间快照，**不**重写。
3. active runbook + 当前状态文档 + CHANGELOG + A1-4 milestone handoff
   同步到 A1-5 后状态。
4. commit 拆两条：行为修复 + 文档宣称升级（**本条**）。

### 行为修复（commit `45df648` 🐛 fix: A1-5 remove allow-empty-latch escape hatch）

- `crates/robot-s0/src/estop_latch.rs`：删 `MissingLatchPolicy { Strict,
  AllowEmpty }` enum + `load_estop_latch_with_mode(path, mode)` 中间
  函数。`load_estop_latch(path)` 直接走 strict fail-closed 唯一路径；
  err 文案改 `"first-deploy: provision \"0\\n\" to both primary +
  backup before start; runtime clear: ClearEStop control action"`，给
  运维明确 ① 首次部署 provision + ② 运行期 ClearEStop 两条补救路径。
- `crates/robot-s0/src/bin/s0_daemon.rs`：删 `--allow-empty-latch` CLI
  parse arm + Args / ResolvedArgs `allow_empty_latch` 字段 + 帮助文段。
- `crates/robot-tests/src/bin/crash_restart_smoke.rs`：删两处 `.arg("--allow-empty-latch")`；
  `run_iteration` 起手势加 latch provision（写 `0\n` 到 primary +
  backup）跟 `scripts/systemd/README.md` "首次部署" 推荐流程一致。
- `scripts/systemd/README.md`：删 "替代流程：一次性 `--allow-empty-latch`
  opt-in" 整段；"推荐流程" 改名 "deploy 流程"；底部不在范围段加 "
  `--allow-empty-latch` opt-in flag — A1-5 已撤"。
- `scripts/systemd/xlerobot-s0-bus1.service` / `bus2.service` /
  `xlerobot-s0-freeze-test.service`：注释删 `--allow-empty-latch` /
  `MissingLatchPolicy::Strict` 提及，改成 "strict fail-closed（A1-5
  撤掉 `--allow-empty-latch` opt-in 后唯一行为）"。bus2 安装步骤删
  "替代 b) drop-in opt-in"（只剩 provision 路径）。
- `scripts/run_clear_estop_smoke.sh` / `crates/robot-rpc/Cargo.toml`
  + `lib.rs` / `crates/robot-s0/src/ipc_tick.rs` / `docs/10_verification.md`：
  注释字面同步。

行为修复验证：

- `cargo fmt --check / clippy --all --all-targets [--features
  robot-tests/clear-estop-e2e] -- -D warnings`：默认 + feature 两套全过。
- `cargo test --workspace --all-targets`：27 test groups 全 ok（含
  estop_latch 14 单测覆盖 strict-only 行为：`load_nonexistent_returns_not_found_err`
  断言 err 文案含 `"ClearEStop control action"` + `"first-deploy"`）。
- 直接 daemon 跑：缺 latch → ERROR `"primary AND backup both missing;
  fail-closed (provision \`0\\n\` ... or use ClearEStop control action
  ...)"` + `fatal: failed to load EStop latch`，新文案正确。
- `--allow-empty-latch` 传给 daemon → "unknown arg: --allow-empty-latch"
  + 退出（flag 已撤）。
- `scripts/run_clear_estop_smoke.sh`：5/5 PASS 不变（mock_hard_rt 路径
  不受影响；harness provision latch=`1\n` 也不受 enum 删除影响）。

**已知**：`crash_restart_smoke` 在本机 dev box 5/5 FAIL，**根因跟 A1-5
无关** —— 是预先存在的 inventory + LTO 交互问题：`xlerobot_bus2_full.toml`
要求 `feetech_sts3215` adapter，但 `cargo build --release --bin
s0_daemon`（即使带 `--features xlerobot`）的产物里 `inventory::submit!`
注册被 fat LTO 剥掉。验证：把 A1-5 改动 stash 后用 commit `9ff22f3`
同款流程跑也 FAIL（latch primary empty）。这条是独立 follow-up，**不**
卡 A1-5 ACK；Orin 真机回归留 reviewer 按需。

### §6.7 真宣称升级（**本条 docs commit**）

`docs/00_design.md` §6.7 不变量正文**不动**；本登记锁住 v0.3 Phase A
收尾后的真宣称状态：

| 不变量 | v0.2 收尾状态 | v0.3 Phase A 收尾状态 |
|---|---|---|
| #1 S0 存活 = 软件层安全充分条件 | ✅ | ✅（不变） |
| #2 systemd 是最外层看门狗 | ⚠️ 代码层面到位（LivenessObserver 4 worker `tick_count`）；未端到端真锁定 | **✅ 真锁住**（v0.3 A2 freeze hook + freeze_systemd_smoke 5/5 PASS + Orin AGX 1 round PASS；详 2026-04-26_6.md） |
| #3 EStop 不可清为失忆态 | ⚠️ 代码层面到位（`MissingLatchPolicy::Strict` + `--allow-empty-latch` opt-in）；"重设 latch=0" 仍走 deploy 流程 | **✅ 真锁住**（v0.3 A1 ClearEStop control action 端到端真闭环 + A1-4 smoke 5/5 PASS + A1-5 撤 `--allow-empty-latch`；strict fail-closed 是 latch 缺失唯一行为，唯一清 latch 路径 = ClearEStop control action） |
| #4 S0 重启不失忆 | ✅（fsync 双写 + crash_restart_smoke 5/5 PASS） | ✅（不变） |
| #5 幂等写 RPC | ✅ 代码层（IdempotencyCache + idempotency_key fingerprint） | ✅ + A1-3 (3/3-b) `idem_replay` e2e case 5/5 PASS（real fsync 只发生一次；Replay 返同 cleared_at_ns） |

### 同步更新的当前状态文档

- `CLAUDE.md` 状态段：A1 进度 A1-1 → A1-5 全 ✅；A2 ✅ 已 land；§6.7
  真宣称表 5 条全 ✅；Phase B 准备开。
- `docs/handoffs/v0.3_a1_4_clear_estop_smoke.md` §5 表格 row #3 ⚠️ →
  ✅；§5.1 改 "A1-1 → A1-5 闭环证据链"；新加 §5.2 "真锁住意味着什么"
  3 条运维语义。

### Scope / 边界（reviewer 拍板）

- **不**改 `docs/00_design.md` §6.7 不变量正文（升级走 CHANGELOG 偏离
  登记，跟 v0.2 round 4 / 5 / 6 / 7 降级登记同款机制）。
- **不**重写历史 daily handoff（`2026-04-26.md` / `_3.md` / `_5.md` /
  `_bus2_runbook*.md` 等保留作 pre-A1-5 时间快照）。
- **不**改 `docs/handoffs/v0.3_entry.md` 正文（v0.3 entry milestone 是
  Phase A 入口锚点；§1.2 降级宣称表 + §3.5 5 步 checklist + §5 禁止项
  都是 entry 阶段记录，A1-5 land 后不重写；当前真宣称状态以本登记 +
  A1-4 handoff §5 + CLAUDE.md 状态段为准）。
- 真宣称升级**不**触碰 §6.7 #1 / #4 / #5（本来就 ✅）。

### 影响面

- 代码：行为 commit `45df648`（12 文件，`+137 -211` —— 删多于加，把逃
  生门彻底收干净）。
- 文档：本 docs commit（CHANGELOG 加本条 + CLAUDE.md 状态段 + A1-4
  handoff §5 升级 ✅）。

### 审阅人

reviewer（用户）— A1-5 拍板：(1) `MissingLatchPolicy` enum 整个删；
(2) 历史 handoff 不重写；(3) commit 拆两条（行为 + 文档宣称升级）。
本登记 + A1-4 handoff §5 升级 + CLAUDE.md 状态段同步 land 后由 reviewer
做 round 复核闭环。

---

## 2026-04-28 · v0.3 A1-3 (3/3-a) `clear_estop_e2e` smoke binary 依赖白名单更新（**状态：登记**）

### 背景

A1-3 (3/3) 起在 `crates/robot-tests` 加 `clear_estop_e2e` 端到端 smoke
binary，跟 `robot_rpc_gateway` + `s0_daemon` 起两进程跑真 gRPC client →
iceoryx2 RR → daemon ipc_thread → latch fsync 双写。需要 client-side
tonic stub + tokio process control + tempdir 等 deps；按 dep 白名单机制
登记 scope + 边界。

### Scope

- **`robot-rpc`** 加 `client` cargo feature：默认仍 `build_client(false)`；
  开 `client` feature（`CARGO_FEATURE_CLIENT=1`）让 build.rs 跑
  `tonic_build::configure().build_client(true)` 生成 `RobotControlClient`
  stub。生产二进制 `robot_rpc_gateway` 默认 features，**不**带 `client`，
  无 client-only runtime 影响。
- **`robot-tests`** 加 `clear_estop_e2e` 入口 cargo feature（`required-features
  = ["clear-estop-e2e"]` 守门，跟 `freeze_systemd_smoke` 同款）：
  - `robot-rpc = { workspace = true, optional = true, features = ["client"] }`
    —— smoke 用 client stub 调 `clear_e_stop` RPC。
  - `tonic = { workspace = true, optional = true }` —— client transport
    stack（用 transport feature 跟 robot-rpc lib 共享）。
  - `tokio = { workspace = true, optional = true, features = ["process",
    "io-util"] }` —— smoke 主 entry `tokio::runtime::Builder` 跑 tonic
    client + 子进程协调；`process` 让 `tokio::process::{Command, Child}`
    + `tokio::time::timeout` 干净协调子进程退出，`io-util`
    让 stderr/stdout 流式 read 等 ready signal。
- **`configs/clear_estop_e2e.toml`** 新加：单 `mock_hard_rt` hardware +
  1 joint，smoke 不碰真 serial / 真校准。
- **不**新加 workspace dep（tonic / tokio / robot-rpc 都已在 workspace）。

### 边界

- **不**改 `docs/00_design.md` §5.5 / §6.7 不变量正文。
- **不**塞 cargo test --workspace 默认（reviewer 拍板：两进程 + iceoryx2
  shm + loopback 端口属于 smoke，跟 `crash_restart_smoke` /
  `freeze_systemd_smoke` 一样 CI / 人工显式跑）。
- **不**用 `tempfile` workspace dep（workspace 顶层注释明示仅在 dev-deps
  位置继承；smoke binary 不能 link dev-dep；harness 自己 mkdtemp 风格用
  `/tmp/clear_estop_e2e_<pid>_<seq>`）。
- **不**在 `robot-rpc` lib `pub use` client stub —— `client` feature 仅
  控制 build.rs，调用方走 `robot_rpc::proto::robot_control_client::RobotControlClient`
  路径直接命中 tonic-generated module（feature off 时该路径不存在，编
  译期报错）。
- **不**接非 loopback / TLS（v0.3 fake-token + plain TCP；TLS 留 v0.4+）。

### 影响面

- `crates/robot-rpc/Cargo.toml` `[features]` 加 `client = []`。
- `crates/robot-rpc/build.rs` 读 `CARGO_FEATURE_CLIENT` 控制
  `tonic_build::build_client(...)`；加 `cargo:rerun-if-env-changed` 让
  cargo 跨 feature toggle 重 build。
- `crates/robot-tests/Cargo.toml`：加 `[[bin]] clear_estop_e2e`（feature
  gated）+ 3 条 optional deps（robot-rpc / tonic / tokio；**不**用
  tempfile workspace dep，详 Scope 段）+ `[features] clear-estop-e2e`。
- `crates/robot-tests/src/bin/clear_estop_e2e.rs`：新文件，~700 行 harness
  骨架（CLI / preflight / `CaseCtx` 分配 / role table 写出 / loopback
  port 分配 / `SubprocessHandle` / readiness probes / `selftest` case）。
- `configs/clear_estop_e2e.toml`：新文件，单 mock_hard_rt hardware。

### 审阅人

reviewer（用户）— A1-3 (3/3) 拍板：robot-rpc 加 `client` feature；
clear_estop_e2e smoke binary 形态选 `crates/robot-tests/src/bin/`（不普
通 tests/*.rs），跟 `crash_restart_smoke` / `freeze_systemd_smoke` 同款
CI/人工显式跑。

---

## 2026-04-28 · v0.3 A1-3 (2/3) `robot_rpc_gateway` binary 依赖白名单更新（**状态：登记**）

### 背景

A1-3 (2/3) 起在 `crates/robot-rpc` 加 `robot_rpc_gateway` 独立进程
binary（reviewer 拍板：loopback-only tonic gRPC server + iceoryx2 RR
client → s0_daemon 端 `/<robot_id>/control/clear_estop` topic）。需要
若干新依赖；按 `docs/CHANGELOG.md` 2026-04-22 "workspace runtime 依赖
白名单机制" 登记 scope + 边界。

### Scope

- **仅 `robot-rpc` crate** link 以下新依赖：
  - `tokio` workspace dep（v1，features `["macros", "rt-multi-thread",
    "signal", "sync", "time", "net"]`）—— tonic transport stack 跑 async
    server 必须的 runtime。
  - `tonic` workspace feature 列表加 `transport`（旧 features
    `["codegen", "prost"]` 扩为 `["codegen", "prost", "transport"]`）——
    A1-1 (3/3) skeleton 阶段只用 codegen+prost；A1-3 (2/3) 真起 server
    要 transport stack。
  - `crossbeam-channel`（已在 workspace；首次进 robot-rpc dep 列表）——
    RR I/O 独立 `std::thread` 跟 tonic worker 之间转发 request/response。
  - `iceoryx2`（已在 workspace；首次进 robot-rpc dep 列表）—— gateway
    端 iceoryx2 Node + rr_client 跨进程 RR。
  - `robot-transport`（已在 workspace；首次进 robot-rpc dep 列表）——
    `rr_client` 单端工厂 + 公共 wire wrapper 类型。
  - `robot-logging`（已在 workspace；首次进 robot-rpc dep 列表）——
    bin `main` 统一 tracing subscriber init。
  - `flatbuffers`（已在 workspace；首次进 robot-rpc dep 列表）—— gateway
    handler decode wire `ClearEStopResponse` 用 `flatbuffers::root::<FbResp>`。

- 其它 crate 仍**禁** link `tonic` / `prost` / `tokio` / `tonic-build`
  / `protoc-bin-vendored`：daemon (`robot-s0`) / state machine
  (`robot-msg`) / transport (`robot-transport`) / RT crates 全部不依赖
  这些 RPC / async 栈。
- `cargo deny` workspace 配置（如有）相应放行 `robot-rpc` link `tokio`，
  其它 crate 仍 deny。

### 边界

- **不**改 `docs/00_design.md` §5.5 / §6.7 不变量正文。
- **不**起非 loopback bind（reviewer 拍板：默认 `127.0.0.1:50051`，
  `--listen-addr` 可覆盖但必须为 loopback；`0.0.0.0` / 外网地址直接拒
  启动）。
- **不**接 TLS / mTLS（v0.3 fake token + plain TCP；TLS 留 v0.4+）。
- **不**接 §5.3 其它 RPC（task / stream / S1 coordinator 交互；B4 范围）。
- **不**升 §6.7 #3 真宣称（A1-5 范围）。

### 影响面

- `crates/robot-rpc/Cargo.toml` `[dependencies]` 加 `tokio` /
  `crossbeam-channel` / `iceoryx2` / `robot-transport` / `robot-logging`
  / `flatbuffers`；`tonic` 通过 workspace 升 feature `transport`。
- `Cargo.toml` workspace `tonic` features 加 `transport`；新加 `tokio`
  workspace dep。
- `crates/robot-rpc/Cargo.toml` 顶部"无 binary，全纯单测"旧描述同步成
  "lib + `robot_rpc_gateway` binary（A1-3 (2/3) 起）"。
- 编译期：`robot-rpc` build 时间增加（tonic transport + tokio +
  iceoryx2 编译；预期 30s+），其它 crate 不受影响。

### 审阅人

reviewer（用户）— A1-3 (2/3) 拍板"`robot-rpc` 加 transport / tokio +
binary 启动期；依赖白名单同步登记单独 docs/build commit"。

---

## 2026-04-27 · A1-3 (1/3b) reviewer round 3 阻断 — RpcStatus 扩 ResourceExhausted/Aborted + deadline 不再事后覆盖 outcome（**状态：登记**）

### 背景

A1-3 (1/3b) commits `bf4f24d` + `2d790f0`（emergency preempt + deadline
三处 check）落地后 reviewer 复核找到两个新阻断：

#### 阻断 1：内部 payload 新增状态在 FlatBuffers wire 上表达不了

`crates/robot-s0/src/ipc_thread.rs` `ClearEStopRrPayload` 已加
`AlreadyInProgress` / `PreemptedByEmergency`，注释要求 gateway 映射到
`tonic::Code::ResourceExhausted` / `Aborted`。但
`crates/robot-msg/schemas/clear_estop.fbs:99` 的 `RpcStatus` enum 只有
7 个 variant（Unknown / Ok / FailedPrecondition / PermissionDenied /
InvalidArgument / Internal / DeadlineExceeded），无法承载这两个新状态。
A1-3 (1/3c) `rr_server_thread` 构造 `ClearEStopResponse.status` 时必然
**丢语义**——除非靠 `error_msg` 字符串解析，但这不该进协议层。

#### 阻断 2：response 前 deadline 覆盖会造成"已执行成功但返 DeadlineExceeded"

`crates/robot-s0/src/ipc_thread.rs:763` （commit `2d790f0`）在 ipc_tick
已完成后用 `DeadlineExceeded` 覆盖真实 outcome；同文件注释也承认硬件 /
磁盘 latch 可能已经清掉。这跟 `clear_estop.fbs:52`（A1-2 round 2 落地
后的"S0 端 action layer ordering"段）写的"`deadline / emergency preempt`
时三个翻转都不发生"硬契约**冲突**——客户端收到 `DeadlineExceeded` 会
以为没成功，但实际硬件已 clear。

### 修复

#### 修复 1：fbs RpcStatus append-only 扩两个 variant

`crates/robot-msg/schemas/clear_estop.fbs` `RpcStatus` enum 加：
- `ResourceExhausted = 7` → `ClearEStopRrPayload::AlreadyInProgress`
  （live 单飞冲突）
- `Aborted = 8` → `ClearEStopRrPayload::PreemptedByEmergency`
  （emergency preempt）

FlatBuffers append-only 兼容：旧数值 0..6 不变，旧 reader 读到 7/8 会
fallback 到 `Unknown`（不破坏向后兼容；同 fbs 顶部 doc 拍板）。

`crates/robot-rpc/src/clear_estop.rs::rpc_status_to_tonic_code` 加两条
显式 match arm：
- `RpcStatus::ResourceExhausted => Code::ResourceExhausted`
- `RpcStatus::Aborted => Code::Aborted`

`rpc_status_variant_count_matches_explicit_mapping` 锁测把
`EXPECTED_VARIANTS` 从 7 升到 9，强制 reviewer 在新增 variant 时同步
更新 mapping + 单测。

#### 修复 2：deadline 不再事后覆盖真实 outcome

`crates/robot-s0/src/ipc_thread.rs` §6.6) RR live response 派发段**撤掉**
`if clock.now().0 > req.deadline_ns { DeadlineExceeded }` 包装层，恢复
直接 `match status` 映射 `ClearEStopTickOutcome` → `ClearEStopRrPayload`。

deadline 现在只在两处 check：
- §5.5) RR drain 入口：拦截过期请求不入队（写盘 / clear / 翻 guard
  都没发生）
- §5.6) preempt 段：drain 入队后到 ipc_tick 之前的窗口（同 emergency
  preempt 共用 take + remove + reply 三件事）

ipc_tick 跑期间过期的极端 window 不再"事后覆盖"——一旦 action 在 deadline
前开始并产出 `Cleared` / `FsyncFailed` / `StateNotSafe` /
`NotTorqueOffConfirmed`，response 必须返**真实** outcome。reviewer 拍板
"若要挡长 fsync 前过期，就把 deadline 传进 action handler，在不可逆步骤
前检查"——这条**留 A1-3 (3/3) 端到端阶段评估**，本轮 1/3b round 3 不
强求；ipc_tick 同步执行 ms 级，single tick 内 deadline 过期的 window
极小。

### 顺手修复

`docs/handoffs/v0.3_entry.md` §3.3 line 5 旧措辞 "显式 match 4 variant"
现在更新为：
- `ipc_tick` 输出 4 个 `ClearEStopTickOutcome` variant
- `ipc_thread` 映射到 8 个 `ClearEStopRrPayload` variant（含
  `WouldClear` / `AlreadyInProgress` / `PreemptedByEmergency` /
  `DeadlineExceeded`）
- gateway `rpc_status_to_tonic_code` 映射到 9 个 `tonic::Code`
  （含新加 `Unknown` 旧 writer 兼容路径）

### 不改变的内容

- 不改 `docs/00_design.md` §5.2 / §5.3 / §6.7 不变量正文
- 不动 fbs 已有 RpcStatus 数值 0..6（FlatBuffers append-only 兼容）
- 不动 `ClearEStopRequest` / `ClearEStopResponse` table 字段或字段编号
- 不升 §6.7 #3 真宣称（A1 全 land 后；A1-5）
- 不接 robot-rpc → S0 RR 入口（A1-3 (1/3c) / A1-3 (2/3)）
- 不撤 `--allow-empty-latch`（A1-5）
- 不把 deadline 传进 `try_apply_clear_estop` action handler（reviewer 拍板
  本轮不强求；A1-3 (3/3) e2e 时再评估）

### 审阅人

reviewer（用户）— A1-3 (1/3b) round 3 阻断 1 + 2 + v0.3_entry §3.3 残留。

---

## 2026-04-27 · A1-3 (1/3b) reviewer round 2 阻断 — RR live emergency preempt + deadline 语义闭环（**状态：登记**）

### 背景

A1-3 (1/3b) commit `d13ed04`（ipc_thread RR control plane drain + live
单飞 + dry_run 短路）落地后 reviewer 复核找到两个真实阻断：

#### 阻断 1：emergency preempt 后 RR live ClearEStop 在 EventQueue 滞留

`crates/robot-s0/src/ipc_thread.rs:536` 把 RR live 转成裸
`Event::ClearEStop` 入 `EventQueue`；`crates/robot-s0/src/ipc_tick.rs:128`
的 `drain_emergency()` 遇到 `EStop` 会 return，但 `EventQueue` **保留**
后面的 `Event::ClearEStop`。结果：

```
t0: RR drain push Event::ClearEStop + in_flight_live = Some(req_X)
t0: EStop topic event 也进 events queue
t0: ipc_tick Step 1 drain_emergency 抽 EStop → 处理 → return outcome
    （ClearEStop 仍在 queue）
t1: ipc_tick Step 1 没 emergency → Step 3 处理 ClearEStop → 三步硬顺序
    成功 → outcome.clear_estop = Some(Cleared)
t1: ipc_thread §6.6) 派发 Cleared 给 req_X
```

`req_X` 是发送时的客户端语义"我看到上一次 latched=true 想 clear"，但
emergency 是新触发的（后于 req_X），客户端**不应**清这次新的 emergency
latch。

#### 阻断 2：内部 RR request 丢了 `deadline_ns`

`docs/00_design.md` §5.2 frozen schema + `clear_estop.fbs:112` 明确要求
daemon 端在收到 request → 处理完之间任意时刻发现 `clock.now() >
deadline_ns` 即返 `DeadlineExceeded`。但 `crates/robot-s0/src/
ipc_thread.rs:98` `ClearEStopRrRequest` 只有 `request_id` / `dry_run` /
`confirmation` / `audit_reason` 四字段；`ClearEStopRrPayload` enum 也没
`DeadlineExceeded` variant。这会让过期 clear 在 ipc_thread 排队或
emergency 后仍可能执行。

### 修复

#### 修复 1：emergency preempt 取消 in-flight live

`ClearEStopRrPayload` 加 `PreemptedByEmergency` variant；ipc_thread 主
循环加 §5.6) **preempt 段**（在 §5.5 RR drain 之后，§6 ipc_tick 之前）：

```
if events 有 EStop / SafetyStopFailed event && in_flight_live 是 Some:
    1. EventQueue::remove_clear_estop_events()  // 防 ipc_tick Step 3 误处理
    2. take in_flight_live → reply PreemptedByEmergency
```

`crates/robot-s0/src/state_machine.rs` `EventQueue` 加：
- `pub fn has_emergency(&self) -> bool`
- `pub fn remove_clear_estop_events(&mut self) -> usize`

#### 修复 2：deadline 三处 check

`ClearEStopRrRequest` 加 `pub deadline_ns: u64`（CLOCK_MONOTONIC_RAW
mono raw 时间戳）；`ClearEStopRrPayload` 加 `DeadlineExceeded` variant。

ipc_thread 主循环三处 deadline check（reviewer 拍板"RR drain、执行前、
响应前都要显式检查"）：

- **§5.5) RR drain 入口**：`clock.now() > req.deadline_ns` → 直接 reply
  `DeadlineExceeded`，**不**入 in_flight_live、**不**push event。
- **§5.6) preempt 段**（emergency / deadline 共用）：drain 入队后到
  ipc_tick 之前的窗口（典型场景：上一 tick 没消费 ClearEStop event，
  本 tick 才消费，中间隔了 500µs sleep）期间 deadline 过期 → take
  in_flight_live + remove ClearEStop event + reply `DeadlineExceeded`。
- **§6.6) RR live response 派发前**：覆盖 ipc_tick 跑期间过期的极端
  window（debug build 慢 / effectful pending 1s 占用）→ 用
  `DeadlineExceeded` 覆盖 ipc_tick 计算的 outcome。

#### 顺手修复：fbs:21-34 注释残留

`clear_estop.fbs` "S0 端 action layer ordering" 段：
- 旧版顺序写 `save_estop_latch(false)` + guards 翻转早于 clear_safety_latch，
  跟 A1-2 round 2 落地的 `save_estop_latch_clear` backup-first +
  clear_safety_latch 早于 guards 不一致
- 重写为 8 步（加 deadline / emergency preempt 两步），跟 ipc_thread
  实施版对齐

### 测试

- 已加单测覆盖 emergency preempt + deadline 三处 check + dry_run 也走
  deadline check 路径；详 commit body。
- 现有 7 条 RR 单测兼容更新（加 `deadline_ns` 字段）。

### 不改变的内容

- 不改 frozen schema (clear_estop.fbs `ClearEStopRequest` /
  `ClearEStopResponse` table 字段或字段编号)；schema 里 `deadline_ns` /
  `RpcStatus::DeadlineExceeded` 早就在，本次只是把 S0 内部 RR types
  跟 schema 对齐。
- 不改 `docs/00_design.md` §5.2 / §5.3 / §6.7 不变量正文。
- 不升 §6.7 #3 真宣称（A1 全 land 后；A1-5）。
- 不接 robot-rpc → S0 RR 入口（A1-3 (1/3c) / A1-3 (2/3)）。
- 不撤 `--allow-empty-latch`（A1-5）。

### 审阅人

reviewer（用户）— A1-3 (1/3b) round 2 阻断 1 + 2 + fbs:27 注释残留。

---

## 2026-04-27 · A1-2 reviewer round 2 阻断 — `save_estop_latch_clear` backup-first 修复 ClearEStop 失败原子性（**状态：登记**）

### 背景

A1-2 commit `60f22df`（ClearEStop 三步硬顺序）落地后 reviewer 复核发现
真实安全阻断：

`crates/robot-s0/src/clear_estop_action.rs::try_apply_clear_estop` 直接
复用 `crates/robot-s0/src/estop_latch.rs:216` 的 `save_estop_latch(_, false)`，
但底层是 **primary-first** 写入：

```
1. save_single_atomic(primary, false)     → 可能成功（primary = "0\n"）
2. save_single_atomic(bak, false)         → 失败 → 函数返 Err
```

partial failure 后磁盘状态：primary `= "0\n"`，bak `= "1\n"`。
`try_apply_clear_estop` 返 `FsyncFailed`，guard 不翻、executor latch 不动；
但 daemon crash/restart 后 `load_estop_latch` 默认走 primary
（`crates/robot-s0/src/estop_latch.rs:118-120`），只在 primary
`NotFound` / `InvalidData` 时才 fallback bak。它读 primary `"0\n"` →
`latched=false` → **软件 latch 被错误清掉**，破坏 §6.7 #4 "S0 重启不失忆"
+ A1-2 失败不变量"磁盘 latch 不动"。

reviewer 拍板阻断：A1-2 commit `60f22df` 安全契约未达成。

### 修复

- `crates/robot-s0/src/estop_latch.rs` 新加 `pub fn save_estop_latch_clear(path)`：
  专为 `ClearEStop` 设计，**backup-first** 写入顺序：

  ```
  save_single_atomic(bak, false)            // backup 先写
      → save_single_atomic(primary, false)  // primary 后写
  ```

  不变量：

  - **bak 写失败** → primary 没动（仍 `"1\n"`）→ 重启 `load_estop_latch`
    走 primary `latched=true`（安全）。
  - **bak 成功 + primary 写失败** → primary 没动（rename 原子；仍 `"1\n"`）
    → 重启 `load_estop_latch` 走 primary `latched=true`。bak 的 `"0\n"`
    **不**会被 load 读到，因为 `load_estop_latch` 默认走 primary，只在
    primary `NotFound` / `InvalidData` 时才 fallback bak。
  - **两步全过** → primary + bak 都 `"0\n"` → 重启 load `latched=false`
    （正常 ClearEStop 成功路径）。

- `crates/robot-s0/src/clear_estop_action.rs::try_apply_clear_estop` 改调
  `save_estop_latch_clear(latch_path)`，不再调 `save_estop_latch(_, false)`。

- **EStop 路径继续用 `save_estop_latch(path, true)`（primary-first）不动**：
  EStop 是 `latched=true` 单调写入。primary-first 失败时 primary 至少
  写出 `"1\n"`（rename 原子），重启 load primary `latched=true`，安全
  语义满足。改 backup-first 反而要为 EStop 多写一次 bak，无收益。

### 单测

- `estop_latch::tests::save_estop_latch_clear_writes_both_files`
  （happy path：两文件 fsync 都写 `"0\n"`）
- `estop_latch::tests::save_estop_latch_clear_backup_failure_preserves_primary_latched`
  （**partial failure 锁测**，reviewer 拍板补：预置 primary + bak `= "1\n"`，
  把 `<latch>.bak` 替换成非空目录让 rename target 失败 →
  `save_estop_latch_clear` 返 Err，但 primary 仍 `"1\n"`，
  `load_estop_latch` 仍返 `Ok(true)`）
- `clear_estop_action::tests::clear_estop_partial_backup_failure_preserves_disk_latched_true`
  （高层 partial failure：`try_apply_clear_estop` 返 `FsyncFailed` 时
  `load_estop_latch` 仍返 `Ok(true)`，guard / executor 不动）

### 不改变的内容

- 不改 `docs/00_design.md` §6.6.1 文字（"fsync `latch=false`（primary + bak）"
  仍然成立，仅内部顺序变了；语义上等价）。
- EStop 路径 `save_estop_latch(path, true)` 不动。
- §6.7 #3 真宣称仍维持 v0.2 "代码层面到位 / 端到端测试留 v0.3" 降级口径
  （A1 全 land 后才升级；本登记只补 A1-2 fsync 原子性阻断）。
- §6.7 #4 真宣称同样不动（v0.2 "S0 重启不失忆 ✅ 真锁住" 边界保留；本
  修复消除 ClearEStop 失败时违反 #4 的窗口）。
- 不接 robot-rpc → S0 RR 入口（A1-3 范围）。
- 不撤 `--allow-empty-latch` CLI flag（A1-5 范围）。

### 审阅人

reviewer（用户）— A1-2 round 2 阻断：`save_estop_latch` primary-first
复用导致 ClearEStop fsync 失败时磁盘 latch 被错误清零，违反 §6.7 #4 +
A1-2 失败不变量。

---

## 2026-04-27 · `docs/00_design.md` §6.6.1 ClearEStop 三步顺序硬契约修正（**状态：已回写 / errata**）

### 背景

A1-2 评审 round 1 reviewer 阻断 5：复核 `docs/00_design.md` §6.6.1 时
发现顺序歧义。旧文本（line 1138）写的是：

> 再 fsync `latch=false`，更新内存 guard，调用 `clear_safety_latch()`

字面顺序把"更新内存 guard"放在 `clear_safety_latch()` **之前**，跟 A1-2
reviewer 拍板的实施级硬契约相反。会让实现端把 guard 翻转放在 executor
latch 清除之前，制造一个"内存 guard 已清、executor latch 未清、磁盘 latch
还是 true"的中间窗口；fsync 失败时尤其危险。

A1-2 reviewer 拍板的硬顺序是：

```
TorqueOffConfirmed gate
    → save_estop_latch(false)               // primary + bak fsync
    → executor_ctrl.clear_safety_latch()
    → guards.estop_latched = false
                                            → 返回 Ack
```

`clear_safety_latch()` 必须在 fsync 持久化**之后**、内存 guard 翻转
**之前**：

- 在 fsync 之前清 executor latch → fsync 失败时硬件已经被允许 enable，
  但磁盘仍记 `latch=true`，重启后退回 Safe → 状态机自相矛盾。
- 在内存 guard 翻转之后清 executor latch → 同 tick 内的 enable 路径会
  看到 guard=false 但 executor latch 仍 true，下个 tick 再清，扩大不
  一致窗口。

### 回写内容

- `docs/00_design.md` §6.6.1 line 1138 句子重写：
  - 旧：`...再 fsync latch=false，更新内存 guard，调用 clear_safety_latch()，最后返回 Ack`
  - 新：`...再 fsync latch=false（primary + bak），调用 clear_safety_latch()，最后翻内存 guard estop_latched=false，返回 Ack`
- 同段附"`clear_safety_latch()` 必须在 fsync 持久化之后、内存 guard 翻转
  之前"理由说明，避免后续实现 / 评审继续犯顺序错。

### 不改变的内容

- 不改 frozen schema (§5.2 / §5.3)。
- 不放宽 §6.7 #3。A1-2 行为代码尚未 land；§6.7 #3 真宣称仍维持 v0.2
  "代码层面到位 / 端到端测试留 v0.3" 降级口径。
- 不动 `docs/handoffs/v0.3_entry.md` §3.3（写于 v0.3 entry 钉边界时已经
  是正确顺序，本 errata 仅同步 `docs/00_design.md` §6.6.1）。
- 不宣称 A1-2 已实现 / 已锁住；本条目只记录契约修正本身。

### 审阅人

reviewer（用户）— A1-2 round 1 阻断 5：§6.6.1 顺序歧义修正。

---

## 2026-04-27 · `docs/00_design.md` RPC routing / auth + S1 action backend 术语澄清（**状态：已回写 / errata**）

### 背景

A1-2 开工前复核 `docs/00_design.md` 时发现三类会误导实现的旧口径：

1. §5.2 写 "按 RPC 类型 × `AuthContext.role` 查
   `configs/rpc_roles.toml`"，但 `AuthContext` 没有 `role` 字段；A1-1
   已拍板 gateway 用 token / 未来 mTLS fingerprint 解析 role，S0 不授权。
2. §5.5 写 "写接口转成 iceoryx2 request-response 发给 S1"，会误导
   `EStop` / `ClearEStop` 这类 safety action 也经 S1。A1 拍板要求
   `ClearEStop` 直接落 S0 action handler。
3. §4.1 / 附录 E 的 IK 表述容易让 Phase B 继续被误读成"必须先落
   IK/FK"。v0.3 最新口径是 policy/action backend + robot profile matrix：
   RL / IK / scripted controller 都是 backend。

### 回写内容

- `docs/00_design.md` §5.2 句子改成：gateway 先把
  `AuthContext.token` / 未来 mTLS fingerprint 解析为 role，再按 RPC 类型 ×
  role 查 `configs/rpc_roles.toml`；S0 端不做授权决策。
- `docs/00_design.md` §5.5 句子改成：普通 task / policy / config 写接口走
  S1；`EStop` / `ClearEStop` 等 safety-critical control action 直接落到
  S0 action handler，S1 不参与授权或清 latch。
- `docs/00_design.md` §4.1 模块地图把 S1 的动作来源改为
  `policy/action backend`，并把 `robot-kinematics` 标为可选 kinematics
  backend。
- `docs/00_design.md` §4.1 同步把 policy runtime 表述从 "In-Process only"
  改成 "InProcess / External"，避免 RL / policy-host 接入时误读为只能内嵌。
- 新增 `docs/00_design.md` §6.6.3 "RPC gateway routing / auth 术语澄清
  （errata）"。
- 新增 `docs/00_design.md` §6.6.4 "S1 action backend / IK-RL 术语澄清
  （errata）"：Phase B gate 是 S1 把高层 action 通过 robot profile 映射
  和安全限幅转换为 S0 joint command；IK/FK、RL、scripted controller 都
  是可插拔 backend。
- `docs/handoffs/v0.3_entry.md` §3.1 / §3.2 同步 `request_id` /
  `idempotency_key` 注释和 auth 口径。

### 不改变的内容

- 不改 `CommandMeta` / `ClearEStopCommand` wire 字段或字段编号。
- 不改 §5.3 `RobotControl` 方法集。
- 不放宽 §6.7 #3；A1 完成前仍不能宣称 `ClearEStop` 真闭环 / #3 真锁住。
- 不撤销 2026-04-22 "新增 IK 层"登记；只澄清 IK/FK 是可选 backend，不是
  Phase B gate。

### 审阅人

reviewer（用户）— 要求把 auth/routing/request_id 残留和 IK/RL 口径全部
修到文档。

---

## 2026-04-27 · v0.3 A1-1 (3/3) robot-rpc / tonic / prost / uuid / blake3 依赖白名单登记（**状态：登记**）

### 背景

A1-1 (3/3) commit `7c9362f` 引入了 robot-rpc crate 跟外部 RPC 依赖（tonic
0.12 / prost 0.13 / uuid 1.10 / blake3 / tonic-build 0.12 /
protoc-bin-vendored 3）。docs/CHANGELOG.md 2026-04-22 "workspace runtime
依赖白名单机制"要求新外部依赖在偏离登记里写明 scope + 边界，本条补登
记（reviewer A1-1 (3/3) round 2 阻断 3）。

### Scope

- **仅 robot-rpc crate** link `tonic` / `prost` / `uuid` / `tonic-build` /
  `protoc-bin-vendored`；其它 crate（robot-msg /
  robot-transport / robot-s0 / robot-s1 / robot-tests / robot-cli /
  robot-core / robot-health / robot-logging / robot-adapters/* /
  robot-rpc 之外的 v0.5+ 新 crate）一律不允许直接 link 上述 RPC / proto
  codegen 依赖。
- `blake3` 是 workspace 既有依赖（`robot-msg` / `robot-s0` 已使用）；
  本条只登记 robot-rpc 新增的 idempotency fingerprint 用途，不把 `blake3`
  收窄成 robot-rpc 专属依赖。
- **生产 release `s0_daemon` binary 0 robot_rpc / tonic / prost 符号**
  —— 已用 `strings target/release/s0_daemon | grep -oE "robot_rpc|tonic::|::tonic|prost::|::prost|robot::control"` 验证。

### 各依赖的引入理由

| 依赖 | 版本 | 用途 |
|---|---|---|
| `tonic` | 0.12 | gRPC server runtime（A1-3 真起 server 时；A1-1 仅生成 server trait） |
| `prost` | 0.13 | Protobuf runtime（tonic 必带） |
| `tonic-build` | 0.12 | proto codegen（仅 robot-rpc/build.rs；非 runtime） |
| `protoc-bin-vendored` | 3 | vendored protoc binary（避免 host 装 protoc；跟 vendor flatc 同款隔离策略） |
| `uuid` | 1.10 | UUIDv7 验证 `CommandMeta.request_id`（A1 fake token；correlation/audit） |
| `blake3` | workspace 已有（robot-msg 用） | idempotency fingerprint（reviewer A1-1 (3/3) round 2 阻断 1 修正：排除 request_id） |

### 不引入

- mTLS / OIDC / JWT 库（A1 fake token；推 v0.5+）。
- async runtime（tonic 默认 tokio；A1-1 (3/3) **不**起 server binary，
  tokio 不进 runtime path）。
- gRPC client（tonic-build `build_client(false)`；A1-3 按需开）。

### 影响面

- 不改 `docs/00_design.md` §4.2 robot-core 零 runtime 依赖原则。
- 不改 frozen FlatBuffers schema / iceoryx2 服务 attribute 体系。
- robot-rpc Cargo.toml 顶部注释跟本条登记互相引用。
- A1 land 后任何"在其它 crate link tonic / prost"的 PR 视为白名单违反，
  按 reviewer 复核流程否决。

### 审阅人

reviewer（用户）— A1-1 (3/3) round 2 阻断 3 闭环。

---

## 2026-04-27 · `docs/00_design.md` §5.2 `request_id` vs `idempotency_key` 角色 errata（**状态：已回写 / errata**）

### 背景

A1-1 (3/3) commit `7c9362f` 引入 robot-rpc 端 idempotency cache 时 reviewer
发现 `docs/00_design.md` §5.2 frozen 正文把 `CommandMeta.request_id` 注释为
"UUIDv7，幂等键"，这跟当前拍板和实现都冲突：

- 实现端 `crates/robot-rpc/src/idempotency.rs` 用 `idempotency_key` 作
  cache key。
- reviewer A1-1 (3/3) round 2 阻断 1 明确：`request_id` 是 correlation /
  audit-only，**每次 retry 必须改变**；`idempotency_key` 才是跨 retry
  稳定的真幂等键。
- A1-2 / A1-3 接班人若按旧注释把 `request_id` 当 cache key 会误杀合法
  retry（同 `idempotency_key` + 新 `request_id` 走 Conflict）。

### 回写内容

- `docs/00_design.md` §5.2 `CommandMeta.request_id` 注释从 "UUIDv7，幂
  等键" 改成 "UUIDv7；correlation / audit only（每次 retry 改变，详
  §6.6.2 errata）"。
- `docs/00_design.md` §5.2 `CommandMeta.idempotency_key` 注释从空白补成
  "幂等键（跨 retry 稳定；详 §6.6.2 errata）"。
- 新增 `docs/00_design.md` §6.6.2 "`CommandMeta.request_id` vs
  `idempotency_key` 角色澄清（errata）"：明文两者用途、retry 模式、
  fingerprint helper 必须排除 `request_id` / `timeout_ms`。

### 不改变的内容

- 不改 `CommandMeta` proto 字段 / 字段编号 / wire format。
- 不改 `RobotControl` 方法集。
- 不放宽 §6.7 #3。

### 审阅人

reviewer（用户）— A1-1 (3/3) round 2 阻断 2 闭环。

---

## 2026-04-27 · v0.3 Phase B：policy/action backend + robot profile matrix 口径澄清（**状态：解释登记**）

### 背景

`docs/handoffs/v0.3_entry.md` 初版把 Phase B 写成 "S1 真实现 +
IK/FK"，容易被误读为 v0.3 必须先落 IK/FK 才能继续。用户明确指出：
后续可能用 RL，不一定需要 IK/FK；同时希望根据不同机器人构型做编译矩阵，
例如 type1 = Feetech 电机 + RL，type2 = DM 电机 + IK。

### 澄清内容

Phase B 的 gate 改为：

> S1 coordinator 真实现 + policy/action interface + robot profile matrix。
> IK/FK、RL、scripted controller 都是可插拔 action backend；v0.3 只要求
> 至少一个 backend 能经 S1 安全约束后跑通到 S0 joint command，不要求
> IK/FK 必须落地。

`docs/handoffs/v0.3_entry.md` §2 已同步更新依赖图与硬约束：

- `Phase B — S1 真实现 + IK/FK` 改成 `Phase B — S1 真实现 +
  policy/action profile matrix`。
- B2 改为 `policy/action interface + robot profile matrix`。
- B3 改为 "至少一个 action backend 跑通到 S0 joint cmd (RL / IK /
  scripted)"。
- 新增 build/deploy profile matrix 约束：一个生产 binary 应明确选择
  motor adapter（如 `feetech` / `dm`）、action backend（如 `rl` / `ik` /
  `scripted`）、robot model（如 `xlerobot` / `piper` / custom）与 command
  mode。
- runtime config 只能选择已编译能力；配置与编译 profile 不匹配必须启动期
  fail fast。
- S0 / adapter 不得知道 action 来源；RL / IK / scripted 输出都必须先在 S1
  归一成安全后的 joint command。
- v0.3 不开放 raw torque / raw current policy 直通 S0。

### 不改变的内容

- 不改 `docs/00_design.md` frozen schema；模块图措辞后续已通过
  2026-04-27 "RPC routing / auth + S1 action backend 术语澄清" errata
  回写为 policy/action backend 口径。
- 不撤销 2026-04-22 "新增 IK 层"登记；`robot-kinematics` 仍是可选
  kinematics backend，但不再是 Phase B gate。
- 不放松 Phase A gate：A1 + A2 仍必须在 Phase B 之前 land。
- 不允许 RL / policy-host 绕过 S1/S0 安全链路。

### 审阅人

reviewer（用户）— 明确 v0.3 可能用 RL，不应把 IK/FK 写成硬前置；要求把
Phase B 文案改成 policy/action backend + robot profile matrix。

---

## 2026-04-27 · `docs/00_design.md` ClearEStop control action 术语澄清（**状态：已回写 / errata**）

### 背景

v0.3 A1 开工复核时，reviewer 多次指出 "`ClearEStop RPC`" 的字面写法容易
被误读为 "只有 gRPC 这一种传输协议能解除 EStop"，或者把 `EStop` 与
`ClearEStop` 两条相反动作混成一条链路。此前
`docs/CHANGELOG.md` 2026-04-26 "v0.3 entry：`ClearEStop RPC` 文案的控制
动作解释" 已做解释登记；本次把该解释以 errata 形式回写到冻结正文，避免后
续实现阶段继续犯错。

### 回写内容

- `docs/00_design.md` §5.2 `ClearEStopCommand` 注释补指针：字段仍是
  `CommandMeta meta + string confirmation`，但读者需看 §6.6.1 的
  `ClearEStop action` 澄清。
- `docs/00_design.md` §5.3 `rpc ClearEStop` 注释补充：这是
  `ClearEStop action` 的首个 transport，且仍独立于 `ClearError`。
- `docs/00_design.md` §6.6 EStop latch 规则从"只能被 `ClearEStop RPC`
  解除"澄清为"只能被 `ClearEStop control action` 解除（首个 transport 是
  maintainer+ 的 `rpc ClearEStop`）"。
- 新增 `docs/00_design.md` §6.6.1 "ClearEStop 术语澄清（errata）"：
  - `ClearEStop` 是安全控制动作，不是 gRPC 传输协议本身。
  - Web 管理界面 / 本地维护面板等未来入口必须复用同一个 action handler
    与同一套 maintainer+ / confirmation / idempotency /
    `TorqueOffConfirmed` / fsync / audit guard。
  - `EStop` 与 `ClearEStop` 是相反方向：`EStop` 置 latch=true 并停输出；
    `ClearEStop` 授权后确认 `TorqueOffConfirmed`，fsync latch=false，
    更新 guard，调用 `clear_safety_latch()`，再返回 `Ack`。
  - 物理急停按钮松开 / 旋出、`rm latch + restart`、deploy provision 都不
    等价于 `ClearEStop`。

### 不改变的内容

- 不改 `CommandMeta` / `ClearEStopCommand` wire 字段。
- 不改 `RobotControl` 方法集。
- 不放宽 §6.7 #3；A1 完成前仍只能宣称"代码层面到位"，不能宣称
  "`ClearEStop` 真闭环 / #3 真锁住"。
- 不引入新的 transport；A1 仍按 v0.3 entry 拍板，先落
  `robot-rpc` 的最小 `ClearEStop` endpoint。

### 审阅人

reviewer（用户）— 要求将 `ClearEStop control action` 写入 design errata，
避免 A1 实现阶段再次误读。

---

## 2026-04-26 · v0.3 A2：§6.7 #2 "systemd 是最外层看门狗" 真宣称升级（**状态：偏离 / 实施完成 + 真机 PASS**）

### 背景

`docs/handoffs/v0.3_entry.md` §1.2 + §4.6 拍板：v0.2 收尾期 §6.7 #2
"systemd 是最外层看门狗" 降级宣称为"代码层面到位 / 没端到端测过"
（`LivenessObserver` 真接 4 worker `tick_count`，但 worker-stall →
`WatchdogSec` → systemd 真重启的完整路径没端到端验证）。v0.3 A2
（freeze hook + control plane + systemd e2e smoke）落地 + 本机 / Orin
真机 PASS 后，**升级**到"真锁住"。

### 升级登记内容

§6.7 #2 完整路径在 v0.3 A2 后**端到端真锁定**：

> S0 内部任一 worker (ipc / rt-or-serial / pub / io) cap-deadlock 或停
> 滞 → `LivenessObserver` 观察对应 `tick_count` 不增 → watchdog 主循
> 环跳 `sd_notify(WATCHDOG=1)` → systemd `WatchdogSec=2s` 真超时 →
> `Restart=on-failure` 拉起新 daemon → 新 daemon 完成 `sd_notify(READY=1)`
> 重新进入 active 状态。

证据链：
1. **代码层**：[`crates/robot-s0/src/systemd_notify.rs::LivenessObserver`](
   ../crates/robot-s0/src/systemd_notify.rs)（v0.2 reviewer round 4
   commit `4222df5`）+ [`crates/robot-s0/src/freeze_hook.rs`](
   ../crates/robot-s0/src/freeze_hook.rs)（v0.3 A2-1 commit `ca42a29`，
   5 worker tick loop 顶端 `cfg(test, feature = "test-freeze-hook")`
   spin hook）+ [`crates/robot-s0/src/freeze_control.rs`](
   ../crates/robot-s0/src/freeze_control.rs)（v0.3 A2-2 commit `17ce63c`，
   test-only iceoryx2 topic + control thread 路由 + daemon 内 monotonic
   clock 算 absolute `freeze_until_ns`）。
2. **端到端 smoke**：[`crates/robot-tests/src/bin/freeze_systemd_smoke.rs`](
   ../crates/robot-tests/src/bin/freeze_systemd_smoke.rs)（v0.3 A2-3
   commit `04d14e1` + reviewer round 2 `91bb8df` + Orin 兼容 `b33b5c4`）：
   起 user-mode systemd unit + freeze ipc_thread 5s → 期望 PID 切换 +
   `is-active` 二次确认 + 4 条 journal 硬断言（`withholding sd_notify`
   ≥ 1 / `s0_daemon starting` ≥ 2 / `freeze_control thread ready` ≥ 2 /
   `state=Ready` ≥ 1）。
3. **真机回归**：FRIDAY workstation 本机 5/5 PASS + Orin AGX 1 round
   PASS（详 [`docs/handoffs/2026-04-26_6.md`](handoffs/2026-04-26_6.md)
   §5 完整 journal 时序 + 数据对照表）。
4. **跨发行版兼容性**：smoke `journalctl` query 走系统 journal
   `_SYSTEMD_USER_UNIT=<unit>` matcher（commit `b33b5c4`），兼容启用 /
   不启用 user journal namespace 两种发行版配置。

### 不在本次升级范围

- §6.7 #3 "EStop 不可清为失忆态"：v0.2 收尾代码层面到位
  （`MissingLatchPolicy::Strict` + `--allow-empty-latch` opt-in），
  完整真宣称留 v0.3 A1 ClearEStop control-plane slice。
- 完整 RPC 网关：A2 的 freeze 控制路径**仅** test-only，**不**走
  `robot-rpc` / 生产 schema；A1 才切第一个生产 control endpoint。

### 影响面

- 不改 `docs/00_design.md` 正文（"systemd 是最外层看门狗" 字面表达不
  变；本条仅升级实施真锁定状态）。
- 不改 frozen FlatBuffers schema / `TopicClass` 体系（`/<robot_id>/test/
  freeze` 是 cfg-gated test-only topic，跟生产命名空间 isolation）。
- 同步更新 [`scripts/systemd/README.md`](../scripts/systemd/README.md)
  "验证 systemd 重启路径" 段从"**仅** Restart=on-failure 已锁；watchdog
  cap-deadlock **留 v0.3**" 改成 "**全套已锁**"。
- [`crates/robot-s0/src/systemd_notify.rs`](../crates/robot-s0/src/systemd_notify.rs)
  模块 doc "Liveness-gated" 段补 "v0.3 已端到端验证" 引用 + 证据链路。
- 模板 [`scripts/systemd/xlerobot-s0-freeze-test.service`](../scripts/systemd/xlerobot-s0-freeze-test.service)
  L49 字面残留 `state=Safe` 改成 `state=Ready` 跟 smoke 实际硬断言对齐
  （reviewer A2-3 round 1 留的非阻断 doc 残留）。

### 审阅人

reviewer (用户) — A2-1 / A2-2 / A2-3 round 1-3 全部放行 + A2-4 Orin 1
round 真机 PASS 拍板进 A2-5。

---

## 2026-04-26 · v0.3 entry：`ClearEStop RPC` 文案的控制动作解释（**状态：偏离**）

### 背景

`docs/00_design.md` §6.6 / §6.7 写的是 "`EStop` latch 只能被
`ClearEStop` RPC 解除"。v0.3 entry handoff 复核时指出：按字面把安全语义
绑死在 **RPC 这一种传输协议** 上过窄，也容易跟物理急停按钮松开、Web 管理
界面、本地维护面板等入口混淆。

### 解释登记

本条是对冻结正文 `RPC` 字样的解释登记，不是新增第四类 CHANGELOG 状态。

冻结设计正文的安全语义解释为：

> `EStop` software latch 只能被显式授权、可审计的 `ClearEStop` control
> action 解除；RPC / Web 管理界面 / 本地维护面板只是 transport / UI 入口，
> 都必须落到同一个 `ClearEStop` action，并走同一套 maintainer+ /
> confirmation / idempotency / `TorqueOffConfirmed` / fsync 双写 / audit
> guard。

这不是放宽 §6.7 #3，而是避免把"解除 EStop 的安全动作"误读成"只能通过
gRPC 这一条网络路径"。物理急停按钮松开 / 旋出只表示**物理 EStop 装置复位**；
不得自动清 `EStop` software latch，更不得自动 enable / restart。解除
software latch 仍必须走 `ClearEStop` control action；解除后也只允许后续
独立 `Enable` / `Start`，不能自动重启。

### 对 v0.3 entry 的影响

- A1 命名为 `ClearEStop control-plane slice`，只切最小外部入口：
  `robot-rpc` 的 `ClearEStop` endpoint + S0 内部 `Event::ClearEStop`
  闭环；**不**提前落完整 RPC 平台。
- Phase B 的 `robot-rpc full gateway` 才扩展其它 §5.3 RPC / task /
  stream / S1 coordinator 交互。
- Web 管理界面 / 本地维护面板未来接入时必须复用同一个 `ClearEStop`
  action，不允许开旁路。
- `rm latch + restart` / deploy provision 仍不等价于 `ClearEStop`。

### 影响面

- 不改 `docs/00_design.md` 正文，不改 frozen schema。
- 本条作为解释登记；若未来决定把 `docs/00_design.md` 正文从 "RPC" 回写成
  "control action"，另走"回写登记"。

### 审阅人

reviewer (用户) 待复核。

---

## 2026-04-26 · v0.2 收尾 reviewer round 5 否决 — runbook / 文档跟代码语义同步（**状态：偏离 / 实施完成 + 真机 PASS**）

### 背景

[`docs/CHANGELOG.md`](CHANGELOG.md) 上一条（commit `d40bfee`，round 4
阻断闭环 + 宣称降级）登记后 reviewer **再次复核否决**：代码层面 round 4
的修我认可，但仓库仍有真机 runbook / 安全宣称跟代码语义不一致，按审核
规则算阻断。

具体三条：
1. **systemd 首次部署 runbook 跟 strict latch 冲突**（高）：
   `scripts/systemd/README.md` 安装步骤 + bus2 unit 顶部注释还是"创建
   目录后直接 start"，没 latch provision 也没 opt-in。代码现在默认
   `MissingLatchPolicy::Strict`，干净机照 runbook 跑会立即 failed。
2. **systemd watchdog runbook 仍过度宣称 §6.7 #2**（高）：
   round 4 handoff 已降级（"freeze hook 留 v0.3"），但 README + bus2
   unit 注释里 "kill -9 MainPID 锁定 §6.7 #2 真机硬门槛" 还在，回到了
   前一轮被否决的过强宣称。
3. **`estop_latch` 公共文档仍写旧语义**（中）：
   实现已 strict fail-closed，但模块 / 函数 doc 还写"文件不存在 = not
   latched / 两份都不存在返 false"，安全语义文档反向。

### 修复（commit `a9f8dc7`）

#### 阻断 1：runbook + unit 首次部署

- `scripts/systemd/README.md` "首次部署 — latch provision **必读**" 段：
  - **推荐流程**：直接 `printf '0\n' | tee` provision primary + bak 双
    文件后正常 start（strict 不阻断）。
  - **替代流程**：drop-in `--allow-empty-latch` 一次性启停产生 latch=0，
    撤掉 drop-in，strict 接管 + 跑"rm latch → start fail" 验证。
  - 全栈部署改成"跑两遍上面推荐流程"。
- bus2 / bus1 unit 顶部注释指向 README，明确不能跳过老式"目录 + start"。

#### 阻断 2：watchdog runbook 宣称降级

- README 标题改成 "**仅** `Restart=on-failure` 已锁；watchdog
  cap-deadlock **留 v0.3**"，开头加 callout 明确：
  - 锁定的是"进程死亡 → `Restart=on-failure`"。
  - §6.7 #2 完整（worker-stall → `WatchdogSec` → 真重启）留 v0.3
    `cfg(test)` freeze hook。
  - 当前 watchdog 已是 liveness-gated（已实施）但端到端真重启路径未测。
- bus2 / bus1 unit 顶部同款降级。

#### 阻断 3：`estop_latch` 公共文档同步 strict 默认

- 模块 doc "文件格式" 段改"按 [`MissingLatchPolicy`] 决定"。
- "读 fallback 自愈" 段改 `NotFound` 分流（`Strict` 返 `NotFound` err；
  `AllowEmpty` 返 `Ok(false)`）。
- `load_estop_latch` 函数 doc 明示走 `Strict`（生产默认）+ `NotFound`
  err 含 "需 `ClearEStop` RPC（v0.3）解除，或 `--allow-empty-latch`
  opt-in" 提示 + Errors 段加 `NotFound` 条目。

### 真机回归（Orin AGX 干净机起点）

按 README 两个流程**端到端跑了一遍**：

```
=== README 推荐流程 §2-§6（bus2）===
- install -d /var/lib/robot/xlerobot_bus2_full ✓
- printf '0\n' | tee provision primary + bak ✓
- systemctl start → is-active=active
- journal: "initial estop_latched latched=false" + "state":"Ready" 稳态

=== README 替代流程 §3f：strict 守门验证 ===
- rm latch primary + bak
- systemctl start → ERROR "primary AND backup both missing; fail-closed
  (need --allow-empty-latch opt-in or ClearEStop RPC in v0.3)"
  + Restart=on-failure × StartLimitBurst=5 后 unit 进 failed
- strict 守门真挡住 "rm latch && restart"，符合预期
```

### 影响面

- 文档：`scripts/systemd/{README.md, xlerobot-s0-bus1.service,
  xlerobot-s0-bus2.service}` + `crates/robot-s0/src/estop_latch.rs`
  模块/函数 doc。**不动代码语义**（commit `4222df5` + `e15cd41` 已
  实施 strict + liveness-gated）。
- 本 CHANGELOG 条目 + `2026-04-26_5.md` handoff §2.4 + §2.5 追加 round 5
  闭环 + 真机回归。

### 审阅人

reviewer (用户) ✓ — round 5 三条文档同步阻断全数闭环 + 真机端到端 runbook
跑通 + strict 守门验证。

---

## 2026-04-26 · v0.2 收尾 reviewer round 4 阻断闭环 + 安全宣称降级（**状态：登记 / 实施完成 + 真机 PASS**）

### 背景

[`docs/CHANGELOG.md`](CHANGELOG.md) 上一条 (commit `3a1cca2`) v0.2 收尾文档登
记后 reviewer **复核否决** "§6.7 #2/#3/#4 全部锁住"宣称。给两条具体代码
语义债：

1. **阻断 1（高 / §6.7 #2）**：`spawn_watchdog_thread` 是独立线程无条件
   `WATCHDOG=1`，只要本线程还活就喂 systemd → RT/SerialRT/ipc/pub 任意
   worker 卡死也照样喂，等价于禁用 watchdog；`docs/00_design.md:1134`
   "内部 RT watchdog 失效 → sd_notify 超时 → systemd 重启 S0" 不成立。
2. **阻断 2（高 / §6.7 #3）**：`load_estop_latch` 在 primary + bak 都不存
   在时返 `Ok(false)`，`rm latch && restart` 等价于"清空 EStop"，跟"只能
   ClearEStop RPC 解除"冲突。

### 修复（commit `4222df5` + `e15cd41`）

**阻断 1 修复**：`systemd_notify` 加 `LivenessObserver` + `spawn_watchdog_thread`
接受 `Vec<LivenessObserver>`。每周期对所有 observer 调 `tick` 闭包 + 跟
上次 snapshot 比较；任一停滞就**不**发 `WATCHDOG=1`，systemd
`WatchdogSec` 超时 → `Restart=on-failure` 拉起。`s0_daemon` main 把 4
worker（ipc / rt-or-serial / pub / io）的 `tick_count` 收集传给 watchdog。
启动 grace 一周期让 worker 完成 first tick。**watchdog 主循环 sleep 移到
compare 之前**避免第一次 iteration 0 时间过去误报 stalled（commit
`e15cd41`）。

**阻断 2 修复**：`estop_latch` 加 `MissingLatchPolicy { Strict, AllowEmpty }`
+ `load_estop_latch_with_mode(path, mode)`。生产 daemon 默认走 `Strict`
（fail-closed `NotFound` err，daemon 拒启动 + tracing::error 报"需
`--allow-empty-latch` opt-in 或 v0.3 ClearEStop RPC"）。新增
`s0_daemon --allow-empty-latch` CLI flag（default false）+
`crash_restart_smoke` 工具自动加该 flag（test 场景每次清空 latch 目录）。

### 锁测

- `systemd_notify::tests::watchdog_pings_when_all_observers_alive` /
  `watchdog_withholds_ping_when_observer_stalled`（双向锁）
- `estop_latch::tests::load_nonexistent_strict_returns_not_found_err` /
  `load_nonexistent_allow_empty_returns_false` /
  `load_returns_not_found_when_both_missing_strict`
- `cargo test -p robot-s0 systemd_notify -- --test-threads=8` × 20 轮
  全过（12 测试 / 0 flake，包括 reviewer round 3 修复的 env race）。

### 真机回归（Orin AGX）

- **happy path 8s**：daemon active + state=Ready；**0 stalled warning**；
  main PID 不变 → systemd 没误重启。
- **fail-closed 验证**：无 latch + 无 flag → systemd unit failed +
  journal ERROR `"primary AND backup both missing; fail-closed"`。
- **opt-in 验证**：drop-in unit override 加 `--allow-empty-latch` →
  unit active + state=Ready。
- **crash_restart_smoke 5/5 PASS**（从未退化）。

### v0.2 收尾安全宣称（按 reviewer 边界，**已降级**）

| §6.7 不变量 | 旧（commit 3a1cca2）误宣称 | 新真实状态 |
|---|---|---|
| #2 systemd 是最外层看门狗 | "systemctl kill -9 → 4s 重启" 锁住 | **代码层面**满足：watchdog 真观测 4 worker tick；任一卡死 → systemd 超时重启。**未做**：daemon 内人为冻 worker（cfg(test) freeze hook）+ 测真重启路径，留 v0.3 测试基础设施。 |
| #3 EStop 不可清为失忆态 | "crash_restart_smoke 5/5" 锁住 | **代码层面**部分满足：fail-closed default + `--allow-empty-latch` opt-in 守门。**未做**：真"只能 `ClearEStop` 解除"语义，留 v0.3 RPC 落地。 |
| #4 S0 重启不失忆 | "fsync 双写 + crash 5/5" 锁住 | **真锁住**（fsync 双写 + 重启读回；smoke 稳定）。 |

### 影响面

- 代码：`crates/robot-s0/src/{systemd_notify,estop_latch,bin/s0_daemon}.rs`
  + `crates/robot-tests/src/bin/crash_restart_smoke.rs`
- 文档：本 CHANGELOG 条目 + `2026-04-26_5.md` handoff +
  `handoffs/README.md`。
- 不动 systemd unit 文件（已在前次 commit 配齐）。
- 依然不阻塞 v0.3 进入；v0.3 第一个 milestone 完成 `ClearEStop` RPC 即可
  把 §6.7 #3 真完整闭环。

### 审阅人

reviewer (用户) ✓ — round 4 给的边界 "代码语义 fail-closed + watchdog
真观测 worker" 实施到位，"systemd 真重启路径 / ClearEStop-only 解除"
完整锁定留 v0.3。

---

## 2026-04-26 · v0.2 收尾 —— Step 9 + systemd 双 bus + #16 partial + reviewer 三轮阻断闭环（**状态：登记 / 实施完成 + 真机 PASS**）

### 背景

[`docs/handoffs/2026-04-26_3.md`](handoffs/2026-04-26_3.md) Stage 2 真
机闭环之后，按 reviewer 命令推进 v0.2 收尾的三件事 —— **Step 9 EStop
crash/restart 测试套**（§6.7 不变量 #3 + #4 真机锁定）+ **systemd
Type=notify 双 bus 集成**（§6.7 不变量 #2 systemd 是最外层看门狗 落地）+
**task #16 eprintln → tracing 迁移**（lib + 生产 daemon ~58 sites；CLI
tool 故意保留）。reviewer 三轮 review 各点出阻断，每轮全数修完真机回归。

### 验证结果（2026-04-26）

详见 [`handoffs/2026-04-26_4.md`](handoffs/2026-04-26_4.md) §3-§4。
关键证据：

- **Step 9**：Orin 真机 `crash_restart_smoke` 5/5 PASS。SIGKILL daemon
  后 latch primary+bak 都 = "1"；重启 daemon 按结构化字段断言
  `initial estop_latched` + `latched=true`。
- **systemd Type=notify**：bus1 + bus2 两 daemon 同时 active；宿主
  mock_s1 + 双 robotctl watch（A=20 / B=20 sample）+ robotctl estop 跨
  进程通信全通；kill -9 daemon 4s 内 `Restart=on-failure` 自动拉起新
  PID。
- **task #16 partial**：daemon 在 systemd
  `Environment=ROBOT_LOG_FORMAT=json` 下 journald 全部输出走 JSON
  Lines，所有结构化字段（`robot_id` / `motor_id` / `joint` / `state` /
  `latched` ...）可索引。

### reviewer 三轮否决 + 修复链路

#### Round 1 阻断（commit `2a1640e`）

1. **systemd `PrivateTmp=yes` 隔离 iceoryx2 跨进程**（高）：daemon 私
   有 /tmp 让宿主 mock_s1 / robotctl 看不到 service。修：删除
   `PrivateTmp=yes`，注释明确权衡。
2. **`crash_restart_smoke` 被 task #16 tracing 迁移打坏**（高）：旧 grep
   `"initial estop_latched = true"` 失效。修：`log_contains_initial_latched_true`
   按结构化字段（message + `latched=true|"latched":true`）+ 7 条 unit
   test（含**回归锁** 钉死旧字串）。
3. **`systemd_notify` 单测并发 env mutation 不稳**（中）：`--test-threads=8`
   循环 20 轮第 7 轮失败。修：拆 `send_message_to(payload, path)` +
   `spawn_watchdog_thread_with_usec(stop, usec)` 内部 helper 不读 env；
   剩余 env-探测测试加全局 `Mutex<()> ENV_LOCK` 串行化。

#### Round 2 阻断（commit `f88e2ab` + `df6e207`）

4. **`ProtectSystem=strict` 让 iceoryx2 `NodeBuilder::create` panic
   InternalError**：reviewer round 1 删 PrivateTmp 后 daemon 在 systemd
   下还是 panic。Bisect 找出 `strict` 让 /var 整体只读，iceoryx2 在
   /var 写其它（非 ReadWritePaths）路径失败。修：降到 `ProtectSystem=full`
   /var 可写、/usr /boot /efi /etc 仍只读。
5. **`crash_restart_smoke` Orin 真机 5/5 FAIL**：reviewer round 1 修了
   grep 仍然 fail。Root cause 双重：
   - tracing-subscriber 默认输出 **stdout 不是 stderr**，只 piped stderr
     拿不到 daemon 启动 banner；
   - 即使 piped 也带 ANSI 转义码（`latched\x1b[0m\x1b[2m=\x1b[0mtrue`
     ≠ `latched=true`）。
   修：抓 stdout + stderr 合并；env 注入 `ROBOT_LOG_FORMAT=json + NO_COLOR=1`
   关 ANSI。

#### Round 3 残留（commit `e2e7e30`）

6. `scripts/systemd/xlerobot-s0-bus2.service` 顶部注释残留 "sleep 5 等
   daemon 进入 Enabled.Active"，跟 README 已修的 "sleep 3 等 Ready" 口
   径不一致（unit 只启 S0，没 S1 心跳进不了 Active）。修：unit 注释同
   步成跟 README 一致。

### 影响面

- **代码**：
  - 新模块 `crates/robot-s0/src/systemd_notify.rs`（~150 行 + 10 单测）
  - 新 binary `crates/robot-tests/src/bin/crash_restart_smoke.rs`（~580
    行 + 7 matcher 单测）
  - `crates/robot-s0/src/bin/s0_daemon.rs`：systemd_notify 集成 + 大量
    eprintln → tracing
  - `crates/robot-adapters/feetech/src/{adapter,factory,error}.rs`：
    eprintln → tracing
  - `crates/robot-s0/src/{rt_thread,estop_latch,watchdog,config_loader}.rs`：
    eprintln → tracing（rt_thread 含 RT 边界注释）
- **配置 / 部署**：
  - `configs/xlerobot_bus1_full.toml`（新；左臂+头部 8 motor）
  - `scripts/systemd/xlerobot-s0-bus1.service`（新）
  - `scripts/systemd/xlerobot-s0-bus2.service`（新；ProtectSystem=full）
  - `scripts/systemd/README.md`
- **文档**：本 CHANGELOG 条目 + `2026-04-26_4.md` handoff +
  `handoffs/README.md` 列表。

### 审阅人

reviewer (用户) ✓ —— 三轮 review，三轮全数修完真机回归通过。

---

## 2026-04-26 · task #15 Stage 2 真机闭环全过 + §5.1 known-issue 真机撤销（**状态：登记 / 实施完成 + 真机 PASS**）

### 背景

[`docs/handoffs/2026-04-26_2.md`](handoffs/2026-04-26_2.md) Stage 2 软件
就绪 + Heartbeat QoS 修复 + §2.2 Operating_Mode + EEPROM unlock 序列修
正落地后，本场把全部交付物上 Orin AGX `10.0.1.41` 真机 bus2（9 motor）
端到端验证。

### 验证结果（2026-04-26）

详见 [`handoffs/2026-04-26_3.md`](handoffs/2026-04-26_3.md) §2。**§2.1 ping
→ §2.2 Operating_Mode 切换 → §2.3 baseline → §3 smoke → §4 ballet（含
双 watch 同跑回归）→ §5 EStop → §6 base 不动锁测 → §7 latch 重启 / 删 /
2nd EStop 全过**。关键证据：

- §2.2 base 3 motor `Operating_Mode` 1 → 0 五步序列（Torque_Enable=0 →
  Lock=0 → Operating_Mode=0 → Lock=1 → readback assert）一次写入即生效。
- §4 双 watch 同跑各拿 **45 sample**（旧实现下 B 永远 0 sample）→
  Heartbeat QoS 修复 (`TopicClass::Heartbeat` max=8) 真机回归 PASS。
- §5 EStop 后 9 motor `Torque_Enable` 全部 1 → 0（broadcast 覆盖 base
  motor 7/8/9，不只是 right arm）。
- §5 latch primary+bak 双文件 fsync 实测 188-255 ms 内落盘（daemon 仍
  alive），与 Stage 1 §5 实测一致；阻断 1 修复在第二次 EStop 路径仍稳。
- §6 base 3 motor `ΔPos = 0/0/0`（frozen mask 真冻死）。
- §7 latch=1 重启 daemon 主动"not auto-sending Event::Enable" + state
  全程 Safe；rm latch 后恢复 Ready/Enabled，2nd EStop 链路同款。

### 撤销 / 回写

- `docs/handoffs/2026-04-26.md` §5.1：横幅升级"待 Stage 2 回归"→"✅ 已撤
  销"，列出真机证据（Stage 2 §4 双 watch 各 45 sample）。
- `docs/handoffs/2026-04-26_bus2_runbook.md` 附 A 表 + 附 C.1：横幅同步
  升级 ✅ 已撤销 + PASS 真机证据指向 [`2026-04-26_3.md`](handoffs/2026-04-26_3.md)
  §2.5。
- `docs/handoffs/2026-04-26_bus2_runbook_stage2.md` 顶部 ⏳ → ✅ Stage 2
  真机闭环全过 + 列出 5 条关键证据。
- `docs/handoffs/README.md` 加 2026-04-26（再续）行指向新 handoff。

### 影响面

- 不动代码（Stage 2 软件已在前序 commit 落地，本批次纯文档收口）。
- 文档：本 CHANGELOG 条目 + Stage 1/2 runbook 顶部状态升级 + handoff
  `2026-04-26_3.md` 新建 + handoffs/README.md 列表追加 + Stage 1 handoff
  `2026-04-26.md` §5.1 横幅 PASS 升级。

### 审阅人

reviewer (用户) ✓ —— 用户授权 "你替我在真机上跑"，本场全部 7 段 + 双
watch 回归实测在 Orin 上跑完。

---

## 2026-04-26 · runbook §5.1 known-issue 撤销 —— Heartbeat QoS 多订阅（**状态：偏离 / 实施完成**）

### 背景

`docs/handoffs/2026-04-26.md` §5.1 把 `robotctl watch` 第二个 sub 拿不到
sample 归因为 "iceoryx2 0.8.1 single-service multi-subscriber attach
race"（框架 bug）。reviewer（用户）2026-04-26 复盘时直接否决该归因，定位
到我们自己的 QoS 分类错误：所有 heartbeat publisher / subscriber 借用
`TopicClass::S1CmdVel`（`crates/robot-transport/src/iceoryx2/qos.rs`
`max_subscribers=1`），第二个 subscriber attach 实际被 iceoryx2 拒绝。
这条对 `S1CmdVel` 单订阅语义本身没问题（cmd_vel 真的只有 S0 一个消费
者），错在被 heartbeat 蹭用。

### 决定

新增独立 [`TopicClass::Heartbeat`](../crates/robot-transport/src/iceoryx2/qos.rs)
（`max_subscribers=8` + `safe_overflow=true` + `history=4` + `buffer=4`），
所有 heartbeat 收发站点（`pub_thread` / `s0_daemon` / `ipc_thread` /
`robotctl` / `mock_s1` / `serial_rt_e2e` / `iceoryx2_rt_baseline`）一次
性迁过去。`S1CmdVel` 保留单订阅语义（max=1）专给真正的 cmd_vel 用。

### 锁测（防回归）

- `crates/robot-transport/src/iceoryx2/qos.rs` 新增三条 unit test：
  - `observer_classes_support_multi_subscriber`：`Heartbeat` / `S0State` /
    `Sensor` / `SafetyEvent` / `TaskEvent` 必须 `max_subscribers >= 8`。
  - `heartbeat_is_drop_old`：`Heartbeat` 必须 `safe_overflow=true` +
    `subscriber_max_buffer_size >= 2`。
  - `safe_overflow_semantics` 扩列把 `Heartbeat` 加入 latest-wins 集合。
- `crates/robot-transport/tests/iceoryx2_roundtrip.rs` 新增两条端到端：
  - `heartbeat_class_supports_two_subscribers_receiving_same_publish`：
    同 service 双 sub 都收到同一帧 publisher（钉死多订阅可达性）。
  - `s1_cmdvel_class_rejects_second_subscriber`：反向证明
    `S1CmdVel` `max_subscribers=1` 第二个 sub create 真的被拒，否则
    前一个测试就退化为恒真。

### 影响面

- `crates/robot-transport/src/iceoryx2/qos.rs`：新增 `TopicClass::Heartbeat`
  + 三条 unit test。
- 迁移站点（pub + sub 两端必须同 class，否则 service QoS 不匹配 open
  失败）：`crates/robot-s0/src/{pub_thread.rs,ipc_thread.rs,bin/s0_daemon.rs}`
  + `crates/robot-s0/tests/serial_rt_e2e.rs`
  + `crates/robot-cli/src/bin/robotctl.rs`
  + `crates/robot-s1/src/bin/mock_s1.rs`
  + `crates/robot-tests/src/bin/iceoryx2_rt_baseline.rs`。
- handoff `docs/handoffs/2026-04-26.md` §5.1 已追加"已修复（本 commit）—
  非框架 bug，QoS 分类错误"指向 follow-up。

### 审阅人

reviewer (用户) ✓ — 假设由用户提出，本 commit 锁住。

---

## 2026-04-26 · `docs/00_design.md` §8 tracelog 格式硬契约（**状态：登记 / 已回写**）

### 背景

§8 原文只一句 "`tracing` + `tracing-subscriber`，结构化 JSON，按进程分
文件到 `/var/log/robot/<robot_id>/`"，没把 tracelog 格式写成硬契约
（结构化字段 / Debug-vs-Display / 双时间戳栈 / 双格式模式 / RT 禁 log
/ 自动 init）。`docs/logging.md` 早已落地权威字段表与级别守则，但 §8
主线没引用，导致评审 / 上手时容易漏。

### 决定

§8 第一条扩成 7 条硬契约骨架，明确：
1. 结构化字段，禁 format-arg message 拼接。
2. `error!(?err, ...)` 用 Debug 捕获 thiserror 结构。
3. human + JSON 双模式（`ROBOT_LOG_FORMAT=json` 切换）；时间戳同时给
   wall clock RFC3339 UTC + monotonic `[+秒.纳秒s]`。
4. `RUST_LOG` `EnvFilter` 标准过滤。
5. bin 自动 init（`#[ctor::ctor]`），lib 不依赖 `robot-logging`。
6. RT thread 内禁所有 `tracing::*!`（包括 literal）—— 走 atomic
   counter / lock-free ring buffer，non-RT 线程定期采样。
7. 字段表 / 级别守则 / 单位约定在 [`docs/logging.md`](logging.md)
   作为权威，新增字段先动那张表 + 登记 CHANGELOG。

### 影响面

- `docs/00_design.md` §8 第一条扩展（其余条目原样保留）。
- `docs/logging.md` 不动（已与新硬契约一致；本回写仅把 §8 主线接通到那张
  权威表）。
- 不动代码：现有 `tracing` 调用已合规（task-32 落地）。

### 审阅人

reviewer (用户) ✓ —— 本 session 明确指令 "spec 加上 tracelog 的格式
要求"，据此回写 §8。

---

## 2026-04-26 · task #15 Stage 1 真机 bus2 6-motor SerialRT 闭环（**状态：登记 / 实施完成 + 真机 PASS**）

### 背景

task-30 v9 M7 落地（state_machine ↔ adapter 接通 + SerialRT 路径 +
ipc_tick errata 三步式 + per-event guard view + ClearEStop confirmed
守门 + EStop 触发即 fsync latch 落盘 + `robotctl estop` 子命令 +
bus2 right_arm 6-motor 配置 + Orin runbook + ensure_latch_parent 守门）。

随后在 Orin 真机上端到端验证 bus2 右臂 6 motor 的 SerialRT 路径：

### 验证结果（2026-04-26）

详见 [`handoffs/2026-04-26.md`](handoffs/2026-04-26.md) §3。**§2 ping → §3
smoke → §4 ballet → §5 EStop → §6 重启 latch=true → §7 删 latch 恢复 + 再
EStop 全过**。关键证据：

- §5/§7 真机 robotctl estop 后**< 200 ms** 内 daemon 仍 active 时 latch
  primary+bak 双文件已 = "1"（fsync 落盘成立，安全语义不依赖 graceful
  shutdown）。
- §5/§7 EStop 后 6 motor `Torque_Enable` 全部从 1 → 0（Feetech broadcast
  `Torque_Enable=0` 真断扭）。
- §6 latch=true 重启后 daemon `initial estop_latched=true` + state 全程
  `Safe`（即便收到 hb 也不进 Ready/Enabled）+ 无任何 `Enable(true)` 转换。

### 暴露的 known-issue（不阻断 #15 但记账）

1. **iceoryx2 0.8.1 single-service multi-subscriber attach race**：daemon +
   2 个 robotctl watch 在同一 service 上 attach，第二个 watch 静默失败拿不
   到 sample。同样影响 `mock_s1.anchor + watch` 同跑场景。**不影响**控制
   链路（control 不走 watch；watch 仅诊断观测）。workaround：物理验证靠
   daemon log + mock_s1 log + post-run motor pos 三方合验。follow-up：升
   iceoryx2 0.9 / 0.10 看是否 fix。
2. **sudo ↔ user 模式切换的 ownership 残留**：sudo daemon 写的 latch 文件
   是 root owner，后续 user daemon 读 `Permission denied` fatal 退出；同
   样 sudo 创建的 `/tmp/iceoryx2/nodes/<uuid>/` 让 user clients 报
   `ServiceInCorruptedState`。**约定**：统一 user 模式（推荐）；切换时清
   `<latch>` + `<latch>.bak` + `/tmp/iceoryx2/{nodes,services}/*`。

### 影响面

- 代码：`crates/robot-s0/src/ipc_thread.rs`（latch fsync hook）+
  `crates/robot-s0/src/bin/s0_daemon.rs`（ensure_latch_parent）+
  `crates/robot-cli/src/bin/robotctl.rs`（estop 子命令 + watch
  --duration-secs）+ `crates/robot-adapters/feetech/src/factory.rs`
  （build_serial_rt）+ `crates/robot-core/src/{adapter,factory,executor_ctrl}.rs`
  （SerialRtAdapter 综合 trait + ExecutorCtrlClient::safety_state）。
- 测试：`crates/robot-s0/src/ipc_thread.rs::tests::estop_topic_writes_latch_to_disk_immediately`
  + `crates/robot-s0/src/ipc_tick.rs::tests::events_in_same_tick_use_per_event_guard_view`
  + `crates/robot-s0/tests/serial_rt_e2e.rs` 5 条 E2E。
- 文档：`configs/xlerobot_bus2_right_arm_full.toml` +
  `docs/handoffs/2026-04-26_bus2_runbook.md`（含真机实测回执 + 附 A 排查
  + 附 C known-issue + workaround）+ `docs/handoffs/2026-04-26.md`
  （session 状态 handoff）。

### 审阅人

reviewer (用户) ✓

---

## 2026-04-21 · 工具链：nightly 硬要求 + edition 2024（**状态：已回写**）

### 决定

- Rust **nightly 是硬要求**，**严格不用 stable**。`rust-toolchain.toml` pin `channel = "nightly"`（可进一步 pin 具体日期 `nightly-YYYY-MM-DD`）。
- **Edition 2024**。`workspace.package.edition = "2024"`。**不写 `rust-version`**：自动跟随最新 nightly；需要复现特定构建时用 `rust-toolchain.toml` 的 `channel = "nightly-YYYY-MM-DD"` pin，而非 `rust-version`。
- 用户（2026-04-21）本 session 明确指令 "严格不用 stable"，据此**回写 `docs/00_design.md` §9**（通过 v0.1.2 "冻结后文档卫生" 机制，附录 E 新增工具链行），并同步回写 dimos 侧源版本 `docs/09_robot_framework.md` §9 + 附录 E。

### 原文 → 当前

原 §9：

> Rust **stable**（尽量不用 nightly）。

当前 §9（见 `docs/00_design.md` / dimos `docs/09_robot_framework.md`）核心句：

- Rust **nightly** 硬要求，不留 stable 回退。CI 不跑 stable 演练。
- 核心 crate（`robot-core` / `robot-msg` / `robot-transport` / `robot-s0` / `robot-s1`）默认不写 `#![feature(...)]`，**目的是降低 nightly 升级回归风险**，不是为了留 stable 后门；确需启用必须 crate 顶部注释 + review。
- 工具类 crate（`robot-cli` / `robot-viz` / `robot-rpc` / `robot-s2-bridge` / `robot-tests` / `robot-recorder`）可自由使用 nightly feature。
- nightly 回归风险靠 **pin 具体日期 + 原地回滚到上一已验证日期** 解决，不切 stable。

### 恒定约束（不变）

1. S0 / S1 RT 铁律（`panic = "abort"`、`mlockall`、禁 heap alloc、禁 `Arc::clone/drop`，`00_design.md` §0 / §4.9）。
2. S0 / S1 禁用 `mimalloc`（`00_design.md` §0 Allocator 策略）—— 与全局 `~/.claude/CLAUDE.md` §5 的 mimalloc 偏好冲突时，本项目优先。
3. Allocator 表按 `00_design.md` §0：外部工具（cli / viz / rpc / s2-bridge）才用 mimalloc。

### 影响面

| 文件 | 调整 |
|------|------|
| `rust-toolchain.toml` | `channel = "nightly"`（Step 0 已创建） |
| `docs/00_design.md` §9 | "stable（尽量不用 nightly）" → "nightly 硬要求 + edition 2024，严格不用 stable" |
| `docs/00_design.md` 附录 E · 冻结后文档卫生 | 追加工具链（2026-04-21）行 |
| dimos `docs/09_robot_framework.md` §9 / 附录 E | 同步回写（源版本） |
| `CLAUDE.md`（项目） | 《Rust 规范》小节重写为 nightly 硬要求表述；workspace 布局注释改引用 `docs/00_design.md §9` |
| `docs/02_start_here.md` | 硬性约束 #4 + Step 0 指示同步 |
| `docs/01_implementation_steps.md` | Step 0 `rust-toolchain.toml` stable → nightly；"不要做的事" #6 改为 "严格不用 stable" |

### 审阅

- 提议 → 决定：用户 loosqk（2026-04-21 本 session 两次明确：先 "使用 nightly"，后 "严格不用 stable"）。
- 复审触发：
  - nightly 连续 3 周破坏核心 crate 编译 → pin 更早日期 + 调查根因，**不切 stable**。
  - 团队扩大 / 引入非 nightly 环境用户 → 重新评估。当前决定视为 v0.x 范围内稳定。

---

## 2026-04-21 · robot-core §4.2 允许列表扩展：加 `libc`（**状态：偏离**）

### 涉及小节

`docs/00_design.md` §4.2 `robot-core` 依赖铁律：

> 允许依赖：thiserror, smallvec, arrayvec, bytemuck, crc32fast

### 决定

Step 1 `robot-core::time` 落地时，`workspace.dependencies` 加入 `libc = "0.2"`，`robot-core/Cargo.toml` 以 workspace 继承方式启用。

### 理由

1. `robot-core::time::SystemClock` 需要 `clock_gettime(CLOCK_MONOTONIC_RAW)` + `nanosleep`（§3.2.1 要求的单调原始时钟），Rust `std`/`core` 均未暴露，必须经 FFI。
2. `libc` 是 `core::ffi` 之上**最薄**的平台常量 + 函数声明层（no_std 友好，零 runtime、零 allocator、零 async executor），与 §4.2 禁止的 `tokio` / `iceoryx2` / `pyo3` / `flatbuffers runtime` / `serde_json` 性质完全不同。
3. 替代路径（手写 `extern "C" { fn clock_gettime(...) }` + 手抄 `timespec` + 手维护 `CLOCK_MONOTONIC_RAW = 4` 的平台差异）反而绕开了 `libc` crate 的跨平台维护，更易出错。
4. 与 §9 "nightly 硬要求 + edition 2024" 属同类"基础约束微调"，此后再有同类需要（例如 `robot-core::os` 对 `sched_setscheduler` / `mlockall` / `sd_notify` 的 FFI）统一走 `libc`。

### 约束

1. **使用范围**：`libc` 仅允许出现在 `robot-core::time`（及未来明确的 OS 抽象模块 `robot-core::os`）。其他模块（`adapter` / `policy` / `task` / `error`）禁止 import `libc`。Step 1 用 module-level `// 允许 libc` 注释标注；CI 后续追加 grep 拦截（`rg 'use libc' crates/robot-core/src/` 结果必须全部命中受控模块）。
2. **no_std 友好**：调用必须包 `unsafe { }` block（edition 2024 默认行为已强制）；不引入 `libc` 的 `std` feature（默认 feature 即含，我们不额外 opt-in）。
3. **平台支持**：`SystemClock` 仅在 `target_os = "linux"` / `target_os = "macos"` 启用真实 syscall；其他 target 下为 `panic` stub，`lib.rs` 顶部注释说明（项目目标平台是 Linux，macOS 为开发期 Step 0–5 支持；Windows 仅 S2 客户端不经 robot-core::time）。

### 影响面

| 文件 | 调整 |
|------|------|
| `Cargo.toml`（workspace 根） | `[workspace.dependencies]` 追加 `libc = "0.2"` |
| `crates/robot-core/Cargo.toml` | `[dependencies]` 追加 `libc = { workspace = true }` |
| `crates/robot-core/src/time.rs` | 新建（Step 1 交付） |
| `docs/00_design.md` §4.2 | **不动**；本条仅作偏离登记 |

### 审阅

- 提议：Claude Code（Step 1 实施前拍板）
- 决定：用户 loosqk（2026-04-21 session 内同意 Step 1 加 libc 的前置决策点）
- 复审触发：若用户同意，可通过 "冻结后文档卫生" 机制（附录 E）回写 `00_design.md` §4.2 允许列表 + 附录 E 行，把本条状态改为 "已回写"。
- 同类未来扩展（`sched_setscheduler` / `mlockall` / `sd_notify` / `pthread_setaffinity` 等 RT 所必需的 syscall）默认复用本条；**非 OS-syscall 性质的新依赖仍须单独登记**。

---

## 2026-04-22 · workspace 布局扩展：新增 IK 层（**状态：已回写**）

### 涉及小节

`docs/00_design.md` §4.1《模块地图》。

### 决定

模块地图新增两类 crate：

1. **`robot-kinematics/`**：trait 层，零 runtime 依赖。定义 `ArmIkSolver` / `BaseKinematics` / `Pose` / `Twist` / `Jacobian` / `IkError`。
2. **`robot-kinematics-<robot>/`**：每种机器人一个实现。目前规划 `xlerobot`（v0.4）、`piper`（v0.6）；用户可 fork 自己的 `robot-kinematics-<mine>` 而不动 s1 / s0 / adapter。

`robot-s1` 新增 `ik/` 子模块持有 `Option<Box<dyn ArmIkSolver>>`，按 Cargo feature（`xlerobot` / `piper` / ...）选具体实现。`tasks/` 新增 `cartesian` 分支走 IK 胶水。

### 理由

1. **用户需求**：v0.4 起用户要给不同机器人接自己的 IK solver（XLeRobot 5DoF 双臂 / 三轮全向，Piper 单臂等），需要一个明确的 trait 层抽象，避免 fork `robot-s1`。
2. **分层正确性**：按 `00_design.md` §1.1，IK 属于 S1 层 ("避障 / WBC / IK")，不属于 S0（关节空间）或 adapter（joint I/O）。把 IK 放在独立 crate 而非 `robot-core`，避免污染 `robot-core` 的零 runtime 依赖铁律（§4.2 允许列表不受影响）。
3. **替换模型**：沿用 adapter 的 Cargo feature + trait object 模式（`00_design.md` §4.5 "inventory 静态注册" 精神一致），build-time 决定，不做 runtime dynamic plugin（与 v0.x 不做 plugin ABI 的原则一致）。
4. **迁移源**：XLeRobot 侧已有现成实现（dimos `src/xlerobot_dimos/kinematics/arm_ik.py` 5DoF IK + `common/units.py` 三轮全向运动学），v0.4 可直接移植为 Rust，作为 trait 的首个落地。

### 关键 trait 约束（v0.3 前定稿，本条只锁 API 形状）

```rust
pub trait ArmIkSolver: Send {
    fn info(&self) -> KinematicInfo;                        // DoF / base_frame / ee_frame
    fn forward(&self, q: &[f64]) -> Pose;                   // FK，不 fallible
    fn inverse(
        &self,
        target: &Pose,
        seed: &[f64],
        deadline: Deadline,                                 // 绑 robot-core::time，避免在 S1 tick 阻塞
    ) -> Result<IkSolution, IkError>;
    fn jacobian(&self, q: &[f64], out: &mut Jacobian);
}

pub trait BaseKinematics: Send {
    fn body_to_wheel(&self, twist: &Twist, out: &mut [f64]);
    fn wheel_to_body(&self, wheel: &[f64]) -> Twist;
    fn integrate_odom(&self, wheel_delta: &[f64], odom: &mut Pose2d);
}
```

- `inverse()` 带 `Deadline`：数值 IK 超时返回 `IkError::DeadlineExceeded`，S1 回落到上一解或进入 DEGRADED。
- `seed` 是上次关节位：多解 IK 保证连续性。
- 错误枚举固定为 `{ NoSolution, OutOfReach, SingularityTooClose, DeadlineExceeded }`。

### 约束

1. **分层铁律**：`robot-kinematics` 不依赖 `iceoryx2` / `tokio` / `pyo3` / vendor SDK；与 `robot-core` 同等"零 runtime"要求。
2. **不进 S0 / adapter**：S0 只收 `JointCommand`（关节空间），adapter 只做 joint I/O；IK 结果必须先在 S1 转成 `JointCommand` 再下发。Cartesian 语义永远不跨过 S1 → S0 边界。
3. **RT 友好**：`ArmIkSolver::inverse()` 调用点（S1 coordinator tick 内）要求 `deadline` 不超过 S1 剩余 tick 预算（<15 ms，§10.1）；超时返回 fallback，不阻塞 coordinator。
4. **build-time 选择，非 runtime plugin**：`robot-s1/Cargo.toml` 用 `[features] xlerobot = ["robot-kinematics-xlerobot"]` 等；一个二进制产物对应一台机器人。
5. **用户扩展路径**：用户 fork 自有 `robot-kinematics-<mine>` crate，在本地 workspace 加 `[patch]` 或独立 workspace 引用，`robot-s1` 以 `default-features = false` + 自定义 feature 挂入。不需要修改上游 `robot-s1` / `robot-s0` / `robot-adapters` / `robot-msg`。

### 影响面

| 文件 | 调整 |
|------|------|
| `docs/00_design.md` §4.1 模块地图 | 新增 `robot-kinematics/` + `robot-kinematics-<robot>/`；`robot-s1` 子目录加 `ik/` 和 `tasks/cartesian` |
| `docs/00_design.md` 附录 E · 冻结后文档卫生 | 追加 "IK 分层（2026-04-22）" 行 |
| dimos `docs/09_robot_framework.md` §4.1 / 附录 E | 同步回写（源版本） |
| `CLAUDE.md`（项目）《规划的 workspace 布局》 | 新增两类 crate + "IK 分层说明"小节 |
| `Cargo.toml` 未来 crate 注释列表 | 追加 `robot-kinematics`（v0.3）/ `-xlerobot`（v0.4）/ `-piper`（v0.6） |
| `docs/01_implementation_steps.md` | 本条**不动**；IK trait 的 Step 放 v0.3 专项规划，届时单独补条目（目前 Step 2-6 不涉及 IK） |

### 审阅

- 提议：Claude Code（用户询问"根据不同机器替换自己的 IK 方便吗"后给出架构补丁）。
- 决定：用户 loosqk（2026-04-22 session 内同意 "现在只做 1) 改布局图 2) 登记 CHANGELOG"）。
- 复审触发：
  - v0.3 定 `ArmIkSolver` trait 时，若发现实际需要 `Box<dyn>` 以外的多态形态（例如泛型 solver 让 S1 coordinator 静态分发），回到本条登记 API 形状变更。
  - v0.6 Piper 异构机器人落地时，若 trait 不足以表达（例如 7DoF 冗余、闭链、可变 DoF），视为"抽象漏洞" → 再补一条 CHANGELOG，届时可能修订 `robot-kinematics` 的 trait。

---

## 2026-04-22 · workspace runtime 依赖白名单机制（**状态：登记/建立约定**）

### 背景

`docs/00_design.md` §4.2 规定了 **`robot-core` 依赖铁律**（允许 `thiserror / smallvec / arrayvec / bytemuck / crc32fast`，禁 `iceoryx2 / tonic / pyo3 / tokio / tracing-subscriber / vendor SDK / flatbuffers runtime / serde_json`，§4.2 只约束 `robot-core`）。

`robot-msg` / `robot-transport` / `robot-s1` 等其他 crate 不在 §4.2 约束范围内，但为防止"顺手加 serde_json / tokio / reqwest"这类依赖漂移，在本 CHANGELOG 登记 **per-crate runtime 依赖白名单约定**，作为 §4.2 机制向整个 workspace 的延伸。

### 约定

1. 每个 crate 首次引入新的 runtime 依赖（非 dev-dep、非 build-dep）前，必须在本 CHANGELOG 追加或更新该 crate 对应的白名单条目，写明：依赖名 / 版本范围 / 理由 / RT 影响 / 使用范围限制。
2. 白名单外依赖 → 视同"偏离"，走标准 CHANGELOG 偏离流程。
3. `dev-dependencies`（`trybuild` / `proptest` / `criterion` 等）**不受此约定约束**，按需添加。
4. 本条是**约定**，不是 00_design.md 回写；§4.2 文字不变。

### 各 crate 白名单（v0.2 开工基线）

#### `robot-core`（§4.2，原文）
- 允许：`thiserror`、`smallvec`、`arrayvec`、`bytemuck`、`crc32fast`
- 例外：`libc = "=1.0.0-alpha.3"`（仅 `robot-core::time` 与未来 `robot-core::os`，见 2026-04-21 条；2026-04-22 由用户决定从 `0.2.x` stable 切到 `1.0` 预发布轨道，精确 pin 防 alpha.N → alpha.N+1 破坏性变更）
- 禁止：`iceoryx2 / tonic / pyo3 / tokio / tracing-subscriber / vendor SDK / flatbuffers runtime / serde_json`

#### `robot-msg`（v0.2 Step 2 登记）
- 允许：
  - `flatbuffers = "=25.12.19"`（与 vendor flatc 25.12.19 对齐；Step 2 schema 反序列化必需。Cargo.toml 用 `=` 精确 pin，防 minor 漂移）
  - `blake3`（`schema_lint` bin 的 `layout_id` 碰撞检测 + `ConfigIdentity` 相关 hash）
- 禁止：同 `robot-core` 禁用列表（flatbuffers runtime 除外）
- RT 影响：`robot-msg` 的 reader 路径零拷贝、零 allocation；`FlatBufferBuilder` 构造**禁止**进 RT thread（§0 RT thread 禁令已收录）

#### `robot-transport`（v0.2 Step 3 首次登记 / Step 5 会追加 iceoryx2）
- 允许（Step 3 已引入，见 `crates/robot-transport/Cargo.toml`）：
  - `robot-msg = { workspace = true }`（解析 FrameHeader / 查 `SCHEMA_REGISTRY`）
  - `robot-core = { workspace = true }`（保留入口；parser 目前未用，但 Step 4
    `SampleLease<T>` / trait 层会用到 `RobotClock` / newtype）
  - `flatbuffers = { workspace = true }`（`flatbuffers::root` verifier + 类型 handle）
  - `thiserror = { workspace = true }`（`ParseError` 分类派生）
- 预期允许（Step 5 追加）：`iceoryx2 = "0.x"`（具体版本在 Step 5 引入时登记 sha256 + tag）
- 禁止：同 `robot-core` 禁用列表，外加 `tokio`（RT 线程会直接走 parser 路径，不得引入 async runtime）
- RT 影响：`parse_sample` **零 alloc、零 panic、零 unwrap**，可在非 RT subscriber 线程调用；RT 线程本身仍不应在 hot path 直接 parse iceoryx2 sample（IPC 线程负责解析后写 slot，§1.2）

#### `robot-kinematics`（v0.3 登记）
- 预期允许：纯 math（`nalgebra`?/自研简薄线代），零 runtime
- v0.3 开工时具体登记

#### `robot-s0`（v0.2 Step 6 登记）
- 允许：`libc`（继承 `robot-core` 例外，RT 线程 syscall 必需）
- 禁止：`tokio` / `mimalloc`（§0 Allocator 策略 + §4.7 Python 隔离的延伸）
- Step 6 具体登记 `iceoryx2` 依赖路径（经 `robot-transport` 间接）

#### `robot-s1`（v0.3 登记）
- 预期：`tokio`（只在 non-RT 协调线程）；具体边界 v0.3 落地时登记

#### `robot-policy-host`（v0.5 登记）
- 预期：`pyo3` / `ort`（ONNX runtime）；v0.5 具体登记

#### 工具类 crate（`robot-cli` / `robot-viz` / `robot-rpc` / `robot-s2-bridge` / `robot-tests` / `robot-recorder`）
- 宽松策略：依赖加入时 CHANGELOG **仍需登记**，但不做"允许/禁止"二分；RT / 安全相关除外
- 例如 `robot-rpc`: `tonic` / `tower` / `prost`；`robot-viz`: `rerun-sdk` / `foxglove`；`robot-s2-bridge`: `pyo3`

### 影响面

| 文件 | 调整 |
|------|------|
| `docs/00_design.md` §4.2 | **不改**；本条是向其他 crate 的机制延伸，不修订 `robot-core` 铁律 |
| dimos `docs/09_robot_framework.md` §4.2 | **不改**（源文档同上） |
| `docs/CHANGELOG.md`（本文件） | 新增本条；未来每次新 crate / 新依赖都在此补登记 |
| `docs/01_implementation_steps.md` Step 2 交付 | 显式列出 `flatbuffers = "=25.12.19"` 并指向本条 |

### 审阅

- 提议：Claude Code（Step 2 开工前决策 #3）。
- 决定：用户 loosqk（2026-04-22 session 内同意 "CHANGELOG 顺手登记一条 robot-msg 的 runtime 依赖白名单"）。
- 复审触发：
  - 任何 crate 首次引入新 runtime 依赖 → 更新本条对应 crate 子清单。
  - 若条目数量超过一页可读范围 → 把白名单拆到单独 `docs/dependency_allowlist.md`，本条仅保留约定本身。

---

## 2026-04-22 · Step 2 工具链决策（**状态：实施细节登记**）

### 涉及 Step

`docs/01_implementation_steps.md` Step 2 · `robot-msg` schema registry。

### 决定（三条）

1. **flatc vendor 策略：A 预编译二进制**
   - 路径：`3rd-party/flatc/{darwin-arm64,linux-x86_64,linux-aarch64}/flatc` + `VERSION` + `checksums.sha256`
   - 版本 pin `flatbuffers 25.12.19`，与 Rust `flatbuffers = "=25.12.19"` 对齐（`=` 精确 pin，防 minor 漂移）
   - 拒绝 B（源码 + cmake 构建）：CI 每次 clean build 多 2–3 min 不可接受，且 flatc 升级频率 ≪ 1/年
   - 升级走手动 `scripts/refresh_flatc.sh` + `git commit`，不在 CI 自动拉新版本

2. **Schema lint：独立 Rust bin**
   - 位置：`crates/robot-msg/src/bin/schema_lint.rs`
   - 调用：`cargo run -p robot-msg --bin schema_lint`
   - 分工：
     - `tests/compat.rs` → 运行时 round-trip / old-new reader 交叉（`cargo test`）
     - `schema_lint` bin → 静态校验 deprecated id / schema_id 唯一 / layout_id 碰撞 / 字段 id 单调
   - v0.2 实现：regex 扫 `.fbs` 文本；v0.3 视需要升级到 `bfbs` 反射
   - **拒绝 Python 脚本**：全局 CLAUDE.md 偏 Rust/C++，CI 不引入 Python runtime

3. **依赖白名单扩展到 workspace**：`flatbuffers = "=25.12.19"` 作为 `robot-msg` 的第一条白名单入库，见本文件 "workspace runtime 依赖白名单机制" 条目

### 影响面

| 文件 | 调整 |
|------|------|
| `docs/01_implementation_steps.md` Step 2 交付 / 测试 / 退出标准 | 改写 vendor 布局、`schema_lint` bin、`refresh_flatc.sh`、退出标准加 "无网可 build / 无 Python 环境" |
| `3rd-party/flatc/` | Step 2 开工时创建目录结构 + 跑 `scripts/refresh_flatc.sh` 拉取二进制（用户本地一次性操作） |
| `scripts/refresh_flatc.sh` / `refresh_flatc.ps1` | Step 2 开工交付 |
| `docs/00_design.md` §9 / §4.1 | **不改**（实施细节，不是架构语义） |

### 审阅

- 提议 + 决定：用户 loosqk（2026-04-22 session 内明确同意 A 预编译 + 独立 Rust lint bin + 白名单登记）。
- 复审触发：
  - flatc 新版本出现关键 schema feature → 走手动升级流程（`refresh_flatc.sh` + sha256 更新）。
  - `schema_lint` 的 regex 策略出现误报 / 漏报 → v0.3 升级到 bfbs 反射。

---

## 2026-04-22 · §4.3 writer API 调整：`publish` 挂到 writer（**状态：已回写**）

### 原文 → 当前

原 `docs/00_design.md` §4.3：

```rust
pub trait Publisher<T> {
    fn loan(&mut self) -> Result<SampleWriter<'_, T>, TransportError>;
    fn publish(&mut self, w: SampleWriter<'_, T>) -> Result<(), TransportError>;
}
```

当前（§4.3 + 附录 E 冻结后文档卫生行）：

```rust
pub trait Publisher<T> {
    fn loan(&mut self) -> Result<SampleWriter<'_, T>, TransportError>;
}

impl<'a, T> SampleWriter<'a, T> {
    pub fn publish(self) -> Result<(), TransportError>;
}
```

### 理由

`Publisher::publish(&mut self, w: SampleWriter<'_, T>)` 在 Rust borrow checker 下不自洽：

1. `loan` 返回 `SampleWriter<'a, T>`，其 `'a` 绑定 `&'a mut Publisher`；
2. 只要 `w` 存活，`&mut Publisher` 借用就被占用；
3. 再调 `Publisher::publish(&mut self, ...)` 要拿第二个 `&mut self` → 冲突。

把 `publish` 挂在 writer 上，**按值消费 self**：writer move 进 `publish` 后自然释放 `&mut Publisher` 借用，publisher 可以立即被再次借用（再次 `loan()`）。语义完全等价。

### 语义不变的约束（§4.3 原保证全部保留）

1. `loan()` 返回的 `SampleWriter` 持 iceoryx2 sample 可写借用，生命周期绑 publisher 的借用期。
2. **未 publish 即 drop**：自动 discard / release loan，不留悬挂 sample（drop impl 实现）。
3. **publish 消费 writer**：调用后 `w` 不可再用；编译期拦截 "publish 后继续写 body/blob"（move semantics）。
4. 错误路径（body 编码失败等）`return Err` 让 writer drop，loan 自动归还。

### 影响面

| 文件 | 调整 |
|------|------|
| `docs/00_design.md` §4.3 | `Publisher` trait 删 `publish` 方法，加独立 `impl SampleWriter::publish(self)` 示例 |
| `docs/00_design.md` 附录 E · 冻结后文档卫生 | 追加 "§4.3 writer API（2026-04-22）" 行 |
| dimos `docs/09_robot_framework.md` §4.3 / 附录 E | 同步回写（源版本） |
| `crates/robot-transport/src/traits.rs` | `Publisher` trait 只有 `loan`；无 `publish` method |
| `crates/robot-transport/src/writer.rs` | `impl<'a, T> SampleWriter<'a, T> { pub fn publish(self) -> ... }` |

### 审阅

- 提议：Claude Code（Step 4 决策 #5，发现 §4.3 原 API 在 borrow checker 下不可实现）
- 决定：用户 loosqk（2026-04-22 session 内同意 "按 ▶ 倾向推"）
- 复审触发：若未来某后端（Zenoh / 裸 shm）的 API 迫使 publish 回到 publisher trait（例如需要 publisher 内部持有的事务性状态在 publish 时更新），重新评估本调整；Step 5 iceoryx2 实现应能直接采纳当前签名。

---

## 2026-04-22 · Step 1 retrofit：`robot-core::time` 补时间代数原语（**状态：登记 / 向前兼容**）

### 涉及 Step

`docs/01_implementation_steps.md` Step 1 · `robot-core::time`（已完成于 `654d7ab`，本条属 retrofit，不回滚 Step 1 状态）。

### 决定

`crates/robot-core/src/time.rs` 追加三个 const fn method，填补 Step 1 原交付清单未覆盖但后续"多传感器时间对齐 / `TimeAligner`"必需的时间代数原语：

```rust
impl MonoRawNs {
    /// `self - other` 的有符号差（ns）。正值 = self 更晚。
    /// i128 中间值防 u64 减法下溢；饱和到 i64（±~292 年 ns，v0.x 绰绰有余）。
    pub const fn signed_delta_ns(self, other: MonoRawNs) -> i64;

    /// |self - other|，方向无关，内部走 u64::abs_diff。
    pub const fn abs_diff(self, other: MonoRawNs) -> DurationNs;
}

impl Deadline {
    /// 距 deadline 剩余时长；已过期返回 None。
    /// 语义与 `Sub for MonoRawNs`（deadline - now）一致。
    pub const fn remaining(self, now: MonoRawNs) -> Option<DurationNs>;
}
```

### 理由

1. **`Sub for MonoRawNs` 只给前向差**（`a >= b` 返 `Some(DurationNs)`，否则 `None`），表达不了"b 比 a 早多少 ns"这种方向信息。`TimeAligner` 的最近邻搜索 / 双向容差判断需要方向，单独开 `signed_delta_ns` 避免每个调用方手工拼 `if a>=b { ... } else { ... }`。
2. **`abs_diff` 是 `TimeAligner` 容差比较的原子操作**（`abs_diff <= tol`）。直接走 `u64::abs_diff` 比手写 if/else 更短、clippy `manual_abs_diff` 零告警。
3. **`Deadline::remaining` 把"距 deadline 还剩多久"从调用方散落的 `deadline.as_mono_raw_ns() - now` 收敛到一处**，const fn + `checked_sub`，零 panic。§6 / §10.1 里所有"剩余预算"语义都该走这个 API。
4. **与 2026-04-22 session 对融合层评估的连续性**：本 session 评估了 "传感器数据根据时间融合吗"（结论：架构只给了时间戳基础设施，没给融合算法层；TimeAligner 该住独立 `robot-fusion` crate，v0.3+ 再排），`robot-core::time` 补这三个方法是**零依赖、向前兼容**的自然延伸，不承诺启动 fusion 工作，但未来谁写 aligner 都少造一次轮子。

### 约束

1. **纯扩展**：signature 向后兼容，不改现有任何 API；原有 33 测试全绿。
2. **零新依赖**：不引入 `nalgebra` / `num-traits` 等 math crate，仍只依赖 `libc`（§4.2 既有例外）。
3. **不 panic / 不 alloc**：三个方法都是 const fn + 饱和 / checked 算术，符合 RT 铁律（§0）；本身仍不建议在 RT thread 直接调用（`robot-core::time` 任意 API 在 RT 路径的使用仍按 §1.2 单独审）。
4. **使用范围**：任意 crate 可用；`TimeAligner` / fusion 层真正落地仍走 CHANGELOG 新条目（届时登记 crate 名 / 依赖 / 策略枚举）。

### 影响面

| 文件 | 调整 |
|------|------|
| `crates/robot-core/src/time.rs` | `impl MonoRawNs` 加 `signed_delta_ns` / `abs_diff`；`impl Deadline` 加 `remaining`；追加 6 条单元测试（含 `u64::MAX` 极限 / 方向对称 / 与 `Sub` 一致性） |
| `docs/00_design.md` §3.2 / §3.2.1 | **不改**（属同一语义域的工具函数扩展，不新增时钟域 / 不引入 wall clock） |
| `docs/01_implementation_steps.md` Step 1 交付清单 | **不改**（retrofit 不回写原步骤；未来整理 v0.2 总结时可并入） |

### 审阅

- 提议：Claude Code（2026-04-22 session 评估 "传感器数据根据时间融合吗" 的后续行动项 #1）
- 决定：用户 loosqk（2026-04-22 session 内 "先把 1 完成"）
- 复审触发：
  - v0.3 抽 `robot-fusion` crate 时，若发现需要有符号 `DurationNs` / `SignedDurationNs` 类型（例如 IMU 样本时间戳可能早于 tick 的 ±1 ms 窗口），考虑把 `signed_delta_ns` 返回值升级为 newtype。
  - 若后续要加 wall clock offset / 硬件时钟校准原语（`HwClockOffset`），单独登记新条目，不塞回本条。

---

## 2026-04-22 · Step 5 开工决策与 iceoryx2 依赖登记（**状态：登记**）

### 涉及 Step

`docs/01_implementation_steps.md` Step 5 · mock iceoryx2 pub/sub。

### 前置：macOS 烟测

在 `/tmp/iceoryx2_smoke` 跑 iceoryx2 0.8.1 独立 sandbox，验证了同进程 pub/sub、ServiceName 字符集、service attribute、event service 四条。macOS (Darwin 25.4) 全绿。烟测源码不入仓库。

### 决策一览（用户 2026-04-22 session 拍板）

| # | 决策 | 结论 |
|---|---|---|
| ① iceoryx2 版本 pin | `iceoryx2 = "=0.8.1"` 精确 pin，仿 flatbuffers `=25.12.19`；升级走 CHANGELOG |
| ② Service name 映射 | **方案 A**：topic 原样喂 `ServiceName::new()`（`/<robot>/s0/<hw>/state` 实测接受），§2.3 命名空间零改动 |
| ③ QoS 映射表（§2.4 → iceoryx2 builder） | 见本条"QoS 映射"小节 |
| ④ Sample size 分档 | 4 档 + 硬编码 default + TOML override 两层，见"size tier"小节 |
| ⑤ schema_id 绑定 | FrameHeader 字段（已落 Step 3）+ iceoryx2 service attribute `schema_id` 两道防线；attribute 是硬约束，不匹配 verifier 拒绝 open |
| ⑥ macOS 可行性 | ✓ 通过烟测；Step 5 可在 macOS 完整做 |
| ⑦ 事件 / 轮询 | 方案 B：`s0_ipc_thread` 用 iceoryx2 Event service + `timed_wait_one` 代替 polling（50 ms 实测 51 ms，抖动 1 ms 内） |

### Size tier（决策 ④ 展开）

**4 档上限**：

| 档 | `max_sample_size` | 覆盖（按当前注册的 9 个 schema） |
|---|---|---|
| control | **8 KiB** | JointLayout / JointState / JointCommand / Twist / EStopEvent / Heartbeat |
| sensor_small | **1 MiB** | 预留：未来 IMU / FT 等小 sensor |
| sensor_large | **16 MiB** | ImageHeader / PointCloudHeader / TensorHeader |
| tensor | **128 MiB** | 预留：未来 `SCHEMA_POLICY_OBSERVATION` / `SCHEMA_ACTION_CHUNK` |

**两层映射**：

1. **硬编码 default**（`crates/robot-msg/src/size_tier.rs`）：
   - `pub enum SizeTier { Control, SensorSmall, SensorLarge, Tensor }`
   - `pub const fn default_tier_of(schema_id: u32) -> Option<SizeTier>`
   - 未注册 schema 返回 `None`（transport 侧 fail-fast）
   - 单元测试穷举 `SCHEMA_REGISTRY`，每个 id 必须返回 `Some`（加新 schema 必同步加 tier，测试拦截遗漏）

2. **TOML override**（`robot-s0` / `robot-s1` 启动期读 `configs/<robot>.toml`）：
   ```toml
   [transport.tier_override]
   "/xlerobot_01/policy/pi0_5/observation" = { max_sample_size_bytes = 33554432 }  # 32 MiB
   ```
   - 以 **topic 字符串** 为键（最常见场景）；schema 级 override 留未来需要时再加
   - 双向允许（升 / 降）；iceoryx2 在 sample 超限 loan 时自然失败，安全网本来就在
   - 所有 override 参与 `ConfigIdentity.transport_config_hash`，跨节点不一致启动拒绝（具体落地在 v0.3 ConfigIdentity 专项）

### QoS 映射（决策 ③）

§2.4 九类 → iceoryx2 `service_builder.publish_subscribe::<T>()` 选项：

| 类别 | 典型 topic | `history_size` | `subscriber_max_buffer_size` | `max_publishers` | `max_subscribers` | 备注 |
|---|---|---|---|---|---|---|
| S0 command | `/<r>/s0/<hw>/command` | 1 | 1 | 1 | 1 | 过时由 S0 按 `on_timeout` 兜底；不启用 safe_overflow（满了即 drop-and-metric） |
| S0 state | `/<r>/s0/<hw>/state` | 2 | 2 | 1 | N | drop old OK；safe_overflow=true |
| S1 trajectory | `/<r>/s1/trajectory/<task>` | 8 | 16 | 1 | N | 丢弃必报警；safe_overflow=false |
| S1 cmd_vel | `/<r>/s1/cmd_vel` | 1 | 1 | 1 | 1 | latest-only；safe_overflow=true |
| Sensor / Image | `/<r>/sensors/**` | 2 | 4 | 1 | N | drop old；safe_overflow=true |
| Safety event | `/<r>/events/estop` | 32 | 64 | N | N | 不丢；满则升 fault + emergency log |
| Task event | `/<r>/s1/task_status` | 64 | 64 | 1 | N | 丢弃必可观测（metric + log） |
| RPC response | iceoryx2 req/resp | 1 | 1 | — | — | deadline 驱动；不在 publish_subscribe |
| Policy observation / action_chunk | `/<r>/policy/<name>/**` | 1 | 2 | 1 | 1 | VLA 单方向；S1 永不等 policy |

**QoS 推迟项**：payload_alignment（当前统一 8 B；图像 blob 需 64 B 的话 v0.3 再加）、`max_loaned_samples`（默认即可）。

### 事件服务（决策 ⑦）

- `s0_ipc_thread` 订阅 `/<r>/s1/heartbeat` 的 Event service，阻塞在 `Listener::timed_wait_one(2 × heartbeat_period)`
- `s0_pub_thread` 仍周期性 publish，不走 event（publish 是主动动作）
- `/events/estop` 类：走 event + publish 双通道（event 通知 wakeup，publish 带载荷）
- Step 5 先落 publish_subscribe，Event service wrapper 放 Step 5 末段或 Step 6 初段

### 影响面

| 文件 | 调整 |
|---|---|
| `Cargo.toml`（workspace 根） | `[workspace.dependencies]` 追加 `iceoryx2 = "=0.8.1"` |
| `crates/robot-transport/Cargo.toml` | `[dependencies]` 可选 `iceoryx2 = { workspace = true, optional = true }`；`[features]` 加 `iceoryx2 = ["dep:iceoryx2"]`；默认 feature 从 `["mock"]` 改 `["mock", "iceoryx2"]`（macOS 烟测已验证） |
| `crates/robot-msg/src/size_tier.rs` | 新增：`SizeTier` / `default_tier_of` + 穷举测试 |
| `crates/robot-msg/src/lib.rs` | 导出 `size_tier::*` |
| `crates/robot-transport/src/iceoryx2/` | 新增：`mod.rs` / `service_name.rs` / `qos.rs` / `publisher.rs` / `subscriber.rs` / `attributes.rs`（Event wrapper 酌情） |
| `docs/01_implementation_steps.md` Step 5 | 交付清单具体化（后续 commit） |
| **`docs/00_design.md`** | **不改**。QoS 映射 + size tier 是实施细节；§2.4 类别定义不变 |

### 约束

1. **iceoryx2 使用范围**：仅 `robot-transport::iceoryx2` 子模块。其他 crate（`robot-msg` / `robot-s0` / `robot-s1` / adapters）禁止 `use iceoryx2`。CI 后续加 grep 拦截。
2. **Service attribute 键空间**：目前只用 `schema_id` / `schema_version`；未来增加 `layout_id` 等需登记新条目。
3. **Config 路径的 override 未在 Step 5 落地**：Step 5 只落 default tier；override 机制推到 v0.3 ConfigIdentity 专项时一起做。

### 审阅

- 提议：Claude Code（Step 5 开工决策，含 macOS 烟测）
- 决定：用户 loosqk（2026-04-22 session 内逐条拍板：①⑥ 烟测通过 → ② 从 B 改回 A；④ 4 档 + 两层 + control 升 8 KiB；⑤ 先看 C 成功 → C；⑦ B）
- 复审触发：
  - iceoryx2 0.9.x / 1.0 发布时重跑烟测 → 更新精确 pin
  - 出现单个 topic 实际需要穿越档位（含升降档）→ 走 TOML override 而非扩充默认档
  - macOS 烟测 regression（iceoryx2 新版 macOS 支持回退）→ 撤回 §3.1 默认 feature，`mock` 回到默认

---

## 2026-04-24 · Target 契约登记机制 + XLeRobot 建 target 骨架（**状态：target-contract · 新增登记类别**）

### 涉及小节

- 本文件序言（新增第 3 类条目："Target 契约登记"）
- `docs/README.md` Spec 分工表（Target spec 行范围精化）
- `docs/targets/README.md`（登记规则表 + 优先级表）
- `docs/targets/xlerobot.md`（新增，骨架）
- 不动 `docs/00_design.md`（target 本就不进设计文档）

### 背景

v0.1.2 冻结设计只管 framework 语义。随 `targets/` 目录落地（2026-04-23 新增 atom01.md）和 2026-04-24 重构（契约 / 参考 / 路线图三分），出现一个问题：**target 文档里的"训练 / 部署硬契约"** 一旦改动，后果与改 framework 语义相当（破坏会直接导致机器人抽搐 / 起飞），但本文件此前只登记 framework / design 偏离，没有收口 target 契约变更。

同时，原 `targets/README.md` 优先级表把 XLeRobot 标为"暂不独立成 target spec"（理由：现役栈在 dimos、指针已在 `02_start_here.md`）。这不对称：ATOM01 作为 v0.5 候选已建 target，XLeRobot 作为 v0.4 首个 adapter 落地目标反而没有，无法让 framework 和 target 两侧对齐演进。

### 决定

1. **本文件序言**从"两类条目"扩成"三类条目"，新增第 3 类：**Target 契约登记** —— 仅覆盖 `targets/<name>.md` §2《训练 / 部署契约》节的新增 / 修改 / 删除。
   - 同文件其他节（§1 硬件清单 / §3 上游实现参考 / §4 映射 / §5 adapter / §6 风险 / §7 参考）**不进本文件**，PR 直接改。
   - 辅助文件（`_migration.md` 路线图 / `_<subsystem>_<commit7>.md` 源码快照）**不进本文件**。
   - 详细规则表见 `targets/README.md`《Spec 登记规则》。

2. **XLeRobot 建 target 文档**：新增 `targets/xlerobot.md`，上游参考 `~/repo/robot/dimos`（Python + dimos 框架），按 6 节骨架组织。
   - v0.4 前是骨架态（§2 契约大量 TBD 项，标注"v0.4 开工时填实"），不阻塞开工；
   - 已知的硬契约（Feetech STS3215 寄存器序列 / 三轮全向运动学 / 校准 offset 持久化）先写进去，后续 v0.4 时补完整数值；
   - 撤回 `targets/README.md` 此前"XLeRobot 不补独立 target 文档"的说明。

### 理由

- Target 契约破坏的后果 = framework 不变量破坏的后果（物理后果，不是软件 bug）。既然 framework 走 CHANGELOG，target 契约也应该走，**统一入口**。
- 只登记 §2 契约节是精确边界：路线图 / 源码快照 / 硬件清单变动节奏快，全收到 CHANGELOG 会淹没 framework 条目。
- XLeRobot 建骨架比"等 v0.4 再建"更好：v0.4 实施时发现契约冲突就晚了，先把骨架和 TBD 清单亮出来，framework v0.3 阶段就能感知到 target 侧要什么。

### 约束

1. **Target 契约登记条目的状态标签用 `target-contract`**，不混用 `偏离` / `已回写` —— 因为这类条目不触发回写 dimos 侧设计文档（dimos 侧没有对应 target 契约定义）。
2. **`targets/README.md` 的《Spec 登记规则》表是权威**：本文件序言只引用，不复述规则。
3. **XLeRobot 骨架里的 TBD 项在 v0.4 开工填实时**，每填一条都走一次本文件（target-contract 登记）。
4. 不动 `docs/00_design.md` —— target 文档本就不进设计冻结范围（`00_design.md` 附录 B 只列 dimos ↔ robot-core v0.1.2 对应关系，不规定 target 硬契约）。

### 影响面

| 文件 | 调整 |
|---|---|
| `docs/CHANGELOG.md` 序言 | 两类条目 → 三类条目；新增 "Target 契约登记" 说明 + 指向 `targets/README.md` |
| `docs/CHANGELOG.md`（本条） | 新增本条目 |
| `docs/README.md` Spec 分工表 | Target spec 行本就限定 §2（2026-04-24 上午已改）；本次新增指向"登记规则表" |
| `docs/targets/README.md` | 优先级表 XLeRobot 一行：文档列从"见 §11.3（暂不独立成 target spec）" → `[xlerobot.md](xlerobot.md)`；撤回相应说明段 |
| `docs/targets/xlerobot.md` | **新增**。6 节骨架；§2 大量标 TBD |
| `docs/02_start_here.md` 上游参考段 | 不动；`xlerobot.md` 里重新整理成 target 视角的指针即可 |
| `docs/00_design.md` | **不动**。设计冻结 |

### 审阅

- 提议：Claude Code（2026-04-24 session 内根据用户"深入交流 target 文档"建议整合）
- 决定：用户 loosqk（2026-04-24 session 内明确："targets/ 进 docs/CHANGELOG.md 登记，仅契约。dimos 也进 target"）
- 复审触发：
  - 第二个真 target（Piper 立项 / 四足）落地，发现 6 节骨架或 §2 契约格式需调整 → 本文件登记一次骨架修订，再统一迁移已有 targets；
  - XLeRobot v0.4 开工，§2 TBD 项全部填实后 → 状态从"骨架"转成"v0.4 实施 spec"，同时追加一条 target-contract 条目登记填实范围；
  - 发现 `_migration.md` 路线图步骤改动也会导致契约解释歧义（实践检验） → 评估是否把"路线图重大调整"也纳入本文件。

---

## 2026-04-24 · S0 `s0_rt_thread` 多 bus 语义澄清 + HardRT 线程拓扑方案 B（**状态：登记 / 候选回写 §1.2**）

### 涉及小节

- `docs/00_design.md` §1.2 "S0 四线程模型"（候选回写；本条先登记，不改冻结正文）
- `docs/00_design.md` §4.9 "RT 铁律"（不动，语义已兼容方案 B）
- `docs/targets/atom01.md` §5.1（同步更新 RT 风险表）
- `docs/targets/xlerobot.md` §5.1（XLeRobot 侧 feetech 多总线同规则）
- `CLAUDE.md`（项目）"S0 线程模型（v0.2 骨架）"小节
- `RtSnapshotWriter` API（新增 per-bus slice 维度，v0.4 多 bus 开工前定形）

### 背景

`docs/00_design.md` §1.2 描述 S0 线程模型时写的是单数 `s0_rt_thread`：

```
s0_ipc_thread    普通优先级
s0_rt_thread     SCHED_FIFO    读 command slot → HardRT adapter → 写 rt_snapshot
s0_pub_thread    普通优先级
s0_io_thread     普通优先级
```

v0.2 Step 6 单 bus 场景下字面成立。但 ATOM01（4 × CAN）和 XLeRobot（2-3 × USB-RS485）都是多 HardRT bus；今天（2026-04-24 session）通读 `~/repo/robot/atom01_deploy` 上游 C++ 实现后，发现原栈 `src/motors/src/protocol/can/socket_can.cpp` 采用**每 bus 2 个 SCHED_FIFO prio=80 线程（RX 异步 `select()` + TX 阻塞 cv + Boost lockfree queue）+ 上层 `ThreadPool(4)` fan-out**，4 bus 膨胀到 13 个 RT 线程。且 `src/motors/src/drivers/dm/dm_motor_driver.cpp:135-160` 的 RX callback 在 I/O 线程里直接 `logger_->error(...)` format + 写非 atomic `float` 成员，**既违反 §4.9 RT 铁律，也有数据竞态**。不能作为 robot-system 的参考实现。

用户（loosqk）本 session 明确自己 EtherCAT 背景的模式：**单 RT 线程 [read → update state queue → read target queue → safety → write bus]**。EtherCAT 单 cycle 一帧遍历 ring，PDO 天然 1:1 同步；CAN 是多帧 bus，每电机一帧，RX 异步到达。但只要在 tick 开始做 non-blocking RX drain，EtherCAT 的单线程闭环就能迁移到 CAN。

### 决定

1. **多 bus 场景下，S0 `s0_rt_thread` 从"单例"重释为"每 HardRT bus 一实例"**。单 bus 场景（v0.2 Step 6 mock、Step 10 Feetech 单关节）仍然 M=1，与冻结正文字面兼容。
2. **每 rt_thread 的 tick 闭环**（方案 B）：

   ```
   loop {
       clock.sleep_until(deadline); deadline += tick_period;
       bus.drain_rx();                          // non-blocking，到 EAGAIN 停
       cmd = target_slot.load_latest();         // SampleLease，latest-wins
       if let fault = safety_check(&state, &cmd) {
           bus.safe_output(); snapshot.publish_fault(fault); continue;
       }
       bus.write_frames_serial(&cmd, &state);   // N 次 write()，稳态 <10 µs/次
       snapshot.write_bus_slice(bus.id, &state);
   }
   ```

3. **`RtSnapshotWriter` 加 per-bus slice 维度**：每个 rt_thread 只写自己 slice（seqlock），消费者（pub_thread / s1 coordinator）读多 slice 组装全关节快照。跨线程锁 0 个。具体 API 形状 v0.4 多 bus 开工前定形，v0.2/v0.3 保持单 slice 等价实现。
4. **跨 bus compute（闭链解耦 / IK / 运动学）不进 rt_thread**，挪 `robot-s1` coordinator（§1.3），保 §4.9 RT 铁律。
5. **write() 异常处理**：`EAGAIN` → 记 metric + 跳过该帧，rt_thread 不 retry；恢复策略上抛 s1 / 业务 app。`SO_SNDBUF` / `txqueuelen` 配足是前提（`SO_SNDBUF ≥ 1 MB`，`txqueuelen ≥ 1000`），否则是配置 bug。
6. **CPU affinity**：每 rt_thread 绑独立核（避免 SCHED_FIFO 跨核迁移抖动），atom01 证实过的 defensive pattern 保留。

### 理由

- **契合 §1.2 语义**：§1.2 的 rt_thread 职责（读 target → 调 adapter → 写 snapshot）本身就是方案 B 的描述，只是从单例 → M 实例。不破坏冻结语义，只澄清多 bus 行为。
- **RT 铁律更容易守**：单线程闭环，无 async callback / 跨线程 format / 无锁队列；atom01 踩过的 spdlog-in-callback 坑自动规避。
- **1 kHz 够用**：单 bus 稳态 ~60 µs write + ~100 µs drain，<1 ms tick 裕度 10×。4 bus 并行跑 1 kHz 需要 ~4 个独立核，Orange Pi 5+（RK3588 8 核）/ Orin 8+ 核够。
- **比 atom01 简 8 倍线程**：13 → 4（ATOM01），13 → 2 或 3（XLeRobot），抖动源显著减少。
- **用户 EtherCAT 经验可复用**：只加"drain RX 前缀"，其他结构一一对应。

### 约束

1. **HardRT adapter trait 按"单 bus 单实例"设计**，不假设能调度多 bus；M bus = M adapter 实例。
2. **所有跨线程状态只走 seqlock / atomic / SampleLease**；rt_thread 共享成员不得是非 atomic 基础类型。
3. **spdlog / tracing-format / 任何 format 参数构造不进 rt_thread**（含入站帧处理内联路径），只能 `AtomicU64::fetch_add` 计数，dump 由 pub_thread 完成。
4. **不在 rt_thread 里 spawn 任务或调 ThreadPool**；bus 内串行写 N 帧就够。
5. **watchdog / `sd_notify` 超时检测保留**：rt_thread 卡 > 2 tick → systemd 杀进程 → 硬件 command timeout 兜底（Feetech 100 ms / DM 类似）。
6. **具体 per-bus slice snapshot API 形状推迟到 v0.3 末 / v0.4 初**（ATOM01 / XLeRobot 第一次真多 bus 落地时）；v0.2 Step 6-10 仍 M=1，本条不阻塞当前实施。

### 影响面

| 文件 | 调整 |
|---|---|
| `docs/00_design.md` §1.2 | **不改正文**（冻结）。多 bus 语义澄清在本条目登记；正式回写等第二个实测数据（XLeRobot v0.4 或 ATOM01 v0.5） |
| `docs/CHANGELOG.md` 本条 | 新增（**本次改动**） |
| `docs/targets/atom01.md` §5.1 | 风险表删除"非阻塞 fd 或 io_thread 代写"的错误建议；新增"每 bus 一个 rt_thread + per-bus seqlock slice"正解描述（**本次改动**） |
| `docs/targets/atom01.md` §4.1 | 表格 "S0 四线程" 行加脚注指向本条目（**本次改动**） |
| `docs/targets/xlerobot.md` §5.1 | feetech adapter 条目补"每 USB-RS485 一个 rt_thread"约定（**本次改动**） |
| `CLAUDE.md`（项目）"S0 线程模型（v0.2 骨架）" | v0.2 单 bus 骨架不动；加一行脚注指向本条目（**本次改动**） |
| `docs/01_implementation_steps.md` Step 6 | 不改（Step 6 仍 M=1 mock）；Step 10 Feetech 单关节也仍 M=1 |
| `crates/robot-transport::RtSnapshot*` API | v0.4 前不改；届时加 per-bus slice 维度 |
| `memory/rt_thread_per_bus_topology.md` | 新增 feedback memory（**本次改动**） |

### 审阅

- 提议：Claude Code（2026-04-24 session 内读完 atom01 上游 socket_can.cpp / dm_motor_driver.cpp / thread_pool.hpp / robot_interface.cpp 后对比用户 EtherCAT 经验整理）
- 决定：用户 loosqk（2026-04-24 session 内明确："就用方案b"）
- 候选回写 §1.2 的触发：
  - XLeRobot v0.4 真多 bus 落地 + cyclictest 1 h p99 < 1.5 ms、RT thread alloc = 0、`write()` 稳态 EAGAIN 计数 = 0 满足 → 正式回写 §1.2 为"每 HardRT bus 一实例"（走回写登记）
  - 如第一次真多 bus 实测发现 B 不达标（EAGAIN 频发 / 抖动超标）→ 评估回退到 atom01 式 TX queue 方案，**但 RX 仍不走 async callback**（保 §4.9 RT 铁律）
- 复审触发：
  - EtherCAT 真 slave 落地（≥ v0.6） → 验证本模式在 PDO 场景下依然成立（预期：drain_rx 变成"读 last cycle PDO"，接口一致）
  - Step 10 Feetech 单总线实测 `write()` 稳态延迟分布 → 写进 `docs/10_verification.md` 作 baseline

---

## 2026-04-24 · §4.7 ExternalPolicyHost 在 RT 内核下调度等价 inline + iceoryx2 RT baseline 提前到 Step 6（**状态：登记 / 验收条件冻结**）

### 涉及小节

- `docs/00_design.md` §4.7 "Policy runtime（InProcess vs External）"（不改正文；本条**收口 External 路径在 RT 内核下的调度风险**）
- `docs/01_implementation_steps.md` Step 6 退出标准（新增 iceoryx2 RT baseline）
- `docs/10_verification.md` §10.1.3 Transport（新增 iceoryx2 RT 行为指标）
- `docs/targets/atom01.md` §4.3 延迟预算 + §6 风险与门槛
- `docs/targets/xlerobot.md` §6 风险与门槛

### 背景

2026-04-24 session 里评估"`robot-system` 能否替代 `atom01_deploy`"时，唯一未 closed 的架构级风险是 **policy_host 独立进程 vs inline ONNX 推理的调度抖动** ——原栈 `inference_thread` SCHED_FIFO 70 inline，robot-system 走 iceoryx2 发/收 obs/action 到独立进程。

用户（loosqk）指出：**atom01 运行环境已经是 5.10 RT 内核（OPi 5+ 装 `assets/*.deb`）或 6.1.83 RT（RDK X5 出厂 RT）**，`rtprio 98 + memlock unlimited` 就位。在 RT 内核 + SCHED_FIFO + CPU pinning + mlockall 下，**独立进程的调度确定性与 inline 等价**，所谓"进程切换抖动"仅是未配 RT 调度时的默认 SCHED_OTHER 行为。

### 决定

1. **§4.7 External 路径在 atom01 部署环境（RT 内核）下不是架构风险**。具体配置：
   - `SCHED_FIFO prio = 70`（与原栈 `inference_thread` 同级）
   - 绑一个 A76 大核（RK3588 4 A76 + 4 A55，A76 ~2.4 GHz；若 A76 不够则让给 CAN rt_thread，policy_host 退 A55，实测取舍）
   - `isolcpus=` 内核 bootarg 剔掉 policy_host 所用核（避免 kernel 随意调度其他任务）
   - `mlockall(MCL_CURRENT | MCL_FUTURE)` 进程启动第一件事
   - `ort::intra_threads = 1` + 启动 2 次 warmup 丢弃（触发 MemPattern + JIT）

2. **XLeRobot（Jetson Orin 默认非 RT 内核）保留 §4.7 InProcess 分支作 fallback**。v0.4 立项时决定是否装 RT 内核；若不装，`robot-policy-host` 以 `feature = "inprocess"` 编进 `robot-s1`，丧失崩溃隔离换调度确定性。

3. **iceoryx2 pub/sub 的 RT 行为 baseline 提前到 Step 6 必产**。此前计划在 v0.5 ATOM01 落地时才测；user 明确"iceoryx2 要提前测"，改为 Step 6 退出标准硬门槛：
   - 场景：50 Hz publish、10 min、Linux RT 内核 + `SCHED_FIFO 70` + CPU pin
   - 指标：receive 时刻相对 publish 的 delta，p99 < 100 µs，p999 < 500 µs
   - 实测环境：`linux-x86_64-nightly-YYYYMMDD` env_id 登记到 `docs/10_verification.md` 附录

### 验收清单（三条）

与 atom01 替代 policy_host 部分相关的验收项：

| 验收项 | 目标 | 落地 Step | 状态 |
|---|---|---|---|
| iceoryx2 RT 行为 baseline | p99 < 100 µs、p999 < 500 µs（50 Hz / 10 min / RT 内核 + SCHED_FIFO）| **Step 6** | ⏳ |
| ORT 稳态 0 alloc（warmup 后）| 第 3 次及之后 `Run()` 经 `assert-no-alloc` 不触发 | v0.5 `robot-policy-host` 落地 | ⏳ |
| 双 shm round-trip (obs + action) | p99 < 50 µs | v0.5 policy_host 接入 | ⏳ |

三条全过 = `§4.7` External 路径在 atom01 部署环境收口；任一不过 → 切回 `InProcessPolicyHost`（`robot-s1` feature flag），设计 §4.7 的 fallback 路径已冻结，无需再评审。

### 理由

- **RT 内核在手**：atom01 硬件环境既定前提，不是额外工程成本。原栈已经依赖 `rtprio` / `memlock`，robot-system 不多付出。
- **inline 和 External 的调度差距在 RT 内核下 = iceoryx2 shm 双向 round-trip 的延迟**：预期 p99 < 50 µs，在 20 ms 推理周期（50 Hz）上是 < 0.25% 预算。
- **ORT 自身是 CPU-bound 计算**（~5-15 ms Run），整个推理期间 policy_host 线程独占核，正好是 SCHED_FIFO 的设计意图；非 RT 内核下才会被抢占。
- **iceoryx2 RT baseline 提前到 Step 6** 的价值：Step 6 是第一个 Linux RT 场景，一起测 cyclictest + iceoryx2 roundtrip 时间机器; 拖到 v0.5 才发现 iceoryx2 本身不够 RT，返工成本高。

### 约束

1. **iceoryx2 RT baseline 不过 → block Step 6 退出**。若 p99 / p999 达不到，不继续 Step 7–10，先调查原因（iceoryx2 版本 / kernel tunable / 测试方法）。
2. **Step 6 iceoryx2 RT 测试必须配 SCHED_FIFO + CPU pin + mlockall**，不是"macOS 开发机随便跑"—— 设备必须是 Linux RT 内核（用户 Linux 开发机 / Orin / Orange Pi 5+ 其中之一）。
3. **RT 内核 tunable 清单**（写入 `scripts/collect_env_id.sh` 采集项）：
   - `/proc/cmdline`（`isolcpus`、`rcu_nocbs`、`threadirqs`、`idle=poll`）
   - `/sys/devices/system/cpu/cpu*/cpufreq/scaling_governor`（建议 performance）
   - `/proc/sys/kernel/sched_rt_runtime_us`（不能限制 RT 抢占）
4. **XLeRobot 不强制 RT 内核**：v0.4 立项时实测 Orin 默认 kernel 下 InProcess 和 External 两种模式延迟差，决定是否装 RT。

### 影响面

| 文件 | 调整 |
|---|---|
| `docs/01_implementation_steps.md` Step 6 退出标准 | 新增 "iceoryx2 RT behavior baseline 落 `docs/10_verification.md`"（**本次改动**） |
| `docs/10_verification.md` §10.1.3 | 新增 "iceoryx2 publish→receive 延迟 p99 < 100 µs / p999 < 500 µs（RT 内核 + SCHED_FIFO 70）" 指标行，状态 ⏳ Step 6（**本次改动**） |
| `docs/targets/atom01.md` §4.3 | 延迟预算段落改写：显式说 RT 内核在手，policy_host 调度与 inline 等价（**本次改动**） |
| `docs/targets/atom01.md` §6 | policy_host 抖动 + SocketCAN write() 两条从风险降级到验收项（**本次改动**） |
| `docs/targets/xlerobot.md` §6 | 新增 "policy_host 调度抖动（Orin 默认非 RT 内核）" 风险 + v0.4 取舍说明（**本次改动**） |
| `docs/CHANGELOG.md` 本条 | 新增（**本次改动**） |
| `docs/00_design.md` §4.7 | **不改正文**（冻结，InProcess / External 两路本就都在）；本条收口 External 在 atom01 场景的判断 |

### 审阅

- 提议：Claude Code（2026-04-24 session 内"RT 内核可用 → policy_host 调度抖动不是真风险"整合）
- 决定：用户 loosqk（2026-04-24 session 内明确："policy_host 可以用绑定内核加高优先级测试。因为 atom01 运行环境已经是 RT kernel" + "iceoryx2 要提前测，都落进去"）
- 复审触发：
  - Step 6 iceoryx2 RT baseline 测出来 p99 超标 → 评估换 iceoryx2 版本 / 或切 InProcess 作 atom01 默认
  - v0.5 ATOM01 `robot-policy-host` 落地实测若 ORT warmup 后仍有 alloc → 加 preallocated arena 或切 InProcess
  - XLeRobot v0.4 实测 Orin 默认 kernel 下 External 能用 → 回撤 InProcess fallback 的必要性；装 RT 内核变成可选

---

## 2026-04-24 · Step 6.5 RT 调优搁置 + XLeRobot 不卡 §10.1 RT 硬门槛（**状态：偏离登记**）

### 涉及小节

- `docs/00_design.md` §10.1 "S0 loop / Safety / crash-restart 指标"（**不动**；本条只把 XLeRobot target 的适用性收口，§10.1 数值对 ATOM01 / 人形 / 四足 仍是契约）
- `docs/01_implementation_steps.md` 顶部"当前位置"行（同步标 Step 6.5 暂不做的理由）
- `docs/targets/xlerobot.md` §1 实时内核行 + §6 风险表（已注 "Orin 默认即可"，本条把 Step 6.5 搁置理由补足）
- `.github/workflows/ci.yml`（**本次改动**：仅挂宽阈值 ubuntu smoke，不挂 Orin self-hosted strict-rt job）
- `scripts/baseline.sh`（**本次改动**：新建封装，本机 / Orin 两模式，门槛不严卡）

### 背景

`docs/handoffs/2026-04-24_5.md` §5 task 22 实测 Orin Nano Super 三轮 60s/30s strict-rt：

```
配置                  | p50    | p99       | p999      | max
--duration 60s       | 22.5µs | 1018.4µs  | 19955.7µs | 20021.2µs   FAIL
--duration 60s --cpu4| 20.5µs | 5746.7µs  | 6447.1µs  | 6504.4µs    FAIL
--duration 30s --cpu4| 21.0µs | 15237.1µs | 15491.1µs | 15499.3µs   FAIL
```

p50 21 µs 表稳态 OK，p99 1–15 ms 是 PREEMPT 内核 + schedutil governor + 无 isolcpus 的系统瓶颈（详 handoff §5.2）。原计划走 handoff §5.3 的 sudo 调优清单（governor / cpu_dma_latency / isolcpus / PREEMPT_RT 内核）→ Step 6.5 闯门槛。

用户（loosqk）2026-04-24 明确两条搁置 Step 6.5 的理由：

1. **XLeRobot 不需要高实时性**：Feetech STS3215 自带 command timeout，硬件层 fail-safe；上层 50 Hz / 20 ms 周期对 100 µs / 500 µs 没有刚需。原 §10.1 硬门槛是按 ATOM01 5×CAN 多 bus 同步刚需推导的（2026-04-24 多 bus 拓扑方案 B 条），XLeRobot 单 USB-RS485 不是同一量级。
2. **Orin Nano 算力本身偏弱**：6 核 A78AE，A78AE 相对 RK3588 A76 缺 RT 内核厂家支持（JetPack 默认 PREEMPT 不是 PREEMPT_RT），自编 PREEMPT_RT 内核 + driver 重新调通的工程成本，对一个不需要硬实时的目标不划算。

### 决定

1. **Step 6.5（Orin RT 调优攻克 §10.1 硬门槛）暂不做**。`docs/01_implementation_steps.md` 10 步主线不受影响（Step 6 退出标准里"iceoryx2 RT baseline harness 就位 + 跑通"已在 task 21 落地，门槛达标 *不是* Step 6 退出条件 —— 见 handoff §1）。
2. **`docs/00_design.md` §10.1 数值不动**：对 **ATOM01 / 四足 / 人形** 等目标仍是契约（这些 target 跑 RT 内核 + 多 bus，门槛是真刚需）。XLeRobot 是 §10.1 数值的 *target-specific 不适用*，不是数值本身的修订。
3. **CI 不挂 Orin self-hosted runner 的 strict-rt baseline job**：理由同 (1)，挂上去会因系统瓶颈持续红 CI；ubuntu-latest 30s smoke job 仍要（本次新增），用宽阈值 p99 ≤ 50 ms / p999 ≤ 100 ms，作用是防 `iceoryx2_rt_baseline` harness 代码退化。
4. **`scripts/baseline.sh`**（本次新增）封装 handoff §5.4 的两条命令，Orin 真要刷数据时手动跑：
   - `./scripts/baseline.sh local [DUR]` —— 本机 release smoke
   - `./scripts/baseline.sh orin  [DUR]` —— ssh Orin 跑 strict-rt（默认 60s + `--cpu 4`）
5. **XLeRobot target 走 §4.7 InProcess fallback** 已在 2026-04-24 "§4.7 ExternalPolicyHost RT 等价 inline" 条 §决定 (2) 锁定，本条只追加注脚：v0.4 立项时的"是否装 RT 内核"决定可以 *直接拍板不装*，理由是 (1)+(2)。

### 约束

1. **本条只对 XLeRobot 生效**。其他 target（ATOM01 / 四足 / 人形）开新 spec 时，`§5/§6` 风险表必须显式声明 "适用 §10.1 RT 硬门槛"，否则视同同一豁免。
2. **`docs/00_design.md` §10.1 文本不能加 "XLeRobot 例外"**（设计冻结）；解释一律放在本 CHANGELOG 条 + `targets/xlerobot.md` §1+§6。
3. **复审触发**：
   - XLeRobot 加第二条 USB-RS485（双臂 / 移动底盘 + 头部）→ Feetech bus 数 ≥ 2 后重新评估是否进入 §10.1 门槛域；
   - dimos 现役栈实测出 ≥ 30 ms 控制延迟 spike 导致行为退化 → 评估是否要装 RT 内核 + cpu_dma_latency；
   - ATOM01 v0.5 落地，发现 §10.1 数值需要进一步细化（per-bus / per-target 数值表）→ 顺带把 XLeRobot 行写进去。
4. **scripts/baseline.sh 的宽阈值不能视作"达标线"**。它是 *harness 不退化* 的最低线；真要拿数据走严阈值需手动传 `--p99-us 100 --p999-us 500`（即 §10.1 数值）。

### 影响面

| 文件 | 调整 |
|---|---|
| `.github/workflows/ci.yml` | 新增 `baseline-smoke` job（ubuntu，30s，宽阈值，依赖 `test`）（**本次改动**） |
| `scripts/baseline.sh` | 新增（**本次改动**） |
| `docs/01_implementation_steps.md` | 顶部"当前位置"行同步：Step 6.5 不做 + 理由指针（**本次改动**） |
| `docs/targets/xlerobot.md` | §1 实时内核行 + §6 风险表注脚补 "Step 6.5 搁置 → 走 InProcess fallback"（**本次改动**） |
| `docs/CHANGELOG.md` 本条 | 新增（**本次改动**） |
| `docs/00_design.md` §10.1 | **不动**（设计冻结） |
| `docs/handoffs/2026-04-24_5.md` | **不动**（handoff 本身就是历史快照，不回写） |

### 审阅

- 提议：Claude Code（2026-04-24 session 内 Step 7 task 24 入手时）
- 决定：用户 loosqk（2026-04-24 session 内明确："推迟 6.5 的原因是 1. 他跑的 xlerobot 不需要高实时性。2. orin nano 本身性能就弱"）
- 复审触发：见上方"约束 §3"

---

## 2026-04-24 · Step 7 task 25 robot-s0 / robot-core::config 依赖白名单更新 + ConfigIdentity v0.2 部分骨架（**状态：登记 / 部分实现**）

### 涉及小节

- `docs/00_design.md` §7.2（TOML schema）+ §7.3（`ConfigIdentity`）—— **不动**正文
- `docs/CHANGELOG.md` 2026-04-22 "workspace runtime 依赖白名单机制" 各 crate 子清单（本条更新 `robot-s0` / 新增 `robot-core::config` 例外）
- `crates/robot-core/src/config.rs`（**本次新增**：`IdentityFingerprint` 类型 + 4 单测）
- `crates/robot-s0/src/config_loader.rs`（**本次新增**：TOML schema strut + 加载入口 + 9 单测）
- `crates/robot-s0/src/bin/s0_daemon.rs`（**本次改动**：`Args` → `ResolvedArgs` 拆分 + `--config` 参数 + identity 打印）
- `configs/xlerobot.toml`（**本次新增**：v0.2 示例）
- `Cargo.toml` workspace.dependencies 加 `serde` / `toml = "0.9"`

### 背景

设计 §7.3 `ConfigIdentity` 是跨进程启动 handshake 的核心：5 个 hash + `robot_id` + `config_version`，用于把"S0 升级后 joint 顺序变了，S1 还用旧配置"这类隐性事故拦在启动期。

`docs/handoffs/2026-04-24_5.md` §8 task 25 入口清单要求：v0.2 阶段 **替代纯 CLI 模式**，从 `configs/<robot>.toml` 读取配置 + 算 `ConfigIdentity`。但 §7.3 描述的"跨进程 iceoryx2 request-response 互换 + 不一致进 `DEGRADED`" 完整实现需要：calibration / schema_registry / adapter inventory **三个 v0.3+ 子系统**全部就位才能算齐 4 hash。强行 v0.2 全做会扯 v0.3 才该做的事。

### 决定

1. **拆分 v0.2 partial vs v0.3 full**：
   - v0.2 只算 `config_hash`（blake3 over TOML 原始 bytes）+ `robot_id` + `config_version`；
   - 余 `calibration_hash` / `schema_registry_hash` / `adapter_registry_hash` 留 `[0u8; 32]` placeholder；
   - 类型 `IdentityFingerprint::v02_partial(...)` + `is_fully_partial() -> bool` 把"v0.2 占位"语义显式化，避免 v0.3 误用 placeholder 做真比对。
   - Display 在 placeholder 字段后缀 `*`，人眼一秒分辨"v0.2 vs v0.3"。
2. **跨进程 handshake 推 v0.3+**：本任务**不**改 state machine、**不**接 iceoryx2 request-response、**不**实现 `DEGRADED` 分支。s0_daemon 仅启动期打印一次 identity，仅供观测。
3. **类型分层 + 依赖隔离**：
   - 纯类型 `IdentityFingerprint` → `robot_core::config`，**无** serde derive，恪守 §4.2 "零 runtime 依赖"。
   - TOML 解析逻辑（含 `serde` derive + `toml::from_str`）→ `robot_s0::config_loader`。`robot-s0` 是 Linux binary crate，加 `serde` / `toml` / `blake3` / `thiserror` 不污染跨平台核心。
   - v0.3 起若 `robot-s1` / `robot-recorder` 也要读同一份 TOML，再抽 `robot-config` 共享 crate（v0.2 单 binary 不值得抽）。
4. **CLI > TOML > env > 默认 优先级**：
   - `s0_daemon --robot-id` 仍可 override TOML `[robot] id`，便于多实例调试 / 同 robot 多硬件 (`--hardware-id` 同理)。
   - **identity 始终来自 TOML**（不跟 CLI override 走）：v0.2 阶段允许这一不对称，因为没有跨进程 handshake 真消费 identity；v0.3 跨进程 handshake 落地时若发现 CLI override 导致 identity 漂移引发问题，再决策（候选：CLI override 时强制重算 identity，或禁止 CLI override 已有 `--config` 字段）。
5. **TOML schema 字段宽松**：`[[hardware]]` / `[[sensor]]` 仅 `id` + `adapter` 必填，其他字段（`baud` / `wheel_radius` / `joints` / `streams` / ...）按需出现，未知字段不报错，由 `extras: toml::Table` flatten 兜底。v0.4 真 adapter 落地时按 adapter 名做 enum dispatch；v0.2 不锁 schema。

### 各 crate 白名单更新

#### `robot-core`（§4.2 + 2026-04-21 libc 例外，本次**不变**）

- `robot-core::config` 模块**只**用 std + 自身已有依赖（无 `serde` / `toml` / `blake3`）。
- 新增类型 `IdentityFingerprint` 是纯 POD（`String` + `[u8; 32]`），不破坏"零 runtime 依赖"。

#### `robot-s0`（v0.2 Step 6 → Step 7 task 25 扩展）

| 依赖 | 范围 | 用途 | RT 影响 |
|---|---|---|---|
| `serde = { workspace = true, features = ["derive"] }` | **本次新增** | `config_loader::RobotConfigToml` deserialize | 仅 daemon 启动期 parse；RT 线程未 link |
| `toml = { workspace = true }`（v0.9） | **本次新增** | TOML 解析 | 同上；与 iceoryx2 0.8.1 子树同 0.9.x，避免 dup-version warning |
| `blake3 = { workspace = true }` | **本次新增** | `compute_v02_partial_identity` | 启动期一次性 hash；RT 线程未调用 |
| `thiserror = { workspace = true }` | **本次新增** | `ConfigLoadError` derive | 同已有 `robot-core::error::RobotError` |

#### workspace.dependencies（首次声明）

```toml
serde = { version = "1", features = ["derive"] }
toml  = "0.9"
```

### 约束

1. **不改 §4.2 robot-core 白名单文字**：本条只在 `robot-s0` 范围引入 serde / toml；`robot-core::config` 仍纯类型。后续若 `robot-msg` / `robot-transport` 等其他核心 crate 想加 serde，单独走 CHANGELOG 评审，不视同本条豁免。
2. **`config_hash` 用 raw bytes 非 canonical 化**：调用方注意 line-ending / trailing newline 差会改 hash。v0.3 跨进程 handshake 实测出"开发机 vs 部署机的 newline 差成痛点"再升 canonical 化。
3. **identity 不参与 v0.2 任何决策**：仅打印一次，禁止任何 v0.2 路径分支（state machine / RPC / topics）依赖 identity 字段值；v0.3 接 handshake 后再开放消费。
4. **`extras: toml::Table` 字段不进类型契约**：v0.4 真 adapter 落地时按 adapter 名 enum dispatch + 强类型 schema；v0.2 的 `extras` 仅作"不报错"占位，不做契约检查。

### 影响面

| 文件 | 调整 |
|---|---|
| `Cargo.toml`（workspace） | `[workspace.dependencies]` 加 `serde` / `toml` |
| `crates/robot-core/Cargo.toml` | **不动**（无新增依赖） |
| `crates/robot-core/src/lib.rs` | `pub mod config` |
| `crates/robot-core/src/config.rs` | 新增 |
| `crates/robot-s0/Cargo.toml` | dependencies 加 `serde` / `toml` / `blake3` / `thiserror` |
| `crates/robot-s0/src/lib.rs` | `pub mod config_loader` |
| `crates/robot-s0/src/config_loader.rs` | 新增 |
| `crates/robot-s0/src/bin/s0_daemon.rs` | `Args` → `ResolvedArgs` + `--config` + identity 打印 |
| `configs/xlerobot.toml` | 新增（v0.2 示例：robot + 3 hardware mock + 1 sensor） |
| `docs/CHANGELOG.md` 本条 | 新增 |
| `docs/00_design.md` §7.2 / §7.3 | **不动**（设计冻结） |

### 审阅

- 提议：Claude Code（2026-04-24 session 内 Step 7 task 25 入手时）
- 决定：用户 loosqk（2026-04-24 session 起手 prompt 已锁定 task 顺序：CI baseline → ConfigIdentity → rt_guard → feetech → 真机）
- 复审触发：
  - v0.3 跨进程 handshake 落地，发现 CLI override identity 漂移成问题 → 评估"identity 跟 CLI 走"还是"CLI override 禁止"（决定 §4 留的口子）。
  - v0.4 真 adapter 落地，发现 `extras: toml::Table` 太宽（adapter 字段拼写错被静默 ignore）→ 评估按 adapter 名 enum dispatch 的实现节奏。
  - 第二个 binary（robot-s1 / robot-recorder）需要读同一 TOML → 评估抽 `robot-config` 共享 crate（决定 §3 留的扩展点）。

---

## 2026-04-24 · Step 7 task 27 robot-adapter-feetech 协议层 + adapter trait 实现（**状态：登记 / 部分实现**）

### 涉及小节

- `docs/CHANGELOG.md` 2026-04-22 "workspace runtime 依赖白名单机制" → 新增
  `robot-adapter-feetech` 子条目
- `docs/01_implementation_steps.md` Step 10 "Feetech 单关节" → task 27 完成
  *协议层 + adapter trait 实现*；剩 *真硬件 wire 路径 + IO 线程* 留 task 28
- `Cargo.toml` workspace.members 新增 `crates/robot-adapters/feetech`
- 不动 `docs/00_design.md` §4.4 / §4.6（trait 形状不变）
- 不动 `docs/00_design.md` §10.1（feetech adapter 自身不引入新 RT 指标）

### 背景

`docs/handoffs/2026-04-24_5.md` §8 task 27 入口清单：把 dimos `feetech_io/`
（8 文件 / ~900 行 Python）+ `06_lerobot_replace_spec.md` 移植成 Rust
adapter，挂 `ManipulatorAdapter` trait。

dimos 端走 `scservo_sdk` Python 绑定；Rust 端**不**链接 `scservo_sdk`，wire
protocol 自实现（packet 头 + checksum + ping/read/write/sync_*）。这样：
- 无 Python / vendor SDK 依赖，CI 可全量编译；
- 协议层每帧字节级单测可比对；
- task 28 真机阶段只换 `SerialTransport` 实现，protocol / bus / adapter 层
  零改动。

### 决定

1. **新 crate `robot-adapter-feetech`**（`crates/robot-adapters/feetech/`）：
   - `registers` —— 14 项控制表 + sign-magnitude 表 + EEPROM 写保护标记 +
     `OperatingMode` / `TorqueMode` enum，字面对照 dimos `registers.py` 117 行。
   - `encoding` —— sign-magnitude encode/decode + LE byte split/combine，对照
     `06_lerobot_replace_spec.md` §1.4 / §2 + dimos `encoding.py`。
   - `protocol` —— `build_ping` / `build_read` / `build_write` / `build_sync_*`
     / `parse_status` / `find_header`，字节级单测对照 Dynamixel protocol 1
     标准（`PING ID=1` = `FF FF 01 02 01 FB`，`WRITE Goal_Position=2048` =
     `FF FF 01 05 03 2A 00 08 C4`）。
   - `transport` —— `SerialTransport` trait + 内存 `MockTransport`（仅单测）。
   - `bus` —— `FeetechBus` 公开 API：`new` / `connect` / `ping` / `read` /
     `write` / `sync_read` / `sync_write`，sign-magnitude 编解码透明，调用方
     一律传/收 `i32`。
   - `calibration` —— `unlock_eeprom` / `lock_eeprom` / `reset_calibration` /
     `write_calibration` / `set_half_turn_homings`，字面移植 dimos
     `calibration.py` 算法。EEPROM `Lock=0` + `Torque_Enable=0` 显式写。
   - `adapter` —— `FeetechAdapter` 实现 `ManipulatorAdapter` trait（9 方法）。
     单位换算：12-bit tick (4096/圈) ↔ rad，中点 = 2047 = 0 rad。
   - `error` —— `FeetechError` 富信息 enum，`From<FeetechError> for RobotError`
     映射成 `&'static str` 静态消息（恪守 robot-core::error 零堆分配约束，
     富信息走 `eprintln!` 在 adapter 层落地）。
   - 83 单元测试，全部用 `MockTransport` 喂虚假回包 → 字节级验证 wire 正确性。

2. **同步阻塞模型 + 文档警告**：v0.2 `read_state` / `write_command` 在调用线程
   内做完整 wire RTT（ms 量级，违反 §4.9 RT 铁律 < 100 µs）。`info().rt_class`
   仍标 `HardRT` —— "目标分类"，不代表 v0.2 已就绪挂 `s0_rt_thread`。task 28
   实测后决定是否引入"adapter 内部 IO 线程 + atomic snapshot"双层。

3. **不引入 `serialport` crate**（task 27 范围）：真 USB-RS485 实现走 task 28
   的 `feature = "real-hw"`，本任务零硬件依赖，CI 可在 mac / Linux runner 全
   量跑通。

4. **不接 inventory + matrix**：留 task 27b（matrix + 自注册），task 27 仅
   交付独立 crate；s0_daemon 当前继续硬 link `robot-adapter-mock`。

### `robot-adapter-feetech` 依赖白名单

| 依赖 | 范围 | 用途 |
|---|---|---|
| `robot-core = { workspace = true }` | dependencies | trait `ManipulatorAdapter` + POD 类型 + `RobotError` |
| `thiserror = { workspace = true }` | dependencies | `FeetechError` derive Display/Error |
| 无 dev-dep | | mock transport 在 src 内自实现，不引外部 |

**预留**（task 28 加）：
- `serialport`（feature `real-hw`）—— USB-RS485 真后端

**禁止**：
- 任何 vendor C/C++ SDK（含 `scservo_sdk`）—— wire protocol 自实现
- `tokio` —— v0.2 同步路径，无 async runtime；task 28 IO 线程方案待定
- `serde` —— calibration JSON 等 v0.4+ 走 `robot-core::config` 共享路径

### 约束

1. **`info().rt_class = HardRT` 是目标分类，不是当前能力**。挂 `s0_rt_thread`
   在 task 28 IO 线程模型落地前**禁止**；当前只能用作离线校准 / 单元测试 /
   非 RT 控制路径（如 robot-cli debug）。
2. **PING 的 model number 通过显式第二步 READ_DATA(addr=3, len=2) 拿到**，
   而非依赖 scservo_sdk 的隐式 PING+READ 复合行为。dimos / lerobot 移植对照时
   注意**两步**对应一次 `bus.ping()` 调用。
3. **EEPROM 写保护必须显式做**：`unlock_eeprom`（`Torque_Enable=0` +
   `Lock=0`）→ 写 EEPROM 寄存器 → `lock_eeprom`（`Lock=1`，**不**重新打开
   `Torque_Enable`）。漏 `Lock=0` → motor 静默忽略写（`06_lerobot_replace_spec.md`
   §11 风险 1）。
4. **rad ↔ raw 单位假设全圈 2π / 4096 tick**：v0.2 不接 calibration / drive_mode
   反转 / 限位裁剪。task 28 真机阶段实测 → 视情况引入 calibration JSON。
5. **velocity / torque raw 单位 v0.2 直接透传 f32**（lerobot 也未做精确换算）。
   task 28 用示波器 + known 命令对照定单位转换。

### 影响面

| 文件 | 调整 |
|---|---|
| `Cargo.toml`（workspace） | members 新增 `crates/robot-adapters/feetech`；workspace.dependencies 加 `robot-adapter-feetech` path |
| `crates/robot-adapters/feetech/` | **新增**整 crate（lib + 9 模块 + 83 单测） |
| `docs/CHANGELOG.md` 本条 | 新增 |
| `docs/00_design.md` | **不动**（trait 形状不变） |
| `docs/01_implementation_steps.md` Step 10 | 后续 task 28 commit 同步更新 "当前位置" |

### 审阅

- 提议：Claude Code（2026-04-24 session 内 Step 7 task 27 入手时）
- 决定：用户 loosqk（2026-04-24 session 内"上真机前冒烟测试 / 全量测试 /
  性能测试 / 火焰图都要搞好" → 拆出 task 27c 阻 task 28；本条只负责协议层 +
  adapter trait 实现）
- 复审触发：
  - task 28 真机实测发现 wire protocol 字节级与 scservo_sdk 不一致 → 回头改 protocol.rs
  - task 28 实测同步 `read_state` 单 tick 延迟 > 5 ms → 引入 adapter 内部 IO 线程 + atomic snapshot 双层架构
  - 第二个 motor 型号需要支持（SMS / SCS 系列）→ registers / sign_bit / model_number 表抽 trait

---

## 2026-04-24 · Step 7 task 27b matrix 编译 + inventory adapter 自注册 + CI 矩阵（**状态：登记 / 实施细节落地**）

### 涉及小节

- `docs/00_design.md` §4.5 "静态注册"（**不动正文**；本条把 §4.5 描述的
  inventory + factory 模式具体实现落地）
- `docs/CHANGELOG.md` 2026-04-22 "workspace runtime 依赖白名单机制" → 新增
  `inventory` 进 `robot-core` / `robot-adapter-mock` / `robot-s0` 白名单条目
- `Cargo.toml` workspace.dependencies 加 `inventory = "0.3"`
- `crates/robot-core/src/factory.rs`（**本次新增**：AdapterBuildContext + AdapterFactory + find_factory + registered_factories + 4 单测）
- `crates/robot-core/src/adapter.rs` 加 `Box<Inner>: ManipulatorAdapter` blanket impl
- `crates/robot-adapters/mock/src/lib.rs` 加 `MockFactory` + `inventory::submit!`
- `crates/robot-s0/Cargo.toml` adapter dep 改 optional + 新增 features `mock-only` / `xlerobot` / `atom01`（基础 `mock` / `feetech`）
- `crates/robot-s0/src/lib.rs` `pub use ... as _` 强制 link（防 dead-code strip）
- `crates/robot-s0/src/bin/s0_daemon.rs` 替换硬 link `MockHardRTAdapter` 为 `find_factory(...).build(ctx)`
- `.github/workflows/ci.yml` 新增 `matrix-build` job（mock-only / xlerobot / atom01 三 combo build + test）

### 背景

`docs/00_design.md` §4.5 + `CLAUDE.md` "robot-kinematics-<robot> 按 feature flag
挂到 robot-s1" 模式：每种机器人配置 link 不同 adapter 子集，s0_daemon 启动期
按 `configs/<robot>.toml` `[[hardware]] adapter = "..."` 字符串实例化对应
factory。

task 24-27 期间 s0_daemon 一直硬 link `MockHardRTAdapter`，没接 inventory 也没
matrix。task 27 落 feetech 协议层后切要解决"不同 robot link 不同 adapter"问题，
否则 task 28 真机 + 后续 piper / realsense 接入时还是改 s0_daemon 硬 link 的
反模式。

### 决定

1. **`robot-core::factory` 模块**：
   - `AdapterBuildContext<'_>` —— v0.2 最小集字段（`instance_name` / `adapter_kind`
     / `joint_names`），不含 `address` / `baud` 等 adapter-specific 字段
     （v0.4 真硬件 adapter 落地时按需扩；扩字段保证不破坏现有 factory 兼容）。
   - `AdapterFactory: Sync + 'static` trait —— `name()` + `build(&ctx) ->
     Result<Box<dyn ManipulatorAdapter + Send>, RobotError>`。
   - `inventory::collect!(&'static dyn AdapterFactory)` + `find_factory(name) ->
     Option<&'static dyn AdapterFactory>` + `registered_factories() -> impl Iterator`。
2. **`Box<Inner>: ManipulatorAdapter` blanket impl** 加在 `robot-core::adapter`
   末尾 —— 让 `Box<dyn ManipulatorAdapter + Send>` 直接满足 `spawn_rt_thread<A>`
   的泛型约束。零 runtime 开销（vtable 间接 + Box deref，与显式 dyn 调用等价）。
3. **每个 adapter crate 自注册**：mock 已注册；feetech 暂不注册（缺 SerialTransport
   选择 + 真硬件依赖，留 task 28 加 `real-hw` feature 时再注册）。
4. **`robot-s0` features 设计**：
   ```toml
   default   = ["xlerobot"]            # 开发期默认拿到 mock + feetech
   mock-only = ["mock"]
   xlerobot  = ["mock", "feetech"]
   atom01    = ["mock"]                # piper crate 待 v0.5 落地后扩 ["mock", "piper"]
   mock      = ["dep:robot-adapter-mock"]
   feetech   = ["dep:robot-adapter-feetech"]
   ```
   组合 features (`xlerobot` / `atom01` / `mock-only`) 对外暴露给用户 + CI；
   基础 features (`mock` / `feetech`) 内部 gating 单 crate dep。
5. **强制 link 防 dead-code strip**：`robot-s0/src/lib.rs` 加
   `#[cfg(feature = "X")] pub use robot_adapter_X as _;`。Rust 不会 strip
   被 `pub use` 的 lib，inventory section 被合并入最终 binary。
6. **s0_daemon adapter 来源**：
   - 有 `--config`：取 TOML 第一段 `[[hardware]]` 的 `adapter` + `joints` 字段，
     走 `find_factory` 查 + `factory.build(&ctx)`。
   - 无 `--config`：fallback 到 `mock_hard_rt`，joint 名按 `j0..jN-1` 自动生成
     （兼容旧 CLI smoke 路径）。
   - 找不到 factory → 打印"已注册：[...]"清单 + exit 2，便于排查 feature 漏开 / 拼写错。
7. **CI matrix-build job** 跑 3 combo（mock-only / xlerobot / atom01），每个
   `cargo build --release -p robot-s0 --bin s0_daemon --no-default-features
   --features <combo>` + `cargo test`。新机器人 entry 加一行 matrix 即可。

### `inventory` 依赖白名单

新增 `inventory = "0.3"` 到 workspace.dependencies，加进 `robot-core` /
`robot-adapter-mock` / `robot-s0` 白名单。

理由：链接器把 `submit!` 项收到 array section，**零 runtime 开销**（无 alloc /
无 IO，仅启动期一次性遍历）。技术上不破坏 §4.2 "robot-core 零 runtime 依赖" 精神，
但形式上是新依赖 → 走白名单登记。

### 约束

1. **`AdapterFactory::name()` 必须与 `[[hardware]] adapter = "..."` 字符串字面
   一致**。本次约定 snake_case：`mock_hard_rt` / `feetech_sts3215` / `piper_*`
   / `realsense_*`。新 factory 在 module-level docs 显式标注与 TOML 字符串关系。
2. **`AdapterBuildContext` 字段扩展不破坏旧 factory**：v0.4 加 `address` /
   `baud` 等字段时，旧 factory 忽略不读即可，新 factory 读新字段。
3. **`info().rt_class = HardRT` 是声明而非编译期保证**：trait 无法静态保证
   `read_state` / `write_command` 不 IO 阻塞。task 27 feetech 同步实现 + task 27c
   bench 报告 + task 28 真机实测 一起把"声明 vs 实测"对齐。
4. **inventory 链接行为依赖 binary link**：lib crate 内 `submit!` 必须经过
   `pub use ... as _` 或类似机制让 binary 真 link。否则 dead-code strip 会丢失
   注册项（test build 因 cargo 默认 link 全 lib，不受影响；release 优化下必中招）。

### 影响面

| 文件 | 调整 |
|---|---|
| `Cargo.toml`（workspace） | `[workspace.dependencies]` 加 `inventory = "0.3"` |
| `crates/robot-core/Cargo.toml` | `[dependencies]` 加 `inventory` |
| `crates/robot-core/src/lib.rs` | `pub mod factory` |
| `crates/robot-core/src/factory.rs` | **新增**（4 单测：自注册 stub + find / iter / context） |
| `crates/robot-core/src/adapter.rs` | 加 `Box<Inner>: ManipulatorAdapter` blanket impl |
| `crates/robot-adapters/mock/Cargo.toml` | `[dependencies]` 加 `inventory` |
| `crates/robot-adapters/mock/src/lib.rs` | `info().name` 改 `mock_hard_rt` 与 factory 一致；末尾加 `MockFactory` + `inventory::submit!` |
| `crates/robot-s0/Cargo.toml` | adapter dep 改 optional；features 新增 `mock-only` / `xlerobot` / `atom01` / `mock` / `feetech` |
| `crates/robot-s0/src/lib.rs` | `pub use robot_adapter_X as _` 强制 link |
| `crates/robot-s0/src/bin/s0_daemon.rs` | 移除 `use robot_adapter_mock::MockHardRTAdapter`；新增 `build_adapter` helper 走 inventory |
| `.github/workflows/ci.yml` | 新增 `matrix-build` job（3 combo） |
| `docs/CHANGELOG.md` 本条 | 新增 |
| `docs/00_design.md` §4.5 | **不动**（设计冻结） |

### 审阅

- 提议：Claude Code（2026-04-24 session 内 task 27b 入手时）
- 决定：用户 loosqk（2026-04-24 session 内"能不能做成矩阵式加载或者 ci，
  不同的机器人编译不同的模块" → 拍板方案 A：先 task 27 协议层 → task 27b
  matrix → task 27c bench → task 28 真机）
- 复审触发：
  - task 28 加 feetech factory + `real-hw` feature → 评估 feetech 注册 factory 时
    SerialTransport 来源（factory 内部硬编码 `serialport::new(addr)`，还是 context
    里加 `address` 字段后 factory 读？）
  - 第三个 robot（piper / 四足）落地 → 重新审视 features 命名约定（是否要
    `xlerobot-arms` / `xlerobot-base` 之类的二级拆分）
  - inventory crate 升级到 0.4 / `linkme` 替代 → 评估 API 兼容性

---

## 2026-04-24 · Step 7 task 27c feetech 性能 / 集成 smoke / 火焰图配套（**状态：登记 / 实施细节落地**）

### 涉及小节

- `docs/10_verification.md` §10.2.1 Harness 清单 → 新增 3 条 task 27c 产出
- `docs/10_verification.md` 附录 → 新增 feetech bench baseline 实测数值
- `Cargo.toml` workspace.dependencies 加 `criterion = "0.5"`（仅 dev-deps 用）
- `crates/robot-adapters/feetech/Cargo.toml` 加 `criterion` dev-dep + `[[bench]]` entry
- `crates/robot-adapters/feetech/benches/feetech_bench.rs`（**本次新增**）
- `crates/robot-adapters/feetech/tests/integration_smoke.rs`（**本次新增**）
- `scripts/feetech_flamegraph.sh`（**本次新增**）

### 背景

用户拍板"上真机前，冒烟测试，全量测试，性能测试和火焰图都要搞好"。task 27
完成 feetech 协议层 + 83 单元测试，task 27b 完成 inventory + matrix 编译；
task 28 上 Orin 真机前还差三件套：(1) 性能基准，(2) 13-motor 规模端到端
集成 smoke，(3) 火焰图配套。本任务全部交付。

### 决定

1. **criterion bench 5 组**（`benches/feetech_bench.rs`）：
   - `encoding`：encode/decode_sign_magnitude + split/combine_bytes_le
   - `protocol_build`：build_ping/read/write/sync_read/sync_write
   - `parse`：parse_status + checksum
   - `bus`：单 motor read/write 端到端（mock transport）
   - `bus_sync`：13-motor sync_read / sync_write（XLeRobot 双臂 5+5 + 三轮 3 规模）
2. **集成 smoke 3 测试**（`tests/integration_smoke.rs`）：
   - 13-motor × 1000 循环 sync_read+sync_write（含 sign-magnitude 跨 0 边界）
   - 13-motor sync_read 解码负值（`-400` raw）
   - 13-motor handshake（每 motor PING + READ_DATA Model_Number）
3. **flamegraph 脚本**（`scripts/feetech_flamegraph.sh`）：
   - 封装 `cargo flamegraph -p robot-adapter-feetech --bench feetech_bench`
   - 默认 case = `sync_read_present_position_13motor`（task 28 真机最关心的 hot path）
   - 前置检查：`cargo-flamegraph` / `perf` / `perf_event_paranoid`
   - 输出 `target/criterion-flamegraph/<case>.svg`
4. **bench 数值入档** `docs/10_verification.md` 附录，作为协议层"无回归"的
   baseline。task 28 真机延迟瓶颈在 USB-RS485 wire RTT，**不**在 Rust 协议层。

### 实测 baseline（Linux x86_64 dev 机，release，criterion 0.5.1）

| 关键 case | 中位数 | 注 |
|---|---|---|
| `encode_sign_magnitude_pos` | 494 ps | 亚纳秒 |
| `decode_sign_magnitude` | 368 ps | |
| `build_sync_write_13motor_2byte` | 33.2 ns | 13 motor 包 build |
| `build_sync_read_13motor` | 14.5 ns | 13 motor 包 build |
| `parse_status_2byte` | 1.58 ns | |
| `bus.read_present_position_1motor` | 66.2 ns | 含 mock tx + ack rx + parse + drain |
| `bus.write_goal_position_1motor` | 73.7 ns | 同上 |
| **`bus.sync_read_present_position_13motor`** | **866 ns** | **task 28 真机最关心 hot path** |
| **`bus.sync_write_goal_position_13motor`** | **240 ns** | **同上**（不 ACK） |

**结论**：13-motor sync_read 端到端 866 ns ≈ 1 µs，远小于 50 Hz tick (20 ms)
预算。task 28 真机延迟全部在 USB-RS485 wire RTT（1 Mbps × packet ~50 B / motor →
几 ms 量级），不在协议层。

### 约束

1. **bench baseline 是相对值**：本机 dev 机数据仅作回归对照（CI 不卡阈值）。
   绝对延迟门槛以 task 28 Orin USB-RS485 实测为准。
2. **集成 smoke 的 1000 次循环非性能基准**：仅验"长循环不出错 + tx/rx buffer
   不泄漏"。性能数据走 criterion bench。
3. **flamegraph 脚本需要前置环境**：`cargo install flamegraph` + `perf` +
   `perf_event_paranoid <= 0`（`echo -1 | sudo tee ...`）。脚本内已检测 + 提示。
4. **CI 不自动跑 bench**：criterion 5+ 分钟一轮太重，本任务不挂 CI。task 28
   真机阶段或 v0.5 性能回归出现时手动跑。

### 影响面

| 文件 | 调整 |
|---|---|
| `Cargo.toml`（workspace） | `[workspace.dependencies]` 加 `criterion`（default-features=false + cargo_bench_support + html_reports） |
| `crates/robot-adapters/feetech/Cargo.toml` | dev-dependencies 加 `criterion`；`[[bench]] name="feetech_bench" harness=false` |
| `crates/robot-adapters/feetech/benches/feetech_bench.rs` | **新增**（5 组 criterion bench） |
| `crates/robot-adapters/feetech/tests/integration_smoke.rs` | **新增**（3 测试） |
| `scripts/feetech_flamegraph.sh` | **新增**（封装 cargo flamegraph） |
| `docs/10_verification.md` §10.2.1 | 加 3 行 task 27c harness |
| `docs/10_verification.md` 附录 | 加 feetech bench baseline 实测表 |
| `docs/10_verification.md` 变更记录 | 加 2026-04-24 条 |
| `docs/CHANGELOG.md` 本条 | 新增 |
| `docs/00_design.md` | **不动**（设计冻结） |

### 审阅

- 提议：Claude Code（2026-04-24 session 内 task 27c 入手时）
- 决定：用户 loosqk（2026-04-24 session 内"上真机前，冒烟测试，全量测试，
  性能测试和火焰图都要搞好"）
- 复审触发：
  - task 28 真机实测发现协议层 hot path 与本机 bench 偏差 > 10× → 重跑
    flamegraph 找瓶颈 + 更新 baseline 表
  - bench 数值在某 commit 突然退化 > 30% → 加 CI 夜间 bench compare job
  - 集成 smoke 1000 循环偶发失败（mock 行为变化 / inventory 链接问题）→ 加
    nightly schedule 跑 smoke 重复 100×

---

## 2026-04-24 · Step 7 task 28 feetech `real-hw` + Orin USB-RS485 单关节真机 bring-up（**状态：登记 / 实施 + 实测**）

### 涉及小节

- `docs/CHANGELOG.md` 2026-04-22 "workspace runtime 依赖白名单机制" → 新增
  `serialport` 进 `robot-adapter-feetech` 白名单（feature gated `real-hw`）
- `docs/CHANGELOG.md` 2026-04-24 task 27b → `AdapterBuildContext` 字段扩展
  规则首次行权（加 `address` + `motor_ids`）
- `docs/01_implementation_steps.md` Step 10 "Feetech 单关节" → bring-up 起步
  完成；剩 *真转电机* + *fail-safe 时延实测* 留下次 session
- `docs/handoffs/2026-04-24_6.md` §3 真机实测全文档
- 不动 `docs/00_design.md`

### Phase A 改动（commit `940c563`，本机准备）

1. **AdapterBuildContext 扩字段**（`robot-core::factory`）：
   - `address: Option<&'a str>` —— mock factory 忽略；feetech 必填
   - `motor_ids: &'a [u8]` —— 空 = fallback 1..=N；显式 = 按 joints 顺序对齐
   - 字段扩展严格遵守 task 27b 约定："旧 factory 忽略不读 / 新 factory 消费"
2. **`serialport = "4"` 加 workspace.dependencies**（仅 dev 机 + Linux 真机，
   依赖系统 `libudev-dev`；本条**进 robot-adapter-feetech 白名单**，仅
   `real-hw` feature 启用时 link，CI 默认 off 保持跨平台编译）
3. **`real-hw` feature**（`robot-adapter-feetech/Cargo.toml`）：拉
   `serialport` + 启用 `transport::real::RealSerialTransport` 模块 + 启用
   `factory::FeetechFactory` 模块（inventory submit）
4. **`xlerobot-real` combo feature**（`robot-s0/Cargo.toml`）：`xlerobot` +
   `robot-adapter-feetech/real-hw`；default 仍 `xlerobot`（mock-only），
   dev / CI 不强制 libudev-dev
5. **s0_daemon `build_adapter` 透传 address + motor_ids**：从
   `[[hardware]]` TOML extras 读 `motor_ids` 数组（`toml::Value::as_array`）
6. **`configs/xlerobot_task28_single_joint.toml`** 新增：`adapter =
   "feetech_sts3215"` + `address = "/dev/serial/by-id/usb-1a86_..."` +
   `joints = ["test_j1"]` + `motor_ids = [1]`

### Phase B 实测（本 session，详 `docs/handoffs/2026-04-24_6.md` §3）

- Orin (5.15.185-tegra aarch64 / JetPack 5) 上 `cargo build --release
  --no-default-features --features xlerobot-real -p robot-s0 --bin s0_daemon`
  通过（1m 36s clean rebuild）
- s0_daemon `--config configs/xlerobot_task28_single_joint.toml
  --smoke-duration 3s` 端到端通过：
  - `FeetechFactory::build` 正确实例化 + 打开 `/dev/serial/by-id/
    usb-1a86_USB_Single_Serial_5A7C121427-if00`
  - `FeetechBus::connect(true)` 内部 handshake：PING ID=1 + READ Model_Number
    → 收到 777 (STS3215)
  - 4 线程 spawn + state machine SAFE → READY（auto-Enable）
  - rt_thread 50 Hz tick 跑 `sync_read_by_id_into Present_Position/Velocity`
    在真 USB-RS485 wire 上 3s × 50Hz = 150 tick 全无错
  - clean shutdown + EStop latch 持久化 + exit 0
- 已确认：**5A7C121427 = bus1**（dimos `04_踩坑记录` 左臂总线，含 ID 1-8 +
  头部）

### 约束

1. **Orin build 两个坑**（已写 handoff §3.2）：
   - `.cargo/config.toml` 设 `rustc-wrapper = sccache`，但 Orin 没装 sccache
     → `CARGO_BUILD_RUSTC_WRAPPER=""` 绕开
   - cargo download 偶发 "transfer too slow"（默认 timeout 30s 不够）→
     `CARGO_HTTP_TIMEOUT=300 CARGO_NET_RETRY=10` + `cargo clean` 从干净状态
2. **address by-id 推荐 / ttyACM 不推荐**：udev 稳定路径 vs USB enumeration
   动态分配。configs 全部应用 by-id 路径。
3. **`motor_ids` 字段必须显式写**（除非 fallback 1..=N 适用）：dimos 多 bus
   场景（bus2 含底盘 ID 7-9）必须显式数组；factory 在 ID 长度不匹配时立刻
   `RobotError::InvalidParameter` 拒绝启动
4. **task 28 *未*覆盖**：真转电机 / kill -9 fail-safe 时延 / bus2 验证 /
   13-motor 同时跑 / Orin flamegraph / dimos 标定值接入。下次 session 待办
   完整列表见 `docs/handoffs/2026-04-24_6.md` §4（含用户提示的 dimos
   `~/repo/dimensional-applications/calibration/{left_arm,right_arm,head,
   base}.json` 4 份现成标定，格式与 `MotorCalibration` strut 字段对齐）

### `robot-adapter-feetech` 依赖白名单更新

| 依赖 | 范围 | feature gate | 用途 |
|---|---|---|---|
| `inventory = { workspace = true }` | dependencies | 默认 | factory 自注册（task 27b 已加，本次未变） |
| `serialport = { workspace = true, optional = true }` | dependencies | `real-hw` | RealSerialTransport USB-RS485 后端（**本次新增**） |

### 影响面

| 文件 | 调整 |
|---|---|
| `Cargo.toml`（workspace） | `[workspace.dependencies]` 加 `serialport = "4"` |
| `crates/robot-core/src/factory.rs` | `AdapterBuildContext` 加 `address` + `motor_ids` 字段 + 单测覆盖 |
| `crates/robot-adapters/feetech/Cargo.toml` | `inventory` deps 加；`serialport` optional + `[features] real-hw` |
| `crates/robot-adapters/feetech/src/transport.rs` | 加 `#[cfg(feature = "real-hw")] pub mod real { ... RealSerialTransport ... }` |
| `crates/robot-adapters/feetech/src/factory.rs` | **新增**（`FeetechFactory` + `inventory::submit!`，feature gated） |
| `crates/robot-adapters/feetech/src/lib.rs` | `pub mod factory` + `pub use FeetechFactory`（feature gated） |
| `crates/robot-s0/Cargo.toml` | `[features] xlerobot-real` 新增 |
| `crates/robot-s0/src/bin/s0_daemon.rs` | `build_adapter` 从 TOML 读 `address` + `extras["motor_ids"]` 数组，透传 ctx |
| `configs/xlerobot_task28_single_joint.toml` | **新增**（task 28 单关节测试 config） |
| `docs/handoffs/2026-04-24_6.md` | **新增**（Phase B 实测全过程 + 6 个待办） |
| `docs/handoffs/README.md` | 列表加 2026-04-24_6 行 |
| `docs/CHANGELOG.md` 本条 | 新增 |
| `docs/00_design.md` | **不动**（设计冻结） |

### 审阅

- 提议：Claude Code（2026-04-24 session 内 task 28 入手时）
- 决定：用户 loosqk（2026-04-24 session 内拍板继续 → "继续 27b/27c/28"
  → "刚才的优化点加上"（task 27c follow-up by-id）→ task 28 真机
  bring-up 跑通 → "orin 里的 dimos-deploy 有标定好的电机值" → handoff 加待办）
- 复审触发：
  - 真转电机 → 需要先 dump 当前 EEPROM Homing_Offset 备份再 write_calibration
    （防覆盖 dimos 已校）
  - bus2 (5A7C122340) 上线，加 second `[[hardware]]` 段 → 评估 s0_daemon
    多 bus 一进程拓扑 vs 多 instance（设计 §1.2 多 bus 方案 B 真落地）
  - dimos calibration JSON 接入 → robot-adapter-feetech 加 serde + serde_json
    依赖（白名单走本条 + CHANGELOG 后续条）

---

## 2026-04-25 · Step 7 task 29 feetech calibration JSON loader + silent clamp helper（**状态：完整通过（commit 1-5 + Orin 真机闭环 + 防跳 fix）**）

### 涉及小节

- `docs/CHANGELOG.md` 2026-04-24 task 28 §复审触发第 3 条 "dimos calibration JSON
  接入 → feetech 加 serde + serde_json 依赖" 行权 —— 本条是该行权的白名单登记 +
  实施首 commit
- `docs/CHANGELOG.md` 2026-04-22 "workspace runtime 依赖白名单机制" → 新增
  `serde` + `serde_json` 进 `robot-adapter-feetech` 依赖白名单
- `docs/01_implementation_steps.md` Step 10 "Feetech 单关节 bring-up" → task 28
  "真转电机" 待办的前置工作（commit 1/3：纯内存 loader；commit 2 给 adapter
  挂 follow-current + silent clamp；commit 3 Orin 三端闭环真转）
- 不动 `docs/00_design.md`

### 关键决定：**不写 EEPROM**

`MotorCalibration` JSON 已由 dimos 侧在 Orin EEPROM 上一次性写好（`Homing_Offset`
/ `Min_Position_Limit` / `Max_Position_Limit` 都是 EEPROM 区），本次 loader
**只读 + 内存缓存**，**不** 调 `crate::calibration::write_calibration` 覆盖 —— 覆盖 dimos
已校值的风险 > 每次冷启动写 EEPROM 的便利。真需要写 EEPROM 走单独的
`robotctl calibrate --apply` 手工命令（task 29 后续阶段再定义）。

用途：

1. adapter 启动时缓存 `range_min/max` 供热路径 silent clamp（1:1 对齐 dimos
   `_manipulator_base.py:422-426`）
2. 未来 raw ↔ rad 换算加 per-joint `direction` / `zero_offset_rad` 扩展（commit 1
   **不**实现，留给 commit 2 / 上层 adapter 配置）

### commit 1 改动范围（本 commit）

1. **`MotorCalibration` 加 serde derive**（`types.rs`）：
   - `#[derive(Serialize, Deserialize)]` + `#[serde(try_from = "MotorCalibrationRaw")]`
   - 内部 `MotorCalibrationRaw` private struct，`drive_mode` 可缺省默认 0（对齐
     dimos `entry.get("drive_mode", 0)`）
   - `TryFrom<MotorCalibrationRaw>` 调 `MotorCalibration::new` 走**同一条校验**
     （homing_offset ∈ [-2047, +2047]、range_min/max ∈ [0, 4095]）
2. **新增 `calibration_io` 模块**：
   - `load_file(path) -> HashMap<String, MotorCalibration>` —— 单文件
   - `load_and_merge(&[Path]) -> HashMap<String, MotorCalibration>` —— 多文件合并，
     motor name 重复 → 报 `CalibrationDuplicateMotor`（比 dimos "warning 后静默覆盖"
     **更严格**，避免配置错配难定位）
   - `clamp_raw(raw, lower, upper)` inline helper（1:1 移植 dimos
     `units.py::clamp_raw`）
   - 内部走 `serde_json::Value` 中转，per-entry 错误附带 motor name + file path
     （比 derive 默认的 "unknown field foo at line X" 更有定位价值）
3. **`FeetechError` 4 个新变体**：
   - `CalibrationIo { path, source: std::io::Error }`
   - `CalibrationParse { path, source: serde_json::Error }`
   - `CalibrationEntryInvalid { path, name, reason: String }`（serde 把 TryFrom 的
     `FeetechError::Display` 压成 String，无法拿回原结构化错误 —— 这里用 String 留
     serde 原文，保留 "range_max=5000 out of [0, 4095]" 这类可读信息）
   - `CalibrationDuplicateMotor { name, first: PathBuf, second: PathBuf }`
   - `From<FeetechError> for RobotError`：前两者归 `InvalidParameter("feetech
     calibration file")`；后两者归 `InvalidParameter("feetech calibration")`
4. **15 个单测**：load_file × 8（含 IO error / parse error / 非 object top-level /
   homing_offset 越界 / range 越界 / u16 溢出 / drive_mode 缺省）+ load_and_merge × 2
   （合并 / 重名）+ clamp_raw × 5（内 / 下越 / 上越 / 负数 / 等值退化）

### commit 2 改动范围（已落地）

1. **`FeetechAdapterConfig` 加 `calibrations: Vec<MotorCalibration>` 字段**（同序
   对齐 `joint_names`；空 Vec = 无标定）。`FeetechAdapter::new` 做长度校验（非空
   长度必须 == `joint_names.len()`），`limits` 依据 calibration 的 `range_min/max`
   raw → rad 换算；无 calibration 时保留原 ±π 默认
2. **`connect()` 加 `calibration.id == bus motor id` 校验**（防配错 `left_arm.json`
   被指给了 `right_arm` joints），不匹配立刻 `RobotError::InvalidParameter`
3. **`enable(true)` 1:1 移植 dimos `_manipulator_base.py:468-504` 防飞序列**：
   - sync_read Present_Position（每 motor 一 status 包）
   - sync_write Goal_Position ← Present（target 追平 current）
   - sleep 50 ms（常量 `ENABLE_SYNC_SETTLE`）消化总线
   - per-motor WRITE Torque_Enable=1 带 3× 重试 + 150 ms 间隔（常量
     `TORQUE_ENABLE_MAX_ATTEMPTS` / `TORQUE_ENABLE_RETRY_DELAY`）
   - 任一 motor 全部重试都失败 → 返回第一失败 motor 的错误
4. **`enable(false)` 也走 3× 重试**（对齐 enable(true) 一致性；无 sync_read 前置）
5. **`write_command` 热路径 silent clamp**：非空 calibration 时每 joint 走
   `clamp_raw(raw, range_min, range_max)`；空 calibration 时直写（dimos
   `_manipulator_base.py:422-426` 1:1）
6. **新增 public helper**：
   - `FeetechAdapter::sync_goal_to_present() -> Result<(), FeetechError>`（读当前
     Present，写回 Goal，不动 Torque）
   - `FeetechAdapter::hold_current_position() -> Result<(), RobotError>`（S0 在
     `CONTROL_LOST` 恢复时主动调，handoff §4.1 语义）
7. **私有 helper `write_torque_enable_with_retry(on: bool)`** 提取，避免 enable
   / disable 两条路径重复 3× retry loop
8. **Factory 侧传空 Vec**：`FeetechFactory::build` 暂时 `calibrations: Vec::new()`
   —— commit 3 扩 factory 从 config 读 `calibration_path` → 加载 → 传 adapter
9. **7 新单测**：enable(true) dimos 序列 / enable(false) 直写 / hold_current_position
   / write_command clamp 有 cal / write_command 无 cal 不 clamp / calibration-derived
   limits / 长度不匹配拒绝 / id 不匹配 connect 拒绝

### commit 3 改动范围（已落地 —— 代码 / config；Orin 真机闭环待下一步）

1. **`AdapterBuildContext` 加 `calibration_paths: &'a [String]` 字段**：
   - 字段语义：一条总线的标定 JSON 路径数组，dimos bus1 = left_arm.json +
     head.json（双文件合并），bus2 = right_arm.json + base.json
   - 空数组 = "无标定"；非空 → factory 负责 `load_and_merge` + 按 `joint_names`
     顺序 lookup
   - 遵循 task 27b 字段扩展规则：旧 factory 忽略不读（mock / 其他 non-feetech
     adapter 不受影响）；新 factory 按需消费
   - `factory.rs` 内 `build_context_passes_through_fields` 测试同步扩
2. **`FeetechFactory::build` 消费 calibration_paths**：
   - `ctx.calibration_paths.is_empty()` → `calibrations: Vec::new()` 传给 adapter
   - 非空 → `calibration_io::load_and_merge(ctx.calibration_paths)` 合并所有文件
     → `motor_name → MotorCalibration` map
   - 按 `joint_names` 顺序 lookup；任一 joint 查不到 → `RobotError::
     InvalidParameter("feetech: calibration missing motor")` + stderr 打印
     available keys 辅助定位
   - build 完成日志附 `calibration=loaded|none` 字段
3. **`s0_daemon::build_adapter` 读 TOML extras**：
   - `[[hardware]] calibration_paths = ["..."]` 字段通过 `hw.extras` 读取
     （`as_array()` + `as_str()` filter_map）
   - 未设置 / 空数组 → 透传空 Vec，factory 不加载
   - fallback 路径（CLI-only mock）保持空 calibration_paths
4. **新增 config 文件**：
   - `configs/xlerobot_task29_single_joint.toml`：基于 task28 config，joint 名
     改为 dimos 标准 `left_arm_shoulder_pan`（对齐 `left_arm.json` 第一条），
     加 `calibration_paths` 指 Orin 上 `/home/loosqk/repo/dimensional-applications
     /calibration/{left_arm,head}.json`（bus1 完整合并）
   - 文件头长注释说明：calibration 只读 / 不写 EEPROM / follow-current 防飞 /
     bus1 拓扑

### commit 3 Orin 端到端闭环实测（**已完整通过**，详见 `docs/handoffs/2026-04-25_1.md` §3）

**bring-up 策略调整**（用户 2026-04-25 指令"只动末端夹爪"）：
原 config 选 `left_arm_shoulder_pan` (ID=1) —— 机械臂未装到小车时旋转 shoulder
风险大；切换到 `left_arm_gripper` (ID=6)，动的是末端夹爪开合，无机械风险。
config 改动在 commit `82e7a13`（task29 config 切换）。

**三端闭环实测结果**：

| 验证项 | 结果 |
|---|---|
| `calibration=loaded`（factory 日志） | ✅ |
| auto-enable 序列不飞（smoke 5s 无 S1） | ✅ state=Ready，夹爪不动 |
| state 升 Enabled（control_link Ok） | ✅ S1 心跳 + command 正常接收 |
| Goal_Position 真物理运动 ±5° | ✅ 用户肉眼物理确认夹爪真的在开合 |
| 正弦振幅跟踪精度 | ✅ 实测 0.083 vs 目标 0.087 rad，误差 4.6% |
| 正弦周期精度 | ✅ 4 s / cycle = 0.25 Hz，3 个完整周期 |
| `CtrlLostHold` 子态触发（S1 duration 到） | ✅ output_mode: Hold |
| `hold_current_position` helper 工作 | ✅ 12 个采样锁死 `-0.008` rad，不 drift / 不归零 |
| s0 / s1 / robotctl clean shutdown | ✅ |

实测命令（供复现 / 后续验证参考）：

```bash
./target/release/s0_daemon \
  --config configs/xlerobot_task29_single_joint.toml \
  --latch-path /tmp/robot-task29/estop_latch --smoke-duration 18s &
sleep 2
./target/release/mock_s1 --robot-id xlerobot_task29 \
  --hardware-id left_arm_single --joints 1 \
  --amplitude 0.087 --frequency 0.25 --duration 12s &
sleep 1
timeout 14 ./target/release/robotctl watch --robot-id xlerobot_task29 \
  --hardware-id left_arm_single --interval-ms 250
```

**未做**（留更后续 session）：
- bus2（5A7C122340 右臂 + 底盘）多 motor 同时闭环
- 13-motor 全栈 sync_read（双 bus 同时）
- fail-safe 时延实测（kill -9 s0_daemon → motor torque disable 时延，设计
  §6.7 + §10.1 要求 < 500 ms）
- `robotctl calibrate --apply` 工具（如果未来需要 Rust 侧主动写 EEPROM）
- Orin flamegraph aarch64 hot path（任务 27c 火焰图是本机 mock，真机分布可能不同）

### commit 4/5 防跳 follow-up（本 session 后续发现 + 修复）

三端闭环真机看到的瑕疵：**从闭合位启动后，mock_s1 接管瞬间电机从 raw ~1400
（闭合）跳到 raw 2047（center）≈ 36° "张开到一半"**。先做了两条独立修复：

- **commit 4**（`61d2b76` · adapter defensive）：`FeetechAdapter::sync_goal_to_
  present` 加 readback + retry：写完 Goal 后 `sleep 20 ms → sync_read Goal_Position`
  验证，若差 > `SYNC_GOAL_TOLERANCE (20 tick)` → 重写最多 3 次；失败返回
  `FeetechError::SyncGoalReadbackMismatch` + 每次 attempt eprintln 对比日志。
  最初假设 "Torque=0 时 sync_write 被忽略"，但用户澄清**从没断电** → adapter
  层不是 root cause。**保留作 defensive 兜底**（真冷启动 + 固件异常时仍有用），
  3 新单测覆盖 retry / tolerance / max-attempts 边界。
- **commit 5**（`25658f0` · **真 root cause fix**）：mock_s1 加 `--anchor-timeout-ms`
  参数（默认 1000）。启动时订阅 S0 state topic 读一帧 `positions[0..joints]`
  做正弦 bias，`target[i] = bias[i] + amp*sin(phase + i*π/4)`。原公式 `amp*sin(phase)`
  第一帧 sin(0)=0 rad = raw 2047 (center) → motor 被硬拉到中点；锚定后第一
  帧 Goal=bias=当前位置 → **motor 不动**。超时 / 订阅失败 fallback 旧行为 +
  warning。

**commit 5 真机验证**（bus1 left_arm_gripper，夹爪已在 raw 1418 闭合位）：

| 验证项 | 结果 |
|---|---|
| mock_s1 anchor log: `bias=[-0.962]` | ✅ 对应 raw ≈ 1420 |
| 第一帧 pos = -0.882 rad | ✅ 不是 0.080 (center)，完全无 36° 跳变 |
| 正弦围绕 bias 摆动 `[-0.969, -0.882]` | ✅ 幅值 ±0.044（限位前有效开合） |
| 负峰被 silent clamp 卡在 range_min (-0.98) | ✅ clamp 同时生效，验证 commit 2 |
| S1 duration 结束 → `hold_current` 锁死 -0.969 × 14 次 | ✅ hold_current 不 drift |
| 用户肉眼："没跳，紧紧闭合没动" | ✅ bias 太接近 range_min → 负半周期物理被 clamp 限位，视觉表现为"挤压闭合" |

**commit 5 改动范围**：
- `crates/robot-s1/src/bin/mock_s1.rs`：加 `--anchor-timeout-ms <MS>` CLI（默认
  1000，0=禁用）；启动时开 state subscriber + 10 ms 间隔 poll；主循环正弦公
  式加 bias；三条 fallback 路径（subscriber open 失败 / view decode 失败 /
  轮询 timeout）全部 `warning + bias=0` 保留旧行为

### commit 5 已知小瑕疵（非 regression）

- `robotctl watch` 用 `timeout 14` 被 SIGTERM kill 时 segfault。全程 13s 数据
  正常，是 iceoryx2 subscriber drop 顺序问题，**不影响功能**。记 task-30
  候选：robotctl signal handler 提早 drop subscriber 或改 `ctrlc` crate 优雅
  退出。

### 依赖白名单更新（robot-adapter-feetech）

| 依赖 | 范围 | feature gate | 用途 |
|---|---|---|---|
| `serde = { workspace = true }` | dependencies | 默认 | `MotorCalibration` try_from + derive（**本次新增**） |
| `serde_json = { workspace = true }` | dependencies | 默认 | JSON 读文件 + per-entry 错误定位（**本次新增**） |
| `tempfile = { workspace = true }` | dev-dependencies | 默认 | 单测 NamedTempFile 写 JSON fixture（**本次新增**） |

### 约束

1. **JSON 格式与 dimos 1:1 兼容**（`scripts/calibrate.py` / `common/calibration.py`
   输出）：顶层 `{motor_name: {id, drive_mode?, homing_offset, range_min, range_max}}`；
   未来格式变更走 CHANGELOG + 对应 dimos 同步
2. **loader 纯只读 / 纯内存**，**不**触 motor EEPROM；`write_calibration` 路径仍在
   `crate::calibration`，调用方自己控制（未来 `robotctl calibrate --apply` 入口）
3. **重名 = 错误**（与 dimos warning 行为有意不同）；静默覆盖的调试成本 > 明确
   拒绝的 config 修正成本
4. **`MotorCalibration::new` 不校验 `id`**（id ∈ [1, 253] 校验在 `Motor::new`，
   calibration 不直接构造 Motor）—— loader 不重复这层，保留 dimos 的松耦合
5. **raw ↔ rad 换算本 commit 不改**：`adapter.rs` 的 `raw_to_rad` / `rad_to_raw`
   保留纯线性（无 direction / zero_offset_rad）；per-joint 偏移扩展留 commit 2
   实际需要时再加，避免过度设计

### 影响面

| 文件 | 调整 | commit |
|---|---|---|
| `Cargo.toml`（workspace） | `[workspace.dependencies]` 加 `serde_json = "1"` + 白名单注释 | 1 |
| `crates/robot-adapters/feetech/Cargo.toml` | `[dependencies]` 加 `serde` + `serde_json`；`[dev-dependencies]` 加 `tempfile` | 1 |
| `crates/robot-adapters/feetech/src/types.rs` | `MotorCalibration` 加 `#[derive(Serialize, Deserialize)]` + `#[serde(try_from)]` + 私有 `MotorCalibrationRaw` struct + `TryFrom` impl | 1 |
| `crates/robot-adapters/feetech/src/calibration_io.rs` | **新增**（`load_file` + `load_and_merge` + `clamp_raw` + 15 单测） | 1 |
| `crates/robot-adapters/feetech/src/error.rs` | `FeetechError` 加 4 变体 + `From` impl 对应更新 | 1 |
| `crates/robot-adapters/feetech/src/lib.rs` | `pub mod calibration_io` + `pub use calibration_io::{clamp_raw, load_and_merge, load_file}` | 1 |
| `crates/robot-s0/tests/rt_guard_soak.rs` | pre-existing 2 处 `clippy::doc_markdown` 错误（`tick_count` / `SCHED_FIFO`）顺手补 backtick | 1 |
| `crates/robot-adapters/feetech/src/adapter.rs` | `FeetechAdapterConfig` 加 `calibrations`；adapter 存字段 + calibration-derived limits；`connect()` 加 id 校验；`enable(true)` 按 dimos 序列 + 50 ms 消化 + 3× 重试；`enable(false)` 同 3× 重试；silent clamp in `write_command`；新加 `sync_goal_to_present` + `hold_current_position` public helper + `write_torque_enable_with_retry` 私有 helper；新增 7 单测 | 2 |
| `crates/robot-adapters/feetech/src/factory.rs` | `FeetechFactory::build` 构造 `FeetechAdapterConfig` 时传 `calibrations: Vec::new()`（commit 3 再扩读 calibration_path） | 2 |
| `crates/robot-core/src/factory.rs` | `AdapterBuildContext` 加 `calibration_paths: &'a [String]`；测试同步扩 | 3 |
| `crates/robot-adapters/feetech/src/factory.rs` | 扩读 `ctx.calibration_paths` → `load_and_merge` → 按 `joint_names` 顺序 lookup → 传 adapter；build 日志附 `calibration=loaded|none` | 3 |
| `crates/robot-s0/src/bin/s0_daemon.rs` | `build_adapter` 从 TOML `hw.extras["calibration_paths"]` 读数组 → 透传 `AdapterBuildContext.calibration_paths` | 3 |
| `configs/xlerobot_task29_single_joint.toml` | **新增**（task 29 单关节 + 完整 bus1 calibration 合并 config） | 3 |
| `docs/CHANGELOG.md` 本条 | 新增 / 更新 | 1 + 2 + 3 |
| `docs/00_design.md` | **不动**（设计冻结） | — |

### 验证（task 29 全部完成后）

- `cargo fmt --all --check` ✅
- `cargo clippy --all --all-targets -- -D warnings` ✅（零警告）
- `cargo test --all` ✅ 全部绿；feetech 单测 **105 pass**（commit 1 的 98 + commit 2 的 7）；
  workspace 总 **336 pass** / 0 fail / 7 ignored
- matrix features 本机 build：`mock-only` / `xlerobot` / `atom01` 三个组合 ✅
- **`xlerobot-real` + Orin 真机端到端闭环**：✅ bus1 `left_arm_gripper` (ID=6)
  真转电机通过（±5° 正弦 × 3 周期 + `CtrlLostHold` 子态 hold_current_position
  语义确认）。详 `docs/handoffs/2026-04-25_1.md` §3

### 审阅

- 提议：Claude Code（2026-04-25 session 开场；用户 2026-04-24 handoff §4.6
  提示 "接入 dimos 现成标定值"）
- 决定：用户 loosqk（2026-04-25 本 session 拍板 "不覆盖 EEPROM，仅内存层
  loader"，"启动时 follow-current 防飞"，"range 超限走 silent clamp 对齐 dimos"）
- 复审触发：
  - adapter 真集成 calibration（commit 2）→ 需定义 `FeetechAdapter::connect` 时
    `MotorCalibration[]` 注入路径 + per-joint direction / zero_offset_rad 是否
    进 crate
  - `robotctl calibrate --apply` 工具 → 如果未来需要一键写 EEPROM，重新激活
    `crate::calibration::write_calibration` 这条路
  - dimos JSON 格式变更 → 后续 CHANGELOG 登记 + 同步 parser

---

## 2026-04-25 · task-32 日志骨架（**状态：登记 / 骨架落地**）

### 涉及小节

- `docs/CHANGELOG.md` 2026-04-22 "workspace runtime 依赖白名单机制" → 新增
  `tracing` / `tracing-subscriber` / `ctor` 进 workspace 白名单
- `docs/00_design.md` 不动（本 task 只增基础设施，不改设计语义）
- 新增规范文档 `docs/logging.md`（字段词汇表 + 级别约定 + RT 铁律）

### 决定

- **事实标准库**：`tracing` + `tracing-subscriber`（Rust 版 spdlog），
  `thiserror` 继续做 error derive 标准（已在用）。
- **时间戳**：wall clock RFC3339 UTC + monotonic `[+secs.ns s]`（`Instant::
  now().duration_since(start)`，`CLOCK_MONOTONIC_RAW` 原理一致，NTP / 闰秒
  无感）。手写 Howard Hinnant 日期算法，零外部 chrono/time 依赖。
- **字段命名统一词汇表**（`docs/logging.md` 权威）：`robot_id` / `hardware_id`
  / `motor_id` / `joint` / `adapter` / `err` (Debug) / `attempt` /
  `duration_ms` 等，新增字段必须走 CHANGELOG + 更新 logging.md。
- **自动初始化**：`ctor` crate `#[ctor::ctor]` 在 `main` 运行前自动装 subscriber
  —— **bin 的 main 里不用写 init 代码**，只要 `Cargo.toml` 依赖 `robot-logging`。
  可选 `let _ = robot_logging::init()` 作 self-documenting anchor（幂等）。
- **RT 铁律保留**：RT thread 内**禁用**任何 `tracing::*!` 调用（含 literal
  message）—— tracing Event 构造可能 alloc，违反 §4.9。RT 路径走 metrics
  atomic counter + 非 RT 线程定期采样。

### 影响面

| 文件 | 调整 |
|---|---|
| `Cargo.toml`（workspace） | `members` 加 `crates/robot-logging`；deps 加 `tracing` / `tracing-subscriber` / `ctor`；`robot-logging` path dep |
| `crates/robot-logging/` | **新增 crate**（135 行 lib.rs + Cargo.toml）：`init()` 公共入口 + `WallAndMono` FormatTime + `#[ctor]` 自动初始化 + 3 单测（civil_from_days / RFC3339 / init 幂等） |
| `crates/robot-s0/Cargo.toml` | 加 `robot-logging` + `tracing` deps |
| `crates/robot-s0/src/bin/s0_daemon.rs` | `main` 顶部加 `let _ = robot_logging::init();`（self-documenting anchor，ctor 已自动跑过） |
| `crates/robot-s1/Cargo.toml` | 同上 |
| `crates/robot-s1/src/bin/mock_s1.rs` | 同上 |
| `crates/robot-cli/Cargo.toml` | 同上 |
| `crates/robot-cli/src/bin/robotctl.rs` | 同上 |
| `crates/robot-adapters/feetech/Cargo.toml` | 加 `tracing` dep（lib 用 macros）；`robot-logging` optional + 进 `real-hw` feature（feetech_poke example 需要） |
| `crates/robot-adapters/feetech/examples/feetech_poke.rs` | 同上（anchor init） |
| `docs/logging.md` | **新增** 规范文档 |
| `docs/CHANGELOG.md` 本条 | 新增 |

### 依赖白名单更新

| 依赖 | 范围 | 用途 |
|---|---|---|
| `tracing = "0.1"` | workspace.dependencies | 结构化日志 macros；**所有 lib / bin 允许用**（RT thread 除外） |
| `tracing-subscriber = { version = "0.3", default-features = false, features = [...] }` | workspace.dependencies | subscriber runtime；**只允许 `robot-logging` crate 用** |
| `ctor = "0.3"` | workspace.dependencies | 自动初始化 hook；**只允许 `robot-logging` crate 用** |

### 约束

1. **本 commit 是骨架 + 规范**，**现有 `eprintln!` / `println!` 调用不动** ——
   全量替换分后续多个小 commit 做（按 crate 切分，每次 30-60 min），避免一次
   改动过大。大致有 100+ callsite 需要迁移。
2. **新代码默认用 tracing**：task-32 之后所有新加的 log 必须用 `tracing::*!`
   + 统一词汇表。reviewer 看到 `eprintln!` 打非 debug 信息（非 fatal error
   stderr / 非 --help 输出）直接 block。
3. **`tracing-journald` 暂不接**：systemd 下 JSON 模式走 stdout → journald 能
   抓结构化字段（`PRIORITY=` 等）。接 native journald 留后续评估（需要比较
   JSON 抓取 vs native binary protocol 的运维友好度）。
4. **Error 结构保留**：打错误一律 `error!(?err, "...")`（`?` 走 Debug），不用
   `error!(err = %err, ...)` 或 `format!("{err}")` 丢结构。

### 验证

- `cargo fmt --all --check` ✅
- `cargo clippy --all --all-targets -- -D warnings` ✅（含 `--features real-hw`）
- `cargo test --all` ✅（robot-logging 新增 3 单测全绿；workspace 共 339 pass）
- `cargo build --all` ✅（matrix 4 组合无回归）

### 审阅

- 提议 → 决定：用户 loosqk（2026-04-25 本 session：`整个项目的日志也要用成熟
  的高性能日志库，就像 spdlog 之于 c++` → `日志格式也要统一方便查看` → `日志
  还要加上 monotonic 时间` → `用事实性的标准 error，例如 thiserror？都用最新
  版本` → `有统一 init 的吗，这要一个一个写`）
- 复审触发：
  - 全量替换 `eprintln!` 时发现词汇表缺字段 → 更新 `docs/logging.md` + CHANGELOG
  - RT 路径日志需求（v0.3+）→ 专门 task 设计 lock-free ring buffer + 异步 flush
  - `tracing-journald` 真切到 native binary protocol → 新 CHANGELOG 条目

---

## 2026-04-25 · task-31 B'a 看门狗扩展骨架：`robot-health` crate（**状态：登记 / 骨架落地**）

### 涉及小节

- session 路线图步骤 4（handoff 2026-04-25_1 §8 / §7.6.3 待办）
- `docs/logging.md` 字段词汇表 → 新增 `cpu_usage_pct` / `load_1m` / `load_5m` /
  `load_15m` / `mem_total_kib` / `mem_available_kib` / `mem_used_pct`
- 依赖 task-32（同日 commit `9cdeb1b`）的日志骨架 —— 本 task 产物直接用
  `tracing::info!` 输出

### 决定

- **独立 crate `robot-health`**：不进 `robot-core`（§4.2 零 runtime 依赖），
  也不强绑 `robot-s0`（其他 daemon 也能用）。
- **只做系统级 + 骨架**：CPU / memory / load 三项，5 秒周期；**不做** per-
  process / top5 / 配置化 —— 留 B'b。
- **手写 `/proc` parser**：不依赖 `sysinfo` / `procfs` crate，纯整数 / 字符串
  解析 + 22 单测（fixture + Linux live read）。macOS stub `Err` → 上层 fallback
  0 让 dev 机编译。
- **非 RT**：`SCHED_OTHER` 默认优先级，秒级周期，**不**挤占 `s0_rt_thread`。
- **`s0_daemon` 默认集成**：spawn 一个 `_health_handle`（Drop 自动 join），
  早失败分支也会触发（main 作用域内的 local），**不需要**改 `graceful_stop`。

### CPU 使用率差分算法

`/proc/stat` 第一行 `cpu user nice system idle iowait irq softirq steal ...`
（jiffies）。两帧差分：`busy_delta / total_delta * 100`。单帧无意义（返回
`0.0` 占位）。本机 12s smoke 实测：帧 1 = 0.0%、帧 2 = 8.91%、帧 3 = 8.98%。

### 影响面

| 文件 | 调整 |
|---|---|
| `Cargo.toml`（workspace） | members 加 `crates/robot-health`；path dep 加 |
| `crates/robot-health/` | **新增 crate**（2 文件 + 22 单测 + 1 doctest）：`lib.rs`（SystemSnapshot / MonitorConfig / spawn_monitor / MonitorHandle）；`proc.rs`（CpuTimes / MemInfo / LoadAvg + 纯整数 parser）|
| `crates/robot-s0/Cargo.toml` | 加 `robot-health` dep |
| `crates/robot-s0/src/bin/s0_daemon.rs` | 第 4.5 阶段（watchdog 之后）加 3 行 spawn_monitor + 5 行注释 |
| `docs/CHANGELOG.md` 本条 | 新增 |

### 验证

- `cargo fmt --all --check` ✅
- `cargo clippy --all --all-targets -- -D warnings` ✅（含 `--features real-hw`）
- `cargo test --all` ✅（robot-health 22 单测 + 1 doctest；workspace 共 361 pass）
- matrix features 本机 build ✅（4 组合）
- 本机 12s smoke：health 3 帧 tracing 输出，CPU 差分稳态 ~9%

### B'b 待做

- per-process 采集（`/proc/<pid>/stat` + `status`）：interested processes RSS / CPU%
- 全系统 top 5 by CPU
- TOML `[monitor]` 段：`interested_processes` 列表 + `interval_secs`
- 可选：`/<robot_id>/s0/system_health` iceoryx2 topic（定义 FlatBuffers schema，
  **先不做**，等真有跨进程订阅需求再加）
- B'c 候选：CPU > 阈值 → `Event::RtOverrun` → state_machine 进 `Fault`；依赖
  task-30 state_machine ↔ adapter 执行链路先接通

### 审阅

- 提议 → 决定：用户 loosqk（2026-04-25 `看门狗要盯着系统总体资源占用以及使
  用率，top5 进程，以及我的每个服务进程资源占用`）

---

## 2026-04-25 · task-31 B'b 看门狗扩展 per-process + top5 + TOML（**状态：登记 / B'b 完成**）

### 涉及小节

- 本条延伸 B'a（同日 commit `b59e286`）；两条合在一起 = task-31 B' 完整骨架
- `docs/logging.md` 词汇表 → 新增 `cpu_usage_pct` / `load_*` / `mem_*` /
  `interested` / `top_by_cpu`
- `robot-s0` TOML schema 加 `[monitor]` 段

### 决定

- **per-process CPU 语义与 `top` 一致**：`delta / total_cpu_delta * num_cpus *
  100`，单核 100% 满载 → 100.0；多核同时 → 超 100。不归一化到 (0, 100]，保留
  直觉语义。
- **`/proc/<pid>/stat` `comm` 字段解析坑**：括号可含空格 + 嵌入括号（`(kworker
  u:1-events)` / `(python 3)`）。解析走 `rfind(')')` 找最后一个右括号后再切字段。
  手写 parser + 4 个专项单测（simple / tricky parens / no parens / short）。
- **HashMap 生命周期管理**：`collect_process_snapshots` 每帧替换 `last_pid_cpu`
  为当前帧存活 pid，**短命进程 pid GC**，长期运行 HashMap 不增长。
- **TOML `[monitor]` 段**：三字段 `interval_secs` / `top_n` / `interested_
  processes`，全部可缺省（走 `Default`，兼容 v0.1 无 `[monitor]` 段 config）。
- **tracing 输出格式**：`interested` / `top_by_cpu` 做 `Display` 字段（一行展示：
  `pid:comm@cpu%[rssM],...`），不打散为嵌套字段 —— 保证"一行看全貌"，JSON 模式
  下为 embedded string（不嵌套 array）。

### 实测（本机 smoke 8s）

```
system health cpu_usage_pct=0.0  ... mem_used_pct=84.55
  interested=825550:s0_daemon@0.0%[20M]
  top_by_cpu=1:systemd@0.0%[10M],2:kthreadd@0.0%[0M],3:pool_workqueue_release@0.0%[0M]

system health cpu_usage_pct=8.83  ... mem_used_pct=84.95
  interested=825550:s0_daemon@1.5%[20M]
  top_by_cpu=3297879:python3@111.7%[38131M],2345972:python3@106.7%[32940M],3131702:rustdesk@20.4%[400M]
```

系统 CPU 差分 8.8%，`s0_daemon` 自身 1.5%（cargo build 负载已散去），`top_by_cpu`
捕捉到本机两个 python3（训练跑满多核 ≈ 110%）+ rustdesk (20%)。完全符合 `top`
视角。

### 影响面

| 文件 | 调整 |
|---|---|
| `crates/robot-health/src/proc.rs` | 加 `PidStatRaw` / `parse_pid_stat` / `read_pid_stat` / `parse_vm_rss_kib` / `read_pid_rss_kib` / `list_pids`；7 新单测（含 tricky comm parens / live self-pid read） |
| `crates/robot-health/src/lib.rs` | `ProcessSnapshot` 新增；`SystemSnapshot` 扩 `interested` + `top_by_cpu`；`MonitorConfig` 扩 `interested_processes` + `top_n`；`collect_process_snapshots` / `per_process_cpu_pct` / `num_cpus_online` / `parse_cpu_online` / `summarize` + 10 新单测 |
| `crates/robot-s0/src/config_loader.rs` | `RobotConfigToml` 加 `monitor: MonitorSection`（`interval_secs` / `top_n` / `interested_processes`，全部 `#[serde(default)]`） |
| `crates/robot-s0/src/bin/s0_daemon.rs` | spawn_monitor 从 `ConfigLoader.parsed.monitor` 构建 `MonitorConfig`，无 `[monitor]` 段 fallback `Default::default()` |
| `docs/logging.md` | 词汇表补 7 个字段（cpu / load / mem / interested / top_by_cpu）|
| `docs/CHANGELOG.md` 本条 | 新增 |

### 验证

- `cargo fmt --all --check` ✅
- `cargo clippy --all --all-targets -- -D warnings` ✅（含 `--features real-hw`）
- `cargo test --all` ✅（robot-health **42 单测 + 1 doctest 全绿**；workspace 共 381 pass）
- matrix features 本机 build ✅（4 组合）
- 本机 8s smoke：`[monitor]` 段 TOML 正确传到 spawn_monitor，interested / top5
  输出正确

### B'c 待做（supervisor + 重启策略）

用户 2026-04-25 明确 **走方案 C**（`B'c 同意c方案`）：

**方案 C：监控 + 决策上报，systemd 执行重启**

- `robot-health` 层**监控** CPU / RSS / 状态（本 B'a + B'b 已完成）
- **新增**：TOML `[monitor.rules]` 段声明阈值（`max_cpu_pct_sustained` / `max_rss_kib`
  / `restart_on_zombie` / `cooldown_secs`），sampler 发现违规 → `tracing::warn!`
  + 发 `Event::ServiceUnhealthy` 到 state_machine（待 task-30 执行链路接通后可用）
- **真动作** 仍由 systemd 执行：可选调 `systemctl restart <service>`（需要
  `robot-health` / `s0_daemon` 有 PolicyKit 权限）
- **不做**：自己 fork/exec 启动服务、自己 reap zombie、自己管 PID 1 语义（与
  §6.7 "systemd 是最外层看门狗" 冲突 → 方案 A 已被否）

工作量估计：~2 小时（rules + warn 级别联动）。留独立 task-31 B'c commit。

### 审阅

- 提议 → 决定：用户 loosqk（2026-04-25 `还要考虑稳定性，以及方案[可能所有
  服务从看门狗启动，如果是非正常退出，重启策略]` → 对齐三方案 → `B'c 同意c方案`）
- 复审触发：
  - B'c 实施时 supervisor ↔ state_machine 交互 → 依赖 task-30 先落地
  - PolicyKit / systemd D-Bus 权限粒度评估 → 可能改 systemd unit 配置

---

## 2026-04-25 · task-30 设计方案：state_machine ↔ adapter 执行链路（**状态：草案 / 等 review**）

### 涉及小节

- 路线图步骤 5（handoff 2026-04-25_1 §8 "task-30 高优先候选"）
- `docs/handoffs/2026-04-25_1.md` §7.5.2 架构缺口记录的延续
- **不动 `docs/00_design.md`**（§4.9 / §6.7 / §1.4 全保）

### 产出

- 新增 `docs/designs/task-30_state_machine_adapter_link.md`（523 行设计文档）
  - §1 问题陈述：当前 state_machine 只反映状态不驱动 adapter；EStop 不断扭是
    安全级 bug；ControlMode::Stop 被 adapter 忽略
  - §2 设计原则（硬约束）：RT 铁律 / systemd supervisor / trait 外部签名 / panic
    abort / 单测 100% 向后兼容 / 允许增量
  - §3 架构图：当前 vs task-30 后（io_helper thread 接管 adapter 所有权）
  - §4 候选方案 A-F 对比，**推荐方案 F**（adapter 在 io_helper 独占 + RT 读写
    lock-free POD）
  - §5 详细设计：AdapterReq/Resp + AdapterCtrlClient + AdapterCtrlServer
    + HotShared；`ManipulatorAdapter::hold_current_position` 加默认 no-op
  - §6 迁移计划 M1-M4 四个独立 commit（1 + 0.5 + 2 + 1 ≈ 4.5 h）
  - §7 测试覆盖：单测 / integration / soak / Orin 真机
  - §8 风险 R1-R4 + 回滚策略（每 commit 独立 revert）
  - §9 不做的事
  - §10 决议请求（5 个问题请 user 回复）

### 5 个决议请求（等 user review）

1. 整体方向方案 F 是否 approve？
2. 迁移计划 M1-M4 分 commit 是否合理？
3. `hold_current_position` trait 默认 no-op 是否接受？
4. rt_thread 降级为纯数据通路（5.5.b，一帧 ≤20 ms 延迟）是否接受？
5. feature flag `state_machine_adapter_link` 是否需要？默认 on / off？

### 审阅

- 提议：Claude Code（2026-04-25 路线图步骤 5 "A 设计方案"）
- 等决：用户 loosqk

### v2 修订（2026-04-25 user review 后）

User 给出 12 条 critical 反馈，全部接受。文档从 523 行 → ~860 行重写覆盖：

**核心 framing 变化**：v1 解决"谁拥有 adapter"；v2 必须同时解决"什么时候允
许 `latest_cmd` 写硬件" —— 后者才是安全闭环。原 v1 的 `hold_current_position`
设计**有破洞**：io_helper 仍然每 tick 转发 stale `latest_cmd`，会冲掉 hold
语义。

**主要 v2 加 / 改 / 删**：
- ➕ `§4 OutputPolicy` 新章节：7 variant（Disabled / ReadyHold / ActiveForward
  / HoldCurrent / ZeroCommand / StopTorque）+ `S0State → OutputPolicy` 映射表 +
  arc-swap 实现；adapter_io_thread 严格按 policy 决定是否转发 `latest_cmd`
- ➕ `§5.1` AdapterReq 拆 `Emergency` / `Normal` 双 channel + `estop_requested:
  AtomicBool` 双兜底；emergency 不被 normal 阻塞
- ➕ `§5.4` server loop 改 `crossbeam_channel::select!` + deadline，**防 busy
  spin**（断连不 CPU 100%）；明确不靠串口 RTT 当节拍器
- ➕ `§5.5` `tick_hardware` 按 OutputPolicy 决定输出；`cmd.mode = Stop` 真触发
  emergency safety_stop；mode 变化时调 set_mode 一次（不每 tick 调）
- ➕ `§5.7` `hold_current_position` trait 默认改 `Err(RobotError::Unsupported)`
  （**不**默认 no-op）；mock 显式 no-op；fallback：hold 失败 → safety_stop
- ➕ `§6 OutputSafetyState` 新章节：`TorqueOffConfirmed` / `TorqueOn` /
  `TorqueUnknown` / `StopFailed`；新 `FaultCode::SafetyStopFailed` /
  `AdapterUncertain`；对外 state topic 加 `safety_state` 字段（与现有
  `OutputMode` 正交：意图 vs 真实硬件状态）
- ➕ `§7 SIGTERM graceful` 新章节：明确不依赖 Drop / panic = abort 不跑 Drop；
  shutdown 走 emergency channel + bounded timeout；SIGKILL / panic 必须靠硬件层
- ➕ `§8 测试覆盖` 加 9 条用例：stale gate / Stop trigger safety / mode change
  once / emergency preempt / no busy spin / shutdown / adapter timeout /
  TorqueOffConfirmed E2E / OutputSafetyState 上报
- ➖ 删 v1 `§5.5.a` "RT 直调 adapter" 退路（user review §7：是方案 F 否定式）
- ➖ 改名 `io_helper` → `adapter_io_thread`（user review §7：原名太轻）
- 🔄 迁移计划 M1-M4 拆为 **M1-M5**（user review §6：M3 单独合并行为退化），
  feature flag 时间线明确：M1-M3 不存在 / M4 默认 off / M5 真机 smoke 通过后
  默认 on / **下个 release 必须 on + 删 flag**（安全链路不可永久 feature gated）
- 🔄 总工时 4.5 h → 6 h（更真实）

**v2 决议请求**（5 个新问题）：
1. v2 整体是否 approve？
2. `OutputPolicy` 7 variant 是否覆盖？需要加 `LimitedForward`（rate limit）吗？
3. `OutputSafetyState` 上 state topic 是否走 `00_design.md` §3.4 schema 评审？
4. M5 行为切换点是否拆两 commit（"flag on" + "删 legacy"）？
5. 真机最低门槛：feetech_poke 读 `Torque_Enable=0` + 物理推夹爪确认 —— 同意 / 加项？

详 `docs/designs/task-30_state_machine_adapter_link.md`（v2，~860 行）。

### v3 修订（2026-04-25 user 二轮 review 后）

User v2 review 给出 7 主问题 + 3 次问题，全部接受 + 重写覆盖文档。718 行
→ 744 行。**关键诚实修正**：v2 把 hardware IO 从 `s0_rt_thread` 移到普通线程
但**声称不改设计**——与 `docs/00_design.md:114` 冻结条款冲突。事实上 Feetech
串口 RTT ms 级**从来不是 HardRT**，但代码（`feetech/src/adapter.rs:202`）仍
标 `RtClass::HardRT`（注释里早就承认是"目标分类"）。v3 把这件事**正式化**为
`task-30-pre-1`。

**v3 新增 3 个前置 task**（必须 task-30 主体 M1-M5 之前完成）：

- **pre-1**（30 min + 评审）：Feetech 重分类 `RtClass::SerialRT`（新增 enum
  variant + 改 feetech adapter `RtClass::HardRT → SerialRT` + 走 CHANGELOG
  设计修订评审 + 加 `10_verification.md` SerialRT baseline 行）
- **pre-2**（1.5 h + 评审）：state schema 补字段（`mode/control_link/output
  _mode/safety_state` 全部加 `Unknown` variant 兼容旧 writer）+ build_helpers
  + pub_thread 真发字段 + robotctl 展示 + schema_compat 测试 + state_machine
  转换表加 `* → Fault(SafetyStopFailed)`（设计修订级 §1.4 / §3.4）
- **pre-3a/b**（45 min）：`RobotError::Unsupported(&'static str)` variant +
  adapter_io_thread 用 `RobotClock` 替代 `Instant::now`（业务代码禁用 §业务
  代码禁用 API）

**v3 主要 v2 修订**：

- ➕ §1.2 / §1.3 / §1.4 v3 新章节：架构修订级问题陈述（Feetech 不是 HardRT；
  schema 与冻结设计不一致）
- 🔄 §5.2 `AdapterCtrlClient` 重写**非阻塞**：v2 用 bounded(1) send 会阻塞
  ipc_thread → 心跳/watchdog/EStop 全卡死。v3 方案 A `pending op slot`（异步
  + state_machine 下次 tick poll resp）；**采用 A**
- 🔄 §5.4 server loop emergency `select!` arm 改**立即 dispatch** —— v2 写
  `recv → _ => loop` 会消费但不处理，丢 EStopLatch 这类语义消息
- 🔄 §5.6 `tick_hardware` `Velocity` 真闭环或 `Unsupported`：v2 set_mode 后
  仍调 `write_command`，但 feetech 对非 Position 静默 no-op = 假闭环。v3：
  `state.current_mode_supports_write` 标位 + 不支持 → `FaultCode::Adapter
  UnsupportedMode` 进 Fault
- 🔄 §6.3 state_machine 转换表加 `* → Fault(SafetyStopFailed)` 全状态接受 ——
  v2 只 `Enabled.* → Fault`，EStop 已 Safe 时再来 SafetyStopFailed 会被拒收；
  这是 §1.4 冻结条款修订，必须走评审
- 🔄 §6.4 `OutputSafetyState` 与 `S0State` **正交独立**（不合并到 Fault），
  state topic 同时暴露两字段（意图 vs 实测）
- 🔄 §7.2 SIGKILL 改写：**不是 task-30 验收项，是 blocking gap**。`docs/10_
  verification.md:102` 冻结 SLA "kill -9 → fail-safe < 500 ms" 留 `task-30-
  post`：servo Goal_Position watchdog 标定 + 硬件继电器
- ➕ §8 测试加 5+ 项：非阻塞 client / state_machine hook 不卡心跳 / SafetyStop
  Failed 全状态接受 / EStopLatch 不丢 / Velocity Unsupported / state schema
  字段全 / pub_thread 真发字段 / robotctl 显示 / **SIGKILL 不通过**（known issue）
- 🔄 §9 迁移加 pre-1/2/3 前置；M1-M5 工作量 6 h → 6.75 h
- 🔄 §11 7 个 v3 决议请求（取代 v2 5 问题）

**v3 7 个决议请求**：

1. v3 整体（pre-1/2/3 + M1-M5 + SIGKILL blocking gap）是否 approve？
2. pre-1 Feetech `RtClass::SerialRT` 走 §4.6 设计修订评审？还是用现有 `SoftRT`
   不新增 variant？
3. pre-2 schema 字段全部加 `Unknown` variant 兼容旧 writer？
4. §5.2 方案 A（pending op slot 异步） vs 方案 B（`send_timeout(50ms)` 紧上限）？
5. `* → Fault(SafetyStopFailed)` 修订是否走 §1.4 转换表评审 + dimos 同步回写？
6. §7.2 SIGKILL 留 `task-30-post` 同意？
7. 真机最低门槛 M5 smoke：`feetech_poke read Torque_Enable=0` + `robotctl status`
   显 `safety_state=TorqueOffConfirmed` + 物理推夹爪。同意 / 加项？

详 `docs/designs/task-30_state_machine_adapter_link.md`（v3，744 行）。

### v4 修订（2026-04-25 user 三轮 review 后 —— 架构级方向修正）

User v3 review 提出**架构级方向修正**：v3 把 hardware IO 永久移到 `adapter_io
_thread`，等于把原设计 §1.2 / §1.4 "HardRT adapter 直挂 s0_rt_thread" 给放
弃了。**这是过度修订**。

User 关键洞察：
- 原设计 §1.2 / §1.4 没错；rt_thread 直接调 HardRT adapter **对真 HardRT 仍
  然成立**（EtherCAT / RT-CAN / DMA mmap / 自家 FPGA shared-mem）
- Feetech 的问题不是设计错，是 **adapter 分类错** —— 标 HardRT 但 ms 级 IO
- 解法：**保留原设计 HardRT 路径不动**；**新增第三类** `RtClass::SerialRT`
  与 HardRT / SoftRT 并列；`s0_daemon` 按 `adapter.info().rt_class` dispatch

判定基准：**可证明的最坏延迟 + RT 铁律合规性**，不靠协议名字。EtherCAT 用了
会阻塞库 / 动态分配 / mutex 也不是 HardRT；某些 CAN bounded + no alloc 就是
HardRT。

**v4 主要修订**：

- 三类 executor 并列：`spawn_rt_thread`（HardRT，**原设计不动**）+ 新增
  `spawn_serial_io_thread`（SerialRT）+ `spawn_io_thread`（SoftRT，v0.4）
- v3 "rt_thread 改读 HotShared / 永久数据通路" → **v4 撤销**；rt_thread 保留
  原设计 `read cmd slot → adapter.write/read → write rt_snapshot`
- v3 OutputPolicy / emergency / IoState / 复杂语义 **范围限定到 SerialRT
  executor**；HardRT executor 的 OutputPolicy gate **简化版**（仅"是否调
  write_command" 分支，不做 hold fallback / mode change 等）
- §3 架构图重画：rt_thread + serial_io_thread + io_thread 三条并列
- §6 新增 `s0_daemon dispatch by RtClass`：`match adapter.info().rt_class`
  决定 spawn 哪个 executor
- 现有 `mock_hard_rt` 保持 `RtClass::HardRT` 不变，走 rt_thread 路径，v0.2
  测试 + smoke 零回归
- Feetech adapter 改 `RtClass::SerialRT`（pre-1），走 serial_io_thread 路径
- 风险加 R9（HardRT executor 简化版 OutputPolicy 语义边界）+ R10（dispatch
  by RtClass 错配的安全风险 + adapter 实现方分类纪律）

**v4 vs v3 本质区别**：
- v3：把 hot path 移走 → rt_thread 永久变数据通路 → **否定原设计**
- v4：**新加一条平行路径** → rt_thread 仍是 HardRT 真接入点 → **原设计完全保留**

文档 744 → 802 行（v4 重画 §3 架构图 + §6 dispatch + §10 R9/R10 + §0 v3→v4
修订表 + §12 7 个新决议）。

**v4 7 个决议请求**：

1. v4 整体（保 HardRT 路径 + 新 SerialRT executor + dispatch by RtClass）approve？
2. `RtClass::SerialRT` 走 §4.6 设计修订 + dimos 同步回写？
3. HardRT executor OutputPolicy gate **简化版** 接受（v0.4 真 HardRT adapter
   需要 hold fallback 等再升级 rt_thread）？
4. `s0_daemon` dispatch 走启动期 match `adapter.info().rt_class` 还是编译期
   trait method（`fn spawn_executor`）？
5. v0.2 范围内 `RtClass::SoftRT` 在 s0_daemon 走 `Err(Unsupported)` 不实施
   接受？
6. M5 行为切换点拆两 commit（flag on + 删 legacy）？
7. 真机最低门槛 M5：mock_hard_rt 走 rt_thread 不回归（task 29 ±17° 双夹爪
   重跑） + feetech 走 serial_io_thread（feetech_poke Torque_Enable=0 +
   robotctl safety_state=TorqueOffConfirmed + 物理推夹爪）。同意 / 加项？

详 `docs/designs/task-30_state_machine_adapter_link.md`（v4，~800 行）。

### v5 修订（2026-04-25 user 四轮 review 后）

User v4 review 给出 6 项 critical 修订，全部接受：

1. **HardRT 安全动作不能只靠"不调 write_command"** —— 与 §0 / §1.4 :149
   "cut output / torque disable" 冲突（"不写"≠"断扭"）。**新增**
   `ExecutorCtrlClient` trait 统一抽象：state_machine 只表达意图，
   HardRT impl 用 atomic action slot（rt_thread tick 内 RT-safe 调
   adapter）+ SerialRT impl 用 emergency / normal channel
2. v4 §4 自相矛盾（"仅 SerialRT" vs "HardRT 也读 policy"）→ v5 重写
   "OutputPolicy 是**所有 motor executor 的统一门控**；执行动作因 executor
   类型不同"
3. RT 路径 `ArcSwap<OutputPolicy>` 有 hazard pointer 复杂度 → 改 **`Atomic
   U8 + #[repr(u8)]`**：`OutputPolicy` 8 variant 单字节够用；RT 读 < 1ns 无
   alloc
4. Feetech `enable(false)` 13 motor × 3 retry × 150ms = 最坏 5.85s 不适合
   emergency → **新增 `safety_stop_fast()`** 走 sync_write broadcast
   (id=0xFE, ~1-2ms)；保留 `enable(false)` 给普通路径
5. SIGKILL 列 blocking gap 但 verification.md:102 仍是 v0.2 SLA → **明确**
   task-30 完成 **≠** v0.2 safety exit criteria 通过；只完成"graceful 软件
   断扭"；硬件 fail-safe 留 task-30-post
6. HardRT `ControlMode::Stop` 合约缺失 → **新增 `RtSafeCtrl` 子 trait**
   （HardRT adapter 必实现）+ `ManipulatorAdapter::write_command(mode=Stop)`
   合约：HardRT adapter 必须实现为 RT-safe cut output，**不**得 no-op

**v5 新增**：

- §4 `ExecutorCtrlClient` trait（4 方法：`set_policy` / `safety_stop` /
  `request_normal` / `poll_last_normal_resp`）；HardRT impl 用 atomic action
  slot；SerialRT impl 用 emergency / normal channel；state_machine `Box<dyn>`
  持有，不感知底层
- §5.1 `OutputPolicy` AtomicU8 + `repr(u8)` 实现
- §5.2 OutputPolicy 在两类 executor 的执行表（HardRT 简化数据 gate；SerialRT
  完整语义）
- §5.3 `RtSafeCtrl` 子 trait（HardRT adapter 必实现 `safety_stop_rt` /
  `enable_rt` / `set_mode_rt`，且 `write_command(Stop)` 必须 RT-safe cut
  output）
- §7 `FeetechAdapter::safety_stop_fast()` 详细设计（sync_write broadcast,
  1-2ms vs 5.85s 慢路径）
- §9 SIGKILL 措辞调整：task-30 完成不等于 v0.2 SLA 通过
- §10 测试加 5+ 项：OutputPolicy AtomicU8 / HardRtCtrlClient action_slot /
  feetech safety_stop_fast 时延 / RtSafeCtrl 合约 / 多 executor 并存
- §11 M1-M5 拆为 M1-M6（加 `RtSafeCtrl` + `safety_stop_fast` + ExecutorCtrl
  Client 三个独立 commit）；总 ~8 h（v4 估 6.75 h，更真实）
- §12 风险加 R11（RtSafeCtrl 合约违约）+ R12（safety_stop_fast 不等 ACK）

**v5 8 个决议请求**：

1. v5 整体（ExecutorCtrlClient + AtomicU8 + RtSafeCtrl + safety_stop_fast）
   approve？
2. ExecutorCtrlClient 4 方法是否覆盖完整？要加 clear_pending / cancel 吗？
3. OutputPolicy 用 AtomicU8 不用 ArcSwap 同意？
4. RtSafeCtrl 子 trait 强制 HardRT adapter 实现接受？
5. FeetechAdapter::safety_stop_fast (sync_write broadcast id=0xFE + 不等 ACK
   + 1-2ms) 设计同意？
6. verification.md:102 状态改 "⏳ blocking gap (软件不可解；待 task-30-post)"
   同意？task-30 实施后此项**仍 ⏳**。
7. M5 行为切换点拆两 commit（flag on + 删 legacy）？
8. 真机最低门槛 M6 smoke 加新项：mock_s1 发 ControlMode::Stop → feetech_poke
   在 mock_s1 跑时 read Torque_Enable=0（验证 Stop 路径走通）。同意 / 加项？

详 `docs/designs/task-30_state_machine_adapter_link.md`（v5，~700 行）。

### v6 修订（2026-04-25 user 五轮 review 后）

User v5 review 给出 3 blocker + 4 修正，全部接受。

**3 Blocker**（不修不能开工）：

- **B1**：`safety_stop_fast` 成功立刻标 `TorqueOffConfirmed` —— 但 fast path
  不等 ACK，"软件下发"≠"硬件已断扭"。**新增 `OutputSafetyState::StopCommanded`**
  variant：下发成功 → `StopCommanded`；下一 tick `confirm_torque_off()` 读回
  → 一致升 `TorqueOffConfirmed`，不一致 → `AdapterUncertain`，读失败 →
  `StopFailed`
- **B2**：现有 `JointStateBuf` / `read_state` 没有 `Torque_Enable` 字段，"下
  一 tick read_state 确认"接口不支持。**新增独立 `OutputSafetyProbe` trait**
  （`fn read_torque_enable_mask(&mut self, out: &mut [bool])`），不污染普通
  hot path；Feetech impl 走 sync_read Torque_Enable 寄存器
- **B3**：HardRT 路径 `ArcSwap<Option<AdapterNormalReq>>` 冷字段在 rt_thread
  `load_full` → 触发 Arc refcount → **违反 §4.9 RT 铁律**。HardRT 控制动作
  **全部** atomic 编码（`emergency_action: AtomicU8` / `normal_action: AtomicU8`
  / `action_arg: AtomicU8` / `action_seq: AtomicU64` / `safety_latched:
  AtomicBool`）；移除所有 ArcSwap / Vec；不可 atomic 编码的请求 → `Unsupported`

**4 修正**：

- **C1** `action_slot` last-writer-wins 危险（SafetyStop 被 Enable(true) 覆盖）
  → 拆 emergency_action + normal_action 两 atomic + safety_latched AtomicBool
  锁住 Enable 直到 ClearEStop
- **C2** `OutputPolicy::from_u8` "全 8 值"措辞错（enum 只 6 variant）→ 改"6
  valid + 兜底 `Disabled`"
- **C3** "Mock + Feetech 不实现 RtSafeCtrl" 自相矛盾 → **Mock (HardRT) 实现** +
  Feetech (SerialRT) 不实现
- **C4** `safety_stop_fast` 伪代码分配 Vec → adapter 持 `safety_stop_buf` +
  `confirm_torque_buf` 启动期预分配；运行期不 alloc

**v6 新增章节**：

- §0 v5→v6 修订记录（3 blocker + 4 修正表）
- §2 加 3 条硬约束（17/18/19）：safety 路径区分下发 vs 确认；RT 路径全
  atomic；安全路径不分配
- §3 架构图 v6（HardRT 全 atomic 字段 + emergency/normal 拆 + safety_latched）
- §4.2 HardRtCtrlInner v6 重写（全 atomic 字段 + rt_thread 主循环消费两 atomic）
- §5.1 OutputPolicy::from_u8 措辞修正
- §5.3 OutputSafetyState 加 StopCommanded variant
- §5.4 OutputSafetyProbe trait 详细设计（独立路径）
- §5.5 RtSafeCtrl 实施方修正（Mock 实现 / Feetech 不实现）
- §6 SerialRT confirm_torque_off 路径完整实现（B1+B2 闭环）
- §7.2 Feetech safety_stop_fast 预分配 buffer + OutputSafetyProbe impl
- §10 测试加 13 项（含 assert_no_alloc soak / safety_latched reject /
  StopCommanded → TorqueOffConfirmed 二阶段 / Mock 实现 RtSafeCtrl 编译验证）
- §11 M1-M7（v5 M1-M6 拆 OutputSafetyProbe 单独 commit）；总 8h → 9.5h
- §12 风险加 R13（HardRT atomic 编码上限）+ R14（confirm 一次 RTT）

**v6 8 个决议请求**：

1. v6 整体 approve？
2. OutputSafetyState 加 StopCommanded 同意（B1）？
3. OutputSafetyProbe 独立 trait（不塞 read_state）同意（B2）？
4. HardRT 全 atomic + 复杂请求 Unsupported 接受（B3）？
5. safety_latched + Enable(true) reject 接受（C1）？
6. M1-M7 拆开总 9.5h 合理？
7. M7 拆两 commit（flag on + 删 legacy）？
8. 真机最低门槛加新项：Stop → safety_state 先 StopCommanded 后 TorqueOffConfirmed
   + SIGTERM 后 feetech_poke read Torque_Enable=0。同意 / 加项？

详 `docs/designs/task-30_state_machine_adapter_link.md`（v6，~860 行）。

### v7 修订（2026-04-25 user 六轮 review 后）

User v6 review 给出 5 项卡点，全部接受（706 行 → 700+ 行）：

1. **HardRT 提交丢请求 + seq 误确认**：v6 `store + fetch_add` 有数据竞争 →
   normal_action 用 `compare_exchange(NONE, action)` 占用 slot；占用失败返
   `NormalReqStatus::Busy`；seq 在 cas 成功**之后**分配 `fetch_add(1) + 1`
   保证与 action 1:1 配对；rt_thread 完成时按 result→seq→ready 顺序写
2. **rt_thread 调 connect / disconnect / hold_current_position 不是 RT-safe**：
   HardRT normal_action 编码集**裁剪**只允许 `RtSafeCtrl` 方法
   （`Enable(bool)` / `SetMode(Position)`）；`Connect / Disconnect` 移
   到 `s0_daemon` main（spawn 前 connect / join 后 disconnect）；
   `HoldCurrent` 在 HardRT 由 `OutputPolicy::HoldCurrent` gate 隐式实现
   （不调 write_command = motor 维持 last Goal）；SerialRT 才走真 helper
3. **`sync_write_by_id` 内部 alloc** 与 "预分配" 承诺冲突 → M4 加
   `bus.sync_write_by_id_into` + `protocol.build_sync_write_into` 预分配
   frame API（与现有 `sync_read_by_id_into` 对称）；safety_stop_fast 走新
   API；`assert_no_alloc` 单测覆盖
4. **`OutputSafetyState::AdapterUncertain` 自相矛盾**（enum 到 StopFailed=4
   但后用 AdapterUncertain）→ 加 `AdapterUncertain = 5` variant；语义：下
   发成功但 confirm 读回 mask 部分一致（≠全断扭，≠全失败）；与 `StopFailed`
   区分明确
5. **pre 任务无具体文件 / 行号** → §11.1 重写 pre 表，每个 pre 列具体文件路径 +
   当前代码状态 + 改动 + 评审：
   - pre-1a：`crates/robot-core/src/adapter.rs:31` (RtClass enum 加 SerialRT)
   - pre-1b：`crates/robot-adapters/feetech/src/adapter.rs:202` (HardRT → SerialRT)
   - pre-1c：`docs/00_design.md` §4.6 + dimos 同步回写
   - pre-1d：`docs/10_verification.md` 加 SerialRT baseline 行
   - pre-2：`crates/robot-msg/schemas/joint.fbs:36` + state_machine
     `* → Fault(SafetyStopFailed)` 转换表
   - pre-3a：`crates/robot-core/src/error.rs:28` (加 Unsupported variant)
   - pre-3b：serial_io_thread 走 RobotClock 替换 `Instant::now`

**v7 主要新增**：

- §0 v6→v7 修订记录（5 项 + 修订位置）
- §2 加硬约束 20：RT slot 提交协议 1:1 配对（CAS 占用 + seq cas 后分配）
- §3 架构图修订 normal_action 协议（CAS occupy / Busy reject）
- §4.2 HardRtCtrlClient 重写：CAS occupy + RtSafeCtrl-only 编码 +
  Connect/Disconnect/HoldCurrent 移出 rt_thread
- §5.2 OutputPolicy 在 HardRT 的实现表更新（HoldCurrent 隐式 / Stop 走 trait
  合约）
- §5.3 OutputSafetyState 加 AdapterUncertain = 5
- §6 confirm_torque_off 三分支（all_off → TorqueOffConfirmed / any_off →
  AdapterUncertain / none_off → StopFailed）
- §7.2 sync_write_by_id_into 预分配 API 完整设计
- §10 加 R15（HardRT HoldCurrent 隐式 hold 假设）
- §11 M1-M7 + M4 重写 + 总 9.5h → 10h
- §12 8 个 v7 决议请求

**v7 8 个决议请求**：

1. v7 整体 approve？
2. NormalReqStatus::Busy 语义：调用方收 Busy 直接放弃本次 request 等下次
   transition 重试 同意？
3. HardRT Connect/Disconnect 移 main + HoldCurrent OutputPolicy gate 同意？
4. R15 "HardRT motor 默认 internal hold last Goal" 假设可接受？
5. sync_write_by_id_into / build_sync_write_into 预分配 frame API 设计同意？
6. pre 任务表列具体文件 + 行号清晰可执行？
7. M7 拆两 commit（flag on + 删 legacy）？
8. 真机门槛加新项：Busy semantics + safety_stop_fast assert_no_alloc +
   AdapterUncertain 三分支路径测。同意 / 加项？

详 `docs/designs/task-30_state_machine_adapter_link.md`（v7，~700 行）。

### v8 修订（2026-04-25 user 七轮 review 后）

User v7 review 给出 5 项 blocker，全部接受：

1. **HardRT normal 提交仍有竞态**（v7 先 store arg 再 cas action_code → arg
   撕裂；cas 后 fetch_add seq → action 与 seq 不匹配）→ 改 **单 packed
   AtomicU64 slot**（bits 0-7 action / 8-15 arg / 16-63 seq），单次 CAS(0,
   packed)；seq 先 fetch_add，cas 失败时该 seq 废弃（允许空洞）
2. **SafetyStop 后同 tick 仍执行 pending Enable(true)**（v7 RT loop emergency
   后无条件处理 normal）→ emergency 处理后**立即** `pending_normal.swap(0)`
   drain，写 `RESP_ERR_PREEMPTED_BY_EMERGENCY`；之后处理新 normal 时仍二次
   check `safety_latched`
3. **Busy 不能"放弃本次 request"**（transition 不一定再发，逻辑 Enabled 但
   硬件没 enable）→ ipc_thread 加 retry 协议 `pending_action: Option<(req,
   Option<seq>)>`；in-flight 未完成不开新 transition；副作用 transition
   (READY→ENABLED) 必须等 poll Ok 才认为对齐；超时 1s 升 Fault
4. **safety_latched 没有清除 API**（trait 无暴露）→ trait 加第 5 method
   `fn clear_safety_latch(&self)`；调用顺序约定 = 清 EStop latch → 清 Fault
   → clear_safety_latch；HardRT impl atomic store false
5. **sync_write_by_id_into 不严格 no-alloc / wire-incorrect**（v7 误用 `&mut
   Vec<u8>` + push；用了不存在字段 `reg.size`；自己算 LEN 错）→ 改 `&mut [u8]`
   slice + 内部栈数组 `[(u8, [u8; 4]); MAX_JOINTS]`；`build_sync_write` 内部
   `vec!` → `[u8; MAX_PARAMS]` 栈数组（与现有 `build_write` 模式一致）；用
   `register.length` 字段（事实）；LEN/wire 由 `build_instruction` 处理（不
   自己写）

**v8 主要新增**：

- §0 5 项 blocker 修订表
- §2 加硬约束 21/22/23：单 packed atomic CAS / emergency 抢占 in-flight /
  副作用 transition 不可丢
- §4.1 trait 加 5 method：`clear_safety_latch`
- §4.2 HardRtCtrlInner 重写：`pending_normal: AtomicU64` packed +
  `seq_counter: AtomicU64` + `last_completed: AtomicU64` packed；`pack_normal`
  / `unpack_normal` / `pack_completed` helper
- §4.3 rt_thread 主循环重排：emergency drain pending + 二次 latch check
- §4.4 新增 ipc_thread retry 协议章节（in-flight 状态机 + 1s 超时）
- §4.5 SerialRT clear_safety_latch impl
- §7.2 `bus::sync_write_by_id_into` slice-based API + 内部栈数组 +
  `register.length` fix；`protocol::build_sync_write` 内部 vec → 栈数组
- §9 测试加 13+ 用例（pack/unpack 单测 / Busy semantics / drain pending /
  retry 协议 / 1s 超时 / clear_safety_latch 顺序 / assert_no_alloc）
- §10 R16 (Busy 死循环) + R17 (seq u48 wrap)
- §11.2 M4 / M5 重写；总 10h → 11h

**v8 8 个决议请求**：

1. v8 整体 approve？
2. pending_normal AtomicU64 packed (8/8/48) 是否合适？预留 reserved bits？
3. emergency 后 RT drain 全部 pending（不只 Enable）同意？
4. ipc retry 协议（in-flight 不完成不开新 transition + 1s 超时升 Fault）同意？
5. `clear_safety_latch` 调用顺序约定要 trait doc 强制 / runtime 校验？
6. M4 改动 (protocol 栈数组 + bus.sync_write_by_id_into slice API) 影响现有
   `sync_write_by_id` 调用方？保留 vs 删除？
7. M7 拆两 commit（flag on + 删 legacy）？
8. 真机门槛加新项：Busy retry / emergency drain pending / clear_safety_latch
   顺序违反 reject。同意 / 加项？

详 `docs/designs/task-30_state_machine_adapter_link.md`（v8，~880 行）。

### v9 修订（2026-04-25 user 八轮 review 后；**最终设计版本**）

User v8 review 5 项 finding，全部接受。**v9 之后建议直接进入实施，不再追加
v10**。

5 项 finding：

1. **副作用 transition 仍可能先 commit 再等硬件 ack**：v8 伪代码立刻 set_policy
   (from_state(new))，state topic 出现 "Enabled 但硬件未 enable" 的撒谎窗口
   → **改两阶段**：`plan_transition(old, event, guards) → Option<PlannedTransition>`
   不提交；副作用 transition 走 pending → poll Ok 才 `commit_transition` +
   `set_policy`；emergency 立即 commit（state 进 Safe，硬件由 OutputSafetyState
   异步反映）；新 transition kind: Pure / Emergency / Effectful

2. **SerialRT clear_safety_latch fire-and-forget 顺序不可靠**（trait void +
   走 normal channel ClearLatch + ArcSwap）→ SerialRT `safety_latched` 改
   `Arc<AtomicBool>` **与 HardRT 完全对齐**；trait method 直接 atomic store
   false（同步 + 立即 visible）；**移除** `AdapterNormalReq::ClearLatch`
   channel 路径

3. **测试 vs 设计自相矛盾**：v8 §9 写"第二次 Busy 不消耗 seq counter"，但
   §0 #1 说"seq 先 fetch_add，cas 失败 seq 废弃" → 改测试期望"允许 seq 空洞，
   Busy 不覆盖 packed slot；fetch_add 永远 +1（即便 cas 失败该 seq 废弃）"

4. **`sync_write_by_id_into` borrow checker 问题**：v8 伪代码 for 循环内
   `entry_refs[i] = (*id, &data_bufs[i][..])` 与 `split_bytes_le(..., &mut
   data_bufs[i][..])` 同时 mut + shared borrow → 编译不过 → **两阶段拆**：
   阶段 1 全部 mut 写 data_bufs；阶段 1 结束（drop mut）；阶段 2 shared 构造
   entry_refs；M4 加 cargo build + clippy 编译验证

5. **frame buffer 大小注释不准确**：v8 写 "258 = 4 header + 254 payload + 1
   chksum"，与 `build_instruction:104` 实际公式 `6 + params.len()` 不符 →
   注释 fix：`max frame = 6 (FF FF ID LEN INST CHKSUM) + params_len`；
   MAX_JOINTS=32 × length=4 → params=162 → frame=168 byte；预分配 256 留余量

**v9 主要新增**：

- §0 5 项 finding 修订表
- §2 加硬约束 24/25：副作用 transition 两阶段 / latch 字段语义跨 executor 一致
- §4.4 重写为两阶段 transition 协议：`plan_transition` / `commit_transition`
  / `commit_fault` API；`PlannedTransition { new_state, new_policy, kind }`；
  `TransitionKind::{ Pure, Emergency, Effectful{action} }`；ipc_thread retry
  coordinator 完整伪代码
- §4.5 SerialRT `safety_latched: Arc<AtomicBool>`（与 HardRT 对齐）；
  `clear_safety_latch` 直接 atomic store；移除 ClearLatch channel
- §7.2 sync_write_by_id_into 两阶段 borrow（mut data_bufs → shared entry_refs）；
  frame size 注释 fix（`6 + params_len` 公式 + MAX_JOINTS=32 计算）
- §9 修测试 #3 期望（seq 空洞允许）+ 加 v9 #1/#2/#4/#5 用例
- §10 R18：两阶段 transition 增加 ipc tick 延迟（HardRT 21ms / SerialRT 100ms）
- §11.2 M7 +0.5h 加两阶段 transition；总 11h → 11.5h

**v9 8 个决议请求**：

1. v9 整体 approve？
2. PlannedTransition 三分类（Pure/Emergency/Effectful）覆盖 state_machine 12
   events 完整？
3. emergency `Pure` 分支立即 commit state（意图）+ safety_state 反映硬件实际
   接受 R18？
4. SerialRT safety_latched 改 Arc<AtomicBool> 与 HardRT 对齐 同意？
5. plan_transition / commit_transition API 扩展 vs 替换 try_transition？
6. M7 +0.5h 加两阶段 transition 总 11.5h 合理？
7. M7 拆两 commit（flag on + 删 legacy）？
8. 真机门槛加新项：副作用 transition 期间 robotctl 看到旧 state /
   SerialRT clear_safety_latch 后立即 Enable / sync_write_by_id_into 编译过。
   同意 / 加项？

详 `docs/designs/task-30_state_machine_adapter_link.md`（v9，~840 行）。

**v9 之后预期路径**：approve → 按 pre-1a/b/c/d → pre-2 → pre-3a/b → M1-M7
顺序进入 task-30 实施。如有更深问题应**实施时反馈到 v10**（设计永远不能完
全闭环；实践才能进一步暴露）。

---

## 2026-04-25 · Step 8 EStop latch 双文件 + 重启恢复（**状态：登记 / 实施完成**）

### 涉及小节

- `docs/01_implementation_steps.md:1366` Step 8 `EStop latch 文件双写 + 重启
  恢复 (fsync 双文件、读取校验、持久化测试)`
- `docs/00_design.md` §6.6 / §6.7 / §1394 风险表 "fsync 双写 + 备份文件"
- `docs/10_verification.md:101-102` Step 8 SLA：EStop latch 持久化必过
- 路线图步骤 6（C task）

### 现状识别

`crates/robot-s0/src/estop_latch.rs` 已存在（task 13 commit `de6ffc6`）：tmp +
fsync + rename + parent dir fsync + 11 单测。但**只写一份文件**，与设计原文
"fsync **双写** + 备份文件"差一份 backup。Step 8 正式补完。

### 修改内容

- `crates/robot-s0/src/estop_latch.rs`：
  - **`save_estop_latch`** 改为双文件：先写 primary 再写 backup（`<path>.bak`），
    每份独立走 tmp + fsync + rename + dir fsync。primary 写完 crash → primary
    新值 + backup 旧值（不丢更新，因为 primary 是权威）；primary 失败 → 直接
    报错（**不**回退到只写 backup）。
  - **`load_estop_latch`** 加 fallback + 自愈：
    - primary 合法 → 用之
    - primary `NotFound` → 试 backup → 拿到合法值后**回写 primary**（自愈）→
      backup 也 `NotFound` → return `false`（首次启动）
    - primary `InvalidData`（损坏）→ 试 backup → 合法 → 回写 primary 自愈；
      backup 也损坏 → return `InvalidData`（**不**默认 false，调用方按 Fault 处理）
  - **`clear_estop_latch`** 同时删 primary + backup（任一 NotFound 视为成功）。
  - 新增 `pub fn backup_path(&Path) -> PathBuf`：派生 `<primary>.bak` 路径。
  - 抽出私有 `read_single` / `save_single_atomic` 共享逻辑。
  - **新增 12 单测**（原 11 + 新 12 = 23 共）：backup_path 派生 / 双文件写入 /
    primary 缺失 fallback / primary 损坏 fallback / 两份都缺失 / 两份都损坏
    （Err InvalidData 不默认 false）/ backup 损坏走 primary / clear 双删 /
    save 覆盖双文件 / 部分损坏不主动修 backup（设计选择：primary 权威）/ restart
    持久化往返。
- `docs/10_verification.md:101`：Step 8 EStop latch 持久化往返 `⏳ → ✅`；
  新增"双文件 fail-over"行 ✅；同时把 `kill -9 → fail-safe < 500 ms`
  状态改 **`⏳ blocking gap`** + 标 task-30-post（task-30 v3 §7.2 已对齐）。
- 不动 `s0_daemon.rs`：`load_estop_latch` / `save_estop_latch` 公开 API 签名
  不变，调用方零改动；backup 路径自动派生。

### 关键决策

- **API 兼容**：保留单 path 参数；backup 隐式派生 `<primary>.bak`。这样
  `s0_daemon` 既有路径计算（`/var/lib/robot/<id>/estop_latch`）/ CLI flag
  `--latch-path` / 单测都不用改。
- **primary 权威**：load 时 primary 合法即用，不主动检查 backup 是否对齐。
  backup 仅在 primary 失败时兜底。设计选择是"备份文件"而非"双副本同步"。
- **两份都损坏 = Fault**：不默认 `false`，避免"系统级故障被掩盖成首次启动"。
  调用方（`s0_daemon`）应进 Fault 状态。
- **`kill -9 → fail-safe` 状态明确为 blocking gap**：与 task-30 v3 §7.2 对齐。
  软件层无法保证 SIGKILL 后的 fail-safe；需要 servo timeout 标定 + 硬件继电器
  （task-30-post）。Step 8 不闭环此项。

### 验证

- `cargo fmt --all --check` ✅
- `cargo clippy --all --all-targets -- -D warnings` ✅（含 `--features real-hw`）
- `cargo test --all` ✅（estop_latch 23 单测；workspace 共 393 pass / 0 fail）
- 单测覆盖 §6.7 "fsync 双写 + 备份文件"全部失效模式：primary 损坏 / 缺失 /
  两份都坏 / 部分损坏 / clear 双删 / restart 往返

### 未做（留 task-30 / Step 9）

- **EStop topic → state_machine 真触发 `safety_stop()`**（adapter 真断扭）
  → task-30 M5
- **`ClearEStop` RPC 端点**（设计 §6.7 要求 maintainer+）→ Step 7+ RPC 网关
- **物理 estop button → latched < 5 ms**（`docs/10_verification.md:100`）→ Step 10 真硬件
- **kill -9 s0 → fail-safe < 500 ms**（`docs/10_verification.md:102`）→ task-30-post
  （servo timeout 标定 + 硬件继电器）

### 审阅

- 提议：Claude Code（路线图步骤 6 "C Step 8 EStop latch"）
- 实施：本 commit
- 复审触发：
  - task-30 实施时 `EStopLatch` emergency 路径调 `save_estop_latch(true)` →
    一并验证双文件路径在真机 SIGTERM 路径下能落盘
  - bus2 / 13-motor 多 hardware_id 时 latch 路径计算 → `s0_daemon` 路径策略可能
    需要扩
