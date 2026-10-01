//! STS3215 寄存器表 + 常量 + enum，源自 dimos `feetech_io/registers.py` 全 117 行
//! + `06_lerobot_replace_spec.md` §1.3 / §1.4。
//!
//! ## 寄存器分区
//!
//! - **EEPROM**（addr 0–39）：永久存储。`Lock = 1` 时写保护；对其余既有允许写
//!   条目，写之前必须先 `Torque_Enable = 0` + `Lock = 0`，区段分类不提供写授权。
//! - **SRAM**（addr 40–69）：RAM，不受 `Lock` 影响。
//! - **只读**：`Model_Number` / `Present_*`。
//! - **本项目诊断专用**：`Phase` / `CW_Dead_Zone` / `CCW_Dead_Zone` 仅允许读取。
//!   厂家将其列为可写 EEPROM；本项目禁止写入，不依赖 `Lock` 的现场状态。
//!
//! ## sign-magnitude 编码
//!
//! [`STS3215_SIGN_BIT`] 列出哪些寄存器用 sign-magnitude（**非二补码**）编码 +
//! 各自 sign bit 位置。例：`Homing_Offset` sign_bit=11 → magnitude 上限 2047；
//! `-1` 编码成 `0x801` 而**不是** `0xFFFF`。
//!
//! 详见 `06_lerobot_replace_spec.md` §1.5（为什么 `Goal_Position` /
//! `Present_Position` 也用 sign-magnitude）。

use crate::error::FeetechError;

// ============================================================================
// 基础参数
// ============================================================================

/// STS3215 model number（`ping` 返回此值确认型号正确）。
pub const MODEL_NUMBER_STS3215: u16 = 777;

/// 一圈 tick 数（12-bit 编码器）。
pub const STS3215_RESOLUTION: u16 = 4096;

/// 半转中点 = `(RESOLUTION - 1) / 2`，半转标零（`set_half_turn_homings`）目标值。
pub const STS3215_MAX_POSITION: u16 = 2047;

/// 默认串口波特率 1 Mbps（`06_lerobot_replace_spec.md` §1.1）。
pub const DEFAULT_BAUDRATE: u32 = 1_000_000;

/// scservo_sdk PacketHandler 的 protocol 版本（STS 系列固定 0）。本字段在 Rust 端
/// 未直接使用（wire protocol 自实现），保留作为 dimos 移植对照参考。
pub const DEFAULT_PROTOCOL: u8 = 0;

/// 单次完整读写调用的默认预算（毫秒），覆盖所有短 IO。
pub const DEFAULT_TIMEOUT_MS: u64 = 50;

// ============================================================================
// 控制表（addr, length）
// ============================================================================

/// 单个寄存器的元数据：起始地址 + 字节数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Register {
    pub addr: u8,
    pub length: u8,
}

impl Register {
    const fn new(addr: u8, length: u8) -> Self {
        Self { addr, length }
    }
}

/// 全部已支持寄存器条目（**仅** dimos / lerobot 用到的部分；扩展见 module-level
/// docs）。
///
/// 用 `&[(name, Register)]` 而非 `HashMap<&str, Register>`：v0.2 寄存器数 14 个，
/// 线性扫描 < 100 ns，零依赖；adapter 启动时 `connect` 内可一次性 cache 到固定
/// 数组进一步消除查表（v0.3 优化）。
pub const STS3215_CONTROL_TABLE: &[(&str, Register)] = &[
    // ── EEPROM (Lock=1 时写保护) ───────────────────────────────────────
    ("Model_Number", Register::new(3, 2)), // read-only
    ("ID", Register::new(5, 1)),
    ("Baud_Rate", Register::new(6, 1)),
    ("Min_Position_Limit", Register::new(9, 2)),
    ("Max_Position_Limit", Register::new(11, 2)),
    ("Phase", Register::new(18, 1)), // 本项目诊断专用，保留 raw unsigned 值
    ("P_Coefficient", Register::new(21, 1)),
    ("D_Coefficient", Register::new(22, 1)),
    ("I_Coefficient", Register::new(23, 1)),
    ("CW_Dead_Zone", Register::new(26, 1)),  // 本项目诊断专用
    ("CCW_Dead_Zone", Register::new(27, 1)), // 本项目诊断专用
    ("Homing_Offset", Register::new(31, 2)), // sign-magnitude bit 11
    ("Operating_Mode", Register::new(33, 1)),
    // ── SRAM ──────────────────────────────────────────────────────────
    ("Torque_Enable", Register::new(40, 1)),
    ("Goal_Position", Register::new(42, 2)), // sign-magnitude bit 15
    ("Goal_Velocity", Register::new(46, 2)), // sign-magnitude bit 15
    ("Lock", Register::new(55, 1)),
    ("Present_Position", Register::new(56, 2)), // read-only sign-magnitude bit 15
    ("Present_Velocity", Register::new(58, 2)), // read-only sign-magnitude bit 15
];

