# 10 · 验证与测试（活文档）

> `docs/00_design.md` §10 的落地版本。历史表格保留各阶段的实测与待验证标签；
> 当前 post-E 候选按下方验收入口推进，不以历史 PASS 替代本轮结果。
>
> - 权威语义来源：`docs/00_design.md` §10 / §6 / §4.9。
> - 偏离 / 调整语义走 `docs/CHANGELOG.md`，不在本文件改。
> - 本文件每次更新 → 在底部《变更记录》追加一行。

---

## 当前候选：post-E 通信故障硬化（2026-09-08）

权威验收条件见 [Spec 的 AC-1–AC-24](../team-swe/specs/post_e_comm_fault_hardening.md)，
函数与命令映射见 [正式 Plan](../team-swe/specs/post_e_comm_fault_hardening.PLAN.md)，
执行状态见 [HANDOFF](../team-swe/specs/post_e_comm_fault_hardening.HANDOFF.md)。

- 实现前清单为 [baseline.json](../.verify/post_e_comm_fault_hardening/baseline.json)：
  20 个 integration target、155 项用例，其中 2 项原有 ignored；枚举不算执行通过。
- 实现前实跑：lib / bins 1345 PASS；RPC 五组 integration 45 PASS，显式指向本次
  debug 构建的 S0 / S1 / gateway。此处仅记录旧代码基线。
- 新增重点：未提交请求超时与异常 Busy 残留、带标签 abort 的 CAS 清理、EStop
  与事务入口两种先后顺序、RR 两道 deadline、启动首 IO 发布门与信任文件屏障。
- 收口需重跑保留用例、debug / release 的 `rt_guard_soak` ignored、隔离 smoke；
  本机条件不足或需要 Orin 现场的项如实记录 `NEEDS_HUMAN`，不得写 PASS。
- Plan 核码澄清见 CHANGELOG 2026-09-08。D15 保持 OPEN，不据本地测试宣称无人值守。
- C2 分包实跑：workspace 全 feature 的 lib / bins 为 1389 PASS；Feetech
  全 feature 145 项库测试 + 12 项独立 integration PASS（库内另含 11 项独立
  有界传输验收）。默认 / 全 feature clippy 完整输出零警告，schema compat 16 PASS，
  real-hw example 编译通过。尚未完成 C3/C4 的运行时收敛与总装，不能替代整候选验收。
- C3 独立评审已批准，S0 库 337 PASS（含 root 34 项独立真实 tick / phase 验收），
  全 48 位 HardRT 兼容性复核通过。liveness 与 bound 超时记录先复现再修正；
  分包 fmt / 两组 clippy / build 零警告，workspace lib / bins 1428 PASS；
  完整 IPC / startup / 进程验收归 C4–C5，证据见 `.verify/post_e_comm_fault_hardening/C3.json`。
- C3 新增 S-40 交错验收：pending 同槽取消保留 seq；标记后暂停、标记 CAS 败给
  serial 消费、重复发布者与取消完成后迟到 timeout。方案已 ACK，测试待执行。

## 0. 使用约定

### 状态标签

| 标签 | 含义 |
|---|---|
| ✅ | 指标已达成，baseline / regression threshold 已写死 |
| 🟡 | 已有测试但指标未实测 / 未落 CI |
| ⏳ | 测试尚未落地（Step 未到） |
| ⚠️ | 实测未达标 / 有待开的债 |

### 更新触发

- 每完成一个 Step：过一遍本文件，把该 Step 解锁的指标状态从 ⏳ → 🟡 或 ✅
- 基线数值实测：在《表》里填 `measured` + `measured_at` + `run_env`
- 回归阈值变动：走 `docs/CHANGELOG.md` 登记 + 更新本文件《10.1 硬指标》表

### run_env 约定

实测值必须同时记录机器 / 内核 / CPU 频率治理器 / iceoryx2 版本，否则不可比较。
本文件用 `env_id` 字符串代称，详解见底部《附录 · 实测环境登记》。

---

## 10.1 硬指标（与 `00_design.md` §10.1 1:1 对齐）

### 10.1.1 S0 loop

