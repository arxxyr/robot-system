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

## Plan

> Plan v3（reviewer Plan round 3 ACK · 2026-05-11）。基于 Spec §AC +
> §Reviewer 拍板硬输入；**不**重新决策 Q1-Q10 / AC 边界 / hard boundary。
> Plan round 1 → round 2 → round 3 三轮 reviewer 拍板归档详 §P.3。

### P.0 工作基线

- 起点：`master` HEAD = `e3f99a1`（含 Spec round 1 完整 + Q1-Q10
  拍板 + 6 AC 边界）。
- 不开新分支（沿 B4 模式直接 commit 到 master + 每段 reviewer ACK）。
- 全 Plan 严格基于 Spec §AC + §Reviewer 拍板；**不**重新决策任何
  Q1-Q10 / AC 边界 / hard boundary。

### P.1 commit 节奏总览

**Happy path**（基础 AC 全过 + 零条件性 AC 触发）：

```
✨ feat: v0.4 候选 3 (1/2) pubsub cleanup smoke binary 骨架 + log
        scrape（feature-gated；含 fake S0 actor + register/shutdown
        events_registry_len log）
✨ feat: v0.4 候选 3 (2/2) AC-A1/A2/A3 实测 + 5 类 metric 曲线
📝 docs: v0.4 候选 3 summary 归档 + Phase 候选 3 完结
```

**触发条件性 AC**（任一）：(2/2) 跟 summary 之间各开**独立 reviewer
round**（每个 AC outline 拍板后再实施 + commit）：

```
✨ feat: v0.4 候选 3 AC-C1 EventsDrainRegistry::reap_finished()
        （outline pending；touch 前另开 round）
✨ feat: v0.4 候选 3 AC-C2 events_subscribe global leak counter
✨ feat: v0.4 候选 3 AC-C3 leftover 策略 <c1|c2|c3>
```

### P.2 工作流（按阶段顺序）

#### P.2.1 [commit (1/2)] smoke binary 骨架 + log scrape

##### P.2.1.a Cargo.toml 变更（feature-gated，沿 `clear_estop_e2e` 模式）

`crates/robot-tests/Cargo.toml` 追加：

```toml
[[bin]]
name = "pubsub_cleanup_smoke"
path = "src/bin/pubsub_cleanup_smoke.rs"
required-features = ["pubsub-cleanup-smoke"]

# optional deps（robot-rpc / tonic / tokio 已有，沿 clear-estop-e2e 同款）
# tokio 已含 process / io-util；本 bin 直接复用

[features]
# v0.4 候选 3 pubsub cleanup smoke 入口 feature。
# - 生产 release 不带本 feature（pubsub_cleanup_smoke binary 不参编译）。
# - 本机跑：cargo run -p robot-tests --bin pubsub_cleanup_smoke
#   --features pubsub-cleanup-smoke -- ...
# - 跟 clear-estop-e2e 同款 feature 守门：启用 robot-rpc/client + tonic
#   + tokio(process,io-util)
pubsub-cleanup-smoke = [
    "dep:robot-rpc",
    "dep:tonic",
    "dep:tokio",
]
```

**Build gate**：

```bash
cargo build -p robot-tests --release --bin pubsub_cleanup_smoke \
    --features pubsub-cleanup-smoke
```

##### P.2.1.b CLI args（沿 `crash_restart_smoke` / `clear_estop_e2e` 同款风格）

- `--bin-dir target/release`（默认）
- `--robot-id-prefix pubsub_cleanup`（默认）
- `--hardware-id <id>`（必填）/ `--config <toml>`（必填）
- `--iterations N`（默认 1000，Q1）
- `--metric-sample-every K`（默认 100，每 K task 一档曲线）
- `--invoke-dwell-ms <ms>`（默认 10，每条 InvokeTask = DwellMs(10ms)）
- `--phase A1|A2|A3|all`（默认 all）
- `--leftover-grace-ms <ms>`（默认 200，A3 SIGKILL 后等 storage 稳定）
- `--output-csv <path>`（可选；默认 stderr only）

