# Spec · v0.4 候选 3 · per-ActionId pubsub cleanup grace 实测

> RIPER **Spec 阶段**产物。沿 [`b4_3_3_cross_process_e2e.md`](b4_3_3_cross_process_e2e.md)
> 同款模板。只定义 What / AC / 非范围 / 风险 / 回滚策略 / 需要 reviewer
> 拍板的问题；**不**写原子实现 Plan。Plan 阶段单独走，待本 Spec
> reviewer round N 拍板后再开。
>
> 范围限定（reviewer 拍板 · `docs/handoffs/v0.4_entry.md` §4.1 + §3.4
> + §3.5）：
> - 默认目标 = **验证 + 最小加固**，不是重设计。
> - 基础 AC 限定在"只验证"层；条件性 AC 仅在实测确证泄漏 / race 才允许
>   落最小代码改动。
> - 严禁从 cleanup smoke 漂到重设计 events_drain / events_subscribe /
>   iceoryx2 wrapper / shutdown 顺序契约 / RPC 语义。
>
> 写于 2026-05-11，v0.4 entry handoff commit `8be13bf` + Pre-Research
> CodeMap commit `cf8ddd8` 之后。

---

## What

为 v0.4 候选 3 写 **B4 per-ActionId pubsub cleanup 真行为实测套件**，补
[`docs/handoffs/v0.3_b4_outline.md`](../../docs/handoffs/v0.3_b4_outline.md)
§5.6 #4b 三条留白（terminal_grace 100 ms 是否合适 / gateway sub 端
service 消失检测语义 / 进程崩溃 leftover topic 清理策略）+
[`docs/handoffs/v0.3_b4_summary.md`](../../docs/handoffs/v0.3_b4_summary.md)
§1.2 不在范围 #5（per-ActionId pubsub topic 动态 cleanup grace 实测）。

具体验证对象（详 `team-swe/codemap/v0_4_pubsub_cleanup.md`）：

1. **events_drain publisher 端 lifecycle**（s1 process）：终态后
   thread / publisher / `Box::leak(node)` 收敛行为；
   `EventsDrainRegistry::len()` 长跑增长曲线。
2. **events_subscribe consumer 端 lifecycle**（gateway process）：
   subscribe thread + `EventsSubscribeStream::drop` 后 thread + Node
   收敛行为；B4 (2/3-c) reviewer 备忘的 leak 检测留白点。
3. **iceoryx2 service handle GC**：service 在 publisher + subscriber +
   node 全 drop 后是否真 GC（iceoryx2 0.8.1 文档 vs 真行为）。
4. **进程崩溃 leftover 行为**：daemon SIGKILL 后 iceoryx2 storage 残留
   语义 + 重启路径影响。

输出形态待 reviewer 拍板（详 §"需要 reviewer 拍板的问题" Q7）；候选
路径见下表。

---

## 影响范围

| 模块 | 改动类型 |
|---|---|
| `crates/robot-tests/src/bin/pubsub_cleanup_smoke.rs` 或 `crates/robot-rpc/tests/pubsub_cleanup_e2e.rs`（**新**；具体路径见 §Q7） | 长跑 smoke + leak detector + grace 实测 + 崩溃 leftover 实测主体（基础 AC） |
| `crates/robot-rpc/tests/common/*`（潜在复用 / 微扩） | 仅当走 integration test 路径时复用 B4 (3/3) 的 case_ctx / subprocess / ready / fake_state；新增 helper 限定在 leak 检测 + 多 task 长跑节奏 |
| `crates/robot-s1/src/events_drain/runtime.rs` | **条件性**：仅当 AC-C1 触发 → 加 `EventsDrainRegistry::unregister(action_id) -> Option<Handle>` + 调用点；shutdown 顺序契约 / 三路退出语义 / Box::leak 模式不动 |
| `crates/robot-s1/src/rr_server/handler.rs` | **条件性**：仅当 AC-C1 触发 → `run_drain_loop` 退出后调 `events_registry.unregister(action_id)` 兜底；invoke handler 成功路径的 register 调用点不变 |
| `crates/robot-rpc/src/events_subscribe.rs` | **条件性**：仅当 AC-C2 触发 → 加 thread-count diff / 资源 leak 检测点；`EventsSubscribeStream::drop` 不主动 join thread 的语义不变 |
| `scripts/systemd/*` / `scripts/iceoryx2_sweep.{sh,ps1}`（新）/ `scripts/systemd/README.md` | **条件性**：仅当 AC-C3 触发 + reviewer Q6 拍板对应策略 → systemd `ExecStartPre=` hook / 独立 sweep 脚本 / 都不做 + 文档约束（三选一 land 一种）；**不**改 s0_daemon / s1_daemon 启动期路径；**不**调 `robot_transport::iceoryx2/*` wrapper API |
| `docs/handoffs/v0.3_b4_outline.md` §5.6 #4b | **基础 AC PASS 后**：原 outline 不可改（frozen）；但实测结论 + default 微调登记到 `docs/CHANGELOG.md`（v0.4 候选 3 实测条目），跟 v0.3 §6.7 升级登记同款机制 |
| `docs/handoffs/v0.4_pubsub_cleanup_summary.md`（**新**） | 候选 3 land 后封存（沿 v0.3_b3_summary / v0.3_b4_summary 模式） |
| `team-swe/codemap/v0_4_pubsub_cleanup.md` | **不动**；Spec 阶段不回写 codemap（Spec 是 codemap 下游） |

