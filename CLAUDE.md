# robot-system（`robot-core`）

> Rust 通用机器人运行时。S0/S1/S2 三层 + iceoryx2 + FlatBuffers + 外部 RPC。
> 架构设计已冻结 —— 一切以 [`docs/00_design.md`](docs/00_design.md) 为准；不在代码 PR 中改架构语义。

## 新 session 必读（按顺序）

1. 本文件 `CLAUDE.md`
2. [`docs/02_start_here.md`](docs/02_start_here.md) — Claude Code 上手指南
3. [`docs/00_design.md`](docs/00_design.md) — 冻结设计（权威）
4. [`docs/01_implementation_steps.md`](docs/01_implementation_steps.md) — v0.2 实施步骤

冲突裁决：`00_design.md` > 本文件 > 全局 `~/.claude/CLAUDE.md` > `02_start_here.md`。

## 状态

> 已完结阶段的流水账（commit 链 / 审计条目 / 测试计数）整段归档至
> `docs/handoffs/v0.5_status_archive.md`，逐字保留未删改。本节只留当前态——
> AGENTS.md 每轮都整份进模型上下文，历史留在这里等于每次请求重复付费。

```
设计版本:  v0.1.2（冻结）

实现阶段:  v0.5 候选 E ✅ 受监督功能 demo 完结（2026-09-01 真机：单臂×2 +
           双臂 + 双夹爪全 PASS + RT 三层加固回归全绿；收口宣称唯一口径 =
           "机器人控制执行面受监督功能 demo 跑通"）。
           D15 硬件 fail-safe 保持 OPEN——不宣称生产替换 / 无人值守；正式
           退役等 post-E 生产安全硬化候选闭环（D-BI3 r5：旧栈已 disable
           自启，unit 保留可回滚）。
           当前 = post-E 通信故障硬化 Spec Approved（2026-09-05 round 21，
           8 项决策全部 ACK）；函数级 Plan Approved，按用户2026-09-14最新分工：
           root定方案与评审，原生子agent具体开发和测试，各阶段验收通过即提交。
           实施状态与证据 = team-swe/specs/post_e_comm_fault_hardening.HANDOFF.md；
           已落地：新权限、高/普通双队列、唯一writer、RPC责任账本与受监督关闭链。
           R4c监督退出与真实重启、R4d-A原连接竞争已验收提交；A基线f9a85be。
           R4d-B双主机同源软件验收已通过：本机14fresh/53复用，Orin有效67门。
           原四失败保留，独审与root验收均通过；详HANDOFF及H包，不称单轮全过。
           B已提交cdeddd2并推送origin；用户2026-09-20授权继续P-E软件长负载。
           P-E两端All72、4096负控和8192长载已验收提交2f1de02，未push。
           P.7基线7c9ffaa已验收。9/22左H2一次前置拒绝，driver exit1/reap，
           authorize/launch均0，未触电机；原拒绝及两个未知PID清场FAIL保留。
           预检修复1b6db3e/4191bf0已独审；50项Python、真实sudo集成通过。
           Orin新build HEAD4191bf0：生产4/测量3、7help/15收尾与只读复检通过。
           首native后零启动计数误拒已修，原FAIL保留；不称单轮全过。
           该软件修复已收口b4ff720；后经继续及“本次全部就绪”确认执行一次左H2。
           本次RT门通过，topic-idle 100+1000 PASS；topic-inflight 100+639后FAIL。
           seq13681注入请求3895634ns、实测3894208ns，短1426ns触发原严格门。
           authorize/launch各1，工具与driver exit1/真实reap；独立断扭和清场成功。
           第3/4组未执行，原FAIL证据已收口eb59bcc，不自动重试。
           注入延时/尾窗修复fb600fd已软件验收，c5daa67运行本次新左H2通过。
           四组各100预热+1000正式、原P99门通过；工具/driver exit0真实reap，断扭/清场通过。
           独审/root已验收，本轮已收口10cff7a；三次旧测量FAIL及一次前置拒绝完整保留。
           第三组SLA-B max52.502592ms保留，判据为P99；收尾电源UNKNOWN，本轮值守已解除。
           右工具0047920与W1测试2e5b515已本地集成并验证；合并态64Python及最终SHA10项、fmt/Clippy通过。
           合并5952498已完成Orin新原生绑定并通过独审：6recipes、生产4/测量3、7help、64+10、单mock4×2。
           18次真实完成、122原件已核；原生软件证据已归档并收口4441f59，实际build仍5952498。
           右H13本地准备已收口9afdc92；首Orin22方法3FAIL/3ERROR因Python缺pidfd API，原FAIL保留。
           native/implementation_r3按能力选择自有child回收，本地27+独立5及Orin27/默认入口通过。
           该历史软件阶段真实wait、前后状态相同且无收养child；已归档，历史round6与早轮回执缺项保留。
           右H13软件已收口3790fd9；9/23本次右H1/H3各100+1000测量通过，实际build仍5952498。
           两工具真实reap/清场成功；首readonly冲突与setsid前置拒绝FAIL保留，证据已独审/root验收并归档。
           右rx_trust.lock由工具首次合法创建，marker仍缺失、latch不变；值守已解除、电源UNKNOWN。
           W1 fork/exec窗口机制已证实，旧偶发失败具体根因仍OPEN_UNCONFIRMED；原FAIL保留。
           历史交接（截至2026-09-25）：左H1/H2/H3、右H1/H3通过；右H2四组测量PASS但最终Disable约2秒Cancelled，
           整体run/report FAIL；首USB瞬态及漏传alignment零启动FAIL保留。工具/driver真实reap，
           独立清场PASS，右marker/latch均Dirty；用户2026-09-23先确认断电、后报告通电且电源/USB重插，
           本次交接时物理现状未知，支撑/接触区未重新确认，重插不构成新H2授权。软件修复ca14f0e已提交推送，
           Orin源码ff-only对齐，571源聚合ae5a04ff、生产4/测量3原生构建、mock AC-12与
           final9/Endpoint1/Lower2定向测试仅软件通过，证据已入H包；2.1秒延迟回执
           端到端及右H2硬件重测未做，正式安装未做，
           六服务停止，deployable=false、D15 OPEN。
           证据保留当前包，旧过程数据与重复文档按用户授权清除。
           Orin 调试 / RT 检查需要权限时直接 sudo，不放宽既定验收预算。
           当前续作（2026-09-29）：最终Disable共用入口及2.1秒真实tonic回环已软件验收。
           Orin最终fmt/根Clippy、lib4+工具182、回环6/9场景、66Python及绑定peer串行workspace通过。
           默认并行S1两例超时仍OPEN，旧FAIL不覆盖；已更新HANDOFF，阶段提交、不push。
           此次未核现场物理状态，右H2仍整体FAIL，D15 OPEN、deployable=false。
           同日后续：ca14冻结件双臂腕部各一次往返及断扭收尾PASS，头部/底盘未测主动运动。
           已加穷尽诊断分类，本机147+5、隔离Linux ARM根Clippy及167+42通过；该阶段Orin因SSH信任待核未测。
           原日志不回填Revoked，读早退/过期tick问题独立OPEN；详情见HANDOFF及hardware_smoke报告。
           9/29 Rerun软件接入已验收：独立robot-viz、十类pub/sub、有界隔离镜像及真实Viewer文件/实时画面。
           macOS与Linux专项17+6通过；Linux根fmt/Clippy、完整串行workspace、13退出、609 RT库测试及优化构建通过。
           源码/动态独审已完成，原失败及有损/多发布者公平性限制保留；证据见 .verify/robot_viz_2026-09-29/summary.json。
           该阶段未push、未部署Orin，SSH信任当时待核；上述Linux全仓结果来自隔离Lima。
           当前续作（2026-09-30）：严格SSH已恢复，Orin独立目录验证a6957cc的620项源码快照。
           已更新浮动nightly至2026-09-30/rustc1.101.0-nightly(5c543b0b8)，fmt/根Clippy、
           17单测、9镜像测试（另1子夹具ignored）、6显式SHM、独立优化构建/help通过。
           独立构建与首集成桥SHA不同，最终0097228b桥已补跑同六项通过；共32项不同测试，不计为38项。
           源码/产物前后身份及真实回收通过；正式ca14 clean、四件SHA不变、六服务停止且无控制进程。
           最终正常RRD官方同版verify通过：1000帧/10类/11实例，gap/drop/reject均0。
           Orin定向软件已验收归档，独审/root通过；本轮SIGINT夹具0帧仅证明生命周期。
           新证据入口 .verify/robot_viz_orin_2026-09-30/summary.json；本轮未重跑完整workspace。
           正式ca14安装未改、未做新硬件动作，右H2/D15与deployable=false不变。
           本阶段仅本地提交、不push；下一步真实发布者观测及硬件门仍需各自验证。
           同日H2续作：产物校验V2跟随最新nightly，前后真实编译器身份绑定六配方；新执行拒V1。
           59120e34原生六配方/七冻结件通过，浮动nightly实际更新为5c543b0b8；589源聚合48360e9f前后相同。
           fmt/根Clippy、71 Python、默认PLAN_ONLY、helper14真实机制及七help通过；非完整workspace。
           冻结测量件mock AC12四组各2软件样本、同源tonic6测试/9场景通过，硬件H2未测。
           release/post/archive独审与root验收通过，253原件及本机证据已归档至 .verify/h2_nightly_v2_2026-09-30/summary.json。
           04:29:11UTC正式后验ca14 clean、四SHA未变、六服务inactive且无控制进程；本阶段无安装/push。
           9/30用户确认无独立自动断电保护，决定暂缓D15实施/验收；状态OPEN/DEFERRED，不记PASS。
           D15工作单保留CodeMap与建议矩阵，非Spec Approved或硬件实现；供电分域未知，本轮不再追问/选型/采购/接线。
           受监督demo沿既有D15-R继续；下一步右H2完整复测，原FAIL与deployable=false保持。
           同日独立软件候选：周期Motion写仅Revoked后重检原安全门，由PeriodicRead独立申请读租约；
           外层deadline保原相位、跳过错失周期至严格未来点，零周期拒绝、溢出保原pending移交。
           两组Orin red原断言FAIL与首次联网setup SIGINT130保留；latest nightly5c543b0b8下定向软件门已通过。
           本机fmt、原生fmt/根Clippy通过；调度6、诊断7、rt_guard+commhook S0lib710、serial_rt_e2e5全过/0ignored。
           调度6与诊断7均为710子集，不累计；两端589输入逐项一致、验证后复核一致，聚合c0388807；三变更SHA批次首尾一致。
           证据 .verify/serial_periodic_recovery_2026-09-30/summary.json，已归档，独审与root验收通过；未部署/未push。
           该周期恢复候选阶段未跑完整workspace或release构建，不外推硬件通过。
           同日续作：b01603b已快进整合，Orin新V2六配方七冻结件通过，更新nightly仍5c543b0b8。
           589输入两端/构建前后逐项一致、聚合c0388807；本轮fmt/根Clippy、71Python、七help、mock4×2、tonic6/9通过。
           S0lib710与serial_rt_e2e5只引用周期恢复同源原件，不累计；release/post独审与root软件验收通过。
           证据 .verify/h2_serial_recovery_2026-09-30/summary.json；正式四件与服务/proc批次中后一致，未部署/push/硬件动作。
           本轮非完整workspace；阶段文档提交后remote快进对齐，再新alignment/当次现场事实/独立GO。
           右H2历史整体FAIL、D15 OPEN/DEFERRED及deployable=false保持；详HANDOFF。
           历史34条Unknown未证实归因、右H2整体FAIL、D15 OPEN/DEFERRED及deployable=false不变。
           历史规则：2026-09-23用户因额度要求暂停；2026-09-25仅授权将当前进度
           写入handoff并推送Git，之后到别处开发。本次交接经独审/root验收后commit/push，
           当时暂停开发及真机测试；真实H2仍需新物理重置/支撑/在场事实与独立GO。
           RX 重同步 / 串口 reopen 已移出本候选。
           后续仍需 ①通信故障硬化硬件门 → ②D15 退役决策 → ③F7 baseline 判据小件；
           Rerun软件按用户后续目标先行完成，Orin部署/硬件观测另验，通信硬化不自动关闭D15。
           详 docs/handoffs/v0.5_demo_run_2026-09-01_summary.md
           + docs/CHANGELOG.md 2026-09-01 两条。

已完结:    候选 E  替换执行面(受监督demo)  → v0.5_demo_run_2026-09-01_summary.md
           候选 A  operator CLI (robotctl)  → v0.5_operator_cli_summary.md
           候选 B  GetState/StreamState     → v0.5_get_stream_state_summary.md
           候选 C  SetEnabled RPC           → v0.5_set_enabled_summary.md
           候选 D  EStop 触发 RPC            → v0.5_estop_summary.md
           v0.5 基建  iceoryx2 0.9.3 + 全依赖升级（2026-07-12：解 toolchain pin +
                      撤 mem::forget workaround ×2）→ 见归档
           v0.4 收口  候选 1 / 2(r1+r2) / 3 / 4 → v0.4_*_summary.md
           v0.3 Phase A/B + v0.2 收尾        → v0.3_*_summary.md + 见归档

§6.7 真宣称: #1 ✅ S0 存活 = 软件层安全充分条件
            #2 ✅ systemd 看门狗（v0.3 A2 freeze hook + freeze_systemd_smoke 端到端真锁）
            #3 ✅ EStop 不可清为失忆态（v0.3 A1 ClearEStop control action 闭环 + A1-5 撤
                  --allow-empty-latch；唯一清 latch 路径 = ClearEStop control action）
            #4 ✅ S0 重启不失忆（v0.2 fsync 双写 + crash_restart_smoke 5/5 PASS）
            #5 ✅ 幂等写 RPC（IdempotencyCache + idempotency_key fingerprint + idem_replay e2e 真过）

上游项目:   dimos / dimos-deploy（只读移植参考；本地路径以实际 checkout 为准）
```

