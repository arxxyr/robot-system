# v0.4 候选 3 · per-ActionId pubsub cleanup grace 实测 CodeMap

> **范围限定**（reviewer 拍板）：B4 pubsub topic 生命周期、events drain
> shutdown、cleanup smoke。**不**扩到 Scripted / IK / dimos 切换。
>
> 沿 [`b4_3_3.md`](b4_3_3.md) 同款 CodeMap 模式：每节列**预期路径 + 行号
> 锚点**，最后给"危险与不确定项"。本 CodeMap 是 RIPER **Pre-Research**
> 阶段产物，下一步进 Spec / Plan，不直接改代码。
>
> 写于 2026-05-11，B4 (3/3) summary commit `cac4515` + v0.4 entry handoff
> commit 之后。

---

## 1. events drain publisher 端（s1 process）

### 1.1 模块入口

**文件** `crates/robot-s1/src/events_drain/{mod,wire,runtime}.rs`

- **mod.rs**（76L）：模块根 + re-export `EventsDrainMetrics` /
  `EventsDrainRegistry` / `EventsDrainThreadConfig` /
  `EventsDrainThreadHandle` / `spawn_events_drain_thread` / `encode_envelope` /
  `is_terminal`。顶部 doc 给跨进程拓扑图（s1 publisher → iceoryx2 pubsub →
  gateway subscriber）。
- **wire.rs**（244L）：纯 enum 映射 + FlatBuffers builder，零线程依赖。
  `encode_envelope` (line 29-72) + `is_terminal` (line 79-84)；5 个
  `ActionEvent` variant → `ActionEventEnvelope` fbs。
- **runtime.rs**（801L）：线程生命周期 + iceoryx2 publisher I/O。

### 1.2 配置（runtime.rs）

`EventsDrainThreadConfig` (line 39-60)：
- `action_id: ActionId` (line 44)
- `topic: String` (line 49) — `/<robot_id>/s1/action_events/<action_id_hex>`
- `poll_interval: Duration` (line 54) — 默认 **50 ms**（`for_action` line 72）
- `terminal_grace: Duration` (line 59) — 默认 **100 ms**（`for_action` line 73；
  outline §5.5 拍板默认值，**真行为没实测过 = §5.6 #4b 留白点 ①**）

`EventsDrainThreadConfig::for_action(robot_id, action_id)` (line 67-75) 是
唯一 builder（s1_daemon / handler.rs / lib unit 都用它）。

### 1.3 Metrics（runtime.rs:78-94）

`EventsDrainMetrics`（默认 0）：
- `events_received` / `events_published` / `publish_errors`
- `terminal_events`（≤1 命中正常）
- `stopped_by_signal` / `stopped_by_disconnect`（互斥；0/1）

**没有 `topic_unregistered` / `service_alive` / `node_dropped` 计数** —— cleanup 验证留白点。

### 1.4 Handle + Registry（runtime.rs:96-262）

- `EventsDrainThreadHandle` (line 96-139)：含 `JoinHandle<()>` (line 99) +
  `Arc<AtomicBool> stop` (line 100) + `Arc<EventsDrainMetrics>` (line 101) +
  `action_id` + `topic` String。`request_stop` (line 127) → set stop=true；
  `join` (line 136) → 阻塞等线程退出。
- `EventsDrainRegistry` (line 164-262)：`Mutex<Vec<EventsDrainThreadHandle>>`
  (line 166)。
  - `register(&self, handle)` (line 189-194)：push 进 Vec。**唯一注册路径
    = `crates/robot-s1/src/rr_server/handler.rs::handle_invoke` line 241**。
  - `request_stop_all(&self)` (line 198-206)：iter + 各 handle.request_stop。
  - `join_all(&self)` (line 220-238)：`mem::take` 锁内 → 锁外 join；
    panic 不短路（首个 panic 优先 Err 返）。
  - `stop_and_join_all(&self)` (line 246-249)：组合调用；典型 daemon
    shutdown 路径。
  - `len()` / `is_empty()` (line 253 / 259)：诊断 only。
  - **没有 `unregister(action_id) -> Option<Handle>` 方法** —— terminal
    handle 留在 Vec 直到 daemon shutdown（cleanup 留白点 ②）。

### 1.5 Spawn API（runtime.rs:268-342）

