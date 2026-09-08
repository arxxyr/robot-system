# post-E 通信故障硬化 · 函数级实施计划

> 日期：2026-09-08。版本：Plan Approved + C3 S-40 Reverse Sync。状态：**Plan Approved，分包 Execute 中**。
> 权威：[`post_e_comm_fault_hardening.md`](post_e_comm_fault_hardening.md)（round 21 Spec Approved）。
> 代码基线：`bd081cb`；工作分支：`feat/post-e-comm-fault-hardening`。
> 本文只展开已批准 D1–D15 与 P.0–P.8，不修改冻结设计。主 agent / reviewer 明确
> `Plan Approved` 后实施；用户已授权团队内部评审通过后持续推进，无需再次请求流程确认。
> 历史批准稿 SHA-256：`3a8a394a74764ad2277bdd5f3aa0901627538f635d4b6208c55cde99d6f2dd30`。
> C1 `fc0c142`、C2 `822ab16` 已提交；C3 实施中新发现 S-40 已由独立 reviewer ACK，
> 本次仅同步已批准的原槽取消 tag 协议；当前文件新 hash 由主 agent 写入 HANDOFF。

## P.0 范围、所有权与当前事实

唯一 Dev 负责生产代码、模块导出、Cargo 清单和模块单测；reviewer 只读独立评审，
给出独立验收场景与 PASS / FAIL，不能以 Dev 自测替代验收；主 agent 负责独立 integration
验收夹具、comm_fault_e2e 入口、Spec、HANDOFF、入口文档、CHANGELOG、10_verification、
Archive 与 `.verify` 汇总。同一文件
只由其 owner 修改。本文由 Dev 维护，评审反馈统一经 Dev 回修。

本次是软件交付：本机 MockClock、伪串口、临时目录、loopback RPC、隔离子进程与测试
systemd 服务可以执行；**不连接真实串口、不执行机器人上扭/断扭、不部署/启停生产
unit、不对真实 marker 授权**。Orin 的 AC-2(d)、AC-12、AC-13 留具名命令与证据格式，
缺现场实测只能记 NEEDS_HUMAN，不写 PASS。D15 硬件 fail-safe 仍 OPEN。

现有调用链已核：

| 链路 | 基线位置 | 实施约束 |
|---|---|---|
| CLI → resolve → build | `s0_daemon.rs::main:453`、`Args::resolve:276`、`build_adapter:1233` | supervisor 首句；先构造共享 ctrl/服务再 open；N 上限在任何业务 spawn 前 |
| 工厂服务 | `robot-core/src/factory.rs::AdapterBuildContext:55`、`feetech/src/factory.rs::build_concrete:41` | 当前只有借用配置；新增借用服务载体，不 downcast |
| 首次串口 IO | `spawn_serial_io_thread:229` 当前调用方 connect:266 并直写 confirmed:269 | 删除同步 connect；七 worker 就绪门之后 serial 首 IO 是 Safety |
| action | `ipc_tick:112` → ctrl packed pending → `execute_serial_normal:584` → adapter enable | timeout 请求 retire，事务不可拆；RAII release 独立于 liveness |
| emergency | `ipc_thread::poll_estop:1188` → 旧 EventQueue → `ipc_tick` | SerialRT ingress 单次原语，Step 1 不再重复发布 |
| pub | `publish_joint_state:256` 直接读 `rt_ctrl.safety_state` | 必须改到 ctrl_kind 选源 accessor，不能只改 client |
| RR | `ipc_thread.rs:725–862` ingress 当前直接 evaluator + pending | evaluator、latch、pending 全部移至 rr lane 第二道 deadline 后 |
| spawn | ipc → ClearEStop RR → SetEnabled RR → wd → executor → pub → io | 七槽预建；executor 提前 spawn 但首 IO 等七者全部 Spawned |
| health | `ExecutorHandle::make_tick_observer:1374`、`robot-health::systemd::notify_ready` | SerialRT 观察 lease 计数；READY 改 additive 三态 API |

精确搜索清单（实施后复跑，不以编译漏网的 `_` arm 代替审计）：

- `AdapterBuildContext {`：core `factory.rs` 两测试、feetech `adapter.rs`/`factory.rs`、
  s0 `s0_daemon.rs`、`tests/serial_rt_e2e.rs`。所有 literal 新增 `services`。
- `FeetechAdapterConfig {`：feetech `adapter.rs` 的 fixtures、`factory.rs` 构造；新增
  preempt/diag/call_deadline 必同步，错误测试 `calibration_id_mismatch_rejected_on_connect`
  改为构造期拒绝。
- `RobotError` match：feetech `error.rs::From`、s0 `serial_io_thread::execute_serial_normal`、
  `s0_daemon::build_adapter`，core error Display/编码测试；mock/rt_thread 中已有泛化错误
  分支保留 legacy 行为，逐个核对新增类不被 SerialRT 错误处理吞掉。
- `AdapterErr`：core executor 编解码；s0 hard_rt_ctrl、serial_io_thread、ipc_tick、
  ipc_thread、set_enabled_action、set_enabled_rr_server；rt_thread 保留既有输出映射。
- `FaultCode`：s0 state_machine、pub_thread、ipc_tick、ipc_thread、set_enabled_action /
  set_enabled_rr_server、rt_thread 测试；rpc `state_convert.rs` 是数值透传，补 round-trip
  锁测，不改变 gateway/proto 行为。
- spawn 与 event_sender 消费：s0 daemon、各线程模块单测、`serial_rt_e2e.rs`、
  `rt_guard_soak.rs`。health/gateway/s1 内同名 spawn 是不同函数，不能机械替换。
- `Guards {` 全仓 fixture 同步加 `rt_serial`；HardRT 默认 false。

## P.1 公共协议、共享原子与诊断（第一组）

### P.1.1 core 新增类型与完整服务接口