| 指标 | 目标 | measured | measured_at | env_id | 状态 | Step |
|---|---|---|---|---|---|---|
| target rate | 1 kHz | — | — | — | ⏳ | Step 6 |
| p99 latency | < 1.5 ms | — | — | — | ⏳ | Step 10 (Orin) |
| max latency (1 h soak) | < 5 ms | — | — | — | ⏳ | Step 10 |
| watchdog timeout path | < 2 cycles | — | — | — | ⏳ | Step 7 |
| RT thread allocation count (after init) | = 0 | — | — | — | ⏳ | Step 6 |
| RT thread `Arc::clone/drop` count | = 0 | — | — | — | ⏳ | Step 6 |

`docs/00_design.md` §4.9 / §10.1 定义；检测钩子 = `robot-tests::rt_arc_guard` + `assert-no-alloc`（§10.2）。

### 10.1.2 S1 loop

| 指标 | 目标 | measured | measured_at | env_id | 状态 | Step |
|---|---|---|---|---|---|---|
| target rate | 100 Hz | — | — | — | ⏳ | Step 6 |
| p99 latency | < 15 ms | — | — | — | ⏳ | Step 10 |
| policy timeout 不阻塞 coordinator | kill -9 policy host → coordinator 仍 100 Hz | — | — | — | ⏳ | Step 9 |

### 10.1.3 Transport

**Measured baseline**（v0.1 必产，v0.2 起作 regression 基准；按机型分别 histogram）：

| 场景 | 目标 | measured p99 | measured_at | env_id | 状态 |
|---|---|---|---|---|---|
| 512 B message p99 (same host) | baseline | — | — | — | 🟡 Step 5 已具备测试入口，数值待 bench |
| 3 MiB blob p99 | baseline | — | — | — | ⏳ Step 6 加 blob 类 topic 后 |
| 1 h soak command topic drop | `== 0` (hard) | — | — | — | ⏳ Step 9 soak harness 落地 |
| **iceoryx2 publish→receive p99**（RT 内核 + `SCHED_FIFO 70` + CPU pin + mlockall，50 Hz × 10 min）| ~~< 100 µs (hard)~~ | — | — | — | ❌ **hz≥50 判据无效（F7 拍板 2026-09-01，PX-59）**：该场景实测尾部为速率错拍伪影非调度延迟，不 block、不作 gate；新判据待 F7 判据小件 |
| **iceoryx2 publish→receive p999**（同上）| ~~< 500 µs (hard)~~ | — | — | — | ❌ 同上（F7 / PX-59） |

**F7 拍板（2026-09-01，PX-59）**：上两行阈值在发布频率 ≥ 50 Hz 下测得
的是 harness 速率错拍伪影（变频判别实证，见 demo summary 附录 A.1），
**不再作为调度 gate**。`iceoryx2_rt_baseline` bin 仍按内置阈值默认输出
FAIL / exit 1——在 F7 判据小件完成前**该输出仅是 legacy 诊断**，不得
用于 ≥50 Hz 的调度判定；原始数据保留，新判据等错拍机制定位后另拍。

**iceoryx2 RT baseline 指标出处**：`docs/CHANGELOG.md` 2026-04-24 "§4.7 policy_host 调度等价 inline" 条目第 2 项决定。测试需在 Linux RT 内核（5.10 RT / 6.1 RT）+ `chrt -f 70` + `taskset` CPU pin + publisher / subscriber 两端均 `mlockall` 下完成，非 RT 或 macOS 测的数据不作 baseline。

**硬约束（不受绝对值影响）**：

| 约束 | 状态 |
|---|---|
| command topic drop count == 0 | ⏳ Step 9 soak |
| sensor topic drop allowed but counted | ⏳ Step 6 metrics hook |
| `unknown_layout_count == 0`（warmup 后） | ⏳ Step 6 |

**v0.2+ regression threshold**（基线确定后启用）：

| 回归项 | 阈值 | 状态 |
|---|---|---|
| command topic p99 | 相比 baseline 回退 ≤ 30% | ⏳ |
| blob topic p99 | 相比 baseline 回退 ≤ 30% | ⏳ |

**Step 5 已落的 transport 层可观测项**（作为未来 bench 的测量点）：
- `robot-transport::ParseMetrics`：每种 `ParseError` 变体独立 `AtomicU64`
- `Iceoryx2Publisher::loan` 早期拒绝 → `TransportError::SampleTooLarge`（size > tier cap）
- `Iceoryx2SampleWriter::iceoryx2_publish` → `loan_slice_uninit` → `send`（三段可打点）

