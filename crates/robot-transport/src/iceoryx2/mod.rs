//! `iceoryx2` 后端（§2.1）。Step 5 分阶段上：
//!
//! - **本 commit**：[`service_name`] + [`qos`] 纯类型层，不含真实 pub/sub。
//! - **后续 commit**：`attributes` / `publisher` / `subscriber` / `event`
//!   wrapper。
//!
//! ### 设计决策
//!
//! 决策冻结在 `docs/CHANGELOG.md` 2026-04-22 "Step 5 开工决策与 iceoryx2
//! 依赖登记"：
//!
//! 1. 版本精确 pin `iceoryx2 = "=0.8.1"`（升级必重跑 `/tmp` 烟测）。
//! 2. topic 字符串原样当 `ServiceName`（方案 A），§2.3 命名空间保持不变。
//! 3. `QoS` 按 §2.4 九类映射到 iceoryx2 builder 选项（见 [`qos`]）。
//! 4. sample 上限按 [`robot_msg::size_tier::SizeTier`] 四档。
//! 5. `schema_id` / `schema_version` 作为 service attribute 双道防线
//!    （attribute 是硬约束，mismatch open 被拒绝）。
//!
//! ### RT 约束
//!
//! - service / publisher / subscriber 构造**不得进 RT thread**（涉及 shm 申请
//!   / 锁 / 文件系统）。构造必须在 `s0_ipc_thread` 启动期 / `s1_coordinator`
//!   non-RT 路径上一次性完成。
//! - 运行期 `publisher.loan_uninit` / `subscriber.receive` 零拷贝零 alloc，
//!   RT thread 可直接调（但按 §1.2 S0 RT thread 仍不直接碰 iceoryx2，
//!   走 `s0_ipc_thread` 代理）。

pub mod qos;
pub mod service_name;

pub use qos::{QosProfile, TopicClass, profile_of};
pub use service_name::topic_to_service_name;
