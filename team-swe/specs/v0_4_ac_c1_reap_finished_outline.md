# Outline · v0.4 候选 3 AC-C1 · `EventsDrainRegistry::reap_finished()`

> **触发 round**：v0.4 候选 3 (2/2-b) 实测后 `events_registry_len`
> 1000 task 长跑后线性 = 1000，500ms grace 后仍 = 1000 不收敛 →
> AC-C1 真触发（详 `team-swe/specs/v0_4_pubsub_cleanup.md` § Plan
> §P.2.3 + 实测 CSV `/tmp/pubsub_cleanup_a1.csv`）。
>
> 本 outline 仅做**设计 + 验收路径拍板**；reviewer round ACK 之后才进
> 实施。沿 v0.3 B2 / B3 outline 同款模板精简版（候选 3 内子工作项，
> 不开新 milestone）。
>
> 写于 2026-05-12，commit `976da62` (2/2-b) push 后实测触发的 follow-up。

---

## 1. 触发证据（来自候选 3 实测 phase all）

```
CSV /tmp/pubsub_cleanup_a1.csv (smoke phase all):
iter | events_registry_len | vmrss_s1_kb | vmrss_gw_kb | gw_threads
   0 | NA                  | 8 028       | 10 692      | 38
 100 | 100                 | 12 752      | 12 544      | 38
 200 | 200                 | 16 832      | 13 792      | 38
 500 | 500                 | 29 648      | 15 348      | 38
1000 | 1 000               | 49 768      | 23 832      | 38
1001 | 1 000   (after 500ms grace; 不收敛)
```

- `events_registry_len` 完美线性（顺序跑 in-flight ≤ 1，应 ≤ 10；
  实测 = N task 数 + 不收敛）→ `EventsDrainRegistry` 持续累积已结束
  thread 的 handle，**确认 AC-C1 真触发**
- VmRSS s1 线性增 ~42 MB / 1000 task ≈ ~42 KB/task，跟 handle +
  `Arc<EventsDrainMetrics>` + `Arc<AtomicBool>` + topic String 累积
  规模相符（`Box::leak(node)` 本轮**不**动，但 leak 量级跟 registry
  handle 数相关）
- `gw_subscribe_threads` 稳定 38-39 → AC-C2 未触发，subscribe thread
  Drop 自然 reap OK
- A3 三子 case 全 PASS（restart 重启 + 30/30 InvokeTask 探活）→ AC-C3
  未触发

---

## 2. `reap_finished` API 设计

```rust
// in crates/robot-s1/src/events_drain/runtime.rs

impl EventsDrainRegistry {
    /// 周期性 reap 已结束的 drain thread handle。锁内用
    /// [`std::thread::JoinHandle::is_finished`] 扫 `Vec<Handle>`,
    /// `swap_remove` 已退出 thread 的 handle 进 owned `Vec`；锁外
    /// 逐个 `join()` 回收 OS thread 资源 + drop
    /// `EventsDrainThreadHandle` 内的 `Arc<AtomicBool>` /
    /// `Arc<EventsDrainMetrics>` / `JoinHandle` / `topic` String /
    /// `action_id`。
    ///
    /// **接 `&self`** —— 可在 `Arc<EventsDrainRegistry>` 上重复调用；
    /// 沿 `register` / `request_stop_all` / `stop_and_join_all` 同款
    /// 签名（B4 reviewer round 6 阻断锁定的 by-ref 约定）。
    ///
    /// 返回值：本次 reap 数（diagnostic / smoke 看收敛用）。
    pub fn reap_finished(&self) -> usize;
}
```

**内部实现骨架**（lib 实施期再拍细节）：

```rust
pub fn reap_finished(&self) -> usize {
    // step 1: 锁内 swap_remove 已 is_finished() 的 handle 进 owned Vec
    let to_join: Vec<EventsDrainThreadHandle> = {
        let mut guard = self.inner.lock()
            .expect("EventsDrainRegistry mutex poisoned");
        let mut taken = Vec::new();
        let mut i = 0;
        while i < guard.len() {
            if guard[i].join.is_finished() {
                taken.push(guard.swap_remove(i));
                // swap_remove 不递增 i；新 i 位置是原 last 元素
            } else {
                i += 1;
            }
        }
        taken
    };

    // step 2: 锁外逐个 join；panic 吞掉沿 join_all 同款策略
    let n = to_join.len();
    for handle in to_join {
        let _ = handle.join();
    }
    n
}
```

