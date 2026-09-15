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
           已落地：新权限、高/普通双队列、唯一writer、RPC责任账本与受监督关闭链；
           W3/W4a/W4b及RR R0–R3已本地验收提交，RR底层两项遗留已修，
           仍待双主机同源验证。完整提交、门及旧失败见HANDOFF/H包。
           当前：R4a与R4b1–b4已本地验收，R4b4三代启动恢复链两profile通过；
           R4c正式round3完整51门通过，独立物证及root验收完成；归档提交后继续R4d-A原连接压力。
           旧默认并行锁例仍有偶发失败，本轮串行工作区通过不代表根因闭合。
           后续：R4d-A原连接压力 → R4d-B双主机同源验证 → P-E长负载。
           用户要求继续有效开发至周额度剩20%；root实时查额度，接近阈值不派新包，
           达到阈值保存交接停止；每阶段通过即commit，不push。
           左H1/H3已通过，H2两次FAIL，左断电；S-46 Orin wire4仍FAIL。
           右臂未测；六服务最后已停止，deployable=false，D15 OPEN。
           证据保留当前包，旧过程数据与重复文档按用户授权清除。
           Orin 调试 / RT 检查需要权限时直接 sudo，不放宽既定验收预算。
           每阶段评审与必要质量门通过即commit；继续至周额度剩20%，不push。
           RX 重同步 / 串口 reopen 已移出本候选。
           后续硬序 ①通信故障硬化 → ②D15 退役决策 → ③robot-viz Rerun 桥
           → ④F7 baseline 判据小件；通信硬化不自动关闭 D15。
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