**严禁改动**（reviewer 拍板 6 条 hard boundaries · v0.4_entry §5）：
- `crates/robot-rpc/proto/robot_control.proto`
- `crates/robot-msg/schemas/{action_request,action_status,action_event,*}.fbs`
- `crates/robot-transport/src/iceoryx2/{mod,publisher,subscriber,*}.rs`（含
  Node ownership 模型 / `Box::leak` 模式 / `open_or_create_with_attributes` 语义）
- `crates/robot-s1/src/coordinator.rs` 任何 state machine / cmd_seq /
  source_task_id 路径
- `crates/robot-s1/src/bin/s1_daemon.rs` shutdown 顺序契约
  （`mark_shutting_down → rr.request_stop_all → rr.join_all →
  events_registry.stop_and_join_all`，reviewer round 6 阻断锁定）
- `crates/robot-rpc/src/{gateway,task_handlers,idempotency,auth,
  rr_io_thread}.rs` 任何 12 步骨架路径 / namespace key / fingerprint
  排除字段 / `ActionRpcStatus → tonic::Code` 显式 match 表
- `docs/00_design.md` 任何正文（任何偏离走 `docs/CHANGELOG.md` 登记）

---

## 风险等级

**低**。本 Spec 默认目标是验证 + 实测沉淀；条件性 AC 仅当实测确证才允许
落最小代码改动，且范围限定在 `EventsDrainRegistry::unregister` /
events_subscribe leak 计数 / leftover 策略一种。

主要风险点：

1. **测试基础设施 churn**：长跑 1000+ task / 进程崩溃 leftover 测试
   时序敏感；可能撞 iceoryx2 0.8.1 内部 race（B4 (3/3) 曾遇 multi-sub
   race，详 `docs/handoffs/2026-04-26.md`）。
2. **条件性 AC 越界**：实测确证泄漏后落 `unregister` 修复时，可能误改
   shutdown 顺序契约 / Box::leak 模式（Spec 用 §"严禁改动"列表硬约束 +
   §AC-C1 触发条件锁住"必须保 shutdown 顺序契约不变"）。
3. **leftover 策略三选一**：reviewer Q6 拍板时如选 systemd hook，会引
   入 `scripts/systemd/` 改动 + 真机回归依赖；如选 daemon startup sweep
   会改动 s0_daemon / s1_daemon 启动路径；如选"都不做"则候选 3 范围内
   leftover 风险仅记录到 summary，留 v0.4 后期 / v0.5 评估。
4. **长跑测试 CI 时长**：1000 task ~×几百 ms ≈ 几分钟；24h soak 不能进
   CI；reviewer Q1 拍板量级时需平衡。
5. **flaky test**：iceoryx2 service collision / shm 配额 / 主机
   `/dev/shm` 空间 / 各 dev box 行为差异。

---

## 回滚策略

候选 3 默认是 **test-only 加项**（基础 AC 全条件性 AC 都未触发的 happy
path），回滚 = revert 实测 commit；不影响生产代码。

如触发条件性 AC 落代码改动：

- **AC-C1 `EventsDrainRegistry::unregister` land**：revert
  `events_drain/runtime.rs` + `rr_server/handler.rs` 改动；不影响
  `EventsDrainRegistry::register` / `stop_and_join_all` / shutdown 顺序
  契约（这些都未改）。
