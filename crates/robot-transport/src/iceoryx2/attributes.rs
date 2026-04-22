//! iceoryx2 service attribute 辅助（决策 ⑤ C · `schema_id` / `schema_version`
//! 作为硬约束 attribute 绑定到 service）。
//!
//! 双道防线：
//! 1. `FrameHeader.schema_id` 字段（运行时 parser 校验，Step 3 已落）
//! 2. iceoryx2 service attribute（创建 + 打开 期硬约束，本模块）
//!
//! 烟测（`/tmp/iceoryx2_smoke`）验证不匹配 attribute 的 `open_with_attributes`
//! 会被拒绝，所以第 2 道防线是"连 open 都过不去"，不是"open 后再校验"。

use iceoryx2::prelude::{AttributeSpecifier, AttributeVerifier};

use crate::error::TransportError;

/// Attribute key 常量：绑定到 service 的 schema 元数据。
pub const ATTR_SCHEMA_ID: &str = "schema_id";
/// Attribute key 常量：`FrameHeader.schema_version` 对应值。
pub const ATTR_SCHEMA_VERSION: &str = "schema_version";

/// 构造创建 service 时需要的 attribute spec。
///
/// `schema_id` 以 `0x01234567` 的十六进制字符串形式写入（方便 `robotctl` /
/// service discovery 读出来对照 `SCHEMA_REGISTRY`）。`schema_version` 用十进制。
///
/// # Errors
///
/// - attribute key / value 超过 iceoryx2 `MAX_ATTRIBUTE_KEY_LENGTH` /
///   `MAX_ATTRIBUTE_VALUE_LENGTH`（默认 64 / 128 字节）。
pub fn schema_attribute_spec(
    schema_id: u32,
    schema_version: u16,
) -> Result<AttributeSpecifier, TransportError> {
    let key_id = ATTR_SCHEMA_ID.try_into().map_err(|e| {
        TransportError::Backend(format!("attribute key {ATTR_SCHEMA_ID:?} invalid: {e:?}"))
    })?;
    let val_id = format!("{schema_id:#010x}")
        .as_str()
        .try_into()
        .map_err(|e| {
            TransportError::Backend(format!("attribute value for schema_id invalid: {e:?}"))
        })?;
    let key_ver = ATTR_SCHEMA_VERSION.try_into().map_err(|e| {
        TransportError::Backend(format!(
            "attribute key {ATTR_SCHEMA_VERSION:?} invalid: {e:?}"
        ))
    })?;
    let val_ver = format!("{schema_version}")
        .as_str()
        .try_into()
        .map_err(|e| {
            TransportError::Backend(format!("attribute value for schema_version invalid: {e:?}"))
        })?;

    AttributeSpecifier::new()
        .define(&key_id, &val_id)
        .map_err(|e| TransportError::Backend(format!("define(schema_id): {e:?}")))?
        .define(&key_ver, &val_ver)
        .map_err(|e| TransportError::Backend(format!("define(schema_version): {e:?}")))
}

/// 构造打开 service 时的 attribute 验证器（subscriber 侧校验 publisher 定义的
/// `schema_id` / `schema_version`）。
///
/// iceoryx2 的 `open_with_attributes(verifier)` 在不匹配时直接返错（不匹配
/// 的定义被 iceoryx2 判为 `IncompatibleAttribute`），我们不做运行时再次比较。
///
/// # Errors
///
/// - 同 [`schema_attribute_spec`]
pub fn schema_attribute_verifier(
    schema_id: u32,
    schema_version: u16,
) -> Result<AttributeVerifier, TransportError> {
    let key_id = ATTR_SCHEMA_ID.try_into().map_err(|e| {
        TransportError::Backend(format!("attribute key {ATTR_SCHEMA_ID:?} invalid: {e:?}"))
    })?;
    let val_id = format!("{schema_id:#010x}")
        .as_str()
        .try_into()
        .map_err(|e| {
            TransportError::Backend(format!("attribute value for schema_id invalid: {e:?}"))
        })?;
    let key_ver = ATTR_SCHEMA_VERSION.try_into().map_err(|e| {
        TransportError::Backend(format!(
            "attribute key {ATTR_SCHEMA_VERSION:?} invalid: {e:?}"
        ))
    })?;
    let val_ver = format!("{schema_version}")
        .as_str()
        .try_into()
        .map_err(|e| {
            TransportError::Backend(format!("attribute value for schema_version invalid: {e:?}"))
        })?;

    AttributeVerifier::new()
        .require(&key_id, &val_id)
        .map_err(|e| TransportError::Backend(format!("require(schema_id): {e:?}")))?
        .require(&key_ver, &val_ver)
        .map_err(|e| TransportError::Backend(format!("require(schema_version): {e:?}")))
}