**关键设计 trade-off**：
- 锁内 `swap_remove` O(N) 单趟（N = 当前 registry 内 handle 数）
- 锁外 join 必要：`JoinHandle::join` 理论可阻塞（虽然 `is_finished()`
  已 true 时 join 应立即返；但 thread 终态 grace 仍可能在最后阶段写
  共享状态，join 是 fence）
- panic 吞掉：跟 `EventsDrainRegistry::join_all` 同款策略（join_all
  返 first panic Err，本路径接 `&self` + diagnostic 模式没必要返
  Result —— reviewer round 拍板）

---

## 3. 调用 cadence

reviewer round 1 拍板**方案 B**：s1_daemon 内独立 reaper thread 1s
周期；smoke binary 不动；hot path（handler::handle_invoke /
events_drain run_drain_loop）不动。

```rust
// crates/robot-s1/src/bin/s1_daemon.rs 加 reaper thread

let events_registry_for_reaper = Arc::clone(&events_registry);
let reaper_stop = Arc::new(AtomicBool::new(false));
let reaper_stop_clone = Arc::clone(&reaper_stop);
let reaper_handle = std::thread::Builder::new()
    .name("s1_events_reaper".into())
    .spawn(move || {
        while !reaper_stop_clone.load(Ordering::Acquire) {
            let reaped = events_registry_for_reaper.reap_finished();
            if reaped > 0 {
                // reviewer round 1 阻断 2：reaped > 0 必须 info 级 log
                // events_registry_len —— smoke 端通过 stderr scrape 取
                // 最近值，没这行的话 scrape 永远是最后一次 register 的
                // 累计值，误判 C1 未收敛。idle tick (reaped == 0) 可选
                // 不打或 debug 级，避免 noise。
                tracing::info!(
                    reaped,
                    events_registry_len = events_registry_for_reaper.len(),
                    op = "reap",
                    "s1_daemon events_drain reaper: reaped finished handles"
                );
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    })?;
```

### 3.1 跟 smoke final wait 的强耦合（reviewer round 1 阻断 1）

reaper cadence = 1s + drain thread `terminal_grace = 100ms`，意味着
"task 全跑完到 events_registry_len 收敛到 0" 的最坏延迟 ≈
`reaper_period + terminal_grace + 调度抖动 margin` ≈ 1100ms。

候选 3 (2/2-b) smoke 现状 `run_phase_a1` final sample 前 sleep
`500ms`（详 `crates/robot-tests/src/bin/pubsub_cleanup_smoke.rs` final
sample 段）—— **不够**，会刚好采在下一次 reap 前误判 C1。

**AC-C1 实施期同步修订**（登记 Plan-Execute diff #3；详 §8.3）：

- `run_phase_a1` final sample 前 sleep 从 `500ms` 改 **`2000ms`**（=
  2 个 reaper cycle + grace + margin；保守留 2x 余量）
- 注释明示：值跟 reaper cadence 强耦合，改 reaper period 必须同步改
  本 grace（或反过来）
- 改 smoke binary 默认 + 同步登记 Plan-Execute diff

### 3.2 备选 cadence（reviewer round 1 标 ACK 默认 B1）

- (B1) **1s 周期（reviewer round 1 ACK 默认）**：burst 1000 task 顺序
  ~150s 总时长 / 1s cadence → 单档 ~7 个 handle 内累积，registry
  内存峰值可控
- (B2) 100ms 周期：cadence 更紧；hot path 锁竞争略增（lock per 100ms
  vs 1s × 10 倍频率）；适合长跑 burst 极快场景；本轮不开（除非 B1
  实测仍 leak / 累积不可控）
- (B3) 自适应：>10 handle 累积时加速 reap；本轮不开

### 3.3 禁止方案（reviewer Plan v3 §P.2.3 拍板）

- (a) drain thread 自 `unregister`（破坏 registry 单一 ownership +
  shutdown 顺序确定性）
