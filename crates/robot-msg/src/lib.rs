//! # robot-msg
//!
//! `FlatBuffers` schema 定义 + flatc 生成代码 + `schema_id` registry。
//!
//! - 生成代码由 `build.rs` 调 vendor flatc（`3rd-party/flatc/<host>/flatc`）
//!   以 `--rust-module-root-file` 模式输出到 `$OUT_DIR/gen/`；`build.rs` 扫描
//!   产物自己生成 `$OUT_DIR/gen_root.rs` 入口（flatc 自带的 `mod.rs` 只会挂
//!   最后一个 schema，不可用）。
//! - 对外命名空间：
//!   - [`msg`] —— 当前版本（`robot.msg.*`）
//!   - [`v0`]  —— compat 用 v0 快照（`robot.msg.v0.*`；flatc 把数字开头转成 `v_0`）
//! - [`schema_id`] —— `schema_id` 常量 + 查表 API（对应 `docs/00_design.md` §2.2）。
//! - 静态校验由独立 bin `cargo run -p robot-msg --bin schema_lint` 提供。
//! - 运行时 old-reader-new-writer / new-reader-old-writer 交叉见 `tests/compat.rs`。
//!
//! ## RT 约束
//!
//! - **reader 路径零 alloc 零拷贝**：解析直接借用 iceoryx2 sample buffer。
//! - `FlatBufferBuilder` 的构造**严禁**进 RT thread（`00_design.md` §0）；
//!   publishers 必须在 RT 线程外准备 sample。

pub mod capability;
pub mod markers;
pub mod schema_id;
pub mod size_tier;

// ---------------------------------------------------------------------------
// flatc 生成代码入口
// ---------------------------------------------------------------------------
//
// 结构（由 build.rs 写 gen_root.rs；include 到本 wrapper mod 内）：
//
//   pub mod robot {
//       pub mod msg {
//           mod <type>_generated;   // #[path = "<OUT_DIR>/gen/robot/msg/<type>_generated.rs"]
//           pub use self::<type>_generated::*;   // × N
//           pub mod v_0 {
//               mod joint_state_generated;
//               pub use self::joint_state_generated::*;
//           }
//       }
//   }
//
// 所有 flatc 生成的 clippy/rustc 噪音在这个 wrapper mod 一次性 allow；
// `[workspace.lints] pedantic = warn` 不会渗透到 include! 进来的代码。
#[rustfmt::skip]
#[allow(
    clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction,
    warnings, dead_code, unused_imports,
    non_camel_case_types, non_snake_case, non_upper_case_globals,
    missing_docs, unreachable_pub,
)]
mod flatc_gen {
    include!(concat!(env!("OUT_DIR"), "/gen_root.rs"));
}

// ---------------------------------------------------------------------------
// 对外命名空间
// ---------------------------------------------------------------------------

/// 当前版本所有 `robot.msg.*` 类型（FlatBuffers table / enum / builder args）。
///
/// 具体导出项由 `build.rs` 扫描的 schema 决定；新增 schema 自动并入。
pub use crate::flatc_gen::robot::msg;

/// v0 快照命名空间（compat 测试用）。
///
/// 源自 `schemas/old/joint_v0.fbs` 的 `robot.msg.v0`；flatc 把数字开头
/// ident 转义成 `v_0`，本模块对外暴露时改用更干净的 `v0`。v0 里**目前只有
/// `JointState`**，直接 re-export 该 generated mod；未来添加更多 v0 类型时
/// 改为 `pub mod v0 { pub use crate::flatc_gen::robot::msg::v_0::*; }`
/// + 相应 glob 规避（见 `build.rs` 生成策略注释）。
pub use crate::flatc_gen::robot::msg::v_0::joint_state_generated as v0;
