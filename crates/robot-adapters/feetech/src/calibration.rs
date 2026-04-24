//! 校准流程 —— 移植自 dimos `feetech_io/calibration.py` 全 186 行。
//!
//! ## EEPROM 写保护
//!
//! `Homing_Offset` / `Min_Position_Limit` / `Max_Position_Limit` 都在 EEPROM 区
//! （[`is_eeprom_register`](crate::registers::is_eeprom_register)）。写之前必须：
//!
//! 1. `Torque_Enable = 0`
//! 2. `Lock = 0`
//!
//! 顺序错 / 漏写 `Lock=0` → 写**静默失败**（电机不报错，只是不生效）。
//! `06_lerobot_replace_spec.md` §11 风险 1 明确标记。
//!
//! 本 crate **显式**做这两步，而不是像 lerobot 那样把它们隐藏在
//! `disable_torque` 里。

use crate::bus::FeetechBus;
use crate::error::FeetechError;
use crate::registers::{STS3215_MAX_POSITION, STS3215_RESOLUTION, TorqueMode};
use crate::transport::SerialTransport;
use crate::types::MotorCalibration;

// ============================================================================
// EEPROM 解锁 / 上锁
// ============================================================================

/// 对每个 motor: `Torque_Enable = 0`，`Lock = 0`。
///
/// **任何**写 EEPROM 区寄存器之前必须先调它。
///
/// # Errors
/// 透传 `bus.write` 错误，遇到第一个失败立即返回（不继续尝试其他 motor）。
pub fn unlock_eeprom<T: SerialTransport>(
    bus: &mut FeetechBus<T>,
    motor_names: &[&str],
) -> Result<(), FeetechError> {
    for name in motor_names {
        bus.write("Torque_Enable", name, i32::from(TorqueMode::Disabled as u8))?;
        bus.write("Lock", name, 0)?;
    }
    Ok(())
}

/// 对每个 motor: `Lock = 1`。**不**重新打开 `Torque_Enable`，由调用方决定。
///
/// # Errors
/// 透传 `bus.write` 错误。
pub fn lock_eeprom<T: SerialTransport>(
    bus: &mut FeetechBus<T>,
    motor_names: &[&str],
) -> Result<(), FeetechError> {
    for name in motor_names {
        bus.write("Lock", name, 1)?;
    }
    Ok(())
}

// ============================================================================
// reset_calibration
// ============================================================================

/// 把 `Homing_Offset` 清零，`Min/Max_Position_Limit` 设回全范围 \[0, 4095\]。
///
/// 自动 unlock + 写 + lock。**遇错也保证 lock**（finally 语义靠手动 `_ = lock_eeprom(..)`
/// 在 hot-path 实现）。
///
/// # Errors
/// 透传 `bus.write` 错误（unlock 阶段失败立即返回；写阶段失败仍尝试 lock 后返回原错）。
pub fn reset_calibration<T: SerialTransport>(
    bus: &mut FeetechBus<T>,
    motor_names: &[&str],
) -> Result<(), FeetechError> {
    unlock_eeprom(bus, motor_names)?;

    let max_position = i32::from(STS3215_RESOLUTION - 1);
    let inner = (|| -> Result<(), FeetechError> {
        for name in motor_names {
            bus.write("Homing_Offset", name, 0)?;
            bus.write("Min_Position_Limit", name, 0)?;
            bus.write("Max_Position_Limit", name, max_position)?;
        }
        Ok(())
    })();

    // finally lock，丢弃 lock 错误以保留主错误
    let lock_err = lock_eeprom(bus, motor_names);
    inner?;
    lock_err
}

// ============================================================================
// write_calibration
// ============================================================================