##### P.2.1.c smoke 内置 fake S0 actor

pubsub_cleanup_smoke 必须复用 B4 (3/3) harness `FakeStatePublisher`
模式（`crates/robot-rpc/tests/common/fake_state.rs:39-153`）：

- **不**起真 s0_daemon（避免 `S0Command max_subscribers=1` 撞配额；
  同 B4 (3/3) Spec § Q3 拍板）
- 50Hz publish `JointState` 到 `/<robot_id>/s0/<hardware_id>/state`
- **必须**用 `robot_core::time::SystemClock::new()` 填 `stamp_ns`
  （跟 s1_daemon `read_anchor_from_s0_state` 同 clock domain；否则
  `max_stale_ms=200ms` 判 stale → s1 anchor exit 4）
- 一条 std::thread + `Box::leak(Node)` + RAII shutdown

**复用方式**：smoke binary 直接 inline 等价实现（沿 `clear_estop_e2e`
不复用 `tests/common` 的同款独立模式；具体实施期定，**不**改
`fake_state.rs` 公共可见性）。

##### P.2.1.d s1_daemon `events_registry_len` log（不碰 tick loop）

reviewer 拍板（Plan round 1 阻断 3）：**不**改
`run_action_tick_loop` 阻塞函数；register 即时 + shutdown final。

| 位置 | log 形态 |
|---|---|
| `crates/robot-s1/src/rr_server/handler.rs::handle_invoke` line 241 之后 | `tracing::info!(events_registry_len = ctx.events_registry.len(), action_id = %action_id, op = "register", ...)` |
| `crates/robot-s1/src/bin/s1_daemon.rs:505` `events_registry.stop_and_join_all()` 之前 / 之后各一行 | `tracing::info!(events_registry_len = events_registry.len(), op = "shutdown_pre" / "shutdown_post", ...)` |
| **可选** 独立 diagnostics thread | s1_daemon 启动期 spawn 一条 `std::thread`（**不**碰 tick loop），1s 周期打 `events_registry_len`；实施期判断是否必须（happy path 仅靠 register/shutdown event log 是否够 smoke 抓增长曲线）→ 如够则不加 thread；否则加 1s diagnostics thread |

**注销路径 log 本 commit 不加**（AC-C1 触发才有，等
`reap_finished` outline）。

**回归**：本改动只新增 `tracing::info!` 行，**不**改
`EventsDrainRegistry::register` API 签名 / 不改 `stop_and_join_all`
/ 不改 shutdown 顺序契约。现有 lib unit + integration test 全保。

##### P.2.1.e Quality Gate

- `cargo fmt --all --check` clean
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo build -p robot-tests --release --bin pubsub_cleanup_smoke
  --features pubsub-cleanup-smoke` 过
- 生产构建（无 feature）`cargo build -p robot-tests --release`
  不 link 本 binary
- 现有 lib unit + integration test 全过（B4 6 cross-process AC +
  s1_daemon 134/188 lib unit + 双路 crash_restart_smoke 5/5+5/5）

#### P.2.2 [commit (2/2)] AC-A1/A2/A3 实测 + 曲线输出

##### P.2.2.a smoke binary 实现

| 阶段 | 范围 | Q 引用 |
|---|---|---|
| AC-A1 | 1000 task 顺序 + 等终态 + 0ms；每 100 task + final 输出 5 类 metric 行 | Q1 + Q2 + Q9 |
| AC-A2 | 100 task + 等终态；测**端到端 client 视角延迟** = `client_receive_ns - event.at_ns`，p50/p95/p99 报告；终态成功率硬性 = 100% | Q3 |
| AC-A3 | A3a/b/c 三 SIGKILL 子 case + 重启后 N=10 InvokeTask 探活 | Q4 + Q5 |

##### P.2.2.b AC-A2 时延口径（Plan v3 修订；reviewer round 2 阻断 1 修复）

`crates/robot-rpc/src/events_subscribe.rs:403` 的
`envelope_to_task_event` 只填 `at_ns`（事件**语义**时刻，由 s1 端
events_drain 透传 `ActionEvent::{Started/Completed/...}.at_ns`）。
gateway forward 时刻**未**进 proto，client 无法直接拿。

**client 视角端到端延迟测量**（reviewer Plan round 2/3 写死硬约束）：

```rust
use robot_core::time::{RobotClock, SystemClock};

