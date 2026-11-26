//! `XLeRobot` 5-DOF 手臂 IK 与三轮全向底盘纯运动学。
//!
//! [`base`] 提供具名 SI 机身/轮速、正逆变换和同比缩放；几何与轮速上限须由调用方显式提供。
//! 该模块不含寄存器 raw 换算、固件 Phase/模式处理、里程计、通用底盘 trait 或 S0/S1 接入。
//! 下面的算法与接口说明仅适用于手臂 IK。
//!
//! # 算法源
//!
//! 源自 dimos `arm_ik.py` 的 5-DOF 闭式解；
//! dimos 端上游为 `XLeRobot` `SO101Robot.py` `SO101Kinematics.inverse_kinematics`。
//! 闭式解策略（沿 dimos `arm_ik.py`）：
//!
//! 1. `shoulder_pan`：水平平面旋转，由目标的 `(x, y)` 决定 → `atan2`。
//! 2. `shoulder_lift` + `elbow_flex`：2-DOF 平面 IK（投影到臂平面的
//!    `distance` + `height` + 余弦定理几何法 + `THETA1_OFFSET` /
//!    `THETA2_OFFSET` 连杆弯折补偿）。
//! 3. `wrist_flex`：末端俯仰补偿（目标 pitch 减前两关节累计 pitch）。
//! 4. `wrist_roll`：目标姿态 roll 直接映射。
//!
//! 正解沿用逆解定义的肘屈曲角（零代表两杆伸直）。原版正解把它当成内角，
//! 导致非直角处的腕点位置以及依赖该位置的腕俯仰补偿存在偏差。
//! 本模型的位置参考点是前臂连杆末端的腕点，未包含腕部到夹尖的工具偏移。
//!
//! 5-DOF 整体性质：闭式 + O(1) sub-µs 量级；不依赖外部数值库。
//!
//! # clamp / out-of-reach 行为路径 (Plan §P.3.2 Q-IK16 (a))
//!
//! **沿 dimos 现有行为 = Q-IK16 (a)**：
//!
//! - **out-of-reach** → 缩放到工作空间边界 + 关节限位 clamp，返
//!   [`Ok(IkSolution { joints_rad: clamped })`] —— **不**返
//!   `Err(IkError::OutOfReach)`。配套外部 informational metric 由
//!   调用方负责（如 `IkBackend::start()` 端 `tracing::warn!` 锚句）。
//! - solver **永不**返 [`IkError::OutOfReach`]（实现可观察行为；
//!   trait 层契约允许 solver 自决路径，详 round 1 Spec AC-6）。
//!
//! [`Ok(IkSolution { joints_rad: clamped })`]: robot_kinematics::IkSolution
//! [`IkError::OutOfReach`]: robot_kinematics::IkError::OutOfReach
//!
//! # yaw 处理 (Plan §P.3.2 Q-IK6 (a))
//!
//! 5-DOF 手臂的 end-effector yaw 由 `shoulder_pan = atan2(target_y *
//! sign, target_x)` 几何上决定，**无**独立 yaw 自由度。caller 提供的
//! [`Pose::orientation`] quaternion 解出的 yaw 必须接近 `0`（容差
//! `1e-3` rad ~ 0.057 度）；超出容差 → 返
//! [`IkError::InvalidInput`]（Q-IK6 (a) ACK）。
//!
//! 调用方必须把 yaw 编码进 `target_xyz` 的方向上（让
//! `atan2(target_y, target_x)` 自动得 yaw），quaternion 仅描述 pitch
//! + roll。
//!
//! [`Pose::orientation`]: robot_kinematics::Pose
//! [`IkError::InvalidInput`]: robot_kinematics::IkError::InvalidInput
//!
//! # `deadline_ns` 处理 (Plan §P.3.2 #12 (a))
//!
//! [`solve()`] 实现**仅响应** `deadline_ns = None` 路径；caller 调用方
//! 走 `IkBackend::start()` backend layer guard 做 deadline
//! enforcement（pre/post solver 调用用 `ctx.clock.now().0` 检查）。
//!
//! [`solve()`]: XLeRobotArmIkSolver::solve
//!
//! - **缘由**：本 impl crate 零 `robot-core` dep（Plan §P.3.2 Q-IK2
//!   (a)）；solver 内部无合法 clock 源；闭式解 O(1) sub-µs 量级，外部
//!   backend guard 已 100% 覆盖 enforcement 语义。
//! - **行为**：solver 收到 `deadline_ns = Some(_)` → 返
//!   [`IkError::InvalidInput`]（防御性 reject，跟"impl 不支持 Some
//!   路径"语义一致；catch 调用方误传）；solver **永不**返
//!   [`IkError::DeadlineExceeded`]。
//!
//! [`IkError::DeadlineExceeded`]: robot_kinematics::IkError::DeadlineExceeded
//!
//! - **trait doc 已知不适配**：round 1 trait doc `crates/robot-
//!   kinematics/src/lib.rs:151` 描述的 `Some(ns)` 路径在本 impl 下**永
//!   不被走到**；属已知不适配（**不是** trait doc 含义被本 impl 改
//!   写）。如未来其它 impl（`robot-kinematics-piper` v0.6+ 等）需要
//!   真正 `Some(ns)` 路径 → 走独立 reviewer round amend round 1
//!   trait spec 添 advisory 语义。
//!
//! # 关节顺序
//!
//! [`KinematicInfo::joint_names`]:
//!
//! 1. `shoulder_pan`
//! 2. `shoulder_lift`
//! 3. `elbow_flex`
//! 4. `wrist_flex`
//! 5. `wrist_roll`
//!
//! 调用方必须保证 `HardwareSection.joints[].name` 跟此顺序一致（Plan
//! §P.3.2 #6 (i) startup 期校验由 S1 wiring 段 `Coordinator::new()`
//! 完成；本 impl crate 不做 startup 校验，只暴露 `info()` 元数据）。
//!
//! [`KinematicInfo::joint_names`]: robot_kinematics::KinematicInfo