## 核心不变量

来自设计文档 §6.7，任何代码修改都不得破坏：

1. **S0 存活 = 软件层安全充分条件**；S0 不在时靠硬件 fail-safe（厂家电机 command timeout）。
2. **systemd 是最外层看门狗**；`sd_notify` 超时 → 重启。
3. **EStop 不可被软件清除为失忆态**；持久化到 `/var/lib/robot/<id>/estop_latch`，只能由 `ClearEStop` RPC (maintainer+) 解除。
4. **S0 重启不失忆**：EStop / fault code / 最后一次 safe 输出都 fsync 双写。
5. **幂等写 RPC**：同 `CommandMeta.idempotency_key` 不产生重复副作用。

## S0 线程模型（v0.2 骨架；v0.3 A1-3 加 rr_server_thread）

```
s0_ipc_thread        普通优先级    iceoryx2 订阅 command/heartbeat  → 写 latest-command slot
                                   (v0.3 A1-3) drain RR control channel → action executor
s0_rt_thread         SCHED_FIFO    读 command slot → HardRT adapter → 写 rt_snapshot
s0_pub_thread        普通优先级    读 rt_snapshot  → iceoryx2 publish state/error/heartbeat
s0_io_thread*        普通优先级    poll SoftRT adapter             → 写 softrt_snapshot
s0_rr_server_thread  普通优先级    (v0.3 A1-3) iceoryx2 RR server I/O → forward 到 ipc_thread
```

