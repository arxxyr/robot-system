# 09 · 通用机器人框架设计（v0.1.2 冻结版）

> **代号**：`robot-core`（暂定，后续可改）
> **目标**：为多种机器人（XLeRobot、Piper、移动底盘、未来四足/人形）提供统一的 Rust 内核。S0/S1/S2 三层分离、iceoryx2 + FlatBuffers 传输、RPC 作为对外边界。
> **适用平台**：Linux x86_64 / Linux arm64（Jetson）；Windows 只做 S2 客户端。
> **状态**：v0.1.2 **已冻结**。合并了 v0.1.1 + v0.1.2 修订 + 评审补充 3 条（`SampleWriter` 生命周期、`frame_header_size ≤ 4096`、RtSnapshot 内存序）。进入 v0.2 实现阶段，架构语义不再改动。
> **变更历程**：v0 → v0.1（附录 C）→ v0.1.1（附录 D）→ v0.1.2（附录 E）。历史快照见 [`archive/framework_history/09_robot_framework_v0.md`](../archive/framework_history/09_robot_framework_v0.md) 与 [`archive/framework_history/11_robot_framework_v0_1_2.md`](../archive/framework_history/11_robot_framework_v0_1_2.md)。

---

## 0. 设计原则

1. **上层不能拖下层**。S0 绝不等 S1，S1 绝不等 S2；每层有独立 deadline 与 fallback。
2. **进程边界走 iceoryx2；但 S0 RT thread 不直接调用 iceoryx2**。详见 §1.2。
3. **Schema-first**。所有跨进程数据必须有 FlatBuffers schema，禁止 ad-hoc 结构。
4. **Zero-copy 优先**。大块数据（图像、点云）不序列化、不 memcpy，只走共享内存。
5. **内部 pub-sub，外部 RPC**。内部一切异步 + fire-and-forget；只有外部调用才允许同步请求-响应。
6. **一个机器人 = 一个 systemd tree**，而非一个进程。安全层永远是独立二进制。
7. **Trait over inheritance**。adapter / policy 一律 trait object，禁止继承式 class 层次。
8. **Python 不进 S1 主进程**。大型模型 / VLA / Torch 必须跑在独立 policy host 进程（§4.7）。
9. **静态链接优先**。v0.x 不做 runtime plugin；所有 adapter 编译期 inventory 注册。
10. **最小依赖**。`robot-core` 零 runtime 依赖；zero iceoryx2、zero pyo3、zero tonic。

### Allocator 策略

| 进程 | 分配器 | RT 约束 |
|------|--------|--------|
| `robot-s0` (RT thread) | 系统默认 | **init 后禁止 heap allocation** |
| `robot-s0` (IPC / Pub / IO thread) | 系统默认 | 允许分配，但预分配优先 |
| `robot-s1` | 系统默认 | coordinator hot path 禁分配；policy host / I/O thread 可分配 |
| `robot-rpc` / `robot-cli` / `robot-viz` / `robot-s2-bridge` | mimalloc | 无 RT 约束 |

**RT thread 禁止清单**：`Vec::push`、`String::new`、`Box::new`、`format!`、日志格式化、带 format arg 的 `tracing::info!`、文件 IO、网络 IO、`panic!`（S0/S1 二进制 `panic = "abort"`）、动态分发到未审计 SDK、`FlatBufferBuilder` 构造、`Arc::clone` / `Arc::drop`（§4.9）。
RT loop 启动前通过 `mlockall(MCL_CURRENT | MCL_FUTURE)` 锁页，并预触所有 buffer。CI 加 allocation-guard（`assert-no-alloc` 或自研钩子）。

---

## 1. 分层模型（System 0 / 1 / 2）

### 1.1 层次概览

```
┌──────────────────────────────────────────────────────────────┐
│  S2  慎思层  (1–10 Hz / event)                               │
│   ├─ LLM planner    ├─ 任务分解    ├─ 场景理解 / 语义建图     │
│   └─ 对话 / HMI                                              │
│   语言: Python / Rust                                         │
│   默认路径: RPC gateway (跨机 / 沙箱 / 脚本)                  │
│   高性能路径: pyo3 + iceoryx2 (venv + shm 权限对齐)           │
│   runtime: asyncio / tokio                                   │
└─────────────────────────▲────────────────────────────────────┘
                          │ iceoryx2 pub-sub + RPC（白名单）
┌─────────────────────────▼────────────────────────────────────┐
│  S1  快反射层 (50–200 Hz)                                     │
│   ├─ Coordinator + Arbiter  ├─ VLA chunk executor            │
│   ├─ Trajectory tracker     ├─ 避障 / WBC / IK               │
│   └─ Policy runtime host (In-Process only)                   │
│   语言: Rust  ‖  runtime: 裸线程 + tokio（I/O 部分）          │
└─────────────────────────▲────────────────────────────────────┘
                          │ iceoryx2 pub-sub (shm, μs)
┌─────────────────────────▼────────────────────────────────────┐
│  S0  反射层  (500 Hz – 1 kHz)                                 │
│   ├─ 硬件驱动 (motor / IMU / FT)  ├─ 软 / 硬限位检查           │
│   ├─ Watchdog / Deadman           ├─ E-Stop latch            │
│   └─ 状态机：SAFE / READY / ENABLED / FAULT / STOPPING        │
│   语言: Rust (+ C++ FFI for 厂家 SDK)                        │
│   线程策略: 四线程（IPC / RT / Pub / IO），独立二进制进程      │
└──────────────────────────────────────────────────────────────┘
```

### 1.2 S0 内部线程模型（四线程）

iceoryx2 是 S0 的**进程边界**通信机制，**不是 S0 RT loop 的直接依赖**。S0 进程至少四类线程，各司其职，互不跨界：

```
                        ┌──────────────────────┐
     iceoryx2 ─────────►│  s0_ipc_thread       │
    (S1 command)        │  普通优先级           │
                        │  FlatBuffers 解析     │
                        │  写 latest-command slot
                        └──────────┬───────────┘
                                   │ atomic / SPSC ring
                                   ▼
                        ┌──────────────────────┐
  HardRT adapter ◄──────┤  s0_rt_thread        │
   (寄存器/DMA/CAN)     │  SCHED_FIFO          │
                        │  500 Hz – 1 kHz      │
                        │  读 latest-command    │
                        │  写 rt_snapshot      │
                        └──────────┬───────────┘
                                   │ seqlock
                                   ▼
                        ┌──────────────────────┐
                        │  s0_pub_thread       │
                        │  普通优先级           │
                        │  读 rt_snapshot       │
     iceoryx2 ◄─────────┤  publish state/error/│
   (S0 state)           │  heartbeat           │
                        │  不做任何 adapter I/O │
                        └──────────────────────┘

                        ┌──────────────────────┐
  SoftRT adapter ◄──────┤  s0_io_thread[0..N]  │
   (USB 相机/           │  普通优先级           │
    vendor C++ SDK/     │  poll SoftRT adapter │
    网络传感器)          │  写 softrt_snapshot  │
                        └──────────────────────┘
```

**要点**：
- **S0 RT thread 不直接调用 iceoryx2**。
- 四类线程边界：
    - `s0_ipc_thread`：订阅 iceoryx2 command / heartbeat / event；写 latest-command slot。
    - `s0_rt_thread`：`SCHED_FIFO`；读 command slot → 调 HardRT adapter → 写 `rt_snapshot`。禁止一切 pub-sub。
    - `s0_pub_thread`：读 `rt_snapshot` → 发布 state / error / heartbeat。**不做任何 adapter I/O**，保持 Pub 线程纯净——一旦阻塞，S1 watchdog 超时会导致整机 CONTROL_LOST。
    - `s0_io_thread[0..N]`：按 SoftRT adapter 数量实例化（也可共享一根）。poll SoftRT adapter（USB 相机、vendor C++ SDK、网络传感器），结果写入独立 `softrt_snapshot`。v0.2 若无 SoftRT 可先不起此线程。
- RT 线程只访问预分配内存：`atomic` command slot + `seqlock` snapshot + adapter DMA buffer；不参与 pub-sub、不做 heap allocation、不 `Arc::clone/drop`（§4.9）。

### 1.3 S0 RT thread 启动流程

```
1. 读取配置 (TOML)
2. 打开硬件 (串口 / USB / SPI / CAN)
3. 分配所有 buffer (ring / snapshot / command slot)
4. 初始化 SPSC ring / seqlock
5. 预触页 (逐 page 读写所有 buffer)
6. mlockall(MCL_CURRENT | MCL_FUTURE)
7. sched_setscheduler(SCHED_FIFO, priority)
8. sched_setaffinity(CPU)
9. 进入 loop
```

### 1.4 S0 状态机

```
主状态:        SAFE / READY / ENABLED / FAULT / STOPPING
ENABLED 子态:  ACTIVE / CONTROL_LOST_HOLD / CONTROL_LOST_ZERO / CONTROL_LOST_STOP
持久化标志:    EStopLatched / ManualOverride
```

状态 transition（每条 transition 的 guard / action / failure 详见 `docs/10_robot_framework_verification.md`，v0.2 开工前必须产出）：

| transition | trigger | guard | action | failure |
|------|------|------|------|------|
| SAFE → READY | `enable()` | `!estop_latched && adapter_connected && calibration_loaded && joint_state_valid && power_ok` | reset watchdog; publish READY | stay SAFE |
| READY → ENABLED.ACTIVE | heartbeat | `heartbeat_ok && command_deadline_valid && limits_valid && hw_enable_ok` | enable servo | stay READY |
| ENABLED.ACTIVE → ENABLED.CONTROL_LOST_* | `s1_heartbeat_timeout` | — | 按最近一条 `JointCommand.on_timeout` 切子态 | — |
| ENABLED.CONTROL_LOST_* → ENABLED.ACTIVE | `s1_heartbeat_ok` | 新命令 seq 单调 | resume tracking | — |
| ENABLED.* → FAULT | `adapter_err` / `cmd_deadline_expired` / `limit_violation` / `watchdog_timeout` / `rt_overrun` | — | disable output; latch fault code | — |
| * → SAFE | `estop()` / physical button | — | cut output; latch EStop (持久化) | — |
| FAULT → READY | `ClearError` RPC | fault cleared by adapter; `!estop_latched` | reset fault code | — |
| EStopLatched → cleared | `ClearEStop` RPC (maintainer+) | 人工确认 guard | clear estop latch | — |

**语义澄清**：
- `ENABLED` 主状态代表硬件仍使能（电机未 torque disable，除非 `CONTROL_LOST_STOP` 子态显式要求）。
- `CONTROL_LOST_*` 子态明确告诉上层 / UI / recorder "控制链路已丢"。避免把 `ENABLED` 误读为 "控制正常"。
- `GetState` / state topic 必须同时报告 `mode`、`control_link`（OK / Lost）、`output_mode`（Active / Hold / Zero / Stop）。
- `ClearError ≠ ClearEStop`：EStop 必须由独立 RPC 显式解除；重启 S0 不清除 EStop（持久化到磁盘，§6.6）。

### 1.5 S1 状态机

