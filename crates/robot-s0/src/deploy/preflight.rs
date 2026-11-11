//! **fail-closed ensure-safe preflight**（v0.5 候选 E 段(9)，Spec D13 +
//! P.2.9 R9-1/R10-2/R10-3 + AC-E16）。
//!
//! ## 语义（D13 四步，顺序不可换）
//!
//! 1. （审计步，可选）预读当前值——**仅记录**，不决定是否执行安全写；
//! 2. **无条件 best-effort 安全写本 bus 全部电机**（[`safe_write_plan`] 给出
//!    有序写序）；**单项失败不中断**；
//! 3. **逐项 readback** 并断言安全值（[`SAFE_VALUE`]）；单项失败同样继续；
//! 4. **末尾聚合**：任一写 / readback 失败或值不安全 → 整体 `Err` → 禁止切换。
//!
//! r3 "先读非零才写" **已撤回**：首读失败会跳过断扭尝试并可能中断其余电机
//! ——安全方向必须**先写后验**。故本模块不做审计预读（步骤 1 可选），
//! 直接进入无条件安全写。
//!
//! ## 为什么 readback 是独立第二遍
//!
//! D13 把"全部写"与"逐项 readback"拍成两步：先让整条 bus 尽快落到断扭态，
//! 再花时间读回验证。写失败的项**仍然**执行 readback（写失败已单独记账，
//! readback 只是补充证据），保持"每电机每安全寄存器都读回"的字面。
//!
//! ## 双 bus 归属（R9-1）
//!
//! 两条 bus 的 motor ID 重叠（均有 1..6），[`DeployTransport`] 只有 `motor`
//! 参数无从判归属，故失败记录一律携带 [`DeployArm`]。
//!
//! ## `TransportErr` 无来源信息（R10-3）
//!
//! 调用点按**位置**分类 [`OpKind::Write`] / [`OpKind::Readback`]；串口构造
//! 失败在 [`ensure_safe_handoff`] 的**签名上**显式传入，聚合成
//! [`PreflightFailure::Connect`]——`OpKind` 不含 `Connect` 变体。

use std::fmt;

use thiserror::Error;

use super::types::{DeployArm, REG_GOAL_VELOCITY, REG_TORQUE_ENABLE};

/// 安全值：`Torque_Enable` 与 `Goal_Velocity` 的安全态同为 0。
pub const SAFE_VALUE: i32 = 0;

/// transport 层错误——**故意不携来源信息**（R10-3）：调用点按位置分类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportErr(pub String);

impl fmt::Display for TransportErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TransportErr {}

/// 一条 bus 的寄存器读写面（feetech 真实现 + mock 测试实现）。
///
/// **只按 motor id 寻址**：部署期不依赖 joint 名，避免与运行期 layout 语义
/// 耦合。
pub trait DeployTransport {
    /// 写单个寄存器。
    ///
    /// # Errors
    ///
    /// 透传底层串口 / 协议错误（[`TransportErr`]）。
    fn write_register(
        &mut self,
        motor: u8,
        reg: &'static str,
        value: i32,
    ) -> Result<(), TransportErr>;

    /// 读回单个寄存器。
    ///
    /// # Errors
    ///
    /// 透传底层串口 / 协议错误（[`TransportErr`]）。
    fn read_register(&mut self, motor: u8, reg: &'static str) -> Result<i32, TransportErr>;
}

/// 失败发生在哪一步（R10-3：**无** `Connect` 变体）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    /// 安全写。
    Write,
    /// 读回校验。
    Readback,
}

impl OpKind {
    /// 日志短名。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Readback => "readback",
        }
    }
}

impl fmt::Display for OpKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// 单条失败记录。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PreflightFailure {
    /// 该侧串口 / bus 构造失败——**整侧写序被跳过**，不影响另一侧。
    #[error("{arm} bus connect failed: {err}")]
    Connect {
        /// 归属臂。
        arm: DeployArm,
        /// 构造错误文本。
        err: String,
    },
    /// 写 / 读回失败，或读回值不安全。
    #[error("{arm} motor {motor} {reg} {op} failed: {err}")]
    Operation {
        /// 归属臂（双 bus 同号 motor 靠本字段区分）。
        arm: DeployArm,
        /// 电机 id。
        motor: u8,
        /// 寄存器名。
        reg: &'static str,
        /// 出错步骤。
        op: OpKind,
        /// 错误文本（读回值不安全时为 `unsafe value N`）。
        err: String,
    },
}

