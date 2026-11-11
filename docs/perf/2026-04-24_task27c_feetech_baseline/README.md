# `feetech` 协议层性能 baseline · 2026-04-24

> Step 7 task 27c 落地的"上 Orin 真机前最后一道防线"性能 baseline。
> criterion bench + perf record 火焰图 + 集成 smoke 三件套实测结果都汇总在此。

---

## 1. 快速结论

- **协议层零瓶颈**：13-motor `sync_read` 端到端 866 ns / `sync_write` 240 ns
  （中位数），远低于 50 Hz tick (20 ms) 预算。
- **task 28 真机延迟瓶颈**几乎全部在 USB-RS485 wire RTT（1 Mbps × packet
  ~50 B / motor → 几 ms 量级），**不**在 Rust 协议层。
- **火焰图揭示的真实可优化点**（task 28 + 留意；本次不改）：
  - ~~`FeetechBus::motor_id` 是 `HashMap<String, Motor>` 查找，`__memcmp_evex_movbe`
    是 hot frame；改 *index-based lookup*（预编译 motor name → joint index 数组）
    可消除 hash + string compare 开销。~~ → ✅ **已落（commit 后续）**：
    `FeetechBus` 加 `sync_read_by_id_into` / `sync_write_by_id` API；
    `FeetechAdapter` 在 `connect()` 缓存 `joint_ids: Vec<u8>` + 预分配
    `pos_raw` / `vel_raw` / `cmd_entries` 缓冲，hot path **零 HashMap lookup +
    零 alloc**。实测改善：
    | case | by-name | by-id | 改善 |
    |---|---|---|---|
    | `sync_read_13motor` | 1044 ns | **482 ns** | **-54%** (2.17×) |
    | `sync_write_13motor` | 298 ns | **138 ns** | **-54%** (2.16×) |
  - `bench/integration` 用的 `MockTransport` 走 `Mutex<VecDeque<u8>>` + `Vec` alloc，
    sync_read 测出的 866 ns **包含约 30%-40% 的 mock fixture 开销**（见 §4.1）；
    真硬件 `serialport::SerialPort` 的 syscall 路径开销分布会**完全不同**，
    task 28 在 Orin 上要重测才有真值。

---

## 2. 测量环境

```
env_id        linux-x86_64-nightly-2026-04-24-feetech-bench
host          AMD Ryzen 9 9950X3D 16-Core (32 thread)
governor      performance
os            Linux 6.17.0-22-generic x86_64（PREEMPT，非 PREEMPT_RT）
rust toolchain nightly（rust-toolchain.toml channel = "nightly"）
profile       bench（继承 release：lto=fat / codegen-units=1 / + debug=true 给 perf 拿 symbol）
criterion     0.5.1
perf          6.17.13（-F 999 --call-graph dwarf）
inferno       0.12.6（perf script → SVG）
```

> 这是 **dev 机**数字，仅作回归对照。绝对延迟门槛以 task 28 Orin USB-RS485
> 实测为准（task 28 完成时回填 handoff 链接）。

---

## 3. criterion bench 数值

详细 17 case 数值见 [`docs/10_verification.md`](../../10_verification.md)
附录 "feetech bench baseline" 表。本文件只列 task 28 最关心的 5 个：

| case | 中位数 | 注 |
|---|---|---|
| **`bus_sync/sync_read_present_position_13motor`** | **866 ns** | task 28 hot path #1 |
| **`bus_sync/sync_write_goal_position_13motor`** | **240 ns** | task 28 hot path #2（不 ACK） |
| `protocol_build/build_sync_write_13motor_2byte` | 33.2 ns | wire packet 序列化 |
| `protocol_build/build_sync_read_13motor` | 14.5 ns | wire packet 序列化 |
| `parse/parse_status_2byte` | 1.58 ns | wire 反序列化 |

13-motor sync_read 与 sync_write 不对称是预期：
- `sync_read` 要解析 13 个 status packet（13× `parse_status` + 13× `combine_bytes_le`
  + 13× `decode_sign_magnitude`）+ mock transport push_rx 13 次（含 Mutex lock）
- `sync_write` 一个包到 broadcast，**无 ACK 解析**

---

## 4. 火焰图实测

### 4.1 sync_read 13motor （hot path #1）

