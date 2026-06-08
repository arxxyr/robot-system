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

```
设计版本:  v0.1.2（冻结）
实现阶段:  v0.5 —— 候选 B (GetState/StreamState 只读 RPC) ✅ **完结**（RIPER：CodeMap
           + Spec r1-3 + Plan r1-2 + 4 段 feat；method 6→8 additive，`a010fe7` →
           `c3a2804`）。proto append 2 rpc + 3 message + 4 enum mirror（state_convert.rs
           fbs→proto 纯转换器）+ gateway 常驻 per-hardware S0 state 订阅池 + cache
           （state_pool.rs；**唯一**底层 subscriber，GetState/StreamState 只读 cache、
           **不**新建 per-RPC subscriber）+ `--config` 解析 [[hardware]].id（D10，0 dep
           robot-s0）+ StreamState 节流（D5a：0→50ms / <20→20ms floor）+ state_rpc_e2e
           6 test（gateway-only，不起 s1）。**0 改 B4 wire 6 method / 0 触 §6.7（只读）/
           0 改 robotctl / 0 改 design.md / 0 改 S0·S1 daemon**。详
           docs/handoffs/v0.5_get_stream_state_summary.md。
           **design-freeze diff + 4 PE 必读**：D3（`GetState(Empty)`→`GetStateRequest
           {hardware_id}` 走 CHANGELOG）/ PE1（fbs enum newtype struct → variant-count
           lock 防漂移）/ PE2（proto-additive 强制 gateway graceful unimplemented 占位
           → 真 handler 2/4+3/4）/ PE3（⚠ state_pool `mem::forget(sub)` 有界泄漏 =
           iceoryx2 0.8.1 Drop SIGSEGV workaround，**iceoryx2 升级后复核撤销**）。
           **C1 延续非安全保证**：read RPC 无 auth 字段、服务端不读不校验 token、
           read_state cap 不实现。cargo test --workspace 1117 PASS / 0 FAIL
           （state_rpc_e2e 6 + task_rpc_e2e 6）。下一步 = 候选 C，A→B→C→D→E 硬序
           （v0.5_entry.md）。

v0.5 候选 A: operator CLI (`robotctl`) ✅ 完结（5 feat + 1 fix + 7 docs = 13 commit，
           `809575f` → `25c2feb`）；crates/robot-cli +7 tonic gRPC operator 子命令 +
           robotctl_smoke 12/12。三条非安全保证：A4（robot-cli dep robot-rpc/client →
           task_rpc_e2e 默认随 workspace 跑）/ C1（read RPC token gate 仅 client-side）/
           D6（--dry-run 仅 clear-estop 端到端）。详
           docs/handoffs/v0.5_operator_cli_summary.md。

v0.4 收口:  候选 3 (per-ActionId pubsub cleanup) ✅ + 候选 1 (Scripted backend) ✅
           + 候选 2 round 1 (robot-kinematics trait crate) ✅
           + 候选 2 round 2 (robot-kinematics-xlerobot impl + IkBackend + S1 wiring + e2e) ✅
           + 候选 4 (dimos 切换 light Research) ✅（commit e6178f2）。

v0.2 收尾:  commit 0fa1d77（2026-04-26 reviewer "复核通过，按降级宣称进入 v0.3"）。
            §6.7 #4 真锁住继承自 v0.2；#2 (A2 land) + #3 (A1-5 land) 在 v0.3 Phase A 升级到真锁住。
            详 docs/handoffs/v0.3_a1_4_clear_estop_smoke.md §5 + docs/CHANGELOG.md A1-5 升级登记。

Phase A 收尾: A1 ClearEStop control plane（A1-1 → A1-5）+ A2 worker freeze hook 全 ✅；
            §6.7 #2 + #3 升级到真锁住。详 docs/handoffs/v0.3_a1_4_clear_estop_smoke.md
            + 2026-04-26_6.md。

§6.7 真宣称: #1 ✅ S0 存活 = 软件层安全充分条件
            #2 ✅ systemd 看门狗（v0.3 A2 freeze hook + freeze_systemd_smoke 端到端真锁）
            #3 ✅ EStop 不可清为失忆态（v0.3 A1 ClearEStop control action 闭环 + A1-5 撤
                  --allow-empty-latch；唯一清 latch 路径 = ClearEStop control action）
            #4 ✅ S0 重启不失忆（v0.2 fsync 双写 + crash_restart_smoke 5/5 PASS）
            #5 ✅ 幂等写 RPC（IdempotencyCache + idempotency_key fingerprint + idem_replay e2e 真过）

Phase B 进度（已完结，commit `cac4515` Phase B 收尾）:
            B1 ✅（s1_daemon hold + heartbeat 闭环 + 启动锚定 fail-fast；commit `f9b0764`）。
            B2 ✅（policy/action interface + S1 coordinator state machine + binary wiring；
                commit chain `4b9d4de` → `4b17e29`；详 docs/handoffs/v0.3_b2_summary.md）。
            B3 ✅（第一个真 action backend = HoldPoseBackend；source_task_id == TickState.cmd_seq
                单调契约锁；commit chain `916202e` → `d1f5031`；详 docs/handoffs/v0.3_b3_summary.md）。
            B4 ✅（robot-rpc full gateway 5 task method + s1 4 RR server + per-ActionId events
                drain pubsub + gateway 5 rr_io_thread + sync↔async 桥 + 6 cross-process e2e；
                详 docs/handoffs/v0.3_b4_summary.md §3 7 条契约 + §4 e2e harness 工程经验）。

v0.4 进度:
            候选 3 ✅（per-ActionId pubsub cleanup + AC-C1 reap_finished；11 commit + summary）。
                EventsDrainRegistry::reap_finished(&self) -> usize 锁内 swap_remove + 锁外 join
                + panic 不传播但 warn + 1s reaper thread + shutdown step 4 reaper.stop+join；
                events_registry_len final == 0 + triggered_c1 == false + smoke --phase all
                exit 0；commit chain `cf8ddd8` → `2518fc0`；详
                docs/handoffs/v0.4_pubsub_cleanup_summary.md。
            候选 1 ✅（Scripted backend = joint-space 线性插值 + waypoint dwell；第二个真
                action backend；6 docs/spec/plan 前置 + 3 实施 commit = 9 commit，加 summary
                = 10 commit）。
                schema 注册链 6 步 atomic + ActionKindProto/Fb/ActionKind 三层 append +
                ScriptedBackend lib + gateway 两道 byte gate (MAX_SCRIPTED_PAYLOAD_BYTES =
                4 KiB + CONTROL_TIER_MAX_BYTES = SizeTier::Control.max_bytes() 单源绑定) +
                scripted_backend_smoke cross-process e2e AC-4/5/6/7 全过；commit chain
                `d8ba207` → `70dcb25`（origin/master 已 push）；详
                docs/handoffs/v0.4_scripted_backend_summary.md。
                **关键契约**：source_task_id == ctx.command_seq 跨 segment 严格单调（沿 B3
                §3.7）+ Dwell start_ns 锚定 Interp 自然完成时刻（覆盖大步推 clock 收敛）+
                cancel 两层分离（backend lib PartialSuccess ↔ coordinator ActionEvent::
                Cancelled 终态 only）。
            候选 2 round 1 ✅（robot-kinematics trait crate first release；3 docs/spec/plan
                前置 + 1 实施 commit = 4 commit，加 summary = 5 commit）。
                新 workspace member crates/robot-kinematics（Cargo.toml workspace 继承 +
                [lints] workspace = true + [dependencies] thiserror 唯一 runtime dep；
                cargo tree --edges normal,no-proc-macro = {thiserror v2.0.18}）+ src/lib.rs
                313 L 单文件（5 共享类型 Pose / Twist / Jacobian / KinematicInfo /
                IkSolution + JointLimit POD 嵌套于 KinematicInfo.joint_limits；自带 POD
                零 nalgebra）+ IkError 5 variant
                (InvalidInput / NoSolution / OutOfReach / SingularityTooClose /
                DeadlineExceeded；thiserror::Error derive；exhaustive match 编译期锁)+
                ArmIkSolver: Send + Sync trait (info() + solve(target: &Pose, seed:
                Option<&[f64]>, deadline_ns: Option<u64>) -> Result<IkSolution, IkError>;
                dyn-safe + assert_dyn_safe::<dyn ArmIkSolver>() 编译期 monomorphize 锁)+
                4 lib unit + AC-6 "trait 层不预钉 XLeRobot clamp 行为" 四处双写防漂移
                (Cargo.toml description + lib.rs 模块 doc + trait doc + Spec AC-6)+ 零
                robot-s1/rpc/msg/s0/adapters/tests 改动 + 零 wire schema 改动；commit chain
                `557e354` → `64cf312` → `98b6033` → `6caf0ee`（origin/master 已 push）+
                summary 本日 land；详 docs/handoffs/v0.4_robot_kinematics_round1_summary.md。
                **关键契约**：trait dyn-safe 编译期锁 + IkError exhaustive match 锁 + 不预钉
                XLeRobot clamp 行为（四处双写）+ deadline_ns: Option<u64> 仓库统一时间模型对齐
                （robot-core::time::MonoRawNs(u64) + Deadline + ActionRequest.deadline_ns;
                trait crate 不 dep robot-core）+ 零 runtime 依赖（trait crate 永远不扩
                [dependencies]）+ 零 robot-s1 改动。
                **不在范围**（推到 round 2）：BaseKinematics trait（Q5(b) ACK 推迟）+
                robot-kinematics-xlerobot impl + IkBackend 第三个真 action backend + S1
                coordinator 持 Box<dyn ArmIkSolver> + IkPayload fbs body (SCHEMA_IK_PAYLOAD
                = 0x040C) + ActionKindProto::Ik / ActionKindFb::Ik / ActionKind::Ik 三层
                enum append + ik_backend_smoke cross-process e2e。
                **cargo deny check 状态**：未新增 violation；overall licenses FAILED 是既有
                baseline（assert_no_alloc BSD-1-Clause + serialport MPL-2.0 + unescaper
                标识符 warning；全部跟 robot-kinematics 无关；baseline 已在 HEAD = 98b6033
                存在）；follow-up 独立 commit / PR 处理，不进 round 1。
            候选 2 round 2 ✅（robot-kinematics-xlerobot impl + IkBackend 第三个真 action
                backend + S1 wiring + ik_backend_smoke cross-process e2e；3 docs/spec/plan
                前置 + 4 实施 commit = 7 commit，加 summary = 8 commit）。
                新 workspace member crates/robot-kinematics-xlerobot（Cargo.toml workspace
                继承 + 唯一 runtime dep robot-kinematics trait crate；零 nalgebra；
                cargo tree --edges normal,no-proc-macro = {robot-kinematics → thiserror}）+
                src/lib.rs 763 L 单文件（XLeRobotArmIkSolver 5-DOF 闭式 IK 沿 dimos
                arm_ik.py path A 闭式硬编码 + Side { Right, Left } sign 控制 target_y 符号
                + Q-IK6 (a) yaw != 0 reject + Q-IK16 (a) clamp 路径走 Ok(clamped) + 9 lib
                unit）+ IK schema 注册链 6 步 atomic (SCHEMA_IK_PAYLOAD = 0x0000_040C +
                IkPayload fbs body + BodyMarker + SizeTier::Control + build_helpers) +
                三层 ActionKind::Ik enum append (proto ACTION_KIND_IK = 4 + fbs Ik = 4 +
                lib Ik) + 三个 variant count 锁测同步 4 → 5 + IkBackend (8 步 semantic
                check 顺序短路 + IkInstance Q-IK17 (a) single-shot tick 收敛 + cancel
                PartialSuccess { warning: "cancelled_{before,after}_emit" } +
                map_ik_error_to_start_error 5 variant exhaustive + inventory::submit! +
                17 lib unit 含 bit-equivalent + Q-IK10 三层 fail-fast: UnknownBackend /
                MissingSolver / NoBackendForKind) + ActionContext::solver: Option<&dyn
                ArmIkSolver> 引用 plumb (Q-IK3 (a) 不 Arc / 不 clone) + Coordinator::
                solver: Option<Box<dyn ArmIkSolver>> 持有 + validate_solver_for_profile
                + ProfileError::MissingSolver 新 variant (Q-IK10b (i) startup 校验) +
                s1_daemon feature-gated XLeRobotArmIkSolver::new(Side::Right) 注入 +
                ik_backend_smoke 5-phase cross-process e2e (selftest / happy / monotonic
                / unreachable_clamp / invalid_input；all 5/5 PASS)；commit chain
                `a42ef73` → `7fd0756` → `266191f` → `19a3f43` → `ce2f498` → `13ecde8`
                → `c0ef8e1`（origin/master 已 push）；详
                docs/handoffs/v0.4_robot_kinematics_round2_summary.md。
                **关键契约**：round 1 trait crate 0 改动（Spec §"严禁改动" #1 hard
                boundary）+ source_task_id == ctx.command_seq bit-equivalent + cross-
                process monotonic 实测（27+ entries 严格单调）+ Q-IK10 三层 fail-fast
                + Q-IK16 (a) clamp 路径走 Ok(clamped) e2e 锁住 + deadline 单一权威 =
                backend pre/post guard（solver 收 None 永不返 DeadlineExceeded）+ 3 类
                ActionStartError 映射严格分离（PayloadDecode / HardwareMismatch /
                Rejected 全部 → tonic FailedPrecondition）+ frozen schema 不动（B4 wire
                契约 + IkPayload byte cap 沿 Control tier 兜底）。
                **Quality Gate 实测**：989 PASS / 0 FAIL workspace tests（+30 new）+
                双路 crash_restart_smoke 5/5+5/5 + scripted_backend_smoke 5/5 PASS +
                pubsub_cleanup_smoke all PASS + triggered_c3=false + ik_backend_smoke
                --phase all **5/5 PASS**（本 round 2 硬 gate）。
                **cargo deny check 状态**：未新增 violation；overall licenses FAILED 是
                既有 baseline（assert_no_alloc BSD-1-Clause + serialport MPL-2.0 两条
                error[rejected] + unescaper GPL-3.0/MIT 一条 warning[parse-error]；3 处
                全部跟 round 2 本轮改动无关；沿 round 1 + 候选 1 同款 follow-up 独立
                处理路径）。
            候选 4 ⬜ 准备开（dimos 切换 light Research）。
                **不**捆绑 round 2 trait crate / impl crate 改动；**不**改候选 1/3 既
                有 smoke binary 行为；触发：本 round 2 summary push + reviewer 候选 4
                entry ACK；详 round 2 summary "开工前必读"接班 checklist。

上游项目:   /Users/loosqk/repo/robot/dimos（XLeRobot 集成，v0.4 后切换到本仓库）
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
真机三轮闭环全通**。新工作走 v0.3 Phase A → B 路线（详
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

- **Rust nightly 是硬要求**，**严格不用 stable**（`rust-toolchain.toml` pin `channel = "nightly"` 或 `nightly-YYYY-MM-DD`）。对应 `docs/00_design.md` §9。
- **Edition 2024**。**不写 `rust-version`**：自动跟随最新 nightly；需要复现构建时用 `rust-toolchain.toml` pin `nightly-YYYY-MM-DD`。
- **核心 crate 默认不用 nightly-only feature**：`robot-core` / `robot-msg` / `robot-transport` / `robot-s0` / `robot-s1` 原则上不写 `#![feature(...)]`；确需启用必须在 crate 顶部注释列出并过 review。目的是**降低 nightly 升级导致的回归风险**，不是留 stable 后门。
- 工具类 crate（`robot-cli` / `robot-viz` / `robot-rpc` / `robot-s2-bridge` / `robot-tests` / `robot-recorder`）可自由使用 nightly feature。
- nightly 回归风险靠**pin 具体日期 + 原地回滚到上一已验证日期**解决；CI 不跑 stable 回退演练。
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

