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
//! - `feetech_bus2_mixed_raw`：显式选择六臂位置＋三轮0/+500 raw；默认全Position。

#![cfg(feature = "real-hw")]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use robot_core::adapter::{ManipulatorAdapter, SerialRtAdapter};
use robot_core::error::RobotError;
use robot_core::factory::{AdapterBuildContext, AdapterFactory};
use robot_core::time::{RobotClock, SystemClock};

use crate::adapter::{
    FeetechAdapter, FeetechAdapterConfig, raw_profiles_for_config,
    validate_calibration_range_tolerances_raw, validate_raw_mixed_calibrations,
};
use crate::bus::FeetechBus;
use crate::calibration_io::load_and_merge;
use crate::registers::DEFAULT_BAUDRATE;
use crate::transport::SerialTransport;
use crate::transport::real::RealSerialTransport;
use crate::types::{Motor, MotorCalibration};

/// task 28 真硬件 factory。
pub struct FeetechFactory;

impl FeetechFactory {
    /// 内部 helper：跑完整的"address / motor_ids / motors / calibration / 串口 /
    /// adapter new" 流水线，返回**concrete** `FeetechAdapter`。`build` 与
    /// `build_serial_rt` 各自把它装进对应的 `Box<dyn …>`。
    ///
    /// `path_kind` 仅用于日志（`"" / " (SerialRT)"`），帮诊断哪条路径触发本次构造。
    fn build_concrete(
        ctx: &AdapterBuildContext<'_>,
        path_kind: &'static str,
    ) -> Result<FeetechAdapter<RealSerialTransport>, RobotError> {
        Self::build_with_transport(ctx, path_kind, |address, clock| {
            RealSerialTransport::open_with_clock(Path::new(address), DEFAULT_BAUDRATE, clock)
                .map_err(|e| {
                    tracing::error!(bus_address = %address, ?e, "feetech open serial failed");
                    RobotError::from(e)
                })
        })
    }

    /// 同一构造流水线只在静态参数齐备后打开transport，测试可注入内存串口。
    fn build_with_transport<T: SerialTransport>(
        ctx: &AdapterBuildContext<'_>,
        path_kind: &'static str,
        open: impl FnOnce(&str, Arc<dyn RobotClock>) -> Result<T, RobotError>,
    ) -> Result<FeetechAdapter<T>, RobotError> {
        let address = ctx
            .address
            .ok_or(RobotError::InvalidParameter("feetech: missing address"))?;
        if ctx.feetech_bus2_mixed_raw
            && (ctx.adapter_kind != "feetech_sts3215"
                || ctx.instance_name != "right_arm_full_with_base"
                || ctx.motor_ids.is_empty())
        {
            return Err(RobotError::InvalidParameter(
                "raw混合总线需要匹配的右总线实例、Feetech及显式电机ID",
            ));
        }

        let motor_ids = resolve_motor_ids(ctx)?;
        validate_calibration_tolerances(ctx)?;
        let profiles = ctx
            .feetech_bus2_mixed_raw
            .then(|| raw_profiles_for_config(ctx.joint_names, &motor_ids))
            .transpose()?;

        // 构造motor与加载校准均在打开串口前完成，不写任何电机参数。
        let mut motors = HashMap::with_capacity(ctx.joint_names.len());
        for (name, id) in ctx.joint_names.iter().zip(motor_ids.iter()) {
            let motor = Motor::new(*id).map_err(|e| {
                tracing::error!(motor_id = id, joint = %name, ?e, "feetech Motor::new failed");
                RobotError::from(e)
            })?;
            motors.insert(name.clone(), motor);
        }
        let calibrations = load_calibrations(ctx, &motor_ids)?;
        if profiles.is_some() {
            validate_raw_mixed_calibrations(&calibrations)?;
        }
        let has_calibration = !calibrations.is_empty();
        let clock: Arc<dyn RobotClock> = ctx.services.map_or_else(
            || Arc::new(SystemClock::new()) as Arc<dyn RobotClock>,
            |services| Arc::clone(&services.clock),
        );
        let call_deadline = ctx.services.map_or(Duration::from_millis(50), |services| {
            Duration::from_nanos(services.call_deadline.0)
        });
        let transport = open(address, Arc::clone(&clock))?;
        let bus = FeetechBus::new(transport, motors).map_err(|e| {
            tracing::error!(?e, "feetech FeetechBus::new failed");
            RobotError::from(e)
        })?;
        let config = FeetechAdapterConfig {
            clock,
            preempt: ctx.services.and_then(|services| services.preempt.clone()),
            diag: ctx.services.and_then(|services| services.diag.clone()),
            call_deadline,
            adapter_name: "feetech_sts3215",
            joint_names: ctx.joint_names.to_vec(),
            calibrations,
            calibration_range_tolerances_raw: ctx.calibration_range_tolerances_raw.to_vec(),
        };
        let adapter = match profiles {
            Some(profiles) => FeetechAdapter::new_bus2_mixed_raw(config, bus, profiles),
            None => FeetechAdapter::new(config, bus).map_err(RobotError::from),
        }?;

        tracing::info!(
            adapter = "feetech_sts3215",
            path_kind,
            instance = %ctx.instance_name,
            bus_address = %address,
            motors = ctx.joint_names.len(),
            motor_ids = ?motor_ids,
            calibration = if has_calibration { "loaded" } else { "none" },
            feetech_bus2_mixed_raw = ctx.feetech_bus2_mixed_raw,
            "feetech factory: built adapter"
        );
        Ok(adapter)
    }
}

