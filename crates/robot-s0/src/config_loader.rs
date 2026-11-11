//! `robot-s0::config_loader` —— `configs/<robot>.toml` 读取 + `ConfigIdentity` 构造。
//!
//! 解析留在 S0，`robot-core` 只提供配置指纹等契约类型。
//! `[[hardware]]` 保留适配器专用字段；`[[sensor]]` 只接受当前支持的
//! `opencv_v4l2` BGR8 相机，并在启动 worker 前校验归属、唯一性与采集参数。
//! 共享的 `[s1]` 段交 S1 验证，未知顶层字段拒绝，避免旧 `[camera]` 被静默忽略。
//! `config_hash` 始终取原始 TOML 字节；其余指纹沿既有部分身份合同。

use std::fs;
use std::path::{Path, PathBuf};

use robot_core::config::IdentityFingerprint;
use serde::{Deserialize, Serialize};

// ============================================================================
// TOML schema
// ============================================================================

/// 顶层配置：`[robot]` + `[[hardware]]` + `[[sensor]]`。
///
/// `derive(Serialize)` 仅给单测用（roundtrip 断言）；运行时 `robot-s0` 不写
/// TOML 回去。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RobotConfigToml {
    pub robot: RobotSection,
    #[serde(default)]
    pub hardware: Vec<HardwareSection>,
    #[serde(default)]
    pub sensor: Vec<SensorSection>,
    /// 同份配置的 S1 参数由 S1 解析；S0 只保留该合法顶层段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s1: Option<toml::Table>,
    /// `[monitor]` 段（task-31 B'b 加）：系统健康采样配置。缺省时 `robot-s0`
    /// 跑默认值（5 s 间隔 / `top_n=5` / 空 `interested_processes`）。
    #[serde(default)]
    pub monitor: MonitorSection,
}

/// `[robot]` 段：v0.2 仅这两字段被 `ConfigIdentity` 直接消费。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct RobotSection {
    pub id: String,
    pub config_version: String,
}

/// 归属一个硬件实例的 `OpenCV` BGR8 相机；路径明确，不借用 shell 环境。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SensorSection {
    pub hardware_id: String,
    pub id: String,
    pub adapter: String,
    pub device: PathBuf,
    pub python: PathBuf,
    pub helper: PathBuf,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl SensorSection {
    #[must_use]
    pub fn camera_config(&self) -> crate::camera::CameraConfig {
        crate::camera::CameraConfig {
            id: self.id.clone(),
            device: self.device.clone(),
            python: self.python.clone(),
            helper: self.helper.clone(),
            width: self.width,
            height: self.height,
            fps: self.fps,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SensorSectionError {
    #[error("[[sensor]] hardware_id={id} 没有匹配的硬件实例")]
    UnknownHardwareId { id: String },
    #[error("[[sensor]] id={id} 使用不支持的 adapter={adapter}")]
    UnsupportedAdapter { id: String, adapter: String },
    #[error("[[sensor]] id 重复：{id}")]
    DuplicateId { id: String },
    #[error("[[sensor]] device 路径重复：{device:?}")]
    DuplicateDevice { device: PathBuf },
    #[error("[[sensor]] id={id} 配置无效：{source}")]
    InvalidConfig {
        id: String,
        source: robot_core::error::RobotError,
    },
}

pub fn validate_camera_sections(cfg: &RobotConfigToml) -> Result<(), SensorSectionError> {
    let mut ids = std::collections::HashSet::with_capacity(cfg.sensor.len());
    let mut devices = std::collections::HashSet::with_capacity(cfg.sensor.len());
    for camera in &cfg.sensor {
        if camera.adapter != "opencv_v4l2" {
            return Err(SensorSectionError::UnsupportedAdapter {
                id: camera.id.clone(),
                adapter: camera.adapter.clone(),
            });
        }
        if resolve_hardware_section(cfg, &camera.hardware_id).is_none() {
            return Err(SensorSectionError::UnknownHardwareId {
                id: camera.hardware_id.clone(),
            });
        }
        camera
            .camera_config()
            .validate()
            .map_err(|source| SensorSectionError::InvalidConfig {
                id: camera.id.clone(),
                source,
            })?;
        if !ids.insert(camera.id.as_str()) {
            return Err(SensorSectionError::DuplicateId {
                id: camera.id.clone(),
            });
        }
        if !devices.insert(camera.device.as_path()) {
            return Err(SensorSectionError::DuplicateDevice {
                device: camera.device.clone(),
            });
        }
    }
    Ok(())
}

/// 仅匹配的 S0 硬件实例拥有相机；其他实例不会打开同一设备。
#[must_use]
pub fn resolve_camera_sections<'a>(
    cfg: &'a RobotConfigToml,
    hardware_id: &str,
) -> Vec<&'a SensorSection> {
    cfg.sensor
        .iter()
        .filter(|camera| camera.hardware_id == hardware_id)
        .collect()
}

/// `[[hardware]]` 段：通用字段强类型化，适配器专用字段由现有工厂读取 `extras`。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct HardwareSection {
    pub id: String,
    pub adapter: String,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub joints: Vec<String>,
    /// **v0.5 候选 E 段(2)（D1/PC1）**：显式 motor id 表，与 `joints` 按
    /// config 顺序一一对应（`layout_id` 编码输入之一）。**必填、无
    /// default**——缺失 = TOML parse 错误（S0 config 错误族 exit 2，撤
    /// extras `filter_map` 静默丢弃）；内容校验（等长/非空/唯一）见
    /// [`validate_hardware_section`]。
    pub motor_ids: Vec<u8>,
    #[serde(default)]
    pub calibration: Option<String>,
    /// 与 `joints/motor_ids` 同序的校准边界容差（raw）；省略为全0。
    #[serde(default)]
    pub calibration_range_tolerances_raw: Option<Vec<u16>>,
    /// 显式启用右总线六臂位置＋三轮0/+500 raw合同；省略保持旧Position路径。
    #[serde(default)]
    pub feetech_bus2_mixed_raw: bool,
    /// **v0.5 候选 C**：启动是否自动 enable（design §7.2 `auto_enable`，B2 typed 化，
    /// 不再走 extras）。`None`（缺省）= **true**（保留 `robot-s0` 既有无条件启动
    /// auto-enable 行为）；`Some(false)` → 启动留 Safe 等 operator `SetEnabled(true)`。
    #[serde(default)]
    pub auto_enable: Option<bool>,
    /// 串口执行线程的严格实时调度；省略时保持既有 CLI 配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial_rt: Option<SerialRtConfig>,
    /// 适配器专用字段（`baud` / `poll_hz` / `wheel_radius` 等）。
    #[serde(flatten, default)]
    pub extras: toml::Table,
}