`spawn_events_drain_thread<F>(cfg, events_rx, publisher_factory, clock,
source_id) -> io::Result<EventsDrainThreadHandle>` (line 290-342)：
- `publisher_factory: FnOnce() -> Iceoryx2Publisher<...> + Send + 'static`
  (line 297-298)：subscriber `!Send`，**必须**在新线程内构造。
- ready signal 模式 (line 310, 329-333)：spawn 返前等 `ready_tx.send(())`，
  factory panic / iceoryx2 错都在这里捕获。
- thread name `s1_events_drain_<action_id>` (line 313)。

### 1.6 主循环（runtime.rs:348-419）

`run_drain_loop(cfg, publisher, events_rx, clock, source_id, stop, metrics)`：
- 主循环 (line 361-402) `events_rx.recv_timeout(poll_interval)`：
  - `Ok(event)` → `is_terminal` 判定 → `publish_event` → terminal 命中
    `metrics.terminal_events += 1` + `terminal_seen=true` + `break`。
  - `Err(Timeout)` → 继续轮询。
  - `Err(Disconnected)` (line 392-400) → `stopped_by_disconnect=1` +
    立即 `return`（**不**等 grace）。
- stop signal 命中 (line 404-412) → `stopped_by_signal=1` + return（**不**等 grace）。
- terminal_seen 命中 (line 416-418) → `thread::sleep(terminal_grace)` +
  函数返回。

### 1.7 Publisher 寿命（关键泄漏点）

`publish_event` (line 421-439) `publisher.loan + writer.publish` —— 注意：
- `publisher` 是 `&mut Iceoryx2Publisher<markers::ActionEventEnvelope>`，由 thread
  closure 持有；**run_drain_loop 返回后 publisher drop**（thread 退出
  自然析构）。
- BUT publisher 内部 `IxPublisher` 持有 service 的 inner state；**service
  本身**由调用 `publisher_factory` 时构造的 `Node` 持有。
- 生产代码 `crates/robot-s1/src/rr_server/handler.rs::make_events_publisher_factory`
  内 **`Box::leak(Box::new(pub_node))`**（runtime.rs lib unit
  line 495 同款，handler.rs 内同样模式）—— **node 永久不 drop**。

**含义**：每个 InvokeTask 成功 → 一个新 Node leak → service 元数据 +
shm fd 永远不 GC。daemon 重启才回收；长跑 N=1000 task 可能炸 shm 配额或
iceoryx2 service registry。

---

## 2. events subscribe consumer 端（gateway process）

### 2.1 模块入口

**文件** `crates/robot-rpc/src/events_subscribe.rs`（731L 单文件）

- 顶部 doc (line 1-64)：跨进程拓扑 + 4 条 lifecycle 拍板（backpressure /
  client disconnect / terminal event / gateway shutdown）+ outline §5.5
  关键约束。
- 模块 doc 明确 (line 56-63)：**B4 (2/3-c) 边界 = 不做 leftover topic
  cleanup**（"s1 端 events_drain `terminal_grace` 后 publisher drop 自然
  unregister；gateway 端只 sub，无需主动清理"）—— v0.4 候选 3 验证
  对象。

### 2.2 配置（events_subscribe.rs:99-117）

`EventsSubscribeConfig`：
- `robot_id: String` + `action_id_hex: String` + `request_id: String`
- `poll_interval: Duration` (line 112) — 默认 **50 ms**（与 s1 端 align）
- `channel_buf: usize` (line 116) — `tokio::sync::mpsc` buf；默认 **16**

### 2.3 Metrics（events_subscribe.rs:120-140）

- `samples_received` / `events_forwarded` / `decode_errors`
- `channel_full_drops` (line 131) — backpressure 计数
- `stopped_by_disconnect` / `stopped_by_signal` / `stopped_by_terminal`
  （三互斥；`stopped_by_terminal` 是 gateway 独有，s1 端没对应字段）

### 2.4 Handle + Spawn（events_subscribe.rs:144-259）

`EventsSubscribeHandle` (line 144-178)：含 `JoinHandle<()>` + `stop` +
`metrics` + `topic`。

`spawn_events_subscribe_thread(cfg) -> io::Result<(Handle, Receiver)>`
(line 202-259)：
- 起 `tokio::sync::mpsc::channel(channel_buf)` (line 209)。
- ready signal 模式 (line 221, 244-248)。
- thread closure 内 (line 223-242)：
  - `NodeBuilder::new().create::<IxService>()` (line 226-228)。
  - `open_sub::<markers::ActionEventEnvelope>` (line 230-235)。
  - **`Box::leak(Box::new(node))` (line 238)** ← 关键泄漏点；同 s1 端模式。
  - 进入 `run_subscribe_loop`。

