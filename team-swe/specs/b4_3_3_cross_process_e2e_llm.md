# B4 (3/3) cross-process e2e — LLM 接手切片

> 给"下次接手 robot-rpc cross-process integration test 的 AI"用：在尽
> 量短的篇幅里压缩**项目背景 + 数据结构 + 关键决策 + 易错点**，让接手
> AI 能跳过重新研究就直接干活。
>
> 完整 Spec 见 `b4_3_3_cross_process_e2e.md`；Phase B summary 见
> `docs/handoffs/v0.3_b4_summary.md`；CodeMap 见 `team-swe/codemap/b4_3_3.md`。

## 1. 项目坐标

- repo：`/home/loosqk/repo/robot/robot-system`（Rust nightly + edition
  2024 + workspace）
- crate：`crates/robot-rpc`（v0.3 A1 ClearEStop control plane gateway
  + B4 5 个 task control plane RPC）
- 当前状态：v0.3 Phase B 完结（B1+B2+B3+B4 全 land；commit chain 末端
  = `2c68a4e` handoff doc）
- 路径锁定：cargo `tests/*.rs` integration test 在
  `crates/robot-rpc/tests/{task_rpc_e2e.rs, common/*.rs}`
- feature gate：`client`（默认 OFF；`[[test]] required-features = ["client"]`
  让 cargo test 默认跳过本 binary）

## 2. 测试拓扑

```text
test 进程（cargo test fn）
  ├── pub fake state    → /<robot>/s0/<hw>/state    (S0State, max_pub=1)
  ├── sub command       → /<robot>/s1/<hw>/command  (S0Command, max_sub=1)  [仅 AC-6 用]
  └── tonic client      → http://127.0.0.1:<port>

spawn → s1_daemon (kill_on_drop=true)
          ├── sub state        (max_sub=8 of S0State)
          ├── pub command      (max_pub=1 of S0Command)
          ├── 4 RR server      (/<robot>/control/{task_invoke,task_cancel,task_status,clear_error})
          └── per-ActionId pubsub publisher  (TaskEvent class)

spawn → robot_rpc_gateway (kill_on_drop=true)
          ├── tonic server bound 127.0.0.1:<port>
          ├── 5 rr_io_thread   (5 RR client × 5 topic)
          └── per-StreamTaskEvents subscribe thread (TaskEvent sub)
```

**不**起真 s0_daemon。FakeStatePublisher 替代是为通过 s1 `anchor_or_fail`
（必须在 5s 内拿到 `max_stale_ms=200ms` 内的 state），用
`robot_core::time::SystemClock::now()` 拿 monotonic ns 跟 s1 同 clock
domain。

## 3. 7 AC 一一映射 test fn

| AC | test fn | 关键断言 |
|---|---|---|
| 1 | `happy_invoke_stream_completed` | invoke ok + stream 非空 + 终态收到（**不**强制收到 Started — pubsub keep-last race） |
| 2 | `cancel_task_emits_cancelled` | invoke + cancel + stream 收终态 ≤ 200ms |
| 3 | `get_task_status_three_paths` | Executing / 终态 / NotFound |
| 4 | `invoke_task_idempotency_replay_and_conflict` | R1 → R2 同 fp action_id 一致 → R3 不同 source = AlreadyExists |
| 5 | `auth_deny_viewer_invoke_and_operator_clear_error` | viewer + operator 两路 PermissionDenied |
| 6 | `wire_contract_source_task_id_monotonic` | sub command topic + ≥2 frames 严格单调 |
| 7 | `handle_clear_error_handler_literal_layer_deadline_exceeded` (lib unit in `src/task_handlers.rs::tests`) | for_test state + 100ms timeout → DeadlineExceeded |

## 4. 关键数据结构（tests/common/）

