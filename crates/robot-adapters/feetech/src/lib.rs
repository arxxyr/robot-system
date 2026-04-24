//! # `robot-adapter-feetech`
//!
//! Feetech STS3215（Phase 0）系列 servo 的 Rust adapter，对应
//! `docs/01_implementation_steps.md` Step 10 + Step 7 task 27/28。
//!
//! ## 与 dimos / lerobot 的关系
//!
//! - **协议参考**：`/home/loosqk/repo/robot/dimos-deploy/src/xlerobot_dimos/common/feetech_io/`
//!   （`bus.py` / `registers.py` / `encoding.py` / `calibration.py` /
//!   `patches.py` / `types.py`）+ `dimos-deploy/docs/06_lerobot_replace_spec.md`
//!   是 *字面级* 移植蓝本。
//! - **不**链接 `scservo_sdk` 或 lerobot —— Rust 端直接实现 wire protocol（packet
//!   头 + checksum + ping/read/write/sync_read/sync_write）。
//! - **patch_setPacketTimeout 不需要**：dimos 那个 patch 是 scservo_sdk 自己的 bug
//!   修补，Rust 端不走 sdk，timeout 计算直接走标准 `std::time::Duration` /
//!   `serialport::SerialPort::timeout`。
//!
//! ## v0.2 task 27 范围（**本 crate 当前内容**）
//!
//! 1. `registers` —— STS3215 控制表 + sign-magnitude 表 + 常量 + enum
//! 2. `encoding` —— sign-magnitude 编解码 + LE byte 拆合
//! 3. `protocol` —— wire packet build/parse（字节级）
//! 4. `transport` —— `SerialTransport` trait + 内存 mock 实现（仅供单测）
//! 5. `bus` —— `FeetechBus` 公开 API：connect / ping / read / write / sync_*
//! 6. `calibration` —— write_calibration / reset / set_half_turn_homings
//! 7. `types` —— Motor / MotorCalibration 强类型
//! 8. `error` —— `FeetechError` enum（thiserror）
//! 9. `adapter` —— `FeetechAdapter` 实现 `ManipulatorAdapter` trait
//!
//! 单元测试覆盖 1-7（mock transport 喂虚假回包，验证字节级正确性 + 编解码
//! roundtrip）。
//!
//! ## v0.2 task 27 *不*做的事
//!
//! - **真 USB-RS485 驱动**：留 task 28，加 `feature = "real-hw"` 时把
//!   `serialport` crate 拉进来，实现 `SerialTransport` for 真串口。
//! - **后台 IO 线程模型**：task 27 的 `FeetechAdapter::read_state` /
//!   `write_command` 是 *同步*调用（在调用线程内做完整 wire RTT）。RT 铁律
//!   要求 < 100 µs，串口 RTT 在 ms 量级 → **不能直接挂 `s0_rt_thread`**。
//!   task 28 真机实测后，根据 latency 决定是否引入 "RT thread + 后台 IO 线程"
//!   双层架构（adapter 内部跑 50-100 Hz IO 循环，trait 方法只读 atomic
//!   snapshot）。当前 v0.2 版本可用于离线校准 / 单元 smoke / s2 工具脚本。
//! - **多 bus 多 motor model**：仅 STS3215，model_number=777 单一种；其他型号
//!   （SMS / SCS）需要时新增 model 表，wire protocol 框架可复用。
//! - **完整 EEPROM 寄存器**：只列 dimos / lerobot 实际用到的 14 项；其他
//!   寄存器（PWM_Limit / Acceleration / etc.）按需补充。

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

pub mod adapter;
pub mod bus;
pub mod calibration;
pub mod calibration_io;
pub mod encoding;
pub mod error;
#[cfg(feature = "real-hw")]
pub mod factory;
pub mod protocol;
pub mod registers;
pub mod transport;
pub mod types;

pub use adapter::FeetechAdapter;
pub use bus::FeetechBus;
pub use calibration_io::{clamp_raw, load_and_merge, load_file};
pub use error::FeetechError;
#[cfg(feature = "real-hw")]
pub use factory::FeetechFactory;
pub use registers::{
    DEFAULT_BAUDRATE, MODEL_NUMBER_STS3215, OperatingMode, STS3215_MAX_POSITION,
    STS3215_RESOLUTION, STS3215_SIGN_BIT, TorqueMode, get_register, is_eeprom_register,
};
pub use transport::SerialTransport;
pub use types::{Motor, MotorCalibration};