`BOOTING → HOMING → IDLE → EXECUTING → DEGRADED → SHUTTING_DOWN`

- `DEGRADED`：部分硬件 / sensor 故障或 policy 超时，只读或低能力运行；拒绝新 task，已有 task 进入 safe hold。
- 状态 transition guard 同样进 verification doc。

### 1.6 控制边界约束

| 来源 | 允许 publish 的 topic |
|------|----------------------|
| S0 | `/<robot>/s0/<hw>/state`、`/<robot>/s0/<hw>/error` |
| S1 coordinator/arbiter | `/<robot>/s0/<hw>/command`、`/<robot>/s1/**` |
| S2 bridge | `/<robot>/s2/goal`、`/<robot>/s2/task_intent`、`/<robot>/s2/policy_hint` |
| S2 bridge **不得** publish | `/<robot>/s0/**`、`/<robot>/s1/trajectory/**`、`/<robot>/s1/cmd_vel` |
| External PolicyHost | `/<robot>/policy/<name>/action_chunk` |

Topic ACL 分三层强制（不承诺"所有错误编译期拦截"，因为 `robot_id` / `hardware_id` / policy 名等来自 runtime 配置）：

1. **编译期 TopicClass 约束**：`Publisher<TopicClass, Msg>` 限制某类进程只能构造某类 topic。例如 `S2GoalPublisher` 无法构造 `S0CommandTopic`（类型不匹配 → 编译失败）。
2. **启动期配置校验**：按 `robot_id` / `hardware_id` / `source_id` / `schema_id` 对照 `configs/topic_acl.toml` 校验；不通过进程拒绝启动。
3. **运行期 source 校验**：每条消息 header 带 `source_id`，subscriber 按 ACL 拒绝非法来源，计数 `unauthorized_publish_count`；超阈值升级 DEGRADED。

---

## 2. 数据与传输架构

### 2.1 三件套分工

| 组件 | 位置 | 职责 |
|------|------|------|
| **iceoryx2** | 本机跨进程 | 基于 POSIX shm 的 pub-sub + event + request-response。内部通信唯一载体。 |
| **FlatBuffers** | 消息格式 | Header / metadata / 控制命令 / 小状态消息。schema 驱动 + 版本化。 |
| **Raw payload** | shm 尾部 | 大块连续数据（图像、点云、音频、深度图、URDF）。不序列化，FlatBuffers header 携带 offset / size / dtype / shape。 |

### 2.2 消息 wire 格式

**不使用全局 body union**。Header 只描述 body 位置与类型，body 按 `schema_id` 路由到对应 FlatBuffers root table。sample 前 4 字节是固定小端 `frame_header_size`，让接收端先切包再解析：

```
┌─────────────────────────────────────────────────────────────┐
│  [0..4]  u32 LE  frame_header_size (= H)                    │
├─────────────────────────────────────────────────────────────┤
│  [4..4+H]  FrameHeader (FlatBuffers root, H 通常 < 256 B)   │
│    ├─ magic: u32               ("RBTC" = 0x52425443)        │
│    ├─ abi_version: u16                                       │
│    ├─ schema_id: u32           （全局唯一 message type id） │
│    ├─ schema_version: u16                                   │
│    ├─ capability_flags: u64    （见 §3.4）                  │
│    ├─ stamp_ns: u64            （CLOCK_MONOTONIC_RAW）      │
│    ├─ source_id: u32           （全局唯一发布者 id）        │
│    ├─ seq: u64                                              │
│    ├─ body_offset: u32         （sample 绝对 offset）       │
│    ├─ body_size: u32                                        │
│    ├─ blob_offset: u32         （sample 绝对 offset，可 0） │
│    ├─ blob_size: u32           （可 0）                     │
│    └─ header_crc32: u32        (可选)                       │
├─────────────────────────────────────────────────────────────┤
│  [body_offset..body_offset+body_size]                       │
│  Body (FlatBuffers root, 按 schema_id 路由)                  │
│    e.g. JointState / JointCommand / Twist / ImageHeader     │
├─────────────────────────────────────────────────────────────┤
│  [blob_offset..blob_offset+blob_size] (HAS_BLOB 时)         │
│  Raw blob，按 BlobDesc.alignment 对齐                        │
└─────────────────────────────────────────────────────────────┘

解析约束（接收端按此顺序验证；任何一步失败 → drop sample + 计 metric + 不 panic）：
  1. sample_size >= 4
  2. 读 u32 LE: frame_header_size H
  3. **0 < H <= 4096**（parser 防御性上限，拒绝坏数据 / bug）
  4. flatbuffers::root::<FrameHeader>(&sample[4..4+H]) with verifier
  5. body_offset >= 4 + H
  6. body_offset + body_size <= sample_size
  7. 若 HAS_BLOB: blob_offset + blob_size <= sample_size 且 blob alignment 满足
  8. body_offset 区间与 blob_offset 区间不重叠
  9. 按 schema_id 查 registry，解析 body
```

- `robot-msg` 维护全局 schema registry：`schema_id → RootType` 映射；新增类型只加 id，不复用废弃 id。
- CI 跑兼容性测试：old-reader-new-writer / new-reader-old-writer；deprecated 字段标注不可删除。

**Blob offset 语义约定（v0.1.2 单 blob）**：
- `FrameHeader.blob_offset` / `blob_size` 描述 sample 内唯一 raw blob 的总区域（sample 绝对 offset）。
- Body 内的 `BlobDesc.offset` 若出现，语义为 blob 内子区域的相对偏移；v0.1.2 **仅允许单 blob**，`BlobDesc.offset = 0`、`BlobDesc.size = FrameHeader.blob_size`。
- 多 blob（RGB + depth + mask 共用一个 sample）或子区域切分推迟到 v0.2 显式扩展，届时再引入 `blob_table`。

### 2.3 Topic 命名空间

```
/<robot_id>/s0/<hardware_id>/state           # 关节状态上报（500 Hz）
/<robot_id>/s0/<hardware_id>/command         # S1 → S0 指令
/<robot_id>/s0/<hardware_id>/error           # 错误上报
/<robot_id>/s1/heartbeat                     # S1 → S0 心跳（50 Hz）
/<robot_id>/s1/joint_state                   # 整机融合关节状态（100 Hz）
/<robot_id>/s1/cmd_vel                       # 底盘速度
/<robot_id>/s1/trajectory/<task_name>        # 轨迹命令
/<robot_id>/s1/task_status                   # 任务状态广播
/<robot_id>/sensors/<sensor_id>/color        # 相机彩色
/<robot_id>/sensors/<sensor_id>/depth        # 相机深度
/<robot_id>/sensors/<sensor_id>/info         # 相机内参
/<robot_id>/s2/goal                          # S2 下发高层目标
/<robot_id>/policy/<name>/observation        # S1 → PolicyHost 观测
/<robot_id>/policy/<name>/action_chunk       # PolicyHost → S1 动作块
/<robot_id>/events/estop                     # 紧停事件
```

规范：小写 + 下划线；永远以 `/` 开头；每个 topic 对应唯一的 schema_id。

### 2.4 QoS 默认值（细化）

| 类别 | 典型 topic | history | depth | stale_after | drop policy |
|------|-----------|---------|-------|-------------|-------------|
| S0 command | `/<robot>/s0/<hw>/command` | keep-last | 1 | 必须（body.deadline_ns 决定） | 超时由 S0 按 `on_timeout`（Hold/Zero/Stop）处理 |
| S0 state | `/<robot>/s0/<hw>/state` | keep-last | 2 | 无 | drop old |
| S1 trajectory | `/<robot>/s1/trajectory/<task>` | keep-all | 8 | `trajectory_id` + `segment_id` + `valid_from/to` | 不得静默丢弃；丢弃必报警 |
| S1 cmd_vel | `/<robot>/s1/cmd_vel` | keep-last | 1 | 硬值（默认 100 ms） | 超时 → S0 归零 |
| Sensor / Image | `/<robot>/sensors/**` | keep-last | 2–4 | 无 | drop old |
| Safety event | `/<robot>/events/estop` / error | keep-all | 32 | 无 | 不得静默丢弃；满了必须升级 fault 或写 emergency log |
| Task event | `/<robot>/s1/task_status` | keep-all bounded | 64 | 无 | 丢弃必须可观测（metric + log） |
| RPC response | iceoryx2 req/resp | keep-last | 1 | RPC deadline | 超时即失败 |

### 2.5 Sample 生命周期规则

- 所有 `MessageView<'a, T>`、`BlobView<'a>` 的生命周期不得超过 iceoryx2 sample lease。
- 禁止把 `BlobView` 存入长期结构（`Arc`、`Box<dyn Any>`、全局 cache）。
- 禁止把 `BlobView` 发送到 async task 或其他线程，除非显式 `to_owned()` 或 `pin_lease()`。
- 高频控制消息只暴露 latest 语义。
- 大 blob topic 必须在 service 创建时声明最大 sample size、history depth、drop policy。

Rust API（`SampleLease<T>` 泛型带住 schema 类型、值语义持有 iceoryx2 loan；`MessageView<'_, T>` 从 lease 借用，Rust 借用检查保证视图不能 outlive lease）：

```rust
pub struct SampleLease<T> {
    // 值语义持有 iceoryx2 sample loan；drop 自动归还
    _ty: PhantomData<T>,
}

impl<T> SampleLease<T> {
    pub fn view(&self) -> Result<MessageView<'_, T>, TransportError>;
}

pub struct MessageView<'a, T> {
    pub header: HeaderView<'a>,
    pub body:   BodyView<'a, T>,
    pub blob:   Option<BlobView<'a>>,
}

pub trait LatestSubscriber<T> {
    fn latest(&mut self) -> Result<Option<SampleLease<T>>, TransportError>;
}
```

调用模式：

```rust
if let Some(lease) = subscriber.latest()? {
    let view = lease.view()?;
    consume(view);
    // lease 出作用域自动归还 loan；view 的借用随 lease 失效
}
```

不对外暴露裸 `&[u8]` 给长期对象持有。

---

## 3. 消息 Schema 规范

### 3.1 单位 & 坐标系

| 物理量 | 单位 | 备注 |
|--------|------|------|
| 长度 | 米 m | |
| 角度 | 弧度 rad | wrap 到 `[-π, π]` 除非明确标注累积角 |
| 线速度 | m/s | |
| 角速度 | rad/s | |
| 力 | N | |
| 力矩 | N·m | |
| 时间戳 | ns (u64, 单调时钟) | 源自 `CLOCK_MONOTONIC_RAW` |

**坐标系约定**：REP-103（ROS 兼容）—— x 前，y 左，z 上；旋转右手系。
**Frame ID 约定**：`<robot_id>/base`、`<robot_id>/odom`、`<robot_id>/<joint>`。

### 3.2 时钟域（硬约束）

- 所有 `stamp_ns` / `deadline_ns` / `valid_from` / `valid_to` 必须使用 `CLOCK_MONOTONIC_RAW`。
- **禁止** wall clock / `gettimeofday` / `SystemTime::now()`。
- 跨机 RPC 传入 deadline：gateway 必须把远端 deadline（wall 或 monotonic）或 `timeout_ms` 转换为**本机** monotonic deadline，并在 header 记录转换误差。
- Record 文件除 monotonic stamp 外额外记录 wall clock offset，用于回放对齐。

