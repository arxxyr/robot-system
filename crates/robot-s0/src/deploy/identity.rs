//! 部署入口的总线身份校验，在取锁、计数及打开串口之前拒绝错侧配置。
//! arm-only 保留 D2 的六关节、单份臂标定精确合同；既有
//! `xlerobot_bus1_full` / `xlerobot_bus2_full` 显式选择完整总线配置，
//! 复用分组布局、硬件及相机验证器，不另存轮位映射。
//! 两种配置都逐字核对完整 by-id 路径并要求 `auto_enable=false`；
//! 安全写序始终由 [`DeployArm::all_ids`] 决定。

use std::path::Path;

use thiserror::Error;

use super::types::DeployArm;
use crate::config_loader::{
    ConfigLoadError, HardwareSection, LoadedConfig, load_robot_config, validate_camera_sections,
    validate_hardware_section,
};
use crate::grouped_position::{XleRobotBus1Layout, XleRobotBus2Layout};

/// 唯一支持的 adapter 字面（`[[hardware]].adapter`）。
pub const EXPECTED_ADAPTER: &str = "feetech_sts3215";
/// 串口必须走稳定的 by-id 路径（`ttyACM*` 枚举顺序会变，D2 注）。
pub const BY_ID_PREFIX: &str = "/dev/serial/by-id/";

/// 一条臂的 D2 身份。
#[derive(Debug, Clone, Copy)]
pub struct ArmIdentity {
    /// 运行身份。
    pub robot_id: &'static str,
    /// `[[hardware]].id`。
    pub hardware_id: &'static str,
    /// `[[hardware]].address`——**完整 by-id 绝对路径，逐字相等**。
    pub address: &'static str,
    /// by-id 路径里的串口序列号（仅日志 / 诊断用；**校验走整串相等**）。
    pub bus_serial: &'static str,
    /// `calibration_paths` 唯一一项的**文件名**（目录随部署，文件名锁死）。
    pub calibration_file: &'static str,
    /// `[[hardware]].motor_ids`（本臂 6 关节；**非**安全写序电机集）。
    pub motor_ids: &'static [u8],
    /// `[[hardware]].joints`（`ACTION_DIM` 序）。
    pub joints: &'static [&'static str],
}

const LEFT: ArmIdentity = ArmIdentity {
    robot_id: "xlerobot_left",
    hardware_id: "left_arm",
    address: "/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C121427-if00",
    bus_serial: "5A7C121427",
    calibration_file: "left_arm.json",
    motor_ids: &[1, 2, 3, 4, 5, 6],
    joints: &[
        "left_arm_shoulder_pan",
        "left_arm_shoulder_lift",
        "left_arm_elbow_flex",
        "left_arm_wrist_flex",
        "left_arm_wrist_roll",
        "left_arm_gripper",
    ],
};

const RIGHT: ArmIdentity = ArmIdentity {
    robot_id: "xlerobot_right",
    hardware_id: "right_arm",
    address: "/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C122340-if00",
    bus_serial: "5A7C122340",
    calibration_file: "right_arm.json",
    motor_ids: &[1, 2, 3, 4, 5, 6],
    joints: &[
        "right_arm_shoulder_pan",
        "right_arm_shoulder_lift",
        "right_arm_elbow_flex",
        "right_arm_wrist_flex",
        "right_arm_wrist_roll",
        "right_arm_gripper",
    ],
};

/// 取某条臂的 D2 身份。
#[must_use]
pub const fn expected(arm: DeployArm) -> &'static ArmIdentity {
    match arm {
        DeployArm::Left => &LEFT,
        DeployArm::Right => &RIGHT,
    }
}

/// 运行身份显式选择本侧 arm-only 或完整总线，其他身份均拒绝。
pub(crate) fn expected_hardware_id(arm: DeployArm, robot_id: &str) -> Option<&'static str> {
    let arm_identity = expected(arm);
    if robot_id == arm_identity.robot_id {
        return Some(arm_identity.hardware_id);
    }
    match (arm, robot_id) {
        (DeployArm::Left, "xlerobot_bus1_full") => Some("left_arm_full_with_head"),
        (DeployArm::Right, "xlerobot_bus2_full") => Some("right_arm_full_with_base"),
        _ => None,
    }
}

