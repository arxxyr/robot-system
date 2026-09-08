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
use std::sync::Arc;
use std::time::Duration;

use robot_core::adapter::{ManipulatorAdapter, SerialRtAdapter};
use robot_core::error::RobotError;
use robot_core::factory::{AdapterBuildContext, AdapterFactory};
use robot_core::time::{RobotClock, SystemClock};

use crate::adapter::{FeetechAdapter, FeetechAdapterConfig};
use crate::bus::FeetechBus;
use crate::calibration_io::load_and_merge;
use crate::registers::DEFAULT_BAUDRATE;
use crate::transport::real::RealSerialTransport;
use crate::types::{Motor, MotorCalibration};

/// task 28 真硬件 factory。
pub struct FeetechFactory;

impl FeetechFactory {
    /// 内部 helper：跑完整的"address / motor_ids / 串口 / motors / calibration /
    /// adapter new" 流水线，返回**concrete** `FeetechAdapter`。`build` 与
    /// `build_serial_rt` 各自把它装进对应的 `Box<dyn …>`。
    ///
    /// `path_kind` 仅用于日志（`"" / " (SerialRT)"`），帮诊断哪条路径触发本次构造。
    fn build_concrete(
        ctx: &AdapterBuildContext<'_>,
        path_kind: &'static str,
    ) -> Result<FeetechAdapter<RealSerialTransport>, RobotError> {
        // ---- 1) address ----
        let address = ctx
            .address
            .ok_or(RobotError::InvalidParameter("feetech: missing address"))?;

        let motor_ids = resolve_motor_ids(ctx)?;

        // ---- 3) 打开真串口 ----
        let clock: Arc<dyn RobotClock> = ctx.services.map_or_else(
            || Arc::new(SystemClock::new()) as Arc<dyn RobotClock>,
            |services| Arc::clone(&services.clock),
        );
        let call_deadline = ctx.services.map_or(Duration::from_millis(50), |services| {
            Duration::from_nanos(services.call_deadline.0)
        });
        let transport = RealSerialTransport::open_with_clock(
            Path::new(address),
            DEFAULT_BAUDRATE,
            Arc::clone(&clock),
        )
        .map_err(|e| {
            tracing::error!(bus_address = %address, ?e, "feetech open serial failed");
            RobotError::from(e)
        })?;

        // ---- 4) 构造 motors HashMap（name → Motor{id}） ----
        let mut motors = HashMap::with_capacity(ctx.joint_names.len());
        for (name, id) in ctx.joint_names.iter().zip(motor_ids.iter()) {
            let motor = Motor::new(*id).map_err(|e| {
                tracing::error!(motor_id = id, joint = %name, ?e, "feetech Motor::new failed");
                RobotError::from(e)
            })?;
            motors.insert(name.clone(), motor);
        }
        let bus = FeetechBus::new(transport, motors).map_err(|e| {
            tracing::error!(?e, "feetech FeetechBus::new failed");
            RobotError::from(e)
        })?;

        // ---- 5) 加载 calibration（task 29 commit 3）----
        //
        // 空 calibration_paths = 不加载 = adapter 无 silent clamp；非空则
        // load_and_merge 所有文件 → 按 joint_names 顺序挑出 MotorCalibration。
        // 任一 joint 缺标定 → fail-fast（防 left_arm.json 漏 wrist_flex 一类错配）。
        let calibrations: Vec<MotorCalibration> = if ctx.calibration_paths.is_empty() {
            Vec::new()
        } else {
            let merged = load_and_merge(ctx.calibration_paths).map_err(|e| {
                tracing::error!(
                    calibration_paths = ?ctx.calibration_paths,
                    ?e,
                    "feetech calibration load_and_merge failed"
                );
                RobotError::from(e)
            })?;
            let mut out = Vec::with_capacity(ctx.joint_names.len());
            for name in ctx.joint_names {
                if let Some(cal) = merged.get(name) {
                    out.push(*cal);
                } else {
                    tracing::error!(
                        joint = %name,
                        calibration_paths = ?ctx.calibration_paths,
                        available = ?merged.keys().collect::<Vec<_>>(),
                        "feetech calibration: joint not found in any file"
                    );
                    return Err(RobotError::InvalidParameter(
                        "feetech: calibration missing motor",
                    ));
                }
            }
            out
        };

        // ---- 6) 构造 adapter ----
        let has_calibration = !calibrations.is_empty();
        let adapter = FeetechAdapter::new(
            FeetechAdapterConfig {
                clock,
                preempt: ctx.services.and_then(|services| services.preempt.clone()),
                diag: ctx.services.and_then(|services| services.diag.clone()),
                call_deadline,
                adapter_name: "feetech_sts3215",
                joint_names: ctx.joint_names.to_vec(),
                calibrations,
            },
            bus,
        )
        .map_err(|e| {
            tracing::error!(?e, "feetech FeetechAdapter::new failed");
            RobotError::from(e)
        })?;

        tracing::info!(
            adapter = "feetech_sts3215",
            path_kind,
            instance = %ctx.instance_name,
            bus_address = %address,
            motors = ctx.joint_names.len(),
            motor_ids = ?motor_ids,
            calibration = if has_calibration { "loaded" } else { "none" },
            "feetech factory: built adapter"
        );
        Ok(adapter)
    }
}

/// 参数解析在打开物理 transport 之前完成，保留原有缺省 ID 策略。
fn resolve_motor_ids(ctx: &AdapterBuildContext<'_>) -> Result<Vec<u8>, RobotError> {
    if ctx.motor_ids.is_empty() {
        return (0..ctx.joint_names.len())
            .map(|index| u8::try_from(index + 1))
            .collect::<Result<_, _>>()
            .map_err(|_| RobotError::InvalidParameter("feetech: joint count > 255"));
    }
    if ctx.motor_ids.len() != ctx.joint_names.len() {
        tracing::error!(
            motor_ids_len = ctx.motor_ids.len(),
            joint_names_len = ctx.joint_names.len(),
            "feetech factory: motor_ids length mismatch joints"
        );
        return Err(RobotError::InvalidParameter(
            "feetech: motor_ids length mismatch joints",
        ));
    }
    Ok(ctx.motor_ids.to_vec())
}

impl AdapterFactory for FeetechFactory {
    fn name(&self) -> &'static str {
        "feetech_sts3215"
    }

    fn build(
        &self,
        ctx: &AdapterBuildContext<'_>,
    ) -> Result<Box<dyn ManipulatorAdapter + Send>, RobotError> {
        Ok(Box::new(Self::build_concrete(ctx, "")?))
    }

    /// **task-30 v9 / M7 阶段 4 第二步**：`FeetechFactory` 是 `RtClass::SerialRT`
    /// 路径，必须 override `build_serial_rt`。`FeetechAdapter` 编译期已实现
    /// `OutputSafetyProbe`（M3），与 `ManipulatorAdapter` 合成
    /// [`SerialRtAdapter`]，blanket impl 直接满足。
    fn build_serial_rt(
        &self,
        ctx: &AdapterBuildContext<'_>,
    ) -> Result<Box<dyn SerialRtAdapter + Send>, RobotError> {
        Ok(Box::new(Self::build_concrete(ctx, " (SerialRT)")?))
    }
}

inventory::submit! { &FeetechFactory as &dyn AdapterFactory }