### 3.2.1 时钟封装（禁止散落调用）

`CLOCK_MONOTONIC_RAW` 不受 NTP / `adjtime` 影响，但多数 Linux sleep / timer API（`clock_nanosleep`、`tokio::time::Instant`）默认基于 `CLOCK_MONOTONIC`。为避免时钟域不一致，业务代码**禁止**直接调用：

- `SystemTime::now()` / `std::time::Instant::now()`
- `clock_gettime(CLOCK_REALTIME | CLOCK_MONOTONIC)`
- `tokio::time::Instant::now()`

统一走 `robot-core::time`：

```rust
pub struct MonoRawNs(pub u64);
pub struct Deadline(pub MonoRawNs);
pub struct DurationNs(pub u64);

pub trait RobotClock {
    fn now(&self) -> MonoRawNs;
    fn sleep_until(&self, d: Deadline);
    fn sleep_for(&self, d: DurationNs);
}
```

若实测发现 sleep 精度需要 `CLOCK_MONOTONIC`（而非 `_RAW`），由 `RobotClock` 实现内部选择合适时钟并在其内部完成换算；业务代码无感。

### 3.3 Float 策略（硬约束）

| 用途 | 类型 |
|------|------|
| S0 / S1 joint position / velocity / effort | `float64` |
| deadline、轨迹时间戳、标定、IK | `float64` |
| Twist / Odometry | `float64` |
| 图像 / 点云 / policy tensor | `float32` |

FlatBuffers 中明确使用 `double` / `float` 字段。不要靠后续重构。

### 3.4 Capability flags

`FrameHeader::capability_flags: u64` 位域（节选）：

```
bit  0: HAS_BLOB
bit  1: BLOB_COMPRESSED_LZ4
bit  2: BLOB_COMPRESSED_ZSTD
bit  3: GPU_MEMORY_HANDLE
bit  4: LITTLE_ENDIAN         (置 1 表示 LE)
bit  5: HEADER_CRC_VALID
bit  6: IS_REPLAY
bit  7: IS_SIMULATION
bit  8: IS_DEGRADED           (源标记本帧来自降级状态)
bit  9: IS_SYNTHETIC          (生成 / 插值 / predicted)
bit 10: BODY_CRC_VALID
bit 11: BLOB_CRC_VALID
bits 12-63: reserved
```

**CRC 默认策略**（避免 3 MB 图像每帧算 CRC 吃 CPU）：

- 默认**关闭**：机内 shm 正常控制路径（S0 command / state、S1 传感器订阅）。
- 默认**开启**：record / replay、跨机 bridge、safety event（`/events/estop`、error）、soak / fuzz 测试、诊断模式。
- Header CRC 开销小（header < 256 B），debug build 默认开启；release build 按上述策略。
- Blob CRC 按消息类型白名单启用（record / 跨机 / 关键事件），默认不对 image / pointcloud 全帧 CRC。

### 3.5 核心 schema

```fbs
namespace robot.msg;

enum ControlMode : byte { Position, Velocity, Torque, ServoPosition, Stop }
enum StaleAction : byte { Hold, Zero, Stop }
enum Dtype : byte { U8, I8, U16, I16, F16, F32, F64, I32, U32 }
enum BlobLayout : byte { HWC, CHW, NHWC, NCHW, PointsXYZ, PointsXYZI, Opaque }
enum Endian : byte { Little, Big }
enum Compression : byte { None, Lz4, Zstd }
enum Device : byte { Cpu, Cuda, Rocm }

table Vec3 { x: double; y: double; z: double; }

// JointLayout：低频（1 Hz 或 change-only），订阅者缓存
table JointLayout {
    layout_id: ulong;           // 64 bits，避免碰撞
    names: [string];
    joint_ids: [uint];
    hardware_id: string;
}

// JointState：高频（500 Hz+），只带 layout_id
table JointState {
    layout_id: ulong;
    position_rad: [double];
    velocity_rps: [double];
    effort_nm: [double];
    stamp_ns: ulong;
    frame_id: string;
}

// JointCommand：高频，只带 layout_id
table JointCommand {
    layout_id: ulong;
    mode: ControlMode;
    target: [double];
    deadline_ns: ulong;         // CLOCK_MONOTONIC_RAW
    priority: ubyte;            // 0~255，大者优先
    source_task_id: ulong;
    on_timeout: StaleAction = Hold;
}

table Twist {
    linear: Vec3;
    angular: Vec3;
    frame_id: string;
    stamp_ns: ulong;
}

// Blob 描述符（内嵌在带 blob 的消息里）
table BlobDesc {
    offset: ulong;              // v0.1.2 固定 0（仅单 blob）
    size: ulong;                // v0.1.2 = FrameHeader.blob_size
    alignment: ushort;
    dtype: Dtype;
    shape: [uint];
    strides: [int];
    layout: BlobLayout;
    endian: Endian;
    compression: Compression;
    crc32: uint;                // 由 capability_flags::BLOB_CRC_VALID 决定
}

table ImageHeader {
    blob: BlobDesc;
    width: uint;
    height: uint;
    encoding: string;           // "rgb8", "mono8", "depth16", "bgr8"
    step: uint;                 // bytes per row
    color_order: string;        // "RGB" / "BGR"
    stamp_ns: ulong;
    frame_id: string;
}

table PointCloudHeader {
    blob: BlobDesc;
    point_count: uint;
    point_step: uint;
    fields: [string];
    is_dense: bool;
    stamp_ns: ulong;
    frame_id: string;
}

table TensorHeader {
    blob: BlobDesc;
    device: Device;
    quantization: string;
    stamp_ns: ulong;
}

table EStopEvent {
    reason: string;
    stamp_ns: ulong;
    source: string;
    latched: bool;
}

table Heartbeat {
    source_id: uint;
    seq: ulong;
    tick_rate_hz: double;
    stamp_ns: ulong;
}
```

所有 schema 放 `robot-msg/schemas/*.fbs`；CI 强制演进规则（只加字段、不改类型、deprecated 而非删除）。

### 3.5.1 `layout_id` 缓存失效规则

- 订阅者必须缓存 `JointLayout`，按 `layout_id` 索引。
- 收到未知 `layout_id` 的 `JointState` / `JointCommand`：
  - **不得解析** `position_rad` / `target` 等数组（长度未知）。
  - 必须通过 iceoryx2 request-response 向发布者请求对应 `JointLayout`，或等待下一次周期广播。
  - 累计 `unknown_layout_count` metric；超过阈值升级 DEGRADED。
- `layout_id` 生成规则：`blake3(robot_id, hardware_id, sorted_joint_names, sorted_joint_ids, order).truncate(8 bytes)` → `u64`；也可配置显式指定并由 CI 校验唯一性。
- `JointLayout` 发布频率：1 Hz 周期 + change-only 触发；订阅者冷启动后 ≤ 1 s 必能拿到。
- Replay / late subscriber：recorder 必须把每个活跃 `layout_id` 对应的 `JointLayout` 写入文件头，保证离线解析不依赖外部状态。

---

## 4. Rust Core 内部结构

### 4.1 模块地图

```
robot-core/          # 只含 trait、错误、基础类型；零 runtime 依赖
  ├─ adapter.rs      # ManipulatorAdapter / BaseAdapter / SensorAdapter trait
  ├─ policy.rs       # Policy trait（输入: Observation → 输出: Action）
  ├─ task.rs         # Task trait + TaskKind (Trajectory / Velocity / Servo / Safety)
  ├─ error.rs        # 统一 RobotError
  └─ time.rs         # MonoRawNs / Deadline / RobotClock

robot-msg/           # FlatBuffers schema + 生成代码 + 类型化 wrapper
  ├─ schemas/*.fbs
  ├─ src/gen/...
  └─ tests/compat.rs # old-reader-new-writer / new-reader-old-writer

robot-transport/     # iceoryx2 封装
  ├─ node.rs         # RobotNode（持有 iceoryx2 Node）
  ├─ publisher.rs    # 类型化 Publisher<T> + SampleWriter
  ├─ subscriber.rs   # 类型化 Subscriber<T> + latest() → SampleLease<T>
  ├─ blob.rs         # BlobView / SampleLease<T> / MessageView<'_, T>
  └─ service.rs      # 本机 request-response（仅给 RPC 网关用）

robot-s0/            # 独立二进制
  ├─ bin/s0_daemon.rs
  ├─ ipc_thread.rs            # 订阅 iceoryx2 command
  ├─ rt_thread.rs             # SCHED_FIFO 主循环
  ├─ pub_thread.rs            # 发布 state (纯净，无 adapter I/O)
  ├─ io_thread.rs             # SoftRT adapter polling
  ├─ state_machine.rs         # SAFE / READY / ENABLED / FAULT
  ├─ watchdog.rs              # S1 心跳 + deadman
  ├─ limits.rs                # 软 / 硬限位 + 电流限幅
  ├─ estop_latch.rs           # 持久化 EStop latch
  └─ drivers/                 # 静态链接的 adapter 挂载点

robot-s1/            # 独立二进制
  ├─ bin/s1_daemon.rs
  ├─ coordinator.rs           # 主循环 READ→COMPUTE→ARBITRATE→ROUTE→WRITE
  ├─ arbiter.rs               # per-joint 优先级仲裁
  ├─ scheduler.rs             # task 调度 + deadline 监控
  ├─ tasks/                   # trajectory / velocity / servo / safety / cartesian (IK)
  ├─ policy_host/             # InProcessPolicyHost only（§4.7）
  └─ ik/                      # Option<Box<dyn ArmIkSolver>> 持有 + cartesian task 胶水

robot-kinematics/    # 零 runtime 依赖；trait 层
  ├─ arm.rs          # ArmIkSolver / KinematicInfo / IkSolution / IkError
  ├─ base.rs         # BaseKinematics（body↔wheel、里程计积分）
  ├─ pose.rs         # Pose / Twist / Jacobian 纯类型
  └─ error.rs        # NoSolution / OutOfReach / SingularityTooClose / DeadlineExceeded

robot-kinematics-<robot>/     # 每种机器人一个实现，按 feature flag 挂到 robot-s1
  ├─ xlerobot/                # XLeRobot 双臂 5DoF + 三轮全向（v0.4；移植自 dimos kinematics/）
  ├─ piper/                   # AgileX 单臂（v0.6，结构异构验证）
  └─ <user>/                  # 用户可 fork 自己的实现，不动 s1 / s0 / adapter

robot-policy-host/   # 独立二进制（每个 policy 一个实例）
  └─ src/            # ExternalPolicyHost 实现（Python / ONNX / HTTP）

robot-adapters/      # 每种硬件一个 crate
  ├─ feetech/        # STS3215
  ├─ piper/          # AgileX
  ├─ realsense/      # Intel RS
  ├─ webcam/         # V4L2
  └─ mock/           # 纯内存 mock

robot-s2-bridge/     # pyo3 module
  ├─ pyo3_module.rs  # 只能订阅观测 + 发布 S2 goal，ACL 强制
  └─ python/

robot-rpc/           # 对外 RPC 网关
  ├─ server.rs       # tonic
  ├─ proto/
  └─ auth.rs         # AuthContext + role/capability 校验

robot-viz/           # Rerun / Foxglove 桥
robot-recorder/      # record/replay 独立进程
robot-cli/           # robotctl 工具
robot-tests/         # 端到端冒烟 + soak + crash/restart
```