impl PreflightFailure {
    /// 该失败归属哪条臂。
    #[must_use]
    pub const fn arm(&self) -> DeployArm {
        match self {
            Self::Connect { arm, .. } | Self::Operation { arm, .. } => *arm,
        }
    }
}

/// 聚合报告——**非空即整体 FAIL**。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnsureSafeReport {
    /// 全部失败记录，按发生顺序（左侧先于右侧）。
    pub failures: Vec<PreflightFailure>,
}

impl EnsureSafeReport {
    /// 只取某条臂的失败（诊断用）。
    #[must_use]
    pub fn failures_for(&self, arm: DeployArm) -> Vec<&PreflightFailure> {
        self.failures.iter().filter(|f| f.arm() == arm).collect()
    }
}

impl fmt::Display for EnsureSafeReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} preflight failure(s)", self.failures.len())?;
        for failure in &self.failures {
            write!(f, "; {failure}")?;
        }
        Ok(())
    }
}

/// 一条安全写（写完后 readback 同一 `(motor, reg)`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafeOp {
    /// 电机 id。
    pub motor: u8,
    /// 寄存器名。
    pub reg: &'static str,
}

/// 本 bus 的**完整有序**安全写序（R10-2 精确计数）：
///
/// - `Left`  = bus1 `[T1..T8]` —— 8 条；
/// - `Right` = bus2 `[T1..T6, V7,T7, V8,T8, V9,T9]` —— 12 条。
///
/// `T` = `Torque_Enable = 0`，`V` = `Goal_Velocity = 0`；**每轮 V 严格先于
/// 同轮 T**（先零速再断扭，D13 第 2 步顺序锁定）。
#[must_use]
pub fn safe_write_plan(arm: DeployArm) -> Vec<SafeOp> {
    let mut plan = Vec::with_capacity(arm.safe_write_count());
    for &motor in arm.torque_only_ids() {
        plan.push(SafeOp {
            motor,
            reg: REG_TORQUE_ENABLE,
        });
    }
    for &motor in arm.velocity_then_torque_ids() {
        plan.push(SafeOp {
            motor,
            reg: REG_GOAL_VELOCITY,
        });
        plan.push(SafeOp {
            motor,
            reg: REG_TORQUE_ENABLE,
        });
    }
    plan
}

/// 对**一条 bus** 执行 ensure-safe（无条件安全写 → 逐项 readback → 聚合）。
///
/// # Errors
///
/// 任一写 / readback 失败或读回值 != [`SAFE_VALUE`] → [`EnsureSafeReport`]
/// （调用方必须 fail-closed：不起新栈）。
pub fn ensure_safe_arm(
    arm: DeployArm,
    bus: &mut dyn DeployTransport,
) -> Result<(), EnsureSafeReport> {
    let mut failures = Vec::new();
    collect_arm_failures(arm, bus, &mut failures);
    finish(failures)
}

/// handoff：**双 bus 全量**（左 8 + 右 12 = 20 条写 + 各自 readback）。
///
/// 串口构造失败在签名上显式（`Err(String)`）：该侧聚合
/// [`PreflightFailure::Connect`] 并**跳过整条写序**，另一侧照常**完整执行**。
///
/// # Errors
///
/// 任一侧有任何失败 → 整体 [`EnsureSafeReport`]。
pub fn ensure_safe_handoff(
    left_bus: Result<&mut dyn DeployTransport, String>,
    right_bus: Result<&mut dyn DeployTransport, String>,
) -> Result<(), EnsureSafeReport> {
    // 两侧的 `&mut dyn` 生命周期各自独立，不能合进同一个数组迭代——逐侧
    // 展开（顺序固定：左先右后，聚合报告顺序即此序）。
    let mut failures = Vec::new();
    collect_side(DeployArm::Left, left_bus, &mut failures);
    collect_side(DeployArm::Right, right_bus, &mut failures);
    finish(failures)
}

