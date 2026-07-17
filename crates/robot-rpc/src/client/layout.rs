//! client 侧 layout 权威派生 + `RobotState.layout_id` 校验
//! （v0.5 候选 E 段(4)，Spec P.2.4 / D1 / PD-E5(a)）。
//!
//! **config 派生是唯一权威**：client 从与 daemon 同一份 config TOML 派生
//! `HardwareLayout`（joints / motor_ids / `layout_id`），对 `GetState` /
//! `StreamState` 返回的 `RobotState.layout_id` 做硬校验——不匹配 = 硬失败
//! （AC-E4），防"读到的状态不是这套关节表"静默错位。

use std::path::{Path, PathBuf};

use robot_msg::joint_layout::compute_layout_id;
use serde::Deserialize;

use crate::proto::RobotState;

/// client 侧关节表视图（config 派生；`layout_id` 由 D1.1 编码算出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardwareLayout {
    pub joints: Vec<String>,
    pub motor_ids: Vec<u8>,
    pub layout_id: u64,
}

/// [`load_hardware_layout`] 的 typed 错误（P.2.4：读失败 / parse / 无该
/// hardware / 内容校验失败四族）。
#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    #[error("read config {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse config TOML: {0}")]
    Parse(String),
    #[error("hardware section id={0:?} not found in config")]
    HardwareIdNotFound(String),
    /// 内容校验失败（与 S0/S1 同判据：非空 / 等长 / 唯一）。
    #[error("hardware section id={id:?} invalid: {reason}")]
    Invalid { id: String, reason: String },
}

/// `RobotState.layout_id` 与 config 派生值不一致（AC-E4 硬失败载体）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "RobotState.layout_id mismatch: expected {expected:#018x} (config 派生), got {got:#018x} —— \
     读到的状态不属于本关节表，硬失败不降级"
)]
pub struct LayoutMismatch {
    pub expected: u64,
    pub got: u64,
}

/// config 的 minimal 视图（serde 忽略未知字段；与 gateway `--config`
/// minimal 视图同款"不 dep robot-s0 schema"边界）。
#[derive(Debug, Deserialize)]
struct ConfigView {
    robot: RobotIdView,
    #[serde(default)]
    hardware: Vec<HardwareView>,
}

#[derive(Debug, Deserialize)]
struct RobotIdView {
    id: String,
}

#[derive(Debug, Deserialize)]
struct HardwareView {
    id: String,
    #[serde(default)]
    joints: Vec<String>,
    #[serde(default)]
    motor_ids: Vec<u8>,
}

/// 从 daemon 同款 config TOML 派生 [`HardwareLayout`]。
///
/// # Errors
///
/// 见 [`LayoutError`] 四族。
pub fn load_hardware_layout(
    config_path: &Path,
    hardware_id: &str,
) -> Result<HardwareLayout, LayoutError> {
    let text = std::fs::read_to_string(config_path).map_err(|source| LayoutError::Read {
        path: config_path.to_path_buf(),
        source,
    })?;
    let view: ConfigView = toml::from_str(&text).map_err(|e| LayoutError::Parse(e.to_string()))?;
    let hw = view
        .hardware
        .into_iter()
        .find(|h| h.id == hardware_id)
        .ok_or_else(|| LayoutError::HardwareIdNotFound(hardware_id.to_string()))?;

    let invalid = |reason: &str| LayoutError::Invalid {
        id: hw.id.clone(),
        reason: reason.to_string(),
    };
    if hw.joints.is_empty() {
        return Err(invalid("joints 为空"));
    }
    if hw.motor_ids.is_empty() {
        return Err(invalid("motor_ids 为空"));
    }
    if hw.joints.len() != hw.motor_ids.len() {
        return Err(invalid("len(motor_ids) != len(joints)"));
    }
    {
        let mut names = std::collections::HashSet::with_capacity(hw.joints.len());
        if !hw.joints.iter().all(|n| names.insert(n.as_str())) {
            return Err(invalid("joint 名重复"));
        }
        let mut ids = [false; 256];
        for &m in &hw.motor_ids {
            if std::mem::replace(&mut ids[usize::from(m)], true) {
                return Err(invalid("motor_id 重复"));
            }
        }
    }

    let pairs: Vec<(&str, u32)> = hw
        .joints
        .iter()
        .map(String::as_str)
        .zip(hw.motor_ids.iter().map(|&m| u32::from(m)))
        .collect();
    let layout_id = compute_layout_id(&view.robot.id, &hw.id, &pairs);
    Ok(HardwareLayout {
        joints: hw.joints,
        motor_ids: hw.motor_ids,
        layout_id,
    })
}