### 2.5 主循环（events_subscribe.rs:273-393）

- 主循环 (line 280) `while !stop.load(...)`：
  - `subscriber.latest()` (line 283-294) — 错误 → log + sleep + continue。
  - 没新 sample (line 296-300) → sleep + continue。
  - lease + envelope decode (line 305-334)。
  - terminal 判定 (line 338-341)：`Completed | Failed | Cancelled`。
  - `tx.try_send` 三路 (line 343-365)：
    - `Ok` → forwarded += 1。
    - `TrySendError::Full` (line 347-354) → drop + warn (backpressure)。
    - `TrySendError::Closed` (line 355-364) → `stopped_by_disconnect=1` +
      return（**不**通知 s1）。
  - terminal 命中 (line 367-379) → `stopped_by_terminal=1` + return。
- stop signal 命中 (line 387-392) → `stopped_by_signal=1`。

### 2.6 EventsSubscribeStream::drop（events_subscribe.rs:483-541）

`pub struct EventsSubscribeStream` (line 483-492)：含 `inner:
ReceiverStream` + `stop: Arc<AtomicBool>` + `_join: Option<JoinHandle<()>>`
(line 489) + `metrics` + `topic`。

`Drop for EventsSubscribeStream` (line 531-541)：
- 仅 `self.stop.store(true, Ordering::Release)` (line 535)。
- **不**调 `_join.take().join()` —— "drop 是 sync，join 阻塞 tokio worker"
  (line 480-482)。
- "OS 自然 reap join handle" (line 482) — B4 reviewer (2/3-c) ACK 时备忘
  留 follow-up，候选 3 验证对象。

**含义**：
- subscribe thread 退出后 thread 自身资源 OS 回收。
- BUT thread 内 `Box::leak(node)` 让 node 永久不 drop → gateway 端
  service handle 同样泄漏（每个 StreamTaskEvents RPC 一个 leaked node）。
- `JoinHandle` 在 `EventsSubscribeStream` 内通过 `_join: Option<JoinHandle>`
  持有，stream drop 时 handle 跟着 drop（不阻塞），但 thread 已经
  signaled stop 了。

---

## 3. iceoryx2 wrapper service lifecycle

### 3.1 模块入口

**文件** `crates/robot-transport/src/iceoryx2/{mod,publisher,subscriber,
service_name,qos,attributes}.rs`

- **mod.rs** (197L)：service open-or-create 入口。
- **publisher.rs** (263L)：`Iceoryx2Publisher<T>` + `Iceoryx2SampleWriter<'a>`。
- **subscriber.rs** (82L)：`Iceoryx2Subscriber<T>` + `Iceoryx2SampleLease`。

### 3.2 service 创建（mod.rs:170-197）

`build_service<T>(node, topic, class) -> PortFactory<...>` (line 170-197)：
- `topic_to_service_name(topic)` (line 175) — 串原样喂（方案 A）。
- `default_tier_of(T::SCHEMA_ID)` (line 176-183) — 校验 schema_id 已注册。
- `profile_of(class)` (line 185)：QoS 9 类映射。
- `schema_attribute_verifier(T::SCHEMA_ID, 1)` (line 186)。
- `node.service_builder(&service_name).publish_subscribe::<[u8]>()
  .history_size(...).subscriber_max_buffer_size(...).max_publishers(...)
  .max_subscribers(...).enable_safe_overflow(...)
  .open_or_create_with_attributes(&verifier)` (line 188-195)。

**关键语义**（mod.rs line 71 注释）：
> 首次调用 create，后续相同 topic 的调用自动 open 同一 service 并校验
> schema attribute。

**含义**：
- 第一次 `pub_node.service_builder(...).open_or_create_with_attributes(...)`
  → 创建 service。
- 同 topic 第二次 → open（reference count +1）。
- iceoryx2 0.8.1 service 在**所有 publisher + subscriber + service handle
  drop 后**才 GC（按 iceoryx2 文档；**没**实测过 = §5.6 #4b 留白点 ②）。

### 3.3 Pub / Sub 入口（mod.rs:79-163）

