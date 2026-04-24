//! `FeetechFactory` —— `inventory` 自注册的真硬件 factory。
//!
//! **仅 `--features real-hw` 编译**。task 27b 已落 inventory 机制（`MockFactory`
//! 自注册）；本文件给 feetech 加同模式注册，task 28 真机阶段 s0_daemon 按
//! `[[hardware]] adapter = "feetech_sts3215"` 字面查到本 factory 并实例化。
//!
//! ## context 字段消费
//!
//! - `joint_names`：每个 joint 一个 motor，按数组顺序对齐 `motor_ids`。
//! - `motor_ids`：必须长度等于 `joint_names`，**或** 空数组（fallback 到
//!   `1..=N`）。dimos bus1 用 ID 1-8 / bus2 用 ID 1-9，需要在 TOML 显式写。
//! - `address`：`/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C12...` 推荐；
//!   ttyACM* 也接受但不稳定。**必填**。
//! - `instance_name` / `adapter_kind`：仅日志用。

#![cfg(feature = "real-hw")]

use std::collections::HashMap;
use std::path::Path;

use robot_core::adapter::ManipulatorAdapter;
use robot_core::error::RobotError;
use robot_core::factory::{AdapterBuildContext, AdapterFactory};

use crate::adapter::{FeetechAdapter, FeetechAdapterConfig};
use crate::bus::FeetechBus;
use crate::registers::DEFAULT_BAUDRATE;
use crate::transport::real::RealSerialTransport;
use crate::types::Motor;

/// task 28 真硬件 factory。
pub struct FeetechFactory;

impl AdapterFactory for FeetechFactory {
    fn name(&self) -> &'static str {
        "feetech_sts3215"
    }

    fn build(
        &self,
        ctx: &AdapterBuildContext<'_>,
    ) -> Result<Box<dyn ManipulatorAdapter + Send>, RobotError> {
        // ---- 1) address ----
        let address = ctx
            .address
            .ok_or(RobotError::InvalidParameter("feetech: missing address"))?;

        // ---- 2) motor_ids ----
        // 缺省策略：长度 0 → fallback 1..=joint_names.len()
        // 显式：长度必须 == joint_names.len()
        let motor_ids: Vec<u8> = if ctx.motor_ids.is_empty() {
            (0..ctx.joint_names.len())
                .map(|i| u8::try_from(i + 1))
                .collect::<Result<_, _>>()
                .map_err(|_| RobotError::InvalidParameter("feetech: joint count > 255"))?
        } else if ctx.motor_ids.len() != ctx.joint_names.len() {
            eprintln!(
                "fatal: feetech factory: motor_ids.len()={} != joint_names.len()={}",
                ctx.motor_ids.len(),
                ctx.joint_names.len()
            );
            return Err(RobotError::InvalidParameter(
                "feetech: motor_ids length mismatch joints",
            ));
        } else {
            ctx.motor_ids.to_vec()
        };

        // ---- 3) 打开真串口 ----
        let transport =
            RealSerialTransport::open(Path::new(address), DEFAULT_BAUDRATE).map_err(|e| {
                eprintln!("fatal: feetech open serial {address}: {e}");
                RobotError::from(e)
            })?;

        // ---- 4) 构造 motors HashMap（name → Motor{id}） ----
        let mut motors = HashMap::with_capacity(ctx.joint_names.len());
        for (name, id) in ctx.joint_names.iter().zip(motor_ids.iter()) {
            let motor = Motor::new(*id).map_err(|e| {
                eprintln!("fatal: feetech motor id={id}: {e}");
                RobotError::from(e)
            })?;
            motors.insert(name.clone(), motor);
        }
        let bus = FeetechBus::new(transport, motors).map_err(|e| {
            eprintln!("fatal: feetech bus new: {e}");
            RobotError::from(e)
        })?;

        // ---- 5) 构造 adapter ----
        //
        // task 29 commit 2：`calibrations: Vec::new()` 表示无标定 —— silent
        // clamp 不启用，limits 保持 ±π 默认。commit 3 会扩这里从
        // ctx.extras / 新字段读 calibration_path，加载 JSON → 按 joint_names
        // 顺序取 MotorCalibration，传给 adapter。
        let adapter = FeetechAdapter::new(
            FeetechAdapterConfig {
                adapter_name: "feetech_sts3215",
                joint_names: ctx.joint_names.to_vec(),
                calibrations: Vec::new(),
            },
            bus,
        )
        .map_err(|e| {
            eprintln!("fatal: feetech adapter new: {e}");
            RobotError::from(e)
        })?;

        eprintln!(
            "feetech factory: built '{}' adapter on {} (motors {} | ids {:?})",
            ctx.instance_name,
            address,
            ctx.joint_names.len(),
            motor_ids
        );
        Ok(Box::new(adapter))
    }
}

inventory::submit! { &FeetechFactory as &dyn AdapterFactory }