pub mod base;
pub mod preview;

/// 按运动学模型的轴序返回所选侧五个位置关节的硬件名称。
#[must_use]
pub fn arm_joint_names(side: Side) -> [String; 5] {
    let prefix = match side {
        Side::Left => "left_arm",
        Side::Right => "right_arm",
    };
    preview::ARM_JOINT_NAMES.map(|name| format!("{prefix}_{name}"))
}

/// 右臂 IK 等既有调用方使用的五轴名称。
#[must_use]
pub fn right_arm_joint_names() -> [String; 5] {
    arm_joint_names(Side::Right)
}

/// 核对该侧总线的完整布局，按模型轴序返回五个主动位置槽。
/// 关节的实际槽号取自配置；右侧三轮在 ID7–9 范围内可自由排列。
pub fn arm_home_slots(
    side: Side,
    hardware_id: &str,
    joints: &[String],
    motor_ids: &[u8],
    mixed_raw: bool,
) -> Result<[usize; 5], &'static str> {
    let (count, gripper) = match (side, hardware_id, mixed_raw) {
        (Side::Left, "left_arm_full_with_head", false) => (8, "left_arm_gripper"),
        (Side::Right, "right_arm_full_with_base", true) => (9, "right_arm_gripper"),
        _ => return Err("手臂回位的总线身份或模式不匹配"),
    };
    if joints.len() != usize::from(count)
        || motor_ids.len() != joints.len()
        || !(1..=count).all(|id| motor_ids.contains(&id))
    {
        return Err("手臂回位的电机布局不匹配");
    }
    let slot_of = |name: &str| {
        joints
            .iter()
            .position(|joint| joint == name)
            .ok_or("回位布局缺少具名关节")
    };
    if motor_ids[slot_of(gripper)?] != 6 {
        return Err("夹爪与电机 ID6 不匹配");
    }
    match side {
        Side::Left => {
            if motor_ids[slot_of("head_motor_1")?] != 7 || motor_ids[slot_of("head_motor_2")?] != 8
            {
                return Err("两头轴与电机 ID7、ID8 不匹配");
            }
        }
        Side::Right => {
            for wheel in ["base_front_wheel", "base_left_wheel", "base_right_wheel"] {
                if motor_ids[slot_of(wheel)?] < 7 {
                    return Err("轮子映射到了手臂位置槽");
                }
            }
        }
    }
    let mut slots = [0; 5];
    for (axis, name) in arm_joint_names(side).iter().enumerate() {
        let slot = slot_of(name)?;
        if usize::from(motor_ids[slot]) != axis + 1 {
            return Err("五轴回位关节与电机 ID1 至 ID5 不匹配");
        }
        slots[axis] = slot;
    }
    Ok(slots)
}

/// 五轴共用同一条线性轨迹。至少 10 秒，按最慢轴向上取整到毫秒。
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "向上取整后已核对 u32 范围"
)]
pub fn arm_home_duration_ms(
    start_rad: [f64; 5],
    max_velocity_rad_s: [f64; 5],
) -> Result<u32, &'static str> {
    let mut duration_ms = 10_000;
    for (start, speed_limit) in start_rad.into_iter().zip(max_velocity_rad_s) {
        if !start.is_finite() || !speed_limit.is_finite() || speed_limit <= 0.0 {
            return Err("手臂回位起点或速率限制无效");
        }
        let travel = start.abs();
        let milliseconds = (travel / speed_limit * 1_000.0).ceil();
        if milliseconds > f64::from(u32::MAX) {
            return Err("手臂回位时长超出可表示范围");
        }
        duration_ms = duration_ms.max(milliseconds as u32);
        // 用与实际目标检查相同的除法处理浮点边界，防止取整后仍微超速。
        if travel / (f64::from(duration_ms) / 1_000.0) > speed_limit {
            duration_ms = duration_ms.checked_add(1).ok_or("手臂回位时长溢出")?;
        }
    }
    Ok(duration_ms)
}

/// 右臂既有 API；两侧共用同一时长计算。
pub use arm_home_duration_ms as right_arm_home_duration_ms;

use robot_kinematics::{ArmIkSolver, IkError, IkSolution, JointLimit, KinematicInfo, Pose};

// ── 连杆参数（沿 dimos `arm_ik.py:42-52`）────────────────────────────

/// 上臂有效长度 (m) — `sqrt(0.11257² + 0.028²)`。
const L1: f64 = 0.1159;

/// 下臂有效长度 (m) — `sqrt(0.1349² + 0.0052²)`。
const L2: f64 = 0.1350;

/// `shoulder_lift` 零位偏移（连杆弯折补偿）= `atan2(0.028, 0.11257)`。
///
/// 沿 dimos `arm_ik.py:47 THETA1_OFFSET = math.atan2(0.028, 0.11257)`
/// 预计算硬编码（`atan2` 非 `const fn`；连杆几何稳定）。
const THETA1_OFFSET: f64 = 0.244_795_414_148_725_3;

/// `elbow` 零位偏移 = `atan2(0.0052, 0.1349) + THETA1_OFFSET`。
///
/// 沿 dimos `arm_ik.py:48`：`atan2(0.0052, 0.1349) ≈ 0.038_530` +
/// `THETA1_OFFSET ≈ 0.244_795` = `≈ 0.283_325`。
const THETA2_OFFSET: f64 = 0.283_325_504_452_036;