### 10.1.4 Safety

| 指标 | 目标 | measured | measured_at | env_id | 状态 | Step |
|---|---|---|---|---|---|---|
| EStop RPC → S0 latched | < 20 ms | — | — | — | ⏳ | Step 8 |
| S1 crash → S0 safe output | < 250 ms | — | — | — | ⏳ | Step 7 |
| RPC gateway crash → S0/S1 | 0 tick drop | — | — | — | ⏳ | Step 9 |
| physical estop button → latched | < 5 ms | — | — | — | ⏳ | Step 10 |
| EStop latch 持久化往返（重启 S0 后仍 latched） | 必过 | ✅ 23 unit tests | 2026-04-25 | local | ✅ | Step 8 |
| EStop latch 双文件 fail-over（primary 损坏/缺失 → backup 自愈） | 必过 | ✅ 同上 | 2026-04-25 | local | ✅ | Step 8 |
| kill -9 s0 → 硬件 fail-safe（torque disable/zero/brake） | < 500 ms | — | — | — | ⏳ blocking gap | Step 10 / task-30-post |

### 10.1.4.1 Adapter RT baseline（按 RtClass 分级）

> **task-30 v9（pre-1d）**：原 §10.1 只覆盖 HardRT / SoftRT 假设。task-30 引入
> `RtClass::SerialRT`（§4.6 Adapter 分级），feetech 串口舵机由 HardRT 改标
> SerialRT，挂 `s0_serial_io_thread`。本节列 SerialRT 专属 SLA + baseline 项。

| 指标 | 目标 | measured | measured_at | env_id | 状态 | Step |
|---|---|---|---|---|---|---|
| **HardRT** `read_state` p99 | < 50 µs | — | — | — | ⏳ | Step 6 mock |
| **HardRT** `write_command` p99 | < 50 µs | — | — | — | ⏳ | Step 6 mock |
| **HardRT** `safety_stop_rt` (RT-only fast path) p99 | < 100 µs | — | — | — | ⏳ | M5 mock + Step 10 |
| **SerialRT** `feetech_poke ping`（9-motor handshake，逐 motor） | baseline（毫秒量级）| ✓ 9/9 ping ok | 2026-04-26 | Orin AGX bus2 5A7C122340 | 🟢 task #15 Stage 1 实测 | Step 10 |
| **SerialRT** `safety_stop_fast`（6-motor sync_write Torque_Enable=0）| **< 10 ms** (hard) | < 1 s 端到端断扭（broadcast 已生效）| 2026-04-26 | 同上 | 🟢 端到端真断扭 | M4 落地 + Orin 实测 |
| **SerialRT** `confirm_torque_off` RTT（6-motor sync_read 验证 Torque_Enable=0）| **< 50 ms** (hard) | post-shutdown 全 0（ms-level 实测留 follow-up）| 2026-04-26 | 同上 | 🟢 hardware 真断扭确认 | M6 落地 + Orin 实测 |
| **SerialRT** `latch fsync` 触发 → 双文件落盘 | < 200 ms（hard，daemon 仍 active）| ✓ < 200 ms | 2026-04-26 | 同上 | 🟢 §5/§7 两次复测 | task #15 阻断 1 |
| **SerialRT** `read_state`（6-motor sync_read Present_Position+Velocity）p99 | < 8 ms | — | — | — | ⏳ p99 数采集留 follow-up | Step 10 |
| **SerialRT** `write_command`（6-motor sync_write Goal_Position）p99 | < 4 ms | — | — | — | ⏳ p99 数采集留 follow-up | Step 10 |
| **SerialRT** ballet（6-motor 50 Hz cmd 12 s 全程）| daemon Enabled.Active 持续 | ✓ 8 s+ Enabled，motor 2/4 偏移 +3-4° (±5° amp) | 2026-04-26 | 同上 | 🟢 §4 物理摆动确认 | task #15 Stage 1 |
| **SoftRT** snapshot 更新到 RT thread 可见的 staleness | < 50 ms | — | — | — | ⏳ | v0.4 RealSense |

