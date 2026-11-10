//! 通用机器人运动学接口与纯类型：定义位姿、速度、雅可比、关节约束和 IK 求解合同。
//!
//! ## 模块边界
//!
//! - 仅依赖 `thiserror` 生成错误实现，不依赖具体机器人、矩阵库或运行时。
//! - [`ArmIkSolver`] 支持 `Box<dyn ArmIkSolver>`，由调用方持有并注入具体求解器。
//! - [`IkSolution`] 仅包含关节弧度；工作空间裁剪与 [`IkError::OutOfReach`] 的
//!   返回策略由求解器实现约定，通用接口不统一该策略。

/// 笛卡尔 pose（translation + quaternion XYZW）。
///
/// quaternion 顺序 `[x, y, z, w]`；normalize 由调用方保证，trait 层
/// **不**校验。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    pub translation: [f64; 3],
    pub orientation: [f64; 4],
}

/// 笛卡尔 twist（linear m/s + angular rad/s）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Twist {
    pub linear: [f64; 3],
    pub angular: [f64; 3],
}

/// row-major spatial Jacobian。
///
/// trait 第一轮**不**强制 `rows == 6` / `data.len() == rows * cols`
/// 等 invariant，仅类型定义；具体语义由 solver 实现负责（第二轮
/// round 由 `robot-kinematics-xlerobot` 等实现 crate 锁住）。
#[derive(Debug, Clone)]
pub struct Jacobian {
    pub rows: usize,
    pub cols: usize,
    pub data: Vec<f64>,
}

/// 单关节 limit（弧度）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JointLimit {
    pub min_rad: f64,
    pub max_rad: f64,
}

/// 运动学元数据（关节数 + 名 + limit）。
///
/// solver 实现自决 lazy / eager 内部布局；trait 层不约束。
#[derive(Debug, Clone)]
pub struct KinematicInfo {
    pub joint_count: usize,
    pub joint_names: Vec<String>,
    pub joint_limits: Vec<JointLimit>,
}

/// IK 解（仅 `joints_rad`）。
///
/// **不**含 `clamped: bool` / `iterations` / `residual_error` 等
/// metadata —— 避免预钉 solver "是否报告 clamp 行为" 的语义（详
/// 模块 doc + Spec AC-6）。
#[derive(Debug, Clone)]
pub struct IkSolution {
    pub joints_rad: Vec<f64>,
}

/// IK 求解错误。
///
/// 5 variant 锁定（Spec AC-4 + Q2(α + `InvalidInput`) ACK）：
///
/// - [`IkError::InvalidInput`]：运行时输入校验（NaN translation /
///   quat 非规范化 / seed 长度 / 配置非法等）。
/// - [`IkError::NoSolution`]：数值法收敛失败 / 闭式解理论边界。
/// - [`IkError::OutOfReach`]：目标 pose 出工作空间（trait 层**可
///   选**返回；solver 也可选 `Ok(clamped)` 路径，详模块 doc）。
/// - [`IkError::SingularityTooClose`]：雅可比病态。
/// - [`IkError::DeadlineExceeded`]：[`ArmIkSolver::solve`] 的
///   `deadline_ns` 超过。
#[derive(Debug, thiserror::Error)]
pub enum IkError {
    #[error("invalid IK input: {reason}")]
    InvalidInput { reason: String },

    #[error("IK numerical solver failed to converge")]
    NoSolution,

    #[error("target pose out of workspace reach (distance {distance_m:.4} m)")]
    OutOfReach { distance_m: f64 },

    #[error("Jacobian singularity too close (condition number {condition_number:.3e})")]
    SingularityTooClose { condition_number: f64 },

    #[error("IK solver deadline exceeded")]
    DeadlineExceeded,
}