// ── 关节限位 ────────────────────────────────────────────────────────
//
// **dimos `LIMITS` 字典语义辨析**（沿 dimos `arm_ik.py:66-72`）：
//
// - `shoulder_pan` / `wrist_flex` / `wrist_roll`：dimos LIMITS 直接是
//   **motor 角度限位**（URDF 来源；无坐标变换）。
// - `shoulder_lift` / `elbow_flex`：dimos LIMITS 实际是 **IK 内部
//   `joint_shoulder` / `joint_elbow` 限位**（_planar_ik 内部 clamp 用）；
//   motor 角度经 `π/2 - joint_shoulder` / `joint_elbow - π/2` 坐标
//   变换后实际范围**不同**（详 `IK_INTERNAL_*` + `SHOULDER_LIFT_LIMIT`
//   / `ELBOW_FLEX_LIMIT` 推导）。
//
// `KinematicInfo.joint_limits` 暴露给 hardware adapter 用做关节限位
// 一致性校验（Plan §P.3.2 #6 (i) startup 期校验），必须是 **motor
// 角度**限位 —— 跟实际下达给电机的 `JointCommand.positions[]` 范围一
// 致。

/// IK 内部 `joint_shoulder` 限位（沿 dimos `arm_ik.py:68`；用于
/// [`planar_ik`] 内部 clamp，**不**暴露给 [`KinematicInfo`]）。
const IK_INTERNAL_SHOULDER_LIMIT: JointLimit = JointLimit {
    min_rad: -0.22,
    max_rad: 3.37,
};

/// IK 内部 `joint_elbow` 限位（沿 dimos `arm_ik.py:69`；用于
/// [`planar_ik`] 内部 clamp，**不**暴露给 [`KinematicInfo`]）。
///
/// `clippy::approx_constant` 警告 `3.14` 接近 `f64::consts::PI`；本值
/// 是 `XLeRobot` URDF 文档明确的关节限位，跟 `π` 巧合接近但**非** `π`
/// 近似 —— 允许 lint。
#[allow(clippy::approx_constant)]
const IK_INTERNAL_ELBOW_LIMIT: JointLimit = JointLimit {
    min_rad: -0.22,
    max_rad: 3.14,
};

// ── motor 角度限位（暴露在 KinematicInfo） ─────────────────────────

/// motor `shoulder_pan` 限位（无坐标变换；同 IK 内部）。
const SHOULDER_PAN_LIMIT: JointLimit = JointLimit {
    min_rad: -2.16,
    max_rad: 2.16,
};

/// motor `shoulder_lift` 限位 = `π/2 - IK_INTERNAL_SHOULDER_LIMIT`
/// 推导：
///
/// - `shoulder_lift_rad = π/2 - joint_shoulder`
/// - `joint_shoulder ∈ [-0.22, 3.37]`
/// - → `shoulder_lift_rad ∈ [π/2 - 3.37, π/2 + 0.22]`
///
/// `π/2 ≈ 1.570_796_326_794_896_6`。
const SHOULDER_LIFT_LIMIT: JointLimit = JointLimit {
    min_rad: -1.799_203_673_205_103_4, // π/2 - 3.37
    max_rad: 1.790_796_326_794_896_6,  // π/2 + 0.22
};

/// motor `elbow_flex` 限位 = `IK_INTERNAL_ELBOW_LIMIT - π/2` 推导：
///
/// - `elbow_flex_rad = joint_elbow - π/2`
/// - `joint_elbow ∈ [-0.22, 3.14]`
/// - → `elbow_flex_rad ∈ [-0.22 - π/2, 3.14 - π/2]`
const ELBOW_FLEX_LIMIT: JointLimit = JointLimit {
    min_rad: -1.790_796_326_794_896_6, // -0.22 - π/2
    max_rad: 1.569_203_673_205_103_4,  //  3.14 - π/2
};

/// motor `wrist_flex` 限位（无坐标变换；同 IK 内部）。
const WRIST_FLEX_LIMIT: JointLimit = JointLimit {
    min_rad: -1.658,
    max_rad: 1.658,
};

/// motor `wrist_roll` 限位（无坐标变换；同 IK 内部）。
const WRIST_ROLL_LIMIT: JointLimit = JointLimit {
    min_rad: -2.744,
    max_rad: 2.841,
};

// ── 输入校验容差 ────────────────────────────────────────────────────

/// quaternion 规范化容差（`|q| - 1.0` 允许范围）。
const QUAT_NORM_TOLERANCE: f64 = 1e-3;

/// yaw 为 "可忽略" 的容差（rad，~0.057 度）。
const YAW_TOLERANCE: f64 = 1e-3;

/// `horizontal_dist` 接近 `0` 的容差（`atan2` 退化保护）。
const HORIZONTAL_DIST_EPS: f64 = 1e-3;

// ── 左右臂选择 ──────────────────────────────────────────────────────

/// `XLeRobot` 双臂选择。
///
/// 单 solver instance 持有一个 [`Side`]；多臂场景调用方持多个
/// [`XLeRobotArmIkSolver`] instance（Plan §P.3.1 Q-IK3 (a) ACK +
/// round 1 Q4(X) "多 solver instance" 同款）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Right,
    Left,
}

impl Side {
    /// `target_y` 符号变换：右臂 +1.0；左臂 -1.0（沿 dimos
    /// `arm_ik.py:199`）。
    fn sign(self) -> f64 {
        match self {
            Self::Right => 1.0,
            Self::Left => -1.0,
        }
    }
}

// ── solver 主体 ─────────────────────────────────────────────────────