- `channel<T>(&node, topic, class)` (line 79-111)：同进程 pub+sub 对（lib
  unit 用）。
- `subscriber<T>(&node, topic, class)` (line 122-136)：仅 subscriber（gateway
  端 events_subscribe 用）。
- `publisher<T>(&node, topic, class)` (line 144-163)：仅 publisher（s1 端
  events_drain 用）。

### 3.4 Publisher / SampleWriter 寿命（publisher.rs）

- 顶部 doc (line 1-26)：**延迟 loan 策略** —— `loan(size)` 不真申请 shm，
  `publish()` 时按 wire 格式合成 + 一次拷贝 + `loan_slice_uninit` + send。
- `Iceoryx2SampleWriter::iceoryx2_publish` (line 140-158)：成功后
  `consumed=true`。
- `Drop for Iceoryx2SampleWriter` (line 248-256)：未 publish 直接 drop（buffer
  释放即可，**无**iceoryx2 归还动作）。

### 3.5 Subscriber / SampleLease 寿命（subscriber.rs）

- 顶部 doc (line 1-13)：**`Sample` 持 owned Arc 共享状态**，subscriber
  drop 后 sample 仍可用；sample drop 自动归还 shm slot。
- `Iceoryx2SampleLease` (line 64-66)：`sample: IxPubSubSample`；drop 时
  iceoryx2 自动归还。

### 3.6 Node 寿命

iceoryx2 `Node` 是 service registry 的**进程级 handle**：
- 持服务发现 / shm 管理 fd。
- 生产代码各处 `Box::leak(Box::new(NodeBuilder::new().create::<IxService>().unwrap()))`：
  - `crates/robot-rpc/src/events_subscribe.rs:238`（gateway 每条 RPC stream 一个）
  - `crates/robot-s1/src/rr_server/handler.rs::make_events_publisher_factory`（待确认；
    本 CodeMap 第 4 节 §4.3 详查）
  - 各 lib unit (events_drain/runtime.rs:495 / events_subscribe.rs:570 /
    rr_server/handler.rs lib unit)

**含义**：每条 task 路径都 leak 1 个 node（s1 publisher 端）+ 1 个
node（gateway subscriber 端）。daemon shutdown / gateway shutdown 才能
回收。长跑 N=1000 task → 2N leaked nodes → shm 配额 / 文件描述符 / iceoryx2
internal storage 都可能炸（**真上限没实测** = §5.6 #4b 留白点 ②）。

---

## 4. s1_daemon 端 events_registry 注册路径

### 4.1 daemon main 入口

**文件** `crates/robot-s1/src/bin/s1_daemon.rs:73-510`

- line 73：`use robot_s1::events_drain::EventsDrainRegistry;`。
- line 423：`let events_registry = Arc::new(EventsDrainRegistry::new());`。
- line 425-431：构造 `RealHandlerCtx { coordinator, events_registry,
  clock, source_id, robot_id }` 包 `Arc`。
- line 444：`spawn_s1_rr_server_threads(&args.robot_id, &ctx)` 起 4 RR
  server thread；ctx 通过 `Arc::clone` 传给每个 handler closure。

### 4.2 shutdown 顺序契约（s1_daemon.rs:468-510）

```rust
// 1. coordinator.mark_shutting_down() — 拒新 submit
coordinator.mark_shutting_down();
// 2. rr.request_stop_all — 通知 RR thread 不接新 wire
rr_handles.request_stop_all();
// 3. rr.join_all — 等所有 in-flight handler 跑完 (含已 spawn + register 的)
rr_handles.join_all().expect_or_log;
// 4. events_registry.stop_and_join_all — 等所有 drain thread 跑完
events_registry.stop_and_join_all().expect_or_log;
drop(ctx);
```

**关键 invariant**（line 483-487 注释）：第 3 步必须先于第 4 步，避免 RR
thread stop-but-not-join 期间 in-flight invoke 走 submit 成功路径并调
`events_registry.register`，跟 `events_registry.stop_and_join_all` 撞上
late register。

**reviewer round 6 阻断锁定**（B4 (2/3-a) 期间）：`events_registry`
方法接 `&self`（不 by-value 消费 Arc），daemon 可以在 Arc 上直接调，**不**
依赖 `Arc::try_unwrap`（避免 RR thread 残留 closure ref-leak 时打破 join
确定性）。

### 4.3 invoke handler 注册路径

**文件** `crates/robot-s1/src/rr_server/handler.rs:90-281`

