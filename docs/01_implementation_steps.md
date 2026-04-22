# 01 · v0.2 实施步骤

> 本文档是 [`docs/00_design.md`](00_design.md) §11.2 "v0.2 实现顺序" 的可执行版本。
> 10 步顺序**强制**；第 6 步闭环打通前不接任何真硬件。
> 当前位置：**Step 2 已完成 · Step 3 未开始**（Step 0 `07a5446`；Step 1 `654d7ab`；Step 2 见本次 commit）。

---

## Step 0 · 项目初始化

### 交付

- `Cargo.toml`（workspace 根，无 `[package]`）
- `rust-toolchain.toml`（nightly，可 pin 具体日期 `nightly-YYYY-MM-DD`）
- `.cargo/config.toml`（sccache、target 目录、共用 rustflags）
- `.gitignore`（`target/`、`.DS_Store`、sccache cache 路径）
- `crates/robot-core/`（空 crate，只有 `src/lib.rs` 和 `Cargo.toml`）

### 退出标准

- `cargo build --all` 过
- `cargo fmt --all --check` 过
- `cargo clippy --all --all-targets -- -D warnings` 过
- git commit（`✨ feat: 初始化 workspace`）

---

## Step 1 · `robot-core::time`

### 目标

统一时钟封装，禁止业务代码直接调 `Instant::now`。

### 依赖

无。

### 交付

- `crates/robot-core/src/time.rs`:
  - `pub struct MonoRawNs(pub u64)`
  - `pub struct Deadline(pub MonoRawNs)`
  - `pub struct DurationNs(pub u64)`
  - `pub trait RobotClock { fn now(&self) -> MonoRawNs; fn sleep_until(&self, d: Deadline); fn sleep_for(&self, d: DurationNs); }`
  - `pub struct SystemClock`（调 `clock_gettime(CLOCK_MONOTONIC_RAW)` / `clock_nanosleep`；Linux only）
  - `pub struct MockClock`（测试实现，手动推进时间）
- `crates/robot-core/Cargo.toml`：只依赖 `thiserror`。**不得**依赖 `tokio` / `iceoryx2` / `pyo3` / `flatbuffers` / `tracing-subscriber` 等。

### 测试

- `SystemClock::now()` 单调递增（1000 次采样验证非降）
- `MockClock` 推进 + `sleep_until` / `sleep_for` 语义
- `Deadline + DurationNs → Deadline` 算术

### 退出标准

- 所有测试过
- `cargo clippy` 零警告
- 单元测试覆盖所有 public API

### 设计文档参考

§3.2, §3.2.1

---

## Step 2 · `robot-msg` schema registry

### 目标

FlatBuffers schema 定义 + 自动生成 + schema registry + 兼容性测试。

### 交付

- `crates/robot-msg/schemas/`：
  - `frame_header.fbs`（§2.2）
  - `joint.fbs`（`JointLayout` / `JointState` / `JointCommand` / `StaleAction` / `ControlMode`）
  - `twist.fbs`
  - `blob.fbs`（`BlobDesc` / `Dtype` / `BlobLayout` / `Endian` / `Compression`）
  - `image.fbs`、`pointcloud.fbs`、`tensor.fbs`
  - `event.fbs`（`EStopEvent` / `Heartbeat`）
- `crates/robot-msg/build.rs` 调 `flatc` 生成 Rust（按 host platform 选 vendor 的 flatc）
- `crates/robot-msg/Cargo.toml` 新增依赖 `flatbuffers = "24"`（见 `docs/CHANGELOG.md` 2026-04-22 workspace 依赖白名单）
- `crates/robot-msg/src/lib.rs`：
  - 全局 `SCHEMA_REGISTRY`（`schema_id: u32 → &'static str` 名称）
  - `schema_id` 常量表
- `crates/robot-msg/src/bin/schema_lint.rs`（**独立 Rust bin**，CI 阶段 `cargo run -p robot-msg --bin schema_lint` 调用）：
  - deprecated 字段 id 不可复用
  - schema_id / layout_id 唯一性 + 碰撞检测（blake3 固定测试向量）
  - 字段 id 单调递增
  - 实现：v0.2 先 regex 扫 `.fbs` 文本，足够覆盖 v0.2–v0.3 schema 复杂度；v0.3 再评估是否上 `bfbs` 反射