硬约束：**RT thread 不碰 iceoryx2、不 `Arc::clone/drop`、不 heap allocation**。违反 = 破坏实时性。

> **多 bus 场景语义**：v0.2 骨架 M=1，上述 `s0_rt_thread` 是单例。v0.4 起真多 bus 落地时（XLeRobot 2–3 × USB-RS485、ATOM01 4 × CAN），`s0_rt_thread` 按 **"每 HardRT bus 一实例"** 展开（方案 B：tick 闭环 drain-RX → read target → safety → serial write → per-bus snapshot slice）。详细语义 + 候选回写 §1.2 触发条件见 [`docs/CHANGELOG.md`](docs/CHANGELOG.md) 2026-04-24 "S0 `s0_rt_thread` 多 bus 语义澄清"。

## v0.2 实现顺序（已收尾，留作历史参考）

设计文档 §11.2。v0.2 Step 1–10 在 `0fa1d77` 全部 land，**最小闭环 + Orin
真机三轮闭环全通**。当时后续走 v0.3 Phase A → B 路线（详
[`docs/handoffs/v0.3_entry.md`](docs/handoffs/v0.3_entry.md)），不再按
Step 序号推进。

```
1.  robot-core::time                 ✅
2.  robot-msg schema registry        ✅
3.  FrameHeader / Body / Blob parser ✅
4.  SampleLease<T> / SampleWriter<T> ✅
5.  mock iceoryx2 pub/sub            ✅
6.  S0 四线程 mock                    ✅
7.  watchdog + CONTROL_LOST_* 切换    ✅（task 24-29）
8.  EStop latch 文件双写 + 重启恢复    ✅（fsync 双写真锁住 §6.7 #4）
9.  crash/restart 测试套               ✅（crash_restart_smoke 5/5 PASS）
10. Feetech 单关节                     ✅（Orin 三轮闭环全通：±5° / ±17° / 双端 ±17° 同步）
```