**SerialRT 测量约束**：
- 必须在 Orin（aarch64 + USB-RS485 实机）跑，dev 机数据**不作 baseline**；
- `safety_stop_fast` / `confirm_torque_off` 跑 100 次取 p50 / p99 / max；
- 触发条件 = `s0_daemon` 已 spawn `s0_serial_io_thread` + Feetech adapter connected
  + bus 上 motor 全 ping 通过；
- baseline 采集时机 = M6 落地后即刻；M7 真机 smoke 跑通过夜间 1 h soak 复查。

**2026-04-26 task #15 Stage 1 实测 baseline**（bus2 right_arm 6 motor + dimos 留扭）：
- §2 9 motor (1-9) 全部 `ping ok: motor_id=<id> ... (Model_Number=STS3215 777)`
- §3 smoke 5s daemon connect 仅，**不**调 enable，motor `Torque_Enable` 维持 baseline (1)，state=Ready
- §4 ballet 12s（mock_s1 anchor 1s + cmd_seq=788）：state Ready→Enabled.Active@t=6s 持续 8s+，post-run motor 2/4 偏 +3-4°（±5° amp 内）
- §5 EStop trigger：daemon active T+200ms 时 latch primary+bak 都 = "1"（fsync 双写已落盘），post-shutdown 6 motor `Torque_Enable=0` 全
- §6 latch=true 重启：`initial estop_latched=true`，state 全程 Safe，hb 进来后仍**不**进 Ready/Enabled
- §7 删 latch 重启：`estop_latched=false` → 第二次 ballet → 第二次 EStop → motor 全 0
  （**⚠️ pre-strict 历史记录**：reviewer round 4 commit `4222df5` 之后默认
  strict fail-closed（A1-5 撤 `--allow-empty-latch` opt-in 后唯一行为）让
  "rm latch && restart" 拒启动，此条 baseline 行为不可在当前 v0.2 复现；
  当前合法 latch=0 重置走 `scripts/systemd/README.md` provision 流程，
  **不**等价于 ClearEStop — 详 docs/CHANGELOG.md 2026-04-26 round 4/5
  条目 + A1-5 commit）

**未填项**（follow-up）：`read_state` / `write_command` p99 ms-level 量化采样（要在 daemon hot path 加 latency
计数 + 100 次抽样）；safety_stop_fast / confirm_torque_off 单独 ms-level 计时（当前只验"端到端 < 1s 断扭"，没拆 sync_write
本身耗时）。这两条留在 task #15 Stage 2 / Step 10 完成。

### 10.1.5 Schema

| 指标 | 目标 | 状态 | Step |
|---|---|---|---|
| old reader reads new writer（所有 v0 组合） | 全通 | 🟡 已落 `robot-msg/tests/compat.rs`（Step 2），v0 只含 JointState；新增 schema 时需补 | Step 2 已做 |
| new reader reads old writer（所有 v0 组合） | 全通 | 🟡 同上 | Step 2 已做 |
| deprecated 字段 id 不可复用 | CI lint 拦截 | ✅ `robot-msg/src/bin/schema_lint.rs`（Step 2） | Step 2 |
| `layout_id` 碰撞检测 | CI lint（固定测试向量） | ✅ `schema_lint` 跑 blake3（Step 2） | Step 2 |
| `SCHEMA_REGISTRY` 严格递增 | CI 单测 | ✅ `schema_id::tests::registry_ids_are_unique_and_monotonic` | Step 2 |
| 每个 `schema_id` 必须有 `SizeTier` | CI 单测 | ✅ `size_tier::tests::every_registered_schema_has_a_tier`（Step 5） | Step 5 |

---

## 10.2 测试基础设施

### 10.2.1 Harness 清单