/// `RobotState.layout_id` 硬校验（AC-E4）：不匹配 → `Err(LayoutMismatch)`，
/// 调用方**不得降级**继续消费该 state。
///
/// # Errors
///
/// [`LayoutMismatch`]。
pub fn validate_state_layout(
    expected: &HardwareLayout,
    state: &RobotState,
) -> Result<(), LayoutMismatch> {
    if state.layout_id == expected.layout_id {
        Ok(())
    } else {
        Err(LayoutMismatch {
            expected: expected.layout_id,
            got: state.layout_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{LayoutError, load_hardware_layout, validate_state_layout};
    use crate::proto::RobotState;
    use robot_msg::joint_layout::compute_layout_id;
    use std::io::Write;

    fn write_temp(toml: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        f
    }

    const OK_TOML: &str = r#"
[robot]
id             = "xlerobot_left"
config_version = "0.5"

[[hardware]]
id        = "left_arm"
adapter   = "feetech_sts3215"
joints    = ["a", "b"]
motor_ids = [1, 2]
"#;

    #[test]
    fn load_derives_same_as_compute() {
        let f = write_temp(OK_TOML);
        let layout = load_hardware_layout(f.path(), "left_arm").unwrap();
        assert_eq!(layout.joints, vec!["a", "b"]);
        assert_eq!(layout.motor_ids, vec![1, 2]);
        assert_eq!(
            layout.layout_id,
            compute_layout_id("xlerobot_left", "left_arm", &[("a", 1), ("b", 2)])
        );
    }

    #[test]
    fn load_error_families() {
        // 无该 hardware
        let f = write_temp(OK_TOML);
        assert!(matches!(
            load_hardware_layout(f.path(), "nope").unwrap_err(),
            LayoutError::HardwareIdNotFound(_)
        ));
        // parse 错
        let f = write_temp("not toml ][");
        assert!(matches!(
            load_hardware_layout(f.path(), "x").unwrap_err(),
            LayoutError::Parse(_)
        ));
        // 内容校验失败（等长）
        let f = write_temp(
            "[robot]\nid = \"r\"\n\n[[hardware]]\nid = \"h\"\n\
             joints = [\"a\"]\nmotor_ids = [1, 2]\n",
        );
        assert!(matches!(
            load_hardware_layout(f.path(), "h").unwrap_err(),
            LayoutError::Invalid { .. }
        ));
        // 读失败
        assert!(matches!(
            load_hardware_layout(std::path::Path::new("/nonexistent/x.toml"), "h").unwrap_err(),
            LayoutError::Read { .. }
        ));
    }

    #[test]
    fn validate_mismatch_hard_fail() {
        let f = write_temp(OK_TOML);
        let layout = load_hardware_layout(f.path(), "left_arm").unwrap();
        let ok_state = RobotState {
            layout_id: layout.layout_id,
            ..Default::default()
        };
        validate_state_layout(&layout, &ok_state).unwrap();
        let bad_state = RobotState {
            layout_id: layout.layout_id ^ 1,
            ..Default::default()
        };
        let err = validate_state_layout(&layout, &bad_state).unwrap_err();
        assert_eq!(err.expected, layout.layout_id);
        assert_eq!(err.got, layout.layout_id ^ 1);
    }
}