// tonic stream poll 拿到 TaskEvent 立即记录 client_receive_ns
let clock = SystemClock::new();
let client_receive_ns: u64 = clock.now().0;
let event_at_ns: u64 = task_event.at_ns;

// 时钟域一致性显式检查（不用 saturating_sub 掩盖）
let latency_ns: u64 = if client_receive_ns >= event_at_ns {
    client_receive_ns - event_at_ns
} else {
    // 反向 = 时钟域假设破裂；forensic dump + smoke fail
    dump_forensic_and_fail("clock_domain_violation", ...);
    return Err(...);
};
```

**关键约束**（reviewer 写死 · Plan round 3 ACK）：

- `client_receive_ns` **必须**用 `robot_core::time::SystemClock::new().now().0`
  （封装 `CLOCK_MONOTONIC_RAW`，跟 s1 端 `event.at_ns` 同 clock
  domain）
- **禁用** `tokio::time::Instant::now()` / `std::time::Instant::now()`
  / `SystemTime::now()`（全局 CLAUDE.md §1 + §8 + smoke binary 内
  `-D warnings` 守门；clippy 拦截）
- 反向 (`client_receive_ns < event_at_ns`) → **forensic dump + smoke
  fail**（直接报 `"clock_domain_violation"`）；**不**用 `saturating_sub`
  掩盖时钟域假设破裂
- smoke binary 跟 s1_daemon / gateway **必须同机器**跑（跨机器跨
  clock domain 不在候选 3 范围）

**测量与判定**：

- 100 task × 终态帧 = 100 个 `latency_ns` 样本（如终态帧丢一帧 = 任一
  task FAIL）
- p50 / p95 / p99 报告
- 终态成功率硬性 = 100%（任一漏终态 = FAIL）
- p99 > 100ms（grace 当前 default）→ 触发 reviewer round N+1，**不**
  自动改 grace default（Q3 拍板）

**不改 frozen schema**：**不**改 `events_subscribe.rs` /
`TaskEvent` proto 加 forward timestamp 字段（避免改 frozen
schema）；如未来真需要 forward timestamp → 走另立 outline。

##### P.2.2.c 5 类 metric 字段（Q2 拍板曲线模式）

- `vmrss_s1_kb` / `vmrss_gw_kb`（`/proc/<pid>/status::VmRSS` 行解析）
- `events_registry_len`（s1_daemon stderr log scrape，正则
  `events_registry_len=(\d+)` 取最近值；register/shutdown event log
  之间用 expected value 推断；如启用 1s diagnostics thread 则直接取
  最近一行）
- `iceoryx2_storage_count`（先 `/dev/shm/iceoryx2_*` glob；实测如
  iceoryx2 0.8.1 真路径不在 `/dev/shm` → 按实测调整 + reviewer
  round N 复核，P.3 #3 已 ACK fallback 路径）
- `gw_subscribe_threads`（`/proc/<gw_pid>/task` count - 基线 thread
  数）

##### P.2.2.d forensic dump + 退出码

forensic dump 沿 B4 (3/3) `Stdio::piped` 模式 binary 化：

- `Stdio::piped()` + 2 drain task → `Arc<Mutex<Vec<String>>>` 全程
  捕获
- 失败时 dump `/tmp/pubsub_cleanup_smoke/<robot_id>/{daemon,gateway}.{stdout,stderr}`
  + metric CSV
- 路径 P.3 #7 已 ACK

退出码沿 `crash_restart_smoke` 同款：

- `0` = AC-A1/A2/A3 全过 + 零条件性 AC 触发
- `1` = CLI / 环境错（binary 找不到 / config 缺）
- `2` = AC 至少一条 FAIL（含触发条件性 AC，需走后续 commit）

##### P.2.2.e Quality Gate

- smoke `--iterations 1000 --phase all` 全过（happy path）或退出码
  2 + metric 输出指明触发的条件性 AC
- 双路 `crash_restart_smoke` 5/5+5/5 不退化
- 现有 lib unit + integration test 全过
- 本 commit **不**应触发任何代码改动（仅 smoke binary 内部 + 输出
  CSV/metric），若实测过程中发现 race / non-deterministic 行为需要
  lib 改动 → 走 reviewer round N+1

#### P.2.3 [conditional commit] AC-C1（触发后另开 reviewer round）

**触发条件**：P.2.2 metric 显示 `events_registry_len` 在 N=1000 +
grace 期后线性增长不收敛。

**reviewer 拍板（P.3 #4）**：**本 Plan 不预批 AC-C1 调用点**。触发后
outline + reviewer round 拍板后才进实施 commit。

**默认研究候选**（outline 期 first reading；实施期可能改）：

- **`EventsDrainRegistry::reap_finished()`**：registry 内用
  `JoinHandle::is_finished()` 找已结束 handle，**锁外 join 并移除**。
  具体形态：
  - `pub fn reap_finished(&self) -> usize`：返回本次 reap 数；锁内
    swap_remove 出已 `is_finished()` 的 handle 进 owned Vec，锁外
    逐个 `join()`
  - 调用点：smoke binary / s1_daemon 周期 thread / `register` 路径
    前置；具体调用 cadence outline 期定
  - 保 `Mutex<Vec<Handle>>` ownership 模型（drain thread **不**自
    unregister；JoinHandle 由 registry 单一 ownership 持有 →
    `join_all` 确定性不破）

**禁止的方案**（reviewer 已拍板拒绝）：

- (a) `run_drain_loop` 退出路径自 unregister
- (b) watcher thread 持 JoinHandle 单独 join
- 任何让 drain thread 端 drop JoinHandle 的路径

**outline 阶段必须钉死**：

- `reap_finished` 跟 `stop_and_join_all` 的协作语义（reap 后
  `stop_and_join_all` 对剩余 handle 仍 no-op 兼容；reap 后
  registry.len() 减小）
- shutdown 顺序契约保不变（4 步顺序 + reviewer round 6 阻断契约）
- lib unit 扩 3+ 测：`reap_finished_empty_returns_zero` /
  `reap_finished_after_terminal_returns_one` /
  `reap_finished_then_stop_and_join_all_is_noop_for_reaped` 等

**回归 Quality Gate**：

- `shutdown_in_flight_invoke_rejected_no_register` /
  `registry_stop_and_join_all_*` 全过
- 双路 `crash_restart_smoke` 5/5+5/5
- 重跑 P.2.2 smoke AC-A1：`events_registry_len` 收敛到稳态（如周期
  reap → 趋近 in-flight task 数）

#### P.2.4 [conditional commit] AC-C2 events_subscribe thread-alive counter（Plan v3 修订；reviewer round 2 阻断 2 修复）

**触发条件**：P.2.2 metric 显示 `gw_subscribe_threads`
(`/proc/<gw_pid>/task` count) 线性增长不收敛 + reviewer round 拍板
触发 C2。

**改动**（P.3 #5 拍板 + Plan round 2 阻断 2 修复）：

`crates/robot-rpc/src/events_subscribe.rs` 加：

```rust
use std::sync::atomic::{AtomicU64, Ordering};