/// `XLeRobot` 5-DOF 手臂 IK solver。
///
/// 详 crate-level doc。
pub struct XLeRobotArmIkSolver {
    info: KinematicInfo,
    side: Side,
}

impl XLeRobotArmIkSolver {
    /// 新建 solver instance。
    #[must_use]
    pub fn new(side: Side) -> Self {
        let info = KinematicInfo {
            joint_count: 5,
            joint_names: preview::ARM_JOINT_NAMES
                .iter()
                .map(|&name| name.to_owned())
                .collect(),
            joint_limits: vec![
                SHOULDER_PAN_LIMIT,
                SHOULDER_LIFT_LIMIT,
                ELBOW_FLEX_LIMIT,
                WRIST_FLEX_LIMIT,
                WRIST_ROLL_LIMIT,
            ],
        };
        Self { info, side }
    }

    /// 从五个电机关节角（弧度）求 IK 模型的腕点位姿。肩基座为原点，X 前、Y 左、Z 上，单位米。
    ///
    /// 四元数沿用逆解的输入约定：肩部水平旋转已计入 XYZ，姿态只编码模型估算的
    /// 俯仰和腕部翻滚。这两个角不描述有工具偏移的夹尖在机身中的完整朝向。
    pub fn forward_pose(&self, joints_rad: &[f64]) -> Result<Pose, IkError> {
        let &[pan, lift, elbow, wrist_flex, roll] = joints_rad else {
            return Err(IkError::InvalidInput {
                reason: format!("正解需要五个关节角，收到 {} 个", joints_rad.len()),
            });
        };
        for (index, (angle, limit)) in joints_rad.iter().zip(&self.info.joint_limits).enumerate() {
            if !(limit.min_rad..=limit.max_rad).contains(angle) {
                return Err(IkError::InvalidInput {
                    reason: format!(
                        "关节 {} 的角度 {angle} 超出模型限位或非有限值",
                        self.info.joint_names[index]
                    ),
                });
            }
        }
        let (radius, z) = planar_fk(lift, elbow);
        let (sin_pan, cos_pan) = pan.sin_cos();
        let pitch = arm_elevation(radius, z) + wrist_flex;
        let (sin_p, cos_p) = (pitch * 0.5).sin_cos();
        let (sin_r, cos_r) = (roll * 0.5).sin_cos();
        Ok(Pose {
            translation: [radius * cos_pan, self.side.sign() * radius * sin_pan, z],
            orientation: [cos_p * sin_r, sin_p * cos_r, -sin_p * sin_r, cos_p * cos_r],
        })
    }
    /// 在下发混合总线前用正解识别被限位或工作空间裁剪的解。误差只覆盖模型计算舍入。
    pub fn check_solution(&self, target: &Pose, joints_rad: &[f64]) -> Result<(), IkError> {
        let actual = self.forward_pose(joints_rad)?;
        let [dx, dy, dz] = std::array::from_fn(|i| actual.translation[i] - target.translation[i]);
        let position_error = dx.hypot(dy).hypot(dz);
        // 四元数 q 和 −q 表示同一姿态。比较夹角余弦前先归一化。
        let norm = target.orientation.iter().map(|q| q * q).sum::<f64>().sqrt();
        let dot = actual
            .orientation
            .iter()
            .zip(target.orientation)
            .map(|(a, b)| a * b)
            .sum::<f64>();
        if position_error <= 1e-7 && norm > 0.0 && (dot.abs() / norm - 1.0).abs() <= 1e-10 {
            Ok(())
        } else {
            Err(IkError::NoSolution)
        }
    }
}

impl Default for XLeRobotArmIkSolver {
    /// 默认右臂（沿 dimos `arm_ik.py:189 side="right"`）。
    fn default() -> Self {
        Self::new(Side::Right)
    }
}

// ── 工具函数 ────────────────────────────────────────────────────────

#[inline]
fn clamp_to_limit(value: f64, limit: &JointLimit) -> f64 {
    value.clamp(limit.min_rad, limit.max_rad)
}

/// quaternion `[x, y, z, w]` → intrinsic XYZ Tait-Bryan `(roll,
/// pitch, yaw)`（弧度）。
///
/// 跟 dimos `arm_ik.py` 的 `(target_pitch, target_roll)` 约定一致：
/// `pitch` 绕 Y 轴（左右），`roll` 绕 X 轴（前后），`yaw` 绕 Z 轴
/// （上下）。
fn quat_to_euler(q: &[f64; 4]) -> (f64, f64, f64) {
    let [qx, qy, qz, qw] = *q;

    let roll_y = 2.0_f64.mul_add(qw * qx + qy * qz, 0.0);
    let roll_x = (qx * qx + qy * qy).mul_add(-2.0, 1.0);
    let roll = roll_y.atan2(roll_x);

    let sinp = 2.0_f64.mul_add(qw * qy - qz * qx, 0.0);
    let pitch = sinp.clamp(-1.0, 1.0).asin();

    let yaw_y = 2.0_f64.mul_add(qw * qz + qx * qy, 0.0);
    let yaw_x = (qy * qy + qz * qz).mul_add(-2.0, 1.0);
    let yaw = yaw_y.atan2(yaw_x);

    (roll, pitch, yaw)
}