/// 按配置槽位取完整校准；轮记录只按物理ID匹配，不改写原值。
fn load_calibrations(
    ctx: &AdapterBuildContext<'_>,
    motor_ids: &[u8],
) -> Result<Vec<MotorCalibration>, RobotError> {
    if ctx.calibration_paths.is_empty() {
        return Ok(Vec::new());
    }
    let merged = load_and_merge(ctx.calibration_paths).map_err(|e| {
        tracing::error!(calibration_paths = ?ctx.calibration_paths, ?e, "feetech calibration load failed");
        RobotError::from(e)
    })?;
    ctx.joint_names
        .iter()
        .zip(motor_ids)
        .map(|(name, &id)| {
            calibration_for_joint(&merged, name, id)?.ok_or(RobotError::InvalidParameter(
                "feetech: calibration missing motor",
            ))
        })
        .collect()
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

fn validate_calibration_tolerances(ctx: &AdapterBuildContext<'_>) -> Result<(), RobotError> {
    validate_calibration_range_tolerances_raw(
        ctx.joint_names.len(),
        ctx.calibration_range_tolerances_raw,
    )
    .map_err(RobotError::from)?;
    if ctx.calibration_paths.is_empty()
        && ctx
            .calibration_range_tolerances_raw
            .iter()
            .any(|&value| value != 0)
    {
        return Err(crate::FeetechError::CalibrationToleranceWithoutCalibration.into());
    }
    Ok(())
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
    use crate::convert::raw_to_rad;
    use crate::transport::MockTransport;
    use std::cell::Cell;
    use tempfile::NamedTempFile;

    const BUS2_NAMES: [&str; 9] = [
        "right_arm_shoulder_pan",
        "right_arm_shoulder_lift",
        "right_arm_elbow_flex",
        "right_arm_wrist_flex",
        "right_arm_wrist_roll",
        "right_arm_gripper",
        "base_front_wheel",
        "base_left_wheel",
        "base_right_wheel",
    ];

    fn context<'a>(
        names: &'a [String],
        ids: &'a [u8],
        paths: &'a [String],
        tolerances: &'a [u16],
        raw: bool,
    ) -> AdapterBuildContext<'a> {
        AdapterBuildContext {
            services: None,
            instance_name: "right_arm_full_with_base",
            adapter_kind: "feetech_sts3215",
            joint_names: names,
            address: Some("/不存在的串口"),
            motor_ids: ids,
            calibration_paths: paths,
            calibration_range_tolerances_raw: tolerances,
            feetech_bus2_mixed_raw: raw,
        }
    }

    fn calibration_file() -> (NamedTempFile, HashMap<String, MotorCalibration>) {
        let names: Vec<_> = BUS2_NAMES.map(str::to_owned).into();
        let records: HashMap<_, _> = names
            .into_iter()
            .zip([1, 2, 3, 4, 5, 6, 8, 9, 7])
            .map(|(name, id)| {
                let calibration =
                    MotorCalibration::new(id, id % 2, -i32::from(id), 1000, 3000).unwrap();
                (name, calibration)
            })
            .collect();
        let file = NamedTempFile::new().unwrap();
        serde_json::to_writer(file.as_file(), &records).unwrap();
        (file, records)
    }

    fn status(id: u8, data: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xff, id, u8::try_from(data.len() + 2).unwrap(), 0];
        bytes.extend_from_slice(data);
        let checksum = !bytes[2..]
            .iter()
            .fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
        bytes.push(checksum);
        bytes
    }

    #[test]
    fn configured_raw_profiles_and_calibrations_follow_all_wheel_permutations() {
        let (file, records) = calibration_file();
        let paths = [file.path().to_string_lossy().into_owned()];
        for wheels in [
            [7, 8, 9],
            [7, 9, 8],
            [8, 7, 9],
            [8, 9, 7],
            [9, 7, 8],
            [9, 8, 7],
        ] {
            for reversed in [false, true] {
                let mut names: Vec<_> = BUS2_NAMES.map(str::to_owned).into();
                let mut ids = vec![1, 2, 3, 4, 5, 6];
                ids.extend(wheels);
                let mut tolerances = vec![20, 20, 20, 20, 20, 20, 0, 0, 0];
                if reversed {
                    names.reverse();
                    ids.reverse();
                    tolerances.reverse();
                }
                let ctx = context(&names, &ids, &paths, &tolerances, true);
                let profiles = raw_profiles_for_config(&names, &ids).unwrap();
                assert_eq!(
                    profiles.map(crate::velocity_profile::Sts3215RawWheelProfile::motor_id),
                    wheels
                );
                for profile in profiles {
                    assert!(profile.accepts(0) && profile.accepts(500));
                    assert!(!profile.accepts(-500) && !profile.accepts(499));
                }
                let calibrations = load_calibrations(&ctx, &ids).unwrap();
                for (calibration, id) in calibrations.iter().zip(&ids) {
                    let original = records.values().find(|record| record.id == *id).unwrap();
                    assert_eq!(calibration, original);
                }
                let (transport, handle) = MockTransport::new();
                let adapter =
                    FeetechFactory::build_with_transport(&ctx, "test", |_, _| Ok(transport))
                        .unwrap();
                assert!(handle.peek_tx().is_empty(), "构造不得执行串口读写");
                for (slot, id) in ids.iter().enumerate() {
                    let tolerance = if *id <= 6 { 20 } else { 0 };
                    assert_eq!(
                        adapter.limits().position_min[slot],
                        raw_to_rad(1000 - tolerance)
                    );
                    assert_eq!(
                        adapter.limits().position_max[slot],
                        raw_to_rad(3000 + tolerance)
                    );
                }
            }
        }
    }

    #[test]
    fn factory_mixed_raw_is_opt_in_and_connect_rechecks_phase_without_parameter_writes() {
        let names: Vec<_> = BUS2_NAMES.map(str::to_owned).into();
        let ids = [1, 2, 3, 4, 5, 6, 8, 9, 7];
        let (file, _) = calibration_file();
        let paths = [file.path().to_string_lossy().into_owned()];
        for (raw, phase) in [(false, 12), (true, 12), (true, 4)] {
            let (transport, handle) = MockTransport::new();
            let ctx = context(&names, &ids, &paths, &[], raw);
            let mut adapter =
                FeetechFactory::build_with_transport(&ctx, "test", |_, _| Ok(transport)).unwrap();
            for id in 1..=9 {
                handle.push_rx(&status(id, &[]));
                handle.push_rx(&status(id, &[9, 3]));
            }
            if raw {
                for id in ids {
                    handle.push_rx(&status(id, &[u8::from(id >= 7)]));
                    handle.push_rx(&status(id, &[0]));
                }
                for id in [8, 9, 7] {
                    for byte in [phase, 3, 10] {
                        handle.push_rx(&status(id, &[byte]));
                    }
                }
            }
            assert_eq!(adapter.connect().is_ok(), !raw || phase == 12);
            let bytes = handle.drain_tx();
            let mut remaining = bytes.as_slice();
            let mut count = 0;
            while !remaining.is_empty() {
                let frame_len = usize::from(remaining[3]) + 4;
                assert!(matches!(remaining[4], 1 | 2), "connect只能PING或读寄存器");
                remaining = &remaining[frame_len..];
                count += 1;
            }
            assert_eq!(
                count,
                if raw {
                    if phase == 12 { 45 } else { 39 }
                } else {
                    18
                }
            );
        }
    }

    #[test]
    fn invalid_raw_layout_calibration_and_tolerances_never_open_transport() {
        let names: Vec<_> = BUS2_NAMES.map(str::to_owned).into();
        let ids = [1, 2, 3, 4, 5, 6, 8, 9, 7];
        let (file, records) = calibration_file();
        let paths = [file.path().to_string_lossy().into_owned()];
        let opened = Cell::new(false);
        for variant in 0..11 {
            std::fs::write(file.path(), serde_json::to_vec(&records).unwrap()).unwrap();
            let mut names = names.clone();
            let mut ids = ids.to_vec();
            let mut tolerances = vec![20, 20, 20, 20, 20, 20, 0, 0, 0];
            let mut ctx_paths = paths.to_vec();
            match variant {
                0 => names[0] = "其他关节".into(),
                1 => ids[6] = 6,
                2 => ids[6] = ids[7],
                3 => ids.clear(),
                4 => ctx_paths.clear(),
                5 => tolerances.truncate(8),
                6 => tolerances[0] = 4096,
                7 => {
                    let mut incomplete = records.clone();
                    incomplete.remove(BUS2_NAMES[0]);
                    std::fs::write(file.path(), serde_json::to_vec(&incomplete).unwrap()).unwrap();
                }
                8 => {
                    let mut inverted = records.clone();
                    inverted.get_mut(BUS2_NAMES[0]).unwrap().range_min = 3001;
                    std::fs::write(file.path(), serde_json::to_vec(&inverted).unwrap()).unwrap();
                }
                _ => {}
            }
            let mut ctx = context(&names, &ids, &ctx_paths, &tolerances, true);
            if variant == 9 {
                ctx.adapter_kind = "mock_hard_rt";
            }
            if variant == 10 {
                ctx.instance_name = "其他硬件实例";
            }
            let result = FeetechFactory::build_with_transport(&ctx, "test", |_, _| {
                opened.set(true);
                Ok(MockTransport::new().0)
            });
            assert!(
                matches!(result, Err(RobotError::InvalidParameter(_))),
                "variant={variant}"
            );
            assert!(!opened.get());
        }
    }

    #[test]
    fn invalid_calibration_tolerances_are_rejected_before_opening_serial() {
        let names = ["j1".to_owned()];
        for tolerances in [&[0, 20][..], &[4096][..], &[1][..]] {
            let context = AdapterBuildContext {
                services: None,
                instance_name: "test",
                adapter_kind: "feetech_sts3215",
                joint_names: &names,
                address: Some("/不存在的串口"),
                motor_ids: &[1],
                calibration_paths: &[],
                calibration_range_tolerances_raw: tolerances,
                feetech_bus2_mixed_raw: false,
            };
            assert!(matches!(
                FeetechFactory::build_concrete(&context, ""),
                Err(RobotError::InvalidParameter(_))
            ));
        }
    }

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