/// 一侧的分派：`Ok` → 完整写序；`Err` → 记 `Connect` 并**跳过整条写序**。
fn collect_side(
    arm: DeployArm,
    bus: Result<&mut dyn DeployTransport, String>,
    failures: &mut Vec<PreflightFailure>,
) {
    match bus {
        Ok(bus) => collect_arm_failures(arm, bus, failures),
        Err(err) => failures.push(PreflightFailure::Connect { arm, err }),
    }
}

/// 单 bus 的两遍执行；失败追加进 `failures`（**不**提前返回）。
fn collect_arm_failures(
    arm: DeployArm,
    bus: &mut dyn DeployTransport,
    failures: &mut Vec<PreflightFailure>,
) {
    let plan = safe_write_plan(arm);
    // 第 2 步：无条件安全写，单项失败继续。
    for op in &plan {
        if let Err(e) = bus.write_register(op.motor, op.reg, SAFE_VALUE) {
            failures.push(PreflightFailure::Operation {
                arm,
                motor: op.motor,
                reg: op.reg,
                op: OpKind::Write,
                err: e.0,
            });
        }
    }
    // 第 3 步：逐项 readback 并断言安全值；写失败的项同样读回（补充证据）。
    for op in &plan {
        match bus.read_register(op.motor, op.reg) {
            Ok(SAFE_VALUE) => {}
            Ok(value) => failures.push(PreflightFailure::Operation {
                arm,
                motor: op.motor,
                reg: op.reg,
                op: OpKind::Readback,
                err: format!("unsafe value {value}"),
            }),
            Err(e) => failures.push(PreflightFailure::Operation {
                arm,
                motor: op.motor,
                reg: op.reg,
                op: OpKind::Readback,
                err: e.0,
            }),
        }
    }
}

fn finish(failures: Vec<PreflightFailure>) -> Result<(), EnsureSafeReport> {
    if failures.is_empty() {
        Ok(())
    } else {
        Err(EnsureSafeReport { failures })
    }
}

/// feetech 真串口实现——**仅 `xlerobot-real`**（拉 serialport / libudev）。
#[cfg(feature = "xlerobot-real")]
pub mod feetech {
    use std::collections::HashMap;
    use std::path::Path;

    use robot_adapter_feetech::bus::FeetechBus;
    use robot_adapter_feetech::registers::DEFAULT_BAUDRATE;
    use robot_adapter_feetech::transport::real::RealSerialTransport;
    use robot_adapter_feetech::types::Motor;

    use super::super::identity::load_and_verify_arm_config;
    use super::super::types::DeployArm;
    use super::{DeployTransport, TransportErr};

    /// 一条真串口 bus 的部署期读写面。
    ///
    /// **不做 handshake**：`connect(true)` 会 ping 每个电机，任一失联即整体
    /// 失败——与 D13 "单项失败继续、末尾聚合" 冲突。
    pub struct FeetechDeployBus {
        bus: FeetechBus<RealSerialTransport>,
    }

    impl FeetechDeployBus {
        /// 打开串口并构造 bus。
        ///
        /// `ids` 只用于建立 `Motor` 表（本模块全部走 by-id API，名字仅占位）。
        ///
        /// # Errors
        ///
        /// 串口打开 / `Motor::new` / `FeetechBus::new` / `connect` 任一步失败
        /// → `Err(String)`，由调用方聚合成 `PreflightFailure::Connect`。
        pub fn open(address: &Path, ids: &[u8]) -> Result<Self, String> {
            let transport = RealSerialTransport::open(address, DEFAULT_BAUDRATE)
                .map_err(|e| format!("open {}: {e}", address.display()))?;
            let mut motors = HashMap::with_capacity(ids.len());
            for &id in ids {
                let motor = Motor::new(id).map_err(|e| format!("motor {id}: {e}"))?;
                motors.insert(format!("m{id}"), motor);
            }
            let mut bus =
                FeetechBus::new(transport, motors).map_err(|e| format!("bus new: {e}"))?;
            bus.connect(false).map_err(|e| format!("connect: {e}"))?;
            Ok(Self { bus })
        }
    }