- **文件**：[`sync_read_13motor.svg`](sync_read_13motor.svg)
- **采集**：120 s × `--bench bus_sync/sync_read_present_position_13motor`，
  perf `-F 999 --call-graph dwarf`，inferno-flamegraph 转 SVG。
- **样本数**：127 077 samples（lost 1 chunk，可忽略）
- **原始 perf.data**：1.0 GB，**不入版本控制**（`.gitignore` 加 `perf*.data`）

**Top 8 self-time stacks**（`inferno-collapse-perf | sort` 的样本计数，
单位 picosec-equivalents 比例值）：

| 排名 | 栈 | 占比含义 |
|---|---|---|
| 1 | `[unknown];_int_free` | glibc 释放（mock fixture Vec drop） |
| 2 | `_int_free` | 同上 |
| 3 | `feetech_bench::fake_status` | **测试 fixture**：每次 iter 构造 status packet（Vec alloc + checksum） |
| 4 | `__GI___libc_realloc` | mock fixture Vec grow |
| 5 | `[unknown];regex_automata::PikeVM` + `RawVecInner::finish_grow` | criterion 内部 case 名匹配 |
| 6 | `__GI___libc_free` | 同 #1 |
| 7 | `__GI___libc_malloc` | mock fixture Vec alloc |
| 8 | `<MockHandle>::push_rx;<Mutex>::lock;atomic_compare_exchange` | mock 喂 rx 字节 + Mutex 锁 |

**核心洞察**：

- **真协议层的 hot path（`parse_status` / `combine_bytes_le` / `decode_sign_magnitude`）
  在 top 15 之外**——它们的样本计数低于测试 fixture / mock transport 自己的开销。
- **866 ns 大约 30%-40% 是 mock fixture（`fake_status` Vec alloc + `push_rx`
  Mutex lock + memcpy）**。真硬件 `serialport::SerialPort::read_exact` 的 syscall
  路径开销分布**完全不同**（更多 kernel time / context switch），不能直接外推。
- **criterion 内部 PikeVM 也吃了一些 CPU**（case name pattern matching）；这是
  测量噪声，不是被测代码。

### 4.2 sync_write 13motor （hot path #2）

- **文件**：[`sync_write_13motor.svg`](sync_write_13motor.svg)
- **采集**：30 s × `--bench bus_sync/sync_write_goal_position_13motor`
- **样本数**：32 334 samples
- **原始 perf.data**：260 MB，**不入版本控制**

**Top 6 self-time stacks**：

| 排名 | 栈 | 占比含义 |
|---|---|---|
| 1 | `[unknown];build_sync_write;copy_from_slice;copy_nonoverlapping` | **真协议层**：13-motor packet 序列化主路径 |
| 2 | `__memcmp_evex_movbe` | **`HashMap::motor_id` 的 String compare**（13 motor × 每 lookup 1 次） |
| 3 | `<FeetechBus>::motor_id` | 同 #2，HashMap lookup 主体 |
| 4 | `_int_free` | bench fixture `Vec::with_capacity` drop |
| 5 | `__libc_calloc` | 同上，alloc |
| 6 | `[unknown];build_sync_write` | 协议层 build 自身 |

**核心洞察**：

- sync_write **不读 ACK** → 没 mock transport 的 push_rx / Mutex 开销 → 真协议层
  栈贡献清晰可见：`build_sync_write` + `copy_from_slice` 是大头，符合预期
  （13-motor × 3 byte/motor = 39 byte memcpy）。
- **`FeetechBus::motor_id` HashMap 查找是真实可优化点**：当前每次 sync_write
  调用对每个 motor 名做一次 `HashMap<String, Motor>::get(&str)`，触发 `__memcmp`。
  **task 28 真硬件接入后**，`FeetechAdapter` 可以在 `connect()` 里把
  `joint_names: Vec<String>` 一次性翻译成 `joint_ids: Vec<u8>` 数组，hot path
  直接索引访问 → 预期消除 30%-40% 的 sync_write 开销。
- alloc 来自 bench fixture（`Vec<(&str, i32)> = Vec::with_capacity(13)` 每 iter
  一次），不是协议层固有成本。`FeetechBus::sync_write` 内部也分配 `encoded_data:
  Vec<(u8, [u8; 4])>`，这点在 task 28 真机阶段值得也考虑预分配。