- (b) watcher thread 持 `JoinHandle` 单独 join（同上）

---

## 4. 与 `stop_and_join_all` 协作语义

```
正常 shutdown 顺序（reviewer round 6 阻断锁定 + 本 outline 新增 step）：

1. coordinator.mark_shutting_down()   ← 拒新 submit
2. rr.request_stop_all()              ← 通知 RR thread 不接新 wire
3. rr.join_all()                      ← 等所有 in-flight handler 跑完
4. (新) reaper.stop + join()          ← 停 reaper 避免跟 step 5 抢
5. events_registry.stop_and_join_all()← 剩余 handle 全 stop+join
```

**协作语义不变**：
- `reap_finished` 仅处理 `is_finished() == true` 的 handle（已自然
  退出）；还活着的 drain thread（terminal_grace 期内 / channel 仍开
  / stop signal 未发）**不动**
- `stop_and_join_all` 接管 reap 后剩余的全部活 handle（`mem::take`
  整个 Vec → 锁外逐个 stop+join），跟 reap 前后**完全等价**
- 重复调 `reap_finished` 或 `stop_and_join_all`：no-op（空 Vec 路径
  现有契约）

**关键 invariant**（保不变）：
- registry 单一 ownership：drain thread **不**持 `EventsDrainRegistry`
  ref，**不**调任何 registry method（避免 Drop / cycle 风险）
- `EventsDrainThreadHandle` 仅由 registry 持有；`register` 把 owned
  handle move 进 Vec
- shutdown 顺序契约（mark_shutting_down → rr.join → events_registry.stop_and_join_all）
  不动；step 4 reaper.stop 插在 step 3 之后、step 5 之前

---

## 5. lib unit 锁测（≥4 测）

`crates/robot-s1/src/events_drain/runtime.rs::tests`：

### `reap_finished_empty_returns_zero`

```rust
#[test]
fn reap_finished_empty_returns_zero() {
    let registry = EventsDrainRegistry::new();
    assert_eq!(registry.reap_finished(), 0);
    assert_eq!(registry.len(), 0);
    // 重复调 no-op
    assert_eq!(registry.reap_finished(), 0);
}
```

### `reap_finished_after_terminal_returns_one`

```rust
#[test]
fn reap_finished_after_terminal_returns_one() {
    // spawn drain + 注册 + send Completed → 等 thread 终态退出
    // → reap_finished returns 1, registry.len() == 0
    let topic = fresh_topic("reap_terminal");
    let _sub = make_subscriber(&topic);
    let (events_tx, events_rx) = mpsc::channel::<ActionEvent>();
    let action_id = fresh_action_id();
    let handle = spawn_events_drain_thread(
        cfg_for(action_id, &topic),
        events_rx,
        make_publisher_factory(topic.clone()),
        Arc::new(SystemClock::new()),
        2,
    )
    .unwrap();
    let registry = EventsDrainRegistry::new();
    registry.register(handle);
    assert_eq!(registry.len(), 1);

    events_tx
        .send(ActionEvent::Completed { outcome: ActionOutcome::Success, at_ns: 1 })
        .unwrap();
    // 等 thread 真退出：terminal_grace 20ms (test cfg) + 安全 margin
    thread::sleep(Duration::from_millis(100));

    assert_eq!(registry.reap_finished(), 1);
    assert_eq!(registry.len(), 0);
}
```

### `reap_finished_active_handle_not_reaped`

> reviewer round 1 拍板测试名（之前 `reap_finished_skips_active_thread`
> 不够明确 invariant）：测试名直接陈述硬约束 = active handle **不**被
> reap。

```rust
#[test]
fn reap_finished_active_handle_not_reaped() {
    // drain thread idle (没发任何事件) → is_finished() false
    // → reap_finished returns 0；handle 仍在 registry
    let topic = fresh_topic("reap_active_not_reaped");
    let _sub = make_subscriber(&topic);
    let (_events_tx, events_rx) = mpsc::channel::<ActionEvent>();
    let action_id = fresh_action_id();
    let handle = spawn_events_drain_thread(...).unwrap();
    let registry = EventsDrainRegistry::new();
    registry.register(handle);

    assert_eq!(registry.reap_finished(), 0);
    assert_eq!(registry.len(), 1, "active handle MUST stay in registry");

    // cleanup: stop + join 走 stop_and_join_all
    registry.stop_and_join_all().unwrap();
    assert_eq!(registry.len(), 0);
}
```