- `crates/robot-msg/tests/compat.rs`：old-reader-new-writer / new-reader-old-writer 运行时交叉（`cargo test` 跑）
- **vendor flatc（策略 A：预编译二进制）**：
  ```
  3rd-party/flatc/
  ├─ README.md                    # 版本 + 校验和 + 升级流程
  ├─ VERSION                      # 纯文本，例如 "24.3.25"
  ├─ darwin-arm64/flatc           # macOS 开发机
  ├─ linux-x86_64/flatc           # Linux 开发机 / x86 CI
  ├─ linux-aarch64/flatc          # Orin / arm64 CI
  └─ checksums.sha256             # 三份二进制的 sha256
  ```
  - 版本 pin `flatbuffers 24.3.25`（与 Rust runtime crate 对齐）
  - 升级频率 < 1/年，手动流程，不在 CI 自动拉新版本
- `scripts/refresh_flatc.sh`：从 `github.com/google/flatbuffers/releases/download/v<VERSION>/` 拉三份官方 release 二进制，校验 sha256 后解压到 `3rd-party/flatc/<platform>/`；CI 跑一次保证 "vendor 文件 = 官方 release"
- `scripts/refresh_flatc.ps1`：Windows 对应版本（不影响 `robot-msg` 构建，用户若只在 macOS/Linux 开发可忽略）

### 测试

- 每个 table round-trip
- 兼容性：v1 writer → v2 reader、v2 writer → v1 reader
- 新字段加在末尾的兼容性
- deprecated 字段 id 不可复用（`schema_lint` bin 拦截）
- schema_id 唯一性（`schema_lint` bin）
- `layout_id` 碰撞检测（`schema_lint` 跑 blake3 固定测试向量）
- `refresh_flatc.sh` 重跑后 vendor 文件 sha256 不变

### 退出标准

- 所有兼容性组合过（运行时）
- `schema_lint` bin 在当前 schema 集上零错误（静态）
- `flatc` 完全 vendor 化，`build.rs` 不尝试调用系统 `flatc`；无网环境可 build
- 不依赖 host 有 Python 环境（无 Python lint 脚本）

### 设计文档参考

§2.2, §3.4, §3.5, §3.5.1

---

## Step 3 · FrameHeader / Body / Blob parser

### 目标

sample 解析 + verify，按 §2.2 的 9 步验证顺序。

### 交付

- `crates/robot-transport/src/parser.rs`:
  - `pub fn parse_sample(bytes: &[u8]) -> Result<ParsedSample<'_>, ParseError>`
  - 9 步验证：`size ≥ 4` → 读 `H` → `0 < H ≤ 4096` → verify header → body/blob offset 在 bound → body-blob 不重叠 → 查 registry
- `ParseError` 分类：`SizeTooSmall` / `HeaderSizeInvalid` / `HeaderVerifyFailed` / `BodyOutOfBounds` / `BlobOutOfBounds` / `BlobOverlap` / `UnknownSchemaId`
- metric hook：每种错误计数

### 测试

- 正常 sample 解析
- 每种错误路径单测
- fuzz 测试（1 分钟 / 目标不 panic）
- 边界：`sample_size = 4`、`H = 4096`、`body_size = 0`

### 退出标准

- 所有错误路径可复现
- 无 panic / 无 unwrap（test 除外）
- fuzz 1 分钟 0 panic

### 设计文档参考

§2.2

---

## Step 4 · `SampleLease<T>` / `SampleWriter<T>`

### 目标

RAII 封装 + 生命周期严格；`view()` 借用不能 outlive lease。

### 交付

- `crates/robot-transport/src/sample.rs`:
  - `pub struct SampleLease<T>`（值语义持有后端 loan；drop 归还）
  - `impl<T> SampleLease<T> { fn view(&self) -> Result<MessageView<'_, T>, TransportError>; }`
  - `pub struct MessageView<'a, T> { header: HeaderView<'a>, body: BodyView<'a, T>, blob: Option<BlobView<'a>> }`
  - `pub struct SampleWriter<'a, T>`（`publish` 按值 consume；drop 未 publish = discard loan）
  - trait：`LatestSubscriber<T>` / `Publisher<T>` / `RequestClient<Req, Resp>`
- 后端：mock 内存实现（`Vec<u8>` 模拟 sample buffer）

### 测试

- `SampleLease` drop 归还 loan（用 mock 计数验证）
- `trybuild` 负向测试：`MessageView` outlive `SampleLease` → 编译失败
- `trybuild` 负向测试：`SampleWriter` publish 后继续写 → 编译失败
- `SampleWriter` 未 publish drop 自动 discard