- 当前 XLeRobot 控制栈跑在 `/Users/loosqk/repo/robot/dimos`（Python + dimos 框架）。
- `robot-system` 的目标是**完全替代 dimos 运行时**，保留 dimos 项目里已验证的 adapter 逻辑（Feetech 寄存器序列、运动学、校准）作为 `robot-adapters/feetech` 的 Rust 移植参考。
- `robot-system` v0.4 完成后，dimos 项目会切换到调用 robot-system 的二进制 + RPC / iceoryx2 客户端。

## 提交规范（与全局 CLAUDE.md §4 一致）

- 分支：`master` 保护；`feat/*` `fix/*`。
- Conventional Commits + emoji 前置，不加 Co-Authored-By。
- Commit 前硬性：`cargo fmt --all && cargo clippy --all --all-targets -- -D warnings`。

## 现在不讨论的话题（架构已冻结）

`iceoryx2 vs eCAL` / `FlatBuffers vs Cap'n Proto` / `gRPC vs ZMQ` / `S0/S1/S2 重分层` / `Python 进 S1` / `SoftRT 挂 Pub` / 改 `layout_id` 宽度 / 改 `panic=abort` 范围。

要改走 `docs/CHANGELOG.md` + `docs/10_verification.md`，不改 `docs/00_design.md`。