## 规划的 workspace 布局

```
robot-system/
├─ Cargo.toml                # workspace 根（无 [package]）
├─ rust-toolchain.toml       # nightly（docs/00_design.md §9）
├─ .cargo/config.toml        # sccache + target 目录
├─ configs/                  # 机器人配置 TOML
├─ 3rd-party/                # vendor flatc、可能的 iceoryx2 patch
├─ docs/
│   ├─ 00_design.md          # v0.1.2 冻结设计（权威）
│   └─ 10_verification.md    # 验证清单（v0.2 已落地，v0.3 持续维护）
├─ scripts/
│   ├─ deploy.sh
│   └─ deploy-windows.ps1
└─ crates/
    ├─ robot-core/           # trait、错误、time；零 runtime 依赖
    ├─ robot-msg/            # FlatBuffers schema + 生成代码
    ├─ robot-transport/      # iceoryx2 封装 + SampleLease<T> / SampleWriter<T>
    ├─ robot-kinematics/     # IK / FK / 雅可比 / 底盘运动学 trait + 纯类型（零 runtime 依赖）
    ├─ robot-kinematics-<robot>/  # 每种机器人一个实现：xlerobot / piper / ...
    │                        #   v0.4 起按需启用；用户可 fork 自己的 robot-kinematics-<mine>
    │                        #   而不动 s1 / s0 / adapter
    ├─ robot-s0/             # 四线程二进制
    ├─ robot-s1/             # coordinator 二进制（持 Option<Box<dyn ArmIkSolver>>，按 feature 选实现）
    ├─ robot-policy-host/    # ExternalPolicyHost 二进制（Python/ONNX/HTTP）
    ├─ robot-adapters/
    │   ├─ mock/
    │   ├─ feetech/
    │   ├─ piper/
    │   ├─ realsense/
    │   └─ webcam/
    ├─ robot-rpc/            # tonic gRPC 网关
    ├─ robot-s2-bridge/      # pyo3 module
    ├─ robot-viz/            # Rerun / Foxglove 桥
    ├─ robot-recorder/       # record/replay
    ├─ robot-cli/            # robotctl
    └─ robot-tests/          # soak + crash/restart + schema_compat
```