| Harness | 位置 | 用途 | 状态 | 触发 Step |
|---|---|---|---|---|
| `cyclictest` 1 h + 直方图 | 外部工具（cyclictest deb 包） | RT 抖动 baseline | ⏳ | Step 6 Linux / Step 10 Orin |
| `iceoryx2 rt_baseline` | `crates/robot-tests/src/bin/iceoryx2_rt_baseline.rs`（Step 6 新建）| 50 Hz × 10 min publish→receive 时刻直方图，配 SCHED_FIFO 70 + CPU pin + mlockall | ⏳ | **Step 6 必产** |
| `assert-no-alloc` 钩子 | RT thread 入口 | CI debug+release 跑 0 alloc | ⏳ | Step 6 |
| `robot-tests::soak` | `crates/robot-tests/src/bin/soak.rs`（未创建） | mock 500 Hz state + 200 Hz command / 1 h / 24 h | ⏳ | Step 6 后 / Step 9 |
| `robot-tests::crash_restart` | `crates/robot-tests/src/bin/crash_restart.rs`（未创建） | 随机 `kill -9` S1 / policy / gateway | ⏳ | Step 9 |
| `robot-tests::schema_compat` | 当前由 `crates/robot-msg/tests/compat.rs` 覆盖 | old/new 交叉解析 | 🟡 v0 层已覆盖；跨 crate 版本交叉推到 v0.3 | Step 2 部分 / v0.3 补 |
| `robot-tests::rt_arc_guard` | 未创建 | RT thread 内 `Arc::clone/drop` 计数 | ⏳ | Step 6 |
| `hello_pubsub` | `crates/robot-tests/src/bin/hello_pubsub.rs` | Step 5 最小闭环演示 | ✅ | Step 5 |
| `feetech_bench` | `crates/robot-adapters/feetech/benches/feetech_bench.rs`（task 27c）| criterion 5 组：encoding / protocol / parse / bus / bus_sync(13motor) | ✅ | Step 7 task 27c |
| `feetech::integration_smoke` | `crates/robot-adapters/feetech/tests/integration_smoke.rs`（task 27c）| 13-motor × 1000 循环 sync_read+sync_write + 跨 0 边界 + handshake | ✅ | Step 7 task 27c |
| `scripts/feetech_flamegraph.sh` | 同上 | 封装 cargo flamegraph 跑 bench 长循环 + perf record + SVG | ✅（脚本就绪；首次运行需 `cargo install flamegraph` + `perf_event_paranoid`） | Step 7 task 27c |

### 10.2.2 现存单元 / 集成测试清单（Step 5 完成时刻快照）

| Crate | 测试 | 计数 | 覆盖 |
|---|---|---|---|
| `robot-core` | `time::tests` | 33 | `RobotClock` / `Deadline` / `MonoRawNs` 代数 / Mock / `SystemClock` 单调 |
| `robot-msg` | `schema_id` + `capability` + `size_tier` | 15 | registry 单调 + 穷举 tier + capability flag 位 |
| `robot-msg` | `tests/compat.rs` | 13 | v0 ↔ 当前 JointState 交叉 + deprecated id 拦截 |
| `robot-transport` | unit | 12 | parser / sample / writer / metrics |
| `robot-transport` | `tests/iceoryx2_roundtrip.rs` | 4 | 真后端 roundtrip / size 超档 / attribute mismatch / Debug |
| `robot-transport` | `tests/sample.rs` | 20 | mock round-trip + SchemaMismatch + BlobMisaligned + 资源计数守恒 |
| `robot-transport` | `tests/iceoryx2` 混入 | 10 | qos + service_name + 类型层 |
| `robot-transport` | `tests/trybuild.rs` | 1 test，编译期 3 条负向 | `MessageView` outlive lease / publish 后写 / 二次 loan |

### 10.2.3 CI 顺序（严格串行，全程 nightly）

| # | 步骤 | 状态 |
|---|---|---|
| 1 | `cargo fmt --all --check` | ✅ `.github/workflows/ci.yml` |
| 2 | `cargo clippy --all --all-targets --all-features -- -D warnings`（含 pedantic） | ✅ |
| 3 | `cargo test --workspace --all-features` | ✅ |
| 4 | `cargo test -p robot-transport --no-default-features --features mock`（iceoryx2 off 保底） | ✅ |
| 5 | `cargo build --release --workspace` | ✅ |
| 6 | `cargo deny check`（advisories / bans / licenses / sources） | ✅ 独立 job（`deny.toml`） |
| 7 | `allocation-guard`（`assert-no-alloc` + RT thread 入口钩子） | ⏳ Step 6 |
| 8 | `schema_compat`（跨版本 crate round-trip） | ⏳ v0.3 |
| 9 | 夜间：soak 1 h / 24 h + `cyclictest` + crash/restart | ⏳ Step 9+ |