/// 活 subscribe thread 数（**不**是活 stream 数）。
/// thread closure 进入 run_subscribe_loop 前 += 1；thread 真退出时
/// RAII guard -= 1。EventsSubscribeStream::drop 不动 counter（仅
/// signal stop=true）—— 防止 drop 立即减计数但 thread 还活着的
/// leak 假阳性。
pub static EVENTS_SUBSCRIBE_THREADS_ALIVE: AtomicU64 = AtomicU64::new(0);

struct ThreadAliveGuard;
impl ThreadAliveGuard {
    #[inline]
    fn enter() -> Self {
        EVENTS_SUBSCRIBE_THREADS_ALIVE.fetch_add(1, Ordering::SeqCst);
        Self
    }
}
impl Drop for ThreadAliveGuard {
    fn drop(&mut self) {
        EVENTS_SUBSCRIBE_THREADS_ALIVE.fetch_sub(1, Ordering::SeqCst);
    }
}
```

`spawn_events_subscribe_thread` thread closure 内（line 223-242
附近），**在** `run_subscribe_loop` 调用前**起 guard**：

```rust
let join = thread::Builder::new().name(thread_name).spawn(move || {
    let node = NodeBuilder::new().create::<IxService>().expect(...);
    let mut subscriber = open_sub(...).expect(...);
    Box::leak(Box::new(node));
    let _ = ready_tx.send(());
    drop(ready_tx);

    let _alive_guard = ThreadAliveGuard::enter();  // ← thread 真启动时 +=1
    run_subscribe_loop(&cfg, &mut subscriber, &tx, &stop_clone, &metrics_clone);
    // _alive_guard drop here = thread 真退出时 -=1
})?;
```

**硬约束**（reviewer Plan round 2 + round 3 写死）：

- counter 语义 = **活 subscribe thread 数**（**不**是活 stream
  数）；名字 `EVENTS_SUBSCRIBE_THREADS_ALIVE` 显式
- counter 加减**只在** thread closure 内 RAII guard 完成
- `EventsSubscribeStream::drop` **不**碰 counter（保 Drop join
  语义；仍**仅** set stop=true；可选加
  `tracing::debug!("requested_stop", topic=...)` log）
- **不**改 `spawn_events_subscribe_thread` API 签名 / 不改
  `EventsSubscribeConfig`

**smoke binary 端读取**：

- 通过 gateway stderr log scrape（gateway 启动期 / shutdown / 周期
  diagnostics 打 `EVENTS_SUBSCRIBE_THREADS_ALIVE` 当前值）
- 或直接读 `/proc/<gw_pid>/task` count（已是 P.2.2 metric 字段
  `gw_subscribe_threads`；本 counter 是 lib 内一致性验证，跟
  OS-level thread count 应一致）

**lib unit 扩**：

- `thread_alive_counter_increments_on_spawn_decrements_on_thread_exit`：
  spawn → counter +=1；request_stop + join → counter -=1
- `events_subscribe_stream_drop_alone_does_not_decrement_counter`：
  仅 drop stream（不 join thread）→ counter 不变（直到 thread 真
  退出 RAII guard drop 才 -=1）

**回归 Quality Gate**：

- `drop_stream_signals_stop_to_thread` lib unit 不退化（drop 信号
  stop 行为不变）
- 重跑 P.2.2 smoke AC-A1：counter 在 N=1000 stream drop + 各 thread
  真退出后收敛到 0（或稳态低值）
- 如收敛到 N（接近 1000）但 OS thread count 也收敛到 0 → counter 实
  现 bug；如 OS thread count 线性增长但 counter 收敛 0 → 真 leak
  （thread 没退出但 counter 未 +=1），两种情况 reviewer 拍板

#### P.2.5 [conditional commit] AC-C3 leftover 策略 land

**触发条件**：P.2.2 AC-A3 任一子 case 实测重启被 leftover 阻塞 +
reviewer round 拍板三选一（Q6 现状 = Spec 阶段不预选）。

**改动按三选一**（详 Spec §AC-C3）：

| 路径 | 改动 |
|---|---|
| **(c1)** systemd ExecStartPre | `scripts/systemd/*.service` 加 `ExecStartPre=...` + `scripts/iceoryx2_sweep.sh`（纯 shell + 文件系统） |
| **(c2)** 独立 sweep 脚本 | `scripts/iceoryx2_sweep.{sh,ps1}` + `scripts/systemd/README.md` 加调用说明 |
| **(c3)** 都不做 + 文档约束 | `scripts/systemd/README.md` + daemon CLI help 加 `--robot-id` 不复用约束 + `docs/CHANGELOG.md` v0.4 候选 3 实测条目 |

**全 3 路径硬约束**：**不**调 `robot_transport::iceoryx2/*` wrapper
API。

**回归 Quality Gate**：

- 重跑 P.2.2 AC-A3：重启后 InvokeTask 100% 成功
- 双路 `crash_restart_smoke` 5/5+5/5

#### P.2.6 [commit summary] summary 归档

**新增文件**：`docs/handoffs/v0.4_pubsub_cleanup_summary.md`（沿
`v0.3_b4_summary.md` 模式）：

- §1 范围 / 不在范围（实测什么 + 触发了哪些条件性 AC）
- §2 commit chain（含本 Plan 全部 commit hash + 任何 reviewer round
  拍板）
- §3 关键契约审计（§6.7 5 条不变量真宣称保持 + shutdown 顺序契约
  保持 + Box::leak 模式不动）
- §4 实测证据（5 类 metric 曲线 + p50/p95/p99 报告 + A3 三子 case
  结果 + 重启探活结果）
- §5 跟其它 milestone handoff 的链接
- §6 开工前必读（候选 1 接班）

`docs/handoffs/README.md` milestone anchor 表追加 v0.4 候选 3
summary 行。

**Quality Gate**：

- 全部 commit chain push 后 reviewer ACK
- 候选 1 不开（Q10 拍板）

### P.3 reviewer Plan round 1/2/3 拍板（已 ACK）

| # | 项目 | 拍板 |
|---|---|---|
| 1 | (1/2) + (2/2) 拆分 vs 一段成形 | **ACK** (1/2) + (2/2) |
| 2 | s1_daemon `events_registry_len` log cadence | **改为 register 即时 + shutdown final**；周期日志如保留则 1s 独立 diagnostics thread；**禁动** `run_action_tick_loop` |
| 3 | iceoryx2 storage path | **ACK** `/dev/shm/iceoryx2_*` + 实测 fallback |
| 4 | AC-C1 调用点 | **不预批**；C1 触发后另开 reviewer round；默认研究 `reap_finished()` 模式；**禁** drain thread 自 unregister / watcher thread |
| 5 | AC-C2 leak metric 形态 | **ACK** global `AtomicU64` + **活 subscribe thread 数**语义 + RAII guard 加减；**禁** 改 `EventsSubscribeStream::drop` join 语义 / **禁** drop 端碰 counter |
| 6 | commit message format | **ACK** 沿 B3 / B4 同款 `✨ feat: v0.4 候选 3 (x/y) ...` |
| 7 | forensic dump path | **ACK** `/tmp/pubsub_cleanup_smoke/<robot_id>/` |

**Plan round 2 阻断（已修进 Plan v3）**：

| # | 阻断 | 修复 |
|---|---|---|
| Plan v2 阻断 1 | AC-A2 时间口径用 `tokio::time::Instant::now()` 不能跨 clock domain | 改为 `robot_core::time::SystemClock::new().now().0`（详 §P.2.2.b） |
| Plan v2 阻断 2 | AC-C2 counter 在 Drop 里 decrement 会掩盖 thread leak | counter 名 `EVENTS_SUBSCRIBE_THREADS_ALIVE`（活 thread 数）+ thread closure 内 RAII guard +=1/-=1；Drop 不碰 counter（详 §P.2.4） |

**Plan round 1 阻断（已修进 Plan v2/v3）**：

| # | 阻断 | 修复 |
|---|---|---|
| Plan v1 阻断 1 | smoke binary 缺 Cargo 变更 | §P.2.1.a feature-gated `[[bin]] + required-features = ["pubsub-cleanup-smoke"]` + optional deps |
| Plan v1 阻断 2 | smoke 拓扑漏 fake S0 actor | §P.2.1.c 复用 `FakeStatePublisher` 模式（50Hz JointState publish + SystemClock + leak Node） |
| Plan v1 阻断 3 | `events_registry_len` 不能写成 "tick loop 每 100ms" | §P.2.1.d register 即时 + shutdown final + 可选 1s 独立 diagnostics thread；**禁动** `run_action_tick_loop` |
| Plan v1 阻断 4 | AC-C1 调用点不能选 (a) `run_drain_loop` 自 unregister | §P.2.3 触发后另开 reviewer round；默认研究 `reap_finished()` |

### P.4 Plan-Execute diff 登记位置

> 实施期任何对 Plan 的偏离（增加 commit / 改 commit message / 调
> default 值 / 等）登记在本节，沿 b4_3_3 spec 同款 "Plan-Execute X
> 处 diff" 段。Plan reviewer ACK 后实施才能进；实施期 reviewer round
> N+1 fix 触发改 Plan 走 Spec round N+1。
>
> 候选 3 完结归档于 [`docs/handoffs/v0.4_pubsub_cleanup_summary.md`](../../docs/handoffs/v0.4_pubsub_cleanup_summary.md)
> （commit `2518fc0` AC-C1 reaper land 之后）。4 处偏离全归档如下：

#### diff #1: (2/2) 拆 (2/2-a) + (2/2-b)

- **原 Plan**：Plan v3 §P.1 拍板 (2/2) 单 commit
  `✨ feat: v0.4 候选 3 (2/2) AC-A1/A2/A3 实测 + 5 类 metric 曲线`
- **实际偏离**：reviewer 实施期拍板拆 2 子 commit
  * (2/2-a) 共用基础设施（metric helpers / RPC helpers / report struct
    / BringUp + bring_up_pair / tear_down_pair / CSV writer）— commit
    `244249e` `✨ feat: v0.4 候选 3 (2/2-a) 共用基础设施
    (metric/RPC/BringUp helpers)`
  * (2/2-b) AC-A1/A2/A3 实测 + 触发判定 — commit `976da62`
    `✨ feat: v0.4 候选 3 (2/2-b) AC-A1/A2/A3 实测 + 触发判定`
- **拍板**：reviewer 实施期拍板（commit message 内登记 "reviewer 实施
  期拍板拆 (2/2) = (2/2-a) 共用基础设施 + (2/2-b) 3 AC 一起"）
- **理由**：(2/2) 量级 ~700-900 行；拆开让 reviewer 复审基础设施 +
  AC 实施分两轮，每段 commit diff 可控
- **影响**：实施 commit chain 多 1 个 commit；commit message 内说明
  Plan-Execute diff；其它路径不变

#### diff #2: smoke `count_iceoryx2_storage_files` storage path 修正

- **原 Plan**：Plan v3 §P.2.2.c 拍板 `iceoryx2_storage_count` glob
  `/dev/shm/iceoryx2_*`
- **实际偏离**：候选 3 (2/2-b) 实测发现 `/dev/shm/iceoryx2_*` glob
  永远 0（错路径）；AC-C1 outline §7 bounded research 实测 iceoryx2
  0.8.1 真 storage = `/tmp/iceoryx2/`（来源
  `iceoryx2-pal-configuration-0.8.1/src/lib.rs:40` Linux 默认
  `ICEORYX2_ROOT_PATH = "/tmp/iceoryx2/"`）；修复进 commit
  `6e0033b` `📝 fix: v0.4 候选 3 smoke
  count_iceoryx2_storage_files storage path 修正`
- **拍板**：AC-C1 outline §7 / §8.1 reviewer round 1 拍板
- **理由**：Plan v3 §P.2.2.c 拍板时未实测真路径；实测期才发现
- **影响**：smoke binary `count_iceoryx2_storage_files` 签名
  `() -> Result<u64, String>` → `() -> (u64, &'static str)`；返
  `(count, root_hit)` tuple；iter=0 eprintln 命中 root；A3 sub-case
  内 2 处 caller 同步解构兼容；root 不存在返 `(0, "")` 降级
  informational only

#### diff #3: smoke `run_phase_a1` final sample sleep 500ms → 2000ms

- **原 Plan**：Plan v3 §P.2.2 拍板候选 3 (2/2-b) 实施 `run_phase_a1`
  final sample 前 sleep 500ms（= 5x `terminal_grace` margin）
- **实际偏离**：AC-C1 加 1s cadence reaper thread 后，final 收敛延迟
  ≈ `reaper_period + terminal_grace + 调度抖动` ≈ 1100ms；500ms 不够会
  刚好采在下一次 reap 前误判 C1。改 2000ms（= 2× reaper cycle +
  terminal_grace + margin，保留 2x 余量）；修进 commit
  `2518fc0` `✨ feat: v0.4 候选 3 AC-C1
  EventsDrainRegistry::reap_finished()`（跟 AC-C1 reaper 行为同 commit）
- **拍板**：AC-C1 outline §3.1 + reviewer round 1 阻断 1
- **理由**：reaper cadence + smoke final sleep 强耦合，必须一起改
- **影响**：smoke 端 `events_registry_len final == 0` 硬 gate 稳定；
  注释明示改 reaper cadence 必须同步改 grace（或反之）

#### diff #4: AC-C1 实施 + smoke storage path fix 拆 2 commit

- **原 Plan**：AC-C1 outline 初版 §8 拍板单 commit
  `✨ feat: v0.4 候选 3 AC-C1 ... + iceoryx2 storage path fallback`
- **实际偏离**：reviewer round 1 §10 #7 拍板拆 2 commit
  * commit (i) `📝 fix: ... storage path 修正`（`6e0033b`）—
    纯 smoke binary 范围、informational fix、可单独 ACK push
  * commit (ii) `✨ feat: AC-C1 EventsDrainRegistry::reap_finished()`
    （`2518fc0`）— s1 lib + s1 binary + smoke final sleep
    联动；依赖 commit (i) land
- **拍板**：AC-C1 outline §8 reviewer round 1 §10 #7
- **理由**：commit (i) 是 informational fix（纯 smoke binary 端，错路径
  改对路径）；commit (ii) 才是 AC-C1 reaper 行为改动（s1 lib API 扩
  `reap_finished` + s1 binary 加 reaper thread + shutdown step 4）；
  拆开让 reviewer 分两段 ACK，每段 diff 可控
- **影响**：实施 commit chain 多 1 个 commit；reviewer 分两轮 ACK
  push；两 commit 都 push 后才一起重跑 phase all 验收

---

### P.4 总结

候选 3 完结时点：commit `2518fc0` AC-C1 reaper land + push。Plan-Execute
diff 累计 4 项全部归档完成。AC-C1 outline §6.1 硬 gate 全过
（`events_registry_len final == 0` + `triggered_c1 == false` + smoke
`--phase all` exit code 0）；软指标 `VmRSS s1` 仍线性 = `Box::leak(node)`
residual，**不**升级本轮 blocker（reviewer round 1 §10 #5 拍板；详
[`docs/handoffs/v0.4_pubsub_cleanup_summary.md`](../../docs/handoffs/v0.4_pubsub_cleanup_summary.md)
§3.7）。

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