/// 身份校验失败——**一律 fail-closed，绝不打开串口**。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdentityError {
    /// config 读取 / 解析失败。
    #[error("load {path}: {message}")]
    Load {
        /// config 路径。
        path: String,
        /// 底层错误文本。
        message: String,
    },
    /// `[[hardware]]` 段数不为 1。
    #[error("{arm}: expected exactly one [[hardware]] in {path}, found {found}")]
    HardwareCount {
        /// 归属臂。
        arm: DeployArm,
        /// config 路径。
        path: String,
        /// 实际段数。
        found: usize,
    },
    /// 某个身份字段与 D2 表不符（**含左右 config 互换**）。
    #[error("{arm}: {field} mismatch: expected {expected:?}, config has {actual:?}")]
    FieldMismatch {
        /// 归属臂。
        arm: DeployArm,
        /// 字段名。
        field: &'static str,
        /// D2 期望值。
        expected: String,
        /// config 实际值。
        actual: String,
    },
}

/// 校验通过的 arm config 摘要（只带下游真正要用的值）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedArmConfig {
    /// 归属臂。
    pub arm: DeployArm,
    /// `[robot].id`。
    pub robot_id: String,
    /// `[[hardware]].id`。
    pub hardware_id: String,
    /// 串口 by-id 绝对路径。
    pub address: String,
    /// 本臂标定文件绝对路径（完整总线的第二份标定也已校验）。
    pub calibration_path: String,
}

fn mismatch(
    arm: DeployArm,
    field: &'static str,
    expected: impl std::fmt::Debug,
    actual: impl std::fmt::Debug,
) -> IdentityError {
    IdentityError::FieldMismatch {
        arm,
        field,
        expected: format!("{expected:?}"),
        actual: format!("{actual:?}"),
    }
}

/// 按本侧显式运行身份校验 arm-only 或完整总线配置。
///
/// # Errors
///
/// 见 [`IdentityError`]——任一项不符即拒绝（调用方**不得**打开串口）。
pub fn verify_arm_config(
    arm: DeployArm,
    loaded: &LoadedConfig,
) -> Result<VerifiedArmConfig, IdentityError> {
    let want = expected(arm);
    let path = loaded.source_path.display().to_string();

    let hardware_id = expected_hardware_id(arm, &loaded.parsed.robot.id).ok_or_else(|| {
        mismatch(
            arm,
            "[robot].id",
            "本侧 arm-only 或完整总线身份",
            &loaded.parsed.robot.id,
        )
    })?;
    let full_bus = loaded.parsed.robot.id != want.robot_id;

    let hardware = match loaded.parsed.hardware.as_slice() {
        [only] => only,
        other => {
            return Err(IdentityError::HardwareCount {
                arm,
                path,
                found: other.len(),
            });
        }
    };

    if hardware.id != hardware_id {
        return Err(mismatch(arm, "[[hardware]].id", hardware_id, &hardware.id));
    }
    if hardware.adapter != EXPECTED_ADAPTER {
        return Err(mismatch(
            arm,
            "[[hardware]].adapter",
            EXPECTED_ADAPTER,
            &hardware.adapter,
        ));
    }

    let address = hardware
        .address
        .as_deref()
        .ok_or_else(|| mismatch(arm, "[[hardware]].address", want.address, "<missing>"))?;
    // **整串相等**：子串匹配会放行伪后缀（`-if01`）与更长伪串号
    // （`5A7C1214270`），两者都指向另一个设备节点（round 2 P0-1）。
    if address != want.address {
        return Err(mismatch(arm, "[[hardware]].address", want.address, address));
    }

    let calibration_path = verify_calibration_paths(arm, want, hardware, full_bus)?;

    if !full_bus && hardware.motor_ids != want.motor_ids {
        return Err(mismatch(
            arm,
            "[[hardware]].motor_ids",
            want.motor_ids,
            &hardware.motor_ids,
        ));
    }
    if !full_bus && hardware.joints != want.joints {
        return Err(mismatch(
            arm,
            "[[hardware]].joints",
            want.joints,
            &hardware.joints,
        ));
    }
    // D15-R (c1+) 安全边界 ②：不得自动上扭。
    if hardware.auto_enable != Some(false) {
        return Err(mismatch(
            arm,
            "[[hardware]].auto_enable",
            Some(false),
            hardware.auto_enable,
        ));
    }
    if full_bus {
        verify_full_bus(arm, loaded, hardware)?;
    }

    Ok(VerifiedArmConfig {
        arm,
        robot_id: loaded.parsed.robot.id.clone(),
        hardware_id: hardware.id.clone(),
        address: address.to_string(),
        calibration_path,
    })
}