/// 查 `STS3215_CONTROL_TABLE` 拿 `Register`。未知名返回
/// [`FeetechError::UnknownRegister`]。
///
/// # Errors
/// 寄存器名不在表中 → [`FeetechError::UnknownRegister`]。
pub fn get_register(name: &'static str) -> Result<Register, FeetechError> {
    STS3215_CONTROL_TABLE
        .iter()
        .find_map(|(n, r)| (*n == name).then_some(*r))
        .ok_or(FeetechError::UnknownRegister(name))
}

/// 仅拒绝本项目三个诊断专用寄存器的写入，零分配。
///
/// 这不是全部寄存器的可写性校验；其他名称仍由既有查表与编码逻辑处理，
/// 未知名称在此返回成功不表示可写。EEPROM 分区也不代替本门。
///
/// # Errors
/// 三个诊断名称返回 [`FeetechError::DiagnosticRegisterWriteForbidden`]。
pub fn ensure_not_diagnostic_write(name: &'static str) -> Result<(), FeetechError> {
    match name {
        "Phase" | "CW_Dead_Zone" | "CCW_Dead_Zone" => {
            Err(FeetechError::DiagnosticRegisterWriteForbidden(name))
        }
        _ => Ok(()),
    }
}

// ============================================================================
// sign-magnitude 编码表
// ============================================================================

/// `(register_name, sign_bit_index)` —— 仅列需要 sign-magnitude 编码的寄存器。
///
/// 例：`Homing_Offset` sign_bit=11 意味着 magnitude 上限 = `(1 << 11) - 1 = 2047`。
pub const STS3215_SIGN_BIT: &[(&str, u32)] = &[
    ("Homing_Offset", 11), // 12-bit signed, range [-2047, +2047]
    ("Goal_Position", 15), // 16-bit signed
    ("Goal_Velocity", 15),
    ("Present_Position", 15),
    ("Present_Velocity", 15),
];

/// 查 `STS3215_SIGN_BIT`：返回 `Some(sign_bit_index)` 表示此寄存器用 sign-magnitude
/// 编码；`None` 表示寄存器值就是 raw unsigned。
#[must_use]
pub fn sign_bit_for(name: &str) -> Option<u32> {
    STS3215_SIGN_BIT
        .iter()
        .find_map(|(n, b)| (*n == name).then_some(*b))
}

// ============================================================================
// EEPROM 写保护：哪些寄存器在 EEPROM 区
// ============================================================================

const EEPROM_REGISTERS: &[&str] = &[
    "ID",
    "Baud_Rate",
    "Min_Position_Limit",
    "Max_Position_Limit",
    "Phase",
    "P_Coefficient",
    "D_Coefficient",
    "I_Coefficient",
    "CW_Dead_Zone",
    "CCW_Dead_Zone",
    "Homing_Offset",
    "Operating_Mode",
];

/// `true` 仅表示该寄存器在 EEPROM 区，不提供写授权。
/// 三个诊断专用条目仍被 [`ensure_not_diagnostic_write`] 拒写；其余既有允许写
/// 条目在写之前需要 `Torque_Enable = 0` + `Lock = 0`。
#[must_use]
pub fn is_eeprom_register(name: &str) -> bool {
    EEPROM_REGISTERS.contains(&name)
}

// ============================================================================
// 枚举
// ============================================================================

/// `Operating_Mode` 寄存器枚举值。
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatingMode {
    Position = 0,
    Velocity = 1,
    Pwm = 2,
    Step = 3,
}