- **AC-C2 events_subscribe leak 计数 land**：revert `events_subscribe.rs`
  改动；不影响 `EventsSubscribeStream::drop` 现有语义。
- **AC-C3 leftover 策略 land**：revert systemd unit 改动 / daemon 启动
  期 sweep 改动；回到"用 robot_id 后缀避免 collision"现状（v0.3 已有）。

如发现某测试 flaky → 标 `#[ignore]` + 留 follow-up issue，**不**回滚整
个 commit（沿 B4 (3/3) 同款 reviewer 备忘）。

---

## 目标分支 / 交付形式

- 分支：`master`（直接 commit，沿 B4 (1/3-a/b/c) → (2/3-a/b/c) → (3/3)
  同款节奏；具体段数待 Plan 阶段拍板，**Spec 阶段不预钉死**）
- 交付：
  - 基础 AC PASS 路径（候选 3 happy path）：1-2 个 `✨ feat:` commit
    （测试套）+ 1 个 `📝 docs:` commit（summary 封存）
  - 触发条件性 AC 路径：每个触发的 AC 各 1 个 `✨ feat:` commit
    （独立验证 + 独立 revert），最多 3 个；reviewer round fix commit
    按需追加
- §6.7 不变量影响：5 条全保（**0** 条降级 / **0** 条升级）；任何意外影
  响走 `docs/CHANGELOG.md` 偏离登记

---

## Acceptance Criteria

> AC 分两类：**基础 AC（A-x，无条件必做）**+ **条件性 AC（C-x，仅当对
> 应基础 AC 实测确证某问题才允许 land 最小修复）**。reviewer 拍板 AC 之
> 前不进 Plan。

### 基础 AC（A 部分 · 无条件必做）

#### AC-A1: per-ActionId pubsub 长跑 + leak detector

- **前置条件**：spawn s1_daemon + robot_rpc_gateway 双进程（沿 B4 (3/3)
  cross-process harness 模式，详 `tests/common/`）。
- **操作**：N task（N 默认 1000；reviewer Q1 拍板）顺序 / 半并发执行
  `InvokeTask kind=DwellMs duration_ms=10..50` + `StreamTaskEvents`
  全程订阅至终态；间隔由 reviewer Q1 / Q9 拍板（避免不真撞 grace 路径）。
- **测量**（每 100 task 一档 + final）：
  - s1 / gateway 各自 `VmRSS`（`/proc/<pid>/status::VmRSS`）增长曲线
  - `/dev/shm/iceoryx2_*` 文件数（如 host 不放 iceoryx2 storage 在
    `/dev/shm` → 用 iceoryx2 配置的真 storage path；Plan 阶段确认）
  - `EventsDrainRegistry::len()`（通过 s1_daemon test-only metric
    endpoint / log scrape / inline accessor；Q8 拍板）
  - subscribe thread 数（`/proc/<gateway_pid>/task` count diff）
- **预期结果**：
  - `VmRSS` 增长曲线：N=1000 后 △RSS / task 收敛到稳态（绝对上限由
    Q2 拍板）
  - `EventsDrainRegistry::len()`：N task 全 land 后保持稳态（≤ N
    if 不 unregister；具体上限由 Q2 拍板）
  - iceoryx2 storage 文件数：N task 后增长 ≤ Q2 拍板上限（**关键**：
    候选 3 验证 "iceoryx2 service 是否真 GC"）
  - 子进程 stderr / stdout 无 panic / `BUG:` / `[ERROR]` 关键字
- **验证方式**：candidate test path 待 Q7 拍板。

#### AC-A2: terminal_grace 真行为实测

- **前置条件**：双进程 ready；client 调用 `StreamTaskEvents` 等待终态。
- **操作**：N task（N 默认 100；reviewer Q3 拍板）每条 InvokeTask
  DwellMs(short) → 全程订阅终态帧；记录 **s1 端 events_drain 收到
  terminal event 时刻** vs **gateway 端 events_subscribe forward 该
  terminal 帧到 mpsc 时刻** 的延迟 + 是否在 `terminal_grace=100ms`
  窗口内拿到。
- **测量**：
  - gateway 端拿到终态帧的成功率（应 = 100%；丢一帧 = 失败）
  - 延迟 p50 / p95 / p99（建议 ≤ 50ms / ≤ 100ms / ≤ 100ms；具体由 Q3
    拍板）