fn verify_full_bus(
    arm: DeployArm,
    loaded: &LoadedConfig,
    hardware: &HardwareSection,
) -> Result<(), IdentityError> {
    let mixed_raw = arm == DeployArm::Right;
    if hardware.feetech_bus2_mixed_raw != mixed_raw {
        return Err(mismatch(
            arm,
            "[[hardware]].feetech_bus2_mixed_raw",
            mixed_raw,
            hardware.feetech_bus2_mixed_raw,
        ));
    }
    validate_hardware_section(hardware)
        .map_err(|e| mismatch(arm, "[[hardware]]", "合法完整总线配置", e.to_string()))?;
    match arm {
        DeployArm::Left => XleRobotBus1Layout::from_hardware(hardware).map(|_| ()),
        DeployArm::Right => XleRobotBus2Layout::from_hardware(hardware).map(|_| ()),
    }
    .map_err(|e| {
        mismatch(
            arm,
            "[[hardware]].layout",
            "本侧完整总线布局",
            e.to_string(),
        )
    })?;
    validate_camera_sections(&loaded.parsed)
        .map_err(|e| mismatch(arm, "[[sensor]]", "本侧合法相机归属与参数", e.to_string()))?;
    Ok(())
}

/// arm-only 恰好一份臂标定；完整总线追加本侧 head/base 标定。
/// 每项都必须是绝对路径，并按当前配置顺序逐字核对文件名。
fn verify_calibration_paths(
    arm: DeployArm,
    want: &ArmIdentity,
    hardware: &HardwareSection,
    full_bus: bool,
) -> Result<String, IdentityError> {
    const FIELD: &str = "[[hardware]].calibration_paths";
    let files = [
        want.calibration_file,
        match arm {
            DeployArm::Left => "head.json",
            DeployArm::Right => "base_left9_right7.json",
        },
    ];
    let expected_files = &files[..if full_bus { 2 } else { 1 }];
    let expected = format!("绝对路径标定 {expected_files:?}");

    let Some(value) = hardware.extras.get("calibration_paths") else {
        return Err(mismatch(arm, FIELD, expected, "<missing>"));
    };
    let Some(array) = value.as_array() else {
        return Err(mismatch(arm, FIELD, expected, value));
    };
    if array.len() != expected_files.len() {
        return Err(mismatch(arm, FIELD, expected, array));
    }
    let mut calibration_path = String::new();
    for (index, (value, file)) in array.iter().zip(expected_files).enumerate() {
        let Some(text) = value.as_str() else {
            return Err(mismatch(arm, FIELD, &expected, value));
        };
        let path = Path::new(text);
        if !path.is_absolute() || path.file_name().and_then(|n| n.to_str()) != Some(*file) {
            return Err(mismatch(arm, FIELD, &expected, text));
        }
        if index == 0 {
            calibration_path = text.to_string();
        }
    }
    Ok(calibration_path)
}

