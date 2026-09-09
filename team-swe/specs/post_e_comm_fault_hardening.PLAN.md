# post-E 通信故障硬化 · 函数级实施计划

> 日期：2026-09-09。版本：Plan Approved + S-40 至 S-46 Reverse Sync；S-47合入S-48。状态：**C1–C5、S-44与S-45软件已评审提交；左H1/H3 PASS，左H2两次FAIL保留，现场已确认左臂断电；S-46本地通过/Orin wire4满FAIL，S-48 A1文件事务本地及独立评审通过；右臂未测，D15 OPEN**。
> 权威：[`post_e_comm_fault_hardening.md`](post_e_comm_fault_hardening.md)（round 21 Spec Approved）。
> 代码基线：`bd081cb`；工作分支：`feat/post-e-comm-fault-hardening`。
> 本文只展开已批准 D1–D15 与 P.0–P.8，不修改冻结设计。主 agent / reviewer 明确
> `Plan Approved` 后实施；用户已授权团队内部评审通过后持续推进，无需再次请求流程确认。
> 历史批准稿 SHA-256：`3a8a394a74764ad2277bdd5f3aa0901627538f635d4b6208c55cde99d6f2dd30`。
> C1 `fc0c142`、C2 `822ab16`、C3 `beebfa0`、C4 `437b14f` 已独立 Approved 并提交。
> S-40 至 S-43 均经主 agent 与独立 reviewer ACK；当前文件新 hash 由主 agent 写入 HANDOFF。
> C4 包含生产总装、test hook、独立 IPC/marker/startup 验收；C5 工具与剩余验收单独提交。

## P.0 范围、所有权与当前事实

**当前分工更新（用户2026-09-09明确指定）**：root负责方案与评审，`cc-claude`负责
具体实现、全部测试与Cargo；旧开发agent已停止。A2执行委托见
`target/post_e_h2_rt/s48/a2/cc_claude_task.md`。下文旧分工由本条覆盖；root维护
文档与证据汇总，cc-claude冻结后交root评审，通过后由root提交，不push。
用户最新要求：最终独立评审由root或Codex子agent承担；Claude交叉检查是前置检查，
不能替代该最终评审。实现者不能自行批准自己的包。

唯一 Dev 负责生产代码、模块导出、Cargo 清单和模块单测；reviewer 只读独立评审，
给出独立验收场景与 PASS / FAIL，不能以 Dev 自测替代验收；主 agent 负责独立 integration
验收夹具、comm_fault_e2e 入口、Spec、HANDOFF、入口文档、CHANGELOG、10_verification、
Archive 与 `.verify` 汇总。同一文件
只由其 owner 修改。本文由 Dev 维护，评审反馈统一经 Dev 回修。