/// 仅配置执行线程；IPC、相机和日志线程保持普通调度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SerialRtConfig {
    pub cpu: usize,
    pub fifo_priority: u8,
}

/// 按 resolved `hardware_id` 在 config 里 match `[[hardware]].id`（v0.5 候选 C B6：
/// `build_adapter` + `auto_enable` 用**同一** matched 段，修既有 `.first()` 错配）。
/// 无匹配返 `None`（调用方 fail-fast；CLI `--hardware-id` 与 config 错配）。
#[must_use]
pub fn resolve_hardware_section<'a>(
    cfg: &'a RobotConfigToml,
    hardware_id: &str,
) -> Option<&'a HardwareSection> {
    cfg.hardware.iter().find(|h| h.id == hardware_id)
}

/// resolved `[[hardware]]` 段内容校验错误（v0.5 候选 E 段(2)，PC1）。
///
/// 全族 fail-fast：S0 侧统一 config 错误族 **exit 2**（映射单测锁
/// [`Self::exit_code`]，AC-E3）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HardwareSectionError {
    #[error("[[hardware]] serial_rt.cpu={cpu} 超过 CPU 集合上限1023")]
    InvalidSerialRtCpu { cpu: usize },
    #[error("[[hardware]] serial_rt.fifo_priority={priority} 不在1..=99")]
    InvalidSerialRtPriority { priority: u8 },
    #[error("[[hardware]] joints={actual} 超过上限 {max}")]
    TooManyJoints { actual: usize, max: usize },
    /// `joints` 为空（无法派生 layout / 驱动任何关节）。
    #[error("[[hardware]] id={id}: joints 为空")]
    EmptyJoints { id: String },
    /// `motor_ids` 为空（typed 必填字段内容为空数组）。
    #[error("[[hardware]] id={id}: motor_ids 为空")]
    EmptyMotorIds { id: String },
    /// `motor_ids` 与 `joints` 长度不等（一一对应关系破坏）。
    #[error("[[hardware]] id={id}: len(motor_ids)={motor_ids} != len(joints)={joints}")]
    LenMismatch {
        id: String,
        joints: usize,
        motor_ids: usize,
    },
    /// joint 名重复（layout 编码 / by-name 寻址二义）。
    #[error("[[hardware]] id={id}: joint 名重复 {name:?}")]
    DupJointName { id: String, name: String },
    /// motor id 重复（总线寻址冲突）。
    #[error("[[hardware]] id={id}: motor_id 重复 {motor_id}")]
    DupMotorId { id: String, motor_id: u8 },
    #[error("[[hardware]] id={id}: 校准边界容差须与 joints 等长且每项在0..=4095")]
    InvalidCalibrationTolerances { id: String },
    #[error("[[hardware]] id={id}: feetech_bus2_mixed_raw仅适用于feetech_sts3215")]
    MixedRawRequiresFeetech { id: String },
    #[error("[[hardware]] id={id}: 位置/raw混合模式需要完整、匹配的右总线布局")]
    InvalidMixedBus2Layout { id: String },
}

impl HardwareSectionError {
    /// error → 退出码映射（AC-E3）：S0 config 内容错误族 = **2**。
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        2
    }
}

impl ConfigLoadError {
    /// error → 退出码映射（AC-E3）：S0 config 加载错误族 = **2**。
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        2
    }
}

