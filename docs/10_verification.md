# 10 · 验证与测试（活文档）

> `docs/00_design.md` §10 的落地版本。**骨架提交**：数值占位，随 Step 5–10
> 推进逐项填实测。
>
> - 权威语义来源：`docs/00_design.md` §10 / §6 / §4.9。
> - 偏离 / 调整语义走 `docs/CHANGELOG.md`，不在本文件改。
> - 本文件每次更新 → 在底部《变更记录》追加一行。

---

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
| **iceoryx2 publish→receive p99**（RT 内核 + `SCHED_FIFO 70` + CPU pin + mlockall，50 Hz × 10 min）| **< 100 µs** (hard) | — | — | — | ⏳ **Step 6 必产**；不过 → block Step 7 |
| **iceoryx2 publish→receive p999**（同上）| **< 500 µs** (hard) | — | — | — | ⏳ **Step 6 必产**；不过 → block Step 7 |

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

### 待登记

- `linux-x86_64-nightly-YYYYMMDD`：Step 6 开工用
- `orin-agx-nightly-YYYYMMDD`：Step 10 前必须

---

## 变更记录

| 日期 | 变更 | 作者 |
|---|---|---|
| 2026-04-22 | 骨架初稿：§10.1/§10.2/§10.3 三级结构 + 状态标签 + env_id 机制；所有数值行占位。Step 5 完成时刻快照 | Claude Code (提议) / loosqk (拍板) |
| 2026-04-22 | §10.2.3 CI 1–6 步接入 `.github/workflows/ci.yml`；`deny.toml` 落地（allow 0BSD；workspace path wildcard + private crate 豁免；iceoryx2 / trybuild winnow dup 走 skip-tree）；`scripts/collect_env_id.sh` 兼容 macOS + Linux。TODO 1/3/4 划掉，只剩 2（assert-no-alloc） | Claude Code |
| 2026-04-24 | §10.2.1 加 3 个 task 27c harness（`feetech_bench` criterion / `feetech::integration_smoke` / `scripts/feetech_flamegraph.sh`）+ 附录加 feetech bench baseline 实测数值。结论：协议层零瓶颈，task 28 真机延迟瓶颈在 USB-RS485 wire RTT。 | Claude Code |

---

## TODO（与本文件直接相关，尚未落）

1. ~~**CI 接入**：`.github/workflows/ci.yml` 骨架按 §10.2.3 九步串行写；Step 6 开工前完成。~~ → ✅ 已落（`.github/workflows/ci.yml`，ubuntu + macos matrix，第 1–6 步串行 + `cargo deny` 独立 job）
2. **`assert-no-alloc` 集成**：`crates/robot-tests::rt_arc_guard` / `robot-s0::rt_thread` 启动期全局安装；Step 6 必须。
3. ~~**cargo-deny 配置**：`deny.toml`（版本 / 许可证 / 重复依赖）Step 6 前加，拦截未经登记的新依赖。~~ → ✅ 已落（`deny.toml`，`cargo deny check` 本地 + CI 均过）
4. ~~**env_id 自动采集脚本**：`scripts/collect_env_id.sh`（uname / sw_vers / sysctl / rustc -V / cargo tree 合成 id），实测时一键生成登记条。~~ → ✅ 已落（`scripts/collect_env_id.sh`，macOS + Linux 兼容）