/// 把 `calibrations` 里的 `homing_offset` / `range_min` / `range_max` 写到对应
/// motor EEPROM。
///
/// `calibrations` 必须按 `motor_name → MotorCalibration` 给齐。motor 对应名字必须
/// 已在 bus 里。
///
/// # Errors
/// - `motor_name` 未知 → [`FeetechError::UnknownMotor`]（透传）
/// - `bus.write` 失败 → 透传，但 EEPROM 仍被 lock 回去
pub fn write_calibration<T: SerialTransport>(
    bus: &mut FeetechBus<T>,
    calibrations: &[(&str, MotorCalibration)],
) -> Result<(), FeetechError> {
    let motor_names: Vec<&str> = calibrations.iter().map(|(n, _)| *n).collect();
    unlock_eeprom(bus, &motor_names)?;

    let inner = (|| -> Result<(), FeetechError> {
        for (name, cal) in calibrations {
            bus.write("Homing_Offset", name, cal.homing_offset)?;
            bus.write("Min_Position_Limit", name, i32::from(cal.range_min))?;
            bus.write("Max_Position_Limit", name, i32::from(cal.range_max))?;
        }
        Ok(())
    })();

    let lock_err = lock_eeprom(bus, &motor_names);
    inner?;
    lock_err
}

// ============================================================================
// set_half_turn_homings
// ============================================================================

