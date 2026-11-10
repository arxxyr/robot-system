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
            for (name, &id) in ctx.joint_names.iter().zip(&motor_ids) {
                if let Some(cal) = calibration_for_joint(&merged, name, id)? {
                    out.push(cal);
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

/// 轮校准只在既有四种轮键中按配置ID唯一取整条记录；臂仍按名称和ID核对。
fn calibration_for_joint(
    calibrations: &HashMap<String, MotorCalibration>,
    name: &str,
    motor_id: u8,
) -> Result<Option<MotorCalibration>, RobotError> {
    if matches!(
        name,
        "base_front_wheel" | "base_left_wheel" | "base_right_wheel"
    ) {
        if !(7..=9).contains(&motor_id) {
            return Err(RobotError::InvalidParameter("配置轮电机ID必须在7到9内"));
        }
        let mut matches = [
            "base_front_wheel",
            "base_back_wheel",
            "base_left_wheel",
            "base_right_wheel",
        ]
        .into_iter()
        .filter_map(|key| calibrations.get(key).copied())
        .filter(|cal| cal.id == motor_id);
        let calibration = matches
            .next()
            .ok_or(RobotError::InvalidParameter("配置轮电机ID缺少校准记录"))?;
        if matches.next().is_some() {
            return Err(RobotError::InvalidParameter("配置轮电机ID存在重复校准记录"));
        }
        return Ok(Some(calibration));
    }
    let current = calibrations.get(name).copied();
    if current.is_some_and(|cal| cal.id != motor_id) {
        return Err(RobotError::InvalidParameter("校准名称与电机ID不匹配"));
    }
    Ok(current)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_calibration_follows_configured_id_without_relabeling_records() {
        let front = MotorCalibration::new(8, 1, -12, 15, 4000).unwrap();
        let left = MotorCalibration::new(9, 0, 23, 10, 4090).unwrap();
        let right = MotorCalibration::new(7, 1, -31, 20, 3990).unwrap();
        for front_key in ["base_front_wheel", "base_back_wheel"] {
            let calibrations = HashMap::from([
                (front_key.to_owned(), front),
                ("base_left_wheel".to_owned(), left),
                ("base_right_wheel".to_owned(), right),
            ]);
            for (name, id, expected) in [
                ("base_front_wheel", 8, front),
                ("base_left_wheel", 9, left),
                ("base_right_wheel", 7, right),
                ("base_left_wheel", 7, right),
                ("base_right_wheel", 9, left),
                ("base_front_wheel", 7, right),
            ] {
                assert_eq!(
                    calibration_for_joint(&calibrations, name, id).unwrap(),
                    Some(expected)
                );
            }
            assert_eq!(calibrations.get(front_key), Some(&front));
            assert_eq!(calibrations.get("base_left_wheel"), Some(&left));
            assert_eq!(calibrations.get("base_right_wheel"), Some(&right));
        }
    }

    #[test]
    fn wheel_calibration_rejects_missing_or_duplicate_ids_within_wheel_keys() {
        let valid = MotorCalibration::identity(8);
        for duplicate_key in ["base_back_wheel", "base_left_wheel", "base_right_wheel"] {
            let calibrations = HashMap::from([
                ("base_front_wheel".to_owned(), valid),
                (duplicate_key.to_owned(), valid),
            ]);
            assert!(calibration_for_joint(&calibrations, "base_front_wheel", 8).is_err());
        }
        for calibrations in [
            HashMap::new(),
            HashMap::from([("base_front_wheel".to_owned(), MotorCalibration::identity(7))]),
            HashMap::from([("head_motor_2".to_owned(), valid)]),
        ] {
            assert!(calibration_for_joint(&calibrations, "base_front_wheel", 8).is_err());
        }
    }

    #[test]
    fn wheel_calibration_rejects_non_wheel_ids_even_with_a_matching_record() {
        for motor_id in [0, 1, 6, 10, 254, 255] {
            let calibrations = HashMap::from([(
                "base_left_wheel".to_owned(),
                MotorCalibration::identity(motor_id),
            )]);
            assert!(calibration_for_joint(&calibrations, "base_left_wheel", motor_id).is_err());
        }
    }

    #[test]
    fn wheel_calibration_does_not_search_other_bus_joint_records() {
        let wheel = MotorCalibration::new(7, 1, -12, 15, 4000).unwrap();
        let other = MotorCalibration::identity(7);
        let calibrations = HashMap::from([
            ("base_right_wheel".to_owned(), wheel),
            ("head_motor_1".to_owned(), other),
            ("left_arm_gripper".to_owned(), other),
        ]);
        assert_eq!(
            calibration_for_joint(&calibrations, "base_left_wheel", 7).unwrap(),
            Some(wheel)
        );
        assert_eq!(
            calibration_for_joint(&calibrations, "head_motor_1", 7).unwrap(),
            Some(other)
        );
        assert!(calibration_for_joint(&calibrations, "head_motor_1", 8).is_err());
        assert_eq!(
            calibration_for_joint(&calibrations, "right_arm_gripper", 7).unwrap(),
            None
        );
    }
}