```rust
// case_ctx.rs
pub struct CaseCtx {
    pub robot_id: String,           // task_rpc_e2e_<pid>_<seq>_<test_name>
    pub hardware_id: &'static str,  // "test_arm"
    pub layout_id: u64,             // 0xCAFE_BABE_DEAD_BEEF
    pub joints: usize,              // 1
    pub listen_addr: SocketAddr,    // 127.0.0.1:<allocated>
    pub config_path: PathBuf,       // /tmp/b4_e2e/<robot_id>/s1.toml
    pub roles_toml_path: PathBuf,   // /tmp/b4_e2e/<robot_id>/rpc_roles.toml
    pub forensic_dir: PathBuf,      // /tmp/b4_e2e/<robot_id>/
    pub channel: tonic::transport::Channel,
    daemon: Option<SubprocessHandle>,
    gateway: Option<SubprocessHandle>,
    fake_state: Option<FakeStatePublisher>,
}
impl CaseCtx {
    pub async fn bring_up(test_name: &'static str) -> Result<Self, BringUpError>;
    pub fn tonic_channel(&self) -> Result<Channel, ...>;
    pub async fn shutdown(self) -> Result<(), CaseShutdownError>;
}
impl Drop for CaseCtx { ... }  // RAII 兜底 + forensic log

// subprocess.rs
pub struct SubprocessHandle { label, pid, child, captured }
impl SubprocessHandle {
    pub fn spawn(label, cmd) -> io::Result<Self>;  // kill_on_drop=true + 2 drain tasks
    pub async fn wait_for_marker(&mut self, needle, timeout) -> Result<(), MarkerError>;
    pub async fn shutdown(self, grace) -> Result<ExitStatus, ShutdownError>;  // SIGTERM → grace → SIGKILL
    pub async fn dump_captured_to(&self, path) -> io::Result<()>;
    pub async fn snapshot_head_tail(&self, n) -> Vec<String>;
}

// ready.rs
const S1_READY_MARKER: &str = "s1_daemon rr_server_threads ready";
const GATEWAY_READY_MARKER: &str = "rr_io_threads started (5 RR I/O threads; B4 (2/3-b))";
const MARKER_TIMEOUT: Duration = 5s;
const TCP_TOTAL: Duration = 500ms; const TCP_INTERVAL: Duration = 50ms;
const RPC_TOTAL: Duration = 5s;    const RPC_INTERVAL: Duration = 100ms;
pub async fn wait_ready(s1, gw, addr) -> Result<Channel, ReadyError>;

// client.rs
pub const VIEWER_TOKEN: &str = "viewer-fake-token-001";
pub const OPERATOR_TOKEN: &str = "operator-fake-token-001";
pub const MAINTAINER_TOKEN: &str = "maintainer-fake-token-001";
pub const ADMIN_TOKEN: &str = "admin-fake-token-001";
pub fn build_invoke_task_command(...) -> InvokeTaskCommand;
pub fn build_cancel_task_command(...) -> CancelTaskCommand;
pub fn build_clear_error_command(...) -> ClearErrorCommand;
pub fn build_task_handle_ref(...) -> TaskHandleRef;

// fake_state.rs
pub struct FakeStatePublisher { handle, stop, topic }
impl FakeStatePublisher {
    pub fn spawn(robot_id, hw_id, joints, layout_id) -> io::Result<Self>;
    pub fn shutdown(self);
}
impl Drop for FakeStatePublisher { ... }
```

## 5. 易错点（v0.4+ 接手前必看）

1. **GLOBAL_LOCK 必须 `tokio::sync::Mutex`，不是 `std::sync::Mutex`**：
   `MutexGuard` 跨 `.await` 是 anti-pattern；clippy `await_holding_lock`
   会 lint，且 fn 切到不同 worker 线程时 guard 失效。reviewer Plan
   Approved 阶段拍板。

2. **测试 toml 不写 `[s1]` / `[s1.profile]` 段**：`S1Section` 含
   `deny_unknown_fields`，跟 nested table `[s1.profile]` 冲突
   （serde 把 profile 当 [s1] 字段拒）。仅写 `[[hardware]]` 让
   `default_b2` (mock_dwell + joint_position) 跑，等价测试路径。

3. **FakeStatePublisher 必须用真 monotonic ns**：用 `seq * 20_000_000`
   当 fake stamp_ns 会被 s1 `max_stale_ms=200ms` 立即判 stale → s1
   anchor exit 4。改用 `robot_core::time::SystemClock::now()`。

4. **AC-1 不强制收 Started 事件**：iceoryx2 pubsub keep-last 单 slot；
   client 端 `events_subscribe_thread` 在 `invoke_task` RPC 返回**之
   后**才起，必然错过第一帧 Started。outline §5.5 #1 已 ack。**仅**
   验"stream 非空 + 终态收到"。