改 `robot-core/src/error.rs` / `executor_ctrl.rs`，新建 `serial_services.rs` / `diag.rs`，
在 `lib.rs` 导出；所有类型不引 runtime 或外部依赖。

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CommErrorKind {
    Timeout = 0, Disconnected = 1, IoRead = 2, IoWrite = 3, OpenFailed = 4,
    FrameCorrupt = 5, ResponseIdMismatch = 6, PeerNoResponse = 7,
    DiscardFailed = 8, PendingDeadline = 9, Unknown = 0x3f,
}
impl CommErrorKind {
    pub const fn from_u8(raw: u8) -> Self;
    pub const fn to_u8(self) -> u8;
    pub const fn clears_trust(self) -> bool;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbortReason { Emergency, Timeout }
// RobotError 增 Comm(CommErrorKind)、Aborted(AbortReason)。
// AdapterErr 增 AbortedByTimeout、Comm(CommErrorKind)。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SafetyStopFailDetail {
    StopFailed { still_on: u8 },
    AdapterUncertain { kind: CommErrorKind },
    StopNotDelivered { kind: CommErrorKind },
}
impl SafetyStopFailDetail {
    pub const fn to_u8(self) -> u8;
    pub const fn from_u8(raw: u8) -> Self;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EStopOutcome { StopIssued, Merged, LatchOnly }
pub trait SerialEmergencyCtrl: Send + Sync {
    fn estop_monotonic(&self) -> EStopOutcome;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NormalAbortTarget { Submitted { seq: u64 }, Unsubmitted }
// ExecutorCtrlClient 唯一 additive default 例外；HardRT/现有测试可保持 no-op。
fn abort_pending_normal(&self, _target: NormalAbortTarget) {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum BusOpClass {
    NormalEffectful, PeriodicRead, PeriodicWrite, Safety, Confirm, Connect,
}
pub trait PreemptProbe: Send + Sync {
    fn should_abort(&self, class: BusOpClass) -> Option<AbortReason>;
    fn begin_transaction(&self, class: BusOpClass) -> Result<(), AbortReason>;
    fn finish_transaction(&self, class: BusOpClass);
    fn checkpoint(&self);
    fn sleep_or_abort(&self, class: BusOpClass, dur: DurationNs)
        -> Result<(), AbortReason>;
    fn operation_class(&self) -> BusOpClass;
    fn confirmation_trusted(&self) -> bool;
    fn report_comm_error(&self, kind: CommErrorKind);
}
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct DiagEvent {
    pub stamp_ns: u64, pub site: u8, pub kind: u8,
    pub a: u8, pub b: u8, pub seq_lo: u32,
}
pub trait DiagSink: Send + Sync { fn record(&self, event: DiagEvent); }
```

接口展开理由：`checkpoint` 在每次 transport 调用返回时执行，而 request+response
必须共持 `normal_io_active`，因此不能用 checkpoint 归还执行权。`begin_transaction`
与 `finish_transaction` 是 D13 acquire/release 的具名接口；`should_abort` 纯读用于
sleep，不取得执行权。`operation_class` 让同一 connect 方法区分启动 Connect 与运行
NormalEffectful；`confirmation_trusted` 把 serial 已选定的 `evidence_trusted(ctx)`
传给 adapter 内 hygiene，不改变 `OutputSafetyProbe` 签名。以上服务均构造期注入，
无 adapter runtime setter / downcast。此接口展开需 reviewer 明确认可后实施。

`TRUST_CLEARING` 完整九类按 Spec；OpenFailed 只在 open 失败，不进入运行矩阵；
PendingDeadline 不清信任。AdapterErr 新码 6、Comm=0x40|kind；穷举 256 decode 稳定。
`joint.fbs::S0FaultCode` 末尾新增 CommFault=7，不改字段/方法/其它数值；沿现有 build.rs
生成，不提交手写生成代码。

### P.1.2 工厂服务与提前构造

`robot-core/src/factory.rs` 新增：

```rust
pub struct AdapterBuildServices {
    pub clock: Arc<dyn RobotClock>,
    pub preempt: Option<Arc<dyn PreemptProbe>>,
    pub diag: Option<Arc<dyn DiagSink>>,
    pub call_deadline: DurationNs,
}
// AdapterBuildContext<'a> 新字段：
pub services: Option<&'a AdapterBuildServices>,
```

保留 context 的 Clone/Copy；为 services 实现不输出 trait 内容的手工 Debug，使 context
仍 Debug。HardRT/SoftRT factory 忽略服务；FeetechFactory::build_concrete 从服务复用同一
clock/preempt/diag，直接构造测试的 None 使用 SystemClock + 50 ms + 无抢占/诊断。
无 worker 前 Arc::clone 全部完成，serial 热路径不 clone。

### P.1.3 s0 原子词与访问者

`hard_rt_ctrl.rs` 保留现有七字段和 `new()`=HardRt，新增 `new_serial()`、`CtrlKind`，
serial alias 不复制一份协议。新增/展开：

```rust
pub enum CtrlKind { HardRt, SerialRt }
impl HardRtCtrlInner {
    pub fn new_serial() -> Self;
    pub fn safety_state(&self) -> OutputSafetyState;
    pub(crate) fn set_serial_safety(&self, state: OutputSafetyState);
    pub(crate) fn claim_serial_estop(&self) -> bool;
    pub(crate) fn claim_ipc_estop(&self) -> bool;
    pub(crate) fn try_acquire_normal_io(&self) -> bool;
    pub(crate) fn release_normal_io(&self);
    pub(crate) fn mailbox_snapshot(&self) -> Option<MailboxSnapshot>;
    pub(crate) fn ack_fault(&self, generation: u32);
}
pub(crate) fn tighten_estop_policy(policy: &AtomicU8);
pub struct MailboxSnapshot {
    pub generation: u32, pub code: FaultCode, pub detail: u8, pub origin: FaultOrigin,
}
pub enum FaultOrigin {
    Periodic, NormalAction, PendingTimeout, InvariantViolation, Confirm,
    StopNotDelivered, BootUntrusted, ConnectFailed, PreflightFailed,
    PreflightTimeout, EStopEscalation,
}
pub struct ActionRecord { pub valid: bool, pub action: u8, pub arg: u8, pub seq: u64 }
pub struct CompletedEffect { pub action: ActionRecord, pub success: bool }
pub const SEQ_BITS: u32 = 40;
pub const SEQ_MASK: u64 = (1_u64 << SEQ_BITS) - 1;
pub const ABORT_NONE: u64 = 0;
pub const ABORT_UNSUBMITTED: u64 = 1_u64 << 63;
pub const PENDING_CANCELLED: u64 = 1_u64 << 63;
pub struct SerialPending { pub action: ActionRecord, pub cancelled: bool }
pub struct InvalidSerialPending(pub u64);
pub fn decode_serial_pending(raw: u64) -> Result<Option<SerialPending>, InvalidSerialPending>;
```

| 字段 | 类型/初值 | writer / reader 与内存序 |
|---|---|---|
| ctrl_kind | CtrlKind，不可变 | 构造期固定 |
| safety_word | AtomicU32，Unknown / 三位0 | 8-bit state + bit8 serial_pending + bit9 ipc_notice + bit10 normal_active；所有改动 strong CAS AcqRel/Acquire，保留他位 |
| active_action | AtomicU64，0 | serial 单写 Relaxed；pub 观测；serial retire 自有记录匹配 |
| completed_effect | AtomicU64，0 | serial 单写 Relaxed；pub 观测；valid/success/action/arg/seq按 D3 |
| abort_request | AtomicU64，0 | IPC Release / serial Acquire；0空、1..SEQ_MASK为Submitted、bit63专用Unsubmitted；先判tag再mask |
| pending_normal（SerialRT） | 既有 AtomicU64，0 | request 发布 payload；EStop strong CAS AcqRel/Acquire 仅将非空未取消词置 bit63；serial AcqRel 消费，取消词不抹去 action/arg/seq；HardRT 48位原逻辑不变 |
| fault_mailbox | AtomicU64，0 | serial Release / IPC Acquire；gen:32/code:8/detail:8/origin:8/保留8 |
| fault_ack | AtomicU32，0 | IPC 唯一 Release / serial Acquire |
| inhibit | AtomicU8，SerialRT 初始 BOOT | serial Release 修改，IPC/serial Acquire；位=COMM_FAULT/ADAPTER_FAULT/STOP_FAILED/CONVERGING/FAULT_PENDING/BOOT |
| session_clean / rx_trust | AtomicBool，true/false | serial Release / IPC、probe Acquire；rx true唯一Preflight通过点 |
| comm_level / l0_since_ns | AtomicU8/AtomicU64，L0/0 | serial Release / IPC Acquire，恢复持续时间按RobotClock |
| liveness_counter | AtomicU64，0 | serial Relaxed / liveness observer Relaxed，仅进度观测 |
| estop_requests | AtomicU32，0 | 发布者 Relaxed；仅诊断 |
| boot_outcome / boot_deadline_expired | AtomicU8/AtomicBool，Pending/false | serial Release发布出口，daemon Acquire；deadline daemon Release/serial Acquire |

`HardRtCtrlClient::safety_state` 委托 inner accessor；`impl SerialEmergencyCtrl` 按
D13 latch → 原槽取消 pending（S-40）→ 诊断计数 → strong CAS（同时两位），CAS 后零权威写。
`request_normal` 只在 SerialRT 检查 seq∈[1,2^40−1]，失败 Unsupported；既有 pending
48-bit 编码不改，HardRT 不收紧取号范围。`abort_pending_normal` 只在 SerialRT 发布。
HardRT `safety_stop` 五步与 legacy 状态/序号不变；SerialRT production EStop 只用新
trait，旧方法不增加隐式双发。

`fault_mailbox` pack/unpack、ActionRecord pack/unpack 均 const 纯函数，位布局不以
Rust struct 内存布局替代。generation==u32::MAX−1 时拒绝新增代，保留 Fault/inhibit，
诊断饱和，无绕回。mailbox 发布唯一 helper 强制 state→inhibit→word 顺序。

### P.1.4 Diag 与 preempt 实现

新增 `robot-s0/src/diag.rs`：

```rust
pub struct SerialDiagRing { /* 256 个 UnsafeCell<MaybeUninit<DiagEvent>>，head/tail/dropped */ }
impl SerialDiagRing {
    pub fn new() -> Self;
    pub fn pop(&self) -> Option<DiagEvent>;
    pub fn dropped(&self) -> u64;
}
impl DiagSink for SerialDiagRing { fn record(&self, event: DiagEvent); }
```

head/tail 使用单调 usize wrapping差计算容量或257个物理槽表示256有效槽，锁定有效容量
恰256；producer只动head、consumer只动tail，Release/Acquire配对；满drop-newest。
unsafe仅此槽封装，Safety注释证明SPSC、不覆盖未读槽。constructor给serial唯一producer，
pub唯一consumer；daemon启动日志直接tracing，不把主线程接到SPSC producer。

新增 `serial_preempt.rs`：

```rust
pub struct SerialPreemptProbe { /* clock/ctrl + 当前class/seq/lease_deadline/trusted */ }
impl SerialPreemptProbe {
    pub fn new(clock: Arc<dyn RobotClock>, ctrl: Arc<HardRtCtrlInner>) -> Self;
    pub(crate) fn set_context(&self, class: BusOpClass, seq: u64,
        deadline: MonoRawNs, confirmation_trusted: bool);
}
impl PreemptProbe for SerialPreemptProbe { /* P.1.1 八个方法 */ }
```

上下文字段由serial唯一写、同线程adapter读；仍用原子避免Sync不成立，Relaxed即可；
ctrl pending/abort读取用Acquire。lease只在transport返回且now<deadline时计数，常规tick
推进也受当前phase deadline检查，不能用tick末自增掩盖超期phase。sleep每片≤5ms，
先检查再sleep、片后检查，保留Emergency优先；不会在sleep取得normal IO执行权。

## P.2 Feetech有界传输与事务（依赖 P.1）

### P.2.1 transport

改 `transport.rs`，保留现有四方法签名，增加：

```rust
pub struct DrainReport { pub bytes: usize, pub deadline_hit: bool }
pub trait SerialTransport: Send {
    // 既有write_all/read_exact/set_timeout/is_connected签名不变。
    fn discard_input(&mut self) -> Result<(), FeetechError>;
    fn drain_until_quiet(&mut self, quiet_required: Duration,
        absolute_budget: Duration) -> Result<DrainReport, FeetechError>;
}
impl RealSerialTransport {
    pub fn open(path: &Path, baudrate: u32) -> Result<Self, FeetechError>;
    pub fn open_with_clock(path: &Path, baudrate: u32,
        clock: Arc<dyn RobotClock>) -> Result<Self, FeetechError>;
}
```

open保留给现有deploy调用，委托open_with_clock；初始50ms。Real持clock、总调用timeout。
每次read_exact/write_all固定起点，循环先算剩余，再port.set_timeout(remaining)，
再read/write一次；Interrupted重算剩余，WouldBlock/TimedOut在预算内重试，read/write=0
按语义返回错误而非忙循环；到期RX TransportTimeout{wanted,got}，TX独立
TransportWriteTimeout{wanted,got}映IoWrite。成功不把剩余timeout泄漏为下一调用总预算。
绝不在W/R之间重试整个事务，不自动reopen。

discard=clear(Input)，drain每段≤1ms，见字节重置quiet，绝对150ms不重置；超时返回
DrainReport{deadline_hit:true}仍probe；ioctl/read失败FeetechError::DiscardFailed。
MockTransport同步实现；新增带注入时钟的ScriptedTransport测试夹具覆盖滴流/部分写，
不只用一个直接返回Timeout的mock证明Real循环。Real底层读写核心抽私有泛型
`fn read_exact_bounded<P: Read + Write + SerialPortTimeout>(...)`/
`write_all_bounded`，其中私有SerialPortTimeout只封装set_timeout；真实port和测试短IO
实现共用算法，避免Mock自身实现deadline伪覆盖。完整签名由：

```rust
trait SerialPortTimeout { fn apply_timeout(&mut self, timeout: Duration) -> io::Result<()>; }
fn read_exact_bounded<P: Read + SerialPortTimeout + ?Sized>(port: &mut P,
    clock: &dyn RobotClock, budget: Duration, buf: &mut [u8]) -> Result<(), FeetechError>;
fn write_all_bounded<P: Write + SerialPortTimeout + ?Sized>(port: &mut P,
    clock: &dyn RobotClock, budget: Duration, buf: &[u8]) -> Result<(), FeetechError>;
```

### P.2.2 bus/adapter

`FeetechBus::new`初始50ms；新增服务安装与事务lease：

```rust
impl<T: SerialTransport> FeetechBus<T> {
    pub fn set_services(&mut self, preempt: Option<Arc<dyn PreemptProbe>>,
        diag: Option<Arc<dyn DiagSink>>);
    pub fn set_op_class(&mut self, class: BusOpClass);
    pub fn confirm_hygiene(&mut self) -> Result<DrainReport, FeetechError>;
}
struct BusTransactionLease<'a> { probe: Option<&'a dyn PreemptProbe>, class: BusOpClass }
impl<'a> BusTransactionLease<'a> {
    fn acquire(probe: Option<&'a dyn PreemptProbe>, class: BusOpClass)
        -> Result<Self, FeetechError>;
}
impl Drop for BusTransactionLease<'_> { fn drop(&mut self); }
```

lease借用bus的probe字段，独立借用transport和预分配buffer（不借用整个self，不clone
Arc）；编码与纯参数校验在acquire前完成，begin成功后才发请求；所有Ok/Err/解析失败/
早return由Drop finish归还。一个write_all +
对应read_exact在同一lease内，transport后分别checkpoint，读完才Drop。无响应sync_write
只含write。ping含两事务(PING+Model_Number)，逐事务lease；connect/handshake不能
再包外层normal lease。覆盖public read/write/sync_read/sync_write/
sync_read_by_id_into/sync_write_by_id/sync_write_by_id_into，消除旁路。

周期IO的入口拒绝与在途不可抢占分开：PeriodicRead/PeriodicWrite在发请求前因
acquire败给EStop返回`Aborted(Emergency)`是合法零IO控制结果；serial立即claim并
处理EStop，不计CommFault、不刷新snapshot、不启动其它normal。已经成功acquire的
周期事务仍完成request+response或本次deadline，不能在读半途抢占。D4“Aborted
不可达”收准为“在途不可抢占”，由主agent先Reverse Sync后实施。

`FeetechAdapterConfig`新增`preempt: Option<Arc<dyn PreemptProbe>>`、
`diag: Option<Arc<dyn DiagSink>>`、`call_deadline: Duration`；new在任何返回前安装
bus.set_timeout(call_deadline)，预填joint_ids与safety_stop_entries并校准motor ID。
`connect`只握手不重建entries；首Safety允许已open未handshake，不再被bus.connected
拦掉（只检查transport.is_connected）；普通IO继续要求connect完成。

adapter每个入口明确class：read_state=PeriodicRead，write_command=PeriodicWrite，
safety_stop=Safety，read_torque_enable_mask=Confirm；enable/set_mode/hold/disconnect=
NormalEffectful；connect取probe.operation_class()（无probe默认NormalEffectful）。
Confirm入口依probe.confirmation_trusted()决定hygiene；hygiene错误记诊断、清session/rx
信任由serial读取错误事实后处理，仍完成probe。为不丢“hygiene失败但probe成功”信息，
SerialPreemptProbe增加构造期共享`observed_comm_error: AtomicU8`服务槽（0空，kind+1）与
`fn report_comm_error(&self, kind: CommErrorKind)`，serial每调用后取走并先清信任/记attempt；
该接口不通过Diag是否丢包决定安全。serial在处理probe结果、计算evidence_trusted和
提交安全态之前先消费hygiene错误事实并清信任；每Confirm tick只增加一次attempt，
禁止hygiene+probe两个错误计两次。reviewer round 1已认可该服务接线。

sync_goal与逐电机write重试遇Comm立即返回（首次定级）；设备NACK/goal mismatch沿已批
重试上限并记录Retry/Recovered，sleep20/150/50ms改分片。全部serial可达tracing替换
Diag，factory初始化日志保留。`error.rs::From`完整按D7分类，Handshake分型、
ResponseIdMismatch独立，永久配置映InvalidParameter，禁止Unknown掩盖已知kind。

本组同时修已有依赖告警：`feetech/Cargo.toml`的robot-logging只在
`examples/feetech_poke.rs::main:152`显式init，library只消费tracing。将robot-logging移至
dev-dependencies，real-hw移除`dep:robot-logging`，保留example的required-features。
验证`cargo check -p robot-adapter-feetech --features real-hw --example feetech_poke`与
完整all-features clippy，确保example仍初始化subscriber，library不再有unused dependency。
此为同包最小fix-forward，不是移除生产日志；基线告警记录保持，不宣称原基线零警告。

## P.3 serial收敛与退场（依赖P.1/P.2）

新建 `serial_convergence.rs`，定义无IO phase机；`serial_io_thread`是唯一adapter owner。

```rust
pub enum ConvergeContext { Fault, Emergency, Preflight }
pub enum ConvergeCompletion { AllOffTrusted, AdapterUncertain { kind: CommErrorKind } }
pub struct ConvergeRound {
    pub ctx: ConvergeContext, pub generation: Option<u32>, pub estop_bound: bool,
    pub attempt: u8, pub some_on_seen: bool,
    pub completed_safety: Option<ConvergeCompletion>,
}
pub enum ConvergePhase {
    Idle, StopIssue, StopRetry, Connect, Confirm, AwaitAck,
    SafetyStopFailedAwaitAck { generation: u32, detail: SafetyStopFailDetail },
    SafetyStopFailed,
}
pub enum BootOutcome { Pending, Passed, Faulted }
pub struct SerialConvergence { /* round/phase/fault/boot/stop_attempt/绝对截止 */ }
impl SerialConvergence {
    pub fn new(authorized: bool, joint_count: usize, spawned_at: MonoRawNs) -> Self;
    pub fn step<A: SerialRtAdapter>(&mut self, adapter: &mut A,
        ctrl: &HardRtCtrlInner, probe: &SerialPreemptProbe, diag: &dyn DiagSink,
        clock: &dyn RobotClock, torque_mask: &mut TorqueEnableMask);
    pub fn enter_fault(&mut self, code: FaultCode, detail: u8, origin: FaultOrigin,
        ctrl: &HardRtCtrlInner, diag: &dyn DiagSink, now: MonoRawNs);
    pub fn claim_estop(&mut self, ctrl: &HardRtCtrlInner,
        diag: &dyn DiagSink, now: MonoRawNs);
    pub fn evidence_trusted(&self, ctrl: &HardRtCtrlInner) -> bool;
    pub fn normal_io_allowed(&self, ctrl: &HardRtCtrlInner) -> bool;
}
fn execute_serial_normal<A: SerialRtAdapter>(adapter: &mut A,
    action: u8, arg: u8, ctrl: &SerialIoCtrlInner) -> AdapterResp;
```

step每tick最多一个phase owner；StopIssue写→下一tickConnect→下一tickConfirm，Safety
失败间隔一tick最多3次；每轮confirm最多5次；不可信150ms drain+100ms probe，返回错误
仍按矩阵attempt，不用wall sleep拼测试。`evidence_trusted`: Preflight=authorized &&
session_clean，其余=rx_trust。可信all-off在Preflight先Release rx=true再CAS confirmed。

运行loop顺序：检查stop/boot门 → tick头claim一次 → timeout retire → convergence.step
（活动则本tick无普通IO）→ normal action → policy/inhibit门下periodic write/read →
事务式快照提交 → lease化进度。每正常adapter调用前设置probe上下文；在effectful内部
事务与sleep因EStop退出后立即交claim分派；无下一normal调用的空窗。

retire对Submitted的active/completed/未消费/unmatched按D3全表；发布active→执行→
completed→清active→last_completed（legacy发布仍Release）。abort尚未匹配时先检尚未
消费pending；EStop drain与timeout交错保持EStop义务且仍服务timeout请求。

**S-40（C3 独立 reviewer ACK）**：SerialRT EStop 不再 swap 清 pending；在同一槽
以 strong CAS 标 `PENDING_CANCELLED=1<<63`。空槽/已标记均不写，CAS 失败重读，
不能把消费后提交的新请求覆盖成旧请求。`decode_serial_pending` 仅剥 bit63，严格
校验 seq∈[1,SEQ_MASK]、位56..62全零、非空 action；不借 HardRT 48位解码掩盖非法位。
serial 唯一消费未匹配 timeout 的合法取消词，原 action 零执行：先发布 exact action/arg/seq 的
`completed_effect(success=false)`，再 `last_completed=PreemptedByEmergency`。
匹配 Submitted timeout 时由 timeout 消费端回 AbortedByTimeout（优先于 tag 的普通
取消响应），仍按 PendingDeadline 完成 Fault/gen/ACK，迟到同 seq
timeout 由完成记录匹配。取消 tag 自身不创建 Emergency、不 claim、不改 policy/
gen；发布者在 tag 后、权威 safety_word CAS 前暂停时也如此。Unsubmitted 对别的
请求的取消残留仍为 Unknown 不变量失败。新增同一 CAS 实现的 test-only hook 覆盖
CAS 败给消费、tag 后暂停、双 publisher、迟到 timeout；root 独立验收 AC-4(n/o/p)
与 AC-9(o7)，不复制生产算法、不增加新事实槽。

**无seq的正常路径已核码**：Step3建立`Some(plan,None)`后IPC停顿超过1s，下次Step2先
超时甚至尚未调用request_normal；因此不能只考虑Busy或称不变量错误。采用
`NormalAbortTarget::{Submitted{seq},Unsubmitted}`，唯一additive方法不增加数量。
IPC超时按seq_opt构造target，先预提交CommFault+Disabled，再Release发布tagged词。
serial先判Unsubmitted高tag，此分支不执行原尚未提交action，以PendingDeadline/
PendingTimeout进入统一Fault收敛并发布gen、等待ACK；不清session_clean/rx_trust，不
置EStop latch。Busy时同样使用Unsubmitted，不把被占用slot的seq当成本请求seq。
上述不清信任只适用于正常无残留的Unsubmitted路径；若持续Busy仍残留非本请求的
pending/active，先记`InvariantViolation`，以`Unknown`清除session_clean/rx_trust，
阻止残留发起新普通IO，在当前事务边界退场并完整收敛，不借其seq冒充本请求完成，
不沿PendingDeadline保留信任。Mailbox既有更严重Fault时保持严重度并完成
既有gen义务；EStop同时到达时合并estop_bound，绝不消费timeout后遗漏ACK收敛。
消费后用`compare_exchange(observed_word,0,AcqRel,Acquire)`清理确切已处理请求，
不使用无条件store/swap吞掉后来发布的请求；abort判tag永远先于seq mask，0仍永不
匹配合法seq。该接缝由主agent先Reverse Sync S39/D3/AC后实施。
completed Enable(false)全成功仅免物理IO，仍发gen、封顶、AwaitAck。

enter_fault统一state→inhibit→mailbox；AdapterErr<CommFault<SSF单调；同级不发新gen，
提升CommFault至多一次重跑StopIssue，SSF直接零IO等ack。old ack不清新位；没有gen的
Emergency可Done，有gen无一例外等ACK。普通Fault不置safety_latched/estop文件，所有
SSF相位零policy写。StopIssue且estop_bound只调用tighten条件CAS。

AwaitAck消费新EStop必须先看本轮completed_safety：可信保留ctx/gen/reasons/BOOT/
FAULT_PENDING重开物理轮，清完成记录，再次结束仍AwaitAck(n)；不可信或None零IO升级
新SSF。早到ACK不能放过未完成物理轮。所有启动重开共用原t_spawn/绝对deadline/
首次物理出口ACK截止，不重复Connect。

`boot_deadline_expired`下一事务边界best-effort：至少一次Safety后跳剩余confirm，
记录Some(AdapterUncertain{Unknown})→Fault/SSF等ack，不能None进AwaitAck。运行通信
Fault不主动退出，只有真停滞由liveness停喂，启动执法例外按D9。

## P.4 IPC与观测（依赖P.1，与P.3成组联调）

### P.4.1 lane数据类型与完整接口

新增 `event_ingress.rs`，`state_machine.rs::EventQueue`复用其类型：

```rust
pub enum LinkEvent { Heartbeat, HeartbeatTimeout }
pub enum ControlEvent {
    Enable, Disable, ClearError, ClearEStop, Stop,
    AdapterErr(FaultCode), CmdDeadlineExpired, LimitViolation, WatchdogTimeout, RtOverrun,
}
pub enum RrEvent { ClearEStop(ClearEStopRrRequest), SetEnabled(SetEnabledRrRequest) }
pub enum AdmitError { Full, Disconnected }
pub struct EmergencyBatch { pub estop: bool, pub ssf: Option<MailboxSnapshot> }
pub const RR_CHANNEL_CAPACITY: usize = 64;
pub struct ExternalEvents { /* bounded Sender<ControlEvent> + 类型化emergency来源 */ }
impl ExternalEvents {
    pub fn estop(&self);
    pub fn control(&self, event: ControlEvent) -> Result<(), AdmitError>;
}
impl EventQueue {
    pub fn admit_link(&mut self, event: LinkEvent);
    pub fn admit_rr(&mut self, event: RrEvent) -> Result<(), RrEvent>;
    pub fn admit_control(&mut self, event: ControlEvent) -> Result<(), AdmitError>;
    pub fn pop_link(&mut self) -> Option<LinkEvent>;
    pub fn pop_rr(&mut self) -> Option<RrEvent>;
    pub fn pop_control(&mut self) -> Option<ControlEvent>;
    pub fn set_estop(&self);
    pub fn claim_estop(&self) -> bool;
}
```

link64、rr128、control128，构造with_capacity后不超过容量；link overflow在有序回放
结束后合成Timeout且占16预算之一，不能在本tick提前越过尚未回放Heartbeat。control
相邻重复合并；rr失败返回原请求以回复ResourceExhausted。HardRT外部emergency使用
共享AtomicBool legacy notice（EventQueue持Arc），不经有界channel。

`ipc_tick`分解为具名step并由一个完整调度函数驱动，不让旧ipc_tick一次drain全queue：

```rust
fn apply_emergency_batch(ctx: &mut IpcTickContext<'_>, batch: EmergencyBatch) -> bool;
fn service_fault_mailbox(ctx: &mut IpcTickContext<'_>);
fn advance_pending(ctx: &mut IpcTickContext<'_>) -> bool;
fn apply_control_event(ctx: &mut IpcTickContext<'_>, event: Event) -> TickOutcome;
fn process_rr_event(ctx: &mut IpcTickContext<'_>, request: RrEvent,
    replies: &RrReplySinks<'_>, pending: &mut PendingReplies) -> TickOutcome;
```

`IpcTickContext<'a>`字段完整为`retry: &'a mut IpcRetryState, state: &'a mut S0State,
guards: &'a mut Guards, executor_ctrl: &'a dyn ExecutorCtrlClient, serial_ctrl:
Option<&'a HardRtCtrlInner>, clock: &'a dyn RobotClock, latch_path: Option<&'a Path>`。
`RrReplySinks`分别借用Option<&Sender<ClearEStopRrResponse>>与Option<&Sender<SetEnabledRrResponse>>；
`PendingReplies`持旧in_flight_live和pending_set_enabled，**不持尚未dequeue的请求**。
旧`ipc_tick`测试入口可委托相同step，不能保留一份行为分叉的实现。

### P.4.2 调度接线

run_loop Step0：command既有校验、heartbeat一次、topic EStop单原语；claimIPC notice，
SSF mailbox快照；wd/external/ClearRR/SetRR各最多8。RR ingress仅检查第一道deadline
与admission，绝不evaluator/latch/Event/pending。Step1 emergency（含EStop先fsync）→
Step1b无条件一次mailbox，才能emergency return。新SSF晚于快照只留下一tick处理，
ACK只用读到generation。

Step2 advance pending：仍在飞return；**本tick成功或失败retire后继续**link16→rr24→
control24，不保留基线成功ACK后无条件return。enable commit后timeout同tick落Lost再
发快照。RR pop后重读clock，过期回复DeadlineExceeded且零evaluator；未过期才
precondition→事件即时执行→必要时建立等待结果的PendingReplies。创建effectful
pending即停止余rr/control；不把已消费RR事件再塞control尾部，否则造成超期和排队
次序漂移。ClearEStop仍使用既有落盘/clear/guard三步原子业务处理。

SetEnabled pending在Fault(AdapterErr|CommFault|SafetyStopFailed)返回AdapterError；
入队前已Fault下Disable保持既有幂等Disabled，不能改为收敛操作。业务错误响应的顺序
仍保持emergency/meta优先。两个RR response容量=max_in_flight=64，共用常量、const
assert+配置校验，response.send保留；新rr lane只是这64+64在飞请求的迁移，不新增
生产者或额外响应。ClearEStop lane满复用已有ResourceExhausted对应域载荷（若需内部
载荷分支可增加，wire不增加）；SetEnabled同理。

RR三个生命周期严格分开：rr_server的`PendingRr`在成功向request channel转发后建立
（既有）；`RrEvent`代表尚在IPC lane排队、未执行的请求；`PendingReplies`仅在dequeue
第二道deadline通过且确需等待业务结果时建立。每个请求全过程至多一个response：
source过期、lane Full、dequeue过期、evaluator拒绝、emergency取消、完成互斥终结，
通过移动所有权取走请求保证不二次reply。服务端自己的pending上限仍64，与IPC lane
容量128是两组source总在飞数，不扩大max_in_flight。

`Guards::rt_serial` + try_transition Fault/EStop保原Fault + classifier给Emergency/
Disabled；HardRT EStop通配Safe/StopTorque不变。CommFault从Safe/Ready/Enabled进Fault，
commit_fault加severity保护。删除ClearError自授权；SerialRT每tick由boot Passed &&
rx_trust && inhibit==0推adapter_connected，adapter_fault_cleared还需confirmed &&
L0持续500ms。HardRT初值语义保持。

pub `publish_joint_state`用inner.safety_state()，map_state_to_wire补CommFault，pub
每poll消费Diag（有限256个）+1Hz摘要；不adapterIO、不读写ConvergeRound，不以日志
反推权威安全态。IPC三类故障直接结构日志带seq/action/kind/elapsed。

## P.5 启动监督与信任文件（P.1后辅助模块可写，完整接线等P.3/P.4）

### P.5.1 marker及锁

新增 `rx_trust.rs`，不改estop_latch：

```rust
pub enum MarkerWriteError { NotCommitted(io::Error), CommittedNotDurable(io::Error) }
pub enum MarkerValue { Dirty, AuthorizedOnce, Missing, Corrupt }
pub struct MarkerStatus { pub primary: MarkerValue, pub backup: MarkerValue }
pub struct ConsumeOutcome { pub authorized: bool, pub status: MarkerStatus }
pub enum MarkerCommand { Authorize, Revoke, Status }
pub enum MarkerCommandOutcome { Complete, Rejected, AuthorizedNotDurable, RevokedNotCanonical }
pub enum TrustLockError { Held { pid: Option<u32> }, Io(io::Error) }
pub struct RxTrustLock { /* 持有File；绝不rename/delete锁文件 */ }
pub trait MarkerFs {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn write_sync_temp(&self, path: &Path, bytes: &[u8]) -> io::Result<PathBuf>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn sync_parent(&self, path: &Path) -> io::Result<()>;
    fn remove_temp(&self, path: &Path) -> io::Result<()>;
}
pub fn write_marker_two_phase(fs: &dyn MarkerFs, path: &Path,
    value: MarkerValue) -> Result<(), MarkerWriteError>;
pub fn marker_status(fs: &dyn MarkerFs, dir: &Path) -> io::Result<MarkerStatus>;
pub fn consume_authorization(fs: &dyn MarkerFs, dir: &Path) -> io::Result<ConsumeOutcome>;
pub fn run_marker_command(fs: &dyn MarkerFs, dir: &Path,
    command: MarkerCommand) -> Result<MarkerCommandOutcome, io::Error>;
pub fn acquire_trust_lock(dir: &Path) -> Result<RxTrustLock, TrustLockError>;
```

临时文件同目录create_new+0600；write+fsync→rename→父fsync，失败清temp；对缺失/损坏
内容和权限错误分开。锁open0600+CLOEXEC+LOCK_NB，EINTR重试，EAGAIN/EWOULDBLOCK=87，
其它=1；取锁后才写pid，持进程生命周期。锁操作抽私有`LockIo` syscall seam供errno
注入，不使用进程列表替代排他。status也取锁。

authorize严格1/1→backup0持久→primary0；primary rename后dir失败=3，前面失败=2。
revoke backup1未持久=2，backup持久后primary失败=4，全成=0。consume只0/0且两份均
持久收紧才authorized，backup phase1失败拒启动；phase2失败只Dirty继续规范化。
所有其余truth table行和掉电回退严格沿D9，不删除/恢复旧marker快照。日志uid、
RobotClock时间、原内容分类、结果和退出码；不向SPSC写主线程事件。

### P.5.2 supervisor、workerstop与同源启动门

新增 `boot_supervisor.rs` / `worker_stop.rs`，robot-health additive READY：

```rust
pub enum ReadyOutcome { Sent, NotConfigured, Err(io::Error) }
pub fn try_notify_ready() -> ReadyOutcome;
pub enum NotifyMode { Required, Standalone, Auto }
pub enum WorkerKind { Ipc, ClearEStopRr, SetEnabledRr, Watchdog, Executor, Pub, Io }
pub struct StopFlags { /* [Arc<AtomicBool>; 7] + spawned位图 */ }
impl StopFlags {
    pub fn new() -> Self;
    pub fn flag(&self, worker: WorkerKind) -> Arc<AtomicBool>;
    pub fn mark_spawned(&self, worker: WorkerKind);
    pub fn request_spawned_stop(&self);
    pub fn all_spawned(&self) -> bool;
}
pub struct BootShared {
    pub spawned_reached: AtomicBool, pub ready_reached: AtomicBool,
    pub joint_count: AtomicUsize, pub supervisor_fired: AtomicU8,
    pub spawned_at_ns: AtomicU64,
}
pub struct BootSupervisor { /* Arc<BootShared>、t_start、join */ }
impl BootSupervisor {
    pub fn start() -> io::Result<Self>;
    pub fn shared(&self) -> Arc<BootShared>;
    pub fn update_joint_count(&self, count: usize);
    pub fn mark_workers_spawned(&self, now: MonoRawNs);
    pub fn mark_ready(&self);
}
pub const fn boot_budget(joints: usize) -> DurationNs;
pub const fn absolute_boot_budget(joints: usize) -> DurationNs;
```

`main()`第一语句调用BootSupervisor::start，内部先取t_start再spawn；日志/parse之前。
supervisor硬循环直接SystemClock（不用dyn trait）、原子读取、50ms sleep；执法只有
Relaxed fired + libc::_exit(88)，不日志/析构。joint_count初32、resolve后只缩短，
CP1=5s，CP2=T_abs+1s；read_reached后自退。帮助/CLI出错由进程退出终止supervisor，
不能先无界join。

**同源首IO门**：executor先spawn时BOOT保持，serial只读`spawned_reached`，未置时
不connect/safety/read/write。主线程七者mark_spawned后取t_spawn，先Relaxed写
spawned_at_ns再Release写spawned_reached；serial Acquire取得同一个t_spawn后创建
SerialConvergence并开始StopIssue。daemon等待截止亦从该t_spawn算，不能executor
spawn时提前计算。HardRT不加IO启动门，保持原语义。

七worker完整新签名（追加stop参数，既有stop构造移出）：

```rust
pub fn spawn_ipc_thread<F>(cfg: IpcThreadConfig, clock: Arc<dyn RobotClock>,
    subscribers_factory: F, cmd_writer: LatestCommandWriter<JointCommandBuf>,
    state_pub: RtSnapshotPublisher<S0StateSnapshot>, watchdog_rx: Receiver<Event>,
    executor_ctrl: Arc<dyn ExecutorCtrlClient>, rr_channels: Option<RrChannels>,
    set_enabled_rr_channels: Option<SetEnabledRrChannels>,
    serial_ctrl: Option<Arc<HardRtCtrlInner>>,
    emergency_ctrl: Option<Arc<dyn SerialEmergencyCtrl>>, stop: Arc<AtomicBool>)
    -> io::Result<IpcThreadHandle>
where F: FnOnce() -> IpcSubscribers + Send + 'static;
pub fn spawn_rr_server_thread<F>(cfg: RrServerThreadConfig, clock: Arc<dyn RobotClock>,
    server_factory: F, rr_request_tx: Sender<ClearEStopRrRequest>,
    rr_response_rx: Receiver<ClearEStopRrResponse>, stop: Arc<AtomicBool>)
    -> io::Result<RrServerThreadHandle>
where F: FnOnce() -> Iceoryx2RrServer<markers::ClearEStopRequest, markers::ClearEStopResponse>
    + Send + 'static;
pub fn spawn_set_enabled_rr_server_thread<F>(cfg: RrServerThreadConfig,
    clock: Arc<dyn RobotClock>, server_factory: F, rr_request_tx: Sender<SetEnabledRrRequest>,
    rr_response_rx: Receiver<SetEnabledRrResponse>, stop: Arc<AtomicBool>)
    -> io::Result<SetEnabledRrServerHandle>
where F: FnOnce() -> Iceoryx2RrServer<markers::SetEnabledRequest, markers::SetEnabledResponse>
    + Send + 'static;
pub fn spawn_watchdog(cfg: WatchdogConfig, clock: Arc<dyn RobotClock>,
    last_hb_ns: Arc<AtomicU64>, event_tx: Sender<Event>, stop: Arc<AtomicBool>)
    -> io::Result<JoinHandle<()>>;
pub fn spawn_serial_io_thread<A>(cfg: SerialIoThreadConfig, clock: Arc<dyn RobotClock>,
    adapter: A, cmd_reader: LatestCommandReader<JointCommandBuf>,
    snapshot_pub: RtSnapshotPublisher<RtSnapshot>, ctrl: Arc<SerialIoCtrlInner>,
    probe: Arc<SerialPreemptProbe>, diag: Arc<SerialDiagRing>, authorized: bool,
    boot: Arc<BootShared>, stop: Arc<AtomicBool>) -> io::Result<SerialIoThreadHandle>
where A: SerialRtAdapter + Send + 'static;
pub fn spawn_rt_thread<A>(cfg: RtThreadConfig, clock: Arc<dyn RobotClock>, adapter: A,
    cmd_reader: LatestCommandReader<JointCommandBuf>, snapshot_pub: RtSnapshotPublisher<RtSnapshot>,
    ctrl: Arc<HardRtCtrlInner>, stop: Arc<AtomicBool>) -> io::Result<RtThreadHandle>
where A: HardRtAdapter + Send + 'static;
pub fn spawn_pub_thread<F>(cfg: PubThreadConfig, clock: Arc<dyn RobotClock>,
    publishers_factory: F, rt_snap_rdr: RtSnapshotReader<RtSnapshot>,
    state_rdr: RtSnapshotReader<S0StateSnapshot>, rt_ctrl: Arc<HardRtCtrlInner>,
    diag: Option<Arc<SerialDiagRing>>, stop: Arc<AtomicBool>) -> io::Result<PubThreadHandle>
where F: FnOnce() -> IpcPublishers + Send + 'static;
pub fn spawn_io_thread(cfg: IoThreadConfig, clock: Arc<dyn RobotClock>,
    stop: Arc<AtomicBool>) -> io::Result<IoThreadHandle>;
```

watchdog当前expect spawn改Result，使七者任一失败归退1；心跳发bounded.try_send，Full
计合并，Disconnected停止。所有handle新增`is_finished(&self)->bool`，request_stop
委托同预建flag（swap去重），join保留。StopFlags在parse后、resolve前建；mark_spawned
只能成功拿到handle后记位，故IPC ready握手卡住CP1判未全spawn，而stop已可预置。

### P.5.3 daemon接线

`Args`新增`--rx-trust-dir`、`--rx-trust-policy marker|static`、`--notify-mode`，以及
`rx-trust authorize|status|revoke --robot-id --latch-path/--rx-trust-dir`子命令；未知/冲突
退2。`validate_hardware_section`新增TooManyJoints{actual,max}，CLI joints1..=32。
`SpawnSelection`在打开adapter前按注册名锁kind/policy：Feetech=Marker/禁止override，
mock_serial=Static可marker，mock_hard=HardRt无policy/拒override，其它SerialRT拒2。
共享ctrl、SerialPreemptProbe、DiagRing、AdapterBuildServices都在open前构造；
`build_adapter(args: &ResolvedArgs, services: Option<&AdapterBuildServices>)`→打开/安装
D_call→核真实RtClass与selection一致→消费marker。未知/错误不会消费授权。

锁/latch/open/consume后按既有七顺序spawn并发布门；HardRT不走marker/preflight，
仍持rx_trust.lock。SerialRT启动guard先false；等待物理出口和gen ack，两者都过才
READY；首次带gen出口t_exit只设置一次，截止min(t_exit+2s,T_abs)。超期先Disabled、
已spawn旗标一次置位，只对is_finished者join，共用T_abs+1s截止；不调用正常路径
无界graceful_stop。READY失败同有界关停。正常关停仍现有语义。

READY按Sent实际send_to成功；未配置仅standalone算就绪；abstract明确Unsupported；
auto看INVOCATION_ID。auto-enable仅Passed；pub/IPC不因READY才开始服务，故启动
Fault/Disabled/ack可被观察。liveness observer SerialRT接同一个ctrl计数；HardRT仍
metrics.tick_count，READY后的systemd watchdog不计CP1。

## P.6 独立验收映射与可执行命令

### P.6.1 测试文件、场景与拒绝伪覆盖

由主 agent（QA Owner）新增 `crates/robot-s0/tests/comm_fault_protocol.rs`、`comm_fault_startup.rs`、
`rx_trust_faults.rs`；Feetech `tests/comm_fault_transport.rs`；core `executor_ctrl.rs`
编码单测。确定性调度seam仅`cfg(test)`或`robot-s0/test-comm-fault-hook`，默认生产编译
无阻塞钩子。原子竞态seam位置必须按AC9(o/q/r)、mailbox①②③、ACK冻结精确放置，
不能用随机sleep“碰运气”。reviewer先给独立场景表，主 agent按接口写验收夹具，reviewer
独立检查反例确实能使错误实现失败。

新增 `robot-tests` feature `comm-fault-e2e`（第10项单feature gate）：
`["dep:robot-s0", "robot-s0/test-comm-fault-hook", "dep:robot-rpc", "robot-rpc/client",
"dep:robot-adapter-feetech", "dep:tokio", "dep:tonic", "dep:serde_json"]`。
现有robot-s0 optional依赖`default-features=false`，新feature还需显式
`robot-s0/mock`、`robot-s0/feetech`，不能依赖其它feature unification。
新增bin `comm_fault_e2e`、`required-features=["comm-fault-e2e"]`；support模块
`src/comm_fault/{mod,harness,scenarios,report}.rs`以bin的path模块引用，不新建外部crate。

CLI：`comm_fault_e2e --phase mock|startup|rpc|all|orin-characterize|orin-sla|orin-broadcast
--bin-dir <abs> --evidence-dir <abs> [--case <AC编号>]`。默认mock；all仅软件三phase。
Orin入口额外要求`--config <abs> --hardware-id <id> --ack-supervised-hardware`且构建
`comm-fault-e2e,real-hw`；本轮不执行。无ack/无匹配配置一律退2、零open。harness注入
只用test-hook S0，生产release不得部署带hook产物；报告记录artifact hash/features。

| AC | 具名测试/场景（新名为锁定计划名） | 命令组/证据 |
|---|---|---|
| 1 | `legacy_wire_additive_only`；下节全部现有target/smoke，HardRT线程测试 | G1/G4，旧API源码差异、test-list、退出码 |
| 2 | `read_drip_preserves_got_deadline`、`write_drip_tx_timeout_kind`、`old_all_off_frame_never_restores_trust`、`trust_clearing_kind_matrix` | G2/G3；Orin characterization=H1 |
| 3 | `budget_census_n6_n13_n32` | G2，W/R/S及完整预算表断言 |
| 4 | `abort_active_completed_pending_matrix`、`abort_unmatched_fails_closed`、`abort_seq_isolation_and_zero_boundary`、`unsubmitted_timeout_before_first_request`、`unsubmitted_timeout_busy_and_emergency_mailbox_races`、`abort_consumption_does_not_erase_new_request`、`transaction_response_is_not_preempted`、`sleep_quantum_abort`、`disable_completed_retire_exemption_requires_ack`、`safety_confirm_connect_ignore_abort` | G2/G3，逐IO/seq/gen记录，含2^40边界与false trust对照 |
| 5 | `nth_motor_nack_converges`、`never_confirmed_with_torque_on`、`fault_converges_without_serial_policy_write` | G2，冻结IPC到confirm后 |
| 6 | `reachable_bus_error_route_matrix`、`unreachable_route_cells_are_guarded`、`periodic_admission_emergency_is_zero_io_no_fault`、`safe_disconnect_fault_code_seven` | G2/G3；六class每格case参数输出 |
| 7 | `comm_fault_wire_mapping_is_exhaustive`、`pub_safety_reads_selected_ctrl_kind` | G2/G3，真实topic与内部源对照 |
| 8 | `mailbox_full_lanes_pending_two_ticks`、`mailbox_generation_upgrade_ack`、`mailbox_publish_order_interleavings`、`mailbox_continuous_estop_four_ticks`、`ssf_after_step_zero_acked_next_tick`、`generation_saturates_fail_closed` | G2，①②③暂停与ACK序列 |
| 9 | `converge_phase_transition_matrix`、`stop_retry_three_then_ssf`、`emergency_batch_twelve_cells`、`estop_single_cas_interleavings`、`estop_prespawn_and_claim_republish`、`serial_fault_estop_plan_matrix`、`normal_io_acquire_total_order`、`serial_policy_cas_interleavings` | G2，o1–o7/q1–q2/r1–r4逐子case |
| 10 | `diag_ring_drop_newest_preserves_oldest`、`diag_retry_recovered_fields`、`serial_thread_emits_no_tracing` | G2，线程名+事件字段+drop计数 |
| 11 | `rr_capacity_dead_server_no_block`、`event_lanes_overflow_by_kind`、`event_lanes_continuous_flood_quotas`、`rr_second_deadline_both_rpc_zero_effects`、`enable_retire_applies_link_before_publish`、`stop_full_channel_already_disables` | G2/G3，10s RSS、2s配额、two RPC负向 |
| 12 | `external_estop_once_survives_caller_return` + `estop_cas_latency_distribution` | G2/G3软件原子性；H2 Orin100warmup+1000样本，P50/P99/max/σ/CAS |
| 13 | `safety_broadcast_mock_under_ten_ms` | G2；H3实机仅待采，不把Mock当实机 |
| 14 | `factory_trust_ctrl_dispatch_matrix` | G2/G3，kind/override/Some/Guards/ctrl四项穷尽 |
| 15 | `preflight_order_and_trust_publication`、`dirty_restart_rejects_enable`、`preflight_rx_error_caps_confirmed`、`latch_marker_independent` | G2/G3，文件/原子/topic时间线 |
| 16 | `liveness_only_after_transport_return`、`liveness_lease_expiry_freezes_even_checkpoints`、`hung_io_stops_feeding`、`connect_lease_progress` | G2/G4，反向checkpoint不退场 |
| 17 | `rpc_comm_fault_mailbox_observe`、`rpc_three_faults_adapter_error` | G3真子进程，10ms GetState/RTT |
| 18 | `adapter_error_all_bytes_roundtrip` | G2，256字节 |
| 19 | `context_confirm_exit_matrix`、`emergency_fault_keeps_estop_obligation`、`await_ack_reopen_keeps_generation_and_early_ack`、`await_ack_uncertain_escalates_without_io`、`all_generations_wait_for_ack` | G2，i–vii含startup deadline与Connect一次 |
| 20 | `spawn_caller_no_bus_io`、`first_safety_deadline_installed_before_io`、`deadline_install_error_preserves_authorization` | G2/G3，七worker首IO门与安装失败 |
| 21 | `marker_truth_table`、`marker_authorize_canonical_matrix`、`marker_consume_failure_matrix`、`marker_revoke_failure_and_power_loss_matrix` | G2，每阶段故障/掉电回退，逐字节检查 |
| 22 | `boot_ready_and_auto_enable_order`、`boot_timeout_has_completion_record`、`boot_supervisor_checkpoint_process_matrix`、`seven_worker_spawn_failure_matrix`、`notify_mode_socket_matrix`、`joint_count_resolve_upper_bound` | G3，每l-1…l16独立子进程，包括pub/io门与ctor盲区登记 |
| 23 | `estop_merge_each_physical_phase`、`await_ack_reopen_round_budget`、`ssf_terminal_repeated_estop_no_io_or_policy`、`estop_claim_counts_allow_level_merge` | G2/G3，每轮独立预算 |
| 24 | `trust_lock_two_daemons_and_commands`、`trust_lock_errno_matrix`、`trust_lock_sigkill_releases`、`deploy_units_restart_and_notify_contract` | G2/G3，tmpdir实例，真实flock |
| Q | `gate_manifest_and_exit_codes` | G0–G4，fresh clippy真实退出码、产物hash/feature、环境限制 |

这些是全部AC到具名入口的映射；每个AC子条（a/b/o1等）在报告作为独立case id。
框架不允许遗漏case时AC仍PASS；NEEDS_HUMAN/未运行不会按success计入。

### P.6.2 现有基线具名清单

基线权威清单已落 `.verify/post_e_comm_fault_hardening/baseline.json`，包含20个integration
targets、155个具名用例及2个既有ignored，枚举命令与每target的required_features都在该文件。
AC-1的`dimos_workflow_e2e`确有12个测试：11个主模块tokio测试，加导入的
`common::subprocess::tests::shutdown_dump_contains_sigterm_handler_output`。主模块具名为：
read_state_both_arms_layout_validated、chunk_pair_scripted_with_gripper、
consecutive_chunks_no_pullback_no_jump、post_success_reset_matrix_cancel、
post_success_reset_matrix_failure、post_success_reset_matrix_ik、
post_success_reset_matrix_hold_pose、start_pose_stale_rejected、pair_partial_failure_paths、
nontarget_fill_current、s0_drops_wrong_layout_commands。必须以Cargo枚举包含导入模块的
全集锁回归，不能仅grep顶层test误计。不得删除、ignore、关feature或降低断言来过门；
现有2项ignored不冒充本轮PASS。baseline.json记录枚举，不宣称155项已执行通过。

还必须完整运行rpc targets：`task_rpc_e2e`、`state_rpc_e2e`、`set_enabled_e2e`、
`estop_e2e`；s0 `serial_rt_e2e`五条：test_serial_rt_dispatch_chooses_serial_factory、
test_heartbeat_drives_enable_via_serial_ack、test_estop_topic_preempts_and_safety_state_reflects_serial_confirm、
test_clear_estop_then_enable_again_no_safety_latch_reject、
test_same_tick_enable_then_clear_estop_via_channel_does_not_misenable；以及`rt_guard_soak`、
`deploy_units`、`deploy_guard_kill`、health systemd tests、transport compat/trybuild。
既有mock SerialRT启动断言按新preflight语义更新，不降低检测目标。

### P.6.3 命令与执行顺序

所有命令在仓库根执行；主agent串行运行Cargo，Dev/reviewer不争抢同一target目录。
G0记录git HEAD/diff、`rustup run nightly rustc -Vv`、`cargo metadata --no-deps`与环境。
PX44 fresh使用`rg --files crates -g '*.rs' -0 | xargs -0 touch`（仅mtime、不清缓存）。
每commit前重复G1；G2按工作包选择，完整交付所有G2；release子进程产物在G3前强制重建。

```bash
# G1：格式与静态门，逐条成功才继续。
cargo +nightly fmt --all
rg --files crates -g '*.rs' -0 | xargs -0 touch
cargo +nightly clippy --all --all-targets --keep-going -- -D warnings
cargo +nightly clippy -p robot-tests --all-features --all-targets --keep-going -- -D warnings

# G2：纯类型/单测/确定性协议。
CARGO_BUILD_WARNINGS=deny cargo +nightly test --workspace --all-features --lib --bins
CARGO_BUILD_WARNINGS=deny cargo +nightly test -p robot-adapter-feetech --test comm_fault_transport
CARGO_BUILD_WARNINGS=deny cargo +nightly test -p robot-s0 --features test-comm-fault-hook --test comm_fault_protocol --test rx_trust_faults
CARGO_BUILD_WARNINGS=deny cargo +nightly test -p robot-transport --no-default-features --features mock

# 单feature隔离（每项独立命令，不合并features）。
for feature_name in freeze-systemd-smoke clear-estop-e2e pubsub-cleanup-smoke scripted-backend-smoke ik-backend-smoke robotctl-smoke sd-watchdog-smoke real-hw supervised-demo comm-fault-e2e; do
  cargo +nightly check -p robot-tests --no-default-features --features "$feature_name" --all-targets || exit "$?"
done

# G3前先构建所有当前checkout的release产物。
CARGO_BUILD_WARNINGS=deny cargo +nightly build --release --workspace
CARGO_BUILD_WARNINGS=deny cargo +nightly build --release -p robot-s0 --features xlerobot-real --bin xlerobot_deploy --bin s0_daemon
CARGO_BUILD_WARNINGS=deny cargo +nightly build --release -p robot-s1 --features test-freeze-hook,test-fault-backend --bin s1_daemon
CARGO_BUILD_WARNINGS=deny cargo +nightly build --release -p robot-rpc --features test-freeze-hook --bin robot_rpc_gateway
CARGO_BUILD_WARNINGS=deny cargo +nightly build --release -p robot-tests --features pubsub-cleanup-smoke,scripted-backend-smoke,ik-backend-smoke,robotctl-smoke,clear-estop-e2e,sd-watchdog-smoke,freeze-systemd-smoke,supervised-demo,comm-fault-e2e
CARGO_BUILD_WARNINGS=deny cargo +nightly build --release -p robot-s0 --features test-freeze-hook,test-comm-fault-hook --bin s0_daemon

# G3：最后一条S0为隔离mock专用hook产物，必须在artifact记录中标记不可部署。
export B4_E2E_BIN_S0_DAEMON="$PWD/target/release/s0_daemon"
export B4_E2E_BIN_S1_DAEMON="$PWD/target/release/s1_daemon"
export B4_E2E_BIN_ROBOT_RPC_GATEWAY="$PWD/target/release/robot_rpc_gateway"
CARGO_BUILD_WARNINGS=deny cargo +nightly test --workspace --all-features -- --test-threads=1
./target/release/comm_fault_e2e --phase all --bin-dir "$PWD/target/release" --evidence-dir "$PWD/.verify/post-e"
./target/release/crash_restart_smoke --bin-dir "$PWD/target/release"
./target/release/clear_estop_e2e --case all
./target/release/pubsub_cleanup_smoke --phase all --bin-dir "$PWD/target/release"
./target/release/scripted_backend_smoke --phase all --bin-dir "$PWD/target/release"
./target/release/ik_backend_smoke --phase all --bin-dir "$PWD/target/release"
./target/release/robotctl_smoke --phase all --bin-dir "$PWD/target/release"

# G4：systemd真实可用才运行；不可用登记NEEDS_HUMAN，不模拟PASS。
./target/release/sd_watchdog_smoke --s1-bin "$PWD/target/release/s1_daemon" --gateway-bin "$PWD/target/release/robot_rpc_gateway" --phase all
./target/release/freeze_systemd_smoke --daemon-bin "$PWD/target/release/s0_daemon" --unit-template "$PWD/scripts/systemd/xlerobot-s0-freeze-test.service" --workdir-base "$PWD/.verify/post-e/freeze" --unit-name-prefix post-e-freeze --robot-id-prefix post_e_freeze --iterations 5 --worker ipc

# schema、allocation、依赖门。
cargo +nightly run -p robot-msg --bin schema_lint
cargo +nightly test -p robot-msg --test compat
cargo +nightly test -p robot-s0 --features rt_guard --test rt_guard_soak -- --ignored --test-threads=1
cargo +nightly test --release -p robot-s0 --features rt_guard --test rt_guard_soak -- --ignored --test-threads=1
cargo +nightly test -p robot-transport --test iox2_sub_first_repro parent_pub_then_child_s1_open -- --ignored --exact --test-threads=1
cargo deny check
cargo +nightly build --workspace --all-targets --timings
```

freeze_systemd_smoke自行在XDG_RUNTIME_DIR的user目录渲染隔离unit，主agent只运行该
测试unit，不能安装/重启生产服务。user systemd不可用时该门保持NEEDS_HUMAN。
`cargo deny`工具缺失若无法按既有工具链
安装则记录门未过。无新增第三方版本；新增内部feature边仍附timings和cargo tree。

`rt_guard_soak`有crate级feature门与test级ignored门，debug/release两命令均显式带
`rt_guard`和`--ignored`，保持默认60秒，日志至少一条测试真实执行。另一既有ignored
`parent_pub_then_child_s1_open`按上列命令显式执行，消费此前构建的当前checkout
`target/release/s1_daemon`；若环境/历史夹具问题失败，记录FAIL并诊断，不删除测试或
将仍ignored计成PASS。RPC三个B4 override一律指向本轮已构建产物，避免release旧缓存
或debug fallback。独立`comm_fault_startup`也在G3完整workspace tests中执行；没有
feature导致0 tests的输出属于门未运行，不能判PASS。

软件全门后重新不带test-hook构建生产artifact，记录sha256、commit、features，避免
混合release目录的最后一份带注入代码。硬件H1/H2/H3命令仅归档，分别使用
`--phase orin-characterize|orin-sla|orin-broadcast`，H2每场景100warmup/1000样本，
保存CSV/P50/P99/max/CAS重试分布/σ_est与Orin PX56环境；P99门不在本机推算。

## P.7 部署与整候选回滚（仅形成可评审操作单，本轮不执行）

改两份arm service：`RestartPreventExitStatus=86 87`、guard `--`后加
`--notify-mode required`，注释解释88可重启及supervisor边界；保留guard六步、
TimeoutStartSec=infinity、左右身份、serial FIFO/core配置。运行身份均loosqk:loosqk，
SupplementaryGroups=dialout；左xlerobot_left/left_arm，右xlerobot_right/right_arm；
配置分别`/etc/robot/xlerobot_left_arm.toml`与`...right_arm.toml`，串口身份必须以
`deploy::identity`既有D2 by-id精确校验，不凭ttyACM序号选择。默认trust目录分别
`/var/lib/robot/xlerobot_left`与right（沿latch parent），目录0750，文件0600。

正式现场先确认机器人姿态/断扭与D15风险后，按“左完成并验证，再右”执行：

1. 将生产不带hook的s0_daemon/xlerobot_deploy、两个unit、两配置的sha256与原文件备份
   到`/var/lib/robot/deploy-backups/post-e-<commit>`；不备份用于恢复的授权marker。
2. `sudo systemctl stop xlerobot-s0-left-arm.service xlerobot-s0-right-arm.service`；
   现场确认物理断扭后才安装。`sudo install -d -o loosqk -g loosqk -m 0750
   /var/lib/robot/xlerobot_left /var/lib/robot/xlerobot_right`；目录已有则核权限。
3. `sudo install -m 0644 scripts/systemd/xlerobot-s0-left-arm.service
   /etc/systemd/system/xlerobot-s0-left-arm.service`（右同）；二进制必须先写同目录
   `.post-e-new`再rename到unit记录的绝对路径，不能cp覆盖正在执行的文件；核SHA。
   `sudo systemctl daemon-reload`。
4. 左bus断电重上电→USB拔插与枚举记录→以loosqk身份执行
   `s0_daemon rx-trust status --robot-id xlerobot_left --latch-path
   /var/lib/robot/xlerobot_left/estop_latch`；非canonical执行同参数revoke至0，再authorize。
   0/3=已授权，2=拒绝，4=撤销已安全但须规范化，87=并发禁止，1=系统错误；不能把3
   当“未发生”。左`sudo systemctl start xlerobot-s0-left-arm.service`，日志核消费→
   首Safety→Connect→Passed→READY Sent；GetState只读核layout/safety/Fault状态。
5. 左通过后右重复。不带扭矩动作证明“启动可用”，不执行SetEnabled自动试运动。
   任一Dirty/SSF/超时/锁冲突停在故障处，不自动authorize或反复restart恢复信任。

回滚单元为完整候选（二进制+unit+配置），**禁止仅撤guard/mailbox/marker代码**。
先stop新实例、现场确认断扭，再原子恢复备份的二进制/unit/config，核原SHA，
`sudo systemctl daemon-reload`；启动旧版前按候选E受监督流程确认，旧版不理解marker，
不能宣称“信任已恢复”。marker、estop latch都不删除、不从旧授权快照还原；重新启用
本候选必须重新D9物理边界+authorize。代码回滚在分支保留证据后整范围revert，不reset
工作区、不清用户修改。无现场授权/验证时上述命令只保留runbook。

## P.8 执行依赖、提交边界与收口

| 提交组 | 文件/工作包 | 依赖与退出门 | 风险/回滚 |
|---|---|---|---|
| C1 公共契约 | P.1全部、受enum/literal影响的调用者、FBS/映射 | 编解码/原子/legacy隔离G1+G2 | 可编译但daemon尚未切换；整组回退 |
| C2 adapter | P.2与factory服务贯穿 | C1；滴流/不可拆/首Safety安装/错误矩阵 | timeout语义扩散；不部署中间版 |
| C3 serial | P.3+首IO boot门类型、测试夹具 | C1/C2；phase/retire/ACK竞态全矩阵 | daemon暂不启用新入口前可独立测 |
| C4 IPC/启动总装 | P.4/P.5、全部spawn消费者、unit/runbook代码部分 | C1–C3；第一次完整可运行软件闭环，G1–G3 | 该组跨文件不可分开发布；整体候选回滚 |
| C5 独立验收/资产 | P.6剩余门、主agent文档/Archive | reviewer按AC复核，软件与现场分开判 | 未满足硬门保持未完成 |

每组任何逻辑偏离先报主agent Reverse Sync，先改Spec/Plan再代码；纯编译/格式修复可
自行修。任何组未跑fmt+fresh clippy不commit；不push、不部署由中间状态绕过整体
验收。`ManipularAdapter`等冻结trait保持0改；preempt构造服务新增接口是此Plan评审项。

round 1集中回修闭合项（本稿冻结待round 2，不带未定行为进入Execute）：

1. PreemptProbe的begin/finish/checkpoint分离、操作域/confirm可信上下文、hygiene错误
   独立原子事实槽获reviewer认可；安全判定之前先消费hygiene事实，每轮attempt只计一次。
2. 无seq超时已核正常可达；采用NormalAbortTarget tagged单词，具体编码、消费/清理及
   交错已在P.3写明。主agent先Reverse Sync Spec后，此分支方可实现。
3. 周期IO入口拒绝与在途不可拆已按D13收准；AC-1历史12/12指代已由Cargo --list核清，
   具名全集以baseline.json冻结；两ignored用例显式命令、RPC三个B4 override、freeze
   user unit完整命令均已列出。不可用systemd/Orin环境须真实登记，禁止删测试绕门。

以上无生产代码改动，无Cargo结果宣称。Plan通过后主agent更新Spec链接与审批记录，
Dev按C1→C5推进，reviewer每组独立审查，修复后再审，最终产出逐AC evidence与
`_human.md`/`_llm.md`，未满足的现场条目显式保留。