**CI 配置文件**：`.github/workflows/ci.yml`。matrix 覆盖 `ubuntu-latest` + `macos-latest`（Step 0–5 两平台都跑）。Step 6 起 Linux-only 工作另开 workflow（`SCHED_FIFO` / `mlockall` 等不可在 macOS 跑）。

---

## 10.3 Record / Replay 格式

### 10.3.1 记录必须保存的字段（§10.3 原样引用）

| 字段 | 语义 | 状态 |
|---|---|---|
| `topic` | iceoryx2 ServiceName 原字符串 | ⏳ Step 9+ recorder |
| `schema_id` | `FrameHeader.schema_id` | ⏳ |
| `schema_version` | `FrameHeader.schema_version` | ⏳ |
| monotonic `stamp_ns` | `FrameHeader.stamp_ns`（`CLOCK_MONOTONIC_RAW`） | ⏳ |
| receive `stamp_ns` | recorder 本地取样时刻 | ⏳ |
| `seq` | `FrameHeader.seq` | ⏳ |
| header bytes | 原始 `FrameHeader` FlatBuffers | ⏳ |
| body bytes | 原始 body FlatBuffers | ⏳ |
| blob bytes 或 blob 文件 offset | 两种存储形式都支持 | ⏳ |
| dropped sample marker | recorder 发现 seq 跳号时插入 | ⏳ |
| clock info（monotonic → wall offset 采样） | 录制开始 + 结束 + 中间抽样 | ⏳ |
| robot config hash | `ConfigIdentity.config_hash` | ⏳ Step 8+ |
| calibration hash | `ConfigIdentity.calibration_hash` | ⏳ |
| session_id | 每次运行 uuid | ⏳ |
| active `layout_id` → `JointLayout` 映射表 | 录制期间所有出现过的组合 | ⏳ |

### 10.3.2 回放模式

| 模式 | 语义 | 状态 |
|---|---|---|
| `as-fast-as-possible` | 忽略时间戳最大速度重放 | ⏳ |
| `real-time` | 按原 `stamp_ns` 间隔重放 | ⏳ |
| `scaled-time` (x2, x0.5, …) | 按倍速缩放重放 | ⏳ |
| `step-by-step` | 交互式逐帧 | ⏳ |
| `topic remap` | 回放到不同 topic | ⏳ |
| `robot_id remap` | 回放到不同机器人实例 | ⏳ |

### 10.3.3 长期互操作

- 短期：自定义二进制格式（body / blob concat + index 文件）。
- 长期：对接 MCAP / LeRobotDataset 导出器（§13 开放问题 5）。

Recorder crate (`crates/robot-recorder/`) 规划时机：v0.3 – v0.4，具体排期见 `docs/01_implementation_steps.md`（本文件不重复）。

---

## 10.4 v0.2 退出标准（本文件视角）

Step 10 完成 = v0.2 达成。以下 **所有** 指标必须从 ⏳ / 🟡 升到 ✅：

- [ ] 10.1.1 S0 loop 全部（1 kHz / p99 / max / watchdog / alloc / arc）
- [ ] 10.1.2 S1 loop 全部
- [ ] 10.1.3 Transport baseline + 1 h soak drop=0 + unknown_layout=0
- [ ] 10.1.4 Safety 全部（含物理按钮 < 5 ms + kill -9 s0 硬件 fail-safe < 500 ms）
- [ ] 10.1.5 Schema 全通
- [ ] 10.2.1 全部 harness 状态 ✅
- [ ] 10.2.3 CI 1 – 9 条全通
- [ ] 10.3 recorder 至少 `real-time` + `as-fast-as-possible` 两模式可用（其余推 v0.3）

⚠️ 当前（Step 5 完成时）距离 v0.2 退出标准还差 Step 6–10 的全部工作量 + 本表绝大多数行。

---

## 附录 · 实测环境登记

随实测推进在此追加条目。建议字段：

```
env_id        macos-m4pro-nightly-20260422
host          Apple MacBook Pro M4 Pro, 64 GB
os            macOS 15.4 (Darwin 25.4.0)
cpu governor  N/A (macOS)
iceoryx2      0.8.1
rust toolchain nightly-2026-04-22 (或当时具体日期)
测量场景       Step 5 同进程 pub/sub
测得项        <列表>
备注           macOS 仅作 Step 0–5 开发期支持；§10.1 绝对值不作 baseline
```