### 4.2 `robot-core` 依赖铁律

```
不允许依赖：
    iceoryx2, tonic, pyo3, tokio, tracing-subscriber,
    vendor SDK, flatbuffers runtime, serde_json

允许依赖：
    thiserror, smallvec, arrayvec, bytemuck, crc32fast
```

### 4.3 `robot-transport` 最小 trait 集

不一开始做成 DDS 抽象，先服务自身模式。API 与 §2.5 一致：

```rust
pub trait LatestSubscriber<T> {
    fn latest(&mut self) -> Result<Option<SampleLease<T>>, TransportError>;
}

pub trait Publisher<T> {
    fn loan(&mut self) -> Result<SampleWriter<'_, T>, TransportError>;
}

impl<'a, T> SampleWriter<'a, T> {
    pub fn publish(self) -> Result<(), TransportError>;
}

pub trait RequestClient<Req, Resp> {
    fn call(&mut self, req: Req, deadline: Deadline) -> Result<Resp, TransportError>;
}
```

**`SampleWriter<'_, T>` 生命周期语义**（避免 loan 泄漏 / publish 后复用）：

- `loan()` 返回的 `SampleWriter` 持有 iceoryx2 sample 的可写借用，生命周期绑定到 publisher 的借用期。
- **未 publish 即 drop**：自动 discard / release loan，不留悬挂 sample。
- **publish 消费 writer**：`writer.publish()` 按值消费 self，调用后 writer 不可再用；编译期拦截 "publish 后继续写 body/blob"。`publish` 方法挂在 writer 上（而非 `Publisher` trait），避免 writer 持有 `&mut Publisher` 期间再调 `Publisher::publish(&mut self, w)` 的 borrow checker 冲突（v0.1.2 → v0.1.3 文档卫生调整，见 `docs/CHANGELOG.md` 2026-04-22 "§4.3 writer API 调整"）。
- 错误路径（例如 body 编码失败）无需显式 release：直接 `return Err(..)` 让 `SampleWriter` drop，iceoryx2 loan 自动归还。

后端先实现 iceoryx2，保留切换到 Zenoh / 裸 shm 的余地。

### 4.4 Adapter trait

```rust
pub trait ManipulatorAdapter: Send {
    fn info(&self) -> AdapterInfo;
    fn connect(&mut self) -> Result<(), RobotError>;
    fn disconnect(&mut self) -> Result<(), RobotError>;

    fn read_state(&mut self, out: &mut JointStateBuf) -> Result<(), RobotError>;
    fn write_command(&mut self, cmd: &JointCommandView) -> Result<(), RobotError>;

    fn set_mode(&mut self, mode: ControlMode) -> Result<(), RobotError>;
    fn enable(&mut self, on: bool) -> Result<(), RobotError>;
    fn safety_stop(&mut self) -> Result<(), RobotError>;

    fn limits(&self) -> &JointLimits;
}

pub trait BaseAdapter: Send {
    fn read_twist(&mut self) -> Result<Twist, RobotError>;
    fn write_twist(&mut self, t: &Twist) -> Result<(), RobotError>;
    fn read_odom(&mut self) -> Result<Odometry, RobotError>;
    fn enable(&mut self, on: bool) -> Result<(), RobotError>;
}

pub trait SensorAdapter: Send {
    type Frame;
    fn start(&mut self) -> Result<(), RobotError>;
    fn poll_frame(&mut self, deadline: Deadline) -> Result<Option<Self::Frame>, RobotError>;
    fn stop(&mut self) -> Result<(), RobotError>;
}
```

### 4.5 Adapter 静态注册

v0.x **不做** runtime dynamic plugin。所有 adapter 编译期静态链接，通过 `inventory` 注册：

```rust
inventory::submit! {
    AdapterRegistration {
        name: "feetech_sts3215",
        kind: AdapterKind::Manipulator,
        rt_class: RtClass::HardRT,
        factory: |cfg| Box::new(FeetechAdapter::new(cfg)),
    }
}
```

一台机器人一个 feature set：

```toml
# robot-s0/Cargo.toml
[features]
default = []
xlerobot = ["robot-adapter-feetech"]
piper = ["robot-adapter-piper"]
```

runtime 插件 ABI 留到 v1.x 之后。

### 4.6 Adapter 分级：HardRT vs SerialRT vs SoftRT

> **task-30 v9 修订（2026-04-25）**：v0.1.x 原版只有 HardRT / SoftRT 两级，
> 把串口舵机错标 HardRT 导致无法挂真 RT 线程。v9 引入第三级 `SerialRT`，
> 与 SoftRT 区分（SerialRT 总线 latency 可证明，SoftRT 不可证明）。

```
HardRT adapter:
  - read_state / write_command 可进 s0_rt_thread
  - bounded latency (< 100 μs typical)
  - 只做寄存器级 / DMA / SPI / CAN 操作
  - 无 malloc、无 lock wait、无 blocking syscall
  - 走 HardRtCtrlClient（packed AtomicU64 + RT-only safety 路径）
  - 典型：FPGA 总线、内存映射 I/O、专用控制板

SerialRT adapter（task-30 新加）:
  - 禁止挂 s0_rt_thread（单帧 RTT 远超 100 μs，毫秒量级）
  - 挂独立的 s0_serial_io_thread（普通优先级 + select! 主循环）
  - 走 SerialIoCtrlClient（独立 channel + atomic latch + safety_stop_fast）
  - 总线 latency **可证明**：固定波特率 + 固定帧长 + 厂家 fail-safe timeout
  - safety_stop_fast 时延 SLA < 10 ms（13-motor 一次 sync_write Torque_Enable=0）
  - confirm_torque_off RTT SLA < 50 ms（13-motor 一次 sync_read 确认）
  - 典型：Feetech STS3215 / Dynamixel / Modbus RTU 串口设备

SoftRT adapter:
  - 禁止挂 s0_rt_thread
  - 禁止挂 s0_pub_thread（Pub 必须纯净；阻塞 = S1 watchdog 超时）
  - 只能挂 s0_io_thread
  - 结果写入独立 softrt_snapshot，RT thread 通过 latest snapshot 只读
  - 总线 latency **不可证明**（USB / 网络 / 未审计 C++ SDK）
  - 典型：USB 相机、未审计厂家 C++ SDK、网络传感器
```

**判定准则**（决策树）：

1. 单帧 RTT 是否能证明 < 100 μs，且无 alloc / lock / blocking？
   - 是 → `HardRT`
   - 否 → 看 2
2. 总线 latency 是否**可证明**（固定波特率 + 固定协议帧长 + 厂家 fail-safe）？
   - 是 → `SerialRT`（毫秒量级 RTT，但 bounded）
   - 否 → `SoftRT`（USB / 网络 / 未审计 SDK，无 latency 证明）

`AdapterInfo` 中声明等级：

```rust
pub struct AdapterInfo {
    pub name: &'static str,
    pub kind: AdapterKind,
    pub rt_class: RtClass,   // HardRT | SerialRT | SoftRT
    pub vendor_sdk: bool,
}
```

S0 启动时按 `rt_class` 分配线程：

| RtClass | 线程 | 控制句柄 | 紧急路径 |
|---|---|---|---|
| HardRT | `s0_rt_thread`（SCHED_FIFO） | `HardRtCtrlClient` | `RtSafeCtrl::safety_stop_rt`（RT-only fast path） |
| SerialRT | `s0_serial_io_thread`（普通） | `SerialIoCtrlClient` | `safety_stop_fast`（broadcast Torque_Enable=0） |
| SoftRT | `s0_io_thread`（普通） | 无控制句柄（read-only） | n/a |

违反（例如 SoftRT 试图挂 RT 线程，或 SerialRT 试图挂 RT 线程）即拒绝启动。

### 4.7 Policy Host 隔离规则

```
InProcessPolicyHost:
  - 仅允许纯 Rust / 小型 ONNX (ort)
  - 禁止 Python / Torch / 大模型 runtime
  - step() 超时必须立即返回 fallback
  - 失败不得触发 S1 coordinator 阻塞

ExternalPolicyHost:
  - 用于 Python / Torch / π0.5 / VLA / 远程推理
  - 独立进程 robot-policy-host@<name>.service
  - 通过 iceoryx2 订阅 /<robot>/policy/<name>/observation
  - 通过 iceoryx2 发布 /<robot>/policy/<name>/action_chunk
  - 崩溃不影响 S1；coordinator 进入 DEGRADED 或继续 tracking 最后一次 chunk
```

**S1 coordinator 永远不等待 policy**。它只消费最新可用 ActionChunk。

### 4.8 Policy trait

```rust
pub trait Policy: Send {
    fn name(&self) -> &str;
    fn reset(&mut self);
    fn step(&mut self, obs: &Observation, deadline: Deadline) -> Result<Action, PolicyError>;
}
```

实现分级：

- `PidPolicy`、`OnnxPolicy` → InProcess
- `PyPolicy`、`TorchPolicy`、`HttpPolicy` → External

### 4.9 ReloadCalibration 与 immutable snapshot 切换

TOML / JSON / serde 只允许在 **init 阶段**使用。S0 RT loop 进入后不再解析任何文本配置。

`ReloadCalibration` 流程：

```
1. RPC 到达 s0_ipc_thread（普通优先级）。
2. ipc_thread 加载 calibration JSON、校验 range、构造 immutable CalibrationSnapshot。
3. ipc_thread 通过 RtSnapshotPublisher::publish() 发布 new snapshot。
4. RT thread 在每个 tick 的安全点通过 RtSnapshotReader::load() 读 raw pointer。
5. 旧 snapshot 由 ipc_thread（或独立 reclaim 线程）周期 drain_retired() 延迟回收，不阻塞 RT。
```

**硬约束**：RT thread 对 `Arc` / snapshot 的操作限制：

```
RT thread 允许：
  - 通过 RtSnapshotReader::load() 取 guard，读字段

RT thread 禁止：
  - Arc::clone / Arc::drop
  - 成为 snapshot 最后一个引用（避免在 RT 线程析构 → 可能 free()）
  - snapshot 析构在 RT thread 执行

回收责任：
  - 新 snapshot 由 s0_ipc_thread 加载 + 校验 + 构造
  - 旧 snapshot 延迟到 s0_ipc_thread（或 reclaim 线程）每 100 ms drain_retired() 时 drop
```

推荐 API 语义（实现可以是 `arc-swap` / RCU / 手写 hazard pointer；用 `arc-swap` 时必须用 `load()` 而非 `load_full()` 以保证读路径不 bump refcount）：