- `RealHandlerCtx` (line 90-107)：含 `events_registry: Arc<EventsDrainRegistry>` (line 96)。
- `handle_invoke` (line 146-281) 成功路径 (line 226-265)：
  - line 231：`let cfg = EventsDrainThreadConfig::for_action(&ctx.robot_id, action_id);`
  - line 232：`let factory = make_events_publisher_factory(cfg.topic.clone());`
  - line 233-239：`spawn_events_drain_thread(cfg, ticket.events, factory,
    Arc::clone(&ctx.clock), ctx.source_id)`。
  - **line 241：`ctx.events_registry.register(handle);`** ← 唯一 register 调用点。
- spawn 失败路径 (line 245-263)：调 `coordinator.cancel(action_id, clock_ref)`
  让 coordinator 回 IDLE，不注册（已经 fail）。

### 4.4 mark_shutting_down 守门 lib unit

**lib unit**（rr_server/handler.rs:688-720 附近）：
`shutdown_in_flight_invoke_rejected_no_register` 锁住 `mark_shutting_down`
后 invoke handler 必返 `Unavailable` + `registry.len() == 0`（line 708 注释
`"events_registry must not register late drain after mark_shutting_down"`）。

---

## 5. per-ActionId pubsub topic 命名 + 当前 grace window

### 5.1 topic 命名

`/<robot_id>/s1/action_events/<action_id_hex>`：
- 构造点 1：`events_drain/runtime.rs::EventsDrainThreadConfig::for_action`
  (line 71)。
- 构造点 2：`events_subscribe.rs::spawn_events_subscribe_thread` (line 208)。
- 构造点 3：`events_subscribe.rs::format_topic` (line 395-397) — 仅 log /
  diagnostic。

### 5.2 当前 grace window 默认值

| 端 | 字段 | 默认值 | 来源 |
|---|---|---|---|
| s1 publisher | `EventsDrainThreadConfig.terminal_grace` | **100 ms** | runtime.rs:73 |
| s1 publisher | `EventsDrainThreadConfig.poll_interval` | **50 ms** | runtime.rs:72 |
| gateway subscriber | `EventsSubscribeConfig.poll_interval` | **50 ms** | events_subscribe.rs:112 |
| gateway subscriber | `EventsSubscribeConfig.channel_buf` | **16** | events_subscribe.rs:116 |

**outline §5.5 拍板**：100 ms grace + 50 ms poll align；**真行为没实测过**
= §5.6 #4b 留白点 ①。reviewer 拍板：候选 3 必须实测。

### 5.3 outline §5.6 #4b 留白点 (3 条)

来自 `docs/handoffs/v0.3_b4_outline.md:363-369`：

1. **topic cleanup grace window 默认 100 ms 是否合适？**
   （reviewer 拍板"如倾向更长 / 更短 / 不 cleanup（让 OS 进程退出时回收）
   则 round 4 修"，但 B4 没实测；候选 3 实测后回填 default）
2. **gateway sub 端 service 消失检测语义**（iceoryx2 0.8.1 行为待 impl
   验证）
3. **进程崩溃时 leftover topic 怎么清理？**（systemd restart hook /
   startup sweep / 都不做让 daemon 启动期重新分配 service name？）

---

## 6. 已有 cleanup / leak 测试

### 6.1 events_drain runtime lib unit（runtime.rs）

| Test | 行 | 锁住什么 |
|---|---|---|
| `started_event_round_trip` | 537 | round-trip 1 个 Started |
| `progress_event_round_trip` | 566 | round-trip 1 个 Progress |
| `completed_terminal_event_breaks_loop_after_grace` | 596 | terminal+grace 路径；assert `terminal_events==1` + `stopped_by_signal==0` + `stopped_by_disconnect==0` |
| `failed_terminal_event_carries_failure_reason` | 629 | Failed + reason 透传 |
| `cancelled_terminal_event_round_trip` | 659 | Cancelled |
| `stop_signal_exits_loop_without_terminal` | 682 | stop signal 路径；`<500ms` join + `stopped_by_signal==1` |
| `channel_disconnect_exits_loop_without_terminal` | 706 | drop tx → disconnect 路径；`stopped_by_disconnect==1` |
| `frame_header_seq_monotonic_per_topic` | 728 | seq 单调（同 topic 内 0,1,...） |
| `registry_stop_and_join_all_handles_empty` | 763 | 空 registry no-op |
| `registry_stop_and_join_all_one_thread` | 772 | 1 thread `<500ms` join；重复调 no-op |