**IK 分层说明**：IK 住在 S1，不进 S0 / adapter。`robot-kinematics` 定义 `ArmIkSolver` / `BaseKinematics` trait；每个机器人一个 `robot-kinematics-<robot>` 实现，按 feature flag（`xlerobot` / `piper` / ...）挂到 `robot-s1`。替换机器人 = 换一个 feature，s1 / s0 / adapter 都不用改。实现时机见 `docs/01_implementation_steps.md`（v0.4 起），trait 草案 v0.3 前定。

## Rust 规范

- **Rust 默认使用最新 nightly，严格不用 stable**。`rust-toolchain.toml` 保持浮动 `channel = "nightly"`，使用前先更新最新工具链；对应 `docs/00_design.md` §9。
- **Edition 2024**。**不写 `rust-version`**；记录实际 `rustc -Vv` / `cargo -V` 用于溯源，不因记录或复现需求默认 pin 日期。
- **核心 crate 默认不用 nightly-only feature**：`robot-core` / `robot-msg` / `robot-transport` / `robot-s0` / `robot-s1` 原则上不写 `#![feature(...)]`；确需启用必须在 crate 顶部注释列出并过 review。目的是**降低 nightly 升级导致的回归风险**，不是留 stable 后门。
- 工具类 crate（`robot-cli` / `robot-viz` / `robot-rpc` / `robot-s2-bridge` / `robot-tests` / `robot-recorder`）可自由使用 nightly feature。
- 新 nightly lint 优先就地修复；仅真实编译回归阻断时临时回退已验证日期，并登记原因、影响范围与解除条件，修复后恢复浮动 nightly。CI 不跑 stable 回退演练。
- Allocator 按 §0 表：S0/S1 用系统默认 + RT 禁 alloc；外部工具（cli/viz/rpc/s2-bridge）用 mimalloc。
- `panic = "abort"` **仅限** `[profile.release.package.robot-s0]` / `[profile.release.package.robot-s1]`；其他 crate 保留 unwind。
- 控制流多分支用 `match`，避免 if-else 链（全局 §5）。
- **`cargo fmt` + `cargo clippy --all --all-targets -- -D warnings` 必须在每次 commit 前零警告通过**。
- CI 严格串行（全程 nightly）：`fmt → clippy → test → build → cargo deny → allocation-guard → schema_compat → soak (夜间)`。