/// 2-DOF 平面 IK（沿 dimos `arm_ik.py:97-152` `_planar_ik` 同款移
/// 植）。
///
/// 在臂平面内，给定 `(水平距离 px, 高度 pz)`，返
/// `(shoulder_lift_rad, elbow_flex_rad)`。
///
/// **out-of-reach 处理**（Q-IK16 (a) clamp 路径）：
///
/// - `r > r_max` → 缩放到 `r = r_max`
/// - `r < r_min && r > 0` → 缩放到 `r = r_min`
///
/// 关节限位 clamp 在内部完成。
fn planar_ik(px: f64, pz: f64) -> (f64, f64) {
    let mut px = px;
    let mut pz = pz;
    let r = (px * px + pz * pz).sqrt();
    let r_max = L1 + L2;
    let r_min = (L1 - L2).abs();

    // 工作空间边界处理（沿 dimos `arm_ik.py:117-126`）
    if r > r_max {
        let scale = r_max / r;
        px *= scale;
        pz *= scale;
    } else if r < r_min && r > 0.0 {
        let scale = r_min / r;
        px *= scale;
        pz *= scale;
    }

    // 重算 `r`（缩放后），用于余弦定理
    let r_clamped = (px * px + pz * pz).sqrt();

    // 余弦定理求 elbow 角（沿 dimos `arm_ik.py:129-131`）：
    // `cos_theta2 = -(r² - L1² - L2²) / (2·L1·L2) = (L1² + L2² - r²) / (2·L1·L2)`。
    let cos_theta2 = (L1 * L1 + L2 * L2 - r_clamped * r_clamped) / (2.0 * L1 * L2);
    let cos_theta2 = cos_theta2.clamp(-1.0, 1.0);
    let theta2 = std::f64::consts::PI - cos_theta2.acos();

    // 求 shoulder 角（沿 dimos `arm_ik.py:134-136`）
    let beta = pz.atan2(px);
    let gamma = (L2 * theta2.sin()).atan2(L1 + L2 * theta2.cos());
    let theta1 = beta + gamma;

    // 加偏移（连杆弯折补偿；沿 dimos `arm_ik.py:139-140`）
    let joint_shoulder = theta1 + THETA1_OFFSET;
    let joint_elbow = theta2 + THETA2_OFFSET;

    // **IK 内部** 限位（沿 dimos `arm_ik.py:143-144`；clamp `joint_*`
    // 变换前的角度，**不是** motor 角；详 `IK_INTERNAL_*` 段说明）
    let joint_shoulder = clamp_to_limit(joint_shoulder, &IK_INTERNAL_SHOULDER_LIMIT);
    let joint_elbow = clamp_to_limit(joint_elbow, &IK_INTERNAL_ELBOW_LIMIT);

    // 坐标系变换（SO101 度数变换；沿 dimos `arm_ik.py:147-150`）
    let half_pi = std::f64::consts::FRAC_PI_2;
    let shoulder_lift_rad = half_pi - joint_shoulder;
    let elbow_flex_rad = joint_elbow - half_pi;

    // 防御性 motor 角度限位 clamp（IK 内部 clamp 后变换出来的 motor
    // 角理论上必在 motor 限位内；本 clamp 是数值漂移安全网）
    let shoulder_lift_rad = clamp_to_limit(shoulder_lift_rad, &SHOULDER_LIFT_LIMIT);
    let elbow_flex_rad = clamp_to_limit(elbow_flex_rad, &ELBOW_FLEX_LIMIT);

    (shoulder_lift_rad, elbow_flex_rad)
}

/// 平面正解。两段从肩到腕的向量分别位于 θ1 和 θ1−θ2，θ2 是逆解给出的屈曲角。
fn planar_fk(shoulder_lift_rad: f64, elbow_flex_rad: f64) -> (f64, f64) {
    let half_pi = std::f64::consts::FRAC_PI_2;
    let joint_shoulder = half_pi - shoulder_lift_rad;
    let joint_elbow = elbow_flex_rad + half_pi;

    let theta1 = joint_shoulder - THETA1_OFFSET;
    let theta2 = joint_elbow - THETA2_OFFSET;
    let (forearm_sin, forearm_cos) = (theta1 - theta2).sin_cos();
    let px = L2.mul_add(forearm_cos, L1 * theta1.cos());
    let pz = L2.mul_add(forearm_sin, L1 * theta1.sin());
    (px, pz)
}

/// 沿用上游腕部俯仰补偿的肩到腕射线仰角；水平投影退化时取零。
fn arm_elevation(horizontal: f64, height: f64) -> f64 {
    if horizontal > HORIZONTAL_DIST_EPS {
        height.atan2(horizontal)
    } else {
        0.0
    }
}

// ── trait impl ──────────────────────────────────────────────────────

impl ArmIkSolver for XLeRobotArmIkSolver {
    fn info(&self) -> &KinematicInfo {
        &self.info
    }