/// `Torque_Enable` 寄存器枚举值。
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorqueMode {
    Disabled = 0,
    Enabled = 1,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_metadata_is_raw_one_byte_eeprom() {
        for (name, address) in [("Phase", 18), ("CW_Dead_Zone", 26), ("CCW_Dead_Zone", 27)] {
            assert_eq!(get_register(name).unwrap(), Register::new(address, 1));
            assert!(is_eeprom_register(name));
            assert_eq!(sign_bit_for(name), None);
            assert!(matches!(
                ensure_not_diagnostic_write(name),
                Err(FeetechError::DiagnosticRegisterWriteForbidden(got)) if got == name
            ));
        }
    }

    #[test]
    fn diagnostic_guard_does_not_reclassify_existing_or_unknown_names() {
        for (name, _) in STS3215_CONTROL_TABLE {
            match *name {
                "Phase" | "CW_Dead_Zone" | "CCW_Dead_Zone" => {}
                _ => assert!(ensure_not_diagnostic_write(name).is_ok()),
            }
        }
        assert!(ensure_not_diagnostic_write("Unknown_Register").is_ok());
        assert!(matches!(
            get_register("Unknown_Register"),
            Err(FeetechError::UnknownRegister("Unknown_Register"))
        ));
    }

    #[test]
    fn known_register_lookup() {
        assert_eq!(get_register("Goal_Position").unwrap(), Register::new(42, 2));
        assert_eq!(get_register("Lock").unwrap(), Register::new(55, 1));
        assert_eq!(
            get_register("Present_Position").unwrap(),
            Register::new(56, 2)
        );
    }

    #[test]
    fn unknown_register_errors() {
        let err = get_register("NopeNope").unwrap_err();
        match err {
            FeetechError::UnknownRegister(n) => assert_eq!(n, "NopeNope"),
            other => panic!("expected UnknownRegister, got {other:?}"),
        }
    }

    #[test]
    fn sign_bit_lookup_matches_dimos() {
        // 字面对照 dimos `registers.py` STS3215_SIGN_BIT 表
        assert_eq!(sign_bit_for("Homing_Offset"), Some(11));
        assert_eq!(sign_bit_for("Goal_Position"), Some(15));
        assert_eq!(sign_bit_for("Goal_Velocity"), Some(15));
        assert_eq!(sign_bit_for("Present_Position"), Some(15));
        assert_eq!(sign_bit_for("Present_Velocity"), Some(15));
        // unsigned 寄存器
        assert_eq!(sign_bit_for("Lock"), None);
        assert_eq!(sign_bit_for("Torque_Enable"), None);
        assert_eq!(sign_bit_for("Min_Position_Limit"), None);
    }

    #[test]
    fn eeprom_registers_match_dimos() {
        // dimos `registers.py` EEPROM_REGISTERS frozenset 9 条
        for name in [
            "ID",
            "Baud_Rate",
            "Min_Position_Limit",
            "Max_Position_Limit",
            "P_Coefficient",
            "D_Coefficient",
            "I_Coefficient",
            "Homing_Offset",
            "Operating_Mode",
        ] {
            assert!(is_eeprom_register(name), "{name} 应在 EEPROM 区");
        }
        // SRAM 区 + 只读不应判 EEPROM
        for name in [
            "Torque_Enable",
            "Goal_Position",
            "Goal_Velocity",
            "Lock",
            "Present_Position",
            "Present_Velocity",
            "Model_Number",
        ] {
            assert!(!is_eeprom_register(name), "{name} 不应在 EEPROM 区");
        }
    }

    #[test]
    fn no_duplicate_register_names() {
        // 防一时手滑表里加重复条目（线性扫描第一条命中，后续被屏蔽）。
        let mut names: Vec<&str> = STS3215_CONTROL_TABLE.iter().map(|(n, _)| *n).collect();
        names.sort_unstable();
        let len_before = names.len();
        names.dedup();
        assert_eq!(len_before, names.len(), "STS3215_CONTROL_TABLE 有重名");
    }

    #[test]
    fn no_duplicate_addresses_per_length() {
        // 同一 (addr, length) 不应重复（可能是表项错抄）。
        let mut keys: Vec<(u8, u8)> = STS3215_CONTROL_TABLE
            .iter()
            .map(|(_, r)| (r.addr, r.length))
            .collect();
        keys.sort_unstable();
        let len_before = keys.len();
        keys.dedup();
        assert_eq!(
            len_before,
            keys.len(),
            "STS3215_CONTROL_TABLE 有 (addr, length) 重复"
        );
    }

    #[test]
    fn enums_have_expected_repr_values() {
        // 寄存器 raw 值跟 dimos 对齐，避免重命名引入 silent regression
        assert_eq!(OperatingMode::Position as u8, 0);
        assert_eq!(OperatingMode::Velocity as u8, 1);
        assert_eq!(OperatingMode::Pwm as u8, 2);
        assert_eq!(OperatingMode::Step as u8, 3);
        assert_eq!(TorqueMode::Disabled as u8, 0);
        assert_eq!(TorqueMode::Enabled as u8, 1);
    }

    #[test]
    fn constants_match_dimos() {
        assert_eq!(MODEL_NUMBER_STS3215, 777);
        assert_eq!(STS3215_RESOLUTION, 4096);
        assert_eq!(STS3215_MAX_POSITION, 2047);
        assert_eq!(DEFAULT_BAUDRATE, 1_000_000);
    }
}