---

## 5. 集成 smoke 结果

- 测试入口：`cargo test -p robot-adapter-feetech --test integration_smoke`
- 3 个测试全 PASS：
  - `xlerobot_13motor_1000_loop_sync_read_write` —— 13 motor × 1000 循环
    sync_read+sync_write，含 sign-magnitude 跨 0 边界
  - `xlerobot_13motor_sync_read_handles_negative_present_position` —— 13 motor
    sync_read 解码负值（-400 raw）
  - `handshake_passes_when_all_13_motors_respond_with_sts3215_model`

---

## 6. 复现步骤

### 6.1 跑 criterion bench

```bash
cargo bench -p robot-adapter-feetech --bench feetech_bench           # 全部组
cargo bench -p robot-adapter-feetech --bench feetech_bench -- bus_sync \
    --warm-up-time 2 --measurement-time 30                            # 仅 bus_sync 组
```

报告生成到 `target/criterion/<group>/<bench>/index.html`。

### 6.2 跑 flamegraph

历史 `cargo flamegraph` 包装已删除（原件可从 Git 查阅）；当时 cargo
flamegraph 0.6.12 实测会把 perf 挂在 cargo 调度进程而非 bench harness（**几乎
全部样本都是 cargo TOML / semver 解析**）。本 baseline 用更直接的 perf record
+ inferno pipeline：

```bash
# 一次性前置
sudo sh -c 'echo -1 > /proc/sys/kernel/perf_event_paranoid'   # 允许采样

# 编译 bench（带 debuginfo）
cargo bench -p robot-adapter-feetech --bench feetech_bench --no-run

# 找 bench binary 路径（hash 后缀每次 rebuild 都变）
BENCH_BIN=$(ls -t target/release/deps/feetech_bench-* | grep -v '\.d$' | head -1)

# perf record sync_read 120s（hot path #1）
perf record -F 999 --call-graph dwarf -o perf-feetech.data -- \
    "$BENCH_BIN" --bench bus_sync/sync_read_present_position_13motor \
    --warm-up-time 2 --measurement-time 120

perf script -i perf-feetech.data \
    | inferno-collapse-perf \
    | inferno-flamegraph --title "feetech sync_read 13motor (120s)" \
    > docs/perf/2026-04-24_task27c_feetech_baseline/sync_read_13motor.svg

# perf record sync_write 30s（hot path #2）
perf record -F 999 --call-graph dwarf -o perf-syncwrite.data -- \
    "$BENCH_BIN" --bench bus_sync/sync_write_goal_position_13motor \
    --warm-up-time 2 --measurement-time 30

perf script -i perf-syncwrite.data \
    | inferno-collapse-perf \
    | inferno-flamegraph --title "feetech sync_write 13motor (30s)" \
    > docs/perf/2026-04-24_task27c_feetech_baseline/sync_write_13motor.svg
```

### 6.3 跑集成 smoke

```bash
cargo test -p robot-adapter-feetech --test integration_smoke
```

---

## 7. 已知限制 / 待办

- **cargo flamegraph 0.6.12 漏样本**：如 §6.2 所述，cargo flamegraph 把 perf
  挂在 cargo 调度而非 bench harness，本目录采用直接 perf record 流程绕开。
  旧包装不再保留，复现时直接使用 §6.2 的 perf record 流程。
- **本机 vs Orin 差距未量化**：本机 Ryzen 9950X3D performance governor，
  Orin Nano A78AE schedutil。task 28 真机阶段需要在 Orin 重跑 §6.2 流程
  得到对照数据。
- **`HashMap<String, Motor>` lookup 是真实可优化点**：见 §4.2 洞察。task 28
  真机阶段如果 sync_write 实测延迟有压力，可以在 `FeetechAdapter::connect`
  里把 `joint_names → joint_ids` 翻译成 `Vec<u8>`，hot path 直接索引访问。
- **`MockTransport` 的 Mutex 偏移真实分布**：mock 用 `Mutex<VecDeque<u8>>` 模拟
  rx_queue，sync_read 走的 Mutex lock 在真硬件路径不存在；真硬件用阻塞 syscall
  `read_exact`，开销分布完全不同。Orin 实测之前不要把 866 ns 当作"真协议层
  延迟"。