### `reap_finished_then_stop_and_join_all_is_noop_for_reaped`

```rust
#[test]
fn reap_finished_then_stop_and_join_all_is_noop_for_reaped() {
    // 2 个 handle：1 个 Completed → reap, 1 个 active → stop_and_join_all
    let registry = EventsDrainRegistry::new();
    let h1 = spawn_drain_for_test();  // 用一个 helper 收敛
    let h2 = spawn_drain_for_test();
    registry.register(h1);
    registry.register(h2);

    // 终态 h1（保留 h2 active）
    // ... send Completed to h1's events_rx ...
    thread::sleep(Duration::from_millis(100));

    assert_eq!(registry.reap_finished(), 1);
    assert_eq!(registry.len(), 1);  // h2 仍 active

    // stop_and_join_all 接管 h2；不重复处理 h1（h1 已 reap）
    registry.stop_and_join_all().unwrap();
    assert_eq!(registry.len(), 0);
}
```

**附加测**（如 reviewer round 拍板再加）：
- `reap_finished_concurrent_register_safe`：测 reaper + register 并发
  压力（spawn 100 个 drain 同时；reap 期间 register 不死锁）
- `reap_finished_thread_panic_swallowed`：drain thread 内 panic → reap
  返 1 + 不 propagate panic（沿 join_all 同款）

---

## 6. 重跑 smoke 收敛判定

AC-C1 实施 land 之后**必须**重跑 `pubsub_cleanup_smoke --phase A1`
+ `--phase all` 并满足以下收敛判定。reviewer round 1 阻断 3 拍板：
**硬 gate 仅锁 `events_registry_len final == 0` + `triggered_c1 = false`
+ 功能回归全过；VmRSS 仅曲线报告，不升级为本轮 blocker**
（`Box::leak(node)` 候选 3 hard boundary 不改可能继续涨）。

### 6.1 硬 gate（HARD — AC-C1 land 必须满足）

| Metric | 当前 (触发) | AC-C1 land 后硬要求 | 备注 |
|---|---|---|---|
| `events_registry_len` (final after 2000ms grace；详 §3.1) | 1000 | **== 0** | reaper + grace 完成全部已结束 handle 的回收；任何 final > 0 = 失败 |
| `triggered_c1` (`judge_triggered_c1` 阈值 10) | true | **false** | final == 0 自然满足 |
| smoke `--phase all` exit code | 2 (C1 触发) | **0** | 无条件性 AC 触发；A2/A3 路径需保持 PASS |

### 6.2 软指标 / informational only（SOFT — 报告即可）

| Metric | 当前 | AC-C1 land 后期望 | 处理 |
|---|---|---|---|
| `events_registry_len` (iter=100/500/1000 采样) | 100 / 500 / 1000 | ≤ ~10 | reaper 1s cadence + 顺序跑 in-flight ≤1，单档累积 ~7-10 合理；超出走 reviewer round 复审 |
| `vmrss_s1_kb` 增长曲线 | 8→50 MB 线性 | 应明显下降 / plateau；如仍线性 = `Box::leak(node)` residual | **不**硬判；summary 期 informational only |
| `vmrss_gw_kb` 增长曲线 | 10→23 MB 线性 | 同上 | 同上 |
| `iceoryx2_storage_count`（Q5 fix 后真路径） | 0（错路径） | 实测值（非 0 = `/tmp/iceoryx2/{nodes,services}/` 累积） | informational only；AC-A3 探活 30/30 是主判定 |

**summary 期处置策略**（reviewer round 1 拍板）：

- 如硬 gate 全过 → AC-C1 PASS；VmRSS 仍线性 → summary 写
  "`Box::leak(node)` residual leak（候选 3 hard boundary 不改 wrapper
  Node ownership 模型，详 §9）；后续 outline / v0.5+ 单独评估"，
  **不**升级为本轮 blocker
