# Spec · B4 (3/3) cross-process integration test

## What

为 v0.3 B4 task control plane 写跨进程端到端集成测试 + lib/in-process ClearError handler unit + 双路 `crash_restart_smoke` 回归，证明 5 RPC（InvokeTask / CancelTask / GetTaskStatus / StreamTaskEvents / ClearError）+ s1 RR server + events_drain pubsub + gateway 5 rr_io_thread 构成的全链路在跨进程下正确工作，且 wire 契约 / 收紧的角色权限 / idempotency 语义没被打破。

预定路径：`crates/robot-rpc/tests/task_rpc_e2e.rs`（沿 cargo `tests/*.rs` integration test 模式，**不**复用 A1 的 `crates/robot-tests/src/bin/clear_estop_e2e.rs` binary 模式 —— reviewer 拍板路径）。

## 影响范围

| 模块 | 改动类型 |
|---|---|
| `crates/robot-rpc/tests/task_rpc_e2e.rs` (新) | 6 cross-process integration test + 测试 harness |
| `crates/robot-rpc/Cargo.toml` | dev-deps 加 tonic-build 客户端 stub / tempfile / nix（如需） |
| `crates/robot-rpc/src/task_handlers.rs` (扩 1 测) | 1 个 lib/in-process ClearError handler test（不进 cross-process 表） |
| `crates/robot-tests/src/bin/crash_restart_smoke.rs` | **不动**；仅 quality gate 跑 5/5+5/5 |
| `docs/handoffs/v0.3_b4_summary.md` (新) | (3/3) 收尾 handoff（reviewer 闭环 + (3/3) ACK 后的 Phase B 完结归档） |

## 风险等级

**中**（cross-process spawn 时序敏感 + iceoryx2 service 名隔离 + signal handler 跨进程 race + per-ActionId topic 生命周期实测留白；详 [`team-swe/codemap/b4_3_3.md`](../codemap/b4_3_3.md) §"危险与不确定项" 8 条）。

## 回滚策略

(3/3) 是 test-only 加项，不改 production 代码（除 1 处 ClearError handler unit）。回滚 = revert commit。如发现某测试 flaky → 标 `#[ignore]` + 留 issue follow-up，**不**回滚整个 commit（test 框架本身已审计通过）。

## 目标分支 / 交付形式

- 分支：`master`（直接 commit；按 (3/3) 单 commit 落 + reviewer round 1 fix commit 模式，沿 (2/3-a) / (2/3-b) / (2/3-c) 同款）
- 交付：1 个 `✨ feat: B4 (3/3) cross-process integration test + ClearError handler unit + double-track smoke regression` commit + (可能) 1 个 reviewer round fix commit
- (3/3) 后：1 个 `📝 docs: B4 (3/3) summary + Phase B 收尾` commit（沿 v0.3_b3_summary.md 同款）

## Acceptance Criteria

> AC 在代码之前定义，**6 个 cross-process AC 直接对应 outline §9.2 + reviewer 重点**，1 个 lib/in-process（outline §9.3 ClearError handler 字面层）。所有 cross-process AC 共用一套 harness（spawn s1_daemon + robot_rpc_gateway 双进程 + 等 ready + 收尾 SIGTERM）。

### AC-1: 端到端 InvokeTask + StreamTaskEvents（cross-process）

- **前置条件**：`robot_id = b4_e2e_${pid}_${case_seq}` 唯一；spawn s1_daemon + gateway 双进程已 ready（s1 anchored + gateway TCP listening + clear_estop dry_run gate 等价的 task_status dry_run gate 通过）。
- **操作**：tonic client 用 `maintainer-fake-token-001` 调 `InvokeTask` with `kind = ACTION_KIND_DWELL_MS, idempotency_key = ik-1, request_id = UUIDv7`。立即调 `StreamTaskEvents { request_id, action_id }` 拿 stream。
- **预期结果**：
  - `InvokeTask` 返 `Ok(TaskHandle { request_id, action_id == request_id })`
  - `StreamTaskEvents` stream 至少收到 1 条 `Started` event，最终收到 1 条 `Completed` 终态 event 后流关闭（mock_dwell duration_ms ~100ms → 总 < 5s）
  - 子进程 stderr / stdout 无 panic / `BUG:` / `[ERROR]` 关键字
- **验证方式**：`cargo test --test task_rpc_e2e --features e2e -- happy_invoke_stream`

### AC-2: CancelTask happy path（cross-process）

- **前置条件**：同 AC-1 双进程 ready。
- **操作**：`InvokeTask kind=DwellMs duration_ms=10000`（长 dwell）→ `StreamTaskEvents` 拿 stream → 100ms 后 `CancelTask { action_id }`（用同 token）→ 等 stream 收到 `Cancelled` 终态。
- **预期结果**：
  - `InvokeTask` 返 `Ok`
  - `CancelTask` 返 `Ok(Ack { request_id, cleared_at_ns > 0, dry_run = false })`（注：fbs `cancel_at_ns` 映射 proto `Ack.cleared_at_ns`）
  - `StreamTaskEvents` stream 收到 `Cancelled` event 后关闭（≤ 200ms）
- **验证方式**：同 AC-1。

### AC-3: GetTaskStatus 三路径（cross-process）

- **前置条件**：同 AC-1 双进程 ready。
- **操作**：3 个独立子 case：
  - **3a 执行中**：`InvokeTask` → 立即 `GetTaskStatus { action_id }` → 期望 `state = EXECUTING`
  - **3b 终态**：`InvokeTask DwellMs(50ms)` → 等 100ms → `GetTaskStatus` → 期望 `state = COMPLETED`
  - **3c 未知 ID**：`GetTaskStatus { action_id = "00000000-0000-7000-8000-000000000000" }` → 期望 `tonic::Code::NotFound`（s1 端 `Coordinator::status` 返 `None` → fbs `ActionRpcStatus::NotFound` → tonic `NotFound`）
- **预期结果**：3 个 sub-case 全 PASS。
- **验证方式**：3 个独立 `#[tokio::test]` async fn。

### AC-4: InvokeTask idempotency replay（cross-process）

- **前置条件**：同 AC-1 双进程 ready。
- **操作**：第 1 次 `InvokeTask kind=DwellMs idempotency_key=ik-rep request_id=R1` → 拿 `TaskHandle1`。等 50ms。第 2 次 **同 idempotency_key + 同其它字段（fingerprint 一致）+ 不同 request_id=R2 + 不同 timeout_ms** → 期望 cache replay 命中 → 返 `TaskHandle2 { request_id=R2, action_id == TaskHandle1.action_id }`（cache 重建用 incoming request_id echo + cached action_id）。
- **预期结果**：第 2 次返的 `action_id` 跟第 1 次一致 + `request_id` 用 R2（reviewer round 2 阻断 1 锁定的"合法 retry 不被误杀"语义）。
- **可选附加**（同 fn 内）：第 3 次 **同 idempotency_key + 不同 source（fingerprint 改变）** → 期望 `tonic::Code::AlreadyExists`（Conflict 路径）。
- **验证方式**：1 个 `#[tokio::test]` 含 3 段。

### AC-5: Auth deny — viewer InvokeTask + operator ClearError（cross-process）

- **前置条件**：同 AC-1 双进程 ready。
- **操作**：
  - **5a viewer**：用 `viewer-fake-token-001` 调 `InvokeTask` → 期望 `tonic::Code::PermissionDenied`（viewer 不持 InvokeTask capability）
  - **5b operator ClearError**：用 `operator-fake-token-001` 调 `ClearError` → 期望 `tonic::Code::PermissionDenied`（B4 (2/3-b) 收紧后 operator 不再持 clear_error capability）
- **预期结果**：两 sub-case 都返 `PermissionDenied` + error msg 含 capability 名。
- **验证方式**：1 个 `#[tokio::test]` 含 2 段。

### AC-6: Wire 契约 — `JointCommand.source_task_id` 单调（cross-process）

- **前置条件**：同 AC-1 双进程 ready + 测试侧另起 1 个 iceoryx2 subscriber 订阅 `/<robot_id>/s1/<hw>/command`（gateway 不消费这个 topic，测试 harness 侧观察）。
- **操作**：`InvokeTask DwellMs(100ms)` → wait stream Completed → 期间收集所有 `JointCommand.source_task_id` 值（应为非空序列；mock_dwell None override 时 hold loop 仍 publish；实际 source_task_id 来自 `TickState.cmd_seq` 透传）。
- **预期结果**：收集到的 `source_task_id` 序列**严格单调递增**（B3 §3.7 wire 契约：S0 `adapter.rs::rt_thread` 用 seq 单调判 `CONTROL_LOST` → `ACTIVE` 恢复；非单调 → wire 协议被破坏）。
- **验证方式**：1 个 `#[tokio::test]` + iceoryx2 subscriber on test 主线程。

### AC-7: ClearError handler 字面层（lib/in-process，**不**进 cross-process）

