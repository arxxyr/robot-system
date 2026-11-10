//! # `robot-adapter-feetech`
//!
//! Feetech STS3215（型号 777）的协议、串口与设备适配。
//!
//! 协议、寄存器、编码与校准沿 dimos / lerobot 的 `feetech_io` 约定移植；
//! Rust 直接实现 packet、checksum、ping/read/write/sync_read/sync_write，
//! 不链接 `scservo_sdk` 或 lerobot。
//!
//! - [`transport::SerialTransport`] 提供传输边界，内存实现用于离线测试；
//!   `real-hw` 启用真实串口与 `factory` 静态注册。
//! - [`FeetechBus`] 处理寄存器操作；[`FeetechAdapter`] 复用 `robot-core` 的
//!   `ManipulatorAdapter` 与 `OutputSafetyProbe`，满足 `SerialRtAdapter` 合同。
//! - adapter 的同步串口调用由 S0 的 `s0_serial_io_thread` 独占执行，
//!   不挂 `s0_rt_thread`，也不在 adapter 内另起后台 I/O 线程。
//! - [`calibration_io`] 加载既有校准，位置换算统一复用 [`convert`]。
//!   底盘混合路径须显式选择既有 raw 合同，不把 raw 速度冒充 SI 速度。
//!
//! 当前只适配 STS3215；寄存器可读写性及诊断限制以 [`registers`] 为准。
//! 单元测试和维护工具的结果不代替默认 S0 的实际动作与停止验收。

#![forbid(unused_must_use)]
// 协议层中英文混排 + raw 字节 ↔ unsigned ↔ signed 的 cast 极频繁，单条 allow
// 散打不如 crate 级集中。其余 pedantic（field_reassign / manual_let_else /
// single_match_else 等）仍由 workspace 默认 deny。
#![allow(
    clippy::doc_markdown,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::cast_lossless,
    clippy::decimal_bitwise_operands,
    clippy::float_cmp
)]

// 只启用在本库测试二进制；生产及独立 integration test 的 allocator 不变。
#[cfg(test)]
#[global_allocator]
static TEST_ALLOCATOR: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

pub mod adapter;
pub mod bus;
pub mod calibration;
pub mod calibration_io;
pub mod convert;
pub mod encoding;
pub mod error;
#[cfg(feature = "real-hw")]
pub mod factory;
pub mod protocol;
pub mod registers;
pub mod transport;
pub mod types;
pub mod velocity_profile;

pub use adapter::FeetechAdapter;
pub use bus::FeetechBus;
pub use calibration_io::{clamp_raw, load_and_merge, load_file};
pub use convert::{RAD_PER_TICK, rad_to_raw, raw_to_rad};
pub use error::FeetechError;
#[cfg(feature = "real-hw")]
pub use factory::FeetechFactory;
pub use registers::{
    DEFAULT_BAUDRATE, MODEL_NUMBER_STS3215, OperatingMode, STS3215_MAX_POSITION,
    STS3215_RESOLUTION, STS3215_SIGN_BIT, TorqueMode, get_register, is_eeprom_register,
};
pub use transport::SerialTransport;
pub use types::{Motor, MotorCalibration};