    fn solve(
        &self,
        target: &Pose,
        _seed: Option<&[f64]>,
        deadline_ns: Option<u64>,
    ) -> Result<IkSolution, IkError> {
        // ── deadline_ns 处理（Plan §P.3.2 #12 (a) backend guard 路径） ─
        // solver 仅响应 None 路径；Some(_) → 防御性 reject（catch caller
        // 误传；详 crate-level doc § deadline_ns 处理）。
        if deadline_ns.is_some() {
            return Err(IkError::InvalidInput {
                reason: "XLeRobotArmIkSolver only supports deadline_ns = None; \
                         use IkBackend backend layer guard for deadline enforcement \
                         (see Plan §P.3.2 #12 (a))"
                    .to_string(),
            });
        }

        // ── 输入校验：NaN / Inf ─────────────────────────────────────
        let [tx, ty, tz] = target.translation;
        if !tx.is_finite() || !ty.is_finite() || !tz.is_finite() {
            return Err(IkError::InvalidInput {
                reason: format!("target.translation contains NaN/Inf: [{tx:?}, {ty:?}, {tz:?}]"),
            });
        }
        let [qx, qy, qz, qw] = target.orientation;
        if !qx.is_finite() || !qy.is_finite() || !qz.is_finite() || !qw.is_finite() {
            return Err(IkError::InvalidInput {
                reason: format!(
                    "target.orientation contains NaN/Inf: \
                     [{qx:?}, {qy:?}, {qz:?}, {qw:?}]"
                ),
            });
        }

        // ── 输入校验：quaternion 规范化 ─────────────────────────────
        let qnorm = qx
            .mul_add(qx, qy.mul_add(qy, qz.mul_add(qz, qw * qw)))
            .sqrt();
        if (qnorm - 1.0).abs() > QUAT_NORM_TOLERANCE {
            return Err(IkError::InvalidInput {
                reason: format!(
                    "target.orientation quaternion not normalized: |q| = {qnorm:.6} \
                     (tolerance = {QUAT_NORM_TOLERANCE})"
                ),
            });
        }

        // ── quat → (roll, pitch, yaw) ───────────────────────────────
        let (roll, pitch, yaw) = quat_to_euler(&target.orientation);

        // ── yaw 处理（Plan §P.3.1 Q-IK6 (a)） ────────────────────────
        // 5-DOF 手臂的 end-effector yaw 由 shoulder_pan 决定；caller
        // 提供的 quat 必须 yaw ≈ 0；超出容差 → InvalidInput。
        if yaw.abs() > YAW_TOLERANCE {
            return Err(IkError::InvalidInput {
                reason: format!(
                    "5-DOF arm cannot represent target yaw {yaw:.6} rad; \
                     caller must encode pitch + roll only in target.orientation \
                     (yaw must be ≈ 0; tolerance = {YAW_TOLERANCE}); \
                     end-effector yaw is geometrically determined by \
                     atan2(target.y, target.x)"
                ),
            });
        }

        // ── 1. shoulder_pan：水平旋转 ───────────────────────────────
        let shoulder_pan_raw = (ty * self.side.sign()).atan2(tx);
        let shoulder_pan = clamp_to_limit(shoulder_pan_raw, &SHOULDER_PAN_LIMIT);

        // ── 2. 投影到臂平面 + 2-DOF IK ─────────────────────────────
        let horizontal_dist = tx.mul_add(tx, ty * ty).sqrt();
        let (shoulder_lift, elbow_flex) = planar_ik(horizontal_dist, tz);

        // ── 3. wrist_flex：末端俯仰补偿 ────────────────────────────
        let (_fk_px, fk_z) = planar_fk(shoulder_lift, elbow_flex);
        let current_pitch_estimate = arm_elevation(horizontal_dist, fk_z);
        let wrist_flex_raw = pitch - current_pitch_estimate;
        let wrist_flex = clamp_to_limit(wrist_flex_raw, &WRIST_FLEX_LIMIT);

        // ── 4. wrist_roll：直接映射 ────────────────────────────────
        let wrist_roll = clamp_to_limit(roll, &WRIST_ROLL_LIMIT);

        // ── 输出（5 关节顺序：shoulder_pan / shoulder_lift /
        //         elbow_flex / wrist_flex / wrist_roll） ───────────
        Ok(IkSolution {
            joints_rad: vec![
                shoulder_pan,
                shoulder_lift,
                elbow_flex,
                wrist_flex,
                wrist_roll,
            ],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Side, XLeRobotArmIkSolver, right_arm_home_duration_ms};
    use robot_kinematics::{ArmIkSolver, IkError, JointLimit, Pose};

    const EPS: f64 = 1e-9;

    /// 单位四元数（无旋转 = pitch + roll + yaw 全 0）。
    const IDENTITY_QUAT: [f64; 4] = [0.0, 0.0, 0.0, 1.0];

    fn assert_within_limit(joint_value: f64, limit: &JointLimit, name: &str) {
        assert!(
            joint_value >= limit.min_rad - EPS && joint_value <= limit.max_rad + EPS,
            "joint {name} = {joint_value:.6} not within [{:.6}, {:.6}]",
            limit.min_rad,
            limit.max_rad,
        );
    }

    fn solver() -> XLeRobotArmIkSolver {
        XLeRobotArmIkSolver::default()
    }

    /// AC-3 lib unit #1: 工作空间内典型点（夹爪正前方 0.15 m）→ Ok +
    /// 所有关节在限位内。
    #[test]
    fn right_arm_home_uses_slowest_axis_and_rounds_up_to_milliseconds() {
        let speeds = [0.01, 0.1, 0.1, 0.2, 0.1];
        assert_eq!(right_arm_home_duration_ms([0.0; 5], speeds), Ok(10_000));
        assert_eq!(
            right_arm_home_duration_ms([0.099, 0.4, 0.1, 0.2, 0.9], speeds),
            Ok(10_000)
        );
        assert_eq!(
            right_arm_home_duration_ms([0.12, 0.4, 0.1, 0.2, 0.9], speeds),
            Ok(12_000)
        );
        assert_eq!(
            right_arm_home_duration_ms([0.12, 0.4, 0.1, 0.2, -2.50001], speeds),
            Ok(25_001)
        );
        for value in [f64::NAN, f64::INFINITY, f64::MAX] {
            assert!(right_arm_home_duration_ms([value; 5], speeds).is_err());
        }
        for speed in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::MIN_POSITIVE] {
            assert!(right_arm_home_duration_ms([1.0; 5], [speed; 5]).is_err());
        }
    }

    #[test]
    fn solve_workspace_interior_returns_ok_with_joints_within_limits() {
        let target = Pose {
            translation: [0.15, 0.0, 0.10],
            orientation: IDENTITY_QUAT,
        };
        let sol = solver()
            .solve(&target, None, None)
            .expect("workspace interior target");
        assert_eq!(sol.joints_rad.len(), 5);

        let info = solver().info().clone();
        for (i, (joint, limit)) in sol
            .joints_rad
            .iter()
            .zip(info.joint_limits.iter())
            .enumerate()
        {
            assert_within_limit(*joint, limit, &info.joint_names[i]);
        }
    }

    /// 几何基准独立于逆解：上臂仰 60°，前臂水平，肩到腕的投影
    /// 分别为 115.9 mm × cos(60°) + 135 mm 和 115.9 mm × sin(60°)。
    #[test]
    fn forward_pose_matches_known_arm_geometry() {
        use std::f64::consts::FRAC_PI_6;
        let joints = [
            FRAC_PI_6,
            0.278_803_361_449_573_63,
            -0.240_273_271_146_262_96,
            0.1,
            -0.4,
        ];
        let radius = 0.192_95;
        let height = 0.100_372_344_298_616_44;
        for (side, direction) in [(Side::Right, 1.0), (Side::Left, -1.0)] {
            let pose = XLeRobotArmIkSolver::new(side)
                .forward_pose(&joints)
                .unwrap();
            let expected = [radius * FRAC_PI_6.cos(), direction * radius * 0.5, height];
            for (actual, expected) in pose.translation.into_iter().zip(expected) {
                assert!((actual - expected).abs() < EPS);
            }
            let (roll, pitch, yaw) = super::quat_to_euler(&pose.orientation);
            assert!((pitch - (height.atan2(radius) + joints[3])).abs() < EPS);
            assert!((roll - joints[4]).abs() < EPS);
            assert!(yaw.abs() < EPS);
        }
    }

    /// 上游 `assets/xlerobot/xlerobot.urdf` 的 `Pitch` 轴到 `Wrist_Pitch` 轴距离只依赖肘角。
    /// 参考值取两段 origin 向量 (0,112.57,28) 和 (0,5.2,134.9) mm；
    /// 后一段先绕 X 转 −1.5708 + (电机肘角 + π/2)，再与前一段相加取模。
    #[test]
    fn shoulder_to_wrist_distance_matches_urdf() {
        let arm = XLeRobotArmIkSolver::new(Side::Right);
        for (elbow_rad, urdf_mm) in [(0.35, 171.938_614), (0.240_690_068_5, 181.616_312)] {
            let joints = [-0.2, 0.25, elbow_rad, -0.1, 0.3];
            let [x, y, z] = arm.forward_pose(&joints).unwrap().translation;
            let distance_mm = x.hypot(y).hypot(z) * 1000.0;
            assert!(
                (distance_mm - urdf_mm).abs() <= 1.0,
                "肘角 {elbow_rad}，肩腕距离 {distance_mm} mm，URDF 参考 {urdf_mm} mm"
            );
        }
    }

    #[test]
    fn forward_pose_rejects_invalid_joint_angles() {
        let arm = solver();
        for len in [0, 4, 6] {
            assert!(arm.forward_pose(&[0.0; 6][..len]).is_err());
        }
        for (index, limit) in arm.info().joint_limits.iter().enumerate() {
            for invalid in [
                limit.min_rad - 1e-4,
                limit.max_rad + 1e-4,
                f64::NAN,
                f64::INFINITY,
            ] {
                let mut joints = [0.0; 5];
                joints[index] = invalid;
                assert!(arm.forward_pose(&joints).is_err());
            }
        }
    }

    /// 三个可达且未饱和的平面目标，在同一几何模型内应只剩浮点误差。
    #[test]
    fn fk_round_trip_2dof_planar() {
        for (radius, height) in [(0.15, 0.10), (0.20, 0.10), (0.16, 0.05)] {
            let (lift, elbow) = super::planar_ik(radius, height);
            let (actual_radius, actual_height) = super::planar_fk(lift, elbow);
            assert!((actual_radius - radius).hypot(actual_height - height) < EPS);
        }
    }

    /// AC-3 lib unit #3: out-of-reach → Ok(clamped within workspace
    /// boundary)（Q-IK16 (a) clamp 路径；solver 永不返
    /// `IkError::OutOfReach`）。
    #[test]
    fn solve_out_of_reach_returns_clamped_ok() {
        // 目标距离 = 2.0 m，远超 L1 + L2 = 0.2509 m
        let target = Pose {
            translation: [2.0, 0.0, 0.0],
            orientation: IDENTITY_QUAT,
        };
        let sol = solver()
            .solve(&target, None, None)
            .expect("out-of-reach should clamp not error");

        // 所有关节在限位内（dimos `_planar_ik` 内部 scale 到 r_max
        // + 限位 clamp）
        let info = solver().info().clone();
        for (i, (joint, limit)) in sol
            .joints_rad
            .iter()
            .zip(info.joint_limits.iter())
            .enumerate()
        {
            assert_within_limit(*joint, limit, &info.joint_names[i]);
        }
    }

    /// AC-3 lib unit #4: yaw != 0 → `Err(IkError::InvalidInput)`
    /// （Q-IK6 (a)）。
    #[test]
    fn solve_nonzero_yaw_returns_invalid_input() {
        // 30° yaw quaternion (绕 Z 轴)
        let yaw_30deg = (std::f64::consts::PI / 6.0) / 2.0; // half-angle for quat
        let target = Pose {
            translation: [0.15, 0.0, 0.10],
            orientation: [0.0, 0.0, yaw_30deg.sin(), yaw_30deg.cos()],
        };
        let err = solver()
            .solve(&target, None, None)
            .expect_err("nonzero yaw should reject");
        match err {
            IkError::InvalidInput { reason } => {
                assert!(
                    reason.contains("yaw"),
                    "InvalidInput reason should mention yaw: {reason}"
                );
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    /// AC-3 lib unit #5: 关节限位 clamp 锁测（target 让 `shoulder_pan`
    /// 应大于 `max_rad` → clamp 到 `max_rad`）。
    #[test]
    fn solve_respects_joint_limits() {
        // shoulder_pan = atan2(y * sign, x); 选 x < 0, y > 0 让 atan2
        // 进 (π/2, π) 区间。如 x = -1.0, y = 1.0:
        //   atan2(1.0, -1.0) = π - π/4 = 3π/4 ≈ 2.356 rad > 2.16
        //   → 撞 SHOULDER_PAN_LIMIT.max_rad，clamp 到 2.16。
        // 注意 `atan2(y, x)` 在 x→0+ 跟 x→0- 都接近 π/2，**不**接近 π；
        // 要逼 atan2 → π 必须 |x| 跟 |y| 同量级且 x 负。
        let target = Pose {
            translation: [-1.0, 1.0, 0.10],
            orientation: IDENTITY_QUAT,
        };
        let sol = solver()
            .solve(&target, None, None)
            .expect("target should clamp not error");

        // shoulder_pan 应被 clamp 到 2.16
        let info = solver().info().clone();
        let shoulder_pan_limit = &info.joint_limits[0];
        let shoulder_pan = sol.joints_rad[0];
        assert!(
            (shoulder_pan - shoulder_pan_limit.max_rad).abs() < EPS,
            "shoulder_pan should be clamped to max_rad = {:.6}, got {shoulder_pan:.6}",
            shoulder_pan_limit.max_rad,
        );

        // 所有关节在限位内
        for (i, (joint, limit)) in sol
            .joints_rad
            .iter()
            .zip(info.joint_limits.iter())
            .enumerate()
        {
            assert_within_limit(*joint, limit, &info.joint_names[i]);
        }
    }

    /// AC-3 lib unit #6: NaN translation → `Err(IkError::InvalidInput)`
    /// （Plan §P.3.2 #10 optional 6 nice-to-have）。
    #[test]
    fn solve_nan_translation_returns_invalid_input() {
        let target = Pose {
            translation: [f64::NAN, 0.0, 0.0],
            orientation: IDENTITY_QUAT,
        };
        let err = solver()
            .solve(&target, None, None)
            .expect_err("NaN translation should reject");
        match err {
            IkError::InvalidInput { reason } => {
                assert!(
                    reason.contains("NaN") || reason.contains("translation"),
                    "InvalidInput reason should mention NaN/translation: {reason}"
                );
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    /// AC-3 lib unit #7: `deadline_ns = Some(_)` → `Err(IkError::
    /// InvalidInput)`（Plan §P.3.2 #12 (a) 防御性 reject + crate-
    /// level doc § `deadline_ns` 处理）。
    #[test]
    fn solve_with_some_deadline_returns_invalid_input() {
        let target = Pose {
            translation: [0.15, 0.0, 0.10],
            orientation: IDENTITY_QUAT,
        };
        let err = solver()
            .solve(&target, None, Some(1_000_000_000))
            .expect_err("Some(deadline_ns) should reject under (a) backend guard path");
        match err {
            IkError::InvalidInput { reason } => {
                assert!(
                    reason.contains("deadline_ns") || reason.contains("backend layer guard"),
                    "InvalidInput reason should mention deadline_ns / backend layer guard: \
                     {reason}"
                );
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    /// AC-3 lib unit #8: `info()` 返 5 关节 + 名 + 限位（`XLeRobot` 关
    /// 节顺序锁测）。
    #[test]
    fn info_has_5_xlerobot_joints() {
        let s = solver();
        let info = s.info();
        assert_eq!(info.joint_count, 5);
        assert_eq!(info.joint_names.len(), 5);
        assert_eq!(info.joint_limits.len(), 5);
        assert_eq!(info.joint_names[0], "shoulder_pan");
        assert_eq!(info.joint_names[1], "shoulder_lift");
        assert_eq!(info.joint_names[2], "elbow_flex");
        assert_eq!(info.joint_names[3], "wrist_flex");
        assert_eq!(info.joint_names[4], "wrist_roll");

        // 限位非空区间
        for (i, limit) in info.joint_limits.iter().enumerate() {
            assert!(
                limit.min_rad < limit.max_rad,
                "joint {} limit min={} max={} should be non-empty",
                info.joint_names[i],
                limit.min_rad,
                limit.max_rad,
            );
        }
    }

    /// AC-3 lib unit #9: 左臂 sign 翻转锁测。
    #[test]
    fn left_arm_shoulder_pan_sign_is_inverted_from_right() {
        let right = XLeRobotArmIkSolver::new(Side::Right);
        let left = XLeRobotArmIkSolver::new(Side::Left);
        let target = Pose {
            translation: [0.15, 0.10, 0.10], // +y
            orientation: IDENTITY_QUAT,
        };
        let right_sol = right.solve(&target, None, None).expect("right arm solve");
        let left_sol = left.solve(&target, None, None).expect("left arm solve");

        // 右臂 sign=+1 → shoulder_pan = atan2(+0.10, 0.15) > 0
        // 左臂 sign=-1 → shoulder_pan = atan2(-0.10, 0.15) < 0
        assert!(right_sol.joints_rad[0] > 0.0, "right shoulder_pan > 0");
        assert!(left_sol.joints_rad[0] < 0.0, "left shoulder_pan < 0");
        assert!(
            (right_sol.joints_rad[0] + left_sol.joints_rad[0]).abs() < EPS,
            "right + left should be ≈ 0 (mirror), got {} + {} = {}",
            right_sol.joints_rad[0],
            left_sol.joints_rad[0],
            right_sol.joints_rad[0] + left_sol.joints_rad[0],
        );
    }
}
