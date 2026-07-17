//! `robot-s0::config_loader` —— `configs/<robot>.toml` 读取 + `ConfigIdentity` 构造。
//!
//! 为什么住在 `robot-s0` 而不是 `robot-core`：
//!
//! - `docs/00_design.md` §4.2 + `docs/CHANGELOG.md` 2026-04-22 "workspace runtime
//!   依赖白名单机制"：`robot-core` 禁 `serde_json` 等 runtime 依赖，恪守"零
//!   runtime 依赖"。serde / toml 同属"runtime 解析层"，按精神也不进 robot-core。
//! - `IdentityFingerprint` 类型（`robot_core::config`）已是 robot-core 的纯类型；
//!   解析 + 构造逻辑下沉到 *使用方* crate（这里是 robot-s0）。v0.3 起若 robot-s1
//!   / robot-recorder 也要读同一份 TOML，再抽出 `robot-config` 共享 crate
//!   （v0.2 单 binary 不值得动结构）。
//!
//! ### v0.2 范围
//!
//! - TOML schema 跟 `docs/00_design.md` §7.2 字面对齐：`[robot]` 单段 +
//!   `[[hardware]]` 多段 + `[[sensor]]` 多段。
//! - `[robot]` 段必须含 `id` + `config_version`（设计 §7.3 `ConfigIdentity`
//!   的 plain 字段源）。
//! - `[[hardware]]` / `[[sensor]]` 字段宽松接收：`id` + `adapter` +
//!   `motor_ids`（v0.5 候选 E 段(2) typed 必填）必填，其他（`address` /
//!   `baud` / `joints` / `calibration` / `wheel_radius` / ...）按需出现，
//!   未知字段不报错（v0.4 接 hardware-specific adapter 时再按 adapter
//!   type cast）。
//! - `IdentityFingerprint` 的 `config_hash = blake3(原始 TOML bytes)`。其余 3 hash
//!   留 placeholder（详 [`robot_core::config`] module-level docs）。
//!
//! ### 不做的事（推 v0.3+）
//!
//! - 跨进程 `ConfigIdentity` handshake（iceoryx2 request-response）。
//! - `calibration_hash` / `schema_registry_hash` / `adapter_registry_hash`
//!   计算（依赖 calibration / schema registry / adapter inventory 三个 v0.3+
//!   子系统）。
//! - hardware-specific 字段强类型化（v0.4 真 adapter 落地时按 adapter 名
//!   做 enum dispatch）。

use std::fs;
use std::path::{Path, PathBuf};

use robot_core::config::IdentityFingerprint;
use serde::{Deserialize, Serialize};

// ============================================================================
// TOML schema
// ============================================================================

/// 顶层配置：`[robot]` + `[[hardware]]` + `[[sensor]]`。
///
/// `derive(Serialize)` 仅给单测用（roundtrip 断言）；运行时 `s0_daemon` 不写
/// TOML 回去。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct RobotConfigToml {
    pub robot: RobotSection,
    #[serde(default)]
    pub hardware: Vec<HardwareSection>,
    #[serde(default)]
    pub sensor: Vec<SensorSection>,
    /// `[monitor]` 段（task-31 B'b 加）：系统健康采样配置。缺省时 `s0_daemon`
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

/// `[[hardware]]` 段：v0.2 通用字段；hardware-specific 字段（`baud` /
/// `wheel_radius` / ...）由 toml unknown-field 默认接受策略消化。
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
    /// **v0.5 候选 C**：启动是否自动 enable（design §7.2 `auto_enable`，B2 typed 化，
    /// 不再走 extras）。`None`（缺省）= **true**（保留 `s0_daemon` 既有无条件启动
    /// auto-enable 行为）；`Some(false)` → 启动留 Safe 等 operator `SetEnabled(true)`。
    #[serde(default)]
    pub auto_enable: Option<bool>,
    /// adapter-specific 整数字段（`baud` / `poll_hz` / ...）。v0.4 按 adapter 名
    /// 拆 enum；v0.2 通用 map 兜底。
    #[serde(flatten, default)]
    pub extras: toml::Table,
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
/// interested_processes = ["s0_daemon", "mock_s1", "robotctl"]
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

/// `[[sensor]]` 段：与 `HardwareSection` 同结构；保留独立类型给 v0.4
/// sensor-specific 字段（streams / fps / ...）扩展。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct SensorSection {
    pub id: String,
    pub adapter: String,
    #[serde(default)]
    pub serial: Option<String>,
    #[serde(default)]
    pub streams: Vec<String>,
    #[serde(flatten, default)]
    pub extras: toml::Table,
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

/// 加载错误。`thiserror` derive 让 `s0_daemon` 可以 `tracing::error!(?err)`
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
}

// ============================================================================
// 入口
// ============================================================================

/// 读 + 解析 + 算 `ConfigIdentity`。失败的三类错误见 [`ConfigLoadError`]。
///
/// `config_hash` 用 *原始 bytes* 算（不是解析后再 canonical 化）—— 即"对 TOML
/// 文件的 byte-exact 同一性"；任何空白 / 注释 / 字段顺序改动都改变 hash。
/// 设计 §7.3 没强制 hash 的 canonical 化方式，"原始 bytes" 是最简单 + 可重现的
/// 选择。v0.3 跨进程 handshake 落地时若发现"开发机和部署机的 line-ending /
/// trailing newline 差导致 hash 不同"成痛点，再升 canonical 化。
///
/// # Errors
///
/// 见 [`ConfigLoadError`]：文件读失败 / 非 UTF-8 / TOML 语法错。
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

[[sensor]]
id = "head_cam"
adapter = "realsense_d435i"
serial = "0123"
streams = ["color", "depth"]
"#;

    fn write_temp(toml: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        f
    }

    #[test]
    fn parse_full_sample_toml() {
        let f = write_temp(SAMPLE_TOML);
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
            cfg.parsed.sensor[0].streams,
            vec!["color".to_string(), "depth".into()]
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
        assert!(cfg.parsed.hardware.is_empty());
        assert!(cfg.parsed.sensor.is_empty());
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
        // SAMPLE_TOML 不写 auto_enable → None（s0_daemon 缺省语义 = true）。
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
    fn motor_ids_typed_parsed_not_in_extras() {
        let f = write_temp(SAMPLE_TOML);
        let cfg = load_robot_config(f.path()).unwrap();
        assert_eq!(cfg.parsed.hardware[0].motor_ids, vec![1, 2, 3]);
        assert_eq!(cfg.parsed.hardware[1].motor_ids, vec![7, 8, 9]);
        assert!(!cfg.parsed.hardware[0].extras.contains_key("motor_ids"));
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
