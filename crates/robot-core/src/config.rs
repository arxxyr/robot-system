//! `robot-core::config` —— `ConfigIdentity` 强类型骨架。
//!
//! `docs/00_design.md` §7.3：所有进程启动期通过 iceoryx2 request-response 交换
//! `ConfigIdentity`，校验 `config_hash` / `calibration_hash` /
//! `schema_registry_hash` / `adapter_registry_hash` 一致性，避免"S0 升级后
//! joint 顺序变了，S1 还用旧配置 → `layout_id` 对不上但没人报错"这类事故。
//!
//! ### v0.2 范围（task 25）
//!
//! - 本文件**只**定义 [`IdentityFingerprint`] 类型 + 比较语义；**不带 serde
//!   derive**，恪守 robot-core §4.2 "零 runtime 依赖"（serde / toml 落在 robot-s0
//!   的 `config_loader` 子模块，使用方自负 deserialize）。
//! - v0.2 阶段只填得起 `robot_id` / `config_version` / `config_hash` 三项；
//!   余 `calibration_hash` / `schema_registry_hash` / `adapter_registry_hash`
//!   留 `[0u8; 32]` placeholder，等 v0.3 各 registry 落地再算。
//! - 跨进程 handshake / `DEGRADED` 状态机分支 / recorder `ConfigMismatch` 事件
//!   均**不在本任务范围**，留 v0.3+。
//!
//! ### 命名取舍：`IdentityFingerprint` 而非 `ConfigIdentity`
//!
//! 设计文档 §7.3 用 `ConfigIdentity` 命名，但本 crate 的类型职责更窄
//! —— 它只是"配置身份的固定指纹"（5 个 hash + 2 个 plain 字段），
//! 不承担 §7.3 描述的*校验流程*（流程在调用方）。`IdentityFingerprint`
//! 反映这一窄范围；调用方的 wrapper 类型再按 §7.3 起名 `ConfigIdentity`
//! 也可以。如果 v0.3 跨进程 handshake 落地时发现命名割裂导致沟通成本，
//! 走 CHANGELOG 改回 `ConfigIdentity`。
//!
//! ### v0.2 仅算 `config_hash` 的理由
//!
//! - `config_hash`：blake3 over TOML 文件原始 bytes。**v0.2 立得起来** —— 无须
//!   解析后 canonical 化，原始 bytes 即权威源；任意字段调整都改变 hash。
//! - `calibration_hash`：v0.2 还没 calibration 子系统，没东西可 hash。
//! - `schema_registry_hash`：robot-msg 已有 `schema_registry`，但暴露 `[u8; 32]`
//!   helper 是 v0.3 任务（v0.2 `schema_lint` bin 算的是 `layout_id` u64，
//!   不是 32-byte hash）。
//! - `adapter_registry_hash`：adapter inventory 子系统 v0.3 才落地。
//!
//! 三个 placeholder 都设 `[0u8; 32]`，跨进程 handshake 实现时（v0.3）若
//! 任一字段仍 placeholder，按"未参与校验"处理（不写约束语义到 v0.2 类型，
//! 避免锁死 v0.3 设计空间）。

#![allow(clippy::module_name_repetitions)]

use core::fmt;

/// 配置身份指纹（v0.2 骨架，详 module-level docs）。
///
/// **不可变**类型：构造完成后所有字段视为 frozen；启动期 handshake 比对一律
/// 走 [`PartialEq`] / [`Eq`]。
///
/// 字段顺序与 `docs/00_design.md` §7.3 文本一致。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IdentityFingerprint {
    /// `configs/<robot_id>.toml` 里 `[robot] id = "..."`。同一 robot 的所有
    /// 进程必须一致。
    pub robot_id: String,

    /// semver，对应 `[robot] config_version = "..."`。`config_hash` 不一致时
    /// `config_version` 给人读用，机器只看 hash。
    pub config_version: String,

    /// `blake3(robot config TOML 原始 bytes).truncate(32)`。任意 TOML 字段改动
    /// 都改变此值。
    pub config_hash: [u8; 32],

    /// v0.2 placeholder = `[0u8; 32]`。v0.3 接 calibration 子系统时填实。
    pub calibration_hash: [u8; 32],

    /// v0.2 placeholder = `[0u8; 32]`。v0.3 接 robot-msg `schema_registry` hash
    /// 后填实。
    pub schema_registry_hash: [u8; 32],

    /// v0.2 placeholder = `[0u8; 32]`。v0.3 接 adapter inventory 后填实。
    pub adapter_registry_hash: [u8; 32],
}

impl IdentityFingerprint {
    /// 构造 v0.2 *partial* fingerprint：只填 `robot_id` / `config_version` /
    /// `config_hash`，余 3 hash 留 placeholder。
    ///
    /// v0.3 跨进程 handshake 落地时引入完整构造器；本函数届时仍可保留作为
    /// 测试 helper（明示"3 hash 留空"）。
    #[must_use]
    pub fn v02_partial(
        robot_id: impl Into<String>,
        config_version: impl Into<String>,
        config_hash: [u8; 32],
    ) -> Self {
        Self {
            robot_id: robot_id.into(),
            config_version: config_version.into(),
            config_hash,
            calibration_hash: [0u8; 32],
            schema_registry_hash: [0u8; 32],
            adapter_registry_hash: [0u8; 32],
        }
    }

