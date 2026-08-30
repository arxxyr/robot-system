//! **D2 身份绑定校验**（v0.5 候选 E 段(9)，reviewer round 1 P0-1 回修）。
//!
//! ## 为什么必须有
//!
//! [`DeployArm`] 决定**安全写序**（左 `[T1..T8]` / 右
//! `[T1..T6, V7,T7, V8,T8, V9,T9]`），config 决定**打开哪条物理串口**。
//! 首版两者**互不校验**：`--arm left --config …right_arm.toml` 会打开 bus2
//! 却执行左侧写序——**干扰已在跑的右臂，且漏掉 wheel ID 9**（右侧 7-9 的
//! 零速+断扭全不发）。这是纯静默的错 bus 操作，没有任何运行期信号。
//!
//! 故：**打开串口之前**先按 D2 身份表逐字校验 config，失败即 fail-closed。
//!
//! ## D2 身份表（Spec D2 拍板表，本模块是代码侧唯一副本）
//!
//! | 项 | 左臂 | 右臂 |
//! |---|---|---|
//! | `robot_id` | `xlerobot_left` | `xlerobot_right` |
//! | `hardware_id` | `left_arm` | `right_arm` |
//! | adapter | `feetech_sts3215` | 同 |
//! | 串口 by-id（**完整路径逐字**） | `…usb-1a86_USB_Single_Serial_5A7C121427-if00`（bus1） | `…_5A7C122340-if00`（bus2） |
//! | `calibration_paths` | **恰好一项** `left_arm.json` | **恰好一项** `right_arm.json` |
//! | `motor_ids` | `[1,2,3,4,5,6]` | `[1,2,3,4,5,6]` |
//! | joints | `left_arm_*` 6 项 | `right_arm_*` 6 项（同构） |
//! | `auto_enable` | `false`（D15-R (c1+) 安全边界 ②） | 同 |
//!
//! `motor_ids` 两侧相同**正是**这条校验存在的理由：单看电机号无从判归属，
//! 只有 `robot_id` / `hardware_id` / 串口路径 / joint 名前缀能定死是哪条 bus。
//!
//! ## 为什么串口要逐字比而不是 `contains(串号)`（round 2 P0-1）
//!
//! 子串匹配会放行**伪后缀**（`…_5A7C121427-if01`）与**更长的伪串号**
//! （`…_5A7C1214270-if00`）——两者都是"串号出现过"但指向另一个设备节点。
//! by-id 路径是稳定且完整可枚举的，直接**整串相等**。
//!
//! ## 为什么必须校验 `calibration_paths`（round 2 P0-1）
//!
//! 该字段住在 `[[hardware]].extras`，`s0_daemon` 读它时是
//! `get(..).and_then(as_array).map(filter_map(as_str)).unwrap_or_default()`
//! ——**缺失 / 非数组 / 元素非字符串一律静默变成空数组**；而 feetech factory
//! 对空数组的语义是"无标定、不做 clamp"，于是**照常启动一个未标定的
//! adapter**。整条链上没有任何报错。故身份校验必须把它钉死：恰好一项、
//! 绝对路径、文件名 == 本臂标定文件。
//!
//! **本校验不覆盖 `[[hardware]].motor_ids` 与安全写序的关系**——写序取
//! [`DeployArm::all_ids`]（本 bus 全量 8 / 9 个），config 的 6 个只是本臂
//! 关节；两者本就不同源（D13 round 8 P0-2 扩程）。

use std::path::Path;

use thiserror::Error;

use super::types::DeployArm;
use crate::config_loader::{ConfigLoadError, HardwareSection, LoadedConfig, load_robot_config};

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
    /// 本臂标定文件绝对路径（`calibration_paths` 唯一一项）。
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

/// 按 D2 身份表逐字校验一份已加载的 arm config。
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

    if loaded.parsed.robot.id != want.robot_id {
        return Err(mismatch(
            arm,
            "[robot].id",
            want.robot_id,
            &loaded.parsed.robot.id,
        ));
    }

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

    if hardware.id != want.hardware_id {
        return Err(mismatch(
            arm,
            "[[hardware]].id",
            want.hardware_id,
            &hardware.id,
        ));
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

    let calibration_path = verify_calibration_paths(arm, want, hardware)?;

    if hardware.motor_ids != want.motor_ids {
        return Err(mismatch(
            arm,
            "[[hardware]].motor_ids",
            want.motor_ids,
            &hardware.motor_ids,
        ));
    }
    if hardware.joints != want.joints {
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

    Ok(VerifiedArmConfig {
        arm,
        robot_id: loaded.parsed.robot.id.clone(),
        hardware_id: hardware.id.clone(),
        address: address.to_string(),
        calibration_path,
    })
}

/// `calibration_paths` 必须是**恰好一项、绝对路径、文件名 == 本臂标定文件**
/// 的数组（round 2 P0-1）。
///
/// 该字段住 `extras`，`s0_daemon` 的读法把"缺失 / 非数组 / 元素非字符串"
/// 全部静默折叠成空数组，而空数组在 feetech factory 里等价于"无标定照常
/// 启动"——所以这里的每一档都必须显式拒绝。
fn verify_calibration_paths(
    arm: DeployArm,
    want: &ArmIdentity,
    hardware: &HardwareSection,
) -> Result<String, IdentityError> {
    const FIELD: &str = "[[hardware]].calibration_paths";
    let expected = format!("[<abs dir>/{}]", want.calibration_file);

    let Some(value) = hardware.extras.get("calibration_paths") else {
        return Err(mismatch(arm, FIELD, expected, "<missing>"));
    };
    let Some(array) = value.as_array() else {
        return Err(mismatch(arm, FIELD, expected, value));
    };
    // 恰好一项：arm-only 不需要 head.json / base.json（D2），多一项即错配。
    let [only] = array.as_slice() else {
        return Err(mismatch(arm, FIELD, expected, array));
    };
    let Some(text) = only.as_str() else {
        return Err(mismatch(arm, FIELD, expected, only));
    };
    let path = Path::new(text);
    if !path.is_absolute() {
        return Err(mismatch(arm, FIELD, expected, text));
    }
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if file_name != want.calibration_file {
        return Err(mismatch(arm, FIELD, want.calibration_file, file_name));
    }
    Ok(text.to_string())
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

    /// 读不到 config → `Load`（不是 panic，也不是"当作通过"）。
    #[test]
    fn unreadable_config_is_a_load_error() {
        let dir = TempDir::new().unwrap();
        let err = load_and_verify_arm_config(DeployArm::Left, &dir.path().join("nope.toml"))
            .expect_err("必须拒绝");
        assert!(matches!(err, IdentityError::Load { .. }), "{err}");
    }
}