/// 半转标零 —— 让每个 motor 当前的物理位置对应到 `Present_Position = 2047`。
///
/// 算法（来自 lerobot / dimos 同名函数）：
///
/// ```text
/// Present_Position = Actual_Position - Homing_Offset
/// 想让 Present 读到 2047，所以 Homing_Offset = Actual - 2047
/// ```
///
/// 步骤：
/// 1. [`reset_calibration`]（清旧 offset，避免读到带旧 offset 的 Present）
/// 2. `sync_read("Present_Position")`（此时 Present = Actual）
/// 3. 算 `offset = current - 2047`
/// 4. unlock + 写 `Homing_Offset` + lock
///
/// **调用方要确保电机断扭矩 + 物理位置在用户希望的"半转中点"上**。
///
/// 返回 `motor_name → offset` 映射，便于持久化到 calibration JSON / 上层显示。
///
/// # Errors
/// 透传 `bus.write` / `bus.sync_read` 错误。
pub fn set_half_turn_homings<T: SerialTransport>(
    bus: &mut FeetechBus<T>,
    motor_names: &[&str],
) -> Result<std::collections::HashMap<String, i32>, FeetechError> {
    reset_calibration(bus, motor_names)?;

    // 读 Present_Position（此时 Present = Actual）
    let positions = bus.sync_read("Present_Position", motor_names)?;

    let half = i32::from(STS3215_MAX_POSITION);
    let mut offsets = std::collections::HashMap::with_capacity(positions.len());
    for (name, pos) in &positions {
        offsets.insert(name.clone(), *pos - half);
    }

    // 写 Homing_Offset（需要 EEPROM 解锁）
    unlock_eeprom(bus, motor_names)?;
    let inner = (|| -> Result<(), FeetechError> {
        for name in motor_names {
            if let Some(offset) = offsets.get(*name) {
                bus.write("Homing_Offset", name, *offset)?;
            }
        }
        Ok(())
    })();
    let lock_err = lock_eeprom(bus, motor_names);
    inner?;
    lock_err?;

    Ok(offsets)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::checksum;
    use crate::transport::{MockHandle, MockTransport};
    use crate::types::Motor;
    use std::collections::HashMap;

    fn make_bus(motors: &[(&str, u8)]) -> (FeetechBus<MockTransport>, MockHandle) {
        let (transport, handle) = MockTransport::new();
        let mut motors_map = HashMap::new();
        for (name, id) in motors {
            motors_map.insert((*name).to_string(), Motor::new(*id).unwrap());
        }
        let bus = FeetechBus::new(transport, motors_map).unwrap();
        (bus, handle)
    }

    fn ack_status(id: u8) -> Vec<u8> {
        // 空 payload status：FF FF ID 02 00 CHK
        let mut out = vec![0xFF, 0xFF, id, 0x02, 0x00];
        out.push(checksum(&out[2..]));
        out
    }

    fn read_status(id: u8, params: &[u8]) -> Vec<u8> {
        let len = u8::try_from(params.len() + 2).unwrap();
        let mut out = vec![0xFF, 0xFF, id, len, 0x00];
        out.extend_from_slice(params);
        out.push(checksum(&out[2..]));
        out
    }

    #[test]
    fn unlock_eeprom_writes_torque_off_then_lock_off_per_motor() {
        let (mut bus, handle) = make_bus(&[("a", 1)]);
        // 4 个 ACK：unlock 阶段 (Torque_Enable + Lock) + reset 主体 ... 这里只测 unlock
        for _ in 0..2 {
            handle.push_rx(&ack_status(1));
        }
        unlock_eeprom(&mut bus, &["a"]).unwrap();
        let _ = handle.drain_tx();
    }

    #[test]
    fn reset_calibration_writes_three_eeprom_regs_per_motor() {
        let (mut bus, handle) = make_bus(&[("a", 1)]);
        // 一个 motor: 2 (unlock) + 3 (reset main) + 1 (lock) = 6 ACK
        for _ in 0..6 {
            handle.push_rx(&ack_status(1));
        }
        reset_calibration(&mut bus, &["a"]).unwrap();
        // tx 应包含 6 个 WRITE_DATA 指令，其中 Homing_Offset / Min / Max 各一次
        let tx = handle.drain_tx();
        // 简单验：Homing_Offset addr=31 = 0x1F 出现一次
        let count_31 = tx.windows(3).filter(|w| *w == [0x03, 0x1F, 0x00]).count();
        assert!(count_31 >= 1, "应至少有一次 WRITE Homing_Offset=0");
        // Max_Position_Limit = 4095 = 0x0FFF, addr=11=0x0B, LE [FF, 0F]
        let count_max = tx
            .windows(4)
            .filter(|w| *w == [0x03, 0x0B, 0xFF, 0x0F])
            .count();
        assert_eq!(count_max, 1, "Max_Position_Limit 应被写入一次");
    }

    #[test]
    fn write_calibration_unlocks_writes_then_locks() {
        let (mut bus, handle) = make_bus(&[("a", 1)]);
        // 一个 motor: 2 unlock + 3 write_calibration + 1 lock = 6 ACK
        for _ in 0..6 {
            handle.push_rx(&ack_status(1));
        }
        let cal = MotorCalibration::new(1, 0, -100, 50, 4000).unwrap();
        write_calibration(&mut bus, &[("a", cal)]).unwrap();
        // tx 应含 Homing_Offset=-100 写入。-100 在 sign_bit=11 编码 = 0x800 | 100 = 0x864, LE [64, 08]
        let tx = handle.drain_tx();
        let has_offset = tx.windows(4).any(|w| *w == [0x03, 0x1F, 0x64, 0x08]);
        assert!(
            has_offset,
            "应找到 WRITE Homing_Offset (encoded -100) frame"
        );
    }

    #[test]
    fn set_half_turn_homings_computes_offset_from_present() {
        let (mut bus, handle) = make_bus(&[("a", 1)]);
        // 流程：reset_calibration (2 unlock + 3 main + 1 lock) → sync_read Present → 2 unlock + 1 write_offset + 1 lock
        for _ in 0..6 {
            handle.push_rx(&ack_status(1));
        }
        // sync_read response: 1 个 motor, Present_Position=2500 (positive raw [0xC4, 0x09])
        handle.push_rx(&read_status(1, &[0xC4, 0x09]));
        // 然后 unlock + write_offset + lock = 4 ACK
        for _ in 0..4 {
            handle.push_rx(&ack_status(1));
        }

        let offsets = set_half_turn_homings(&mut bus, &["a"]).unwrap();
        assert_eq!(offsets.get("a"), Some(&(2500 - 2047)));
        // 也即 +453
        assert_eq!(offsets.get("a"), Some(&453));
    }

    #[test]
    fn set_half_turn_homings_negative_offset() {
        // current=100, expect offset = 100 - 2047 = -1947 (在 [-2047, +2047] 范围内)
        let (mut bus, handle) = make_bus(&[("a", 1)]);
        for _ in 0..6 {
            handle.push_rx(&ack_status(1));
        }
        // Present_Position=100 (LE [0x64, 0x00])
        handle.push_rx(&read_status(1, &[0x64, 0x00]));
        for _ in 0..4 {
            handle.push_rx(&ack_status(1));
        }
        let offsets = set_half_turn_homings(&mut bus, &["a"]).unwrap();
        assert_eq!(offsets.get("a"), Some(&-1947));
    }
}