```rust
pub struct RtSnapshotReader<T: 'static> {
    // 读路径：load raw ptr，不 bump refcount、不 drop
}

impl<T: 'static> RtSnapshotReader<T> {
    /// RT-safe：返回 &T，生命周期绑定到 reader guard
    pub fn load(&self) -> RtSnapshotGuard<'_, T>;
}

pub struct RtSnapshotPublisher<T: 'static> {
    // 写路径：只在普通线程调用
}

impl<T: 'static> RtSnapshotPublisher<T> {
    pub fn publish(&self, new: Box<T>);     // 替换 current，旧值入 retired list
    pub fn drain_retired(&self);            // 普通线程周期调用，回收旧 snapshot
}
```

**内存序约束**（实现方选 epoch / hazard / 延迟队列，但必须满足以下语义）：

- `RtSnapshotPublisher::publish` 发布新指针使用 **release store**。
- `RtSnapshotReader::load` 读取当前指针使用 **acquire load**。
- Retired snapshot 的回收必须晚于所有 RT reader guard 退出 —— 通过 epoch-based reclamation / hazard pointer / 延迟队列保证；**禁止**在 publish 时立即释放旧值，否则可能与 RT reader 并发。
- `drain_retired()` 只在确认无 reader 持有该 epoch 后才真正 drop。

典型实现选择：
- 单 writer + 多 reader + 低频更新 → `arc-swap` 的 `Guard` (`load()`) + `rcu()` 批量回收。
- 需要严格 RT 零依赖 → 手写 epoch counter：publisher 轮转 epoch，reader 进 guard 时记录 epoch；drain 时扫描所有 reader epoch < publish_epoch 才回收。

非法校准（range 冲突、ID 重复、超过软限位）在 ipc_thread 校验时拒绝并返回 RPC 错误，不污染 RT 路径。

---

## 5. RPC 边界（对外）

### 5.1 设计约束

1. **只给外部客户用**：CLI、Web UI、远程监控。S2 优先走 pyo3 + iceoryx2，跨机时才经 RPC。
2. **不做内部通信**。S0 ↔ S1 ↔ External PolicyHost 全部 iceoryx2。
3. **强类型 + 可发现**。用 gRPC（tonic）；FlatBuffers 不做 RPC。
4. **白名单方法**。
5. **只读接口可无鉴权，写接口必须鉴权**（mTLS / token）。

### 5.2 CommandMeta + 强类型 Command

不使用 `google.protobuf.Any`（弱类型会牺牲 IDL 可读性、生成代码类型安全、文档可发现性、细粒度权限校验和兼容性检查）。envelope 拆为 `CommandMeta`，每个写 RPC 各自强类型：

```proto
message CommandMeta {
    string request_id      = 1;   // UUIDv7，幂等键
    string source          = 2;   // "cli.loosqk" / "web.ui"
    AuthContext auth       = 3;   // token / mTLS cert fingerprint
    uint64 timeout_ms      = 4;   // 客户端-侧相对超时，不依赖时钟同步
    string idempotency_key = 5;
    bool   dry_run         = 6;
    string audit_reason    = 7;   // 审计日志（"who/why"）
}

message SetEnabledCommand         { CommandMeta meta = 1; bool   enabled = 2; }
message EStopCommand              { CommandMeta meta = 1; string estop_reason = 2; }   // 机器人-侧原因
message ClearErrorCommand         { CommandMeta meta = 1; }
message ClearEStopCommand         { CommandMeta meta = 1; string confirmation = 2; }   // 人工确认串
message InvokeTaskCommand         { CommandMeta meta = 1; string task_name = 2; bytes task_payload = 3; }
message CancelTaskCommand         { CommandMeta meta = 1; TaskHandleRef handle = 2; }
message ReloadCalibrationCommand  { CommandMeta meta = 1; }
message SetPolicyCommand          { CommandMeta meta = 1; string policy_name = 2; bytes policy_config = 3; }
```

gateway 把 `CommandMeta.timeout_ms` 转为本机 monotonic deadline（`now() + timeout_ms * 1_000_000`），写入 iceoryx2 request header。权限校验按 RPC 类型 × `AuthContext.role` 查 `configs/rpc_roles.toml`。

**字段语义消歧**：
- `CommandMeta.audit_reason`：审计日志用，记录 "谁、为什么调"（human context）。
- `EStopCommand.estop_reason`：机器人-侧原因，进 event log、recorder、UI 报警（robot context）。
- 真有跨机 wall deadline 需求时再加 `oneof deadline_spec { timeout_ms; deadline_wall_ns }`，v0.1.2 只要 `timeout_ms`。

### 5.3 方法集

```proto
service RobotControl {
    // 只读
    rpc ListHardware     (Empty)          returns (HardwareList);
    rpc ListJoints       (Empty)          returns (JointList);
    rpc ListTasks        (Empty)          returns (TaskList);
    rpc GetState         (Empty)          returns (RobotState);
    rpc StreamState      (StreamRequest)  returns (stream RobotState);
    rpc StreamLogs       (LogFilter)      returns (stream LogRecord);

    // 控制（强类型 Command + CommandMeta + 鉴权）
    rpc SetEnabled       (SetEnabledCommand)        returns (Ack);
    rpc EStop            (EStopCommand)             returns (Ack);
    rpc ClearError       (ClearErrorCommand)        returns (Ack);
    rpc ClearEStop       (ClearEStopCommand)        returns (Ack);    // 独立于 ClearError
    rpc InvokeTask       (InvokeTaskCommand)        returns (TaskHandle);
    rpc CancelTask       (CancelTaskCommand)        returns (Ack);
    rpc GetTaskStatus    (TaskHandleRef)            returns (TaskStatus);
    rpc StreamTaskEvents (TaskHandleRef)            returns (stream TaskEvent);

    // 配置
    rpc ReloadCalibration (ReloadCalibrationCommand) returns (Ack);
    rpc SetPolicy         (SetPolicyCommand)         returns (Ack);
}
```

### 5.4 权限模型（接口 v0.1 必须到位，实现可延后）

```
roles:       viewer / operator / maintainer / admin
capabilities:
    read_state
    invoke_task
    set_enabled
    clear_error
    clear_estop         (maintainer+)
    estop               (any role allowed)
    reload_calibration  (maintainer+)
    set_policy          (admin)
    shutdown            (admin)
```

role → capabilities 表放 `configs/rpc_roles.toml`。v0.1.2 可接受 fake token，但 AuthContext 字段必须到位；mTLS / OIDC 推到 v0.5+。

### 5.5 网关实现

`robot-rpc-gateway`：独立进程，崩了不影响机器人运行。订阅 iceoryx2 只读 topic 做状态聚合；写接口转成 iceoryx2 request-response 发给 S1。

### 5.6 S2 bridge 两种模式

```
embedded/local 高性能模式（pyo3 + iceoryx2 直连）：
    仅用于：VLA observation 打包、policy 结果消费、本机高频观测订阅。
    要求：机器人 venv + 共享内存权限 + schema_registry_hash 一致。
    默认不对普通 Python 脚本开放（避免 shared memory 权限、库版本、schema 漂移等问题）。

remote/client 模式（默认推荐）：
    所有脚本、UI、开发工具统一走 robot-rpc-gateway。
    跨机 / 沙箱 / 非 venv 用户、以及普通调试脚本一律走这条路。
```

ACL 强制：pyo3 模块只能 publish 白名单（`/<robot>/s2/goal`、`/<robot>/s2/task_intent`、`/<robot>/s2/policy_hint`）；违反 → runtime panic + `sd_notify FAIL` + 写 emergency log。

---

## 6. 看门狗与进程崩溃恢复

### 6.1 看门狗拓扑

```
          systemd (PID 1)
             │ WatchdogSec
             ▼
    ┌─────────────────────┐
    │  robot-s0.service   │ ◄──── 硬件级看门狗（可选，厂家电机）
    │   internal wd:      │
    │   rt_loop heartbeat │
    │    → ipc check      │
    │    → sd_notify      │
    └──────────┬──────────┘
               │ iceoryx2 /<robot>/s1/heartbeat  (50 Hz)
               ▼
    ┌─────────────────────┐
    │  robot-s1.service   │
    │   internal wd:      │
    │   coordinator tick  │
    │    → sd_notify      │
    └──────────┬──────────┘
               │
    ┌──────────┴──────────┬──────────────┬────────────────┐
    ▼                     ▼              ▼                ▼
 policy-host         rpc-gateway       viz           recorder
```

### 6.2 心跳语义

| 链路 | 方向 | 频率 | 超时 | 超时动作 |
|------|------|------|------|---------|
| S0 RT → S0 IPC | 内部 atomic counter | 1 kHz | 10 ms | IPC 标记 RT stall → FAULT → 输出 safe hold |
| S0 → systemd | `sd_notify WATCHDOG=1` | 5 Hz | 2 s | systemd 重启 S0 |
| S1 → S0 | iceoryx2 `/<robot>/s1/heartbeat` | 50 Hz | 200 ms | S0 切 `ENABLED.CONTROL_LOST_*`（见 §6.3），不退出主态 ENABLED |
| S1 → systemd | `sd_notify` | 2 Hz | 5 s | systemd 重启 S1；S0 不动 |
| PolicyHost → S1 | action_chunk seq 单调 | policy 周期 | 2 × 预期周期 | S1 标记 policy stale → tracking 最后一次 chunk → 超时进入 DEGRADED |
| RPC gateway → systemd | `sd_notify` | 2 Hz | 5 s | systemd 重启 gateway；不影响其他 |

### 6.3 S1 崩溃时 S0 的安全输出

```
S0 watchdog 超时 (> 200 ms 无 S1 heartbeat):
    1. 记录 fault_code = S1_STALL（不清 EStop latch）。
    2. 按最近一条 JointCommand.on_timeout 切换 ENABLED 子态：
         Hold → ENABLED.CONTROL_LOST_HOLD  (保持最后位置, position mode)
         Zero → ENABLED.CONTROL_LOST_ZERO  (速度归零, velocity mode)
         Stop → ENABLED.CONTROL_LOST_STOP  (torque disable + brake)
    3. state topic 明确报告 (mode=ENABLED, control_link=Lost, output_mode=...)。
    4. S1 重启并恢复 heartbeat 后，新命令 seq 单调 → 回到 ENABLED.ACTIVE。
       若期间 S0 已升级 FAULT（例如限位、硬件错误），则需 ClearError RPC。
```

**语义澄清**：不用"不退出 ENABLED"这种表述，因为 `ENABLED` 主状态 + `CONTROL_LOST_*` 子态才是完整描述。上层 UI / recorder / 任务调度器应订阅 `control_link` 字段，而非仅看主状态。

### 6.4 systemd unit 约束

```ini
# robot-s0.service —— 最重要、最独立
[Service]
Type=notify
Restart=on-failure
RestartSec=1
WatchdogSec=2s
LimitRTPRIO=95
LimitMEMLOCK=infinity
CPUAffinity=2-3
# 不 BindsTo 任何服务；S1 崩溃不会拉挂 S0
```