    /// 从**本臂 config** 解析串口并打开该 bus——guard 与 `robot-deploy`
    /// 各子命令**共用同一条解析路径**（避免双源漂移）。
    ///
    /// **打开串口之前先过 [`verify`](super::super::identity::verify_arm_config)
    /// 的 D2 身份校验**（round 1 P0-1）：`arm` 决定安全写序、config 决定物理
    /// bus，两者不绑定时 `--arm left --config …right_arm.toml` 会打开 bus2
    /// 却跑左侧写序——干扰在跑的右臂且漏掉 wheel ID 9。
    ///
    /// 电机集取 [`DeployArm::all_ids`]（**本 bus 全量**，D13 round 8 P0-2
    /// 扩程），**不是** config 里的 `motor_ids`——后者只含本臂 6 个关节，
    /// 覆盖不到 head / wheels 的崩溃残留扭矩。
    ///
    /// # Errors
    ///
    /// D2 身份校验失败（含左右 config 互换）/ 串口打开失败。
    pub fn open_arm_bus(arm: DeployArm, config_path: &Path) -> Result<FeetechDeployBus, String> {
        let verified = load_and_verify_arm_config(arm, config_path).map_err(|e| e.to_string())?;
        FeetechDeployBus::open(Path::new(&verified.address), &arm.all_ids())
            .map_err(|e| format!("{arm} bus: {e}"))
    }

    impl DeployTransport for FeetechDeployBus {
        fn write_register(
            &mut self,
            motor: u8,
            reg: &'static str,
            value: i32,
        ) -> Result<(), TransportErr> {
            self.bus
                .sync_write_by_id(reg, &[(motor, value)])
                .map_err(|e| TransportErr(e.to_string()))
        }