### 业务代码禁用的 API

- `SystemTime::now()` / `std::time::Instant::now()` / `tokio::time::Instant::now()` → 走 `robot-core::time::RobotClock`。
- RT thread 内的 `Vec::push`、`Box::new`、`format!`、日志格式化、`Arc::clone/drop`、`FlatBufferBuilder` 构造、文件 / 网络 IO。
- `google.protobuf.Any` 作为 RPC payload → 用 `CommandMeta` + 强类型 Command。

## 构建假设

- **v0.1–v0.5 开发**：macOS 可跑前 5 步（纯类型 + 单元测试 + mock iceoryx2）。
- **第 6 步起需要 Linux**（`SCHED_FIFO` + `mlockall` + `sd_notify`）；Orin 或 Linux 开发机。
- **硬件接入（第 10 步）**：Orin + Feetech STS3215 + RealSense。

## 与上游 dimos 的关系

- 上游 dimos / dimos-deploy 是 Python 控制栈的只读移植参考；本地路径以实际 checkout 为准。
- `robot-system` 的目标是**完全替代 dimos 运行时**，保留 dimos 项目里已验证的 adapter 逻辑（Feetech 寄存器序列、运动学、校准）作为 `robot-adapters/feetech` 的 Rust 移植参考。
- v0.5 候选 E 已按原生替换控制执行面的路线跑通受监督 demo；旧栈自启已 disable、unit 保留回滚。
  不再执行早期“改造 dimos 为 RPC 客户端”的计划；正式退役待 D15 等生产安全硬化闭合。

## 提交规范（与全局 CLAUDE.md §4 一致）

- 分支：`master` 保护；`feat/*` `fix/*`。
- Conventional Commits + emoji 前置，不加 Co-Authored-By。
- Commit 前硬性：`cargo fmt --all && cargo clippy --all --all-targets -- -D warnings`。

## 现在不讨论的话题（架构已冻结）

`iceoryx2 vs eCAL` / `FlatBuffers vs Cap'n Proto` / `gRPC vs ZMQ` / `S0/S1/S2 重分层` / `Python 进 S1` / `SoftRT 挂 Pub` / 改 `layout_id` 宽度 / 改 `panic=abort` 范围。

要改走 `docs/CHANGELOG.md` + `docs/10_verification.md`，不改 `docs/00_design.md`。