/// 读 config 并校验 D2 身份。
///
/// # Errors
///
/// 见 [`IdentityError`]。
pub fn load_and_verify_arm_config(
    arm: DeployArm,
    path: &Path,
) -> Result<VerifiedArmConfig, IdentityError> {
    let loaded = load_robot_config(path).map_err(|e: ConfigLoadError| IdentityError::Load {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    verify_arm_config(arm, &loaded)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;

    fn repo_config(arm: DeployArm) -> PathBuf {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../configs");
        PathBuf::from(format!("{root}/xlerobot_{}_arm.toml", arm.label()))
    }

    fn full_config(arm: DeployArm) -> LoadedConfig {
        let file = match arm {
            DeployArm::Left => "xlerobot_bus1_full.toml",
            DeployArm::Right => "xlerobot_bus2_full.toml",
        };
        load_robot_config(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../configs")
                .join(file),
        )
        .expect("读取完整总线配置")
    }

    /// 把权威 config 文本做一处定点改写后落到 tempdir。
    fn mutated(arm: DeployArm, from: &str, to: &str) -> (TempDir, PathBuf) {
        let text = std::fs::read_to_string(repo_config(arm)).expect("read authoritative");
        assert!(text.contains(from), "锚点 {from:?} 不在权威 config 内");
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("mutated.toml");
        std::fs::write(&path, text.replace(from, to)).unwrap();
        (dir, path)
    }

    fn field_err(arm: DeployArm, path: &Path) -> (&'static str, String) {
        match load_and_verify_arm_config(arm, path).expect_err("必须拒绝") {
            IdentityError::FieldMismatch { field, actual, .. } => (field, actual),
            other => panic!("期望 FieldMismatch，得到 {other}"),
        }
    }

    /// 仓库两份权威 config 必须**逐字**满足 D2 身份表。
    #[test]
    fn authoritative_configs_match_d2_identity() {
        for arm in [DeployArm::Left, DeployArm::Right] {
            let verified =
                load_and_verify_arm_config(arm, &repo_config(arm)).expect("权威 config 必须过");
            let want = expected(arm);
            assert_eq!(verified.robot_id, want.robot_id);
            assert_eq!(verified.hardware_id, want.hardware_id);
            assert_eq!(verified.address, want.address);
            assert!(verified.address.starts_with(BY_ID_PREFIX));
            assert!(verified.address.contains(want.bus_serial), "{verified:?}");
            assert!(
                verified.calibration_path.ends_with(want.calibration_file),
                "{verified:?}"
            );
            assert!(Path::new(&verified.calibration_path).is_absolute());
        }
    }

    /// **P0-1 反向主 case**：左右 config 互换必须被拒——否则
    /// `--arm left --config right.toml` 会打开 bus2 却跑左侧写序。
    #[test]
    fn swapped_config_is_rejected_before_opening_any_bus() {
        for (arm, other) in [
            (DeployArm::Left, DeployArm::Right),
            (DeployArm::Right, DeployArm::Left),
        ] {
            let (field, actual) = field_err(arm, &repo_config(other));
            assert_eq!(field, "[robot].id", "{arm} ← {other}");
            assert_eq!(actual, format!("{:?}", expected(other).robot_id));
        }
    }

    /// 逐字段反向：每改一处就必须命中对应字段。
    #[test]
    fn each_identity_field_is_checked() {
        let arm = DeployArm::Left;

        let (_d, p) = mutated(
            arm,
            "id             = \"xlerobot_left\"",
            "id             = \"xlerobot_x\"",
        );
        assert_eq!(field_err(arm, &p).0, "[robot].id");

        let (_d, p) = mutated(
            arm,
            "id          = \"left_arm\"",
            "id          = \"other_arm\"",
        );
        assert_eq!(field_err(arm, &p).0, "[[hardware]].id");

        let (_d, p) = mutated(arm, "feetech_sts3215", "mock");
        assert_eq!(field_err(arm, &p).0, "[[hardware]].adapter");

        // 串号换成右 bus = 最危险的错 bus 场景（其余字段全对）。
        let (_d, p) = mutated(arm, "5A7C121427", "5A7C122340");
        assert_eq!(field_err(arm, &p).0, "[[hardware]].address");

        // 非 by-id 的不稳定路径。
        let (_d, p) = mutated(
            arm,
            "\"/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C121427-if00\"",
            "\"/dev/ttyACM0\"",
        );
        assert_eq!(field_err(arm, &p).0, "[[hardware]].address");

        let (_d, p) = mutated(
            arm,
            "motor_ids   = [1, 2, 3, 4, 5, 6]",
            "motor_ids   = [1, 2, 3, 4, 5, 7]",
        );
        assert_eq!(field_err(arm, &p).0, "[[hardware]].motor_ids");

        let (_d, p) = mutated(arm, "\"left_arm_gripper\"", "\"left_arm_claw\"");
        assert_eq!(field_err(arm, &p).0, "[[hardware]].joints");

        // D15-R (c1+) 安全边界 ②：auto_enable 必须显式 false。
        let (_d, p) = mutated(arm, "auto_enable = false", "auto_enable = true");
        assert_eq!(field_err(arm, &p).0, "[[hardware]].auto_enable");
        let (_d, p) = mutated(arm, "auto_enable = false", "# auto_enable removed");
        assert_eq!(field_err(arm, &p).0, "[[hardware]].auto_enable");
    }

    /// **round 2 P0-1**：`contains(串号)` 会放行的三类伪串口路径——整串
    /// 相等才拦得住。
    #[test]
    fn lookalike_by_id_paths_are_rejected() {
        let arm = DeployArm::Left;
        let real = "usb-1a86_USB_Single_Serial_5A7C121427-if00";
        for fake in [
            // 伪后缀：同串号的另一个设备节点。
            "usb-1a86_USB_Single_Serial_5A7C121427-if01",
            // 更长的伪串号：`contains("5A7C121427")` 为真。
            "usb-1a86_USB_Single_Serial_5A7C1214270-if00",
            // 串号被包在另一段里。
            "usb-1a86_USB_Single_Serial_X5A7C121427X-if00",
            // 厂商段不同。
            "usb-Other_Vendor_5A7C121427-if00",
        ] {
            let (_d, p) = mutated(arm, real, fake);
            assert!(
                std::fs::read_to_string(&p).unwrap().contains("5A7C121427"),
                "{fake}: 前提是串号仍在路径里（否则测不到 contains 的漏网）"
            );
            assert_eq!(
                field_err(arm, &p).0,
                "[[hardware]].address",
                "{fake} 必须被拒"
            );
        }
    }

    /// **round 2 P0-1**：`calibration_paths` 住 extras，`s0_daemon` 对
    /// 缺失 / 非数组 / 元素非字符串一律**静默折叠成空数组**，而空数组在
    /// feetech factory 里等价于"无标定照常启动"。五档全拒。
    #[test]
    fn calibration_paths_must_be_exactly_this_arm_file() {
        const FIELD: &str = "[[hardware]].calibration_paths";
        let arm = DeployArm::Left;
        let real = "\"/home/loosqk/repo/dimensional-applications/calibration/left_arm.json\",";

        // ① 整段缺失。
        let (_d, p) = mutated(
            arm,
            "calibration_paths = [",
            "removed_calibration_paths = [",
        );
        assert_eq!(field_err(arm, &p).0, FIELD, "缺失必须拒");

        // ② 错臂标定文件。
        let (_d, p) = mutated(arm, "left_arm.json", "right_arm.json");
        assert_eq!(field_err(arm, &p).0, FIELD, "错臂必须拒");

        // ③ 多一项（arm-only 不需要 head.json）。
        let (_d, p) = mutated(
            arm,
            real,
            "\"/home/loosqk/repo/dimensional-applications/calibration/left_arm.json\",\n    \"/home/loosqk/repo/dimensional-applications/calibration/head.json\",",
        );
        assert_eq!(field_err(arm, &p).0, FIELD, "多项必须拒");

        // ④ 类型错：写成裸字符串而非数组（extras 侧 as_array 会 None）。
        let (_d, p) = mutated(
            arm,
            "calibration_paths = [\n    \"/home/loosqk/repo/dimensional-applications/calibration/left_arm.json\",\n]",
            "calibration_paths = \"/home/loosqk/repo/dimensional-applications/calibration/left_arm.json\"",
        );
        assert_eq!(field_err(arm, &p).0, FIELD, "非数组必须拒");

        // ⑤ 元素类型错 + 空数组。
        for bad in ["123,", ""] {
            let (_d, p) = mutated(arm, real, bad);
            assert_eq!(field_err(arm, &p).0, FIELD, "{bad:?} 必须拒");
        }

        // ⑥ 相对路径（会随 unit WorkingDirectory 漂移）。
        let (_d, p) = mutated(arm, real, "\"calibration/left_arm.json\",");
        assert_eq!(field_err(arm, &p).0, FIELD, "相对路径必须拒");
    }

    /// 缺 address / 多个 `[[hardware]]` 段。
    #[test]
    fn missing_address_and_multiple_hardware_sections_are_rejected() {
        let arm = DeployArm::Left;
        let (_d, p) = mutated(
            arm,
            "address     = \"/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C121427-if00\"",
            "# address removed",
        );
        assert_eq!(field_err(arm, &p).0, "[[hardware]].address");

        // 复制一整份 [[hardware]] 段 → 段数 2。
        let text = std::fs::read_to_string(repo_config(arm)).unwrap();
        let start = text.find("[[hardware]]").unwrap();
        let end = text.find("\n[s1.profile]").unwrap();
        let block = text[start..end].to_string();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("two_hw.toml");
        std::fs::write(&path, format!("{text}\n{block}\n")).unwrap();
        assert!(matches!(
            load_and_verify_arm_config(arm, &path).expect_err("必须拒绝"),
            IdentityError::HardwareCount { found: 2, .. }
        ));
    }

    #[test]
    fn full_configs_match_their_bus_and_keep_the_arm_calibration() {
        for arm in [DeployArm::Left, DeployArm::Right] {
            let loaded = full_config(arm);
            let verified = verify_arm_config(arm, &loaded).expect("本侧完整总线应通过");
            assert_eq!(verified.robot_id, loaded.parsed.robot.id);
            assert_eq!(verified.hardware_id, loaded.parsed.hardware[0].id);
            assert_eq!(verified.address, expected(arm).address);
            assert!(
                verified
                    .calibration_path
                    .ends_with(expected(arm).calibration_file)
            );
            let other = match arm {
                DeployArm::Left => DeployArm::Right,
                DeployArm::Right => DeployArm::Left,
            };
            assert!(matches!(
                verify_arm_config(other, &loaded),
                Err(IdentityError::FieldMismatch {
                    field: "[robot].id",
                    ..
                })
            ));
        }
        // 轮位仍来自配置；部署校验不得另冻结前/左/右轮排列。
        let mut reordered = full_config(DeployArm::Right);
        reordered.parsed.hardware[0].motor_ids[6..].rotate_left(1);
        verify_arm_config(DeployArm::Right, &reordered).expect("合法轮位排列应通过");
    }

    #[test]
    fn full_configs_reject_wrong_bus_enable_mode_and_incomplete_layout() {
        for arm in [DeployArm::Left, DeployArm::Right] {
            let original = full_config(arm);
            for auto_enable in [None, Some(true)] {
                let mut loaded = original.clone();
                loaded.parsed.hardware[0].auto_enable = auto_enable;
                assert!(verify_arm_config(arm, &loaded).is_err());
            }
            let mut loaded = original.clone();
            loaded.parsed.hardware[0].feetech_bus2_mixed_raw = arm != DeployArm::Right;
            assert!(verify_arm_config(arm, &loaded).is_err());

            let mut loaded = original.clone();
            loaded.parsed.hardware[0].address = Some("/dev/ttyACM0".to_string());
            assert!(verify_arm_config(arm, &loaded).is_err());

            let mut loaded = original.clone();
            loaded.parsed.hardware[0].id = expected(arm).hardware_id.to_string();
            assert!(verify_arm_config(arm, &loaded).is_err());

            let mut loaded = original.clone();
            loaded.parsed.hardware[0].motor_ids.pop();
            assert!(verify_arm_config(arm, &loaded).is_err());

            let mut loaded = original.clone();
            loaded.parsed.hardware[0].joints[0] = "unknown_joint".to_string();
            assert!(verify_arm_config(arm, &loaded).is_err());

            let mut loaded = original;
            loaded.parsed.hardware[0].motor_ids[1] = loaded.parsed.hardware[0].motor_ids[0];
            assert!(verify_arm_config(arm, &loaded).is_err());
        }
    }

    #[test]
    fn full_configs_require_both_correct_absolute_calibration_paths() {
        for arm in [DeployArm::Left, DeployArm::Right] {
            let original = full_config(arm);
            let mut loaded = original.clone();
            loaded.parsed.hardware[0].extras.remove("calibration_paths");
            assert!(verify_arm_config(arm, &loaded).is_err());

            for bad in [
                toml::Value::Array(vec![]),
                toml::Value::String("/calibration/left_arm.json".to_string()),
                toml::Value::Array(vec![toml::Value::Integer(1)]),
            ] {
                let mut loaded = original.clone();
                loaded.parsed.hardware[0]
                    .extras
                    .insert("calibration_paths".to_string(), bad);
                assert!(verify_arm_config(arm, &loaded).is_err());
            }
            for bad_second in ["wrong.json", "head.json", "base_left9_right7.json"] {
                let mut loaded = original.clone();
                let paths = loaded.parsed.hardware[0].extras["calibration_paths"]
                    .as_array_mut()
                    .unwrap();
                paths[1] = toml::Value::String(bad_second.to_string());
                assert!(verify_arm_config(arm, &loaded).is_err(), "相对路径必须拒绝");
            }
            let mut loaded = original.clone();
            let paths = loaded.parsed.hardware[0].extras["calibration_paths"]
                .as_array_mut()
                .unwrap();
            paths.pop();
            assert!(
                verify_arm_config(arm, &loaded).is_err(),
                "缺第二份标定必须拒绝"
            );

            let mut loaded = original;
            let paths = loaded.parsed.hardware[0].extras["calibration_paths"]
                .as_array_mut()
                .unwrap();
            paths[1] = toml::Value::String("/calibration/wrong.json".to_string());
            assert!(verify_arm_config(arm, &loaded).is_err(), "错侧标定必须拒绝");
        }
    }

    #[test]
    fn full_config_sensors_use_the_existing_ownership_and_parameter_checks() {
        for arm in [DeployArm::Left, DeployArm::Right] {
            let mut loaded = full_config(arm);
            loaded.parsed.sensor[0].hardware_id = expected(arm).hardware_id.to_string();
            assert!(
                verify_arm_config(arm, &loaded).is_err(),
                "不得指向arm-only实例"
            );

            let mut loaded = full_config(arm);
            loaded.parsed.sensor[0].fps = 0;
            assert!(
                verify_arm_config(arm, &loaded).is_err(),
                "无效采集参数必须拒绝"
            );
        }
    }

    /// 读不到 config → `Load`（不是 panic，也不是"当作通过"）。
    #[test]
    fn unreadable_config_is_a_load_error() {
        let dir = TempDir::new().unwrap();
        let err = load_and_verify_arm_config(DeployArm::Left, &dir.path().join("nope.toml"))
            .expect_err("必须拒绝");
        assert!(matches!(err, IdentityError::Load { .. }), "{err}");
    }
}