本次是软件交付：本机 MockClock、伪串口、临时目录、loopback RPC、隔离子进程与测试
systemd 服务可以执行；**不连接真实串口、不执行机器人上扭/断扭、不部署/启停生产
unit、不对真实 marker 授权**。Orin 的 AC-2(d)、AC-12、AC-13 留具名命令与证据格式，
缺现场实测只能记 NEEDS_HUMAN，不写 PASS。D15 硬件 fail-safe 仍 OPEN。
以上为最初执行边界；用户后续明确授权停止六项 Orin 开发服务与更新原仓库开展原路径
软件验收，事实与限制见 P.6.4–P.6.6。本地与 Orin 均已恢复并核验无 hook 正式产物，
六服务继续停止，`deployable=false`。用户已明确授权 Orin 调试尤其 RT 检查需要权限时
直接使用 `sudo`；记录实际权限和调度环境，不提高资源阈值或放宽既定预算。
用户现已确认可以现场配合 H1–H3；真实电机测量按每臂现场步骤完成信号推进，
P.6.10 修复与复核通过后才运行 H2。常驻服务仍停止，正式部署流程未执行。

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
enum PendingAdvance { Ready, InFlight, CancelledByEmergency }
fn advance_pending(ctx: &mut IpcTickContext<'_>) -> PendingAdvance;
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

**C4 S41 Reverse Sync（内部独立评审已 ACK）**：仅 `EmergencyBatch.estop` 为真时，
Step1 用 O(1) 长度记录当前 rr lane 的取消前缀，包含本 tick Step0 已 admission 请求。
后续每次 rr pop 都消耗一个前缀计数并占 rr24 配额（含 dry_run 与 expired）；第二道
deadline 优先，过期照常 DeadlineExceeded。未过期的 live 前缀 ClearEStop 返回
PreemptedByEmergency、live SetEnabled 返回 PreemptedByEStop，零 evaluator/副作用；
dry_run 正常只读评估。重复 EStop 刷新为当时完整 rr.len。未来新 admission、仍在
source/gRPC 的请求和 control lane 不在该取消域。SSF-only 不标前缀，尚排队 Disable
按当前 Fault 保持既有幂等 Disabled；既在飞的 PendingReplies 保持 emergency/SSF
返回优先级，both 时 EStop 优先。禁止紧急路径扫描/清空 rr lane 或无限同步回包。

`Guards::rt_serial` + try_transition Fault/EStop保原Fault + classifier给Emergency/
Disabled；HardRT EStop通配Safe/StopTorque不变。CommFault从Safe/Ready/Enabled进Fault，
commit_fault加severity保护。删除ClearError自授权；SerialRT每tick由boot Passed &&
rx_trust && abort_request==0 && inhibit==0推adapter_connected，adapter_fault_cleared还需confirmed &&
L0持续500ms。HardRT初值语义保持。

**C4 S42 Reverse Sync（内部独立评审已 ACK）**：IPC timeout 预提交 SerialRT Fault 时
立即清本地 adapter_connected/adapter_fault_cleared，不能让同 tick ClearError 使用
先前健康 guard。持续推导必须先 Acquire 读 abort_request，再读 inhibit；任一未清则
不可 ready。mailbox 在本 tick 开头 guard 刷新之后提交 Fault 也当场清这两个本地量，
避免 Step1b 与 control 之间的 ClearError 窗口。SerialRT 的 PreemptedByEmergency
完成返回类型化 CancelledByEmergency，只退 pending、不变成 AdapterErr/CommFault，也
不重复 EStop 原语；必须在 process_lanes 前取走原在飞 SetEnabled owner 并回复
PreemptedByEStop，不能让 tick 尾的取消标记误伤新 admission。HardRT 保旧映射。
SerialRT 正常错误先 route_normal 登记 fault/inhibit/mailbox，再 Release 发布完成；
IPC 看到错误完成时不能从旧健康 guard 恢复 Ready。Preempted 保持不登记 Fault。

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
    pub fn request_worker_stop(&self, worker: WorkerKind);
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
CP1=5s，CP2=T_abs+1s；ready_reached后自退。帮助/CLI出错由进程退出终止supervisor，
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

**C4 S43 Reverse Sync（内部独立评审已 ACK）**：`wait_startup` 每次先检测必需
worker 是否已退出，再允许物理/ACK 成功返回。HardRT 也在 READY 前检查 worker。
auto-enable 移至启动物理/ACK 完成后、真实 READY 发送前；`ExternalEvents::control`
返回 `Ok` 正常继续，`AdmitError::Full` 结构化记录并保持当前安全状态、不重试，
`Disconnected` 结构化记录并返回 1，进入尚未 READY 的既有有界清理。READY 后
主循环检测真正 IPC handle `is_finished()`，错误退出 1，Disabled 后按原顺序停止
全部 worker。watchdog 的 Disconnected 同样结构化记录并退出。正常顺序关闭逐个
调用 `StopFlags::request_worker_stop`，启动失败仍用 `request_spawned_stop`；观测位
仅在真正停止原子写入之后发布，不用测试指令本身冒充已清理。

### P.5.4 独立 test hook 与时间观测的实际接口

仅 `robot-s0/test-comm-fault-hook` 编译 `comm_fault_hook`、`comm_fault_adapter`、
`comm_fault_observation`，默认生产不包含。旧 `test-freeze-hook` 独立开关；
robot-tests 的 optional s0 依赖不再隐式带 freeze，仅 `freeze-systemd-smoke` 启用它。

```rust
#[repr(u8)]
pub enum HookCommandKind {
    PauseWorker = 1, RefuseStop = 2, SerialResult = 3, RequestWorkerStop = 4,
}
#[repr(C)] // 24 字节，同一公共定义供父/子进程编译，不复制偏移。
pub struct TestCommFaultCommand {
    pub sequence: u64, pub duration_ms: u32, pub count: u32,
    pub kind: u8, pub target: u8, pub error: u8, pub reserved: [u8; 5],
}
pub fn spawn_control_thread(robot_id: String, clock: Arc<dyn RobotClock>,
    observation: Option<Arc<ObservationPage>>, worker_stops: [Arc<AtomicBool>; 7])
    -> io::Result<CommFaultControlHandle>;
impl ObservationPage {
    pub fn create(path: &Path) -> io::Result<Self>;
    pub fn open(path: &Path) -> io::Result<Self>;
    pub fn snapshot(&self) -> ObservationSnapshot;
}
```

独立 raw POD topic 为 `/<robot_id>/test/comm_fault`，publisher/subscriber 各 1，
history 1、buffer 8、safe overflow 关闭。控制线程先消费 history，再发布 HookReady；
不属七 worker、不参与 liveness。每个成功应用命令最后 Release 发布 CommandApplied。
Pause/Refuse target 沿七 worker 0..6 编码，相对 duration_ms，0 解除；SerialResult
沿六 BusOpClass 编码，count 1..65535、通信错误 0..9/63、HardwareFault 64、
InvalidParameter 65、延迟后真正 inner 操作 66、Confirm 真实返回 some-on 67。
脚本用单 AtomicU64 保存次数/错误/延迟，CAS 消费不吞控制器的新覆盖。RequestWorkerStop
仅将指定预建 worker stop 原子置 true，其 duration/count/error 必须为 0；不伪造进程
退出。RefusedStopBitmap 只在 worker 真正观察到 stop 且正在拒停时置位。

`ROBOT_TEST_BOOT_PAUSE_STAGE` 仅在真实阶段到达后写一次 stderr
`comm_fault_hook stage=<stage>` 再冻结：before_logging、before_parse、before_resolve、
adapter_open、ipc_ready、after_spawn、before_ready、marker_write（真实文件 fsync 前）。
`ROBOT_TEST_BOOT_FAIL_WORKER` 在对应七 worker 的真实 spawn 前返回系统错误。
`ROBOT_TEST_IPC_EXIT_BEFORE_AUTO_ENABLE=1` 在真正发送 auto-enable 前请求 IPC 停止并
等待 handle finished，随后仍调用真实 control 得到 Disconnected；不替换 supervisor。

父端先 create 0600 的 4096 字节共享页并保持 mapping；daemon 仅 open 已有页，
校验长度/version，不 truncate，不在 RT 打开文件。mmap/install 必须在监督线程成功
spawn 之后；ObservationReady 仅在 OnceLock 安装成功后发布。Linux test-only
`.preinit_array` 只用 clock_gettime(MONOTONIC_RAW)+原子记录初始化边界；main 首句
时刻为结束，保守包络包含 runtime 过渡，不把 logging ctor 当全部 ctor。C5 对最终
产物 readelf/nm 确认其先于全部 `.init_array` 项。

ObservationSnapshot 单源字段：version、main_start_ns、spawned_ns、joint_count、
ready_ns、supervisor_exit_ns、fired_ns、fired_checkpoint、worker_bitmap、first_io_ns、
disabled_ns、stop_bitmap、preinit_ns、observation_ready、first_exit_ns、ready_reached、
command_applied、hook_ready、serial_calls[6]、refused_stop_bitmap、preflight_finished_ns。
值先写、完成 flag 后 Release；父端 Acquire flag 后读值。READY 先取 ready_at，再写
权威 shared.ready_reached Release，最后页 ready_at/ReadyReached。preflight_finished
记录真实 finish_boot(Passed/Failed)，在最终 boot_outcome Release 前发布；健康 Passed
不写 first_exit（首次带 gen 的出口），不得混淆两者。CommFaultAdapter 在调用者最早
构造，早于任何潜在 adapter IO，记录真实首 IO/六类调用，脚本仍走真实 convergence。

## P.6 独立验收映射与可执行命令

### P.6.1 测试文件、场景与拒绝伪覆盖

由主 agent（QA Owner）新增 crate 内 `src/post_e_acceptance.rs`、
`src/post_e_convergence_acceptance.rs`、`src/post_e_ipc_acceptance.rs`（cfg(test)，通过
真实 pub(crate) 接口验收，不为独立作者扩大生产公开面），以及 integration
`tests/comm_fault_startup.rs`、`tests/rx_trust_faults.rs`；Feetech
`tests/comm_fault_transport.rs` 与真实私有 helper 子模块；core `executor_ctrl.rs`
编码单测。确定性调度seam仅`cfg(test)`或`robot-s0/test-comm-fault-hook`，默认生产编译
无阻塞钩子。原子竞态seam位置必须按AC9(o/q/r)、mailbox①②③、ACK冻结精确放置，
不能用随机sleep“碰运气”。reviewer先给独立场景表，主 agent按接口写验收夹具，reviewer
独立检查反例确实能使错误实现失败。

C5 新增 `robot-tests` feature `comm-fault-e2e`（第10项单feature gate；不以未完成
工具阻塞 C4 的独立评审与提交）：
`["dep:robot-s0", "robot-s0/test-comm-fault-hook", "dep:robot-rpc", "robot-rpc/client",
"dep:robot-adapter-feetech", "dep:tokio", "dep:tonic", "dep:serde_json"]`。
现有robot-s0 optional依赖`default-features=false`，新feature还需显式
`robot-s0/mock`、`robot-s0/feetech`，不能依赖其它feature unification。
新增bin `comm_fault_e2e`、`required-features=["comm-fault-e2e"]`；support模块
`src/comm_fault/{mod,harness,scenarios,report}.rs`以bin的path模块引用，不新建外部crate。

CLI：`comm_fault_e2e --phase mock|startup|rpc|all|orin-characterize|orin-sla|orin-broadcast
--bin-dir <abs> --evidence-dir <abs> [--case <AC编号>]`。默认mock；all仅软件三phase。
Orin入口额外要求`--config <abs> --hardware-id <id> --ack-supervised-hardware`且构建
`comm-fault-e2e,real-hw`；现场执行按 P.0 当前授权。无ack/无匹配配置一律退2、零open。harness注入
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

C4 实际独立启动入口 `comm_fault_startup` 包含八个具名测试：
`initialization_and_all_seven_workers_gate_first_bus_io`、
`each_worker_spawn_failure_stops_only_the_successful_prefix`、
`main_pause_before_spawn_is_enforced_by_cp1`、`main_pause_after_spawn_is_enforced_by_cp2`、
`missing_required_notify_socket_never_commits_ready`、
`thirty_three_joints_reject_before_workers_or_bus_io`、
`auto_enable_disconnected_exits_before_ready`、
`ipc_worker_exit_after_ready_terminates_standalone_daemon`。后两项锁 AC11(ac)/S43；
正常时序验证 preflight_finished 而 first_exit==0；CP1/CP2 用真实 main_start 到 fired
测时，不以 parent spawn→wait 冒充硬截止或 ctor 预算。IPC 独立模块当前 26 组，
marker 独立 integration 当前 9 组，实际 Cargo --list 与日志为执行数量权威。

这些是全部AC到具名入口的映射；每个AC子条（a/b/o1等）在报告作为独立case id。
框架不允许遗漏case时AC仍PASS；NEEDS_HUMAN/未运行不会按success计入。

### P.6.1a C5 已批准测量接缝、实际工具与失败证据

Dev 拥有 `comm_fault_trace.rs`、原生产调用点和 `comm_fault/sla.rs`（H2）；主 agent
拥有其它 comm_fault 模块及独立 trace 环 oracle。生产默认 feature 不含这些观测，
不增加 wire schema、外部库或生产状态写入路径。本节由主 agent/reviewer 已 ACK 的
测量接口展开；真实 Orin 动作仍只登记命令，当前禁止执行。

- `TracePage::{create(&Path),open(&Path)} -> io::Result<Self>`，
  `drain(&self, limit:usize)->Vec<TraceEvent>`、`dropped()->u64`、`installed()->bool`、
  `armed_seq()->u64`。独立 mmap 环容量 8192；父端 create_new/0600/精确长度并初始化，
  S0 在真实 BootSupervisor spawn 后只 open 已有页。RT 不 open/mmap/分配。
- `TraceEvent { sample_seq:u64, stamp_ns:u64, kind:TraceKind, source:u64,
  normal_seq:u64, generation:u32, a:u64, b:u64 }`。8 个字段以 AtomicU64 写入，
  生产者最多 8 次 strong CAS 预留；字段 Relaxed 后槽 generation Release 提交。
  消费者以不等待的原子准入串行读，槽 Acquire 后读取全部字段，再 Release 归还槽。
  先预留但暂停的槽阻止越过读，不能拿后槽当作其内容。满/准入竞争耗尽/无效编码增加
  dropped；累计 dropped>0 使整次运行 invalid。counter 到界拒绝，不回绕复用。
- `TraceKind` 1–18：TopicReceived、ExternalEnter、PrimitiveCommitted、NormalAcquire、
  NormalRelease、SerialClaim、IpcClaim、SafetyStart、SafetyReturn、ConfirmReturn、
  AllOffTrusted、IpcEStopCommit、FirstCommError、MailboxPublished、FaultIpcCommit、
  InjectedDelay、MailboxPublishing、CommErrorObserved。原语 a=实际 strong CAS 失败次数，
  b 的 bit0=成功 CAS 前 ACTIVE、bit1=Merged、bit2=LatchOnly；acquire/release/两 claim
  的 a=实际失败次数、b=成功取得/原位曾置位。Safety a=真实 attempt、return b=Ok；
  Confirm a=真实 on 数、b=Ok；all-off 只在生产信任判定通过后发事件。
- Topic t0 是 `poll_estop.latest()` 真取得 lease 后立即取时；external t0 是真实
  `ExternalEvents::estop` 入口。t1 在权威 strong CAS 成功返回处立即取时。
  原语 CAS 后再读的 normal_seq 可能已被 serial 清零，不用它冒充线性化点的 seq。
  H2 用同 epoch 的真实 acquire/release 区间与 CAS 前 ACTIVE 验证归属；in-flight
  必须 Acquired≤t0<Released。缺区间、重复 acquire、t1→全关间新 normal、Merged、
  LatchOnly、零 Safety/Confirm、缺两消费者任一成功 claim 均不能成为有效样本。
  physical/inflight 计入有效样本前，还必须在 t0 所属完整 lease 内恰有一个真实
  InjectedDelay：epoch/gen/normal_seq 与该 lease 相同，a 为 1–5ms 请求时长，
  b 为 sleep 返回后的实际耗时且 b≥a，stamp 为 sleep 开始，完整 [stamp,stamp+b]
  位于 acquire/release 内。缺失、重复、身份错配、越界或时间溢出均 invalid；
  idle/software 不强制这项随机载荷，不能因此伪造 mock 的事务执行权。
- FirstCommError 来自 S0 实际收到/建立 `RobotError::Comm` 的 typed 边界或真实 hygiene
  `report_comm_error`；记录最初时刻/normal seq，待实际 publish 才绑定真实 generation，
  不预测下一代。已在 adapter 内恢复的重试不伪装成 S0 首 typed 错误。mailbox 权威
  Release 前后分别取时并记录同一代，GetState 从较早发布前界保守计算。IPC commit
  明确使用实际 ACK 的 snapshot.generation；ACK n 后并发发布 n+1，记录仍属于 n。
  SSF batch 的真实 Fault commit 同样发 FaultIpcCommit，不依赖普通 mailbox 路径。
- hook POD kind5 `ArmSample`：sequence 非零；target0 设测量 epoch，target1 解除；
  kind6 `ExternalEStop`：通过持有的真实 ExternalEvents 调原语一次；kind7
  `NormalDelayProfile`：duration_ms=min、count=max，1≤min≤max≤5 或 0/0 关闭。
  U[1,5]ms 随机附加载荷位于真实 normal acquire 成功后、第一 W/R 前，事务仍完整
  request+response；记录请求延迟与实测延迟，不宣称每个 W/R 独立 U 分布。
  Arm 仅写测量 epoch，绝不清权威位、故障、latch、policy。下一次 arm 必须上一轮
  真实物理收敛和两个 claim 已退场，并完成真实 RPC ClearEStop/SetEnabled 恢复。
- `ROBOT_TEST_INITIAL_COMMAND_SEQUENCE` 可选非零 u64；指定时子端唯有实际 apply 对应
  sequence 才能发布 HookReady 与 spawn ready。父端 publisher 持续 update_connections
  发送历史；没有匹配脚本则继续 drain，原 BootSupervisor 仍执法，不伪造 applied。

H2 `sla::run(&Args)->Result<Value,String>` 仅显式 orin-sla + real-hw + 现场确认运行。
先复用 D2 完整 arm/config/by-id 校验，核 udev 1a86:55d3/cdc_acm 与 Orin 型号；记录
current_exe、工具/S0/gateway 的 `.build.json` features 和实际 SHA256、配置与标定
SHA256、kernel/governor/L4T。真实实例锁与 marker 仅检查既有一次性授权，S0 自行
consume；工具不代写许可、不构造假 latch。S0 构建记录须含 test-comm-fault-hook 与
xlerobot-real，工具须含 comm-fault-e2e、real-hw。

四组 topic/external × idle/inflight 各 100 warmup + 1000 有效样本。每组真实 RR 干跑
目标 20/s，逐请求记录计划/实际开始/结束及完整 response；错误 response 或掉过一整个
50ms 调度槽则负载无效，不把缺负载当快速 PASS。独立 5ms 调度探针记录 σ_est 原始值。
每组保存 SLA-A/B 与原语/acquire/两 claim CAS 分布 P50/P99/max、全部 events.csv、
attempts.jsonl 和 background.json；任何失败都保留已 drain 的原始事件及 invalid 原因。
collect 不按 sample_seq 过滤已消费事件；等待完成期间任何一批混入外代即判 invalid，
该批全部原始事件仍写 CSV。独立两批采集反例锁住真实等待路径，不只调用 analyze。
分类失配保留并计 retry，不按时延挑样本；其它因果缺失直接失败。P99 A≤2ms、B idle≤20ms /
inflight≤75ms；fast-path 与全路径参考界分列并标明还需加调度延迟。无论测量成败，独立
请求真实 EStop、读取全关状态并幂等 Disable，再退出自己启动的进程；收尾失败也是失败。

C5 软件回归发现 C4 startup cooperative cleanup 把 join 轮询耗尽到 CP2，导致实际退出
晚约 1–4ms。修复只收紧内部 join 截止为 CP2−50ms，先验截止、只 join finished 句柄，
sleep≤min(5ms,剩余)；CP2/监督期限不延长，独立进程 oracle 仍严格原绝对退出上界。
覆盖 7 worker RefuseStop 与原合作/执法两分支，不把 11.95s 等超期合作当 PASS。

新增具名门：独立 `comm_fault_trace_acceptance` 四组（跨 mapping 两圈、暂停首 producer、
8193 精确 drop、双 consumer）；模块 `fault_trace_keeps_acked_generation_when_next_fault_is_published`
与 `ssf_commit_emits_its_actual_generation_before_next_mailbox` 通过真实 ACK 后 seam 发布 n+1，
截取真实 emit 验证 n 与 SSF 事件存在。独立采样 oracle 由主 agent 编写，不用 Dev 的分析
函数自算 expected。所有真机 H2 结果在本轮只能为 NEEDS_HUMAN，绝不由软件结果代填。

补充 AC-11(z) 两个真实单 tick 用例：
`real_enable_retirement_applies_lost_before_snapshot_under_four_source_flood` 与
`real_enable_under_four_source_flood_keeps_stop_disabled_and_retires_to_stopping`。
复用独立 ScriptedAdapter，回调仅延迟真实 `enable(true)` 返回并推进 200×5ms IPC tick，
正常完成由生产 SerialTick 发布，不手写 AdapterResp。每 tick 四源各 64 次 try_send，
锁 source 8 配额与在飞 lane/evaluator 零 drain；Stop 用真实 ExternalEvents.control
当场 Disabled。1s 边界完成后首快照为 Lost/Stopping，不出现 Active；两条已 admission
但过期的请求第二道 deadline 直接拒绝、零 evaluator；新 RR 全部在 1.105s+14tick
内回唯一结果，停止注入后实际排空。原 Enable 物理动作完整 Ok，未发 abort。

软件采样 `sla::software_probe(&Args)` 经主 agent ACK 接入 Mock/All 的
`runtime_idle_estop_sampling_and_recovery`（AC-12/13）。用真 mock S0/gateway，
topic/external 各两轮，共用采集器与 analyze；每轮验证真实 ClearEStop/SetEnabled
非 dry 完成回执及 GetState Enabled/Active/TorqueOn，再触发一次原语并等待真实全关。
记录实际 mock Safety 调用时间并核 <10ms；最终真实 EStop/Disabled 后清理进程。
现有 MockSerialRTAdapter 不执行总线 begin_transaction，所以仅验证空闲两入口，
显式 runtime_only=true、hardware_only=false、in_flight_exercised=false，不加假 lease，
不改 factory，不套 Orin P99。独立 `sla_acceptance.rs` 的 25 组完整/突变证据（原 21
加本轮 4 项）覆盖
四分类、代次、双入口、未知位、CAS旧ACTIVE、执行权配对、Safety attempt、Confirm
唯一性、故障标记、慢样本保留、实际随机载荷的边界/身份/完整区间，以及 collect
等待中混代仍完整保留证据；纯合成 oracle 不能作为实测数据。

单 feature 门暴露工具依赖消费域后，`comm-fault-e2e` 显式拥有 feetech 依赖，
`real-hw=["robot-adapter-feetech?/real-hw"]` 仅弱转发，不自动启用验收 binary。
纯协议记录、校验和独立 oracle 在无 real-hw 的 comm-fault-e2e 下可用，真实设备
环境核验/open/run 仍受 real-hw 限制。既有软件 runtime case 附加 `feetech_wire_check`：
实际 FeetechAdapter.safety_stop_fast→MeasuredTransport<MockTransport>，两枚非连续
电机 ID，构造期 timeout 与操作记录分开；Safety 恰一 write、零 read，完整广播数据
全零且无丢失/截断。它复用 H3 的记录器与协议校验，不打开设备、不增加 case、不改变
原软件结果的物理范围。新增具名
`software_protocol_entry_exercises_actual_feetech_encoder_without_a_device` 锁实际入口。

AC-17 使用被测 daemon 未覆盖的同源码默认配置：IPC tick=0.5ms、serial tick=5ms，
二者不能互换，也不宣称实际周期无抖动。三个独立验收界分别是：

| 观测 | 验收界 | 当前配置值 |
|---|---|---|
| 在飞 RPC 总超时退场 | 1s + 2×IPC tick | 1.001s |
| mailbox 发布前界到 GetState 可见返回 | 2×IPC tick + 23ms + 10ms 轮询 + 实测 RTT | 34ms + RTT |
| 首 typed 通信错误到 GetState 可见返回 | 上项 + serial tick + 105ms 在途 IO 界 | 144ms + RTT |

RTT 必须等于同一次 GetState 的 response_ns−request_ns，不能用任意填值扩大预算；
mailbox 使用同 gen 的真实发布前界保守计时，保留发布后界与实际 IPC commit。
独立 `rpc_acceptance.rs` 四项具名反例为
`rpc_retirement_uses_two_half_millisecond_ipc_ticks_without_old_slack`、
`mailbox_bound_rejects_values_inside_the_old_serial_tick_based_limit`、
`first_error_bound_separately_adds_one_serial_tick_and_the_inflight_io_bound`、
`invented_rtt_cannot_expand_the_fault_visibility_budget`，覆盖精确边界、边界外 1ns、
旧宽松界内仍须拒绝及伪造 RTT，不以另一条预算失败代偿本条断言。

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

# G2库存：必须同时保留完整清单和实际ignored清单；只核保留，不冒充执行通过。
mkdir -p target/post_e_verify
CARGO_BUILD_WARNINGS=deny cargo +nightly test --workspace --all-features --tests -- --list > target/post_e_verify/c5_current_test_list.log 2>&1
CARGO_BUILD_WARNINGS=deny cargo +nightly test --workspace --all-features --tests -- --ignored --list > target/post_e_verify/c5_current_ignored_list.log 2>&1
uv run --no-project python scripts/check_post_e_inventory.py --baseline .verify/post_e_comm_fault_hardening/baseline.json --current-list target/post_e_verify/c5_current_test_list.log --current-ignored target/post_e_verify/c5_current_ignored_list.log --output target/post_e_verify/c5_inventory.json

# G2：纯类型/单测/确定性协议。
CARGO_BUILD_WARNINGS=deny cargo +nightly test --workspace --all-features --lib --bins
CARGO_BUILD_WARNINGS=deny cargo +nightly test -p robot-adapter-feetech --test comm_fault_transport
CARGO_BUILD_WARNINGS=deny cargo +nightly test -p robot-s0 --all-features --lib post_e_
CARGO_BUILD_WARNINGS=deny cargo +nightly test -p robot-s0 --features test-comm-fault-hook --test comm_fault_startup --test rx_trust_faults
CARGO_BUILD_WARNINGS=deny cargo +nightly test -p robot-transport --no-default-features --features mock
# 此测试仅在not(debug_assertions)下存在；debug零测试不得算覆盖。
CARGO_BUILD_WARNINGS=deny cargo +nightly test --release -p robot-s0 --all-features --lib rr_server_thread::tests::rr_server_handles_orphan_response_no_panic_release_only -- --exact --test-threads=1

# 单feature隔离（每项独立命令，不合并features）。
for feature_name in freeze-systemd-smoke clear-estop-e2e pubsub-cleanup-smoke scripted-backend-smoke ik-backend-smoke robotctl-smoke sd-watchdog-smoke real-hw supervised-demo comm-fault-e2e; do
  cargo +nightly check -p robot-tests --no-default-features --features "$feature_name" --all-targets || exit "$?"
done

# 两份hook ELF各自检查真实链接产物，不能拿debug报告代替release或只查源码属性。
CARGO_BUILD_WARNINGS=deny cargo +nightly build -p robot-s0 --features test-freeze-hook,test-comm-fault-hook --bin s0_daemon
uv run --no-project python scripts/check_post_e_elf_init.py --binary target/debug/s0_daemon --output target/post_e_verify/c5_elf_debug.json

# G3前先构建所有当前checkout的release产物。
CARGO_BUILD_WARNINGS=deny cargo +nightly build --release --workspace
CARGO_BUILD_WARNINGS=deny cargo +nightly build --release -p robot-s0 --features xlerobot-real --bin xlerobot_deploy --bin s0_daemon
CARGO_BUILD_WARNINGS=deny cargo +nightly build --release -p robot-s1 --features test-freeze-hook,test-fault-backend --bin s1_daemon
CARGO_BUILD_WARNINGS=deny cargo +nightly build --release -p robot-rpc --features test-freeze-hook --bin robot_rpc_gateway
CARGO_BUILD_WARNINGS=deny cargo +nightly build --release -p robot-tests --features pubsub-cleanup-smoke,scripted-backend-smoke,ik-backend-smoke,robotctl-smoke,clear-estop-e2e,sd-watchdog-smoke,freeze-systemd-smoke,supervised-demo,comm-fault-e2e
CARGO_BUILD_WARNINGS=deny cargo +nightly build --release -p robot-s0 --features test-freeze-hook,test-comm-fault-hook --bin s0_daemon
uv run --no-project python scripts/check_post_e_elf_init.py --binary target/release/s0_daemon --output target/post_e_verify/c5_elf_release.json

# G3：最后一条S0为隔离mock专用hook产物，必须在artifact记录中标记不可部署。
export B4_E2E_BIN_S0_DAEMON="$PWD/target/release/s0_daemon"
export B4_E2E_BIN_S1_DAEMON="$PWD/target/release/s1_daemon"
export B4_E2E_BIN_ROBOT_RPC_GATEWAY="$PWD/target/release/robot_rpc_gateway"
CARGO_BUILD_WARNINGS=deny cargo +nightly test --workspace --all-features -- --test-threads=1
./target/release/comm_fault_e2e --phase all --bin-dir "$PWD/target/release" --evidence-dir "$PWD/.verify/post-e"
# 八套旧smoke使用本轮构建后按SHA冻结的完整release目录，禁止缺产物时fallback。
POST_E_BIN="$PWD/target/post_e_verify/c5_release_bins"
POST_E_SMOKE="$(mktemp -d "$PWD/.verify/post-e/smoke.XXXXXX")"
"$POST_E_BIN/crash_restart_smoke" --bin-dir "$POST_E_BIN" --config "$PWD/configs/crash_restart_smoke.toml" --hardware-id smoke_arm --iterations 5 --robot-id-prefix post_e_crash --latch-dir "$POST_E_SMOKE/crash"
"$POST_E_BIN/clear_estop_e2e" --bin-dir "$POST_E_BIN" --config "$PWD/configs/clear_estop_e2e.toml" --case all --workdir "$POST_E_SMOKE/clear"
"$POST_E_BIN/pubsub_cleanup_smoke" --bin-dir "$POST_E_BIN" --config "$PWD/configs/crash_restart_smoke.toml" --hardware-id smoke_arm --joints 1 --phase all --iterations 1000 --metric-sample-every 100 --robot-id-prefix post_e_pubsub --workdir "$POST_E_SMOKE/pubsub" --output-csv "$POST_E_SMOKE/pubsub.csv"
"$POST_E_BIN/scripted_backend_smoke" --bin-dir "$POST_E_BIN" --config "$PWD/configs/scripted_backend_smoke.toml" --hardware-id smoke_arm --joints 1 --phase all --robot-id-prefix post_e_scripted --workdir "$POST_E_SMOKE/scripted"
"$POST_E_BIN/ik_backend_smoke" --bin-dir "$POST_E_BIN" --config "$PWD/configs/ik_backend_smoke.toml" --hardware-id ik_arm --joints 5 --phase all --robot-id-prefix post_e_ik --workdir "$POST_E_SMOKE/ik"
"$POST_E_BIN/robotctl_smoke" --bin-dir "$POST_E_BIN" --config "$PWD/configs/robotctl_smoke.toml" --hardware-id ik_arm --joints 5 --phase all --robot-id-prefix post_e_robotctl --workdir "$POST_E_SMOKE/robotctl"

# G4：systemd真实可用才运行；不可用登记NEEDS_HUMAN，不模拟PASS。
"$POST_E_BIN/sd_watchdog_smoke" --s1-bin "$POST_E_BIN/s1_daemon" --gateway-bin "$POST_E_BIN/robot_rpc_gateway" --s1-unit-template "$PWD/scripts/systemd/xlerobot-s1-freeze-test.service" --gateway-unit-template "$PWD/scripts/systemd/xlerobot-gateway-freeze-test.service" --roles-toml "$PWD/configs/rpc_roles.toml" --phase all --s1-workers tick,heartbeat_pub,rr_server:0,state_cache --gateway-workers rr_io:0,estop_pub,state_pool:1,tonic_probe --robot-id-prefix post_e_sdwd --unit-name-prefix post-e-sdwd --workdir-base "$POST_E_SMOKE/sd-watchdog"
"$POST_E_BIN/freeze_systemd_smoke" --daemon-bin "$POST_E_BIN/s0_daemon" --unit-template "$PWD/scripts/systemd/xlerobot-s0-freeze-test.service" --workdir-base "$POST_E_SMOKE/freeze" --unit-name-prefix post-e-freeze --robot-id-prefix post_e_freeze --iterations 5 --worker ipc

# schema、allocation、依赖门。
cargo +nightly run -p robot-msg --bin schema_lint
cargo +nightly test -p robot-msg --test compat
cargo +nightly test -p robot-s0 --features rt_guard --test rt_guard_soak -- --ignored --test-threads=1
cargo +nightly test --release -p robot-s0 --features rt_guard --test rt_guard_soak -- --ignored --test-threads=1
cargo +nightly test -p robot-transport --test iox2_sub_first_repro parent_pub_then_child_s1_open -- --ignored --exact --test-threads=1
cargo deny check
cargo +nightly build --workspace --all-targets --timings
```

八套旧 smoke 各有独立工作目录或 latch 目录；重新执行时使用新的运行目录，不能让
上一轮 latch/marker 残留改变初态。crash 使用真实 s1_daemon 默认路径，从上述配置
读关节，不传仅 mock_s1 路径接受的 --joints。freeze 自己生成最小 mock 配置；
sd_watchdog 按既有模板生成每 case 配置，不传工具未定义的 --config 参数。
执行驱动先清 inherited NOTIFY_SOCKET/INVOCATION_ID/WATCHDOG_USEC/WATCHDOG_PID
及全部 ROBOT_TEST_*，保留 XDG_RUNTIME_DIR/DBUS_SESSION_BUS_ADDRESS；测试工具再为
所属子进程安装具名 hook。G4 日志使用 `_SYSTEMD_USER_UNIT` 精确过滤临时 unit。
freeze_systemd_smoke自行在XDG_RUNTIME_DIR的user目录渲染隔离unit，主agent只运行该
测试unit，不能安装/重启生产服务。user systemd不可用时该门保持NEEDS_HUMAN。
`cargo deny`工具缺失若无法按既有工具链
安装则记录门未过。用户另行授权的依赖更新已独立提交 `0001858`；当前工具的
optional 直接依赖见 P.6.9。使用已锁定依赖，feature / 依赖变化仍附 timings 和 cargo tree。

当前软件验收结果统一见 P.6.5。release orphan_response 夹具使用生产要求的
bounded64，必须按上述 release exact 命令看到 1 项实际执行；库存、ELF 与进程
场景分别记录结果，不能用过滤门或条件编译下的零测试代替完整验收。

用户授权的依赖更新不免除两个既有 ignored 的独立运行；它们必须在新 lock 下按上列
命令重新实跑，库存保留、普通全量测试和依赖阶段的 PASS 均不能替代。
`rt_guard_soak`有crate级feature门与test级ignored门，debug/release两命令均显式带
`rt_guard`和`--ignored`，保持默认60秒，日志至少一条测试真实执行。另一既有ignored
`parent_pub_then_child_s1_open`按上列命令显式执行，消费此前构建的当前checkout
`target/release/s1_daemon`；若环境/历史夹具问题失败，记录FAIL并诊断，不删除测试或
将仍ignored计成PASS。RPC三个B4 override一律指向本轮已构建产物，避免release旧缓存
或debug fallback。独立`comm_fault_startup`也在G3完整workspace tests中执行；没有
feature导致0 tests的输出属于门未运行，不能判PASS。

软件全门后重新不带test-hook构建生产artifact，记录sha256、commit、features，避免
混合release目录的最后一份带注入代码。硬件H1/H2/H3按逐臂现场完成信号推进，分别使用
`--phase orin-characterize|orin-sla|orin-broadcast`，H2每场景100warmup/1000样本，
保存CSV/P50/P99/max/CAS重试分布/σ_est与Orin PX56环境；P99门不在本机推算。

### P.6.4 显式分配守卫与 Orin 开发环境

workspace 的 `assert_no_alloc={version="1.1",default-features=false}` 关闭上游
`disable_release`，保留 debug/release 的真实分配拦截；不启用 warn_debug/warn_release，
不为 daemon 新增全局 allocator，不扩大生产分配器范围。分配违规为 abort，不能
描述为可由 catch_unwind 恢复；RtArc 原有 panic 语义保持不变。

具名门 `rt_guard_allocation_in_real_scope_aborts_child` 重新执行当前测试 binary；
子进程设置 RLIMIT_CORE=0，进入真实 rt_entry 后以不分配的 stderr write 记录标记，
再由真实全局 allocator 分配 64 字节。父端有界等待，必须收到 SIGABRT 且进入标记
恰一次；普通非零退出、超时或只告警均失败。守卫漏过时只在 scope 外释放并 exit73。
该门不替代原一分钟 ignored soak，四条命令分别执行：

```bash
CARGO_BUILD_WARNINGS=deny cargo +nightly test --locked -p robot-s0 --features rt_guard --test rt_guard_soak rt_guard_allocation_in_real_scope_aborts_child -- --exact --test-threads=1 --nocapture
CARGO_BUILD_WARNINGS=deny cargo +nightly test --locked --release -p robot-s0 --features rt_guard --test rt_guard_soak rt_guard_allocation_in_real_scope_aborts_child -- --exact --test-threads=1 --nocapture
RT_GUARD_SOAK_SECS=60 CARGO_BUILD_WARNINGS=deny cargo +nightly test --locked -p robot-s0 --features rt_guard --test rt_guard_soak rt_guard_soak_no_alloc_no_arc_for_one_minute -- --ignored --exact --test-threads=1 --nocapture
RT_GUARD_SOAK_SECS=60 CARGO_BUILD_WARNINGS=deny cargo +nightly test --locked --release -p robot-s0 --features rt_guard --test rt_guard_soak rt_guard_soak_no_alloc_no_arc_for_one_minute -- --ignored --exact --test-threads=1 --nocapture
```

用户已授权更新 `loosqk@10.0.1.40` 原仓库并按 gateway → S1 → S0 停止六项开发服务：
`xlerobot-gateway-left.service`、`xlerobot-gateway-right.service`、
`xlerobot-s1-left.service`、`xlerobot-s1-right.service`、
`xlerobot-s0-left-arm.service`、`xlerobot-s0-right-arm.service`。
六项均已核验 inactive/dead、MainPID=0，并保持停止。Orin 调试 / RT 检查需要权限时
直接使用已获授权的 `sudo`，记录实际权限和调度环境；NOFILE=1024、1000 轮、2 秒
资源回收 grace 与完整 60 秒 soak 均不放宽。该授权不包括真实电机测量、自动启动
服务或正式部署；H1/H2/H3 仍 NEEDS_HUMAN，D15 OPEN。

### P.6.5 当前软件验收结果与证据入口

当前源码 `e6d2b30699379ee0e2222649efb4e79093978c612e0d6a67bc83dce9720d2598`
（308 输入）已获独立 **APPROVED_SOFTWARE_SCOPE**，汇总状态为
**SOFTWARE_ACCEPTED_HARDWARE_PENDING**，`deployable=false`。

本地与 Orin 均完成 **1737 PASS = 1528 lib/bins + 208 integration + 1 doc**、
**11 ignored = 2 integration + 9 doc**；两端 67 软件场景和八套 smoke 全 PASS。
既有 ignored 按 P.6.3/P.6.4 单独执行，不与全量计数相加；库存为 23 integration
targets / 210 cases / 2 ignored。A1 保持原 1000 轮与精确 Node 集合硬门；临时
systemd unit 和验证进程已清理。debug/release 真实分配 abort 与两轮完整 60 秒
soak 共四项 RT guard 均通过，fresh、单 feature、schema、deny、ELF 与构建门均通过。

本地与 Orin 已实证恢复四个无测试 hook 正式产物：S0 xlerobot-real 的
`xlerobot_deploy` / `s0_daemon`、默认 S1 `s1_daemon`、默认 gateway
`robot_rpc_gateway`。Orin 四个 ELF 为 AArch64；构建 manifest 保留实际
features/profile/source/commit/SHA，不把后续提交号反填成构建身份。六项原服务
保持停止，软件批准不等于部署或机器人使能。

当前结论与逐项索引仅维护于 [C5.json](../../.verify/post_e_comm_fault_hardening/C5.json)，
最新验收证据为 [C5_evidence.tar.gz](../../.verify/post_e_comm_fault_hardening/C5_evidence.tar.gz)。
证据包按仓库相对路径保存原始 run 与执行器；需要查看时先在仓库根解包：

```bash
tar -xzf .verify/post_e_comm_fault_hardening/C5_evidence.tar.gz -C .
```

日常验收命令仍以本节为准。核包后按授权清除散落的 `target/post_e_verify` 验收目录、
临时执行器与重复测试 bin；正式 `target/release` 产物和 Cargo 缓存保留。
旧轮次日志、旧源码快照、重复产物及历史归档按用户授权清除，不再承诺保留旧过程数据。
现场 H1/H2/H3 的命令、样本与判据仍按 P.6.1 执行，不能由软件结果代填。

### P.6.6 双臂夹具的首帧屏障

`robot-rpc/tests/common/dual_ctx.rs` 在 `wait_ready` 后、返回 ArmStack 前调用真实
GetState；任务 RPC READY 不代表独立 state cache 已收到首帧。RobotClock 的一次
5 秒总预算覆盖 RPC 与轮询，只重试本 hardware 精确 NoFrame/Unavailable；沿用
PairConfig 的 20ms 轮询并截到剩余预算。robot/hardware、配置派生 layout、关节
长度与有限位置必须合法，其它 RPC 错误或非法 payload 立即失败。合法首帧立即返回，
不加固定 settle，不放宽后续 strict GetState、运动漂移或 terminal SLA。

五项同一 helper 回归覆盖：NoFrame→合法首帧、非法 payload 立即拒绝、精确错误
分类、RPC/轮询共用总截止、挂起 RPC 被总截止截断。前四项使用可控 future/MockClock，
最后一项使用永不完成的 future 核实际 timeout，不能靠增加 wall-time 预算过门。

### P.6.7 Action 端口的 Node 生命周期

S1 `rr_server/handler.rs::make_events_publisher_factory` 与 gateway
`events_subscribe.rs::spawn_events_subscribe_thread` 不再永久泄漏每 Action 的 Node。
S1 使用 P.6.9 的私有 guard，gateway 的 Node / Subscriber 在线程 scope 内正常
回收。history、terminal grace、retry、QoS 和公共接口保持原约定，不扩展固定
daemon 生命周期保留点的重构范围。

回归必须使用真实调用点：S1 实际工厂创建 Publisher 后接收真实帧；gateway 调用
真实 spawn，核 Action/request 身份和时间戳，再 request_stop + join。两端均在
NOFILE=128、CORE=0 的隔离子进程中执行 256 轮，父端并发读双管道，30 秒总截止后
kill/wait。父 harness 每端计一项，子进程重复输出不重复累计；这两项不能替代
原 1000 轮 A1、八套 smoke 或完整进程验收。

```bash
CARGO_BUILD_WARNINGS=deny cargo +nightly test --locked -p robot-s1 --all-features --lib action_publisher_releases_node_under_fd_limit -- --nocapture --test-threads=1
CARGO_BUILD_WARNINGS=deny cargo +nightly test --locked -p robot-rpc --all-features --lib action_subscriber_releases_node_under_fd_limit -- --nocapture --test-threads=1
CARGO_BUILD_WARNINGS=deny cargo +nightly test --locked -p robot-s1 -p robot-rpc --all-features --lib
CARGO_BUILD_WARNINGS=deny cargo +nightly clippy --locked -p robot-s1 -p robot-rpc --all-features --all-targets -- -D warnings
```

### P.6.8 生命周期测试的继承 FD 隔离

两处 `cfg(test)` 子进程启动在 `pre_exec` 中使用原始 `SYS_close_range` 将 fd≥3
标记为 CLOEXEC，保留 spawn 错误管道直到 exec。ENOSYS/EINVAL 时按父
RLIMIT_NOFILE 硬上界的 checked 转换有限遍历 fcntl，保留原 flags 后追加 CLOEXEC。
pre_exec 仅做系统调用与栈运算，无分配、日志或锁；EBADF 跳过、EINTR 重试，其它
错误返回并使测试失败。非 CLOEXEC 哨兵必须在 child 入口为 EBADF，父 FD 和 flags
仍有效。NOFILE128、CORE0、256 轮、真实收发和回收判据保持不变。

```bash
CARGO_BUILD_WARNINGS=deny cargo +nightly test --locked -p robot-s1 -p robot-rpc --all-features --lib -- --test-threads=1 --nocapture
ROBOT_TEST_NODE_FORCE_FCNTL=1 CARGO_BUILD_WARNINGS=deny cargo +nightly test --locked -p robot-s1 --all-features --lib action_publisher_releases_node_under_fd_limit -- --nocapture --test-threads=1
ROBOT_TEST_NODE_FORCE_FCNTL=1 CARGO_BUILD_WARNINGS=deny cargo +nightly test --locked -p robot-rpc --all-features --lib action_subscriber_releases_node_under_fd_limit -- --nocapture --test-threads=1
```

`ROBOT_TEST_NODE_FORCE_FCNTL` 只由 cfg(test) 夹具读取，不进入 production hook 或
正式 daemon。串行门覆盖前序测试资源污染，哨兵也能在单跑时确定性验证 exec 隔离。

### P.6.9 Node 析构顺序与 smoke 精确目录硬门

iceoryx2 0.9.3 Publisher 的 Sender 可能在外层 port_tag 退场前释放最后一个
SharedNode，使 Node 目录删除遇到 ENOTEMPTY。S1 私有 `EventsPublisherGuard`
按 publisher 在前、Node 在后声明字段；线程持完整 guard，只借 publisher，禁止
解构造成逆序退场。公开 spawn 签名保持兼容，history/QoS/retry/grace 不变。
真实调用点回归读取 `/proc/self/fd` 的实际 Node 路径：存活期必须存在，完整 drop
后必须不存在，空目录也失败；隔离调用点的 FD=4 不代表完整 daemon 常态基线。

`pubsub_cleanup_smoke` 使用上游 `UniqueSystemId::from(u128).pid()` 解码十进制
Node ID，不复制位布局。扫描前后确认两个实际 Child 仍活着，baseline 每个 PID
非空；原 2 秒 grace 后，final 相对 baseline 的新增 ID 集合必须为空。同数量替换
与空目录均失败，扫描异常不能吞为零或 NA。失败先保存 Node/PID、日志与 CSV，
再显式 teardown 并退出 2。CSV 既有文件列保持兼容，末尾追加目录数；全局文件/目录
仅用于信息采样，不跟随符号链接，错误写 NA（其它进程删除造成的 NotFound 可忽略），
不能受其它 PID 影响硬门判定；验收观察期间不清全局残留。

工具只新增 optional 直接依赖 `iceoryx2-bb-posix =0.9.3`，由
`pubsub-cleanup-smoke` 开启；版本已在 lock 中，不升级版本，外部直接依赖为 28 项。
五项新增用例覆盖空目录与另一 PID 隔离、全局文件归零及等数量替换、根/单端基线
缺失及本 PID 文件/symlink 错误、全局目录统计不跟随 symlink、CSV 兼容及错误 NA。
原三项用例保留，无新增 ignored；八项局部门与独立评审均通过。

```bash
cargo +nightly fmt --all
CARGO_BUILD_WARNINGS=deny cargo +nightly test --locked -p robot-tests --features pubsub-cleanup-smoke --bin pubsub_cleanup_smoke
CARGO_BUILD_WARNINGS=deny cargo +nightly clippy --locked -p robot-tests --all-features --all-targets -- -D warnings
git diff --check
```

A1 仍按 P.6.3 完整参数执行原 1000 轮；单 feature、tree、timings、deny、全量 tests、
67 场景、八套 smoke、正式产物和 fresh 均由当前源码实跑，结果见 P.6.5。

### P.6.10 H2 的 PX-56 装配与使能前硬门（S-44）

范围仅 `comm_fault/sla.rs`（developer）与 `sla_acceptance.rs`（主 agent 独立
验收）；主 agent 维护 Spec / 本计划 / 当前交接。无新依赖、生产接口或架构语义变化。

1. `H2RtProfile::for_hardware_id` 固定左 CPU4、右 CPU5、FIFO70、普通 CPU0–3；
   未知硬件拒绝。仅 physical 路径的 `configure_physical_s0` 追加现有三个 CLI 参数，
   CPU 掩码在父进程预构造，exec 前仅用原始系统调用设置；不在 fork 后分配、日志或加锁。
2. `read_s0_threads`（可作为通用进程读取器）读取 `/proc/<pid>/task`、comm 与
   `sched_getscheduler` / `sched_getparam` / `sched_getaffinity`。线程集合读取前后
   必须一致且非空，任何读取/系统调用错误、线程消失均拒绝，不以缺失值冒充 OTHER。
3. `validate_s0_rt_snapshot` 要求恰一个 `s0_serial_io_th` 是 FIFO70、精确目标核；
   主线程必须存在，其余全部 OTHER0、精确 CPU0–3。工具启动时由 `taskset -c 0-3`
   继承普通掩码，gateway 沿用继承；首次使能前也核两进程所有线程为 OTHER0 / 0–3。
   工具不在 Tokio 线程已生成后仅修改主线程亲和性，不用整进程 chrt 升 RT。
4. `run_instance` 在 S0 boot_ready、gateway ready 后，首次 `measure` / `recover`
   / SetEnabled 前检查三进程并写 `rt_preflight.json`，含 profile、PID/TID/comm、
   实际 policy/priority/CPU、RobotClock 时间及错误。三进程各自错误全部收集后先写
   报告，再判通过/失败，不因某次读取失败跳过留证。检查失败不进入测量，但仍执行
   既有 `final_stop` 和有界进程退场。软件 probe 不走此硬门。调度快照仅证明检查
   当时环境，不声称连续监控或 PREEMPT_RT 内核。
5. 独立反例覆盖左右正确、未知身份、serial 缺失/重复/OTHER/错优先级/错核、主线程
   缺失、普通线程 FIFO/越界/窄掩码、读取失败与线程集合变化。参数装配区分左右和
   软件路径。另用无特权受控子进程实际读取 Linux 线程调度/亲和性，只在子进程选
   本机可用 CPU，测试父进程保持不变；合成快照不作为 Orin 实测证据。

本地按序执行 fmt → 默认 workspace Clippy → robot-tests 全 feature Clippy →
comm_fault_e2e 独立测试与构建；运行现有 mock SLA probe 验证软件路径，复用已核
S0/GW 输入但保留真实构建 manifest。Orin owner 独占 Cargo，隔离最终产物并核正式
四 bin 未变化。独立代码评审、必要门全绿后提交工具修复；实物 H2 仍独立留待现场。

S-45 左H2驱动已经用于第二次真实FAIL，`left_h2_current`运行目录已消费，不能复用。
当前转入P.6.12的软件修复与等效长负载；S-46独立评审提交并对齐两端后，才维护
新的具体现场驱动与输出目录。新物理边界和持续值守事实仍是下一次H2的执行前提；
右臂必须另行准备并审核入口，不能仅替换左臂驱动参数，也不同时跑双臂。
当前现场操作已结束，用户确认左臂断电。源代码、构建身份与原始FAIL见
[H.json](../../.verify/post_e_comm_fault_hardening/H.json)，后续执行约束见HANDOFF。

### P.6.11 H2 完整采样边界与 idle 夹具（S-45，软件验收已批准）

本包修复左 H2 首次运行的跨 epoch 孤立 release，并显式建立 idle 前置条件。
developer 负责 `comm_fault_trace.rs`、`comm_fault_hook.rs`、`serial_tick.rs`、
`comm_fault/sla.rs`、`comm_fault/rpc.rs` 及必要私有接线；主 agent 负责独立 acceptance、文档与本地
Cargo；reviewer 独立评审，Orin owner 独占远端 Cargo。无依赖、生产协议或调度变更。

1. `comm_fault_trace`：测试 TracePage 控制头升级 VERSION，TraceEvent 布局不变。
   `request_boundary(seq, mode, now)` 支持 ArmInflight/ArmIdle/Disarm，拒绝零、
   超过 61 位、重复/回退 seq 及未完成请求覆盖。请求单词打包 seq+mode，control
   单写；CAS 占位 Writing 成功后才写固定 `now+1s` 截止并发布请求。请求、回执及
   ready/deadline 等边界字段统一 SeqCst，保证 ARM 上双读代次与多字段不会混读。
   serial 消费已发布请求，真实 ACTIVE 未清时不得落实 epoch，不主动清权威执行权。
2. `serial_service_boundary(ctrl, now)` 在 serial 的真实事务边界处理请求并独占
   epoch/回执/gate。回执先 Applying(seq)，写 ready_ns/绝对截止，再发布
   Ready/Disarmed/Failed(seq)；读取者前后核同一终态单词，禁止混代。disarm 使用
   独立新 request_seq 回执，epoch=0 不充当回执。idle 截止固定 ready_ns+1s；
   超时留下不能被 disarm 清除的 sticky failure(seq,cause)，撤 gate、保留 epoch。
   回执必须能核序号、模式、ready_ns、截止与状态；拒绝/解除/到期事实均留证。
   disarm 只允许替换已完成 Ready/Failed/Disarmed；Writing/Pending/Applying
   不覆盖。新 arm 不得覆盖仍 armed 的 Ready，也不得跨 sticky FAIL 继续测量。
   expiry/撤门每 tick 独立办理，不因新请求处于 Writing/Pending 或 Fault 跳过。
3. `SerialTick::tick_impl` 在 begin_tick 后无条件执行非阻塞 boundary service，
   此处不提前 return；Fault/EStop/SSF 也能推进 disarm。已有 claim_estop、
   retire_timeout、phase.step、inhibit 处理保持优先。gate 仅在这些安全处理之后
   阻新的正常动作与 periodic；真实 SERIAL_ESTOP_PENDING/inhibit/StopCommanded
   先于 claim 清位被观察后可撤 gate，t0 本身不撤 gate。不得阻 Safety/Confirm、
   AwaitAck、停止或超时退休，不改原语调用点，不为采样改生产绝对 tick 调度。
4. `consume_normal` 在 gate 有效时只保留可执行正常请求；cancelled、abort 或
   非法词仍走既有完成/故障路径。用观察值 CAS 取得应退休的 pending，避免判断后
   无条件 swap 错取新正常请求；无 hook 路径保留原行为。有效普通请求最长只等待
   固定 idle 窗口，不靠无限搁置制造空闲。serial 边界服务不做分配、锁或文件 IO。
5. control `ArmSample` 改为请求提交，CommandApplied 仅表示 control 已处理。预期拒绝与
   超时记结构化失败，不以 `?` 退出 control 线程，确保最终 ExternalEStop 通道可用。
   `rpc::inflight_fault` 的既有采样调用者同样改用此握手并在最终 drain 前完成 disarm；
   三种 Fault 原验收界不变。tool 接纳同 seq/mode 的及时 Ready/Disarmed；idle 还要
   当前 gate 有效且未过 idle 截止。已观测 Pending 的固定1秒到期即 FAIL，3秒仅为
   未送达/无快照的外层上限；及时完成的 Disarmed 不因工具晚读误判超时。tool 专用
   请求 helper 等待匹配 seq/mode 的真实边界回执；idle/inflight 显式选模式。
   同一轮询先 Acquire 读 CommandApplied，再读一致边界快照；只用本序号确认字
   之后取得的快照判断真实拒绝。独立回归在首次读取后发布真实 TracePage 请求和
   Release 确认，旧读取顺序须确定性失败；修复后等待 Pending，真实拒绝仍失败。
   collect 完成后请求边界 disarm，再 final drain 保存尾部；任何超时/失配/sticky
   failure 保留原始数据并 FAIL。最终独立 EStop 收尾不依赖 disarm 成功，不重授权。
6. 先从真实 acquire/arm/release 接缝跑出确定性 RED，再验证修复。永久独立回归
   用真实 TracePage 双映射、隔离子进程与 ctrl/probe/tick 调用点，不能用固定 epoch
   的 capture_emitted 隐藏接缝。覆盖旧 lease 中途请求、Ready 后立即 EStop、
   陈旧/重复/覆盖请求、到期不续期、Fault disarm、cancel/abort/非法词绕 gate、
   t0→t1 与新请求交错；现有 strict analyzer 拒绝反例继续有效。完成后删除当前
   读取临时真机 CSV 的诊断测试，不提交机器特定测试路径。
7. 四组合软件验收补充已独立 ACK：`CommFaultAdapter` 仅在实际 adapter 身份为
   `mock_serial_rt` 且测试 hook 启用时，用私有 RAII guard 围住真实 mock 普通
   方法调用及错误返回，使用既有 probe begin/finish；begin 已注入延迟，不重复调用。
   拒绝 acquire 时零 inner 调用、零伪 release，保留 AbortReason。Safety/Confirm/
   Connect 不取 normal lease；Feetech 路径不增加第二次 acquire，不扩大其私有 API。
   不以是否已 arm 选择 lease，以覆盖 arm 前真实事务边界。四组各至少2个分类匹配
   的完整样本，经真实 RPC 恢复和独立收尾；报告 mock lease/inflight，不冒充 UART
   或物理断扭/P99。独立反例核在途 EStop、失败归还、EStop拒入及非mock零重复lease。

按序运行 fmt → 默认 workspace Clippy → S0/robot-tests 全 feature Clippy →
相关单元/集成测试 → 三个实际 Cargo JSON 构建 → 真实 mock 两入口 idle/inflight
采集/恢复/收尾。必要 RT allocation 与原语/退休回归不得省。两端必要门与独立评审
通过后提交；不以本包 mock 判定 Orin P99。新真机 H2 需要新的物理边界完成信号与
D9 一次授权，失败证据保留在当前唯一 H 包中，不覆盖为 PASS。

### P.6.12 H2 背景负载、缓存工作集与分项收尾（S-46，Plan Approved，实施中）

真实依据：`ce61623`/`c07a9886…514b1` 左H2第二次运行，topic/idle为100预热+
138正式、零失配，随后缓存恰4096项耗尽（3620背景干跑+476真实恢复）。3621项
背景中11项启动迟到至少50ms；前导RPC201.565/275.143/172.031ms，S0完成前占
201.088/274.283/171.115ms，完成到client接收不足1ms。故先前的串行await无法
保持固定20/s；不归因未证实的CPU/磁盘因素。原始目录为
`target/post_e_h2_rt/orin/left_h2_current`，含逐SHA索引、静态trace/observation、
全部请求时间戳及现场断电补充。GetState已确认state3，后续Disable容量拒绝，
最终收尾整体FAIL；原始记录不覆盖，现场确认已消费，不重复执行旧驱动。

实现边界为`robot-tests`及必要文档/验收资产；真实handler/传输反例可在现有
`robot-rpc`测试模块补充`cfg(test)`测试，不改其生产逻辑。不得改生产RPC、默认4096/TTL300、
干跑幂等合同、transport QoS、S0调度、原H2四组样本/载荷/P99/RT门。

1. **入口与容量装配**：`comm_fault/mod.rs`新增`Phase::MockH2Load`，CLI
   `--phase mock-h2-load`，不并入`all`和原67场景。仅该软件phase接受
   `--h2-software-cache-entries 4096|8192`，缺省8192；其余phase拒绝该参数，
   软件phase仍拒绝所有现场参数。`sla.rs`以私有`SlaRunKind`区分Physical、
   SoftwareProbe、SoftwareLoad；H2专用gateway装配复用`rpc::gateway_command`，
   只Physical/SoftwareLoad显式追加TTL300和容量。Physical固定8192，短probe及
   通用RPC入口保持原默认。有效CLI/TTL/容量/在途上限写入environment。
2. **固定节拍**：私有`sla_background.rs`承载共同负载运行器。纯
   `BackgroundSchedule::admit(now_ns,in_flight)`使用同一RobotClock域，早到
   不发；实际start满足`due <= start < due+50ms`且tracked<64才允许发起，
   checked推进原绝对due恰50ms。迟到、满槽、算术溢出锁存首错，不重置基准、
   跳槽或追赶欠账。64包括尚未开始和完成未收回的任务。
3. **真实请求与响应**：有界JoinSet将发起与响应等待分开；任务首次实际执行时
   再读start并校验50ms门，不能拿spawn时刻替代。每项全新metadata、真实
   `SetEnabled(dry_run=true,enabled=false)`；同Channel clone不绕RR。完整RPC
   从actual start起固定2000ms外层截止，保原请求/Endpoint2000ms与gateway
   grace500ms。每条记录sequence/request_id/due/start/finish、Ack或Status/
   timeout/join失败；Ack核身份、dry_run与非零完成戳。按发起sequence汇总，
   不把完成顺序当发起间隔；记录launched/completed/峰值及未完成清单。
4. **传输容量真实边界**：应用forward与S0pending各64；现有iceoryx2每client
   active request实际为4、PendingResponse Drop才释放。工具64不保证wire64。
   独立反例须占住真实4个wire槽并核第5个真实失败；若等效长跑触发此门，保留
   FAIL、另据实证Reverse Sync，不通过静默扩大QoS或延迟已释放wire的HTTP
   回复冒充成功。生成器的慢回复/乱序测试只证明其调度接缝，不证明wire容量。
5. **首错与收尾**：`BackgroundHealth::check`可见首错，负载发生迟发、响应
   错误/超时、join失败或满槽立即停新发。`recover`开始、600ms等待后发Clear前、
   Clear返回且发Enable前检查；已经发出的live RPC必须等待原合同结果，不能
   select/drop后让旧请求越过final_stop。下一轮及采集轮询也检查health；错误仍
   disarm→drain→保存事件。`finish`自stop起固定2500ms drain预算，不续期；
   到期列出未完成并FAIL，仅回收背景dry任务，live恢复请求不取消。5ms调度探针
   独立停止/join，保存原值及时钟起点以便对齐；任何错误后仍走独立final_stop。
6. **分项final_stop**：保ExternalEStop、Disarm、700ms等待、GetState确认state3、
   随后Disable的顺序。单独结构化保存get_state的真实状态或错误，以及disable的
   attempted/结果或未发原因。任一步失败整体FAIL；读state3不能掩盖Disable失败。
7. **无电机长负载**：新software_load用真实mock S0/GW/tool及相同运行器，
   ≥600ms恢复、真实Clear/Enable/GetState、真实ExternalEStop及全关确认循环。
   4096作为负面对照，首次真实容量拒绝即按原失败收尾并保留RED；8192至少360s
   且成功唯一写key累计>8192，最多600s，未满足即FAIL。窗口结束不再起新恢复，
   已发live请求先退休再收尾。明确无UART/无物理P99宣称。统计每条节拍、所有
   结果、真实恢复次数及唯一key集合/总数，以跨越容量证明TTL轮换，不能只看运行时长。
8. **容量界与必要门**：TTL300s从commit计；合规窗口保守背景6002+恢复1004+
   跨窗背景64+恢复1+最终Disable1=7072<8192。测试调用真实装配/调度接缝，并以
   真实handler锁满缓存时GetState仍读、新写拒、旧key重放/冲突原义及TTL释放。
   MockClock/可控response future锁早到/边界49999999与50000000ns、64满、溢出、
   首错sticky、不补发、201/275ms慢响应与乱序配对、错误后禁止下一Enable且保
   已发live结果；final_stop反例锁state3+Disable失败的分项证据。两端各运行
   4096 RED与8192长负载GREEN，必要工具单元/装配、原67软件场景及受影响门通过。
   同机Cargo串行；真机新H2必须在独立评审、fmt/Clippy、提交与两端对齐后，再收
   到新的物理重置及持续在场事实，生成新驱动/新输出目录，不能沿用已消费目录。

### P.6.13 RR worker 按真实可用槽发送（S-47，暂停，待并入S-48）

用户已明确基础控制高优先级、连续目标普通通道。本节只保留容量子方案，尚未
实施；原统一FIFO发送方案要服从P.6.14分类与保留容量设计，不能先落地再补优先级。

输入为S-46两端实际软件证据：本地五阶段通过；Orin原67通过但长测在真实wire4
满时拒绝第五请求，完整raw见`target/post_e_h2_rt/orin/s46/acceptance1/load4096`。
765–768的S0完成戳晚于拒绝约299ms，ClearEStop位于另一client，不占SetEnabled
四槽。先收后发只是正确准入的一部分，不能代替实际容量背压。旧FAIL始终保留。

1. **transport容量观察**：只改`iceoryx2/request_response.rs`的wrapper与
   `rr_client`/`rr_channel`构造，在已open/create的service上读取实际静态active
   上限并保存，提供只读getter；不改`build_rr_service`QoS或其它工厂合同。
   getter必须核非默认已存在service的实际值；原默认4直接send第五条仍失败。
2. **单轮运行接缝**：`robot-rpc/rr_io_thread.rs`提取必要私有单轮/回收辅助，
   closed→ready/error→有限forward准入；pending必须实际Drop后才计算余量。
   `in_flight.len()<client实际上限`才取原crossbeam队列，不添加VecDeque、pending
   临时槽或重试。每轮dequeue预算固定64，包含closed及send失败；每项前观察stop。
   所有成功send仅进入原in_flight，真实send/poll错误仍关闭oneshot并计原错误。
3. **取消与关闭**：新增独立queued取消/关闭计数，不混入已发handler_drops。
   queued已closed零send；保留inflight取消/乱序回复/收包错误的真实pending归还。
   stop后不发新queued，最多64次显式回收原生产队列并Drop剩余接收端；production
   tonic已退出无新入队，故queued+inflight退场计数可完整核。不得无限drain生产者。
   其它公共调用方的并发取消检查/send竞态不增加原子撤销承诺。
4. **截止与FIFO**：原wire bytes不重建、不改stamp/deadline，handler及tool预算
   不续期。RrForwardRequest携带构造wire时的同一deadline；六worker复用gateway
   注入的同一RobotClock。send前检查closed和`now > deadline`；过期经内部
   ExpiredBeforeSend类型映射DeadlineExceeded，零wire且不写成功缓存。
   Clear/Set handler与四task方法共同await接缝都要接通。等于截止保留原比较边界；
   不保证check/send原子。S1 Invoke旧映射InvalidArgument、Cancel/Status/ClearError
   无同等执行门的缺口单列，不能外推已进入wire后的零副作用。原FIFO只为待调整
   子方案，最终发送优先级由P.6.14规定；满槽closed项占位造成Full仍如实报告。
5. **永久回归**：在真实transport接缝保留四槽+第五拒绝；将原gateway第五拒绝
   测试保留为RED依据并改成四槽占用→第五留队→释放→一次送达GREEN。核原64
   队列第65Full、FIFO、queued/inflight取消、queued+inflight关闭、ready/poll
   error真实归还、非默认service容量getter及每轮64预算。必须有真实wire至少
   518ms占槽而固定20/s仍入原队列、释放后完整身份配对的集成反例，不能仅延迟HTTP。
   截止测试核六真handler携带值等于wire原deadline，2s过期而grace仍开时返回
   DeadlineExceeded且真实RR零收包；核相等/+1ns、cache不commit、同源MockClock。
   不新增依赖。六topic共用运行器
   装配和已有cancel/timeout/routing/shutdown回归都要通过。
6. **验证与收口**：同机Cargo串行，fmt→默认Clippy→transport/RPC/tool相关
   features Clippy→相关测试→实际三个CargoJSON产物→原67→4096预期RED→8192
   至少360s且unique>8192。两端源码一致，保留旧S-46本地通过和Orin失败；新输出
   目录、新run/build manifest绑定实际source/base。驱动原时间/首因/清理oracle
   不放宽。独立审核全部证据后commit，再准备新物理H2驱动；D15 OPEN不变。

### P.6.14 基础控制与普通目标优先级隔离（S-48，A1已提交，A2评审通过）

本节优先于P.6.13推进，用户要求已授权，不需要再次询问流程许可。按用户最新分工，
root负责函数级方案与独立评审，`cc-claude`负责具体开发和测试，root核验证据并提交。
各阶段完整协议经评审后才实施。现场最后确认左臂断电，纯软件阶段不打开串口。

1. **两个队列的端到端分类与容量**：保留强类型RPC/topic，主调度只使用两个有界
   工作队列，每轮先高再普通；保留既有原子安全notice与普通侧latest slot，不扩成
   通用多级调度框架。紧急安全独立；live Disable/Stop
   先于live Enable/Clear；dry-run属于普通诊断，JointCommand单独latest-only。
   检查gateway幂等准入、forward、实际wire槽、S0 RR入口、EventQueue每一层，
   给基础控制保留有界容量。分类与拒绝码、每类上限、同类FIFO及跨类抢占要在
   实施前锁定，不能把dry_run原20/s要求悄悄删除，也不能把所有请求直接堆大队列。
2. **唯一owner与优先调度**：改`event_ingress`与`ipc_runtime`调度接缝；安全入口
   先于普通目标解析，安全降低先于恢复。normal pending不能挡住Disable；其真实
   退休回执必须按身份消费，旧Enable不能迟到翻回ActiveForward。heartbeat期限与
   mailbox不因业务lane持续忙而失去推进。每轮工作量有界，不引入第二状态机写者。
3. **磁盘解耦协议先审**：`ipc_tick/clear_estop_action/estop_latch`拟接单disk writer，
   worker仅做有界磁盘事务，不能修改state/guards/executor。必须先落盘Clear事务
   标记/恢复协议与唯一线性化点：标记先于双0写入，未完成事务启动时保守恢复，
   新EStop/Disable与Clear提交由同一原子权限仲裁，迟到结果不得解锁。当前
   `clear_safety_latch`仅storefalse，不能沿用“先读epoch再store”的竞态接缝。
   不使用异常重启无条件额外锁存的运行标记代替事务恢复；保留原双文件失败语义。
   marker清理、Ack、超时、abort、writer退出及每个崩溃点必须经独立审查。
4. **线程与RT约束**：普通gateway/IPC/persist允许受控分配，但不设为serial FIFO。
   RT仍只操作预分配POD/原子与唯一总线执行器，不进入RPC、持久化或中间件。
   新worker同步接入READY、启动失败回滚、watchdog、CP2收尾和TID验证。
5. **确定性反例先于长测**：从真正publish/call之前记录入口时刻；压满普通目标与
   诊断的每层资源，持久化阶段用可控屏障停住至少518ms。释放屏障前观察到紧急
   安全和Disable推进；核抢占旧Enable、Clear前后新EStop、真实请求唯一回复、
   真实临时文件逐crash点恢复、固定截止和shutdown。旧AC-12取样后t0/P99不变，
   新发布前起点独立记录；禁止先解除屏障再开始计时。
6. **分阶段验收与提交**：依据完整评审后的代码图确定可独立提交的小阶段；每个
   阶段独立评审、fmt/Clippy与必要回归通过后commit。最终保留旧S-46失败证据，
   两端重跑原67、长负载、RT guard及新增优先级反例；软件关闭前不恢复物理H2。

#### P.6.14.1 A1 纯文件事务与恢复（Plan Approved，本地及独立评审通过）

重启恢复后的独立reviewer已核当前文档、loader与strict helper，三项旧阻断及
repair前完整record校验、canonical身份、单writer前提全部通过；root批准本阶段实施。

新增`crates/robot-s0/src/clear_estop_transaction.rs`，`lib.rs`只注册模块。本阶段
不接生产prepare_execution、IPC owner、旧Clear入口，不改线程数、依赖、CLI或
正常启动语义；不宣称双队列或优先级隔离已落地。复用现有blake3/tempfile。

文件层以非Clone的`ClearTxnStore`独占目标，变更接口取`&mut self`。身份记录绑定
robot_id/hardware_id/canonical_latch_path，事务含txn_id、expected_epoch、
original_latched，固定版本、完整性校验与有界读取。身份不匹配不得修别的目标。
marker采用固定112字节二进制：magic8、version u16 LE、保留6字节零、目标身份
blake3摘要32、非零txn_id u128 LE、expected_epoch u64 LE、original_latched一字节
0/1、保留7字节零、前80字节的blake3校验32；版本1，magic为`RBTCLRTX`。
身份摘要以固定域和各段长度绑定robot_id、hardware_id、canonical_latch_path原生
编码字节；摘要代替可变字符串入标记。最多读113字节，拒绝短记录、尾随、未知
版本、非零保留位或坏校验。txn唯一性由上层分配保证，文件层拒绝零txn及旧marker。
canonical路径按已存在父目录canonicalize后拼原文件名绑定，不依赖primary当前
存在或其inode；原子替换不改变身份。单writer/部署锁是调用方前提，非Clone本身
不提供跨实例或跨进程互斥，不新增第二套锁文件协议。

marker只接受普通文件；Linux读取采用`O_NOFOLLOW | O_NONBLOCK`并核fstat类型，
仅真实NotFound视为不存在。链接（包括悬空链接）、目录、FIFO等必须明确拒绝，
不得进入旧loader自愈；inspect、完整record复核与重同步入口遵守同一合同。
临时latch文件排他创建；撞上残留文件仅更换序号，最多64次，耗尽返回对应创建
阶段错误。不得覆盖残留，也不得将选名重试扩展为事务或磁盘失败重试。

- `inspect_pending`：只读核当前marker，返回完整记录或明确错误。合法可读不等于
  marker已durable，更不等于双0已准备；此结果不能直接构造PreparedClear。
- `prepare_clear`：`create_new`拒覆盖旧marker；写完整记录→file fsync→dir fsync
  成功后才strict backup0→primary0，返回带不可混淆身份的PreparedClear。
- `repair_prepared`：按上层明确给定的RepairTarget严格修复，保留marker；文件层
  在任何latch写入前先核当前marker完整record，不自行读取epoch推断最新权限。
  若为恢复失败事务另设从inspect记录取得repair
  令牌的入口，任何false写入之前必须重新核完整记录并strict同步marker文件和
  目录；不得把一次成功读取当作durability凭证。
  false按backup0→primary0，true按primary1→backup1，全部严格同步。
- PreparedClear/RepairedClear字段私有、非Clone，变更/清理凭证按值消费；凭证
  绑定完整record。`remove_prepared_marker`消费凭证，再次核当前完整record一致
  才unlink并strict dir fsync；旧凭证不得删除新txn。此API只执行文件操作，
  不代表Clear授权，高层持有Finalize许可是后续总装前提。
- unlink成功、目录fsync失败时必须返回对应阶段错误，当前命名空间中的marker
  可能已消失；不能承诺清理失败必保marker或重开必true。不自动重建/重试、
  不虚构原子删除；未来总装需在调用删除前已经持有有效Finalize许可。
- `recover_and_load`：先marker；有效pending strict双1→cleanup→true；坏/错identity/
  读错直接拒绝且不先自愈。无marker才调用原load，原0/1/缺失语义保持。

测试使用真实TempDir文件，在marker与两份latch的写入、文件fsync、rename和目录
fsync边界注入确定性中断并重新打开恢复。核无durable marker绝不写0、重复prepare
拒覆盖、校验/版本/身份/txn错配、有marker必保守恢复、repair各目标与失败保marker、
从可读记录恢复到false前重同步、令牌消费与旧令牌不能删新txn、unlink后目录
同步失败的实际不可见marker及明确错误阶段、严格操作顺序。只证明实际文件状态、进程中断恢复与fsync调用
顺序，不称断电实测。独立源码评审、必要测试及全workspace fmt/Clippy通过后，
只提交本阶段源文件与对应文档证据；S-46未关闭事项及旧失败不得顺带宣称完成。

本地实际门：`b61a1534…b6c20e`/313输入、base `ce61623`；模块28入口、公开API7、
旧latch24、compile-fail3全部通过，fmt/Clippy零警告。16处进程退出是实际子进程
中断，逐步骤故障hook位于成功操作之后；目录同步错误用例含目录open失败，
不称每种fsync errno或真实掉电已测试。first Clippy首红与second通过分开留证。

#### P.6.14.2 A2–A4 后续相依阶段（A2已批准，A3/A4待详细评审）

A1已提交`9fe20ea`。A2已通过独立Plan Approved及最后的同代次观测增量ACK，允许按以下
函数级方案实施；A3/A4仍待各自详细评审。

权限必须扩展原`serial safety_word`的CAS竞争域，在A3一次替换旧word、latch与
inhibit读写；独立旁置epoch不解决check-then-acquire竞态。OutputPolicy只作输出
选择，实际运动仍核同一权威域。A2先独立原语和测试，不接生产半套权威。
初始化分配不回绕domain身份，令牌绑定domain与权限epoch；只读snapshot或复制
的执行视图不能构造owner。begin才生成唯一私有、非Clone owner，阶段转换按值
消费并至多返回一个后继；失败必须还回owner。撤销只收紧权限、不清占用位，
新begin拒Busy；只有当前owner退休才释放占位，丢弃owner保持保守卡住。
须补全执行许可的一次性转换：旧Copy视图不得在后继事务或同次执行结束后再授权。

A2独立评审与验收最低矩阵：

| 反例 | 必须观察到的结果 |
|---|---|
| IO acquire→EStop / EStop→acquire | 同域CAS确定先后；后来的运动获取失败，已开始事务沿既有安全责任退场 |
| Clear各阶段遇EStop、Disable、Shutdown或每类inhibit | 旧恢复权限撤销，阶段占位保留，实际磁盘生命周期仍能匹配退休 |
| 新begin、旧owner、跨domain、跨epoch及复制执行视图 | 不能覆盖新Pending，不能重复执行或释放，不能从只读快照制造变更许可 |
| Enable硬件成功但IPC未commit时Disable到达 | 旧成功只能退休，不能提交ActiveForward或恢复运动 |
| epoch/domain耗尽 | 不回绕；已存在权威仍能收紧与退休，永不再授予恢复权限 |
| Disabled/Latch/Finalizing与遥测 | 保留健康读取；通信失信/BOOT/收敛原禁入条件不被绕过 |
| Safety/Confirm/必要恢复动作与占位 | 不因Clear磁盘等待或普通队列满丧失安全降低能力，不创造额外EStop |

CAS只能称lock-free，不称wait-free或固定重试次数；时延预算由A4真实竞争测量。
独立原语热路径不得alloc/free、Arc操作、日志格式化或文件/中间件调用，新增独立
测试须安装真实allocation guard，构造与线程夹具放在受测scope之外。

##### A2 函数级方案（已实现，本地门与root评审通过）

新增`crates/robot-s0/src/control_authority.rs`及模块测试，`lib.rs`只注册；`cc-claude`
实现并执行独立integration/alloc验收，root逐项审查源码与原始测试证据。无依赖/trait/wire变更，不改旧控制路径。本阶段允许
文件独立编译和验证，生产迁移必须等A3全部旧写者与读取点一起切换。

2026-09-09 A2验收：第六轮13道门全部通过，来源439af44e…248a8d3/317输入、
base 9fe20ea；模块48、compile-fail13、默认公开16、debug/release真实guard各21、
rt_guard-only18，A1四项回归28/7/24/3。root亲自完成源码评审、日志摘要/计数与
源码身份复算，证据见H.json的software.s48.a2及包内local/s48/a2。未接生产或访问Orin。

`ControlAuthority`封装一个`AtomicU64`及初始化时取得的只读domain_id，均不公开
原子或raw setter。bit0–7保留OutputSafetyState编码，8为serial EStop pending，
9为IPC EStop notice，10为NORMAL_IO_ACTIVE，11为Latch，12–14为Phase，15为
Disabled，16为Shutdown，17为Exhausted，18–23为既有六类inhibit，24–63为40位
permission_epoch。Phase为Idle(0)/ClearPreparing(1)/ClearFinalizing(2)/
EnableQueued(3)/EnableExecuting(4)/EnableSucceeded(5)/EnableFailed(6)，7非法。
三个占位概念由Phase派生，不再各存一个bool。权限epoch与动作seq/trace epoch
分别命名、分别验证，不复用语义。domain来自非RT构造时checked全局计数，不回绕；
初始epoch=1、Disabled=true、Phase=Idle，latch/safety/inhibits由构造参数给定。
domain耗尽构造失败。snapshot为只读Copy类型，不能构造任何owner或执行许可。

| API/阶段 | 同域CAS与所有权合同 |
|---|---|
| `estop/disable/shutdown/tighten_inhibit` | 递增权限epoch、收紧相应位，保留Phase/IO_ACTIVE；EStop置Latch+Disabled+双notice并沿原表收紧safety，Disable只置Disabled、不创造EStop，Shutdown不可逆 |
| `reserve_enable` | Disabled、Idle且恢复门允许时递增epoch，转Queued，生成唯一非Clone EnableOwner与只读Copy EnableStartView；已Enabled的高层幂等返回不再次reserve，底层Disabled=false拒绝NotDisabled；事务新代次避免旧view复活 |
| `start_enable(view)` | 核domain/epoch/恢复门，唯一Queued→Executing CAS返回非Clone EnableExecution；第二次start无论同view或复制view均失败 |
| `finish_enable(execution, actual_result)` | 消费执行令牌，在整个真实adapter动作结束、其IO已释放后转Succeeded/Failed；即使epoch已撤销也记录真实结果，但不能用旧成功把新Stop状态写成TorqueOn |
| `commit_enable(owner)` | 仅Succeeded、当前safety=TorqueOn且原epoch/恢复门仍有效，消费owner、清Disabled并回Idle、推进epoch；迟到成功不能覆盖新保护 |
| `retire_enable(owner)` | 仅Queued或真实终态可消费owner回Idle；Executing拒绝且返还owner。只清本事务占位、保留最新保护，并使旧view永不重新有效 |
| `reserve_clear` | Disabled、Idle、TorqueOffConfirmed、无阻断原因及有效权限时递增epoch、转Preparing并生成唯一ClearPendingOwner；A3还必须核唯一owner维护的Safe与业务guard |
| `commit_clear(owner)` | 同域核domain/epoch/守门，清Latch、Preparing→Finalizing并推进epoch，Disabled保持；返回唯一ClearFinalizingOwner，失败返原owner |
| `begin_repair(pending_owner)` | 生成内含唯一owner、目标及采样epoch的非Clone RepairPermit；不另返可与owner错配的decision。pre-commit保原latch，新EStop要求Latched，只有原false可用Original(false) |
| post-commit修复 | Finalizing且当前Latch=false时重新核同域后直接授权Finalize；不得用Original(true)回滚已提交Clear。后续EStop置Latch=true时只能进入Latched修复 |
| `authorize_repair_cleanup(permit)` | 文件修复实际完成后，同域核采样epoch未变才返回唯一CleanupPermit；不匹配返同owner的新一轮修复入口并保持占位/marker，不能拿旧修复结果清标记 |
| `authorize_finalize(owner)` | 双0已prepared且当前Latch=false时，同域CAS核当前保护状态后消费Finalizing owner并授予文件清理许可；后到Disable可以使运动仍被禁止，不要求回滚已提交Clear |
| `retire_clear_cleanup(permit)` | 仅真实marker清理及strict目录同步成功后消费许可、清自己的Phase，保留所有最新保护；结果分别记录logical_committed与superseded，不能把后到事件误报成Clear从未发生 |

所有owner/执行/修复/cleanup令牌字段私有、非Clone，无自引用生命周期；失败
返回输入owner或唯一合法后继，不能暗中丢失清理责任。Drop不解除任何位。
任何安全撤销都不清Phase；新begin必须Idle，因此旧owner存在期间不能有新事务。
只读view无退休API，保证消费旧owner不可能释放后继事务；跨domain先拒且不改位。
文件许可只是调用方下一步义务，A2不执行IO；A3必须以A1实际文件结果驱动后继。
许可后到unlink/fsync间的新EStop仍是“原语先行、持久化随后”的边界；清理失败
不得退休或成功Ack。Clear成功Ack须晚于strict清理，且保留已发生逻辑提交的事实。

Feetech `enable(true)`包含多个wire事务，EnableExecuting覆盖整个adapter动作。
各wire步骤从EnableExecution派生只读EnableExecutionView，经同域IO acquire核验；
一次wire release不等于整个Enable完成。A3 packed请求可以附POD StartView，但
不得从snapshot/seq自行制造执行许可，真实result/seq须在终态发布前准备好。

普通IO基础门为没有serial pending、没有inhibit且IO_ACTIVE为空；所有获取在
同一个权限word上CAS置IO_ACTIVE，消费NormalIoLease只释放该位并保留新保护。

| NormalIoIntent | 额外门与边界 |
|---|---|
| PeriodicRead | 不因Latch/Disabled/ClearPreparing/Finalizing拒绝；不得绕过基础通信/BOOT/收敛门 |
| Motion（周期写及模式控制） | 未Latch、Idle、未Disabled、未Shutdown/Exhausted；所有抑制原因仍为零 |
| EnableStep(view) | domain与该Enable epoch匹配、Executing，未Latch/Shutdown/Exhausted；Disabled本来应为true，不据此挡住已授权使能步骤 |
| NonEnergizingControl(闭合类型) | 闭合枚举仅Connect/Disconnect/Disable，保留基础门，不因Latch/Disabled/Clear阶段统一拒绝；没有未知NormalEffectful通配豁免 |

Safety/Confirm/启动Connect维持既有特殊操作域，不占normal业务lease，不受新恢复
抑制挡住安全降低。普通请求到intent的全部映射在A3按真实AdapterNormalReq逐项
评审；SetMode全部模式均走Motion门（不能把Stop名字当作已断扭），HoldCurrent
仍保留原Unsupported；Disconnect不主动断扭，仅报告Unknown。
当前只有Connect/Disconnect/Enable/SetMode/HoldCurrent，没有ClearError
总线action，不新增冻结trait变体。ClearError为owner上的Pure状态迁移，A3单独
核其实际guard与当前权威，不伪造串口命令；已有不支持动作不因此变成支持。

inhibit仅Boot/Converging/FaultPending可解除，CommFault/AdapterFault/StopFailed
沿本候选保持sticky。`capture_inhibit_release(reason)`产生非Clone单原因许可；
`try_release_inhibit(permit)`核domain/epoch及该位仍存在，只清该位并推进epoch，
保留其他原因/保护/Phase。A3先捕获许可，再由唯一实际owner复核boot终态、实际
收敛退休或mailbox当前generation的匹配ACK，最后用原许可epoch提交；不得先
检查旧义务再无条件捕获更新后的epoch。FaultPending发布/ACK与许可的跨字段顺序
在A3逐点证明，不能靠A2单个CAS概括为已原子。许可过期必须重走捕获与真实义务
复核，不能自动刷epoch重试。
Clear各阶段没有清inhibit权限，Shutdown/Exhausted不可清除。

安全状态没有接受任意`OutputSafetyState`的setter，只有以下有限入口。普通
观测令牌非Clone且按值消费，须在真实操作开始时采样domain/epoch/Phase/safety；
完成时四者都必须仍匹配，notice/IO位变化不要求相等。不能从snapshot
构造，过期结果不能重新采样当前epoch后复述，须新的实际操作产生新上下文。

| 入口 | 允许转换与撤销规则 |
|---|---|
| `tighten_safety(SafetyTightening)` | 枚举仅StopCommanded/StopFailed/AdapterUncertain；每次推进epoch、置Disabled并保留Phase/IO/notice。StopCommanded不覆盖Failed/Uncertain，Uncertain不覆盖Failed，Failed最保守；饱和仍可收紧 |
| `begin_safety_observation(SafetyOperation)` | 闭合操作仅ConfirmOff/Disable/Disconnect，真实操作开始时生成含Phase与safety的私有上下文；不提供任意状态参数 |
| `complete_safety_observation`：ConfirmOff/Disable | 实际可信成功、当前epoch匹配且非Exhausted；当前Unknown/On/StopCommanded/OffConfirmed→OffConfirmed，Failed/Uncertain拒绝放宽；Disable另须当前Disabled。成功不递增epoch |
| `complete_safety_observation`：Disconnect | 实际成功、当前epoch匹配且非Exhausted；置Disabled并递增epoch，撤销更早观测；普通四态→Unknown，Failed/Uncertain保持。断连不冒充断扭 |
| `finish_enable`发布On | 只有唯一Execution令牌的真实成功可发布；须原epoch/恢复门仍有效、非Exhausted，Failed/Uncertain不可放宽。撤销后只记动作Succeeded事实，不更新新保护状态 |

EnableExecuting期间的旧Confirm不得在同epoch的finish_enable改变Phase/On之后
写回Off；若Succeeded之后新的真实Confirm已观测Off，commit_enable也必须因当前
safety不是TorqueOn而拒绝旧使能提交。两种同代次交错均列为确定性回归。
普通失败或不可信结果走既有错误责任分流，并由typed收紧入口推进epoch；旧同代
Confirm不能覆盖新故障。EStop本身仍沿既有转换表，wire编码不变。
notice消费、IO获取/释放、ConfirmOff/Disable成功不推进权限epoch；Disconnect的
安全撤销例外已明确。NormalIoLease私有非Clone，唯一release出口消费lease，
没有raw清IO_ACTIVE的方法；任何撤销均保留该位，旧lease退休前不能产生新lease。
next epoch耗尽时置Exhausted+Disabled，不回绕；禁止新事务、权限提交或放宽，
仍允许安全收紧、notice消费、真实IO释放、执行结果记账及唯一owner退休。

饱和后新事件不能再递增epoch，故文件清理不能只比较epoch：修复许可还必须核
当前所需文件目标，pre-commit为`original_latched || current_latch`，post-commit
修复必须仍为current_latch=true且目标Latched。不匹配返同owner重修，false修复
后的新EStop即使仍为MAX也不能授权删除marker。Finalize始终核当前Latch=false，
只完成已提交Clear的文件收尾，不授运动权限。inhibit解除与一切普通/放宽观测在
Exhausted下拒绝；superseded至少包含`epoch改变 || Exhausted`，独立保留
logical_committed。MAX测试必须同时覆盖旧false修复、新EStop、旧确认和退休。

- A2 权限原语：同一原子域内capture/commit/release token，EStop/Disable撤销旧
  恢复权限，避免ABA且耗尽fail-closed；RT请求与执行器读同一权威门，先验证原语。
- A3 S0完整总装：两业务队列、单在途disk mailbox、事务生命周期与启动恢复一起
  接通；真实Enable退休、Disable抢占、worker READY/回滚/watchdog/CP2同步完成。
  不将半条异步Clear默认启用后再等待下一提交补恢复。
- A4 网关与总体验收：两类准入及保留容量、actual-wire背压、发送前typed截止；
  数值与拒绝策略另经二审钉死。新增独立优先级场景，原67默认集合不变，两端
  原长测、RT与新增反例全部通过后才准备下一轮物理H2。

#### P.6.14.3 A3a 共享权限类型归位（root Plan Approved，2026-09-09）

A2已提交`dee67fa`。实际wire的`PreemptProbe`定义在robot-core、由adapter实现/调用，
不能反向引用robot-s0中的NormalIoLease。A3使用强类型租约交接，避免RT锁、Box、
擦除类型后伪造令牌；因此先把纯权限原语归位到core。此包只调整依赖方向和公开路径，
不接生产权威；后续总装仍须一次切换全部旧读写者，不把本包称为双队列已完成。

- 移动control_authority本体、word和模块测试至robot-core；标准库原子与既有
  executor_ctrl类型依赖不增加runtime依赖。domain全局计数器只留一份。
- robot-core::lib注册模块；robot-s0::lib直接重新导出同一模块，不留复制实现。
  core文档反例改用robot_core路径，私有字段/非Clone等错误目标不变。
- 唯一依赖s0旧inhibit常量的位兼容测试移至s0::lib的cfg(test)模块，常量仍为
  pub(crate)，不为测试扩大公开API；其余模块测试随实现迁移。
  s0公开验收保留，用core/s0两个路径交叉传递令牌、交错构造核同一类型和唯一domain。
- core增加默认关闭的test-comm-fault-hook构造验收特性，s0同名特性显式转发；
  不加入default、rt_guard或任何机器人生产组合。不新增crate或改依赖版本。
- 全workspace fmt/Clippy、core全库单测与13项compile-fail、s0公开验收默认及
  debug/release真实guard、rt_guard-only、A1回归必须通过。核生产feature图不泄漏
  测试构造入口；涉及Cargo特性配置，执行workspace/all-targets的build --timings留证。
- root审查搬迁前后语义等价、单一计数器与feature边界，cc-claude实现和执行测试；
  通过即单独commit。该包不修改PreemptProbe、AdapterNormalReq、旧安全权威或运行流程。

完整A3仍需锁定双队列容量/准入、强类型执行上下文和Abort责任、唯一writer的
所有权/启动恢复/停止预算，以及既有snapshot并发访问修复；见A3勘察，不提前启用。

#### P.6.14.4 A3b 快照并发安全（root Plan Approved，2026-09-09）

A3a已提交fdac02c。总装勘察发现现snapshot的UnsafeCell载荷在读写线程间使用
非原子volatile操作，version重检无法消除数据竞争；!Clone写端的publish(&self)
也不能保证排他写。基于fdac02c原样模块的最小并发夹具已由Miri报告数据竞争，
实际证据见local/s48/a3b/cc/reproduce，最终摘要须独立核验。此包保持设计要求的
预分配单写多读seqlock，先修共享基础件，不把它包装成权限总装或H2修复已完成。

**接口与同步协议**：

- SnapshotValue为Copy + Send + Sync + 'static的安全编码契约，关联Words仅允许
  固定[u64; N]。SnapshotWords用私有sealed边界限制到该数组族，提供固定长度的
  零值/只读及可变slice视图。不能用任意AsRef实现或动态集合伪装固定大小。
- 底层仅AtomicU64版本和预先初始化的Box<[AtomicU64]>载荷，由Arc在非RT构造。
  不存UnsafeCell<T>，不用volatile/transmute/原始结构体字节，也不新增unsafe impl。
  泛型同步本体与RtSnapshot的字段codec分文件，最小Miri可直接编译实际同步本体。
- publish改为&mut self并返回Result<(), SnapshotPublishError>，写端不Clone。
  先在栈上encode，先核偶数版本checked_add(2)，再依次SeqCst存奇版本、所有载荷
  字、偶版本。所有载荷和版本读写均用SeqCst，初版不优化为混合弱序/手写fence。
  这给一致性检查一个共同全序；无版本回绕时，相同偶版本的两次读取之间不能
  夹入一次发布。编码在奇版本之前，奇区间只做固定数量原子存储。
- 版本初值0，最大可发布偶值u64::MAX-1；再发布返回VersionExhausted，完整保留
  上一偶版本和载荷。不得回绕或只停更却继续宣称发布成功。reader维持load()语义：
  栈上固定Words、读偶版本/全载荷/再版本，一致后decode，否则重试；只供非RT读。
  删除“读取最坏只等一个tick”和未经证据支持的60ns等断言，线程被调度暂停时
  不承诺reader墙钟上界。codec需满足decode(encode(value))保持全部业务字段。
- codec为安全Rust；自定义codec违反可逆契约是程序错误，不得靠unsafe构造无效
  Rust枚举值。RT无分配结论只覆盖实际生产codec及经真实guard测试的路径。

**字段编码**（仅进程内实现，不改wire/FlatBuffers布局或业务枚举）：

- RtSnapshot在transport私有codec模块实现trait；每2个f32的to_bits打包为1个
  u64，顺序为positions/velocities/torques；随后state.stamp_ns、joint_count、
  tick_seq、tick_deadline_ns各一字。当前MAX_JOINTS=32共52字，保留全部浮点
  位模式、无效区数组及完整u8计数，不夹带标定/限幅/NaN过滤。字段与长度由
  MAX_JOINTS常量推导，奇数长度的尾字高半置0，避免读padding。
- S0StateSnapshot在s0的私有codec模块实现：3字。第0字state tag占0..2位，
  Enabled的sub占3..4、link占5、output占6..7，Fault code占8..10；未用位为0。
  tag按Safe/Ready/Enabled/Fault/Stopping显式对应0/1/2/3/4；其余枚举按当前
  源码声明顺序显式match编码与解码，不用as/transmute依赖Rust内部表示。
  第1字Guards的11个bool按字段声明顺序占0..10，command_mode占11..13、
  on_timeout_hint占14..15；第2字完整stamp_ns。Enabled三个字段互相独立，
  不借编码修正不匹配组合。字段codec保持业务值，不向core反向依赖s0。

**调用方与失败出口**：

- 全量检索所有snapshot publisher调用者，真实写者传&mut；测试及文档同步。
  修改范围为transport snapshot/codec/lib、s0 state codec、rt/serial/ipc及相关
  测试；core adapter仅修正错误的“Copy本身保证seqlock安全”与无证据性能注释。
  不顺带重构状态机、adapter、权限或双队列。
- RT/Serial/IPC的publish失败分别增加自身snapshot_publish_errors原子计数并
  结束该worker，让既有is_finished或进度停滞监督发现；不清latch或伪装Ready。
  HardRT通过rt_entry返回Result，在guard退出后才结束外层循环/析构拥有者；
  首次发布错误同样处理，不在RT中format、panic或drop Arc。不忽略任何Result。
  SerialTick已经完成的物理tick计数不回滚；失败后不能继续发布或执行下一tick。
- 明确核对已有监督入口与实际错误路径，不能把“worker退出”称为已物理断扭。
  独立评审已确认READY后主循环原先只检查IPC；standalone没有WATCHDOG_USEC时
  不启动systemd watchdog，BootSupervisor又已退出。因此本包同时在daemon的
  READY循环检查executor存在且未结束，缺失/结束走错误退出及既有收尾，不能
  只依赖systemd。增加无WATCHDOG_USEC/NOTIFY_SOCKET的真实mock进程回归，确认
  READY之后由版本耗尽触发executor结束、daemon非零退出；与IPC耗尽出口分开核。
  独立增量评审发现历史READY页与发现退出的日志不能证明实际耗尽时序；两个
  回归均须在真实publish调用前使用与ready_ns同域的RobotClock取attempt_started_ns，
  仅该次返回VersionExhausted时原子记录，非RT侧导出并断言
  ready_ns < failure_attempt_started_ns。仅test feature启用此观测，不在RT日志或
  分配，也不能在失败后才取时间（暂停跨READY会误证）。wait_ready成功前核截止
  与进程存活；这些检查不能代替真实调用前事件顺序。保留正常exit 1与无watchdog
  /notify断言，失败及源码变化后的门轮次分别记录。
  时间导出须在实际对应worker join完成之后读取其metrics；is_finished本身不提供
  这些Relaxed字段的同步保证。保留测试用metrics引用到非RT收尾之后再导出，不能
  只在join前读一次零值就跳过，也不能将一次Release/Acquire读当作必见新值。
  版本耗尽注入构造仅cfg(test)或默认关闭的transport test-comm-fault-hook可见，
  验证偶数输入；s0同名feature显式转发，不加入任何生产组合。

**验收与提交**：

- 旧模块原样Miri红灯及&self经Arc多写可编译反例保留。修复后同一并发驱动仅
  按新API加mut/处理Result及Payload codec，不能改线程数/载荷/迭代/seed掩盖问题；
  3个seed都须通过且无UB。实际生产codec在workspace测试中另核，不冒充最小夹具
  已解释执行完整S0。常规多读压力保留，断言完整数据一致与writer完成后的最终值。
- 浮点逐位往返覆盖NaN/负零/无穷及各字段边界；state覆盖全部Fault、Enabled
  独立组合、control_mode/timeout及每个bool位，外加独立已知字值样例。MAX边界
  保留上一完整快照，真实worker失败退出/计数可观察。编译反例锁住非Clone写端、
  &self和共享Arc不能publish、非固定Words不能实现契约。
- cc-claude串行执行fmt、全workspace Clippy、相关guard/hook特性Clippy、完整
  workspace/all-targets测试、transport/S0相关文档反例、Miri、debug/release
  真实allocation guard（首发布/稳态/耗尽与codec路径）、既有RT guard soak及
  原67软件场景。已有ignored按既定专项命令运行，不能在汇总中静默当作PASS。
  核7个生产组合core及transport均无测试hook，阳性对照确认转发；配置变更跑
  workspace/all-targets build --timings。记录实际命令/计数/退出码，不预填总数。
- 根Cargo.lock和第三方依赖版本不变，不新增依赖。失败如实保留，偏离先Reverse
  Sync给root；代码冻结后由独立reviewer审查实现，root裁决阻断项并复核来源与
  原始门证据，再单独commit，不push；按用户要求委派细审，避免root重复读全量日志。
  只做本地软件，不SSH/串口/服务/daemon硬件实例。S46已有改动保持且不混入此提交。

#### P.6.14.5 A3c / W0 可重试的文件事务凭证（root Plan Approved，2026-09-09）

A3b已独立评审提交`d67752ded3fba7fd5de1ea31b83243b6f7cd8382`。唯一writer总装需要
文件步骤失败后保留实际取得的资源权利；旧A1错误只带stage，不能区分创建/删除已
发生而后置检查失败。本包先完成纯文件原语，不等待监督与容量方案，不接生产控制链。
方案来源为`local/s48/a3/cc_writer_plan_r2.md` §5及
`cc_writer_r2_resolutions.md` §A（SHA-256
`fd02ed2051fdf81205256a3f960d47f2e50c7d4dc961a5d9b00f31114ef6e0e2`）；
独立`independent_writer_w0_r2_review.md`已给出W0 Plan Approved。本节采纳其明确
解释；增补§B监督、W1–W4总装和priority提案不随本节批准。

**目录与凭证身份**：ClearTxnStore::open保留canonical父目录的活File、dev/ino及
原目标摘要，文件名为相对目录的名字。读写、rename、unlink、恢复、自愈、临时文件
Drop清理都相对于同一目录fd；目录同步只sync该fd，不重开原路径。父目录改名或
原路径被替换后仍操作原目录。单writer、目标锁与目录内无并发改名者仍是接线前提；
本包不安装目标锁，也不宣称抗敌对目录竞态。

CreatedMarker立即保留create_new取得的可读写活fd（0600、O_RDWR|O_CREAT|O_EXCL|
O_CLOEXEC），活fd承载marker身份，并携带record与目录身份。首次marker元数据读取
失败仍返同一Created，不允许先丢fd并隐式unlink再返NotOwned。所有可失败的身份
采样必须在目录sync成功前完成，使Durable提升本身不可失败；不能sync成功后因
fstat失败降回Created。全部凭证密封、不可Clone、按值
消费，绑定目标/目录/marker身份；PendingClear仅只读检查，不实现删除凭证。
discard_created先核目标和目录，再核活fd为普通文件、nlink=1、长度<=112；从偏移0
读取完整内容并确认EOF，必须为本次记录编码的前缀，再核相对路径仍指向同一inode。
空前缀本身不授权删除，尾随字节、换inode、跨目标、真实身份不符拒绝且不unlink。
临时fstat/read错误尚未证明不符，必须返还原凭证；不得按错误stage猜失权。

**函数和阶段合同**：公开open、prepare_clear、repair<C>、remove<C>、discard_created、
resync_removal、persist_latched、recover_and_load、inspect_pending，具体入参依增补
§A.6。prepare/repair/remove/discard/resync/persist显式接收StepGuard；恢复内部用
NoGuard。MarkerCredential仅允许PreparedClear、DurableMarker、RepairedClear。
以StageError记录stage和Before/Operation/After；GuardInterrupt独立于syscall EINTR，
不得被IO重试吞掉。保留已有成功Operation之后的crash hook语义。

- 凭证随真实Operation成功推进：create得到File立即Created，marker目录sync成功
  才Durable，unlink成功立即Unsynced。之后After失败仍返已经推进的凭证。
- PrepareError区分NotOwned/Created/Durable。NotOwned仅表示本次未取得所有权，
  不证明磁盘上无marker；AlreadyExists不得被当成本事务成功。
- RepairError<C>在Before中断、临时IO错误或latch部分写入后保留C；只有真实
  NotFound或确认身份/内容不符才失权。RemoveError<C>区分Retained(C)、
  Unsynced(UnsyncedRemoval)、Lost；unlink前失败与后置错误严格区分。
- UnsyncedRemoval绑定原目录及目标；resync只重试该目录同步，失败保留凭证，
  绝不再次unlink，不凭当前路径不存在宣布strict完成。跨目标拒绝且不成功。
- Created丢弃后若尚欠目录同步，调用方须保留原修复目标：同步成功后继续该目标
  的文件修复，不能把resync当作Clear退休。W0以纯文件驱动验证此续段；实际
  WriterToken、RepairPermit与IPC协调属于后续W3，本包不接入。
- persist_latched按primary1→backup1严格同步；clear仍先durable marker再backup0→
  primary0。recover_and_load保留原strict缺失/fallback与损坏marker语义，全部
  迁移到绑定fd算法。旧repair_prepared/remove_*同步迁移删除，不留绕过返证合同
  的旧入口；repair_pending仅恢复内部使用。

**实现边界与验收**：唯一开发者负责clear_estop_transaction模块、必要内部fd/错误
辅助与相关公开验收/文档反例；不改daemon、writer、IPC、观测页、权限、Cargo依赖
版本或S-46未提交文件。如确需扩大文件范围，先报root，不自行接通生产调用方。

保留并迁移A1顺序、strict恢复、特殊文件、损坏标记、目标身份、16个真实进程crash
检查点与不可复制/重复消费反例；新增逐阶段三相返证、临时读取错误返证、空/半写/
全等/非前缀/尾随/nlink/换inode/跨目标，目录改名和原路径替换后仍同步旧目录，
unlink后重复失败不重删、同凭证最终重试成功、Created→Unsynced→同步→继续修复。
目录rename不再冒充目录sync失败注入；失败点与已完成副作用必须有确定性证据。

cc-claude串行执行fmt、全workspace/all-targets Clippy及涉及的hook编译路径Clippy、
A1全部模块/公开/原latch回归与文档反例，实际进程crash检查点；按影响补充测试。
不为纯文件原语重复A3b全workspace长测/Miri/RT soak；若改依赖或触及RT则先报偏离。
每轮记录实际命令、退出码、具名计数、原始日志SHA、验证前后源码清单；失败轮保留，
不得预填PASS或覆盖旧A1/A3b证据。源码冻结后独立源码和证据评审，root验收通过即
单独commit，不push、不访问Orin。本阶段仍不宣称总装完成或H2已修复。

#### P.6.14.6 A3d / W1 启动锁与事务恢复（root Plan Approved，2026-09-09）

W1方向与命名/链接边界已由Codex独立评审批准，见
`local/s48/a3/independent_writer_w1_plan_review.md`及`root_w1_lock_contract.md`。
实施依赖W0最终源码/证据复审通过并提交；本节不授权与W0同时修改启动恢复依赖。
该前置已在`8624f27`完成；W1执行前置增量亦获独立批准，见
`local/s48/a3/independent_w1_execution_preflight_review.md`，现可按本节实施。
接口为`acquire_latch_lock(&Path) -> Result<LatchTargetLock, LatchLockError>`，由
`latch_lock.rs`统一完成名称/锁校验；`Held`退出89，IO及配置错误退出1，不伪称被占用。
锁所有者提供目录身份与绑定fd的数据对象检查，辅助函数对外暴露以实际调用最小化。
daemon用锁与W0的目录身份比对后恢复；不复制W0文件算法。不改现有RX信任87与
启动CP1/CP2退出88。独立函数级核验已通过，见
`local/s48/a3/independent_w1_function_plan_review.md`；W0验收提交前置现已满足。

**共享的名称校验**


以canonical父目录+OsStr原始字节解析主文件名，不依赖UTF-8。拒绝空名、`.`/`..`、
末尾为`.bak`/`.clear_pending`/`.lock`，以及符合以下完整临时文件后缀的名字：
`.tmp.<十进制pid>.<十进制seq>`、`.clear_tmp.<十进制pid>.<十进制seq>`。
这里两个数字段非空，只含ASCII数字，匹配从相应分隔符到basename结尾；不以是否
能解析进u64决定保留性（过长数字也保留）。所有派生文件均在同一父目录。
固定RX信任资源同时保留：拒绝primary名`rx_trust_marker`和`rx_trust`，其备份、
稳定锁与临时文件已由上述规则排除；避免latch数据或其锁与RX信任文件重合。
实际W0临时名在冻结后核对；如新增家族，统一验证器与证明/测试必须一起扩展。

派生primary、backup、marker、稳定锁及两种临时文件家族的名字由上述规则划成
互不相交集合；同一类的完整生成名相等只能来自相同primary（临时类另含pid/seq）。
这不仅防并发覆盖，也拒绝把旧backup/lock改作另一个合法primary。不同合法basename
在同目录仍可独立持锁，不改成整目录单例。原路径指向符号链接时遵循冻结W0目标
身份规则，不把canonical父目录以外的别名当成新的独立锁目标。

daemon/provision/guard使用同一验证器，在任何latch/锁文件写入之前拒绝不合法名字，
给明确InvalidInput/配置错误；不是Held。rx-trust维护子命令分流保持原行为，不取得
latch锁；不改变其既有写授权或Dirty操作。
禁止独立包装层二次取同一锁；provision共享入口持锁覆盖整个read/repair/fsync。

**稳定锁对象**

锁名`<primary>.lock`，永不rename/unlink。按绑定canonical目录fd相对openat，
O_RDWR|O_CREAT|O_NOFOLLOW|O_CLOEXEC|O_NONBLOCK，mode0600，不带TRUNC。
先fstat确认普通文件、nlink=1，再非阻塞flock(LOCK_EX|LOCK_NB)，仅EINTR重试；
EAGAIN/EWOULDBLOCK→Held，其余IO或身份错误fail-closed。只在成功持锁且身份核对
通过后才set_len/seek/write PID；失败竞争者不能改变持有者PID或任何latch字节。
活File为RAII所有者，不Clone；借鉴rx_trust的flock/EINTR/CLOEXEC辅助，不复制其
未验证类型就截断的打开流程。单writer/目录内无并发改名者前提不变。

daemon在rx-trust子命令分流后、run之前取得锁，活至process exit；provision整个
文件过程持锁，guard exec前释放后由daemon重新获取，竞争者至多一方成功。
W1恢复使用W0最终open→recover_and_load，成功值同步进入既有ctrl和ExecutionResources，
错误退出1且不得到daemon adapter_open；不把这一结论扩大成部署guard全链零串口。
启动完成后store可在普通线程结束生命周期；第八worker和writer所有权移交仍属W2。

增补：锁成功后、恢复/数据读取/自愈之前，通过绑定目录fd统一检查现存primary与
backup：不跟随链接，必须普通文件且nlink=1；仅真实NotFound进入原缺失规则，
其余IO错误fail-closed。daemon与provision共享这一入口，拒绝以不同basename/锁
访问同一数据对象的symlink/hardlink别名。损坏的普通文件内容仍按原strict fallback。
ClearTxnStore打开后、recover之前，还须核其directory_identity与锁绑定的目录身份
一致；父目录替换导致不一致则在数据副作用前失败，不能拿旧目录的锁写新目录。
锁与检查均使用绑定fd，不能校验时再按绝对路径重开目录。

**最小验收与提交**

同目录两个不同合法名字可并发持锁；同canonical目标/父目录别名互斥；不同trust
目录不能绕过latch锁。`.bak`/`.lock`/marker及两种临时名、RX信任名字在副作用前拒绝，
覆盖无对应A实例存在时的旧派生文件，避免只证明并发互斥。非UTF-8合法名保留。
锁为symlink/hardlink/FIFO/目录均拒绝且primary/backup原字节不变；覆盖primary1、
backup0的有效崩溃状态。合法锁EINTR、CLOEXEC、竞争者不改PID、进程退出后可重取。
现存primary/backup的symlink、跨不同名字的hardlink、特殊文件均拒绝且不自愈；
另核真实缺失/损坏普通文件fallback不变，以及锁后父目录替换时不写新目录。
启动恢复保留W1独立报告全部无marker/严格缺失/有效pending/损坏或身份错误/IO失败
分支，断言真实阶段与文件态。旧同步IPC和退出持久化仍受同一锁，不改其安全语义。
范围仅锁/共用验证器、daemon恢复、provision/guard及相关回归；不接监督、writer、
新authority或双队列。最终由Codex独立评审与必要fmt/Clippy/相关测试后才提交。

执行增补（已独立批准）：provision保留D2三分支与最终目录fsync：双缺失写1/1，
单缺失从合法现存副本补齐，双存在内容不动；有效pending留给daemon恢复。
共享锁/名称/对象检查不等于两入口对所有缺失状态结果相同，不能将daemon的严格
缺失策略替换成provision行为。原deploy_guard_kill与guard的真实执行次序必须回归。
两份`xlerobot-s0-{left,right}-arm.service`的`RestartPreventExitStatus`增为
`86 87 89`，精确列表由deploy_units验证；保留1/88及其他原有重启与看门狗语义。
不执行服务操作。部署CLI受`xlerobot-real`约束，须另跑该feature下
`cargo clippy --locked -p robot-s0 --bin xlerobot_deploy --features xlerobot-real -- -D warnings`，
并用无硬件夹具验证provision CLI的新89/1和旧业务错误映射。其余Cargo均使用
仓库rust-toolchain默认nightly；完整来源枚举包含新源码/测试/unit，不固定W0的331项。

必要清理增补：provision转入绑定目录fd后，允许删除`estop_latch.rs`已无调用者的
`save_single_strict`与`fsync_parent_dir_strict`；不改旧runtime写序、loader自愈、
`save_single_inner`或旧测试，不保留删除历史占位注释。独立范围核验见
`local/s48/a3d/w1_scope_cleanup_review.md`，root批准见`root_w1_scope_addendum.md`；
该范围批准不替代W1冻结源码终审。

#### P.6.14.7 后续总装的已批准合同（合同批准，函数级实施包另审）

生命周期r6 SHA`ea842921e007d433b53164136cd66e10a0e1fe79f4a39f8ec571ea1814f0caa3`
已获独立终审与root批准，见`local/s48/a3/independent_priority_lifecycle_r6_review.md`
与`root_priority_lifecycle_r6_approval.md`。监督r4
SHA`141fe78c9c795710edef4e28d3448b97519e747a22e806d5f991caa3077b7602`已获独立终审，
root采纳，见`independent_supervision_resolution_r4_review.md`。本节不授权并发修改
W1源码，不自动批准容量默认值、旧S46 oracle变更或全部生产接线。

生命周期必须继承唯一move许可与允许来源集合；Sending/Sent/Orphan/
RepliedUnretired/NoHandle间只转移责任，已证未投递/未发过期或真退休才释放一次。
未知与未退休不按TTL遗忘；Terminal从terminal_at起原TTL保留。旧写wire缺退休
证据保持Pending，新wire区分effect/outcome/retired。R-Retire只延后回复，不删除
live SetEnabled原判断顺序或业务期限；首个结果锁存至真退休，Clear特殊原合同保留。
成功dry-run、EStop弱Ack/history、S1注册/长任务分离、只读非RR与GetTaskStatus
真实RR均按r6逐方法表实现。RR精确static_config、无safe overflow与真实满队列
反例不能只用客户端cap证明；数值与配置边界另经容量方案评审。

监督新增政策为Bstep=2s、Bpickup=500ms、Bshutdown=10s、Q=10ms、M=250ms、
L=D−M。原CP1/CP2及其他预算不改；250ms测试迟到容限与生产截止分开记录。
单次CAS先武装D，再由赢家发布shutdown权限epoch；多次调用不续D或重复推进epoch。
阶段和实际进入时间同一AtomicU64，FileStep戳在WriterGuard::before首条语句发布，
测试屏障在其后；READY前已开始的阶段按max(实际开始,armed_at)+预算监督。
公告先于try_send，退出前复读所判候选；不把await retry退避编码成未来阶段开始。

EStopObligation的Pending/Active与连续非settled区间起点同字保存，整体settled
才归零；ClaimGuard在异常/提前退出时先恢复责任再发布Exited，完成CAS保留新Pending。
unsettled_since不等于单笔requested_at；request先赢则旧完成留下Pending(t1)，
完成先赢则归零后新请求从t2起。2s仅保守告警：历史sticky保留，当前health对应
责任settled后解除。持续有界IO失败不因总时长主动终止S0，不宣称整个事务有界退休。
Clear实际接受重投最多8次，跨Repair/Cleanup/Resync/目标变化不重置，之后Stuck
保留owner/permit/token/提交事实。未退休不发终态回复，RPC自身期限照常结束。

真实Lower ACK必须核原身份/seq退休、Shutdown+Disabled和TorqueOffConfirmed，
Stale先消费并退休旧seq再重新实际操作；RX不可信/Exhausted不能假ACK。
ACK成功后才停executor，Clear真退休且flush完成后再停IPC及独立S0 watchdog。
ACK失败只停r4表②线程，executor/IPC保留，S0 watchdog按收尾条件进residual。
helper接口保留真实join Result，任一Err不得clean。RunExit持残余句柄到进程退出，
D覆盖出口日志/导出等潜在阻塞点；A3b观测仅真实join后导出，不拿is_finished作同步。
writer的Exited由main及监督双发现；D未武装时按exited_at+Bpickup候选，已武装由D接管。
备用Store在启动时核与W1活锁/worker Store同目录，正常期不写；仅真实join后取
LastResortToken作一次persist_latched，保留新Pending，flushed按是否真正settled记录。
未join不能兜底；不能panic后按已替换路径重开。可收尾失败1与实际监督截止90分开
验收：有效writer阶段/推进候选和D竞争最早绝对截止，分别留CP3/CP4来源；武装D
不能压掉更早的FileStep/pickup候选，同刻按FileStep→pickup/推进→shutdown。
WriterExited仍按既定特例在D武装后由D接管；不得将其特例扩到其他writer候选。

Spec/CHANGELOG同步明确步骤有界、显式未完成、真正退休的区别；永久IO失败时
重启记忆风险仍存在，D15 OPEN不变。后续首包可建立未接生产的纯模块，接线包
必须整体保证EStop/Clear唯一writer与权限迁移，不能新旧写盘路径并发，也不能
先把worker位图/观测页改为8却仍只spawn7。各实施包冻结后仍需Codex独立终审。

#### P.6.14.8 W0.1 只读双份核验（本地独立验收通过，2026-09-09）

函数合同`local/s48/a3/root_w01_pair_contract.md`的SHA为
`7e51ec7e8bd57156e609b4210db71a8fc8502ee28934e2db5192d426ba0afe57`，
独立报告`independent_w01_pair_plan_review.md`的SHA为
`5a7b9465d724053c217c1acd5f09a35709d40a5b49c18e98185d252c02371d17`，
root采纳，见`root_w01_pair_approval.md`。实施依赖W1源码终审、质量门和提交。

仅新增`ClearTxnStore::inspect_pair`、`LatchCopyState`及`PairView`；
`InspectPair`追加现有阶段末尾，使用原Engine/StepContext三相守门。读取绑定目录fd，
每份以NOFOLLOW/NONBLOCK打开，同fd核普通文件/nlink=1后解析。仅open的ENOENT
为Missing，坏内容为Corrupt，真实IO/对象错误直接StageError，不返回部分PairView。
一个步骤内先guard再顺序读取两份，成功后才执行测试After；不承诺外部并发改名下
的原子快照。绝不写入、自愈、同步或读改marker，不产生或消费持久化凭证。

无生产调用方，不改W0旧恢复/写入算法及依赖。测试覆盖值/缺失/坏值、对象拒绝、
绑定目录替换、三相及无文件内容/条目副作用；fmt、workspace与hook Clippy、W0
模块/公开入口/旧latch/doc门通过，独立源码和证据评审后提交。完整执行边界以
上述单一函数合同为准；本节不批准W2a或后续总装激活。

执行结果：base `1b21457`，round2七门实际通过，335项来源前后一致、恰三文件变化。
独立源码/证据终审及root采纳见H包`local/s48/w01/review.md`；旧解析抽取等价。
起点为与W1真实结束清单相同的派生记录，非本包开发前采样；原轮与审计误判保留。
未接生产调用方，未运行Orin；此通过不替代W2a与生产总装验收。

#### P.6.14.9 W2a writer纯模块（本地独立验收通过）

函数方案`local/s48/a3/cc_w2_execution_plan_r4.md` SHA
`90769d166516c98fbd3158bc9028ff3ea784e409749cc912e4d68573082105a3`已由root
按独立B1–B5及后续具体反例终审，见`root_w2_execution_plan_r4_approval.md`。
只批准无生产调用方的obligation/progress/announce/protocol/core/pairing/supervision
模块及测试；真实W1锁和W0.1均须先冻结提交，实施前核最终公开接口。

Claim在Active时禁止重复领取、跨实例不能完成；连续未settled起点及新Pending
保留原合同。Created删除后目录同步失败保留原修复目标，重同步后继续真实修复。
配对需真实锁的目录与primary目标同时匹配worker/standby；真实join才产生对应凭证，
兜底token借用其standby、按值使用一次，写前拒绝不触文件，真实StageError原样保留。
公开不可表达的跨standby使用以compile_fail证明，不为测试暴露伪造凭证入口。

该包不接daemon、不spawn第八worker、不改生产观测版本或旧控制权威。W2b只准备
仍必要的独立机制，重复W2c删除；生产权限迁移、实际身份退休、ACK、READY监督、
关闭D/90、8个worker与观测、EStop/Clear/退出唯一磁盘writer在W3统一激活。
后续详细函数实现仍另审，不能把此依赖边界当作W3实施批准。

W2a最终round6在base `81c5c54`完成七门并通过独立源码终审与root证据核验：
344输入、10文件；模块41/公开19/doc10、workspace1877通过及1既有忽略。
R1–R7回修和round5测试缺口已闭合，完整限制见`local/s48/a3e/review.md`；
无生产调用方/Orin操作，下一包W2b按已批准合同实施。

#### P.6.14.10 容量策略与S46新口径（合同批准，函数实施另审）

`local/s48/a3/cc_capacity_policy_r3.md` SHA
`4bfb06cbdb985b3b4b798d106ed750fa0cce331b54b4c0fd38033cbad551f6f0`
经独立原五项/r2增量复核与root终审，具体权威规则见
`root_capacity_policy_r3_approval.md`，其拒绝分类及未知枚举澄清优先于候选例子。

默认应用显式L4/C8，打开后实际静态配置精确匹配，不自动使用主机默认或更大服务。
S0每client N2/Restore1/Lower1，两个控制topic各High16/Normal48；四个S1 topic
各Normal64、N=L。S0同时核可信peer与全局类/总份额，未退休责任占位不释放。
High每tick非阻塞处理，不保留retry整体早退；Normal队列按2·C(L−2)派生，默认32。

总cache4096/8192和TTL300不变，嵌套总len门的Emergency/Lower/Restore策略预留为
64/64/128；高类可用低类空位，预留不是类硬上限或负载测量保证。NoHandle减少
还能准入的新请求数量，不改变总len阈值。peer/队列容量拒绝为QueueFull，真正进入
业务仲裁后的事务忙才为AlreadyInProgress；均映射ResourceExhausted、domain分开。

对P.6.12第7/8条的后续S48实施显式采用O1 Reverse Sync：无并发/无GC可控夹具
锁精确3840/7936总len门；真实长负载只对Normal的cache Capacity核当次GC后总len
达到阈值。更早wire/queue错误按真实来源失败，不能计作缓存EXPECTED_RED；记录
总/类/stage占用及GC，Lower收尾单验实果。旧S46结论与文件保留原义，新语义落
新S48目录、source/base和H键，不重算旧结果。20/s、2s/500ms和wire4预算不变。

对应实施时显式追加SetEnabled容量domain并跑实际旧新schema兼容；未知数值不能
伪称自动变成Unknown变体，也不证明旧责任退休。当前只批准合同，不授权全部
函数接线或运行长测；永久IO/未知责任与D15边界均保持原口径。

#### P.6.14.11 W2b 关闭与join机制（已评审提交6defac4）

函数计划`local/s48/a3/cc_w2b_shutdown_mechanism_plan_r3.md` SHA
`131e1b0900783046174a9ed87e75491b64c81efc3c6685f4dd58f4b7a01d3835`已独立
Plan Approved并获root采纳，见`independent_w2b_r3_review.md`及`root_w2b_r3_approval.md`。
只有无生产调用方的shutdown_mechanism与health/freeze只增方法；继承监督r4预算。

真实writer join原样保留Result和实例凭证；health Pending保留句柄，freeze/hook
严格保持feature边界。NotSpawned与实际Joined分开，状态私有、不能伪造同步；
插入失败返原句柄，teardown按值单次消费，真实结果和残余句柄进入唯一报告。
既有stop/Drop与daemon不改；真实关闭顺序、main全出口、D/90及观测在W3激活。
实施须W2a先独立验收提交并核最终API，源码和证据另行终审；本节无运行PASS。

W2a已提交`b5a0bfa`，实际WriterHandle只有is_finished/join，停止源为另传只读WriterStop。
W2b最小接口增补经独立审评及root批准，见`local/s48/w2b/root_writer_control_approval.md`：
新模块内以WriterControl和私有ManagedWriterHandle的唯一spawn绑定同一Arc控制对象，
原W2a不改、真实join凭证不变；请求停止不等于已经退出。HardRt导入/变体/match沿
现有Linux cfg，HandleKind稳定。全部增量沿原十一门，生产控制实现仍留W3。

#W2b 实施验收：round3 本地十一门全零、346项完整来源一致，独立源码复审及root
原始证据核验通过，APPROVED_SOFTWARE_SCOPE，详H包local/s48/w2b/review.md。
初版截止接纳、等待预算、失败夹具回收及真实拒停/panic报告路径已修；round1/2原记录保留。
CC额度耗尽后转原生implementation开发/测试与独立Codex终审；无生产调用方或Orin操作。
W1旧锁测试偶发失败另保留观察OPEN，不据重复通过宣称根因闭合。

### P.6.14.12 优先级原语函数分包（Plan Approved，2026-09-09）

函数方案`local/s48/a3/cc_priority_execution_plan_r4.md` SHA
`f96375336a67785e2acb5f9128987ea9e69c692635c4522b122cf3b415951781`已独立
Plan Approved，root采纳见`root_priority_execution_plan_r4_approval.md`；批准限原语分包。
P-M/P-T准备schema与显式RR profile；P-G1准备类缓存、队列、信用及回复分类；
P-S1/P-L1准备可信peer准入、双队列和心跳新鲜性；P-A1准备typed槽与真实总线租约桥。
旧生产API和调用路径保留，各包另核真实基线、运行质量门并独立终审提交。

P-M为schema唯一owner，W3a只引用最终生成类型；旧v0依赖一并冻结，旧builder兼容。
SetEnabled保留0–9，追加SupersededByDisable=10/QueueFull=11/Superseded=12，
Shutdown继续PreemptedByShutdown=7。P-M修改build.rs须附实际workspace --timings报告。
总线桥按值持有真实NormalIoLease，按class/intent受检构造、跨域拒绝返原凭证，
外部crate公开API夹具验证，不暴露raw或Drop自动释放。Start槽为固定域+原子epoch，
take/withdraw在同字竞争后可复用；Execution安装/重复读取/清除不消费权限视图。
目标原子能力编译期约束只证明lock-free，RT零分配门保留，不宣称wait-free。

P-A1任务r2及root范围裁决已批准准备态，详local/s48/pa1/root_task_r2_approval.md。
每条waiter独立锁存，容量满返QueueFull{Lower}且返原请求；完整Enable RR/旧seq
随真实retire成功移交cleanup。桥先核三方domain再withdraw，不接收bool退休证明。
未撤销Queued起步失败及Succeeded真实commit拒绝的退休入口留W3结合拥有返回补齐，
不得据此原语包声称全部退休路径或生产接线完成；原九门与真实allocator测试保留。

S46原七件工具/回归文件已由S46-B独立源码/证据终审通过；round2七门、三CLI及
81具名通过，详local/s48/s46/review.md。旧wire4 FAIL与长测来源保留，不算修复成功，
不成为W0.1/W2a/W2b前置。无调用者模块按依赖推进，下一包P-M。
W3-P与W3统一激活gateway/S0/S1协议、profile和真实退休，并同时迁移权限/唯一writer/
监督/ACK；不支持新gateway配缺退休字段旧S0的生产组合。完整W3函数接线另审，
本节不批准硬件操作或新负载PASS；详既有容量/生命周期合同及root四项裁决。

#### P.6.14.13 W3a Clear协调原语（Plan Approved，待W2a/W2b）

函数稿`local/s48/a3/cc_w3a_coordination_plan_r7.md` SHA
`f16f519bd812c992bdf6c6c77a947c850b0f862f7ea0f9876e17a6b087117736`已获
独立增量复审及root采纳，见`root_w3a_coordination_plan_r7_approval.md`。
只批准无生产调用者的协调/health模块及A2两个专项接口：abandon_repair零改字返证；
retire_unprepared_clear仅对完整匹配的Prepare前置拒绝，CAS只清Preparing并保留最新保护位。
Cleanup与Unprepared各经真实A2退休才回包，后者无文件完成事实、不清EStop义务。

每tick单槽暂存最多一条完成，先消费身份匹配的完成和可执行退休，再判剩余责任
WriterGone，最后统一派发。真实claim计一次不回滚；AwaitRetry改目标仍计共享8次，
首次正常修复/重同步按显式来源表处理。所有派发失败保留原owner及具名原因。
实施后须真实mailbox反例和十道门，不能以方案批准替代运行PASS；schema仍由P-M
负责，daemon/IPC/权限权威/ACK/worker8/磁盘生产切换继续留完整W3另审。

## P.7 部署与整候选回滚（仅形成可评审操作单，本轮不执行完整流程）

S48整候选单位按P.6.14.12扩为gateway/S0/S1相关二进制、unit和配置。下方旧S0
步骤只作既有模板；W3实际接线后必须按真实路径/unit补齐整套SHA、安装/停止顺序
及回滚清单并独立评审，不能只回滚S0后声称整套兼容。本轮不执行部署。

改两份arm service：`RestartPreventExitStatus=86 87 89`、guard `--`后加
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
自行修。任何组未实际跑fmt+clippy不commit；同一最终源码的有效缓存命中（Fresh）
仍为通过，不为制造Checking输出执行cargo clean。不push、不部署由中间状态绕过整体
验收。`ManipularAdapter`等冻结trait保持0改；preempt构造服务新增接口是此Plan评审项。

round 1集中回修闭合项（本稿冻结待round 2，不带未定行为进入Execute）：

1. PreemptProbe的begin/finish/checkpoint分离、操作域/confirm可信上下文、hygiene错误
   独立原子事实槽获reviewer认可；安全判定之前先消费hygiene事实，每轮attempt只计一次。
2. 无seq超时已核正常可达；采用NormalAbortTarget tagged单词，具体编码、消费/清理及
   交错已在P.3写明。主agent先Reverse Sync Spec后，此分支方可实现。
3. 周期IO入口拒绝与在途不可拆已按D13收准；AC-1历史12/12指代已由Cargo --list核清，
   具名全集以baseline.json冻结；两ignored用例显式命令、RPC三个B4 override、freeze
   user unit完整命令均已列出。不可用systemd/Orin环境须真实登记，禁止删测试绕门。

以上为 round 1 计划审查当时的无生产改动记录；当前实施与验收进展以顶部和 P.6.5 为准。
Plan通过后主agent更新Spec链接与审批记录，
Dev按C1→C5推进，reviewer每组独立审查，修复后再审，最终以逐 AC evidence 和 HANDOFF
记录结果，当前证据入口为 C5.json 与 C5_evidence.tar.gz；未满足的现场条目显式保留。