5. **`[[test]] required-features = ["client"]`**：A1 拍板生产
   binary **不**带 tonic client stub（client 在 dimos / robotctl 端
   起，不在 robot-system workspace 内）。required-features 让
   `cargo test --workspace --all-targets` 默认跳过本 binary，CI 显
   式 `cargo test --features client --test task_rpc_e2e -- --test-threads=1`
   才跑。**不**改成 `default = ["client"]`。

6. **iceoryx2 service 名隔离 = `robot_id_<pid>_<seq>_<test_name>`**：
   缺 pid 会让跨 cargo test process 重跑碰 service 残留；缺 seq 会
   让同进程并发碰；都缺会撞 max_pub/max_sub 配额。CASE_SEQ 是
   `static AtomicU32`。

7. **shutdown 必须先 SIGTERM 后 grace**：tokio::process::Child::kill
   是硬 SIGKILL，没 grace。用 `libc::kill(pid, SIGTERM)` + 3s wait
   + SIGKILL fallback。`kill_on_drop(true)` 是 Drop 路径的兜底，**不**
   是正常路径的 graceful shutdown。

8. **AC-6 测试侧 sub command topic = max_sub=1**：测试拓扑下不起真
   s0_daemon → 测试侧是唯一 sub，配额 OK。如果改架构让 test + 真
   s0 共存，max_sub=1 会撞。

9. **B3 wire 契约（source_task_id 单调）跨进程仍要保**：B4 的 RPC
   handler / events_drain / events_subscribe 任一环节都**不**得重写
   `JointCommand.source_task_id`；wire 由 s1 coordinator
   `TickState.cmd_seq` 单一 source 驱动。详 `docs/handoffs/v0.3_b3_summary.md`
   §3 +  `docs/handoffs/v0.3_b4_summary.md` §3.1。

10. **不引 `serial_test` dep**：reviewer Innovate 阶段拍板。用
    `tokio::sync::Mutex` 自实现串行 + `--test-threads=1` 双层防御。

## 6. 命令速查

```bash
# 编译 + lint（dev / CI 必跑）
cargo +nightly fmt --all
cargo +nightly clippy --workspace --all-targets --features "robot-rpc/client" -- -D warnings

# build release（cross-process e2e 必须 release，s1_daemon / gateway 二进制）
cargo +nightly build --release --workspace --features "robot-rpc/client"

# 跑 cross-process e2e 6 个（独立）
cargo +nightly test -p robot-rpc --features client --test task_rpc_e2e --release -- --test-threads=1

# 跑全 workspace tests（默认跳 task_rpc_e2e；显式 features 才跑）
cargo +nightly test --workspace --tests --features "robot-rpc/client" -- --test-threads=1

# 双路 smoke（reviewer wiring 回归门槛）
target/release/crash_restart_smoke --iterations 5
target/release/crash_restart_smoke --use-mock-s1 --iterations 5
```

## 7. 关键 commit / file 锚点

| 主题 | commit / 文件 |
|---|---|
| (3/3) e2e 实现 | `f52daf8` |
| handoff doc | `2c68a4e` |
| 6 cross-process test | `crates/robot-rpc/tests/task_rpc_e2e.rs` |
| harness 6 文件 | `crates/robot-rpc/tests/common/{mod,case_ctx,subprocess,ready,client,fake_state}.rs` |
| AC-7 lib unit | `crates/robot-rpc/src/task_handlers.rs::tests::handle_clear_error_handler_literal_layer_deadline_exceeded` |
| dev-deps + [[test]] required-features | `crates/robot-rpc/Cargo.toml` |
| Phase B 完结 summary | `docs/handoffs/v0.3_b4_summary.md` |

## 8. RIPER 流程产物

按 team-swe skill 走完整 RIPER：
- Step -1 Pre-Research：CodeMap `team-swe/codemap/b4_3_3.md` (106L)
- Step 0 Intake：Spec § AC（7 条）
- Step 1 Research：[Q1]-[Q4] 全消除（cargo test 模式 / ready signal /
  iceoryx2 multi-sub / outline §9 对齐）
- Step 2 Innovate：Candidate A (cargo tests) vs Candidate B (binary)
  5 维度对比 + 默认推进 A
- Step 3 Plan：原子级清单（10 段）+ reviewer Plan Approved
- Step 4 Execute：5 处 Plan-Execute Diff 全记录在 § Execute Log
- Step 5 Review：三角验收（Spec ↔ 代码 ↔ Execute Log）+ Reverse Sync
  无破坏 AC 路径
- Step 6 Archive：本文 (`_llm.md`) + `_human.md` 落盘