```ini
# robot-s1.service
[Unit]
After=robot-s0.service
Wants=robot-s0.service
# 注意：不是 Requires / BindsTo；S1 单独重启不应重启 S0

[Service]
Type=notify
Restart=on-failure
RestartSec=2
WatchdogSec=5s
```

```ini
# robot-rpc-gateway.service / robot-viz.service / robot-recorder.service
[Service]
Type=notify
Restart=on-failure
RestartSec=5
```

```ini
# robot-policy-host@<name>.service  (template unit)
[Service]
Type=notify
Restart=on-failure
RestartSec=3
WatchdogSec=10s
# VLA / Torch 启动慢，超时放宽
```

### 6.5 重启后的状态恢复

| 进程 | 重启后状态 |
|------|----------|
| S0 | 回到 `SAFE`；EStop latch 保持（从磁盘读回），必须人工 `ClearEStop` |
| S1 | `BOOTING → HOMING`：读 S0 当前 joint state，**不强制** home；上次未完成 task 不自动续跑 |
| PolicyHost | cold start；前 N 帧输出 fallback action（保持当前位置）直到 warmup 完成；coordinator 在此期间进入 DEGRADED |
| RPC gateway | 无状态，立即可用；in-flight `CommandMeta.idempotency_key` 去重 |
| Recorder | 新开文件（不续写旧文件），写入一条 `SessionBoundary` 事件 |

### 6.5.1 S0 崩溃 / 重启窗口的硬件 fail-safe

`systemd` 重启 S0 需要数秒到数十秒，这个窗口内 S0 软件不在，**必须由硬件 / 驱动层保证安全**：

1. 所有 motor adapter 必须在**硬件侧**配置 command timeout（厂家寄存器或板载固件），典型 50 – 200 ms。
2. S0 进程退出、崩溃或串口 fd 关闭后，驱动层必须立即触发 torque disable / velocity zero / brake（由硬件 timeout 兜底）。
3. **不支持硬件 timeout 的电机**：adapter 不得标记 `HardRT`，不得进入 `ENABLED.ACTIVE` 自动控制模式，只能以 servo / 示教 / 手动模式使用。
4. `systemd Restart` 是**恢复机制**，不是**安全机制**。把安全责任压给硬件 / 固件，软件侧只做快速收敛。

CI 验收（v0.2+）：物理 `kill -9 s0_daemon`，电机必须在 < 500 ms 内进入 fail-safe 状态。

### 6.6 EStop 一致性

```
EStop 来源：
    物理按钮     → S0 GPIO 中断 → 立即 latch SAFE + 写磁盘
    RPC EStop    → gateway → iceoryx2 /events/estop → S0 latch SAFE + 写磁盘
    S1 detect    → iceoryx2 /events/estop → S0 latch SAFE + 写磁盘
    S0 watchdog  → 内部 → latch FAULT（非 EStop，可 ClearError）

EStop latch 持久化到 /var/lib/robot/<robot_id>/estop_latch：
    - 只能被 ClearEStop RPC 解除（maintainer+ 权限）
    - 重启 S0 不清除
    - 磁盘损坏导致读不到 → 启动即拒绝进入 READY，进入 SAFE 等人工介入
```

### 6.7 不变量（系统级保证）

1. **S0 存活 = 软件层安全充分条件**。S1 / S2 / gateway 都可崩，S0 活着 + watchdog 正常 = 软件输出安全。
   **S0 不存活时的安全由硬件 / 驱动 fail-safe 提供**（§6.5.1）。systemd 只负责恢复，不负责安全。
2. **systemd 是最外层看门狗**。S0 内部 RT watchdog 失效 → `sd_notify` 超时 → systemd 重启 S0。
3. **EStop 不可被软件清除为失忆态**。硬复位需要物理人工确认 + maintainer 权限。
4. **S0 重启不失忆**。EStop latch、累计 fault code、最后一次 safe 输出都持久化。
5. **幂等写 RPC**。同 `CommandMeta.idempotency_key` 重复到达不得产生重复副作用。

---

## 7. Adapter 与硬件抽象

### 7.1 生命周期

```
[register]   编译期 inventory 登记
[discover]   S0 启动时扫描 registry，按 rt_class 分配线程
[connect]    按配置实例化 adapter，打开串口 / USB / CAN
[calibrate]  加载 calibration JSON，写 homing offset
[enable]     write_enable(true)
[loop]       read → publish / subscribe → write
[disable]    write_enable(false) on shutdown
[disconnect] 释放资源
```

### 7.2 配置文件

每个机器人一个 TOML：

```toml
[robot]
id = "xlerobot_01"

[[hardware]]
id = "bus1"
adapter = "feetech_sts3215"
address = "/dev/ttyACM0"
baud = 1_000_000
joints = ["left_arm_shoulder_pan", "left_arm_shoulder_lift", ...]
calibration = "calibration/bus1.json"
auto_enable = true

[[hardware]]
id = "bus2_base"
adapter = "feetech_omni3"
address = "/dev/ttyACM1"
wheel_radius = 0.05
base_radius = 0.125
joints = ["wheel_1", "wheel_2", "wheel_3"]

[[sensor]]
id = "head_cam"
adapter = "realsense_d435i"
serial = "..."
streams = ["color", "depth"]
```

S0 读这份配置启动，S1 读同一份做 topic 订阅 / 发布。

### 7.3 配置版本与一致性（ConfigIdentity）

启动时所有进程计算并交换 `ConfigIdentity`：

```rust
pub struct ConfigIdentity {
    pub robot_id:              String,
    pub config_version:        String,     // semver，对应 configs/<robot_id>.toml
    pub config_hash:           [u8; 32],   // blake3(robot config TOML)
    pub calibration_hash:      [u8; 32],   // blake3(所有 calibration JSON 排序后拼接)
    pub schema_registry_hash:  [u8; 32],   // blake3(sorted schema_id → schema_version)
    pub adapter_registry_hash: [u8; 32],   // blake3(sorted inventory entries)
}
```

校验规则：

- S0 / S1 / recorder / rpc-gateway / policy-host 启动时通过 iceoryx2 request-response 互换 `ConfigIdentity`。
- `config_hash` / `calibration_hash` / `schema_registry_hash` 必须完全一致。
- `adapter_registry_hash` 允许 S1 ⊇ S0（S1 可能订阅额外 sensor），但 S0 侧必须 ⊆ S1 侧。
- 不一致时：
  - S1 不进入 `EXECUTING`，停留 `DEGRADED`。
  - RPC gateway 标记 `degraded` 并拒绝所有写 RPC。
  - recorder 写一条 `ConfigMismatch` 事件后继续录制（观测不因配置不齐而丢）。

record 文件头必须保存完整 `ConfigIdentity`；回放时据此校验或用 `--force-replay` 显式覆盖。

避免的典型事故：S0 升级后 joint 顺序变了，S1 还用旧配置 → `layout_id` 对不上但没人报错 → 命令错位。

---

## 8. 观测与调试

- **日志**：`tracing` + `tracing-subscriber`，结构化 JSON，按进程分文件到 `/var/log/robot/<robot_id>/`。
- **指标**：`metrics` crate → Prometheus endpoint（9200）。关键指标：`tick_latency_p99`、`rt_overrun_count`、`message_drop_count{topic}`、`adapter_rtt`、`policy_inference_ms`、`watchdog_timeout_count`、`unknown_layout_count`、`unauthorized_publish_count`。
- **Tracing**：OpenTelemetry，可选，跨进程 span（`request_id` 贯穿）。
- **可视化**：`robot-viz` 独立进程，订阅 iceoryx2 全量 topic 转 Rerun / Foxglove，不在关键路径。
- **录制 / 回放**：见 §10.3。

---

## 9. 构建与工具链

- Rust **nightly**（硬要求；`rust-toolchain.toml` pin `channel = "nightly"`）。**不保留 stable 回退路径**，CI 不跑 stable 演练。
- **Edition 2024**（`workspace.package.edition = "2024"`）。**不写 `rust-version`**：随 nightly 最新版自动走；如需复现特定日期构建，用 `rust-toolchain.toml` pin `nightly-YYYY-MM-DD`，而非 `rust-version`。
- 核心 crate（`robot-core` / `robot-msg` / `robot-transport` / `robot-s0` / `robot-s1`）默认不依赖 nightly-only `#![feature(...)]`；若确需启用，必须在 crate 顶部注释单独列出并过 review —— 目的是**避免 nightly 升级引入破坏性变更**，不是为了回切 stable。
- 工具类 crate（`robot-cli` / `robot-viz` / `robot-rpc` / `robot-s2-bridge` / `robot-tests` / `robot-recorder`）可自由使用 nightly feature。
- S0 / S1 不得依赖 `mimalloc`（§0 Allocator 策略不因 nightly 工具链改变）。
- Cargo workspace profile：

```toml
[profile.release]
lto = "fat"
codegen-units = 1
# 默认保留 unwind，利于 CLI / 工具诊断

# S0 / S1 二进制必须 abort（避免 RT 路径 panic unwind）
[profile.release.package.robot-s0]
panic = "abort"

[profile.release.package.robot-s1]
panic = "abort"
```

- FlatBuffers 通过 `build.rs` 调 `flatc` 生成代码；`flatc` 由 `3rd-party/` vendor，不依赖系统。
- iceoryx2 走 crates.io；如需本地 patch，`[patch.crates-io]` 指向 vendor 路径。
- CI 顺序（严格串行，失败即终止；全程仅 nightly）：
  1. `cargo fmt --all --check`
  2. `cargo clippy --all --all-targets -- -D warnings`
  3. `cargo test --all`
  4. `cargo build --release`
  5. `cargo deny check`
  6. 额外：allocation-guard 测试（release + debug）、FlatBuffers schema 兼容性、1 h soak（夜间 job）、`kill -9 s0` fail-safe 验证（v0.2+）
- nightly 回归风险缓解：CI 支持 pin 具体 nightly 日期（`rust-toolchain.toml` 写 `channel = "nightly-YYYY-MM-DD"`），周期性手动升级 + 观察；若新日期破坏核心 crate 则原地回滚到上一已验证日期，**不回切 stable**。
- 二进制发布命名遵照全局 CLAUDE.md §9 模板：`robot-s0-v${version}+${git_sha7}-linux-arm64.tar.gz`，例如 `robot-s0-v0.1.2+abc1234-linux-arm64.tar.gz`。

---

## 10. 验证与测试（v0.2 开工前必须产出 `docs/10_robot_framework_verification.md`）

### 10.1 硬指标

**S0 loop**
```
target:                 1 kHz
p99 latency:            < 1.5 ms
max latency (1 h soak): < 5 ms
watchdog timeout path:  < 2 cycles
RT thread allocation count (after init) = 0
RT thread Arc::clone / Arc::drop count = 0       (v0.1.2 新增)
```

**S1 loop**
```
target:      100 Hz
p99 latency: < 15 ms
policy timeout 不阻塞 coordinator （验证：kill -9 policy host，coordinator 仍 100 Hz）
```

**Transport**（v0.1 建立 measured baseline，v0.2 起用 regression threshold 做 CI）