- **预期结果**：
  - 成功率 = 100%（与现有 B4 (3/3) AC-1 / AC-2 一致；如 < 100% =
    grace 默认值不够，触发 Q3 调整）
  - p99 ≤ terminal_grace 当前默认值（100ms）；超过 = 触发 Q3 调整 +
    `docs/CHANGELOG.md` 登记
- **验证方式**：同 AC-A1 路径。

#### AC-A3: 进程崩溃 leftover 现状实测

- **前置条件**：双进程 ready；调若干 InvokeTask 让 events_drain thread
  + iceoryx2 service handle 实际存在。
- **操作**：
  - **A3a**：SIGKILL s1_daemon → 重启 daemon → 验启动是否被 leftover
    阻塞（service create 错 / NodeBuilder 错 / 新 InvokeTask 失败）
  - **A3b**：SIGKILL gateway → 重启 gateway → 验启动 + 已建 stream
    后续是否被 leftover 阻塞
  - **A3c**：同时 SIGKILL 双进程 → 重启 → 验同上
- **测量**：
  - daemon / gateway 重启是否成功（exit code 0 + ready signal）
  - iceoryx2 storage path 下 stale 文件数 + 类型
  - 重启后 N task 新调 InvokeTask 成功率（应 = 100%）
- **预期结果（不预设；本 AC 是实测）**：
  - 如 leftover **不**阻塞重启 + 新 task → 候选 3 完成基础 AC；
    leftover 策略 = Q6 选 "(c) 都不做"；登记 summary 后结案
  - 如 leftover **阻塞**重启 / 新 task → 触发条件性 AC-C3 + Q6
    拍板 systemd hook / daemon sweep 二选一
- **验证方式**：同 AC-A1 路径。

### 条件性 AC（C 部分 · 仅当对应基础 AC 实测确证才允许 land）

#### AC-C1: `EventsDrainRegistry::unregister` 落地

- **触发条件**：AC-A1 实测 `EventsDrainRegistry::len()` 在 N task 长跑
  后**线性增长**（不收敛）+ reviewer 确认这是 leak 而非测试 setup
  artifact。
- **允许的最小代码改动**：
  - `crates/robot-s1/src/events_drain/runtime.rs`：加
    `EventsDrainRegistry::unregister(action_id: ActionId) -> Option<EventsDrainThreadHandle>`
    （内部 `Mutex<Vec<Handle>>` 线性 swap_remove；接 `&self`，沿
    `request_stop_all` 同款签名）
  - `crates/robot-s1/src/rr_server/handler.rs` 或
    `events_drain/runtime.rs::run_drain_loop`：在线程退出路径
    （terminal+grace / disconnect / stop signal 三路）调
    `events_registry.unregister(action_id)`（如调用点在 thread 闭包内
    →需要 weak ref 传 registry；Plan 阶段评估两种调用点 trade-off）
  - lib unit：扩 `runtime.rs::tests` 锁 unregister 行为 + 长跑 N=100
    后 `len() == 0`
- **必须保的契约**（reviewer 硬约束）：
  - shutdown 顺序契约不变：`mark_shutting_down →
    rr.request_stop_all → rr.join_all → events_registry.stop_and_join_all`
    依然有效；`stop_and_join_all` 现状 = `mem::take` 拿走整个 Vec
    后逐个 join，对 unregister 后剩余 0 个 handle 是 no-op（已有
    `registry_stop_and_join_all_handles_empty` lib unit 锁住）
  - `Box::leak(node)` 模式**不**动（reviewer 硬约束 5）
  - `register` API 签名 / 调用点不动
- **不在范围**：weak ref / Arc cycle 问题如发现是 lib API 本质缺陷
  → 走另立 outline，**不**在本 AC 修
- **预期结果**：`EventsDrainRegistry::len()` 在 N task 长跑后收敛到
  稳态（最多 = 当前 in-flight task 数）

#### AC-C2: events_subscribe leak 计数

- **触发条件**：AC-A1 实测 gateway 端 `subscribe_thread_count` 在 N
  StreamTaskEvents RPC drop 后**线性增长** + reviewer 确认是 leak
  （而非 `EventsSubscribeStream::drop` 后 thread 正常退出但 OS reap
  延迟）。
