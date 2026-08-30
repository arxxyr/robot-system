//! **部署件**：真机交接 fail-closed 启动链（v0.5 候选 E 段(9)，Spec D13 +
//! P.2.9；PD-E2(a) 宿主 = robot-s0 lib + `xlerobot_deploy` bin）。
//!
//! ## 为什么需要它
//!
//! arm-only S0 **不驱动也不能 `EStop`** head/base 电机，而旧栈退出只
//! best-effort 断扭且吞失败——切换窗口内 head/wheels 的扭矩/速度状态**未知**
//! （D13）。且 `auto_enable=false` 只防新 S0 自动上扭，**不消除崩溃残留扭矩**
//! （Feetech `connect()` 不写 `Torque_Enable=0`）。故真机启动任一 arm-only S0
//! **之前**必须对该 bus 全部电机做 ensure-safe，失败即 **fail-closed 禁止切换**。
//!
//! ## 模块分工
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`types`] | [`DeployArm`] 与本 bus 电机集（robot-s0 本地定义，**不**依赖 `robot-kinematics-xlerobot::Side`，R9-1） |
//! | [`identity`] | **D2 身份绑定校验**（arm ↔ config；打开串口前 fail-closed，round 1 P0-1） |
//! | [`counter`] | guard 连败预算：`<robot_id>.count` 原子读写 + `<robot_id>.lock` **稳定 inode** flock（r10 AA2/AA3） |
//! | [`provision`] | latch 三分支 provision + 统一 dir-fsync 最终步骤（D2 第 1 条 / AC-E21） |
//! | [`preflight`] | `DeployTransport` trait + `ensure_safe_arm` / `ensure_safe_handoff`（AC-E16） |
//! | [`guard`] | pre-increment 六步 + 退出码映射（AC-E22） |
//!
//! ## feature 边界（段(9) 形态补充，登记 Spec P.4）
//!
//! Spec P.2.9 写"`xlerobot-real` feature 域内"。实施时**只把真串口实现与
//! bin 放进该 feature**（`preflight::feetech` / `[[bin]] xlerobot_deploy`），
//! 纯逻辑（types / identity / counter / provision / preflight trait 与聚合
//! 算法 / guard）
//! **不加 feature 门**——否则 AC-E16 / E21 / E22 的 unit 判据在默认 gate
//! （`cargo test --workspace`，default = `xlerobot` 不含 `real-hw`）里根本不
//! 编译、更不会跑到。真硬件依赖（serialport / libudev）仍只由
//! `xlerobot-real` 拉入。

pub mod counter;
pub mod guard;
pub mod identity;
pub mod preflight;
pub mod provision;
pub mod types;

pub use types::{DeployArm, HANDOFF_WRITE_COUNT};