/// 笛卡尔目标 → 关节弧度的 IK solver trait。
///
/// # dyn-safety
///
/// 严格 dyn-safe：方法签名不含 generic param（除非 `where Self: Sized`
/// bound）/ 不返 `Self`；可作 `Box<dyn ArmIkSolver>` 存于 S1
/// coordinator（Q6 ACK；编译期锁测见 `arm_ik_solver_is_dyn_safe`
/// 单测）。
///
/// # 多臂支持
///
/// trait **不**暴露 `side` / `arm_id` 字段（Q4(X) ACK）；多臂场景由
/// 调用方持多个 `Box<dyn ArmIkSolver>` instance（每臂一个；跟 dimos
/// `ArmIK(side)` 模式一致）。
///
/// # 错误模型与 clamp 行为
///
/// [`IkError::OutOfReach`] 在本 trait 层**可选** —— solver 实现**可
/// 以**返 `Ok(IkSolution { joints_rad: clamped_to_workspace })` +
/// 配套外部 informational metric，**也可以**返
/// `Err(IkError::OutOfReach { distance_m })`；trait crate 不强制语
/// 义（`XLeRobot` 实施期第二轮 round 拍板，详模块 doc）。
pub trait ArmIkSolver: Send + Sync {
    /// 返回 solver 的运动学元数据（关节数 / 名 / limit）。
    ///
    /// trait 层不约束 lazy / eager 内部布局；solver 实现自决。
    fn info(&self) -> &KinematicInfo;

    /// 求解笛卡尔目标到关节弧度。
    ///
    /// # 参数
    ///
    /// - `target`：目标 pose；trait 层**不**钉死 frame 抽象，frame
    ///   语义由 solver 实现 + 配置文档约定（第二轮 round 跟
    ///   `IkPayload` fbs body 一起拍板，Q12）。
    /// - `seed`：可选 seed（数值法用；闭式解 ignore）。`None` = 无
    ///   seed；`Some(slice)` 长度由 solver 实现按 [`Self::info`] 的
    ///   `joint_count` 校验，违反返 [`IkError::InvalidInput`]。
    /// - `deadline_ns`：可选 deadline（绝对 ns from
    ///   `CLOCK_MONOTONIC_RAW`；语义跟 `robot-core::time::MonoRawNs`
    ///   `(u64)`、`Deadline(MonoRawNs)`、`ActionRequest.deadline_ns`
    ///   `: u64` 仓库统一时间模型对齐；trait crate **不**依赖
    ///   `robot-core`，仅值语义对齐避免 signed conversion 损失精度，
    ///   并避免跨 crate 转换无意义 cast）。`None` = 无 deadline；
    ///   `Some(ns)` = 绝对超时 ns；solver 内部自查 + 返
    ///   [`IkError::DeadlineExceeded`]。
    fn solve(
        &self,
        target: &Pose,
        seed: Option<&[f64]>,
        deadline_ns: Option<u64>,
    ) -> Result<IkSolution, IkError>;
}

#[cfg(test)]
mod tests {
    use super::{
        ArmIkSolver, IkError, IkSolution, Jacobian, JointLimit, KinematicInfo, Pose, Twist,
    };