/// resolved `[[hardware]]` 段内容校验（v0.5 候选 E 段(2)，Spec P.2.2）：
/// `joints` 非空 / `motor_ids` 非空 / 两表等长 / joint 名唯一 / motor id 唯一
/// （u8 域由类型承担）。首个 violation 即返回。
///
/// # Errors
///
/// 见 [`HardwareSectionError`] 五 variant。
pub fn validate_hardware_section(hw: &HardwareSection) -> Result<(), HardwareSectionError> {
    if let Some(rt) = hw.serial_rt {
        if rt.cpu >= 1024 {
            return Err(HardwareSectionError::InvalidSerialRtCpu { cpu: rt.cpu });
        }
        if !(1..=99).contains(&rt.fifo_priority) {
            return Err(HardwareSectionError::InvalidSerialRtPriority {
                priority: rt.fifo_priority,
            });
        }
    }
    if hw.feetech_bus2_mixed_raw && hw.adapter != "feetech_sts3215" {
        return Err(HardwareSectionError::MixedRawRequiresFeetech { id: hw.id.clone() });
    }
    if hw.joints.len() > robot_core::adapter::MAX_JOINTS {
        return Err(HardwareSectionError::TooManyJoints {
            actual: hw.joints.len(),
            max: robot_core::adapter::MAX_JOINTS,
        });
    }
    if hw.joints.is_empty() {
        return Err(HardwareSectionError::EmptyJoints { id: hw.id.clone() });
    }
    if hw.motor_ids.is_empty() {
        return Err(HardwareSectionError::EmptyMotorIds { id: hw.id.clone() });
    }
    if hw.motor_ids.len() != hw.joints.len() {
        return Err(HardwareSectionError::LenMismatch {
            id: hw.id.clone(),
            joints: hw.joints.len(),
            motor_ids: hw.motor_ids.len(),
        });
    }
    if let Some(values) = &hw.calibration_range_tolerances_raw
        && (values.len() != hw.joints.len() || values.iter().any(|&value| value > 4095))
    {
        return Err(HardwareSectionError::InvalidCalibrationTolerances { id: hw.id.clone() });
    }
    let mut seen_names = std::collections::HashSet::with_capacity(hw.joints.len());
    for name in &hw.joints {
        if !seen_names.insert(name.as_str()) {
            return Err(HardwareSectionError::DupJointName {
                id: hw.id.clone(),
                name: name.clone(),
            });
        }
    }
    let mut seen_ids = [false; 256];
    for &motor_id in &hw.motor_ids {
        if seen_ids[usize::from(motor_id)] {
            return Err(HardwareSectionError::DupMotorId {
                id: hw.id.clone(),
                motor_id,
            });
        }
        seen_ids[usize::from(motor_id)] = true;
    }
    if hw.feetech_bus2_mixed_raw {
        crate::grouped_position::XleRobotBus2Layout::from_hardware(hw)
            .map_err(|_| HardwareSectionError::InvalidMixedBus2Layout { id: hw.id.clone() })?;
    }
    Ok(())
}

/// `[monitor]` 段（task-31 B'b）：`robot-health` 采样配置。
///
/// 示例：
///
/// ```toml
/// [monitor]
/// interval_secs = 5
/// top_n = 5
/// interested_processes = ["robot-s0", "robot-mock-s1", "robot-ctl"]
/// ```
///
/// 所有字段缺省 → `MonitorSection::default()`（5 s / top 5 / 空列表）。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct MonitorSection {
    #[serde(default = "default_monitor_interval_secs")]
    pub interval_secs: u64,
    #[serde(default = "default_monitor_top_n")]
    pub top_n: usize,
    #[serde(default)]
    pub interested_processes: Vec<String>,
}

impl Default for MonitorSection {
    fn default() -> Self {
        Self {
            interval_secs: default_monitor_interval_secs(),
            top_n: default_monitor_top_n(),
            interested_processes: Vec::new(),
        }
    }
}

const fn default_monitor_interval_secs() -> u64 {
    5
}
const fn default_monitor_top_n() -> usize {
    5
}

// ============================================================================
// 加载结果
// ============================================================================

/// `load_robot_config` 返回值：解析后类型 + 原始 bytes + 算出的 `ConfigIdentity`。
///
/// 保留 `raw_bytes` 是为了：
/// - hash 由调用方需要时重算（默认已经在 `identity.config_hash` 里）；
/// - v0.3 跨进程 handshake 时 recorder 要往 record 文件头写完整 TOML（设计
///   §10.3 record 头含 `ConfigIdentity`）。
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub source_path: PathBuf,
    pub raw_bytes: Vec<u8>,
    pub parsed: RobotConfigToml,
    pub identity: IdentityFingerprint,
}

/// 加载错误。`thiserror` derive 让 `robot-s0` 可以 `tracing::error!(?err)`
/// 拿到结构化字段 + Display 人类可读消息（详 [`docs/logging.md`](../../../docs/logging.md)
/// 字段使用约定）。
#[derive(Debug, thiserror::Error)]
pub enum ConfigLoadError {
    #[error("read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("config bytes are not valid UTF-8 ({path}): {source}")]
    Utf8 {
        path: PathBuf,
        #[source]
        source: std::str::Utf8Error,
    },
    #[error("parse TOML ({path}): {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("传感器配置无效 ({path}): {source}")]
    Sensor {
        path: PathBuf,
        #[source]
        source: SensorSectionError,
    },
}

// ============================================================================
// 入口
// ============================================================================