- **允许的最小代码改动**：
  - `crates/robot-rpc/src/events_subscribe.rs`：加 thread-count diff
    / 资源 leak 检测计数（B4 (2/3-c) reviewer 备忘点）；具体形态
    Plan 阶段拍板（global counter / per-stream counter / `tracing::warn!`
    + metrics 三选一）
  - 必须保 `EventsSubscribeStream::drop` 现状语义（**仅** set
    stop=true，**不**主动 join thread）
  - **不**改 `spawn_events_subscribe_thread` API 签名
- **预期结果**：N 次 drop 后 metric 显示 thread 收敛 + iceoryx2
  storage 收敛（如 iceoryx2 真 GC）

#### AC-C3: leftover topic 清理策略 land

- **触发条件**：AC-A3 任一子 case (a/b/c) 实测确证 leftover 阻塞重启
  / 新 task → reviewer Q6 拍板三选一中的一种。
- **允许的最小代码改动**（三选一，只 land 一种；**全 3 路径都不得动
  `robot_transport::iceoryx2/*` wrapper API**，本轮 hard boundary）：
  - **(c1) systemd `ExecStartPre=` hook**：在 `scripts/systemd/*.service`
    模板里加 `ExecStartPre=/usr/local/bin/robot_iceoryx2_sweep.sh
    <robot_id>` + sweep 脚本；脚本**只走 shell + 操作 iceoryx2 storage
    文件系统路径**（`find` / `rm` / 等），**不**调任何 Rust API；不动
    daemon 代码
  - **(c2) 独立 sweep 脚本**：仅交付 `scripts/iceoryx2_sweep.sh` +
    `scripts/iceoryx2_sweep.ps1`（沿 `CLAUDE.md` §7 Shell 检测 + §9
    双入口模式），由运维 / cron / 真机部署 runbook 手动调用；脚本同
    (c1) 同款"只走 shell + 文件系统"约束；**不**进 daemon 启动路径 /
    **不**进 systemd unit
  - **(c3) 都不做 + 文档约束**：仅在 `scripts/systemd/README.md` /
    daemon CLI help 加 "`--robot-id` 必须每次重启不复用" 强制约束 +
    `--robot-id` 命名空间隔离的真机回归说明；进 `docs/CHANGELOG.md`
    v0.4 候选 3 实测条目（沿 v0.3 同款偏离 / 实测登记机制）
- **预期结果**：A3 三子 case 实测全过（重启成功 + 新 task 100% 成功）
- **明示 out of scope**（reviewer round 1 拍板 hard boundary）：
  - **daemon 启动期 in-process sweep** —— 如需要在 s0_daemon /
    s1_daemon `main` 调任何 sweep API（含 `robot_transport::iceoryx2::*`
    helper），本轮**直接 out of scope**，走另立 outline（v0.4 后期 /
    v0.5）。原因：in-process sweep 必然侵入
    `robot_transport::iceoryx2/*` wrapper（无法仅靠 shell 路径触达
    iceoryx2 internal service registry），跟 §"严禁改动" 列表冲突。
  - sweep 脚本路径下任何对 iceoryx2 service handle / Node 的进程内
    操作 —— 仅允许操作 iceoryx2 storage 的**文件系统**路径
    （`/dev/shm/iceoryx2_*` 或 iceoryx2 0.8.1 真 storage path）+
    shell 工具（`find` / `rm` / `stat` / 等）；任何 Rust binary /
    Python script 调 iceoryx2 API → out of scope。

### Quality Gate（不进 AC，但是 PASS 条件）