- 如硬 gate 任一不过 → AC-C1 FAIL，走 reviewer round N+1 复审 reaper
  cadence / API 设计

### 6.3 回归 Quality Gate（无回归）

- `cargo fmt --all --check` clean
- `cargo clippy --workspace --all-targets -- -D warnings` 双路 clean
- `cargo test --workspace --tests --features "robot-rpc/client" -- --test-threads=1`：
  940 PASS / 0 FAIL（936 + 4 个 `reap_finished_*` lib unit）
- shutdown 顺序契约 lib unit 全保（`registry_stop_and_join_all_*` +
  `shutdown_in_flight_invoke_rejected_no_register`）
- 双路 `crash_restart_smoke` 5/5+5/5 PASS

---

## 7. Q5 storage path bounded research（同 round 处理）

### 7.1 Research 结果

**iceoryx2 0.8.1 真 storage path = `/tmp/iceoryx2/`**（Linux）。

来源：

- `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/iceoryx2-pal-configuration-0.8.1/src/lib.rs:40`
  定义 `pub const ICEORYX2_ROOT_PATH: &[u8] = b"/tmp/iceoryx2/";` (Linux 路径)
- Mac / Windows 不同（Mac `/tmp/iceoryx2/`，Windows `C:\\Temp\\iceoryx2\\`）；
  Android `/data/iceoryx2/`
- iceoryx2 config 可 override：`Config::default().global.service.directory`，但本项目**不** override（默认走 ICEORYX2_ROOT_PATH）

实测验证（候选 3 实测期间 spawn s1_daemon + gateway 后）：

```
/tmp/iceoryx2/nodes/     ← 10 个 .node_monitor + iox2_node.details 文件 (per node)
/tmp/iceoryx2/services/  ← service 元数据（实测期间无明显增长，可能 service 复用）
```

### 7.2 smoke binary 修复（**独立 commit**，先于 AC-C1 land；详 §8）

`count_iceoryx2_storage_files()` 改 walk `/tmp/iceoryx2/` 递归计文件
数 + 显式输出命中的 storage root（reviewer round 1 拍板：同时输出命
中路径，让 forensic 能确认是否真扫到正确根）：

```rust
/// iceoryx2 0.8.1 真 storage path（iceoryx2-pal-configuration-0.8.1
/// ICEORYX2_ROOT_PATH = "/tmp/iceoryx2/" on Linux；详 AC-C1 outline §7）。
/// 之前 glob `/dev/shm/iceoryx2_*` 永远 0 是错路径（Plan v3 §P.2.2.c
/// 拍板时未实测；Plan-Execute diff #2，详 outline §8.3）。
///
/// 返 (count, root_hit)：`root_hit` = `"/tmp/iceoryx2"` 如目录存在，否
/// 则 `""`；caller eprintln 显式打印让 forensic 能确认。如目录不存在
/// (host 不跑过 iceoryx2 或 OS 已清) → 返 (0, "") 降级 informational。
fn count_iceoryx2_storage_files() -> (u64, &'static str) {
    const ICEORYX2_ROOT: &str = "/tmp/iceoryx2";
    let dir = std::path::Path::new(ICEORYX2_ROOT);
    if !dir.exists() {
        return (0, "");
    }
    (count_files_recursive(dir), ICEORYX2_ROOT)
}

fn count_files_recursive(dir: &std::path::Path) -> u64 {
    let mut count: u64 = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            count += 1;
        } else if path.is_dir() {
            count = count.saturating_add(count_files_recursive(&path));
        }
    }
    count
}
```

`sample_metric` caller 同步更新调用签名（unpack tuple + 在第一次 sample
时 eprintln 命中 storage root；以后 sample 仅取 count，root_hit 不重复
打）。

### 7.3 Q5 拍板（reviewer round 1）

- **ACK** `/tmp/iceoryx2/` recursive fallback 路径；显式输出命中
  storage root 让 forensic 能确认
- 如 host 不跑过 iceoryx2 (root 不存在) → 返 (0, "") 降级 informational
  only，跟 AC-A3 主判定（重启后 InvokeTask 30/30）兼容
- 如 root 存在但 count 仍 0（极少；可能 iceoryx2 内部 service 刚销毁）
  → 仍 informational only，AC-A3 探活兜底
