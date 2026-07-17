//! client 侧共享语义模块（v0.5 候选 E 段(4) 首立）。
//!
//! 放 **robot-rpc lib**（PD-E3：共享语义单源；robot-tests / e2e harness /
//! 后续 `dual_arm_workflow`〔段(7)〕都从这里 import，禁止各自复制）。
//! 本模块**不**依赖 tonic `client` feature——只消费 prost message 类型
//! （server-only build 亦生成）+ toml + `robot_msg::joint_layout`。

pub mod layout;