```
v0.1 必产出（measured baseline，按机型分别保存 histogram）：
    512 B message p99 (same host)
    3 MB blob p99
    1 h soak drop count for command topic

v0.1 硬约束（不受绝对值影响）：
    command topic drop count == 0        (hard)
    sensor topic drop allowed but counted (hard)
    unknown_layout_count == 0             (hard, 经过 warmup 后)

v0.2+ regression threshold:
    command topic p99 不得比 baseline 回退 > 30%
    blob topic    p99 不得比 baseline 回退 > 30%
```

说明：不同内核 / Jetson vs x86 / CPU governor / NUMA / iceoryx2 版本都会影响绝对值，μs 级 SLA 直接写死不可信；先让数据说话，后做回归检测。

**Safety**
```
EStop RPC → S0 latched:          < 20 ms
S1 crash → S0 safe output:        < 250 ms
RPC gateway crash → S0/S1:        0 tick drop
physical estop button → latched:  < 5 ms
EStop latch 持久化往返（重启 S0 后仍 latched）:  必须通过
kill -9 s0 → 硬件 fail-safe (torque disable/zero/brake): < 500 ms
```

**Schema**
```
old reader reads new writer:  所有 v0 schema 组合通过
new reader reads old writer:  所有 v0 schema 组合通过
deprecated 字段 id 不可复用:   CI lint
layout_id 碰撞检测:             CI lint (基于显式配置集)
```

### 10.2 测试基础设施

- `cyclictest` 跑 1 h，记录 RT 抖动直方图，纳入 CI 报告。
- `assert-no-alloc` 包 RT thread 入口，CI 跑 debug + release。
- `robot-tests::soak` 跑 mock 500 Hz state + 200 Hz command，1 h / 24 h 两档。
- `robot-tests::crash_restart` 随机 `kill -9` S1 / policy / gateway，校验 S0 输出与恢复。
- `robot-tests::schema_compat` 生成所有 schema 组合，跑 old/new 交叉解析。
- `robot-tests::rt_arc_guard` 钩子检测 RT thread 内 `Arc::clone/drop` 次数。

### 10.3 Record / Replay 格式

**记录必须保存**：

```
topic
schema_id
schema_version
monotonic stamp_ns
receive stamp_ns
seq
header bytes
body bytes
blob bytes 或 blob 文件 offset
dropped sample marker
clock info (monotonic → wall offset 采样)
robot config hash
calibration hash
session_id
active layout_id → JointLayout 映射表
```

**回放模式**：
```
as-fast-as-possible
real-time
scaled-time (x2, x0.5, ...)
step-by-step
topic remap
robot_id remap
```

**长期互操作**：短期自定义格式（二进制 + index），长期考虑对接 MCAP / LeRobotDataset 导出器。

---

## 11. 演进路线

### 11.1 版本与交付

| 版本 | 周期 | 交付 |
|------|------|------|
| **v0.1** | 3 周 | `robot-core` + `robot-msg` + `robot-transport` + `robot-cli`；mock adapter 自循环；**1 h long-soak**、schema 兼容性测试、crash/restart 测试、latency histogram、allocation guard、RT Arc guard；`docs/10_robot_framework_verification.md` 落地 |
| **v0.2** | 3 周 | `robot-s0` 四线程骨架（IPC/RT/Pub/IO）+ watchdog + SAFE/READY/ENABLED 状态机 + EStop latch 持久化；**先验通信层再接硬件**：watchdog 超时 / command deadline 过期 / EStop latch 不被 ClearError 清掉 / `kill -9` fail-safe 均须验证；Feetech adapter 单关节读写 |
| **v0.3** | 3 周 | `robot-s1` coordinator + arbiter + trajectory/velocity task；RPC gateway 只读接口 + CommandMeta + 强类型 Command |
| **v0.4** | 3 周 | 迁移 XLeRobot 全硬件；RealSense + webcam adapter（SoftRT，挂 `s0_io_thread`）；Rerun 桥；对齐现有 dimos 功能 |
| **v0.5** | 3 周 | `robot-s2-bridge` (pyo3) + ExternalPolicyHost + HTTP/gRPC policy 客户端；π0.5 VLA 端到端跑通 |
| **v0.6** | 4 周 | **结构异构**第二机器人验证（Piper 单臂 + 差速底盘）；修 adapter trait / message schema 漏洞 |
| **v1.0** | — | API 冻结，发布到内部 crates registry；systemd unit 模板齐全；runbook 完成 |

### 11.2 v0.2 实现顺序（强制）

不要直接接真硬件。按下面顺序：

```
1.  robot-core::time                         (MonoRawNs / Deadline / RobotClock trait)
2.  robot-msg schema registry                (FlatBuffers schema + schema_id 表 + old/new 兼容测试)
3.  FrameHeader / Body / Blob sample parser  (4B size prefix + verifier + 单元测试)
4.  SampleLease<T> + MessageView<'_, T> API  (纯类型设计 + mock 内存后端)
5.  mock iceoryx2 pub/sub                    (真的 iceoryx2 service，但 publisher/subscriber 都在一个进程)
6.  S0 四线程 mock                            (ipc + rt + pub + io；rt 用 mock HardRT adapter)
7.  watchdog + ENABLED.CONTROL_LOST_* 切换    (kill -9 s1 验证子态转换)
8.  EStop latch 文件双写 + 重启恢复           (fsync 双文件、读取校验、持久化测试)
9.  crash/restart 测试套                      (kill -9 s1 / policy / gateway 随机组合)
10. 接 Feetech 单关节                          (HardRT adapter 第一次上真硬件)
```

**最小闭环**（第 6–7 步打通即达成）：

```
mock S1 command → iceoryx2 → s0_ipc_thread
                → latest-command slot → s0_rt_thread
                → rt_snapshot        → s0_pub_thread
                → iceoryx2 state     → robot-cli status
```

这条链路跑通后再谈任何真硬件和真 policy。

---

## 12. 风险与缓解

| 风险 | 影响 | 缓解 |
|------|------|------|
| iceoryx2 Rust 绑定早期 bug | 本机通信全线崩 | v0.1 先做 fuzz + 1 h soak；`robot-transport` 抽 `Transport` trait，可切 Zenoh / 裸 shm |
| 厂家 C++ SDK FFI 成本 | 拖慢新硬件 | `rust-ffi` skill 三明治模式；先让 C++ 跑，再 Rust 化；未审计的一律 SoftRT |
| FlatBuffers schema 演进失控 | 上下层版本错配 | `schema_id + schema_version` + CI 兼容性交叉测试；deprecated 不可删 |
| S0 实时性被打断 | 抖动触发 fault | `mlockall`、预分配、`panic = "abort"`、`assert-no-alloc`、`rt_arc_guard`、CI 跑 cyclictest |
| 跨机器人抽象漏洞 | v1.0 后破坏性改动 | v0.6 强制结构异构第二机器人；提前暴露抽象缺陷 |
| Python S2 GC pause 拖累 S1 | VLA 推理阻塞 | Python 强制 ExternalPolicyHost；跨进程 iceoryx2；S1 永不等 policy |
| EStop latch 文件损坏 | 重启后失忆 | 启动时读不到 latch 即拒绝进入 READY；`fsync` 双写 + 备份文件 |
| systemd Restart 风暴 | 反复崩反复重启 | `StartLimitBurst=5`、`StartLimitIntervalSec=60`；超过阈值进入人工介入模式 |
| `s0_pub_thread` 被拖慢 | 整机 CONTROL_LOST | Pub 线程只做 iceoryx2 publish；SoftRT 禁挂 Pub，只能挂 `s0_io_thread`；CI 注入 100 ms 阻塞验证隔离 |
| `Arc` refcount 污染 RT | RT 抖动 / 析构触发 free | `RtSnapshotReader::load()` 不 bump refcount；`rt_arc_guard` 测试拦截 |
| `layout_id` 碰撞 | 关节数组错位 → 发错命令 | 64 bits；CI lint 显式配置集；未知 layout 不解析 |

---

## 13. 开放问题

1. **S1 task DSL**：底层统一 `Task` trait + 状态机；**行为树仅作为 S2 / S1 上层编排**，不直接控制 servo step。
2. **多机器人协同**：v0.6 前不承诺跨机控制通道；跨机只做观测 / 任务意图 / 日志。选型 Zenoh（机器人-机器人 / 边缘）+ MQTT（云端状态），v0.6 实测再定。
3. **安全形式化验证**：S0 安全态 Kani / Prusti 验证？v1.0 后评估。
4. **权限模型**：v0.1.2 接口到位（§5.4），实现 mTLS / OIDC 推到 v0.5+。
5. **Record/replay 标准**：短期自定义（§10.3），长期对接 MCAP 或 LeRobotDataset。
6. **硬件级 watchdog**：厂家电机 / IMU 自带 watchdog 要不要接？v0.2 实测决定。
7. **跨机 wall deadline**：v0.1.2 只有 `timeout_ms`；是否加 `oneof deadline_spec` 看 v0.6 跨机需求。
8. **多 blob / GPU handle**：v0.1.2 单 blob + `GPU_MEMORY_HANDLE` flag 占位；多 blob 排到 v0.3。

---

## 附录 A · 术语

- **S0 / S1 / S2**：参考 Figure AI 的命名，分别对应反射 / 快反射 / 慎思三层。
- **iceoryx2**：Eclipse iceoryx 的 Rust 重写版，本机零拷贝 pub-sub。
- **FlatBuffers**：Google schema-driven 序列化格式，反序列化零开销。
- **Blob**：一块连续的二进制数据，通过 shm 传递，不走序列化。
- **Policy**：把观测映射到动作的函数（PID / RL / VLA）。
- **Task**：S1 中的一个控制目标（一条轨迹、一段底盘速度、一次伺服跟踪）。
- **HardRT / SerialRT / SoftRT adapter**：按是否可进 S0 RT loop 分级的 adapter（§4.6）。
- **InProcessPolicyHost / ExternalPolicyHost**：按是否跨进程分级的 policy runtime。
- **Sample lease**：iceoryx2 sample 的借用期；`SampleLease<T>` 是 Rust 封装，值语义持有 loan。
- **StaleAction**：命令过期后 S0 采取的动作（Hold / Zero / Stop）。
- **EStop latch**：紧停锁存，持久化到磁盘，只能通过 `ClearEStop` RPC（maintainer+）解除。
- **RtSnapshotReader / Publisher**：RT 安全的 immutable snapshot 切换原语；读路径不 bump refcount，回收在普通线程。
- **ControlLink**：state topic 暴露的控制链路状态（OK / Lost），与 `ENABLED` 主态组合描述系统。

## 附录 B · 与 dimos 的对应关系

