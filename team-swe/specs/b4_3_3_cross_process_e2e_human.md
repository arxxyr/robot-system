# B4 (3/3) cross-process e2e — Human-readable summary

> 给 reviewer / 同事审阅用。完整推导见
> [`b4_3_3_cross_process_e2e.md`](b4_3_3_cross_process_e2e.md)；CodeMap
> 见 [`../codemap/b4_3_3.md`](../codemap/b4_3_3.md)。
>
> 状态：reviewer ACK + push（commit `f52daf8`）；handoff doc 单独
> commit `2c68a4e` 归档。

## 做了什么

为 v0.3 B4 task control plane（5 RPC：InvokeTask / CancelTask /
GetTaskStatus / StreamTaskEvents / ClearError）写 cross-process
integration test，证 5 RPC + s1 RR server + events_drain pubsub +
gateway 5 rr_io_thread + events_subscribe 全链路在跨进程下正确工作。

**交付**：`crates/robot-rpc/tests/task_rpc_e2e.rs` + `tests/common/`
shared harness + 1 个 lib/in-process AC-7 test + 双路 smoke 5/5+5/5
回归。

## 7 个 AC

| AC | 验证什么 | 怎么验 |
|---|---|---|
| 1 | InvokeTask + StreamTaskEvents 端到端 | invoke DwellMs → stream 收到 Progress + Completed 终态 |
| 2 | CancelTask happy + Cancelled 终态 | invoke 5s timeout → 50ms 后 cancel → stream 收到终态（≤ 200ms） |
| 3 | GetTaskStatus 三路径 | 执行中=Executing / 终态=Completed / 未知 ID=NotFound |
| 4 | Idempotency replay + conflict | 同 ik+不同 request_id+不同 timeout = Replay (action_id 一致)；同 ik+不同 source = AlreadyExists |
| 5 | Auth deny | viewer InvokeTask + operator ClearError 都返 PermissionDenied（B4 (2/3-b) 收紧后） |
| 6 | Wire 契约 source_task_id 单调 | 测试侧 sub command topic + ≥2 frames 严格单调（B3 §3.7 锁定） |
| 7 | ClearError handler 字面层 | for_test state + 100ms timeout → DeadlineExceeded（lib unit；不进 cross-process） |

## 关键决策 5 条

1. **路径 = `crates/robot-rpc/tests/task_rpc_e2e.rs`**（cargo
   `tests/*.rs` integration test）— 而非 A1-style binary 在
   `crates/robot-tests/src/bin/`。理由：CI `cargo test --workspace
   --all-targets` 自然覆盖；`[[test]] required-features = ["client"]`
   让默认跳过、显式 `--features client` 才跑（不破生产 binary 不带
   client stub 的拍板）。Candidate B (binary 模式) 留作 fallback 不
   被默认采用。
2. **6 个 `#[tokio::test]` 强制串行**：`static GLOBAL_LOCK:
   tokio::sync::Mutex<()>` + 推荐 `--test-threads=1` 双层防御。**故
   意**用 `tokio::sync::Mutex` 而非 `std::sync::Mutex`（guard 跨
   `.await` 是 anti-pattern）。
3. **测试侧扮演 s0 角色**：起 `FakeStatePublisher` 50Hz publish
   JointState 给 `/<robot>/s0/<hw>/state`，**不**起真 s0_daemon
   （iceoryx2 S0Command max_subscribers=1，避免和测试侧 sub command
   topic 撞）。fake state 用 `robot_core::time::SystemClock::now()`
   填 stamp_ns，跟 s1 anchor clock domain 同步。
4. **三段式 ready signal**：marker (5s) → TCP probe (500ms / 50ms
   retry) → RPC probe (5s / 100ms retry) GetTaskStatus zero_uuid
   期 NotFound（端到端 round-trip 通了）。各段独立 timeout，失败
   fingerprint 各异。
5. **子进程清理**：`tokio::process::Command::kill_on_drop(true)` +
   `Drop for CaseCtx` SIGTERM (3s grace) → SIGKILL fallback；test
   panic 路径靠 RAII 兜底；forensic dump 写 `/tmp/b4_e2e/<robot_id>/`。

## Plan-Execute 5 处 diff（无 scope 漂移）

执行时碰到 cargo / serde / iceoryx2 框架要求，按 RIPER reverse-sync
原则**先修 Spec 再修代码**：

1. Cargo.toml 加 `[[test]] required-features = ["client"]`（cargo
   test 默认跳过本 binary，不破生产 binary 不带 client stub 拍板）
2. dev-deps 加 `robot-core`（fake_state 用 SystemClock 让 stamp_ns
   通过 s1 max_stale_ms check）
3. 测试 toml 不写 `[s1]` / `[s1.profile]` 段（serde
   `deny_unknown_fields` 跟 nested table 冲突；走 `default_b2`
   fallback 等价）
4. AC-1 不强制收 `Started` 事件（pubsub keep-last 单 slot；client
   端 events_subscribe thread 在 invoke RPC 返回**之后**才起，可能
   错过第一帧；outline §5.5 #1 已 ack 此设计）
5. CaseShutdownError 加 `Signal` variant 留 forensic 路径（不影响
   功能）

详细 diff 推导见 Spec § Execute Log。

## 工程经验（v0.4+ 跨进程 e2e 复用）

直接抄 `crates/robot-rpc/tests/common/` 的 6 文件结构 + 9 工程点已
固化在 `docs/handoffs/v0.3_b4_summary.md` §4：

- `case_ctx.rs` (329L)：CaseCtx + bring_up + shutdown + Drop
- `subprocess.rs` (359L)：SubprocessHandle + 2 drain task + marker
  poll + libc SIGTERM
- `ready.rs` (156L)：三段式 ready
- `client.rs` (155L)：proto cmd builders + 4 token const
- `fake_state.rs` (153L)：替代 s0_daemon
- `mod.rs` (30L)：re-export

## 验证证据

- `cargo test -p robot-rpc --features client --test task_rpc_e2e --release -- --test-threads=1`：6/6 PASS（1.73s wall）
- `cargo test --workspace --tests --features "robot-rpc/client" -- --test-threads=1`：全过（robot-rpc lib 134 + task_rpc_e2e 6 + robot-s1 lib 188 + ... 全过）
- `target/release/crash_restart_smoke --iterations 5`：5/5 PASS
- `target/release/crash_restart_smoke --use-mock-s1 --iterations 5`：5/5 PASS

## reviewer 闭环

- Innovate Approved：5 维度对比 (子进程清理 / 并发控制 / ready signal
  / forensic / 路径) + Candidate A 默认推进 + Candidate B fallback
- Plan Approved（带 2 处小修：文件数 7 新+2 改 / GLOBAL_LOCK 用
  tokio::sync::Mutex）
- 实现完成 ACK + push f52daf8
- handoff doc 单独 commit 2c68a4e（不混进 e2e 实现）

B4 至此功能闭环完成；进 v0.4 路线（v0.3_b4_summary §7）。