### 退出标准

- `trybuild` 所有负向测试通过（即按预期编译失败）
- mock 后端无 leak

### 设计文档参考

§2.5, §4.3

---

## Step 5 · mock iceoryx2 pub/sub

### 目标

真正的 iceoryx2 service，但 publisher 和 subscriber 同进程；验证 trait 封装在真后端上工作。

### 交付

- `crates/robot-transport/src/iceoryx2/`:
  - `Iceoryx2Publisher<T>` / `Iceoryx2Subscriber<T>`
  - QoS 按 §2.4 的类别映射
  - service 创建时声明 max sample size / history depth / drop policy
- `crates/robot-tests/src/bin/hello_pubsub.rs` 示例（一个进程内 pub + sub，打印收到的 JointState）

### 测试

- 同进程 pub → sub 收到 JointState
- sample size 超限拒绝 loan
- drop subscriber 不影响 publisher
- 100 Hz pub/sub 跑 1 分钟无 drop

### 退出标准

- 夜间 1 h soak：command topic drop = 0
- unknown_layout_count = 0（warmup 后）

### 设计文档参考

§2.1, §2.3, §2.4

---

## Step 6 · S0 四线程 mock（**最小闭环**）

### 目标

最小闭环关键一步。本步完成后整条链路贯通（见设计文档 §11.2 图）。

### 前置

Linux（`SCHED_FIFO` + `mlockall` + `sd_notify`）。macOS 无法跑，转 Linux 开发机或 Orin。

### 交付

- `crates/robot-s0/src/`:
  - `ipc_thread.rs`：iceoryx2 订阅 command → 写 atomic latest-command slot
  - `rt_thread.rs`：`SCHED_FIFO` + `mlockall` 骨架 + 读 command slot → 调 mock HardRT adapter → 写 seqlock snapshot
  - `pub_thread.rs`：读 snapshot → iceoryx2 publish state / heartbeat（**不做 adapter I/O**）
  - `io_thread.rs`：v0.2 step 6 先 no-op 占位
  - `state_machine.rs`：SAFE / READY / ENABLED.ACTIVE 三态
- `crates/robot-adapters/mock/`：mock HardRT adapter，内存模拟关节（`rt_class = HardRT`）
- `crates/robot-s1/src/bin/mock_s1.rs`：最简 command 发送循环 + heartbeat 50 Hz
- `crates/robot-cli/src/bin/robotctl.rs`：subscribe state → stdout

### 测试

- 启动 `mock_s1` + `robot-s0` + `robotctl`：看到 state 流
- RT thread `assert-no-alloc` 钩子：init 后零 allocation
- RT thread `rt_arc_guard`：零 `Arc::clone/drop`
- `kill -9 mock_s1` 观察 S0 行为（正式验证放 step 7）

### 退出标准

- **最小闭环贯通**（§11.2 图）
- 1 h soak：无 drop、无 RT overrun

### 设计文档参考

§1.2, §1.3, §1.4, §4.4, §4.5, §4.6

---

## Step 7 · watchdog + `CONTROL_LOST_*` 切换

### 目标

S1 stall 时 S0 按 `on_timeout` 切 ENABLED 子态，主状态保持 ENABLED。

### 交付

- `crates/robot-s0/src/watchdog.rs`:
  - 订阅 `/<robot>/s1/heartbeat`，200 ms 超时检测
  - 子态切换：Hold / Zero / Stop
  - state topic 同时报告 `mode` / `control_link` / `output_mode`
- `mock_s1` 加 `--freeze-after-ms N` 测试钩子（模拟 stall）

### 测试

- `kill -9 mock_s1` → S0 在 < 250 ms 进入 `ENABLED.CONTROL_LOST_HOLD`
- 重启 `mock_s1` → 新 seq 单调 → S0 回 `ENABLED.ACTIVE`
- 不同 `on_timeout` 值分支分别触发

### 退出标准

§10.1 Safety：`S1 crash → S0 safe output < 250 ms`

### 设计文档参考

§1.4, §6.2, §6.3

---

## Step 8 · EStop latch 文件双写 + 重启恢复

### 目标

EStop 持久化 + 重启后保持 latched。

### 交付