        fn read_register(&mut self, motor: u8, reg: &'static str) -> Result<i32, TransportErr> {
            let mut out = [0i32; 1];
            self.bus
                .sync_read_by_id_into(reg, &[motor], &mut out)
                .map_err(|e| TransportErr(e.to_string()))?;
            Ok(out[0])
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    /// 记录型 mock：写序完整落 `log`，可按 `(motor, reg)` 注入写 / 读失败或
    /// 不安全读回值。
    #[derive(Default)]
    struct MockBus {
        log: Vec<String>,
        write_fail: HashSet<(u8, &'static str)>,
        read_fail: HashSet<(u8, &'static str)>,
        values: HashMap<(u8, &'static str), i32>,
    }

    impl MockBus {
        /// 初值全部**非安全**（1）——只有写成功才会翻成 0，能抓"漏写"。
        fn new() -> Self {
            Self::default()
        }

        fn value(&self, key: (u8, &'static str)) -> i32 {
            self.values.get(&key).copied().unwrap_or(1)
        }

        /// 只保留写操作的有序序列（AC-E16 断言序列本身，非只断数量）。
        fn writes(&self) -> Vec<&str> {
            self.log
                .iter()
                .filter(|e| e.starts_with('W'))
                .map(String::as_str)
                .collect()
        }
    }

    impl DeployTransport for MockBus {
        fn write_register(
            &mut self,
            motor: u8,
            reg: &'static str,
            value: i32,
        ) -> Result<(), TransportErr> {
            self.log.push(format!("W:{motor}:{reg}"));
            if self.write_fail.contains(&(motor, reg)) {
                return Err(TransportErr(format!("write boom {motor}")));
            }
            self.values.insert((motor, reg), value);
            Ok(())
        }

        fn read_register(&mut self, motor: u8, reg: &'static str) -> Result<i32, TransportErr> {
            self.log.push(format!("R:{motor}:{reg}"));
            if self.read_fail.contains(&(motor, reg)) {
                return Err(TransportErr(format!("read boom {motor}")));
            }
            Ok(self.value((motor, reg)))
        }
    }

    const T: &str = REG_TORQUE_ENABLE;
    const V: &str = REG_GOAL_VELOCITY;

    fn expect_writes(entries: &[(u8, &str)]) -> Vec<String> {
        entries
            .iter()
            .map(|(id, reg)| format!("W:{id}:{reg}"))
            .collect()
    }

    /// 左 bus：`[T1..T8]` 共 8 条，无轮子零速。
    #[test]
    fn left_plan_is_eight_torque_writes_in_order() {
        let mut bus = MockBus::new();
        ensure_safe_arm(DeployArm::Left, &mut bus).expect("全绿");
        let expected = expect_writes(&[
            (1, T),
            (2, T),
            (3, T),
            (4, T),
            (5, T),
            (6, T),
            (7, T),
            (8, T),
        ]);
        assert_eq!(bus.writes(), expected, "左 bus 有序写序");
        // 8 写 + 8 readback。
        assert_eq!(bus.log.len(), 16);
    }

    /// 右 bus：`[T1..T6, V7,T7, V8,T8, V9,T9]` 共 12 条——**每轮 V 严格先于
    /// 同轮 T**（AC-E16 断言序列本身）。
    #[test]
    fn right_plan_is_twelve_writes_with_velocity_before_torque() {
        let mut bus = MockBus::new();
        ensure_safe_arm(DeployArm::Right, &mut bus).expect("全绿");
        let expected = expect_writes(&[
            (1, T),
            (2, T),
            (3, T),
            (4, T),
            (5, T),
            (6, T),
            (7, V),
            (7, T),
            (8, V),
            (8, T),
            (9, V),
            (9, T),
        ]);
        assert_eq!(bus.writes(), expected, "右 bus 有序写序（V 先于同轮 T）");
        assert_eq!(bus.log.len(), 24);
    }

    /// handoff：17 电机 / 20 条写 / 各自 readback。
    #[test]
    fn handoff_covers_seventeen_motors_and_twenty_writes() {
        let mut left = MockBus::new();
        let mut right = MockBus::new();
        ensure_safe_handoff(Ok(&mut left), Ok(&mut right)).expect("全绿");
        assert_eq!(left.writes().len() + right.writes().len(), 20);
        assert_eq!(left.writes().len(), 8);
        assert_eq!(right.writes().len(), 12);
        let motors: HashSet<u8> = left
            .log
            .iter()
            .chain(right.log.iter())
            .map(|e| e.split(':').nth(1).unwrap().parse::<u8>().unwrap())
            .collect();
        // 左 8 + 右 9 = 17 电机（左右 id 重叠，故按侧分别统计）。
        assert_eq!(
            DeployArm::Left.all_ids().len() + DeployArm::Right.all_ids().len(),
            17
        );
        assert!(!motors.is_empty());
    }

    /// 单项写失败**不中断**：其余写与全部 readback 照常执行，末尾聚合 FAIL。
    #[test]
    fn write_failure_continues_and_aggregates() {
        let mut bus = MockBus::new();
        bus.write_fail.insert((3, T));
        let report = ensure_safe_arm(DeployArm::Left, &mut bus).expect_err("必须 FAIL");
        // 完整 8 条写仍然发出。
        assert_eq!(bus.writes().len(), 8);
        // 写失败 1 条 + 该电机 readback 仍为初值 1（不安全）1 条。
        assert_eq!(report.failures.len(), 2, "{report}");
        assert!(matches!(
            report.failures[0],
            PreflightFailure::Operation {
                arm: DeployArm::Left,
                motor: 3,
                op: OpKind::Write,
                ..
            }
        ));
        assert!(matches!(
            report.failures[1],
            PreflightFailure::Operation {
                motor: 3,
                op: OpKind::Readback,
                ..
            }
        ));
    }

    /// readback 读到不安全值 → `Operation{op: Readback}`（写本身成功）。
    #[test]
    fn unsafe_readback_value_fails() {
        let mut bus = MockBus::new();
        ensure_safe_arm(DeployArm::Left, &mut bus).expect("先全绿");
        // 模拟"写成功但电机没落到安全态"：直接篡改回不安全值。
        bus.log.clear();
        bus.values.insert((5, T), 1);
        bus.write_fail.insert((5, T)); // 写失败 → 不会被改回 0
        let report = ensure_safe_arm(DeployArm::Left, &mut bus).expect_err("必须 FAIL");
        let readback: Vec<_> = report
            .failures
            .iter()
            .filter(|f| {
                matches!(
                    f,
                    PreflightFailure::Operation {
                        op: OpKind::Readback,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(readback.len(), 1);
        assert!(format!("{}", readback[0]).contains("unsafe value 1"));
    }

    /// readback 传输失败 → `Operation{op: Readback}` 且带原始错误文本。
    #[test]
    fn readback_transport_failure_is_recorded() {
        let mut bus = MockBus::new();
        bus.read_fail.insert((2, T));
        let report = ensure_safe_arm(DeployArm::Left, &mut bus).expect_err("必须 FAIL");
        assert_eq!(report.failures.len(), 1);
        assert!(format!("{report}").contains("read boom 2"), "{report}");
    }

    /// 反向 ①（R9-1）：双 bus 各注入**同号** motor 失败 → 两条记录按
    /// `PreflightFailure.arm` 分别归属，不混淆。
    #[test]
    fn same_motor_id_on_both_buses_is_attributed_by_arm() {
        let mut left = MockBus::new();
        let mut right = MockBus::new();
        left.write_fail.insert((3, T));
        right.write_fail.insert((3, T));
        let report = ensure_safe_handoff(Ok(&mut left), Ok(&mut right)).expect_err("必须 FAIL");
        let left_ops = report.failures_for(DeployArm::Left);
        let right_ops = report.failures_for(DeployArm::Right);
        assert_eq!(left_ops.len(), 2, "写失败 + 不安全 readback");
        assert_eq!(right_ops.len(), 2);
        for f in left_ops {
            assert_eq!(f.arm(), DeployArm::Left);
            assert!(format!("{f}").starts_with("left motor 3 "), "{f}");
        }
        for f in right_ops {
            assert_eq!(f.arm(), DeployArm::Right);
            assert!(format!("{f}").starts_with("right motor 3 "), "{f}");
        }
    }

    /// 反向 ②（R10-3）：bus1 = `Err(构造失败)` → 断言 bus2 **12 条有序写序
    /// 全部执行** + 各自 readback；最终聚合含 `Connect{Left}`。
    #[test]
    fn left_connect_error_does_not_block_right_full_sequence() {
        let mut right = MockBus::new();
        let report = ensure_safe_handoff(Err("no such device".to_string()), Ok(&mut right))
            .expect_err("必须 FAIL");
        let expected = expect_writes(&[
            (1, T),
            (2, T),
            (3, T),
            (4, T),
            (5, T),
            (6, T),
            (7, V),
            (7, T),
            (8, V),
            (8, T),
            (9, V),
            (9, T),
        ]);
        assert_eq!(right.writes(), expected, "右侧必须完整有序执行");
        assert_eq!(right.log.len(), 24, "12 写 + 12 readback");
        assert_eq!(report.failures.len(), 1);
        assert!(
            matches!(
                &report.failures[0],
                PreflightFailure::Connect {
                    arm: DeployArm::Left,
                    err
                } if err == "no such device"
            ),
            "{report}"
        );
    }

    /// 双侧构造失败 → 两条 `Connect`，无任何写。
    #[test]
    fn both_connect_errors_aggregate_without_writes() {
        let report = ensure_safe_handoff(Err("left down".into()), Err("right down".into()))
            .expect_err("必须 FAIL");
        assert_eq!(report.failures.len(), 2);
        assert_eq!(report.failures[0].arm(), DeployArm::Left);
        assert_eq!(report.failures[1].arm(), DeployArm::Right);
    }

    /// 写序长度与 `safe_write_count` 自洽。
    #[test]
    fn plan_len_matches_safe_write_count() {
        for arm in [DeployArm::Left, DeployArm::Right] {
            assert_eq!(safe_write_plan(arm).len(), arm.safe_write_count(), "{arm}");
        }
    }
}
