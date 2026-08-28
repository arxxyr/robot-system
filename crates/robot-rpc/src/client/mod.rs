//! client 侧共享语义模块（v0.5 候选 E 段(4) 首立）。
//!
//! 放 **robot-rpc lib**（PD-E3：共享语义单源；robot-tests / e2e harness /
//! 后续 `dual_arm_workflow`〔段(7)〕都从这里 import，禁止各自复制）。
//! 本模块**不**依赖 tonic `client` feature——只消费 prost message 类型
//! （server-only build 亦生成）+ toml + `robot_msg::joint_layout`。
//!
//! - [`layout`]（段(4)）：config 派生关节表 + `RobotState.layout_id` 硬校验。
//! - [`trajectory`]（段(6)，D7/D8/D11）：`JointTrajectory` 形状 →
//!   `ScriptedPayload` waypoints（rad-native；name→index / 时长差分 / 首点
//!   宽容校验 / 缺位填 current）+ fbs 构造。
//! - [`dual_arm_workflow`]（段(7)，D4/D5 + AC-E14）：双臂成对提交协议
//!   （并发提交 + Ack 偏差 gate / 终态精确集 / 提交期与执行期部分失败 cancel
//!   / 不收敛双 EStop / `GetTaskStatus` 收敛）——RPC 通路经 `ArmEndpoint`
//!   trait 注入，e2e 与真机验收 bin 同向消费。
//! - [`baseline_shim`]（段(6)，D11）：冻结 12 维原始形态（臂 度 + 夹爪
//!   `[0,100]`）→ rad 导入 shim（测试侧；左右夹爪 rad 范围由调用方经
//!   feetech 公共换算 API 派生后传入，本 crate 不复制公式）。

pub mod baseline_shim;
pub mod dual_arm_workflow;
pub mod layout;
pub mod trajectory;