初始化：

```
env_id        macos-m4pro-nightly-20260422
host          Apple MacBook Pro M4 Pro（详 CPU 型号补）
os            Darwin 25.4.0
cpu governor  —
iceoryx2      0.8.1
rust toolchain nightly（Cargo.toml edition=2024；未 pin 具体日期）
测得项        (仅定性) hello_pubsub 同进程 roundtrip OK；无 drop；
              size tier cap 早期拒绝正确；attribute 不匹配 open 被拒
备注           不作为 §10.1 baseline —— 设计文档明确要求按 Orin / Linux
              x86_64 两档实测
```

### feetech bench baseline（Step 7 task 27c 实测，Linux x86_64 dev 机）

```
env_id        linux-x86_64-nightly-2026-04-24-feetech-bench
host          (dev 机；CPU 型号 + governor 待 collect_env_id.sh 补)
os            Linux 6.17（PREEMPT，非 PREEMPT_RT）
rust toolchain nightly（rust-toolchain.toml channel = "nightly"）
profile       release（lto=fat / codegen-units=1）
criterion     0.5.1
测量场景       criterion `--warm-up-time 1 --measurement-time 2`（短跑，仅作回归 baseline）
```

| 组 / case | 中位数 |
|---|---|
| encoding/encode_sign_magnitude_pos | 494 ps |
| encoding/encode_sign_magnitude_neg | 492 ps |
| encoding/decode_sign_magnitude     | 368 ps |
| encoding/split_bytes_le_2 / _4     | 184 / 183 ps |
| encoding/combine_bytes_le_2 / _4   | 367 / 368 ps |
| protocol_build/build_ping          | 560 ps |
| protocol_build/build_read          | 562 ps |
| protocol_build/build_write_2byte   | 966 ps |
| protocol_build/build_sync_write_13motor_2byte | 33.2 ns |
| protocol_build/build_sync_read_13motor        | 14.5 ns |
| parse/parse_status_empty           | 1.50 ns |
| parse/parse_status_2byte           | 1.58 ns |
| parse/checksum_6byte               | 1.28 ns |
| bus/write_goal_position_1motor     | 73.7 ns |
| bus/read_present_position_1motor   | 66.2 ns |
| **bus_sync/sync_read_present_position_13motor**  | **866 ns** |
| **bus_sync/sync_write_goal_position_13motor**    | **240 ns** |

**判断**：protocol / encoding 层零瓶颈。13-motor sync_read 端到端 866 ns ≈ 1 µs，
远小于 50 Hz tick (20 ms) 预算；task 28 真机 Orin USB-RS485 实测的延迟瓶颈
**几乎全部在 wire RTT**（1 Mbps × packet ~50 B / motor → 几 ms 量级），不在 Rust
协议层。这是上 Orin 真机前的"协议层无回归"基线。

### SerialRT FIFO opt-in（XLeRobot 部署）——验证与回滚口径（2026-09-01）

§4.6 偏离登记见 `CHANGELOG.md` 2026-09-01（Spec P.4 PX-56/57）。
**默认口径：不传 `--serial-fifo` 时 SerialRT 线程仍为普通优先级，与
00_design.md §4.6 逐字一致**；FIFO 仅 XLeRobot 双臂 unit opt-in。

**验证**（Orin，栈起后；`pid=$(systemctl show -p MainPID --value
xlerobot-s0-<side>-arm.service)`，两层取证）：

1. 调度类 / 落核：`ps -L -p "$pid" -o pid,tid,comm,cls,rtprio,psr`
   → `s0_serial_io_th` 行 `cls=FF rtprio=70 psr=4|5`；其余线程 `TS`。
   （默认 `ps -Lp` 不显示这三列，必须带 `-o`。）
2. 每线程亲和掩码：遍历 `/proc/$pid/task/*/status` 读
   `Cpus_allowed_list` → serial 线程 = `4`（左）/ `5`（右），
   其余线程全部 `0-3`（unit `CPUAffinity=0-3` 继承）。
3. `supervised_demo --phase read-state` PASS + 起栈窗口
   `journalctl -b` 无 `TransportTimeout`。
4. guard 计数 `/run/robot-system/preflight/*.count` 保持 `0\n`。

**回滚**（不动代码，unit 面即可）：