    /// [`IkError`] exhaustive match 锁测（沿候选 1
    /// `action_kind_three_v04_variants` 同款模式；漏 variant 编译
    /// 期 fail）。
    fn variant_name(e: &IkError) -> &'static str {
        match e {
            IkError::InvalidInput { .. } => "InvalidInput",
            IkError::NoSolution => "NoSolution",
            IkError::OutOfReach { .. } => "OutOfReach",
            IkError::SingularityTooClose { .. } => "SingularityTooClose",
            IkError::DeadlineExceeded => "DeadlineExceeded",
        }
    }

    #[test]
    fn ik_error_variant_exhaustive() {
        assert_eq!(
            variant_name(&IkError::InvalidInput { reason: "x".into() }),
            "InvalidInput",
        );
        assert_eq!(variant_name(&IkError::NoSolution), "NoSolution");
        assert_eq!(
            variant_name(&IkError::OutOfReach { distance_m: 0.1 }),
            "OutOfReach",
        );
        assert_eq!(
            variant_name(&IkError::SingularityTooClose {
                condition_number: 1.0e6,
            }),
            "SingularityTooClose",
        );
        assert_eq!(variant_name(&IkError::DeadlineExceeded), "DeadlineExceeded",);
    }

    #[test]
    fn pose_twist_jacobian_pod_constructable() {
        // f64 字段用 abs <= 1e-12 检查（沿 robot-health / robot-s0
        // 现有 `(a - b).abs() < <ε>` 模式；规避 clippy::float_cmp 对
        // 数组 `assert_eq!` 的严格相等告警）。
        const EPS: f64 = 1e-12;

        let pose = Pose {
            translation: [0.0; 3],
            orientation: [0.0, 0.0, 0.0, 1.0],
        };
        assert!(pose.translation.iter().all(|x| x.abs() < EPS));
        assert!(pose.orientation[0..3].iter().all(|x| x.abs() < EPS));
        assert!((pose.orientation[3] - 1.0).abs() < EPS);

        let twist = Twist {
            linear: [0.0; 3],
            angular: [0.0; 3],
        };
        assert!(twist.linear.iter().all(|x| x.abs() < EPS));
        assert!(twist.angular.iter().all(|x| x.abs() < EPS));

        let jac = Jacobian {
            rows: 6,
            cols: 3,
            data: vec![0.0; 18],
        };
        assert_eq!(jac.rows, 6);
        assert_eq!(jac.cols, 3);
        assert_eq!(jac.data.len(), jac.rows * jac.cols);
        assert!(jac.data.iter().all(|x| x.abs() < EPS));

        let info = KinematicInfo {
            joint_count: 1,
            joint_names: vec!["j0".into()],
            joint_limits: vec![JointLimit {
                min_rad: -1.0,
                max_rad: 1.0,
            }],
        };
        assert_eq!(info.joint_count, info.joint_names.len());
        assert_eq!(info.joint_count, info.joint_limits.len());
        assert!((info.joint_limits[0].min_rad + 1.0).abs() < EPS);
        assert!((info.joint_limits[0].max_rad - 1.0).abs() < EPS);

        let sol = IkSolution {
            joints_rad: vec![0.0],
        };
        assert_eq!(sol.joints_rad.len(), info.joint_count);
        assert!(sol.joints_rad.iter().all(|x| x.abs() < EPS));
    }

    /// 编译期 bound check —— 若 [`ArmIkSolver`] 非 dyn-safe，本函数
    /// 签名编译 fail（沿候选 1 同款 monomorphize 锁测模式）。
    fn assert_dyn_safe<T: ?Sized + ArmIkSolver>() {}

    #[test]
    fn arm_ik_solver_is_dyn_safe() {
        // 强制 monomorphize `assert_dyn_safe::<dyn ArmIkSolver>` ——
        // 若 trait 引入 `Self` 返回值 / generic method 等 dyn-unsafe
        // 元素，本行编译 fail（trait object 不可构）。
        assert_dyn_safe::<dyn ArmIkSolver>();
    }

    struct StubSolver {
        info: KinematicInfo,
    }

    impl ArmIkSolver for StubSolver {
        fn info(&self) -> &KinematicInfo {
            &self.info
        }
        fn solve(
            &self,
            _target: &Pose,
            _seed: Option<&[f64]>,
            _deadline_ns: Option<u64>,
        ) -> Result<IkSolution, IkError> {
            Ok(IkSolution {
                joints_rad: vec![0.0; self.info.joint_count],
            })
        }
    }

    #[test]
    fn arm_ik_solver_trait_object_callable() {
        let stub: Box<dyn ArmIkSolver> = Box::new(StubSolver {
            info: KinematicInfo {
                joint_count: 3,
                joint_names: vec!["a".into(), "b".into(), "c".into()],
                joint_limits: vec![
                    JointLimit {
                        min_rad: -1.0,
                        max_rad: 1.0,
                    };
                    3
                ],
            },
        });
        let target = Pose {
            translation: [0.0; 3],
            orientation: [0.0, 0.0, 0.0, 1.0],
        };
        let sol = stub.solve(&target, None, None).expect("stub solver");
        assert_eq!(sol.joints_rad.len(), 3);
        assert_eq!(stub.info().joint_count, 3);
    }
}