- `cargo fmt --all --check` clean
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo test --workspace --tests --features "robot-rpc/client" --
  --test-threads=1`：全过（含 events_drain + events_subscribe +
  rr_server lib unit + B4 (3/3) task_rpc_e2e 6 cross-process）
- 双路 `crash_restart_smoke` 5/5 + 5/5（沿 B3 / B4 同款回归门槛）
- 候选 3 实测套件：基础 AC-A1/A2/A3 全过；条件性 AC-C1/C2/C3 按触发
  条件 land 后各自附带 lib unit / smoke 验证
- shutdown 顺序契约**不**破：现有 `shutdown_in_flight_invoke_rejected_no_register`
  + `registry_stop_and_join_all_*` lib unit 全保
- proto + fbs schema 字节级一致（无 frozen schema 改动）
- §6.7 5 条不变量真宣称保持（**0** 降级 / **0** 升级 / **0** 偏移）

---

## Reviewer 非阻断点（先列；可随 commit 进入）

- AC-A1 长跑节奏（顺序 vs 半并发）：建议先用顺序（沿 B2 单 action 序
  列化语义；半并发是 B3+ scheduler 范围，候选 3 不开半并发）
- AC-A2 终态延迟 p99 vs grace 默认 100ms 的关系：如实测 p99 远 < 100ms
  → grace 可下调（**不**做；候选 3 仅实测，不调 default；如需调走
  CHANGELOG 偏离登记 + reviewer round N 拍板）
- forensic dump 形态：沿 B4 (3/3) `Stdio::piped()` + 2 drain task →
  `Arc<Mutex<Vec<String>>>` + 失败时 dump `/tmp/v0_4_cleanup/<robot_id>/`
  模式（reviewer Q7 路径选定后定）
- §6.7 不升级到第 6 条 invariant（候选 3 默认**不**升级新 invariant；
  实测结论登记到 summary 即可，跟 B3 §3.7 source_task_id 单调同款处理）

---

## 不在范围（候选 3 边界）

按 reviewer 拍板（v0.4_entry §5 6 条 hard boundaries · §4.2 不在范围
列表）+ 本 Spec §"严禁改动"列表：

- **候选 1 / 2 / 4 任何 schema / payload / trait 改动**（Scripted /
  IK / robot-kinematics / dimos 切换）
- **iceoryx2 wrapper 重设计**：`Box::leak(node)` 模式 / Node ownership
  / `open_or_create_with_attributes` 语义 / publisher / subscriber
  生命周期 — 即使实测确证有泄漏，候选 3 也**不**改 wrapper；走另立
  outline（v0.4 后期 / v0.5）
- **shutdown 顺序契约重设计**：B4 §3.5 锁住的顺序不动；AC-C1 允许加
  `unregister` 但**不**改 4 步顺序
- **proto / fbs / action schema 改动**：B4 frozen；append 也不在候选
  3 范围（candidate 1 land 时再 append `ActionKindProto::Scripted`）
- **新 RPC method / RR topic / pubsub topic 命名**：B4 5 method + 4
  RR topic + per-ActionId pubsub topic 全 frozen
- **idempotency cache / auth / capability / role table 改动**：B4
  §3.2 + §3.3 + B4 (2/3-b) 收紧锁定
- **events_subscribe `Drop` 主动 join thread**：B4 (2/3-c) 拍板"drop
  是 sync，join 阻塞 tokio worker 不可取"；AC-C2 仅允许加 leak 计数，
  **不**改 Drop 行为
- **§6.7 不变量任一条降级 / 升级**：5 条全保
- **24h soak / 真硬件回归**：candidate 3 默认在 dev box 跑；真硬件
  回归推 candidate 3 land 后单独安排（沿 v0.3 B 真硬件回归节奏）
- **mTLS / OIDC / 真 auth**：v0.5+
- **rate limiting / circuit breaker**：v0.5+

---

## Reviewer 拍板（Plan 阶段输入）

> Spec round 1 (2026-05-11) reviewer 拍板 10 条 Q（Q1-Q10）。本节是
> Plan 阶段的**硬输入**；Plan 阶段不再"按 default 倾向走"或"待拍板"，
> 直接引用本节 Reviewer 拍板项。任何对 Q 的修改走 Spec round N+1。

### Q1: AC-A1 长跑量级 N + 节奏

- **选项**：(a) N=100（5 分钟级，CI 友好） / (b) N=1000（10-15 分钟，
  默认 candidate）/ (c) N=10000（小时级，仅手动）/ (d) 时间驱动（跑
  10 分钟取多少 task）
- **节奏**：(α) 顺序（一条 task 完成再下一条；候选 3 推荐，避开 B3+
  scheduler 范围）/ (β) 半并发（多 stream 同时 in-flight，每次 cap
  K 条）
- **Reviewer 拍板**：**(b) + (α) — N=1000，顺序执行**
- **Plan 输入**：长跑 binary `--iterations 1000` 默认；顺序模式
  （等终态再下一条，配合 Q9）；不进半并发路径（B3+ scheduler 范围）

### Q2: AC-A1 leak 检测阈值（三组）

- **VmRSS 增长上限**：N=1000 后 s1 △RSS / gateway △RSS 各自 ≤ ?
  - 候选：5 MB / 50 MB / 不设硬上限只画曲线
- **`EventsDrainRegistry::len()` 上限**：稳态 ≤ N task / ≤ 当前
  in-flight task / ≤ 0（强制 unregister）
- **iceoryx2 storage 文件数上限**：N=1000 后 ≤ ? 个 stale 文件
- **Reviewer 拍板**：**第一轮不设硬 RSS 上限；必须输出曲线**。
  **触发条件**（任一命中 = 触发对应条件性 AC）：
  - `EventsDrainRegistry::len()` 在 N=1000 + grace 期后**线性增长不
    收敛** → 触发 AC-C1
  - 子进程 subscribe thread count 在 N 次 stream drop + grace 期后
    **线性增长不收敛** → 触发 AC-C2
  - iceoryx2 storage stale 文件**阻塞重启 / 阻塞新 task** → 触发
    AC-C3
- **Plan 输入**：smoke binary 每 100 task 一档 + final 输出 5 类
  metric 曲线（VmRSS s1 + VmRSS gateway + `events_registry_len` +
  iceoryx2 storage 文件数 + subscribe thread count）；分析 + 触发
  判定走 Plan/实测期，**不**预设硬阈值

### Q3: AC-A2 终态帧延迟硬阈值

- **选项**：(a) 不设硬阈值，实测画 p50/p95/p99 直方图后报告 / (b)
  p99 ≤ 100ms（grace 当前 default） / (c) p99 ≤ 50ms（grace 当前
  default 的一半，留余量）
- **Reviewer 拍板**：**终态帧成功率硬性 = 100%；p50/p95/p99 先报告**。
  **p99 > 100ms（grace 当前 default）→ 触发 reviewer round，不自动
  改 default**
- **Plan 输入**：smoke 强断言"终态帧到 100%"（任一 task 漏终态 =
  FAIL）；记录 p50/p95/p99 + 报告；超 100ms 走 reviewer round N+1
  讨论 grace default 调整（不进自动修改路径）

### Q4: AC-A3 进程崩溃测试场景

- **选项**：(I) 仅 SIGKILL / (II) SIGKILL + 显式 panic(假 `--cfg
  test-panic`) / (III) SIGKILL + panic + OOM 模拟
- **Reviewer 拍板**：**(I) 只做 SIGKILL 三子 case（a/b/c）**
- **Plan 输入**：A3a (SIGKILL s1) + A3b (SIGKILL gateway) +
  A3c (SIGKILL 双进程) 三场景；不做 panic / OOM 模拟

### Q5: AC-A3 leftover 检测对象

- **选项**：(a) 仅 `/dev/shm/iceoryx2_*` 文件数 / (b) 仅 iceoryx2
  service discovery API（如 0.8.1 暴露） / (c) (a) + 重启后 service
  create 错误码 / (d) (a) + (b) + (c) 三合一
- **Reviewer 拍板**：**(c) — storage 文件数 + 重启后 create /
  InvokeTask 成功性；不用强依赖 iceoryx2 service discovery API**
- **Plan 输入**：smoke 在 SIGKILL 前后 stat iceoryx2 storage path
  + 重启后跑 N=10 个 InvokeTask 探活；如新 task 100% 成功 = leftover
  不阻塞；如失败 = 触发 AC-C3

### Q6: AC-C3 leftover 清理策略三选一

- **选项**：(c1) systemd `ExecStartPre=` hook / (c2) 独立 sweep 脚本
  / (c3) 都不做 + 文档约束（详 §AC-C3）
- **Reviewer 拍板**：**Spec 阶段不预选；本轮 hard boundary = 禁止
  wrapper API；若 A3 实测不阻塞，default = (c3) 都不做 + summary
  记录**
- **Plan 输入**：A3 实测后 reviewer round 拍板三选一；本轮 hard
  boundary：(c1) / (c2) / (c3) 任一路径**都不得**调
  `robot_transport::iceoryx2/*` wrapper API（详 §AC-C3 "明示 out of
  scope" 段；原"daemon 启动期 sweep" 已 out of scope）

### Q7: 测试代码路径 = lib integration test vs smoke binary

- **选项**：(a) `crates/robot-rpc/tests/pubsub_cleanup_e2e.rs`（沿
  B4 (3/3) `task_rpc_e2e.rs` 模式 + `[[test]] required-features =
  ["client"]` 让 cargo test 默认跳过）/ (b)
  `crates/robot-tests/src/bin/pubsub_cleanup_smoke.rs`（沿
  `crash_restart_smoke` / `clear_estop_e2e` 模式，独立 binary
  `--iterations N`）/ (c) 两种都做（lib 测覆盖 N=100 + smoke 覆盖
  N=1000）
- **Reviewer 拍板**：**(b) — `crates/robot-tests/src/bin/pubsub_cleanup_smoke.rs`**
- **Plan 输入**：独立 smoke binary，沿 `crash_restart_smoke` /
  `clear_estop_e2e` 同款模式；`--bin-dir` + `--robot-id-prefix` +
  `--iterations N` + `--hardware-id` + `--config` + ...；**不**走
  cargo test 默认路径；harness 自起 s1_daemon + gateway + RAII 清理
  + forensic dump。B4 (3/3) `tests/common/*` 是 lib integration test
  路径下的工程经验，binary 路径下需自行实现等价 harness（沿
  `crash_restart_smoke` 现有模式更直接）

### Q8: `EventsDrainRegistry::len()` 暴露方式

- **选项**：(a) test-only metric endpoint（HTTP / iceoryx2 topic /
  Unix socket 三选一） / (b) log scrape（s1_daemon log 周期打印
  `events_registry_len=N`） / (c) inline accessor（test binary 跟
  s1_daemon 同进程跑，Arc 共享 registry；但 B4 (3/3) 已锁定测试侧
  扮演 s0 角色 + 起独立 s1_daemon binary 路径，本选项不兼容）
- **Reviewer 拍板**：**(b) — log scrape `events_registry_len=N`**
- **Plan 输入**：s1_daemon 启动期 / shutdown 期 / 周期（每 100ms /
  按 Plan 阶段定）`tracing::info!(events_registry_len = N, "...")`
  结构化字段；smoke binary 端通过 stdout/stderr 抓 forensic + 解析
  曲线。**不**新增 metric endpoint / **不**改 daemon API

### Q9: AC-A1 task 间隔

- **选项**：(a) 0ms（连续 invoke 不等终态） / (b) 等终态 + 0ms 间隔
  / (c) 等终态 + grace_window 全过（保证 events_drain thread 真
  退出后再开下一条）
- **Reviewer 拍板**：**(b) — 等终态 + 0ms 间隔**
- **Plan 输入**：每条 task = invoke → 订阅终态 → 立即下一条；
  events_drain thread 在 grace 期内还活着的场景由本路径覆盖
  （真实客户端连续发任务行为）

### Q10: candidate 3 land 后是否阻塞候选 1

- **选项**：(a) 阻塞（候选 1 必须等候选 3 全 land 才开） / (b) 不阻塞
  （候选 1 / 候选 3 可并行 land）
- **Reviewer 拍板**：**(a) — 候选 3 land 前不开候选 1**（沿
  `docs/handoffs/v0.4_entry.md` §5 #5 跳序约束 hard boundary）
- **Plan 输入**：候选 3 全 AC（基础 AC-A1/A2/A3 + 任何触发的
  条件性 AC + Quality Gate）land 完成 + summary 归档 + reviewer
  ACK 之后才开候选 1。v0.4_entry §3.1 "可并行" 表述跟本拍板冲突，
  本 round 已补 docs fix 修正（独立 commit，跟本 Spec 同期 land）

---

## 跟其它 milestone handoff / spec 的链接

- [`docs/handoffs/v0.4_entry.md`](../../docs/handoffs/v0.4_entry.md)
  §4 当前切入点 + §5 6 条 hard boundaries
- [`team-swe/codemap/v0_4_pubsub_cleanup.md`](../codemap/v0_4_pubsub_cleanup.md)
  完整 Pre-Research（events_drain / events_subscribe / iceoryx2 wrapper
  + 10 条危险与不确定项）
- [`docs/handoffs/v0.3_b4_outline.md`](../../docs/handoffs/v0.3_b4_outline.md)
  §5.5 + §5.6 + §12 表 #4b 原 outline 留白
- [`docs/handoffs/v0.3_b4_summary.md`](../../docs/handoffs/v0.3_b4_summary.md)
  §1.2 #5 不在范围 + §3.5 shutdown 顺序契约 + §3.6 stream lifecycle
  + §4 跨进程 e2e harness 工程经验
- [`team-swe/specs/b4_3_3_cross_process_e2e.md`](b4_3_3_cross_process_e2e.md)
  Spec 模板参考
- `docs/00_design.md` §4.1 模块地图 + §4.9 RT 铁律 + §6.7 5 条不变量
  全保