| dimos | robot-core v0.1.2 | 说明 |
|-------|-------------------|------|
| `ControlCoordinator` | `robot-s1::coordinator` | Rust 重写，独立二进制 |
| `ManipulatorAdapter` / `TwistBaseAdapter` protocol | `robot-core::adapter` traits | trait + `inventory` 静态注册 |
| LCM messages | FlatBuffers schema in `robot-msg` | schema 驱动 + 版本化 + 4B size prefix |
| LCM transport | iceoryx2（机内）+ Zenoh（跨机，v0.6+） | 零拷贝 + `SampleLease<T>` |
| `.pth` hook | `inventory::submit!` | 编译期静态注册 |
| `RPCClient` / `RPCServer` | `robot-rpc-gateway` (gRPC) | 对外专用，`CommandMeta` + 强类型 Command |
| `rerun_bridge` | `robot-viz` | 独立进程，订阅全 topic |
| Blueprint | 配置 TOML + S0 / S1 启动参数 | 不做声明式 DSL |
| 进程间通信（dimos 通过 `spawn`） | systemd tree + iceoryx2 | 崩溃隔离 |

## 附录 C · 评审修订说明（v0 → v0.1）

| 条目 | v0 | v0.1 |
|------|-----|------|
| S0 ↔ iceoryx2 | S0 直接收发 iceoryx2 | S0 三线程拆分，RT thread 不碰 iceoryx2（§1.2） |
| Policy runtime | 线程 / 进程描述不一致 | 明确 `InProcessPolicyHost` vs `ExternalPolicyHost`（§4.7） |
| 全局 mimalloc | 统一默认 | S0/S1 系统默认 + RT 禁 alloc；仅外部工具用 mimalloc（§0） |
| Inventory + 动态加载 | 表述矛盾 | 统一静态链接 + inventory 注册，dynamic plugin 推 v1.x（§4.5） |
| FlatBuffers body union | 全局 union | Header + typed body，`schema_id` 路由（§2.2） |
| JointState/Command `names` | 每帧重复 strings | `layout_id` + 低频 `JointLayout`（§3.5） |
| Float 策略 | 默认 float32 | joint/轨迹 float64，tensor float32（§3.3） |
| 时钟域 | 仅要求 monotonic stamp | deadline 也必须 monotonic；RPC 网关做转换（§3.2） |
| RPC 方法 | 直接 args | `CommandRequest` envelope + `TaskHandle` 分离（§5.2–5.3） |
| `ClearError` vs `ClearEStop` | 未区分 | 独立 RPC + 独立 latch（§1.4, §6.6） |
| Raw blob ABI | 仅 offset/size/dtype/shape | 加 alignment / strides / layout / endian / compression / crc / color_order（§3.5） |
| Record / replay | 一句带过 | 格式规范 + 回放模式（§10.3） |
| 看门狗 / 崩溃恢复 | 只提 watchdog 一词 | 新增 §6：拓扑、心跳语义、重启状态恢复、EStop 一致性、systemd 约束 |
| 验证文档 | v0.2 前补 | v0.1 必须产出 |
| v0.1 范围 | hello-world | 加 soak / 兼容性 / crash-restart / allocation guard |
| 权限模型 | 推迟 | v0.1 接口到位，实现可延后 |
| 控制边界 ACL | 软约束 | 编译期 topic ↔ publisher 类型绑定 |
| Adapter RT 分级 | 无 | HardRT / SerialRT / SoftRT 显式声明（§4.6） |
| S2 bridge 边界 | 单一 pyo3 | embedded + remote 两种模式 + ACL（§5.6） |
| QoS | 四类粗粒度 | 按 topic 类别细化（§2.4） |
| 行为树 vs 状态机 | 两者都支持 | 底层统一状态机；行为树仅用于 S2 编排（§13.1） |

## 附录 D · v0.1 → v0.1.1 语义修订

v0.1.1 **只改语义、不扩功能**。

| 条目 | v0.1 | v0.1.1 |
|------|------|--------|
| S0 iceoryx2 调用者 | "IPC 是唯一调用者" 与 pub 线程发布 state 自相矛盾 | 明确 IPC + Pub 两个普通线程都可调 iceoryx2，RT 不碰（§1.2） |
| ENABLED 语义 | "S1 崩时不退出 ENABLED" 有歧义 | 引入 ENABLED 子态 ACTIVE / CONTROL_LOST_HOLD / CONTROL_LOST_ZERO / CONTROL_LOST_STOP；state topic 暴露 `control_link`（§1.4, §6.3） |
| Topic ACL | "编译期检查即违反失败" 过强 | 三层 ACL：编译期 TopicClass + 启动期 config + 运行期 source_id（§1.6） |
| Blob offset 语义 | FrameHeader 与 BlobDesc 双 offset 含糊 | v0.1.1 仅单 blob，`BlobDesc.offset = 0`；多 blob 推 v0.2（§2.2） |
| SampleLease API | `PhantomData<&'a SampleLease<'a>>` 不拥有 lease | `SampleLease` 值语义持有 loan，`view()` 返回借用（§2.5） |
| 时钟封装 | 仅要求 `CLOCK_MONOTONIC_RAW` | 新增 `robot-core::time` 统一 API，禁止业务代码直接调 `Instant::now` 等（§3.2.1） |
| CRC 策略 | 未说明开启时机 | 默认关闭机内路径；record / 跨机 / 安全事件 / 诊断模式开启（§3.4） |
| `layout_id` 缓存失效 | 未说明 | 未知 id → 不解析 + 请求 JointLayout + DEGRADED；recorder 写文件头（§3.5.1） |
| ReloadCalibration 实时性 | 未说明 | 新增 §4.9：immutable snapshot + atomic pointer swap，RT 零解析 |
| RPC envelope | `CommandRequest` + `google.protobuf.Any`（弱类型） | `CommandMeta` + 每 RPC 强类型 Command（§5.2–5.3） |
| S2 bridge | 两种模式并列 | 默认远程 RPC；pyo3 + iceoryx2 仅高性能路径 + 严格 ACL（§5.6） |
| S0 崩溃窗口安全 | "S0 存活是充分条件" 被误读为唯一保证 | 明确 S0 不存活时由硬件 / 驱动 fail-safe 保证（§6.5.1, §6.7 不变量 1） |
| ConfigIdentity | 无 | 新增 §7.3：多进程启动交换 hash，不一致 → DEGRADED |
| Transport 指标 | 硬 SLA（30 μs / 200 μs） | v0.1 measured baseline；硬约束只保留 drop=0；v0.2+ regression threshold（§10.1） |

## 附录 E · v0.1.1 → v0.1.2 实现级修订

v0.1.2 解决 v0.1.1 评审发现的 5 个实现级自洽问题 + 4 个小修。冻结后不再动语义。

| 条目 | v0.1.1 | v0.1.2 |
|------|--------|--------|
| `LatestSubscriber` API 不一致 | §2.5 用新 API，§4.3 残留旧 API | 统一 `fn latest() -> Result<Option<SampleLease<T>>, _>`；`SampleLease<T>` 泛型带住 schema 类型（§2.5, §4.3） |
| sample 解析歧义 | FlatBuffers root 无明确 size 边界 | 加 4 字节 LE `frame_header_size` 前缀；解析按 "先切包再 verify" 两步（§2.2） |
| `Arc` 在 RT 路径 | §4.9 用 `Arc<CalibrationSnapshot>`，RT 可能 clone/drop | 改用 `RtSnapshotReader/Publisher`；RT 禁 clone/drop；回收在 ipc_thread `drain_retired()`；`rt_arc_guard` 测试（§0, §4.9, §10.1） |
| SoftRT 挂 Pub 线程风险 | 允许挂 IPC / Pub | 禁止挂 Pub；引入 `s0_io_thread[0..N]`；`s0_pub_thread` 保持纯净（§1.1, §1.2, §4.6） |
| `layout_id` 宽度 | 32 bits（blake3 截前 4 字节） | 64 bits（blake3 截前 8 字节）；避免跨时间 / 跨机器人碰撞（§3.5, §3.5.1） |
| §1.1 S2 语言行 | "Python (pyo3)" 暗示默认嵌入 | 改为默认 RPC + 高性能 pyo3 双路径描述（§1.1） |
| `deadline_wall_ns` | 暗示必须 wall clock | 改 `timeout_ms`（相对超时，不依赖时钟同步）；wall deadline 留 v0.6 跨机场景再加 oneof（§5.2） |
| `reason` 字段同名冲突 | `CommandMeta.reason` + `EStopCommand.reason` | 重命名 `CommandMeta.audit_reason` + `EStopCommand.estop_reason`（§5.2） |
| `panic = "abort"` 全局 | workspace 根统一 abort 影响 CLI / 工具调试 | 仅 `[profile.release.package.robot-s0/s1]` 启用；其他保留 unwind（§9） |

### 合入后评审补充（v0.1.2 终稿）

| 条目 | 补强点 |
|------|--------|
| `SampleWriter` 生命周期 | 明确"未 publish 即 drop discard loan"、"publish 消费 writer"、"错误路径无需显式 release"；避免 loan 泄漏（§4.3） |
| `frame_header_size` 上限 | 硬上限 ≤ 4096；新增"body 与 blob 区间不重叠"防御性检查（§2.2） |
| RtSnapshot 内存序 | publisher release store / reader acquire load / 回收必须晚于所有 reader guard 退出（epoch / hazard / 延迟队列）（§4.9） |

### 冻结后文档卫生（非架构性）

| 条目 | 调整 |
|------|------|
| verification doc 时限 | v0.1 原计划"v0.1 必须产出"，v0.1.2 实际收敛为"v0.2 开工前必须产出"（§1.4 / §10 标题）；附录 C 保留 v0 → v0.1 当时的历史决定 |
| 发布命名示例 | §9 改用模板 `robot-s0-v${version}+${git_sha7}-linux-arm64.tar.gz` + 当前版本 `v0.1.2` 为例，避免示例落后于真实版本 |
| 工具链（2026-04-21） | §9 由 "Rust stable（尽量不用 nightly）" 改为 **nightly 硬要求 + edition 2024**；**不保留 stable 回退**；不写 `rust-version`（随最新 nightly 走，复现构建用 `nightly-YYYY-MM-DD` pin）；核心 crate 默认不用 nightly-only feature 以防升级回归；回归风险靠 pin nightly 日期 + 原地回滚，不切 stable；S0/S1 禁 `mimalloc` 约束（§0）不变 |
| IK 分层（2026-04-22） | §4.1 模块地图新增 `robot-kinematics`（trait crate）+ `robot-kinematics-<robot>`（每机器人实现）。IK 位于 S1，不进 S0 / adapter；`robot-s1` 以 feature flag 挂具体 solver，替换机器人 = 换 feature；`robot-kinematics` 零 runtime 依赖，不放入 `robot-core`（§4.2 允许列表不受影响）。trait 草案 v0.3 前定，XLeRobot 实现 v0.4 起跟 adapter 对齐时落地 |
| §4.3 writer API（2026-04-22） | `Publisher::publish(&mut self, w)` 在 writer 持 `&'a mut Publisher` 时会与再取 `&mut self` 冲突（Rust borrow checker 规则）；调整为 `SampleWriter::publish(self) -> Result<()>`（方法挂 writer 上，按值消费 self 自然释放 publisher borrow）。语义完全不变：按值消费、drop without publish = discard loan、loan 泄漏编译期拦截。详见 `docs/CHANGELOG.md` 2026-04-22 "§4.3 writer API 调整" |