### 6.2 events_subscribe lib unit（events_subscribe.rs）

| Test | 行 | 锁住什么 |
|---|---|---|
| `started_event_round_trip` | 612 | 跨 publisher → subscriber thread → recv 1 帧 |
| `terminal_completed_event_exits_thread` | 640 | terminal Completed → thread `<1s` 退 |
| `channel_close_exits_thread` | 682 | drop rx + stop signal → 退 |
| `drop_stream_signals_stop_to_thread` | 703 | `EventsSubscribeStream::drop` 1s 内 set `stopped_by_signal=1` |

### 6.3 rr_server handler lib unit（handler.rs:572, 688）

- `invoke_happy_returns_ok_and_registers_drain` (line 572)：handle_invoke 成功 →
  registry.len() == 1（含 spawn 后 stop_and_join_all 兜底）。
- `shutdown_in_flight_invoke_rejected_no_register` (line 688)：
  mark_shutting_down 后 invoke 必返 Unavailable + registry.len() == 0。

### 6.4 cross-process integration test（task_rpc_e2e.rs）

`crates/robot-rpc/tests/task_rpc_e2e.rs` 6 cross-process AC（B4 (3/3) land）：
- AC-1 happy_invoke_stream
- AC-2 cancel_task_emits_cancelled
- AC-3 get_task_status_three_paths
- AC-4 invoke_task_idempotency_replay_and_conflict
- AC-5 auth_deny_viewer_invoke_and_operator_clear_error
- AC-6 wire_contract_source_task_id_monotonic

**没有任何**：
- N=1000 task 长跑 / `events_registry.len()` 增长曲线
- iceoryx2 service 在 N task 后是否真 GC
- 进程崩溃 leftover topic 清理验证
- gateway shutdown 期 in-flight stream wait grace
- shm fd 配额 / 文件描述符 leak 检测

→ **候选 3 实施 = 补这 5 类测试**。

---

## 7. crash_restart_smoke 模式参考

候选 3 candidate smoke binary 形态参考。

**文件** `crates/robot-tests/src/bin/crash_restart_smoke.rs:1-80`

- **运行方式**：`./target/release/<binary>` → **不**走 cargo test（line 9
  注释：smoke / e2e binary 不该默认跑）。
- **CLI args**（line 36-51 风格）：`--bin-dir target/release` /
  `--robot-id-prefix <id>` / `--hardware-id` / `--config <toml>` /
  `--iterations N` / 等。
- **退出码**：`0`=PASS / `1`=CLI/环境错 / `2`=至少一次 FAIL（line 76-79）。
- **多迭代**（line 30-31）：每次用不同 `robot_id` 后缀避免 iceoryx2
  service collision。
- **forensic dump**：失败时 dump 到 `/tmp/<binary>/<robot_id>/`（参考
  task_rpc_e2e.rs `tests/common/case_ctx.rs:dump_captured_to`）。

候选 3 candidate 文件名：`crates/robot-tests/src/bin/pubsub_cleanup_smoke.rs`
（最终命名 Spec 阶段定）。

---

## 8. 跨进程 e2e harness 复用（B4 (3/3) 沉淀）

候选 3 cross-process smoke 可直接复用 B4 (3/3) `crates/robot-rpc/tests/common/`
6 文件结构（详 `docs/handoffs/v0.3_b4_summary.md` §4 工程经验表）：

- `case_ctx.rs` (329L)：CaseCtx + bring_up + shutdown + Drop
- `subprocess.rs` (359L)：SubprocessHandle + 2 drain task + marker poll +
  libc SIGTERM
- `ready.rs` (156L)：三段式 ready
- `client.rs` (155L)：proto cmd builders + 4 token const
- `fake_state.rs` (153L)：替代 s0_daemon
- `mod.rs` (30L)：re-export

或在 `crates/robot-tests/src/bin/<name>.rs` 仿 `crash_restart_smoke.rs`
模式独立写（取决于 Spec 期 reviewer 拍板 candidate test path = lib
integration test vs smoke binary）。

---

## 危险与不确定项（候选 3 Spec 期必须钉死）