- AC-A3 leftover 探活语义不变：重启后 InvokeTask 30/30 是主判定，
  storage count 是辅助 informational 指标

---

## 8. 实施 commit 拆分（reviewer round 1 拍板）

reviewer round 1 **不 ACK 同 commit** —— 拆 **2 commit**：先 Q5 smoke
path 修正（小、独立、纯 smoke binary 范围）；再 AC-C1 reaper 行为改动
（s1 lib + s1 binary + smoke binary 联动）。两 commit 都 push 后才一起
重跑 phase all 验收。

### 8.1 commit (i) — Q5 smoke storage path 修正

```
📝 fix: v0.4 候选 3 smoke count_iceoryx2_storage_files storage path 修正
```

范围（仅 smoke binary，纯 informational fix）：

- `crates/robot-tests/src/bin/pubsub_cleanup_smoke.rs`：
  * `count_iceoryx2_storage_files()` 签名 `() -> Result<u64, String>`
    改 `() -> (u64, &'static str)`（返 count + 命中 storage root；详
    §7.2）
  * 加 `count_files_recursive` helper
  * `sample_metric` caller 同步：第一次 sample (iter=0) 时 eprintln
    打印命中 root；后续 sample 仅取 count
- 不动 s1 lib / s1 binary / gateway / shutdown 顺序

Quality Gate（跟 (2/2-b) Quality Gate 等价 + smoke binary 内部 6 lib
unit + selftest 端到端 PASS）。

### 8.2 commit (ii) — AC-C1 reaper 行为改动（依赖 commit (i) land 之后）

```
✨ feat: v0.4 候选 3 AC-C1 EventsDrainRegistry::reap_finished()
```

范围：

1. `crates/robot-s1/src/events_drain/runtime.rs`:
   - `EventsDrainRegistry::reap_finished(&self) -> usize` impl
   - 4 lib unit (§5)：empty / after_terminal / `active_handle_not_reaped` /
     reap_then_stop_and_join_all_is_noop_for_reaped
   - panic 不向调用方传播但 warn 记录（reviewer round 1 §10 #1 拍板）
2. `crates/robot-s1/src/bin/s1_daemon.rs`:
   - reaper thread spawn (1s cadence，reaped > 0 时 info 级 log；详
     §3 + reviewer round 1 阻断 2)
   - shutdown 顺序加 step 4: reaper.stop + join，插在
     `rr.join_all()` 之后、`events_registry.stop_and_join_all()` 之前
     （reviewer round 1 §10 #3 拍板）
3. `crates/robot-tests/src/bin/pubsub_cleanup_smoke.rs`:
   - `run_phase_a1` final sample 前 sleep 从 `500ms` 改 `2000ms`（详
     §3.1；跟 reaper 1s cadence + terminal_grace 强耦合，登记 Plan-
     Execute diff #3，详 §8.3）

### 8.3 Plan-Execute diff 累计（候选 3 summary 期 amend Spec §P.4）

- diff #1: (2/2) 拆 (2/2-a) + (2/2-b)（已登记 commit 244249e）
- diff #2: smoke binary `count_iceoryx2_storage_files` glob
  `/dev/shm/iceoryx2_*` 是错路径（Plan v3 §P.2.2.c 拍板时未实测；本
  outline §7 修正，commit (i) land）
- diff #3: AC-C1 reaper land 同步改 smoke final sample sleep
  `500ms → 2000ms`（跟 reaper cadence 强耦合，详 §3.1；commit (ii) 内
  同步 land）
- diff #4: AC-C1 实施 + smoke fix 拆 2 commit（reviewer round 1 阻断
  原 outline §8 同 commit 的拍板；本 outline §8.1 / 8.2 拆分）

---

## 9. 不在范围（hard boundary）

- **`Box::leak(node)` 模式**：本 outline 不动；wrapper iceoryx2 Node
  ownership 模型保（reviewer round 6 + Plan v3 §P.2.3 拍板）
- **shutdown 顺序契约**：4 步顺序不动；本 outline 仅在 step 3 和 4
  之间插 reaper.stop step（不破坏现有顺序语义）