/// 读 + 解析 + 校验传感器配置，再算 `ConfigIdentity`。
///
/// `config_hash` 用 *原始 bytes* 算（不是解析后再 canonical 化）—— 即"对 TOML
/// 文件的 byte-exact 同一性"；任何空白 / 注释 / 字段顺序改动都改变 hash。
/// 设计 §7.3 没强制 hash 的 canonical 化方式，"原始 bytes" 是最简单 + 可重现的
/// 选择。v0.3 跨进程 handshake 落地时若发现"开发机和部署机的 line-ending /
/// trailing newline 差导致 hash 不同"成痛点，再升 canonical 化。
///
/// # Errors
///
/// 见 [`ConfigLoadError`]：文件读失败、非 UTF-8、TOML 或传感器配置错误。
pub fn load_robot_config(path: &Path) -> Result<LoadedConfig, ConfigLoadError> {
    let raw_bytes = fs::read(path).map_err(|source| ConfigLoadError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let raw_str = std::str::from_utf8(&raw_bytes).map_err(|source| ConfigLoadError::Utf8 {
        path: path.to_path_buf(),
        source,
    })?;
    let parsed: RobotConfigToml =
        toml::from_str(raw_str).map_err(|source| ConfigLoadError::Toml {
            path: path.to_path_buf(),
            source,
        })?;
    validate_camera_sections(&parsed).map_err(|source| ConfigLoadError::Sensor {
        path: path.to_path_buf(),
        source,
    })?;

    let identity = compute_v02_partial_identity(&raw_bytes, &parsed.robot);

    Ok(LoadedConfig {
        source_path: path.to_path_buf(),
        raw_bytes,
        parsed,
        identity,
    })
}

/// 由 raw TOML bytes + `[robot]` 段构造 v0.2 partial fingerprint。
///
/// 公开是为了让单测和未来 dry-run 工具不必走 `load_robot_config` 整链路。
#[must_use]
pub fn compute_v02_partial_identity(raw_bytes: &[u8], robot: &RobotSection) -> IdentityFingerprint {
    let hash = *blake3::hash(raw_bytes).as_bytes();
    IdentityFingerprint::v02_partial(robot.id.clone(), robot.config_version.clone(), hash)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const SAMPLE_TOML: &str = r#"
[robot]
id = "xlerobot"
config_version = "0.1"

[[hardware]]
id = "left_arm"
adapter = "mock_hard_rt"
address = "/dev/ttyACM0"
joints = ["j1", "j2", "j3"]
motor_ids = [1, 2, 3]
baud = 1000000

[[hardware]]
id = "base"
adapter = "mock_omni3"
joints = ["wheel_1", "wheel_2", "wheel_3"]
motor_ids = [7, 8, 9]
wheel_radius = 0.05
"#;

    fn write_temp(toml: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        f
    }

    const SENSOR_TOML: &str = r#"
[[sensor]]
hardware_id = "left_arm"
id = "head_cam"
adapter = "opencv_v4l2"
device = "/dev/v4l/by-id/test-camera"
python = "/opt/robot-camera/bin/python"
helper = "/opt/robot/scripts/camera_capture.py"
width = 640
height = 480
fps = 30
"#;

    #[test]
    fn camera_sections_are_optional_and_bound_to_their_hardware_instances() {
        let baseline = load_robot_config(write_temp(SAMPLE_TOML).path()).unwrap();
        assert_eq!(baseline.parsed.sensor, [] as [SensorSection; 0]);
        let second = SENSOR_TOML
            .replace("head_cam", "left_wrist")
            .replace("test-camera", "left-wrist");
        let third = SENSOR_TOML
            .replace("head_cam", "right_wrist")
            .replace("test-camera", "right-wrist")
            .replace("left_arm", "base");
        let configured = load_robot_config(
            write_temp(&format!("{SAMPLE_TOML}\n{SENSOR_TOML}\n{second}\n{third}")).path(),
        )
        .unwrap();
        let cameras = resolve_camera_sections(&configured.parsed, "left_arm");
        assert_eq!(
            cameras
                .iter()
                .map(|camera| camera.id.as_str())
                .collect::<Vec<_>>(),
            ["head_cam", "left_wrist"]
        );
        assert_eq!(cameras[0].camera_config().width, 640);
        let other = resolve_camera_sections(&configured.parsed, "base");
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].id, "right_wrist");
        assert_eq!(
            resolve_camera_sections(&configured.parsed, "missing"),
            [] as [&SensorSection; 0]
        );
        let encoded = toml::to_string(&configured.parsed).unwrap();
        let decoded: RobotConfigToml = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.sensor, configured.parsed.sensor);
    }

    #[test]
    fn invalid_camera_configuration_is_rejected_before_any_worker_starts() {
        for (before, after) in [
            ("hardware_id = \"left_arm\"", "hardware_id = \"missing\""),
            ("width = 640", "width = 0"),
            ("height = 480", "height = 4294967295"),
            ("fps = 30", "fps = 0"),
            ("/opt/robot-camera/bin/python", "relative-python"),
            ("id = \"head_cam\"", "id = \"bad/camera\""),
        ] {
            let text = format!("{SAMPLE_TOML}\n{}", SENSOR_TOML.replace(before, after));
            assert!(
                matches!(
                    load_robot_config(write_temp(&text).path()),
                    Err(ConfigLoadError::Sensor { .. })
                ),
                "未拒绝 {after}"
            );
        }
    }

    #[test]
    fn sensor_ids_are_unique_across_hardware_instances() {
        let second = SENSOR_TOML
            .replace("test-camera", "another-camera")
            .replace("left_arm", "base");
        let text = format!("{SAMPLE_TOML}\n{SENSOR_TOML}\n{second}");
        assert!(matches!(
            load_robot_config(write_temp(&text).path()),
            Err(ConfigLoadError::Sensor {
                source: SensorSectionError::DuplicateId { id },
                ..
            }) if id == "head_cam"
        ));
    }

    #[test]
    fn sensor_devices_are_unique_across_hardware_instances() {
        let second = SENSOR_TOML
            .replace("head_cam", "another_camera")
            .replace("left_arm", "base");
        let text = format!("{SAMPLE_TOML}\n{SENSOR_TOML}\n{second}");
        assert!(matches!(
            load_robot_config(write_temp(&text).path()),
            Err(ConfigLoadError::Sensor {
                source: SensorSectionError::DuplicateDevice { device },
                ..
            }) if device == Path::new("/dev/v4l/by-id/test-camera")
        ));
    }

    #[test]
    fn unsupported_sensor_adapter_is_rejected() {
        let sensor = SENSOR_TOML.replace("opencv_v4l2", "realsense_d435i");
        let text = format!("{SAMPLE_TOML}\n{sensor}");
        assert!(matches!(
            load_robot_config(write_temp(&text).path()),
            Err(ConfigLoadError::Sensor {
                source: SensorSectionError::UnsupportedAdapter { adapter, .. },
                ..
            }) if adapter == "realsense_d435i"
        ));
    }

    #[test]
    fn legacy_camera_and_unknown_sensor_fields_are_not_silently_ignored() {
        for sensor in [
            SENSOR_TOML.replace("[[sensor]]", "[camera]"),
            format!("{SENSOR_TOML}\nstreams = [\"color\", \"depth\"]\n"),
            format!("{SENSOR_TOML}\nserial = \"unimplemented\"\n"),
        ] {
            let text = format!("{SAMPLE_TOML}\n{sensor}");
            assert!(matches!(
                load_robot_config(write_temp(&text).path()),
                Err(ConfigLoadError::Toml { .. })
            ));
        }
    }

    #[test]
    fn shared_s1_configuration_remains_accepted() {
        let text = format!("{SAMPLE_TOML}\n[s1.profile]\nbackends = [\"scripted\"]\n");
        let config = load_robot_config(write_temp(&text).path()).unwrap();
        let profile = &config.parsed.s1.as_ref().unwrap()["profile"];
        assert_eq!(profile["backends"][0].as_str(), Some("scripted"));
        let encoded = toml::to_string(&config.parsed).unwrap();
        let decoded: RobotConfigToml = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.s1, config.parsed.s1);
    }

    #[test]
    fn repository_robot_configs_still_parse() {
        for text in [
            include_str!("../../../configs/xlerobot.toml"),
            include_str!("../../../configs/xlerobot-bus1.toml"),
            include_str!("../../../configs/xlerobot-bus2.toml"),
            include_str!("../../../configs/xlerobot-left-arm.toml"),
            include_str!("../../../configs/xlerobot-right-arm.toml"),
            include_str!("../../../configs/scripted-backend-smoke.toml"),
            include_str!("../../../configs/robotctl-smoke.toml"),
            include_str!("../../../configs/crash-restart-smoke.toml"),
            include_str!("../../../configs/clear-estop-e2e.toml"),
            include_str!("../../../configs/ik-backend-smoke.toml"),
        ] {
            let parsed: RobotConfigToml = toml::from_str(text).unwrap();
            validate_camera_sections(&parsed).unwrap();
        }
    }

    #[test]
    fn parse_full_sample_toml() {
        let f = write_temp(&format!("{SAMPLE_TOML}\n{SENSOR_TOML}"));
        let cfg = load_robot_config(f.path()).unwrap();

        assert_eq!(cfg.parsed.robot.id, "xlerobot");
        assert_eq!(cfg.parsed.robot.config_version, "0.1");
        assert_eq!(cfg.parsed.hardware.len(), 2);
        assert_eq!(cfg.parsed.hardware[0].id, "left_arm");
        assert_eq!(cfg.parsed.hardware[0].adapter, "mock_hard_rt");
        assert_eq!(
            cfg.parsed.hardware[0].address.as_deref(),
            Some("/dev/ttyACM0")
        );
        assert_eq!(
            cfg.parsed.hardware[0].joints,
            vec!["j1".to_string(), "j2".into(), "j3".into()]
        );
        // hardware-specific extras 走 flatten 兜底
        assert_eq!(
            cfg.parsed.hardware[0]
                .extras
                .get("baud")
                .and_then(toml::Value::as_integer),
            Some(1_000_000)
        );
        assert_eq!(cfg.parsed.hardware[1].id, "base");
        assert_eq!(
            cfg.parsed.hardware[1]
                .extras
                .get("wheel_radius")
                .and_then(toml::Value::as_float),
            Some(0.05)
        );
        assert_eq!(cfg.parsed.sensor.len(), 1);
        assert_eq!(cfg.parsed.sensor[0].id, "head_cam");
        assert_eq!(
            cfg.parsed.sensor[0].camera_config(),
            crate::camera::CameraConfig {
                id: "head_cam".into(),
                device: "/dev/v4l/by-id/test-camera".into(),
                python: "/opt/robot-camera/bin/python".into(),
                helper: "/opt/robot/scripts/camera_capture.py".into(),
                width: 640,
                height: 480,
                fps: 30,
            }
        );
    }

    #[test]
    fn identity_is_v02_partial_with_blake3_config_hash() {
        let f = write_temp(SAMPLE_TOML);
        let cfg = load_robot_config(f.path()).unwrap();

        assert_eq!(cfg.identity.robot_id, "xlerobot");
        assert_eq!(cfg.identity.config_version, "0.1");
        assert_eq!(
            cfg.identity.config_hash,
            *blake3::hash(SAMPLE_TOML.as_bytes()).as_bytes()
        );
        assert!(cfg.identity.is_fully_partial());
    }

    #[test]
    fn config_hash_changes_when_a_byte_changes() {
        let f1 = write_temp(SAMPLE_TOML);
        // 改一个 hardware id：左 arm → left_armX
        let modified = SAMPLE_TOML.replace(r#"id = "left_arm""#, r#"id = "left_armX""#);
        assert_ne!(SAMPLE_TOML, modified, "test setup: 替换发生");
        let f2 = write_temp(&modified);

        let id1 = load_robot_config(f1.path()).unwrap().identity;
        let id2 = load_robot_config(f2.path()).unwrap().identity;

        assert_eq!(id1.robot_id, id2.robot_id, "robot_id 未变");
        assert_eq!(id1.config_version, id2.config_version);
        assert_ne!(
            id1.config_hash, id2.config_hash,
            "TOML byte 变 → config_hash 必须变"
        );
    }

    #[test]
    fn config_hash_changes_for_pure_whitespace_diff() {
        // 验证文档约定："原始 bytes 算 hash" → 即使语义等价，空白差也改 hash。
        let f1 = write_temp(SAMPLE_TOML);
        let with_extra_blank = SAMPLE_TOML.replace("\n[[hardware]]", "\n\n[[hardware]]");
        assert_ne!(SAMPLE_TOML, with_extra_blank);
        let f2 = write_temp(&with_extra_blank);

        let id1 = load_robot_config(f1.path()).unwrap().identity;
        let id2 = load_robot_config(f2.path()).unwrap().identity;
        assert_ne!(id1.config_hash, id2.config_hash);
    }

    #[test]
    fn missing_robot_section_yields_toml_error() {
        let f = write_temp("[[hardware]]\nid = \"x\"\nadapter = \"y\"\n");
        let err = load_robot_config(f.path()).unwrap_err();
        match err {
            ConfigLoadError::Toml { .. } => {}
            other => panic!("expected Toml error, got {other:?}"),
        }
    }

    #[test]
    fn missing_required_robot_fields_fails() {
        let f = write_temp("[robot]\nid = \"only_id\"\n"); // 缺 config_version
        let err = load_robot_config(f.path()).unwrap_err();
        match err {
            ConfigLoadError::Toml { .. } => {}
            other => panic!("expected Toml error, got {other:?}"),
        }
    }

    #[test]
    fn nonexistent_path_yields_read_error() {
        let path = std::env::temp_dir().join("nonexistent_robot_config_xyz.toml");
        let _ = std::fs::remove_file(&path); // 确保不存在
        let err = load_robot_config(&path).unwrap_err();
        match err {
            ConfigLoadError::Read { .. } => {}
            other => panic!("expected Read error, got {other:?}"),
        }
    }

    #[test]
    fn empty_hardware_and_sensor_arrays_default_ok() {
        let f = write_temp("[robot]\nid = \"only_robot\"\nconfig_version = \"0.1\"\n");
        let cfg = load_robot_config(f.path()).unwrap();
        assert!(
            cfg.parsed.hardware.is_empty(),
            "expected no [[hardware]], got {}",
            cfg.parsed.hardware.len()
        );
        assert!(
            cfg.parsed.sensor.is_empty(),
            "expected no [[sensor]], got {}",
            cfg.parsed.sensor.len()
        );
        assert!(cfg.identity.is_fully_partial());
    }

    #[test]
    fn compute_v02_partial_identity_independent_of_load() {
        // 验证 helper 公开签名能不走 load_robot_config 也用。
        let robot = RobotSection {
            id: "robotX".into(),
            config_version: "1.2.3".into(),
        };
        let raw = b"toml-bytes-doesnt-need-to-be-valid-here";
        let id = compute_v02_partial_identity(raw, &robot);
        assert_eq!(id.robot_id, "robotX");
        assert_eq!(id.config_version, "1.2.3");
        assert_eq!(id.config_hash, *blake3::hash(raw).as_bytes());
        assert!(id.is_fully_partial());
    }

    // ---- v0.5 候选 C：按 hardware_id 选段 + auto_enable typed ----

    #[test]
    fn resolve_hardware_section_by_id() {
        let f = write_temp(SAMPLE_TOML);
        let cfg = load_robot_config(f.path()).unwrap();
        // 命中第二段（**非** first）：证明 build_adapter 不再 .first() 盲取。
        let hw = resolve_hardware_section(&cfg.parsed, "base").unwrap();
        assert_eq!(hw.id, "base");
        assert_eq!(hw.adapter, "mock_omni3");
    }

    #[test]
    fn resolve_hardware_section_no_match_none() {
        let f = write_temp(SAMPLE_TOML);
        let cfg = load_robot_config(f.path()).unwrap();
        assert!(resolve_hardware_section(&cfg.parsed, "nonexistent").is_none());
    }

    #[test]
    fn auto_enable_typed_parsed_false() {
        // auto_enable 走 typed 字段（不落 extras），可显式关掉启动自动上使能。
        let toml = "[robot]\nid = \"r\"\nconfig_version = \"0.1\"\n\n\
                    [[hardware]]\nid = \"arm\"\nadapter = \"mock_hard_rt\"\n\
                    motor_ids = []\nauto_enable = false\n";
        let f = write_temp(toml);
        let cfg = load_robot_config(f.path()).unwrap();
        assert_eq!(cfg.parsed.hardware[0].auto_enable, Some(false));
        // typed 字段不应漏进 flatten extras。
        assert!(!cfg.parsed.hardware[0].extras.contains_key("auto_enable"));
    }

    #[test]
    fn auto_enable_absent_is_none() {
        // SAMPLE_TOML 不写 auto_enable → None（robot-s0 缺省语义 = true）。
        let f = write_temp(SAMPLE_TOML);
        let cfg = load_robot_config(f.path()).unwrap();
        assert_eq!(cfg.parsed.hardware[0].auto_enable, None);
    }

    // ---- v0.5 候选 E 段(2)：motor_ids typed + validate（AC-E3 拒路径）----

    /// 便捷构造：合法段基线，各拒路径在其上单点破坏。
    fn valid_hw() -> HardwareSection {
        let f = write_temp(SAMPLE_TOML);
        let cfg = load_robot_config(f.path()).unwrap();
        cfg.parsed.hardware[0].clone()
    }

    #[test]
    fn serial_rt_configuration_is_typed_and_checked() {
        let mut hw = valid_hw();
        assert!(hw.serial_rt.is_none());
        hw.serial_rt = Some(SerialRtConfig {
            cpu: 4,
            fifo_priority: 70,
        });
        validate_hardware_section(&hw).unwrap();
        let text = toml::to_string(&hw).unwrap();
        let parsed: HardwareSection = toml::from_str(&text).unwrap();
        assert_eq!(parsed.serial_rt, hw.serial_rt);
        assert!(!parsed.extras.contains_key("serial_rt"));
        for cpu in [1024, usize::MAX] {
            hw.serial_rt.as_mut().unwrap().cpu = cpu;
            assert!(matches!(
                validate_hardware_section(&hw),
                Err(HardwareSectionError::InvalidSerialRtCpu { .. })
            ));
        }
        hw.serial_rt.as_mut().unwrap().cpu = 4;
        for priority in [0, 100, 255] {
            hw.serial_rt.as_mut().unwrap().fifo_priority = priority;
            assert!(matches!(
                validate_hardware_section(&hw),
                Err(HardwareSectionError::InvalidSerialRtPriority { .. })
            ));
        }
        for priority in [1, 99] {
            hw.serial_rt.as_mut().unwrap().fifo_priority = priority;
            validate_hardware_section(&hw).unwrap();
        }
    }

    #[test]
    fn serial_rt_requires_complete_known_fields() {
        for value in [
            "{ cpu = 4 }",
            "{ fifo_priority = 70 }",
            "{ cpu = -1, fifo_priority = 70 }",
            "{ cpu = 4, fifo_priority = 70, strict = false }",
        ] {
            let text = format!(
                "serial_rt = {value}\n{}",
                toml::to_string(&valid_hw()).unwrap()
            );
            assert!(toml::from_str::<HardwareSection>(&text).is_err(), "{value}");
        }
    }

    #[test]
    fn motor_ids_typed_parsed_not_in_extras() {
        let f = write_temp(SAMPLE_TOML);
        let cfg = load_robot_config(f.path()).unwrap();
        assert_eq!(cfg.parsed.hardware[0].motor_ids, vec![1, 2, 3]);
        assert_eq!(cfg.parsed.hardware[1].motor_ids, vec![7, 8, 9]);
        assert!(!cfg.parsed.hardware[0].extras.contains_key("motor_ids"));
    }

    #[test]
    fn mixed_raw_is_explicit_typed_and_preserves_default_position() {
        let hw = valid_hw();
        assert!(!hw.feetech_bus2_mixed_raw);
        let mut config: RobotConfigToml =
            toml::from_str(include_str!("../../../configs/xlerobot-bus2.toml")).unwrap();
        let mut enabled = config.hardware.remove(0);
        enabled.feetech_bus2_mixed_raw = true;
        validate_hardware_section(&enabled).unwrap();
        let text = toml::to_string(&enabled).unwrap();
        let decoded: HardwareSection = toml::from_str(&text).unwrap();
        assert!(decoded.feetech_bus2_mixed_raw);
        assert!(!decoded.extras.contains_key("feetech_bus2_mixed_raw"));
        for invalid in ["1", "\"true\"", "[]", "{}"] {
            let malformed = text.replace(
                "feetech_bus2_mixed_raw = true",
                &format!("feetech_bus2_mixed_raw = {invalid}"),
            );
            assert!(toml::from_str::<HardwareSection>(&malformed).is_err());
        }
        enabled.adapter = hw.adapter;
        assert!(matches!(
            validate_hardware_section(&enabled),
            Err(HardwareSectionError::MixedRawRequiresFeetech { .. })
        ));
    }

    #[test]
    fn mixed_raw_rejects_wrong_instance_and_layout_before_factory() {
        let mut config: RobotConfigToml =
            toml::from_str(include_str!("../../../configs/xlerobot-bus2.toml")).unwrap();
        let mut hardware = config.hardware.remove(0);
        hardware.feetech_bus2_mixed_raw = true;
        validate_hardware_section(&hardware).unwrap();
        for invalid in ["instance", "joint", "id"] {
            let mut altered = hardware.clone();
            match invalid {
                "instance" => altered.id = "wrong".into(),
                "joint" => altered.joints[0] = "wrong".into(),
                _ => altered.motor_ids[0] = 10,
            }
            assert!(matches!(
                validate_hardware_section(&altered),
                Err(HardwareSectionError::InvalidMixedBus2Layout { .. })
            ));
        }
    }

    #[test]
    fn calibration_tolerances_are_optional_per_joint_configuration() {
        let mut hw = valid_hw();
        assert_eq!(hw.calibration_range_tolerances_raw, None);
        hw.calibration_range_tolerances_raw = Some(vec![0, 20, 7]);
        validate_hardware_section(&hw).unwrap();
        let encoded = toml::to_string(&hw).unwrap();
        let decoded: HardwareSection = toml::from_str(&encoded).unwrap();
        assert_eq!(
            decoded.calibration_range_tolerances_raw,
            Some(vec![0, 20, 7])
        );
        assert!(
            !decoded
                .extras
                .contains_key("calibration_range_tolerances_raw")
        );
    }

    #[test]
    fn calibration_tolerances_reject_invalid_length_or_raw_values() {
        let mut hw = valid_hw();
        for values in [vec![], vec![20], vec![0, 20, 4096]] {
            hw.calibration_range_tolerances_raw = Some(values);
            assert!(matches!(
                validate_hardware_section(&hw),
                Err(HardwareSectionError::InvalidCalibrationTolerances { .. })
            ));
        }
        let base = toml::to_string(&valid_hw()).unwrap();
        for invalid in ["[-1, 0, 0]", "[true, 0, 0]", "[1.5, 0, 0]", "[65536, 0, 0]"] {
            let text = format!("{base}\ncalibration_range_tolerances_raw = {invalid}\n");
            assert!(toml::from_str::<HardwareSection>(&text).is_err());
        }
    }

    #[test]
    fn missing_motor_ids_is_parse_error() {
        // 必填无 default：缺失 = TOML parse 错误（config 错误族 exit 2）。
        let toml = "[robot]\nid = \"r\"\nconfig_version = \"0.1\"\n\n\
                    [[hardware]]\nid = \"arm\"\nadapter = \"mock_hard_rt\"\n\
                    joints = [\"j1\"]\n";
        let f = write_temp(toml);
        let err = load_robot_config(f.path()).unwrap_err();
        assert!(matches!(err, ConfigLoadError::Toml { .. }), "{err:?}");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn validate_rejects_empty_joints() {
        let mut hw = valid_hw();
        hw.joints.clear();
        let err = validate_hardware_section(&hw).unwrap_err();
        assert!(matches!(err, HardwareSectionError::EmptyJoints { .. }));
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn validate_rejects_empty_motor_ids() {
        let mut hw = valid_hw();
        hw.motor_ids.clear();
        assert!(matches!(
            validate_hardware_section(&hw).unwrap_err(),
            HardwareSectionError::EmptyMotorIds { .. }
        ));
    }

    #[test]
    fn validate_rejects_len_mismatch() {
        let mut hw = valid_hw();
        hw.motor_ids.push(4);
        assert!(matches!(
            validate_hardware_section(&hw).unwrap_err(),
            HardwareSectionError::LenMismatch {
                joints: 3,
                motor_ids: 4,
                ..
            }
        ));
    }

    #[test]
    fn validate_rejects_dup_joint_name() {
        let mut hw = valid_hw();
        hw.joints[2] = hw.joints[0].clone();
        assert!(matches!(
            validate_hardware_section(&hw).unwrap_err(),
            HardwareSectionError::DupJointName { .. }
        ));
    }

    #[test]
    fn validate_rejects_dup_motor_id() {
        let mut hw = valid_hw();
        hw.motor_ids[2] = hw.motor_ids[0];
        assert!(matches!(
            validate_hardware_section(&hw).unwrap_err(),
            HardwareSectionError::DupMotorId { motor_id: 1, .. }
        ));
    }

    #[test]
    fn validate_accepts_sample() {
        let f = write_temp(SAMPLE_TOML);
        let cfg = load_robot_config(f.path()).unwrap();
        for hw in &cfg.parsed.hardware {
            validate_hardware_section(hw).unwrap();
        }
    }
}