1. **`Box::leak(Box::new(node))` 模式真实泄漏量** —— 每条 task 在 s1 +
   gateway 各 leak 1 node；node 内部多大？1000 task 真撑得住吗？要在
   smoke 内监 `/proc/<pid>/status::VmRSS` + `ls /dev/shm/iceoryx2_*` 计数。

2. **`EventsDrainRegistry` 没 unregister 路径** —— 长跑 N task →
   `Mutex<Vec<Handle>>` 长 N+；Vec push 没问题，但 `request_stop_all`
   遍历 N 个 handle，`join_all` 也是 N。N=1000 的 daemon shutdown 时间
   = ?（每个 handle stop signal 在 50ms poll_interval 内退 +
   100ms terminal grace；在 release build 上 join 应该 fast，但
   pessimistic upper bound 没算过）。

3. **iceoryx2 0.8.1 service GC 真行为未实测** —— 文档说"all
   subscribers + publishers + service handle drop 后 GC"，但生产代码
   永远 leak node = service handle 永不 drop。daemon 重启路径下
   leftover topic 是否阻塞新 service create？是否阻塞重命名（同
   action_id 不可能重复，但 robot_id 可能跨 daemon 重启复用）？

4. **进程崩溃 leftover** —— SIGKILL daemon 后 iceoryx2 storage
   （`/dev/shm/iceoryx2_*` / `/tmp/iceoryx2/` 等）可能残留。下次启动是否
   阻塞？是否需要 systemd `ExecStartPre=` 清理 hook？是否需要 daemon
   启动期 sweep？三种策略各自的 trade-off 没评估。

5. **gateway shutdown 期 in-flight stream** —— B4 (2/3-c) 接受
   "tonic graceful shutdown 自然处理 client-disconnect / terminal"，
   但 gateway SIGKILL 路径下 subscribe thread 能否退？JoinHandle 在
   `EventsSubscribeStream::drop` 仅持有不 join，进程退出时 OS reap，
   但是否有 race（subscriber 还在 latest() 调用中 + node 已 leaked）？

6. **`EventsSubscribeStream::drop` leak 检测**（B4 reviewer (2/3-c)
   非阻断点）—— "subscribe thread 的 handle 有回收路径"备忘点；候选 3
   实测时加 thread count 前后 diff 断言，如真有 leak 走代码改动。

7. **`channel_full_drops` 长跑趋势** —— gateway 端 channel_buf=16；
   client slow / 大量 Progress 事件下是否真触发？outline §5.5 #1 接受
   drop+warn，但 metrics 上限没钉。

8. **测试隔离（iceoryx2 service collision）** —— smoke 多 iteration 用
   不同 robot_id 避免 collision（crash_restart_smoke line 30-31 同款）；
   action_id 是 UUIDv7 每次唯一所以 per-task 不冲突。BUT 同 action_id
   测试用例（lib unit）必须 fresh_topic 命名隔离。

9. **systemd 集成**（如选 systemd hook 路径解决进程崩溃 leftover）—— 沿
   `scripts/systemd/` 同款模式；候选 3 是否引入 systemd 改动 reviewer
   Spec 期拍板。

10. **§6.7 不变量保 5 条** —— 候选 3 完全在 cleanup / 运维加固层面，
    **不**应触发任一不变量降级。Spec 期必须显式列"不变量影响 = 不动 5 条"。

---

## 跟其它 milestone handoff / spec 的链接

- [`docs/handoffs/v0.4_entry.md`](../../docs/handoffs/v0.4_entry.md)：v0.4 入口锚点 + 4 候选 + 优先级 + 切入点
- [`docs/handoffs/v0.3_b4_summary.md`](../../docs/handoffs/v0.3_b4_summary.md) §1.2 #5 + §3.5 + §4：B4 cleanup 留白边界 + shutdown 顺序契约 + e2e 工程经验
- [`docs/handoffs/v0.3_b4_outline.md`](../../docs/handoffs/v0.3_b4_outline.md) §5.5 + §5.6 + §12 表 #4b：原始 outline 拍板 + 留白点
- [`team-swe/codemap/b4_3_3.md`](b4_3_3.md)：B4 (3/3) cross-process e2e 同款 CodeMap 模板（本 CodeMap 沿用结构）
- `docs/00_design.md` §4.1 模块地图（events_drain / events_subscribe）+
  §4.9 RT 铁律（events 路径不碰 RT）
- 后续 Spec / Plan：`team-swe/specs/v0_4_pubsub_cleanup.md`（待写）