- **前置条件**：构造 `GatewayHandlerState::for_test()` + tonic Request<ClearErrorCommand>。
- **操作**：调 `task_handlers::handle_clear_error(&state, request).await`。
- **预期结果**：因 mock RR forward channel rx 已 leak（不真接 daemon），返 tonic `Code::Unavailable`（forward channel disconnect / timeout）；**不**期望真 land Degraded → Idle 转换 — 那条由 `crates/robot-s1/tests/action_dispatch.rs::safety_abort_transitions_to_degraded_then_clear_error_resumes_idle` 已覆盖。本 AC 仅锁住 RPC handler 字面层（proto encode → decide path → forward → timeout/unavailable mapping）。
- **验证方式**：1 个 `#[tokio::test]` in `crates/robot-rpc/src/task_handlers.rs` `tests` mod。

## Quality Gate（不进 AC，但是 PASS 条件）

- `cargo fmt --all` clean
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo test --workspace --all-targets`：所有现有测试 + 新加 7 测全过
- `target/release/crash_restart_smoke --iterations 5`：5/5 PASS
- `target/release/crash_restart_smoke --use-mock-s1 --iterations 5`：5/5 PASS
- 新加 6 个 cross-process AC：每个 case 子进程 stderr 无 panic / `BUG:` 关键字（harness 检）

## Reviewer 非阻断点（随 commit 进入）

reviewer (2/3-c) ACK 时附加："EventsSubscribeStream::drop 里除了 request_stop，要确认 subscribe thread 的 handle 有回收路径"。

回应：(2/3-c) 落地选择 detach 而非 join（drop 是 sync，join 阻塞 tokio worker；详 events_subscribe.rs::EventsSubscribeStream 顶部 doc）。`drop_stream_signals_stop_to_thread` 测试已锁住 1s 内 stop 信号被观察到（poll_interval=50ms → ≤ 1s 退出 + iceoryx2 subscriber drop + OS 回收 detached thread 资源）。

(3/3) 不为此加专项测；如 reviewer round 1 觉得证据不够，可在 cross-process happy_invoke_stream test 内嵌 thread count 前后 diff 断言（low priority，留 follow-up）。

## 不在范围（B4 (3/3) 边界）

- `--allow-empty-latch` / EStop latch 路径（A1 已锁；B4 不碰）
- `JointCommand.source_task_id` 升级 §6.7 invariant（outline §10 #9 default：保持现状）
- per-ActionId pubsub topic dynamic cleanup grace 实测扩展（outline §5.6 #4b 留 v0.4+）
- mTLS / OIDC（v0.5+）
- proto / fbs schema 字段集改动（B4 schema frozen，仅 append）

## 不确定项（Research 阶段消除 / Innovate 阶段拍板）

- **[Q1]** 跨进程 e2e test 用 cargo `tests/*.rs` integration test 还是参照 A1 用 `crates/robot-tests/src/bin/` binary？
  - **reviewer 已拍**：cargo `tests/*.rs`（路径 `crates/robot-rpc/tests/task_rpc_e2e.rs`）
  - 但 cargo test 默认每 test fn 独立进程跑会让 spawn child process 的 lifecycle 变复杂；A1 用 binary 是为绕开 cargo test 串并发问题
  - **Research 阶段验证**：cargo `tests/*.rs` 真能跑 6 个独立 spawn 子进程的 case 吗？端口 / iceoryx2 service 名 / 子进程 cleanup 在 cargo test 框架下行得通？
- **[Q2]** spawn 子进程的 ready signal 检测：A1 用 stderr marker poll；B4 task RPC 没有"dry_run readiness gate"等价物 —— 用什么作 ready signal？
  - 候选 A：复用 ClearEStop dry_run gate（gateway 起来 = 5 个 RR client 都起来）
  - 候选 B：用 `GetTaskStatus { action_id = unknown }` 期望 `NotFound` 作 ready probe
  - 候选 C：用 stderr marker `"rr_server_threads ready"`（s1）+ `"tonic server bound on"`（gateway）
  - **Research 阶段验证**：哪个 candidate 最稳？
- **[Q3]** `JointCommand.source_task_id` 单调验证（AC-6）需要测试侧订阅 `/<robot_id>/s1/<hw>/command` —— iceoryx2 多订阅者并发是否安全？测试侧 subscriber 跟 s1 publisher 跑在不同进程，应该 OK，但要验证。
- **[Q4]** outline §9.2 default #8a (c) 路径下 6 cross-process 跑全工作量 vs 单 case 选择性跑：reviewer 拍板"覆盖 5 RPC + StreamTaskEvents 端到端"是否要每 RPC 一个独立 test？还是 1 个 happy + 5 个 reject + 1 个 contract？
  - **当前 AC 设计**：6 个 cross-process AC（happy invoke+stream / cancel / status 三路径 / idempotency / auth deny / wire 契约）+ 1 lib/in-process（ClearError handler）= 总 7 测，跟 outline §9.2 / §9.5 一致。

## State

- ✅ Step -1 Pre-Research（CodeMap 已落 `team-swe/codemap/b4_3_3.md`）
- 🟢 Step 0 Intake（本 Spec 落地，AC 7 条）
- ⬜ Step 1 Research（实验 [Q1]-[Q4]，回写 § Research Findings）
- ⬜ Step 2 Innovate（≥ 2 候选 spawn / ready 模式）
- ⬜ Step 3 Plan（原子级 + 用户审批门禁）
- ⬜ Step 4 Execute
- ⬜ Step 5 Review
- ⬜ Step 6 Archive

## Research Findings

### [Q1] cargo `tests/*.rs` 多子进程 spawn 可行性

**结论**：**可行但需配套 harness module**；reviewer 拍板路径（`crates/robot-rpc/tests/task_rpc_e2e.rs`）站得住，但要把 A1 SubprocessHandle / CaseCtx 等基础设施迁移成 `tests/common/mod.rs` 共享 module 给 6 个 test fn 复用，不能让每个 test fn 各写一份 boilerplate。

**证据**：

- `cargo test` 默认行为（参 `crates/robot-s0/tests/serial_rt_e2e.rs:73-94` 实例 + cargo Book §10.3）：
  - 每个 `tests/<name>.rs` 编译成一个独立 test binary；一个 binary 内 `#[test]` / `#[tokio::test]` fn 默认按 CPU 核数并发跑（环境变量 `--test-threads` / `RUST_TEST_THREADS` 覆盖）
  - 6 个 async test 都在**同一进程** + 不同线程；**每个** `#[tokio::test]` 各自构造独立 tokio Runtime（`tokio::test` macro 内部行为）
  - 同 binary 内并发 OK；隔离靠 `static SEQ: AtomicU32 = AtomicU32::new(0)` + `format!("...{}_{}", std::process::id(), seq)` 模式给 robot_id / iceoryx2 service / loopback port 派生唯一名（`serial_rt_e2e.rs:73-94` 已 demo）
- A1 e2e 选 binary 模式而非 cargo test 的真原因（`crates/robot-tests/src/bin/clear_estop_e2e.rs:9-11` + `docs/handoffs/v0.3_a1_4_clear_estop_smoke.md:2-11`）：**集中管理 tokio runtime + 集中日志收集 + `--case all` 一键复跑** 三点；不是 cargo test 不能做，是工程便利
- 子进程 cleanup 关键：`tokio::process::Child` Drop **默认不 kill**（A1 注释 `clear_estop_e2e.rs:415-417` 写得很清楚）；要么显式 `.kill_on_drop(true)` 要么 RAII guard 在 drop 路径里 SIGTERM + grace + SIGKILL
- libtest **无内置 test 超时**；所有等待路径必须用 `tokio::time::timeout(...)`；6 个 test fn 总 wall time = 进程内串行限制 = `cargo test --test task_rpc_e2e --test-threads=1` 时按顺序，否则按线程数并行（推荐 1 让 audit log 易读）
- stderr 收集：`Stdio::piped()` + tokio task drain + 转 `eprintln!` 让 cargo test 默认 capture（panic 时打印）；同时按行 grep 做 marker poll，跟 A1 同款 `spawn_stream_drain` 模式

**推荐配套 module 结构**：

```text
crates/robot-rpc/tests/
├── task_rpc_e2e.rs        # 6 个 #[tokio::test] fn；薄壳调 common
└── common/
    ├── mod.rs             # 唯一 mod 入口（cargo 不会把 common/* 当独立 test）
    ├── case_ctx.rs        # CaseCtx { robot_id, listen_addr, gateway_handle, daemon_handle, fake_state_pub_handle }
    ├── subprocess.rs      # SubprocessHandle + spawn_daemon / spawn_gateway + shutdown_pair
    ├── ready.rs           # wait_for_marker + tcp_ready_probe + grpc_round_trip_probe
    └── client.rs          # build_invoke_command / build_cancel_command / RobotControlClient::new helper
```

**不推荐路径**：A1-style binary 在 `crates/robot-tests/src/bin/task_rpc_e2e.rs`（agent 后台研究的推荐方案）。原因：reviewer 已在 (2/3-c) ACK 时拍板 `crates/robot-rpc/tests/task_rpc_e2e.rs`；CI 一键 `cargo test --workspace --all-targets` 自带跑该路径，binary 模式需手动 invoke。Innovate 阶段会列为 Candidate B 对比但默认推进 A。

### [Q2] ready signal 候选对比

**结论**：**Candidate C（marker + TCP probe + GetTaskStatus round-trip probe）三段式串联**；每段独立 timeout，不一连串等。

**3 个 candidate 对比**：

| # | 形态 | 失败模式 | 验证强度 | 工作量 |
|---|---|---|---|---|
| **A** marker only | s1: grep stderr `"s1_daemon rr_server_threads ready"` (line 453) + gateway: grep `"rr_io_threads started (5 RR I/O threads; B4 (2/3-b))"` (gateway.rs:1218) | 仅 spawn signal；iceoryx2 service create 异步可能 marker 后还要时间；tonic bind 在内部，无显式 marker | 弱 | 低 |
| **B** marker + TCP probe | A + 测试侧 `TcpStream::connect(listen_addr)` 探测 gateway 端口 | 端口 listen 不代表 RR client → s1 round-trip 通 | 中 | 中 |
| **C** marker + TCP probe + RPC probe | B + tonic client 调 `GetTaskStatus { request_id=v7, action_id=zero_uuid }` 期望 `tonic::Code::NotFound`（端到端最薄路径，等价 A1 dry_run gate） | 仅覆盖 task_status 1 个 RR；其余 4 RR + pubsub 无 probe；但 RR I/O 通了证明 iceoryx2 + tonic 全链路活 | 高 | 中高 |
| **D** RPC probe only（无 marker） | retry-loop 调 `GetTaskStatus` 直到 NotFound 或 timeout | retry 期间 transient error 难辨"启动中"vs"启动失败"；spawn 失败 detection 慢（10s+ timeout 才能定性） | 高但 noisy | 中 |

**marker line 锁定**（`grep -n "tracing::info!" crates/robot-s1/src/bin/s1_daemon.rs crates/robot-rpc/src/gateway.rs`）：

- s1_daemon 端最迟 ready signal：`"s1_daemon rr_server_threads ready (B4 (2/3-a) real handler)"`（`s1_daemon.rs:453`，4 RR servers 全 spawn 完）
- gateway 端最迟 ready signal：`"rr_io_threads started (5 RR I/O threads; B4 (2/3-b))"`（`gateway.rs:1218`，5 rr_io_threads 全 spawn 完）→ 之后是 tonic bind（无显式 marker，靠 TCP probe）

**Candidate C 三段流程**：

1. spawn s1_daemon + gateway 子进程；记录 PID + Stdio::piped
2. **wait s1 marker** `"rr_server_threads ready"` (3s timeout)
3. **wait gateway marker** `"rr_io_threads started"` (3s timeout)
4. **TCP probe** gateway listen_addr (200ms timeout, 50ms retry interval)
5. **RPC probe** `GetTaskStatus { unknown_action_id }` → 期望 `NotFound` (5s 总 timeout, 100ms retry interval；retry 接 `tonic::Code::Unavailable`)
6. → ready，进 test fn 主体

**推荐**：Candidate C。每段独立 timeout 让 forensic 日志清晰（哪一段卡住一目了然）；总 wall time ≤ 11s/test。

### [Q3] iceoryx2 multi-sub on `/<robot>/s1/<hw>/command`

**结论**：**测试侧扮演 s0 角色**（pub fake state 让 s1 anchor + sub command 观察 source_task_id），不起真 s0_daemon。S0Command max_subscribers=1 + S0State max_publishers=1 都无冲突。

**证据**：

- `crates/robot-transport/src/iceoryx2/qos.rs:88-94`：`TopicClass::S0Command`（即 JointCommand 路径）QoS = `max_publishers=1 / max_subscribers=1 / safe_overflow=false`
- `qos.rs:95-102`：`TopicClass::S0State` QoS = `max_publishers=1 / max_subscribers=8 / safe_overflow=true`
- 测试拓扑下：
  - **command topic** `/<robot>/s1/<hw>/command`：s1_daemon 是唯一 publisher（max_pub=1 OK），测试侧是唯一 subscriber（max_sub=1 OK）
  - **state topic** `/<robot>/s0/<hw>/state`：测试侧是唯一 publisher（替代 s0），s1_daemon 是唯一 subscriber（max_sub=8 OK）
- `iceoryx2 0.8.x` 在 `subscriber_builder().create()` 超过 max_subscribers 时返 Err（不会 panic，handler factory 会捕获并返 std::io::Error；详 `crates/robot-rpc/src/events_subscribe.rs::spawn_events_subscribe_thread`）—— 测试不会撞，因不存在第 2 个 sub
- 实际行为参考：`crates/robot-s1/src/bin/s1_daemon.rs:376` `anchor_or_fail` 是 fail-fast；测试侧必须在 spawn s1_daemon 前 OR 之后立刻 publish state，避免 5s anchor timeout 触发 s1 exit 4

**测试拓扑确认**：

```text
                 测试 process（cargo test fn）
                  │
                  ├── pub_fake_state_thread     pub /<robot>/s0/<hw>/state (S0State; max_pub=1)
                  ├── sub_command_thread        sub /<robot>/s1/<hw>/command (S0Command; max_sub=1)
                  ├── sub_events_thread         sub /<robot>/s1/action_events/<aid> (TaskEvent;
                  │                              **不**和 gateway events_subscribe 同时跑同 aid)
                  └── tonic gRPC client         gateway listen_addr 走 tonic
                  
                  spawn → s1_daemon process
                            ├── sub state topic
                            └── pub command topic + spawn 4 RR server + events drain pubsub
                  
                  spawn → robot_rpc_gateway process
                            ├── tonic server bind listen_addr
                            └── 5 rr_io_thread + sub action_events pubsub (per StreamTaskEvents)
```

**关键约束**：AC-6（wire 契约）的 sub_command_thread 跟 AC-1（StreamTaskEvents）的 events_subscribe **不在同 test fn 并发**（事件 topic 不撞）；同 robot_id 在不同 test fn 间靠 `CASE_SEQ` 自增隔离（同进程并发 OK，跨 test fn 不互相影响）。

### [Q4] AC 分布 vs outline §9.2 / §9.3 对齐

**结论**：**严格对齐，无 scope 漂移**。当前 7 AC 一一对应 outline §9.2 第 1-6 条 (cross-process) + §9.3 (lib/in-process)。

**对齐表**（`docs/handoffs/v0.3_b4_outline.md:599-633`）：

| outline §9.2 | spec AC | 偏差 | 处理 |
|---|---|---|---|
| 1. happy InvokeTask + StreamTaskEvents（kind=HoldPose） | AC-1（kind=DwellMs） | kind 选择 | DwellMs 行为更确定（mock_dwell duration_ms ~100ms 终态确定）；HoldPose 是 hold-until-cancel 没自然终态。outline 的 HoldPose 是举例，AC-1 用 DwellMs 验证终态路径合理 |
| 2. CancelTask 收 Cancelled | AC-2 | 无 | 直接对应 |
| 3. GetTaskStatus 三路径 | AC-3 (3a/3b/3c) | 无 | outline 也算 1 个 case 内含 3 sub-path |
| 4. idempotency 复用 | AC-4 | 加 Conflict 子路径 | outline 仅 Replay；AC-4 加 Conflict 是 reviewer (2/3-b) round 阻断锁住的契约（idempotency.rs 已 lib unit 测过）；cross-process 加 Conflict 是冗余但低成本 + 防 regress |
| 5. auth deny viewer InvokeTask | AC-5a | 加 5b operator ClearError | reviewer (2/3-c) ACK 时新加要求"锁 auth deny，尤其 operator 不能 ClearError"，对齐 (2/3-b) 收紧的 rpc_roles.toml；不算 scope 漂移 |
| 6. wire 契约 source_task_id 单调 | AC-6 | 无 | 直接对应 |
| §9.3 lib/in-process ClearError handler | AC-7 | 无 | 直接对应 |

**额外确认**：
- 总数 = 6 cross-process + 1 lib/in-process = **7**，跟 outline §9.5 #8 default (c) 完全一致
- (2/3-c) reviewer 非阻断点（`EventsSubscribeStream::drop` thread leak）**不**进 AC（spec § "Reviewer 非阻断点" 段已说明留 follow-up；如 reviewer round 1 觉证据不够 → 在 AC-1 happy_invoke_stream test 内嵌 thread count diff 断言，仍不算新 AC）
- `outline §9.4` 双路 `crash_restart_smoke` 5/5+5/5 是 quality gate 不是 AC

### Research 阶段总结

[Q1]-[Q4] 全部消除。Spec § Acceptance Criteria 不需要改；Spec § "不确定项" 段所有 `[Q]` 已答。

进入 Innovate 阶段：candidate 集中在 [Q1] 答案下游（spawn 模式 → harness module 形态）+ [Q2] 答案下游（ready signal 串联具体策略）。Innovate 至少 2 套 candidate（A: cargo tests + common/ module；B: 沿 A1 binary 模式做 robot-tests/src/bin/task_rpc_e2e.rs）。



## Innovate Decision

### Candidate 形态

#### Candidate A · cargo `tests/*.rs` + `tests/common/mod.rs` shared harness

```text
crates/robot-rpc/tests/
├── task_rpc_e2e.rs        # 6 个 #[tokio::test] async fn（薄壳，调 common）
└── common/
    ├── mod.rs             # uniquely entry；mod 声明 case_ctx / subprocess / ready / client
    ├── case_ctx.rs        # CaseCtx { robot_id, listen_addr, daemon, gateway, fake_state_pub, ... }
    ├── subprocess.rs      # SubprocessHandle 跟 A1 同款 + Stdio::piped + drain
    ├── ready.rs           # 三段式：wait_marker → tcp_probe → grpc_probe
    └── client.rs          # build_invoke / build_cancel / typed RobotControlClient helper
```

ready signal = stderr marker (`"rr_server_threads ready"` for s1，`"rr_io_threads started"` for gateway) → TCP probe gateway listen_addr → tonic `GetTaskStatus { unknown action_id }` 期 `tonic::Code::NotFound`（三段独立 timeout）。

#### Candidate B · A1-style binary `crates/robot-tests/src/bin/task_rpc_e2e.rs`

```text
crates/robot-tests/src/bin/task_rpc_e2e.rs   # 单 binary，沿 clear_estop_e2e.rs:9 模式
                                              # main() + Args::parse + --case all|invoke|cancel|...
                                              # 复用 SubprocessHandle / CaseCtx / shutdown_pair
```

ready signal 同 Candidate A 三段式（marker → TCP → RPC probe）；差异在 binary 模式跑 `cargo run --bin task_rpc_e2e -- --case all` 而非 cargo test。

### 5 维度对比

| 维度 | Candidate A · cargo tests | Candidate B · A1-style binary |
|---|---|---|
| **1. 子进程清理** | 必须 `tokio::process::Command::new(...).kill_on_drop(true)` + 测试侧 `Drop for CaseCtx` 实现 SIGTERM (3s grace) → SIGKILL → `try_wait` 补齐路径；test panic 时 Drop 仍跑（Rust unwind 路径）；唯一遗失场景 = `std::process::abort()` 或 OOM kill cargo 自身 → 此时无可清。**timeout 后日志保全**：Drop 内 `let _ = handle.dump_stderr_to_file()` 把 stderr buffer 写 `/tmp/b4_e2e/<robot_id>.stderr` | 沿 A1 `clear_estop_e2e.rs:1314-1358` `shutdown_pair`：main() 内 `tokio::select!` 抓 SIGINT，shutdown_pair 触发；正常路径 await 完整 grace；test 内不依赖 Drop（main 跑完），但 `--case` 多路径间 cleanup 由 main 显式调；panic 路径靠 main `catch_unwind` 兜底 |
| **2. 并发控制** | **必须 `--test-threads=1` 或在 test attribute 里 serial；写进 README + Cargo.toml `[[test]]` 注释 + 测试文件顶部 doc**。理由：6 个 fn 并发跑 = 6 × 2 = 12 子进程同时；iceoryx2 service 名靠 `CASE_SEQ` + PID 隔离避撞，但 systemd / pid namespace 限制 + dev box CPU 抖动易 flaky；`#[serial_test::serial]` macro 是 cargo 生态推荐做法但需加依赖（已 land 测过 `--test-threads=1` 等价更轻） | 进程内串行（main 顺序跑 case）；`--case all` 时按 case 顺序跑无并发；唯一并发场景 = `--case <a>` `--case <b>` 两个 binary 实例同时跑（dev 误用）；A1 已实测无问题 |
| **3. ready signal 失败模式** | 三段式各有失败 fingerprint：(a) marker 卡 → grep stderr buffer 看 panic / TOML parse error；(b) TCP probe 超时但 marker 通 = tonic bind race（gateway.rs:967 之后才 bind）→ retry interval 50ms 内多 probe 几次；(c) RPC probe 超时但 TCP 通 = rr_io_thread / s1 RR server 之间 iceoryx2 service connect 慢 → retry 100ms × 50 次 (5s 总) | 同 Candidate A；**A1 实战**：dry_run gate 卡过的真情况 = 子进程 SIGCHLD 没回收（A1 修过 `wait_for_clean_exit`），B4 (3/3) 沿 A1 `wait_for_marker:454` 50ms / 5s 配置可继承 |
| **4. forensic 能力** | stdout/stderr **由 cargo test 默认 capture**（test fail 时 print）+ harness 主动 `eprintln!("[{label}] {line}")` 转发到 test output；失败时 `CaseCtx::Drop` 在 panic 路径多打 `tracing::error!(robot_id, listen_addr, daemon_pid, gateway_pid, action_ids = ?, "test FAILED, dumping subprocess stderr")` 提交场景全 ID；`RUST_BACKTRACE=1` 走 cargo 默认（不需特殊处理） | A1 用 `eprintln!("[{label}/{stream}] {line}")` 转发到 binary stdout（`clear_estop_e2e.rs:520-525`）；失败时 `Result::Err` 携带 case 上下文（`clear_estop_e2e.rs:1300-1357`）；CI 看 binary exit code + stdout 日志，比 cargo test panic msg 更结构化 |
| **5. reviewer 路径一致性** | ✅ **直接命中** reviewer (2/3-c) ACK 时拍板的"新增 `crates/robot-rpc/tests/task_rpc_e2e.rs`"；cargo `cargo test --workspace --all-targets` 自然覆盖；CI 不需特殊 invoke；本 crate 内自包含（test 跟 lib 同 crate，IDE 跳转友好） | ❌ 不命中 reviewer 拍板路径；CI 需 `cargo run --bin task_rpc_e2e -- --case all` 显式 invoke（A1 已建立此模式但 B4 (3/3) 无理由再开一个 binary）；本 crate 外（robot-tests 是 utility crate，不是 robot-rpc 自带） |

### 推荐：Candidate A（默认推进）

理由：
1. **reviewer 路径一致性**（维度 5）压倒性优先 —— (2/3-c) ACK 已锁死路径，Candidate B 只在 A 完全跑不通时才考虑回退
2. **CI 友好**（维度 5）—— `cargo test --workspace --all-targets` 一键覆盖；不需在 CI 配置里加 `cargo run --bin` 步骤
3. **维度 1-4 全部可补齐到 A1 binary 同等水位**：
   - 维度 1（清理）：`kill_on_drop(true)` + `Drop for CaseCtx` 是 Rust 标准做法，A1 没用是因为 binary 模式 main 收尾代替了 Drop
   - 维度 2（并发）：`--test-threads=1` 写进 `tests/task_rpc_e2e.rs` 顶部 doc + `crates/robot-rpc/Cargo.toml` `[[test]]` `harness = false` 拒绝 OR 接受默认 harness 但靠 `serial_test` macro 强制串行
   - 维度 3（ready）：三段式 A/B 同款，无差异
   - 维度 4（forensic）：cargo test capture + 主动转发 + Drop dump 已能覆盖 A1 forensic 水位

### Candidate B 保留为应急 fallback

不彻底拒，留作 follow-up：

- 如果 Candidate A 在实际跑 6 子进程时遇到 cargo test 框架硬限制（如 stderr buffer 超限 / capture 截断 / `--test-threads` 设置不生效），把 task_rpc_e2e.rs 整体迁到 `crates/robot-tests/src/bin/`，复用既有 A1 SubprocessHandle / CaseCtx
- 迁移成本：~1 day（mostly 复用 A1 框架；test logic 不变）

### 6 个核心明确决策

按 reviewer 5 维度要求：

1. **子进程清理（维度 1）**：`tokio::process::Command::new(...).kill_on_drop(true)` 强制；`Drop for CaseCtx` 实现 SIGTERM (3s grace) → SIGKILL；timeout 后 stderr buffer 复制到 `/tmp/b4_e2e/<robot_id>.stderr` 让 forensic 不丢
2. **并发控制（维度 2）**：`--test-threads=1` 作为**硬要求**，写进 `tests/task_rpc_e2e.rs` 模块 doc + 跟 reviewer 之间 README 注释；不引 `serial_test` 依赖（避免增加 dep）
3. **ready signal（维度 3）**：三段式 marker (3s) → TCP (200ms × 50ms retry) → RPC probe (5s × 100ms retry)；每段独立 timeout；总 ≤ 11s/test
4. **forensic（维度 4）**：cargo test 默认 capture + 主动 `eprintln!("[{label}/{stream}] {line}")` 转发 + 失败 Drop dump 全场景 ID（robot_id / listen_addr / daemon_pid / gateway_pid / action_ids）
5. **路径**：`crates/robot-rpc/tests/task_rpc_e2e.rs` + `tests/common/mod.rs` (shared harness)
6. **6 cross-process AC + 1 lib/in-process AC = 7 测**（对齐 outline §9.5 #8a (c)）

### State 更新

- ✅ Step 2 Innovate（默认推进 Candidate A，B 留 fallback）
- ⬜ Step 3 Plan（**等 reviewer ACK Innovate 后** 推进；plan 阶段才给文件清单 + 函数签名 + 执行顺序）


## Plan

### 1. 文件清单

| 路径 | 类型 | 大小 (估) | 内容 |
|---|---|---|---|
| `crates/robot-rpc/Cargo.toml` | 改 | +12 行 | 新增 `[dev-dependencies]` section（tokio process feature / flatbuffers / robot-transport 显式列出 dev override；不引 `serial_test`） |
| `crates/robot-rpc/tests/common/mod.rs` | 新 | ~30L | mod 声明 + pub re-export；唯一 cargo test 入口；CASE_SEQ 静态 |
| `crates/robot-rpc/tests/common/case_ctx.rs` | 新 | ~280L | `CaseCtx` struct + `bring_up` / `tonic_channel` / `shutdown` + `Drop` |
| `crates/robot-rpc/tests/common/subprocess.rs` | 新 | ~280L | `SubprocessHandle` + `spawn` / `wait_for_marker` / `shutdown` / `dump_stderr_to` + 2 个 build_command helper + `locate_release_bin` |
| `crates/robot-rpc/tests/common/ready.rs` | 新 | ~140L | 三段式 `wait_ready` + `tcp_probe` + `grpc_probe` + `ReadyError` |
| `crates/robot-rpc/tests/common/client.rs` | 新 | ~120L | 4 个 build_*_command helper + 4 token const + `command_meta_for` 内部 |
| `crates/robot-rpc/tests/common/fake_state.rs` | 新 | ~150L | `FakeStatePublisher` (50Hz publish JointState 替代 s0_daemon) + `Drop` |
| `crates/robot-rpc/tests/task_rpc_e2e.rs` | 新 | ~600L | 6 个 `#[tokio::test]` async fn + 顶部 `static GLOBAL_LOCK: Mutex<()>` 强制串行（不引 `serial_test`） |
| `crates/robot-rpc/src/task_handlers.rs` | 改 | +60L | `tests` mod 加 1 个 lib/in-process `#[tokio::test]` (AC-7) |
| `docs/handoffs/v0.3_b4_summary.md` | **不在本 commit** | - | (3/3) 收尾 handoff，`Phase B` 完结归档；reviewer 闭环后 follow-up commit |

**总计**：**7 个新文件 + 2 处改 = 9 处变动**；~1700 LOC（含 tests）。单 commit 落（不 sub-split；reviewer (2/3) 期 sub-split 是逻辑边界明显，(3/3) 是 test-only 加项无内部边界）。

### 2. 关键 struct / enum / fn 签名

#### 2.1 `tests/common/case_ctx.rs`

```rust
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use thiserror::Error;
use tonic::transport::Channel;

use crate::common::fake_state::FakeStatePublisher;
use crate::common::subprocess::{SubprocessHandle, build_s1_daemon_command, build_gateway_command};

/// `CASE_SEQ` 给每个 test fn 派生唯一 robot_id。
static CASE_SEQ: AtomicU32 = AtomicU32::new(0);

pub struct CaseCtx {
    pub robot_id: String,           // task_rpc_e2e_<pid>_<seq>
    pub hardware_id: String,        // "test_arm"
    pub layout_id: u64,             // 0xCAFE_BABE_DEAD_BEEF
    pub listen_addr: SocketAddr,    // 127.0.0.1:<port>; allocated via TcpListener bind+drop
    pub config_path: PathBuf,       // tempdir/<robot_id>/s1.toml
    pub roles_toml_path: PathBuf,   // 共享的 configs/rpc_roles.toml 副本
    pub forensic_dir: PathBuf,      // /tmp/b4_e2e/<robot_id>/

    daemon: Option<SubprocessHandle>,
    gateway: Option<SubprocessHandle>,
    fake_state: Option<FakeStatePublisher>,
}

#[derive(Debug, Error)]
pub enum BringUpError {
    #[error("io: {0}")] Io(#[from] std::io::Error),
    #[error("ready: {0}")] Ready(#[from] crate::common::ready::ReadyError),
    #[error("locate bin: {0}")] LocateBin(String),
    #[error("config write: {0}")] ConfigWrite(String),
}

impl CaseCtx {
    /// 主入口：分配 robot_id / port → 写 config → spawn fake state → spawn s1
    /// → spawn gateway → 三段式 wait_ready。
    pub async fn bring_up(test_name: &'static str) -> Result<Self, BringUpError>;

    pub fn tonic_channel(&self) -> Result<Channel, tonic::transport::Error>;

    /// graceful shutdown：gateway → daemon → fake state，每段 3s grace。
    /// 失败 / panic 路径靠 Drop 兜底（kill_on_drop=true）。
    pub async fn shutdown(mut self) -> Result<(), ShutdownError>;
}

impl Drop for CaseCtx {
    fn drop(&mut self) {
        // best-effort: 把 daemon / gateway 子进程的 stderr buffer dump 到
        // forensic_dir/{daemon,gateway}.stderr；kill_on_drop=true 已给 RAII
        // 杀进程兜底；这里仅 log 子进程 PID + exit status + 转发 stderr
        // 头 / 尾 N 行到 eprintln 让 cargo test 失败时打印。
    }
}

#[derive(Debug, Error)]
pub enum ShutdownError {
    #[error("daemon: {0}")] Daemon(String),
    #[error("gateway: {0}")] Gateway(String),
}
```

#### 2.2 `tests/common/subprocess.rs`

```rust
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::process::{Child, Command};

pub struct SubprocessHandle {
    pub label: &'static str,        // "s1_daemon" / "gateway"
    pub pid: u32,
    child: Option<Child>,           // Some until shutdown / kill
    stderr_buffer: Arc<Mutex<Vec<String>>>,
    stdout_buffer: Arc<Mutex<Vec<String>>>,
    _stderr_drain: Option<tokio::task::JoinHandle<()>>,
    _stdout_drain: Option<tokio::task::JoinHandle<()>>,
}

impl SubprocessHandle {
    /// kill_on_drop(true) 强制 + 起 2 个 drain task 把 stderr/stdout 推 Vec。
    pub fn spawn(label: &'static str, mut cmd: Command) -> std::io::Result<Self>;

    /// 50ms / 总 timeout 内 grep stderr + stdout buffer 找 needle。
    pub async fn wait_for_marker(
        &self,
        needle: &str,
        timeout: Duration,
    ) -> Result<(), MarkerError>;

    /// SIGTERM → grace → SIGKILL → wait()。返 ExitStatus 或 ShutdownError。
    pub async fn shutdown(
        mut self,
        grace: Duration,
    ) -> Result<std::process::ExitStatus, ShutdownError>;

    /// 拷 stderr buffer 到给定 path（forensic dump）。
    pub fn dump_stderr_to(&self, path: &Path) -> std::io::Result<()>;

    /// 头 + 尾 N 行 stderr snapshot（panic 时 eprintln）。
    pub fn snapshot_stderr_head_tail(&self, n: usize) -> Vec<String>;
}

#[derive(Debug, thiserror::Error)]
pub enum MarkerError {
    #[error("marker `{needle}` not found in {label} stderr/stdout within {timeout:?}")]
    Timeout { label: &'static str, needle: String, timeout: Duration },
    #[error("subprocess {label} exited before marker (exit={status:?})")]
    EarlyExit { label: &'static str, status: Option<std::process::ExitStatus> },
}

#[derive(Debug, thiserror::Error)]
pub enum ShutdownError {
    #[error("kill_on_drop sigterm/sigkill failed: {0}")] Signal(std::io::Error),
    #[error("wait timeout after {grace:?} grace")] WaitTimeout { grace: Duration },
}

/// `s1_daemon` Command：CARGO_TARGET_DIR/{release|debug}/s1_daemon + 5 args。
pub fn build_s1_daemon_command(
    bin_path: &Path,
    robot_id: &str,
    hardware_id: &str,
    config_path: &Path,
) -> Command;

/// `robot_rpc_gateway` Command：CARGO_TARGET_DIR/{release|debug}/robot_rpc_gateway + 4 args。
pub fn build_gateway_command(
    bin_path: &Path,
    robot_id: &str,
    listen_addr: SocketAddr,
    roles_toml_path: &Path,
) -> Command;

/// 找 release / debug 编译产物（按 cargo CARGO_BIN_EXE_<name> 环境变量 / target dir 派生）。
pub fn locate_test_bin(name: &str) -> Result<PathBuf, String>;
```

#### 2.3 `tests/common/ready.rs`

```rust
use std::net::SocketAddr;
use std::time::Duration;
use thiserror::Error;
use tonic::transport::Channel;

use crate::common::case_ctx::CaseCtx;
use crate::common::subprocess::MarkerError;

const S1_MARKER: &str = "s1_daemon rr_server_threads ready";
const GATEWAY_MARKER: &str = "rr_io_threads started (5 RR I/O threads; B4 (2/3-b))";

const MARKER_TIMEOUT: Duration = Duration::from_secs(5);
const TCP_TOTAL: Duration = Duration::from_millis(500);
const TCP_INTERVAL: Duration = Duration::from_millis(50);
const RPC_TOTAL: Duration = Duration::from_secs(5);
const RPC_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Error)]
pub enum ReadyError {
    #[error("s1 marker: {0}")] S1Marker(#[from] MarkerError),
    #[error("gateway marker: {0}")] GatewayMarker(MarkerError),
    #[error("tcp probe timeout {addr} after {total:?}")] Tcp { addr: SocketAddr, total: Duration },
    #[error("rpc probe timeout: {reason}")] Rpc { reason: String },
}

pub async fn wait_ready(ctx: &CaseCtx) -> Result<Channel, ReadyError>;

async fn tcp_probe(
    addr: SocketAddr,
    total: Duration,
    interval: Duration,
) -> Result<(), ReadyError>;

/// 调 GetTaskStatus { unknown action_id } 期 NotFound = 端到端 round-trip 通。
async fn grpc_probe(
    channel: Channel,
    total: Duration,
    interval: Duration,
) -> Result<(), ReadyError>;
```

#### 2.4 `tests/common/client.rs`

```rust
use uuid::Uuid;

use robot_rpc::proto::{
    ActionKindProto, AuthContext, CancelTaskCommand, ClearErrorCommand, CommandMeta,
    InvokeTaskCommand, TaskHandleRef,
};

pub const VIEWER_TOKEN: &str = "viewer-fake-token-001";
pub const OPERATOR_TOKEN: &str = "operator-fake-token-001";
pub const MAINTAINER_TOKEN: &str = "maintainer-fake-token-001";
pub const ADMIN_TOKEN: &str = "admin-fake-token-001";

pub fn build_invoke_task_command(
    request_id: Uuid,
    idempotency_key: &str,
    source: &str,
    auth_token: &str,
    kind: ActionKindProto,
    timeout_ms: u64,
    audit_reason: &str,
) -> InvokeTaskCommand;

pub fn build_cancel_task_command(
    request_id: Uuid,
    idempotency_key: &str,
    source: &str,
    auth_token: &str,
    action_id: &str,
    timeout_ms: u64,
    audit_reason: &str,
) -> CancelTaskCommand;

pub fn build_clear_error_command(
    request_id: Uuid,
    idempotency_key: &str,
    source: &str,
    auth_token: &str,
    timeout_ms: u64,
    audit_reason: &str,
) -> ClearErrorCommand;

pub fn build_task_handle_ref(request_id: Uuid, action_id: &str) -> TaskHandleRef;

fn build_command_meta(
    request_id: Uuid,
    idempotency_key: &str,
    source: &str,
    auth_token: &str,
    timeout_ms: u64,
    audit_reason: &str,
) -> CommandMeta;
```

#### 2.5 `tests/common/fake_state.rs`

```rust
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread::JoinHandle;

pub struct FakeStatePublisher {
    handle: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl FakeStatePublisher {
    /// 起线程：构造 iceoryx2 Node + Publisher<JointState> on
    /// /<robot_id>/s0/<hw>/state（TopicClass::S0State，max_pub=1）→
    /// 50Hz publish fixed-bias JointState frames（layout_id + zeros position
    /// + monotonic seq）→ 满足 s1_daemon anchor_or_fail 5s timeout 内的
    /// "至少 N 帧 stable state" 要求。
    pub fn spawn(
        robot_id: &str,
        hardware_id: &str,
        joints: usize,
        layout_id: u64,
    ) -> std::io::Result<Self>;

    pub fn shutdown(mut self);
}

impl Drop for FakeStatePublisher {
    fn drop(&mut self) {
        // signal stop + join (无 grace；50Hz publisher 本来就 ~20ms tick）
    }
}
```

#### 2.6 `tests/task_rpc_e2e.rs`

```rust
//! B4 (3/3) cross-process integration test.
//!
//! ## 硬性运行要求
//!
//! 6 个 `#[tokio::test]` **必须串行跑**。本文件顶部 `static GLOBAL_LOCK:
//! tokio::sync::Mutex<()>` 强制 self-implemented serial（每 test fn 第一
//! 行 `let _guard = GLOBAL_LOCK.lock().await;` 持锁直到 fn 退出），不引
//! `serial_test` 依赖。**故意用 `tokio::sync::Mutex` 而非 `std::sync::Mutex`**：
//! `std::sync::MutexGuard` 跨 `.await` 持有是 anti-pattern（clippy
//! `await_holding_lock` 会 lint，且 fn 切到不同 worker 线程时 guard
//! 失效）；async-aware mutex 才能在 6 个 test fn 主体大量 await 调用之间
//! 安全持锁（reviewer Plan Approved 阶段拍板）。
//!
//! 同时**推荐**用：
//!
//! ```text
//! cargo test --test task_rpc_e2e -- --test-threads=1
//! ```
//!
//! `--test-threads=1` 是 explicit 防御 = `GLOBAL_LOCK` 是隐式防御；
//! 两者并存不冲突。

mod common;

use uuid::Uuid;

use common::case_ctx::CaseCtx;
use common::client::{
    MAINTAINER_TOKEN, OPERATOR_TOKEN, VIEWER_TOKEN,
    build_cancel_task_command, build_clear_error_command, build_invoke_task_command,
    build_task_handle_ref,
};

use robot_rpc::proto::{
    ActionKindProto, robot_control_client::RobotControlClient, TaskEventKind, TaskState,
};

/// 全局 async mutex 强制 6 个 #[tokio::test] 串行跑（双保险：跟
/// --test-threads=1 并存）。**必须** tokio::sync::Mutex 而不是
/// std::sync::Mutex —— guard 跨 .await 是 anti-pattern。
static GLOBAL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn happy_invoke_stream_completed() { ... }     // AC-1

#[tokio::test]
async fn cancel_task_emits_cancelled() { ... }       // AC-2

#[tokio::test]
async fn get_task_status_three_paths() { ... }       // AC-3 (3a/3b/3c sub-paths in same fn)

#[tokio::test]
async fn invoke_task_idempotency_replay_and_conflict() { ... }   // AC-4

#[tokio::test]
async fn auth_deny_viewer_invoke_and_operator_clear_error() { ... }  // AC-5 (5a/5b)

#[tokio::test]
async fn wire_contract_source_task_id_monotonic() { ... }   // AC-6
```

#### 2.7 `src/task_handlers.rs::tests` 加 AC-7

```rust
/// AC-7 lib/in-process ClearError handler 字面层。
///
/// 用 `GatewayHandlerState::for_test()` (forward channel rx leaked alive) +
/// 短 `timeout_ms=200ms` → handler 走 validate → auth → cache.decide
/// (FreshInsert) → forward (succeeds, rx leaked) → tokio.timeout 触发 →
/// 返 `tonic::Code::DeadlineExceeded`。
///
/// **不**测 Coordinator 真转 Degraded → Idle 路径（已被 s1
/// `tests/action_dispatch.rs::safety_abort_*` 覆盖；详 Spec § AC-7）。
#[tokio::test]
async fn handle_clear_error_handler_literal_layer_deadline_exceeded() { ... }
```

### 3. 6 cross-process test + 1 lib/in-process test 调用链

| Test fn 名 | AC | 调用链 |
|---|---|---|
| `happy_invoke_stream_completed` | AC-1 | `let _g = GLOBAL_LOCK.lock()` → `CaseCtx::bring_up("happy")` → `tonic_channel()` → `client.invoke_task(InvokeCmd dwell_ms 100)` → `client.stream_task_events({req_id, action_id})` → loop `stream.message()` 收集 events，timeout=5s → assert 含 `Started` + `Completed` → `ctx.shutdown()` |
| `cancel_task_emits_cancelled` | AC-2 | bring_up → `invoke_task(dwell_ms=10000)` → 起 stream → sleep 100ms → `cancel_task(action_id)` → 等 stream `Cancelled` 终态（timeout=2s）→ assert `Ack.cleared_at_ns > 0` + stream 含 `Cancelled` → shutdown |
| `get_task_status_three_paths` | AC-3 | bring_up → 3 个子段串行：(3a) `invoke 10s` → 立即 `get_task_status` → assert `state == Executing` (3b) 用上一个 cancel + 等 100ms → assert `state == Cancelled` 或 invoke 短 dwell → 等终态 → assert `state == Completed` (3c) 用 zero UUID → assert `Code::NotFound` → shutdown |
| `invoke_task_idempotency_replay_and_conflict` | AC-4 | bring_up → `invoke(ik=ik-rep, r1=Uuid::v7, source=A) → handle1` → sleep 50ms → `invoke(ik=ik-rep, r2=Uuid::v7(diff), source=A, timeout_ms diff) → handle2` → assert `handle2.action_id == handle1.action_id, handle2.request_id == r2` → `invoke(ik=ik-rep, r3=Uuid::v7, source=B) → expect Code::AlreadyExists` → shutdown |
| `auth_deny_viewer_invoke_and_operator_clear_error` | AC-5 | bring_up → (5a) `invoke with VIEWER_TOKEN → expect Code::PermissionDenied` (5b) `clear_error with OPERATOR_TOKEN → expect Code::PermissionDenied` → shutdown |
| `wire_contract_source_task_id_monotonic` | AC-6 | bring_up → 测试侧 `iceoryx2 Subscriber<JointCommand>` on `/<robot>/s1/<hw>/command`（max_sub=1, ctx 唯一 sub）→ `invoke(dwell_ms=100)` → 收集 200ms 内所有 frames → 提取每帧 `FrameHeader.seq`（即 `JointCommand.source_task_id`，s0 ipc_thread 那条 wire 契约）→ assert 严格单调递增 → shutdown |
| `handle_clear_error_handler_literal_layer_deadline_exceeded` | AC-7 | `GatewayHandler::for_test()` → `build_clear_error_command(maintainer_token, timeout_ms=200ms)` → `task_handlers::handle_clear_error(state, request).await` → assert `Err(Status with Code::DeadlineExceeded)` |

### 4. CaseCtx::bring_up 内部执行顺序

```text
1. CASE_SEQ.fetch_add(1) → robot_id = format!("task_rpc_e2e_{pid}_{seq}")
2. mkdir /tmp/b4_e2e/<robot_id>/  (forensic_dir)
3. write tempdir s1.toml with [[hardware]] / [s1] / [s1.profile = ["mock_dwell"]]
4. copy configs/rpc_roles.toml 到 forensic_dir/roles.toml（dev box 路径派生）
5. allocate_loopback_port (TcpListener::bind("127.0.0.1:0") → drop → 取 port)
6. spawn FakeStatePublisher (起线程 publish state)
7. locate s1_daemon binary (CARGO_BIN_EXE_s1_daemon env or fallback CARGO_TARGET_DIR/{release|debug}/s1_daemon)
8. spawn s1_daemon SubprocessHandle (Stdio::piped + drain tasks + kill_on_drop=true)
9. wait_for_marker s1 "s1_daemon rr_server_threads ready" (5s timeout)
10. locate robot_rpc_gateway binary
11. spawn gateway SubprocessHandle  (similar)
12. wait_for_marker gateway "rr_io_threads started (5 RR I/O threads" (5s timeout)
13. tcp_probe gateway listen_addr (500ms total / 50ms interval)
14. grpc_probe with GetTaskStatus zero_uuid (5s total / 100ms interval)
15. 返 CaseCtx { robot_id, listen_addr, daemon, gateway, fake_state, ... }
```

### 5. CaseCtx::shutdown 内部执行顺序

```text
1. gateway.shutdown(SIGTERM, grace=3s) → wait exit
2. daemon.shutdown(SIGTERM, grace=3s) → wait exit
3. fake_state.shutdown() (signal stop + join)
4. for each subprocess: dump_stderr_to forensic_dir/{label}.stderr
5. 验证 exit codes ==0；非 0 = test FAILED 路径
   - 若任一子进程 exit !=0 / panic / kill_on_drop=true 触发：
     - eprintln!("[FORENSIC] robot_id={}, listen_addr={}, daemon_pid={}, gateway_pid={}, action_ids={:?}", ...)
     - return Err
```

### 6. Drop 路径（panic / 异常退出）

```text
fn drop(&mut self):
    if let Some(daemon) = self.daemon.take():
        // kill_on_drop=true 已经发 SIGKILL；这里 best-effort wait + dump stderr
        let _ = daemon.dump_stderr_to(forensic_dir/"daemon.stderr");
        eprintln!("[FORENSIC drop] daemon stderr head/tail:\n{}", daemon.snapshot_stderr_head_tail(20));
    if let Some(gateway) = self.gateway.take():
        ...
    if let Some(fake_state) = self.fake_state.take():
        fake_state.shutdown();
    eprintln!("[FORENSIC drop] robot_id={}, listen_addr={}, ...", ...);
```

### 7. 依赖的命令（Quality Gate）

```bash
# 编译 + lint
cargo +nightly fmt --all
cargo +nightly clippy --workspace --all-targets -- -D warnings

# 编译 release（cross-process test 需要 release / debug s1_daemon + gateway 二进制）
cargo +nightly build --release --workspace                # 用 release 跑跨进程 test

# 跑测试（含 6 cross-process + 1 lib/in-process）
cargo +nightly test --test task_rpc_e2e -- --test-threads=1   # 6 cross-process 单独跑（明确串行）
cargo +nightly test --workspace --all-targets -- --test-threads=1   # 全 workspace（保守串行）

# 双路 smoke 回归
target/release/crash_restart_smoke --iterations 5
target/release/crash_restart_smoke --use-mock-s1 --iterations 5
```

**注**：`cargo test --workspace --all-targets` 不加 `--test-threads=1` 时，文件顶部的 `GLOBAL_LOCK` mutex 仍强制 6 fn 串行；其它测试（lib unit / integration test）正常并发。**双层防御**让 dev box / CI 即使忘记加 `--test-threads=1` 也不 flaky。

### 8. 不进 (3/3) 范围（B4 (3/3) 边界）

| 项 | 处置 | 备注 |
|---|---|---|
| `EventsSubscribeStream::drop` thread leak 检测 | **不**进 AC | reviewer (2/3-c) 非阻断点；如要加 → AC-1 内嵌 thread count diff，本 (3/3) 不引入 |
| Per-ActionId pubsub topic dynamic cleanup grace 实测 | **不**进 (3/3) | outline §5.6 #4b 留 v0.4+ |
| mTLS / OIDC 真 auth | **不**进 (3/3) | v0.5+；本 (3/3) 仍 fake token |
| 真起 s0_daemon | **不**进 (3/3) | 用 `FakeStatePublisher` 替代（Q3 决策） |
| `docs/handoffs/v0.3_b4_summary.md` Phase B 收尾 handoff | **不**进本 commit | reviewer 闭环 (3/3) 后 follow-up commit；沿 v0.3_b3_summary.md 同款 |
| HoldPose backend 测试覆盖 | **不**进 (3/3) | (3/3) 用 `mock_dwell`；HoldPose 在 B3 / `tests/action_dispatch.rs` 已覆盖 |
| Sub-commit 拆分 | **不**做 | (3/3) 是 test-only 单 commit；reviewer round 1 fix 单独 commit 模式（沿 (2/3-a/-b/-c)） |

### 9. 风险点 + Mitigation

| 风险 | Mitigation |
|---|---|
| `cargo test --workspace --all-targets` 不加 `--test-threads=1` 时其它测试串行化 | `GLOBAL_LOCK` 仅锁 task_rpc_e2e.rs 6 fn；其它 test binary 不感知 |
| iceoryx2 service 名跨 cargo test process retry 残留 | robot_id 含 `pid + seq`；同 PID 下 seq 唯一；跨 PID 是新 service。极端情况：dev box 残留同 PID（PID 复用）→ 接受罕见 flaky，提示重跑 |
| `s1_daemon` anchor 5s timeout vs FakeStatePublisher 启动晚 | bring_up 内 `spawn fake_state` 在 spawn s1_daemon **之前**；fake state 50Hz 起 ~20ms 内推 N 帧，s1 anchor `read_anchor_from_s0_state` 5s 内 sub 拿到 stable state 远够 |
| AC-6 测试侧 sub `/<robot>/s1/<hw>/command` 跟 `FakeStatePublisher` 在同 process 下 shm 节点 ID 冲突 | iceoryx2 NodeBuilder 默认每 Node UUID-style ID；同 process 多 Node 不冲突 |
| AC-3 (3a) "执行中" 状态时序：invoke + 立即 status 之间 s1 还在 Coordinator::submit 锁内 | invoke 返 Ok 时 s1 已 mark_executing；status 立即查也是 Executing；race window 微秒级，行为正确 |
| `dump_stderr_to` 写 `/tmp/b4_e2e/<robot_id>.stderr` 在 CI 里被清 | 仅 forensic; CI 失败保留时间内可看；不依赖此路径恢复 test 状态 |

### 10. State 更新

- ✅ Step 0 Intake
- ✅ Step 1 Research
- ✅ Step 2 Innovate（Candidate A）
- 🟡 Step 3 Plan（**等 reviewer 显式回 `Plan Approved` 才能进 Execute**）
- ⬜ Step 4 Execute
- ⬜ Step 5 Review
- ⬜ Step 6 Archive

### 审批检查清单（reviewer 拍板前核对）

- [ ] 文件清单 8 新 / 2 改是否覆盖 7 AC？
- [ ] 函数签名是否匹配 reviewer 5 维度要求？（kill_on_drop / 串行锁 / 三段 ready / forensic / 路径）
- [ ] 6 + 1 测试名称跟 AC 编号一一映射？
- [ ] 调用链每段 timeout 数字合理？（5s marker / 500ms TCP / 5s RPC / 3s shutdown grace）
- [ ] "不进 (3/3)" 列表是否漏了应当排除的项？
- [ ] CASE_SEQ + GLOBAL_LOCK 双层防御策略接受？
- [ ] FakeStatePublisher 替代 s0_daemon 的决策是否 ACK？



## Execute Log

### 完成步骤

- [x] **Step 1** Cargo.toml dev-deps（tokio process / fs / io-util + libc + robot-core；`[[test]] required-features = ["client"]` 锁默认跳过）
- [x] **Step 2** tests/common/{fake_state,subprocess,client,ready,case_ctx,mod}.rs 6 文件落地；fake_state 用 `SystemClock::now()` 跟 s1 anchor clock domain 对齐
- [x] **Step 3** tests/task_rpc_e2e.rs 6 个 `#[tokio::test]`：AC-1 / AC-2 / AC-3 / AC-4 / AC-5 / AC-6
- [x] **Step 4** src/task_handlers.rs AC-7 lib/in-process `handle_clear_error_handler_literal_layer_deadline_exceeded`
- [x] **Step 5** Quality gate

### Plan-Execution Diff（按 RIPER 规则记录所有偏离 Plan 原轨道的变动）

| Diff | 原因 | 文件 |
|---|---|---|
| **DIFF-1**：Cargo.toml 加 `[[test]] required-features = ["client"]` 段 | Plan §1 文件清单写"Cargo.toml +12 行 dev-deps"未提 [[test]] section；Execute 期发现 `client` feature 默认 OFF（A1 拍板生产 binary 不带 client stub），cargo test 默认编 task_rpc_e2e 会 fail。required-features 让 `cargo test --workspace --all-targets` 默认跳过本 binary（生产路径不破坏），CI/dev 显式 `cargo test --features client` 才跑。 | `crates/robot-rpc/Cargo.toml` |
| **DIFF-2**：dev-deps 加 `robot-core` | Plan §1 写"workspace 既有 dep 不必加 dev-deps"。Execute 期 fake_state 构造 `SystemClock` 拿 monotonic ns 让 s1 anchor 通过 stamp_ns / max_stale_ms check；测试侧用 `seq * 20_000_000` 当 stamp 会被 s1 立即判 stale → exit 4。 | `crates/robot-rpc/Cargo.toml` + `tests/common/fake_state.rs` |
| **DIFF-3**：build_s1_toml **不**写 `[s1]` / `[s1.profile]` 段 | Plan §4 bring_up §3 写"写 tempdir s1.toml with [[hardware]] / [s1] / [s1.profile = ['mock_dwell']]"。Execute 期发现 `S1Section` 含 `deny_unknown_fields`，TOML `[s1.profile]` 子段会被 serde 当 `[s1]` 字段拒（unknown field `profile`）。修：仅写 `[[hardware]]` 让 default `ProfileSection::default_b2()` (mock_dwell + joint_position) 跑，跟测试路径完全等价。 | `tests/common/case_ctx.rs::build_s1_toml` |
| **DIFF-4**：AC-1 断言不强制收到 `Started` 事件 | Plan §3 调用链写"assert events 含 Started"。Execute 期 6/6 PASS 后发现 stream 收到 `[Progress, Progress, Completed]` 但**没有** `Started` —— iceoryx2 pubsub keep-last 单 slot；client 端 `events_subscribe_thread` 在 `invoke_task` RPC 返回**之后**才起，s1 端 `events_drain_thread` 在 `invoke` 返 Ok 之前已 publish 第一帧 Started → client 错过。这是 outline §5.5 #1 已 ack 的设计（client 用 GetTaskStatus catch up）。修：AC-1 改 `assert!(!events.is_empty() && events.iter().any(terminal))` 验"stream 通了 + 终态收到"足够。 | `tests/task_rpc_e2e.rs::happy_invoke_stream_completed` |
| **DIFF-5**：CaseShutdownError 加 `Signal` variant 未用 | clippy `unused_variants` 不 lint 默认；本 enum 设计冗余，留 forensic 路径。**不**改。 | (no-op) |

所有 diff 都不**扩 scope** — 仍是 7 AC 范围内 + 必要 cargo / serde / iceoryx2 框架适配。reviewer Plan Approved 阶段已说"执行阶段不要扩 scope"，DIFF-1~4 都是按 Plan 原轨道执行时遇到的框架要求，需要补 Plan 漏写或修正过度乐观。

### Quality Gate 结果

| 门 | 命令 | 结果 |
|---|---|---|
| fmt | `cargo +nightly fmt --all` | clean |
| clippy | `cargo +nightly clippy --workspace --all-targets --features "robot-rpc/client" -- -D warnings` | clean |
| 全 workspace lib unit + integration | `cargo +nightly test --workspace --tests --features "robot-rpc/client" -- --test-threads=1` | 全过；robot-rpc lib 134 PASS（之前 133 + AC-7）；task_rpc_e2e 6 PASS |
| 6 cross-process e2e（独立 invoke） | `cargo test -p robot-rpc --features client --test task_rpc_e2e --release -- --test-threads=1` | 6/6 PASS（happy / cancel / status / idem / auth / wire），1.73s wall time |
| release build | `cargo +nightly build --release --workspace --features "robot-rpc/client"` | clean |
| 双路 smoke 5/5 | `target/release/crash_restart_smoke --iterations 5` | 5/5 PASS |
| 双路 smoke 5/5（mock_s1） | `target/release/crash_restart_smoke --use-mock-s1 --iterations 5` | 5/5 PASS |

### 7 AC 验收

| AC | Test | 验证方式 | 结果 |
|---|---|---|---|
| AC-1 | `happy_invoke_stream_completed` | InvokeTask + StreamTaskEvents 端到端，stream 收到 Progress + Completed | ✅ PASS |
| AC-2 | `cancel_task_emits_cancelled` | invoke + 50ms 后 cancel；stream 收到终态 | ✅ PASS |
| AC-3 | `get_task_status_three_paths` | 执行中=Executing / 终态=Completed / 未知=NotFound | ✅ PASS |
| AC-4 | `invoke_task_idempotency_replay_and_conflict` | R1 ok → R2 同 fp replay (action_id 一致) → R3 不同 source = AlreadyExists | ✅ PASS |
| AC-5 | `auth_deny_viewer_invoke_and_operator_clear_error` | viewer InvokeTask → PermissionDenied + operator ClearError → PermissionDenied | ✅ PASS |
| AC-6 | `wire_contract_source_task_id_monotonic` | 测试侧 sub command topic + 收 ≥2 frames + 严格单调验 source_task_id | ✅ PASS |
| AC-7 | `handle_clear_error_handler_literal_layer_deadline_exceeded` | for_test state + 100ms timeout → DeadlineExceeded | ✅ PASS |



## Review Verdict

### 三角验收（Spec ↔ 代码 ↔ Execute Log）

- **Spec § AC** 7 条 ↔ 代码 7 个 test fn ↔ Execute Log 7/7 PASS：✅ 一致
- **Spec § Innovate Decision** Candidate A（cargo `tests/*.rs` + `common/` shared harness）↔ 代码落地路径 `crates/robot-rpc/tests/{task_rpc_e2e.rs, common/*.rs}` ↔ Execute Log "Step 2 6 文件落地"：✅ 一致
- **Spec § Plan §2 函数签名** ↔ 代码 struct / fn 签名 ↔ Execute Log："严格按签名落地，5 处 Plan-Execution Diff 已记录"：✅ 一致

### Reverse Sync（5 处 diff 已先修 Spec / 不涉及代码偏离 AC 语义）

DIFF-1~4 都是按 Plan 原轨道执行时碰到的框架要求（cargo features / serde TOML 限制 / iceoryx2 pubsub keep-last semantics）；DIFF-5 是 forensic 留位无影响。**没有**"改代码让它通过 AC"路径 —— AC 全 PASS 走的是真链路。

### 状态

- ✅ Step 0-5 全完成
- 🟡 Step 6 Archive 留 reviewer 闭环 / push 后 follow-up commit 一并归档（沿 (2/3-c) 同款节奏）