1. 最小回滚（只撤 FIFO）：双臂 unit ExecStart 撤 `--serial-fifo 70`
   （`--cpu`/`--strict-rt` 可留；strict 检查只对已启用项执法）→
   `daemon-reload` + 重启栈 → serial 线程回普通优先级（TS）。
2. 完整回退（还原 `48cc4a9` 前 unit 全文）：ExecStart 撤
   `--serial-fifo 70`、`--cpu 4|5`、`--strict-rt` **三个参数**；删
   `LimitRTPRIO=99`、`LimitMEMLOCK=infinity`、`CPUAffinity=0-3` 三行；
   恢复 `RestrictRealtime=yes`；`daemon-reload` + 重启栈。
3. 内核侧独立回滚：`/boot/extlinux/extlinux.conf.bak-20260901` 还原
   isolcpus（与本偏离正交，非必需）。

### 待登记

- `linux-x86_64-nightly-YYYYMMDD`：Step 6 开工用
- `orin-agx-nightly-YYYYMMDD`：Step 10 前必须

---

## 变更记录

| 日期 | 变更 | 作者 |
|---|---|---|
| 2026-09-08 | 当前 post-E 验收入口、实现前 Cargo 清单与实跑基线；新增 S-39 / D3 / D4 / D13 验收范围，未标新增项通过 | Codex |
| 2026-09-08 | C2 有界传输与 adapter 分包验证；独立测试实际执行，整候选门仍待 C3–C5 | Codex |
| 2026-09-08 | S-40 同槽取消标记与确定性交错验收登记，方案已 ACK，未宣称新增测试通过 | Codex |
| 2026-04-22 | 骨架初稿：§10.1/§10.2/§10.3 三级结构 + 状态标签 + env_id 机制；所有数值行占位。Step 5 完成时刻快照 | Claude Code (提议) / loosqk (拍板) |
| 2026-04-22 | §10.2.3 CI 1–6 步接入 `.github/workflows/ci.yml`；`deny.toml` 落地（allow 0BSD；workspace path wildcard + private crate 豁免；iceoryx2 / trybuild winnow dup 走 skip-tree）；`scripts/collect_env_id.sh` 兼容 macOS + Linux。TODO 1/3/4 划掉，只剩 2（assert-no-alloc） | Claude Code |
| 2026-04-24 | §10.2.1 加 3 个 task 27c harness（`feetech_bench` criterion / `feetech::integration_smoke` / `scripts/feetech_flamegraph.sh`）+ 附录加 feetech bench baseline 实测数值。结论：协议层零瓶颈，task 28 真机延迟瓶颈在 USB-RS485 wire RTT。 | Claude Code |
| 2026-09-01 | 附录加 SerialRT FIFO opt-in 验证与回滚口径（§4.6 偏离 PX-56/57 的验证面；默认普通优先级不变） | Claude Code (codex 复审要求) / loosqk (拍板) |
| 2026-09-01 | F7 拍板同步：p99/p999 两行 hard gate 在 hz≥50 宣告无效（PX-59）；bin 默认 FAIL 降级为 legacy 诊断（判据小件前） | Claude Code (Reverse Sync) / loosqk (拍板) |

---

## TODO（与本文件直接相关，尚未落）

1. ~~**CI 接入**：`.github/workflows/ci.yml` 骨架按 §10.2.3 九步串行写；Step 6 开工前完成。~~ → ✅ 已落（`.github/workflows/ci.yml`，ubuntu + macos matrix，第 1–6 步串行 + `cargo deny` 独立 job）
2. **`assert-no-alloc` 集成**：`crates/robot-tests::rt_arc_guard` / `robot-s0::rt_thread` 启动期全局安装；Step 6 必须。
3. ~~**cargo-deny 配置**：`deny.toml`（版本 / 许可证 / 重复依赖）Step 6 前加，拦截未经登记的新依赖。~~ → ✅ 已落（`deny.toml`，`cargo deny check` 本地 + CI 均过）
4. ~~**env_id 自动采集脚本**：`scripts/collect_env_id.sh`（uname / sw_vers / sysctl / rustc -V / cargo tree 合成 id），实测时一键生成登记条。~~ → ✅ 已落（`scripts/collect_env_id.sh`，macOS + Linux 兼容）