- **events_subscribe** / gateway 端：未触发 AC-C2，本 outline 不改
  gateway 路径
- **frozen proto / fbs schema**：不动
- **`EventsDrainRegistry::register` API**：签名不动
- **`run_action_tick_loop` / coordinator tick path**：不动

---

## 10. Reviewer round 1 拍板（已 ACK / 修订归档）

| # | 项目 | 拍板 |
|---|---|---|
| 1 | §2 API 设计 | **ACK** `pub fn reap_finished(&self) -> usize`；锁内/锁外分阶段；**panic 不向调用方传播但必须 warn 记录**；返回值按移除 handle 数计 |
| 2 | §3 调用 cadence | **ACK** 独立 reaper thread 1s 周期；附带阻断 1 (§3.1 smoke final sleep 改 2000ms) + 阻断 2 (§3 reaped > 0 必须 info 级 log) 两条修订；CLI override 本轮不开 |
| 3 | §4 shutdown 顺序 | **ACK** 加 step 4 reaper.stop+join，插在 `rr_handles.join_all()` 之后、`events_registry.stop_and_join_all()` 之前 |
| 4 | §5 lib unit | **ACK** 4 测；测试名明确 active handle 不被 reap（`reap_finished_active_handle_not_reaped`，已改） |
| 5 | §6 重跑 smoke 收敛判定 | **修订**：硬 gate 仅锁 `events_registry_len final == 0` + `triggered_c1 == false` + 功能回归全过；**VmRSS 仅曲线报告，不硬判**；Box::leak residual 不升级为本轮 blocker |
| 6 | §7 Q5 storage path | **ACK** `/tmp/iceoryx2/` recursive fallback；显式输出命中 storage root；如仍为 0 降级 informational only |
| 7 | §8 commit 拆分 | **修订（不 ACK 同 commit）**：拆 2 commit — 先 Q5 smoke path 修正（commit i），再 AC-C1 reaper 行为改动（commit ii）；两 commit 都 push 后才一起重跑 phase all 验收 |
| 8 | §9 hard boundary | **ACK** 现有列表（`Box::leak(node)` / shutdown 顺序契约 / events_subscribe / proto / register API / tick path 全不动） |

### 10.1 reviewer round 1 阻断（已修进 outline）

| # | 阻断 | 修订位置 |
|---|---|---|
| 阻断 1 | 1s reaper cadence 与 final == 0 判定不自洽 | §3.1 + §6.1 + §8.2 commit (ii)：smoke `run_phase_a1` final sample 前 sleep 从 500ms 改 2000ms |
| 阻断 2 | reaper 不 log 则 log scrape 看不到 len 下降 | §3 reaper thread closure：`reaped > 0` 时 `tracing::info!` 显式打 `events_registry_len` |
| 阻断 3 | VmRSS plateau 不能作为硬 gate | §6.1 / §6.2 把 VmRSS 从硬 gate 表挪到软指标表；summary 期 informational only |

### 10.2 进实施前 reviewer ACK outline commit

本 outline 修订完成后**单独 commit + push**（不进任何代码改动）；
reviewer 复审 push 后 outline 内容，ACK 之后才进 §8 commit (i) Q5
smoke path 修正实施。

---

## 跟其它 milestone handoff / spec 的链接

- [`team-swe/specs/v0_4_pubsub_cleanup.md`](v0_4_pubsub_cleanup.md)
  § AC-C1 + § Plan §P.2.3
- [`docs/handoffs/v0.4_entry.md`](../../docs/handoffs/v0.4_entry.md)
  §4 当前切入点 + §5 6 条 hard boundaries
- [`team-swe/codemap/v0_4_pubsub_cleanup.md`](../codemap/v0_4_pubsub_cleanup.md)
  Pre-Research（events_drain / registry / shutdown 顺序契约 / 危险与
  不确定项）
- `docs/handoffs/v0.3_b4_summary.md` §3.5 shutdown 顺序契约（reviewer
  round 6 阻断锁定） + B4 §3.6 stream lifecycle
- `crates/robot-s1/src/events_drain/runtime.rs` 现有
  `EventsDrainRegistry` impl
- `crates/robot-s1/src/bin/s1_daemon.rs:488-510` shutdown 顺序代码