    /// 4 hash 字段全部为 placeholder（`[0; 32]`）时返回 `true`。
    /// 用途：调用方判断 fingerprint 是否完全 v0.2 partial（vs v0.3 full）。
    #[must_use]
    pub fn is_fully_partial(&self) -> bool {
        const ZERO: [u8; 32] = [0u8; 32];
        self.calibration_hash == ZERO
            && self.schema_registry_hash == ZERO
            && self.adapter_registry_hash == ZERO
    }
}

impl fmt::Display for IdentityFingerprint {
    /// 紧凑单行格式：`robot=<id> ver=<v> cfg=<hex8> cal=<hex8>* schema=<hex8>* adapter=<hex8>*`
    ///
    /// 后缀 `*` 标记 placeholder，方便人眼一眼分辨"v0.2 partial vs v0.3 full"。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "robot={} ver={} cfg={} cal={}{} schema={}{} adapter={}{}",
            self.robot_id,
            self.config_version,
            hex8(&self.config_hash),
            hex8(&self.calibration_hash),
            placeholder_suffix(&self.calibration_hash),
            hex8(&self.schema_registry_hash),
            placeholder_suffix(&self.schema_registry_hash),
            hex8(&self.adapter_registry_hash),
            placeholder_suffix(&self.adapter_registry_hash),
        )
    }
}

/// 取前 8 字节（16 hex 字符）作为人类可读 hash 摘要。
fn hex8(h: &[u8; 32]) -> String {
    let mut s = String::with_capacity(16);
    for b in &h[..8] {
        let _ = fmt::Write::write_fmt(&mut s, format_args!("{b:02x}"));
    }
    s
}

fn placeholder_suffix(h: &[u8; 32]) -> &'static str {
    if *h == [0u8; 32] { "*" } else { "" }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_hash(seed: u8) -> [u8; 32] {
        let mut h = [0u8; 32];
        for (i, slot) in h.iter_mut().enumerate() {
            *slot = seed.wrapping_add(u8::try_from(i & 0xff).unwrap_or(0));
        }
        h
    }

    #[test]
    fn v02_partial_fills_three_fields_leaves_others_zero() {
        let fp = IdentityFingerprint::v02_partial("xlerobot", "0.1", sample_hash(1));
        assert_eq!(fp.robot_id, "xlerobot");
        assert_eq!(fp.config_version, "0.1");
        assert_eq!(fp.config_hash, sample_hash(1));
        assert_eq!(fp.calibration_hash, [0u8; 32]);
        assert_eq!(fp.schema_registry_hash, [0u8; 32]);
        assert_eq!(fp.adapter_registry_hash, [0u8; 32]);
        assert!(fp.is_fully_partial());
    }

    #[test]
    fn equality_requires_all_seven_fields_match() {
        let base = IdentityFingerprint::v02_partial("a", "0.1", sample_hash(1));

        // 自反
        assert_eq!(base, base.clone());

        // robot_id 改 → 不等
        let mut b = base.clone();
        b.robot_id = "different".into();
        assert_ne!(base, b);

        // config_version 改 → 不等
        let mut b = base.clone();
        b.config_version = "0.2".into();
        assert_ne!(base, b);

        // config_hash 改 → 不等
        let mut b = base.clone();
        b.config_hash = sample_hash(2);
        assert_ne!(base, b);

        // calibration_hash 从 placeholder 改 → 不等
        let mut b = base.clone();
        b.calibration_hash = sample_hash(3);
        assert_ne!(base, b);
        assert!(!b.is_fully_partial());

        // schema_registry_hash 从 placeholder 改 → 不等
        let mut b = base.clone();
        b.schema_registry_hash = sample_hash(4);
        assert_ne!(base, b);
        assert!(!b.is_fully_partial());

        // adapter_registry_hash 从 placeholder 改 → 不等
        let mut b = base.clone();
        b.adapter_registry_hash = sample_hash(5);
        assert_ne!(base, b);
        assert!(!b.is_fully_partial());
    }

    #[test]
    fn display_marks_placeholders_with_star_suffix() {
        let fp = IdentityFingerprint::v02_partial("xlerobot", "0.1", sample_hash(1));
        let s = format!("{fp}");
        // 必须有 robot / ver / cfg / cal / schema / adapter 6 字段
        assert!(s.contains("robot=xlerobot"), "{s}");
        assert!(s.contains("ver=0.1"), "{s}");
        // 三个 placeholder 字段带 `*` 后缀；config_hash 不带（非 placeholder）
        assert!(s.contains("cal=0000000000000000*"), "{s}");
        assert!(s.contains("schema=0000000000000000*"), "{s}");
        assert!(s.contains("adapter=0000000000000000*"), "{s}");
        // config_hash 用 sample_hash(1) 起头：0x01 0x02 0x03 ... → "0102030405060708"
        assert!(s.contains("cfg=0102030405060708"), "{s}");
    }

    #[test]
    fn full_fingerprint_has_no_placeholder_suffix() {
        let fp = IdentityFingerprint {
            robot_id: "xlerobot".into(),
            config_version: "0.1".into(),
            config_hash: sample_hash(1),
            calibration_hash: sample_hash(2),
            schema_registry_hash: sample_hash(3),
            adapter_registry_hash: sample_hash(4),
        };
        assert!(!fp.is_fully_partial());
        let s = format!("{fp}");
        // 全部字段非零 → 任意 `*` 出现都说明 Display 误判
        assert!(!s.contains('*'), "should have no placeholder marker: {s}");
    }
}