- `crates/robot-s0/src/estop_latch.rs`:
  - 双文件 + `fsync`：`/var/lib/robot/<id>/estop_latch{,.bak}`
  - 启动读回 + 一致性校验（两文件不一致取较严格那一份）
  - 磁盘损坏 / 读不到 → 启动即拒绝进入 READY
- mock GPIO button trigger（Orin 上再接真按钮）

### 测试

- 触发 EStop → 双文件存在 → 重启 S0 → 读回 latched
- 手动损坏一个文件 → 启动仍 latched（取 latched）
- 两文件都损坏 → 启动拒绝 READY，进入 SAFE
- `ClearError` 不清除 EStop
- `ClearEStop` (maintainer+) 清除 → 文件消失

### 退出标准

§10.1 Safety：`EStop latch 持久化往返` 通过

### 设计文档参考

§1.4, §6.5, §6.6

---

## Step 9 · crash/restart 测试套

### 目标

随机 kill 各组件，验证 S0 输出不丢 safe + 幂等。

### 交付

- `crates/robot-tests/src/bin/crash_restart.rs`:
  - 随机 `kill -9` `mock_s1` / policy / rpc-gateway 组合
  - 验证 S0 输出 0 tick drop
  - `idempotency_key` 幂等性测试（同 key 重放 3 次副作用发生 1 次）
- CI 夜间 job：1 h

### 测试

- 1000 次随机 kill：S0 0 次意外输出
- 幂等 RPC 重放 3 次：副作用 1 次

### 退出标准

§10.1 crash/restart 指标全通

### 设计文档参考

§6.2–§6.5, §10.2

---

## Step 10 · Feetech 单关节（真硬件）

### 目标

第一次上真硬件。HardRT adapter。

### 前置

- Orin（Linux arm64）
- Feetech STS3215 × 1 + USB-RS485
- **硬件侧配置 command timeout**（典型 100 ms）—— 参考 dimos 项目 `docs/06_lerobot_replace_spec.md`
- 校准 JSON 到位

### 交付

- `crates/robot-adapters/feetech/`：
  - 从 dimos `src/xlerobot_dimos/common/feetech_io/` 移植为 Rust
  - `rt_class = HardRT`；寄存器操作序列严格按 dimos `06_lerobot_replace_spec.md`
  - 单元测试 + Orin 上的硬件测试
- 真 `SCHED_FIFO` + `mlockall` + CPU affinity

### 测试

- `cyclictest` 1 h p99 < 1.5 ms，max < 5 ms
- 单关节指令跟踪（position mode）
- `kill -9 s0` 验证硬件 fail-safe < 500 ms（电机 torque disable）

### 退出标准

§10.1 S0 loop 指标全通

### 设计文档参考

§4.6, §6.5.1, §10.1
+ dimos `docs/06_lerobot_replace_spec.md`

---

## 里程碑

| 里程碑 | 覆盖 | 平台 |
|--------|------|------|
| v0.2-m1 | Step 0–4 | macOS（纯 Rust 类型 + mock 后端） |
| v0.2-m2 | Step 5–6 | Linux 开发机（最小闭环） |
| v0.2-m3 | Step 7–9 | Linux 开发机（安全链路） |
| v0.2-final | Step 10 | Orin + 真硬件 |

---

## 不要做的事

1. **不跳步**。Step 6 闭环打通前不接任何真硬件。
2. **不改 `docs/00_design.md`**。发现语义问题 → 写到 `docs/CHANGELOG.md` 讨论。
3. **不放松 RT thread 限制**（禁 alloc / Arc / IO / panic / format）。
4. **不引入 tokio 到 `robot-core`**。
5. **不用 `google.protobuf.Any`**（已废弃选项）。
6. **严格不用 stable；nightly 是硬要求**。核心 crate（`robot-core` / `robot-msg` / `robot-transport` / `robot-s0` / `robot-s1`）默认不写 `#![feature(...)]` —— 只为降低 nightly 升级回归风险，不是为了留 stable 后门。见 `docs/00_design.md` §9 与 `docs/CHANGELOG.md` 2026-04-21 条。
7. **`cargo fmt` + `cargo clippy -D warnings` 不过不提交**。

---

## 何时更新本文档

- 每完成一步，在开头 "当前位置" 行更新状态。
- 发现步骤设计有遗漏 → 只改本文档，不改 `00_design.md`。
- 步骤里承诺的验证指标按实测 baseline 更新 → 同步到 `docs/10_verification.md`（见 `00_design.md` §10）。
