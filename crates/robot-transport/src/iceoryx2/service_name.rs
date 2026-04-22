//! Topic 字符串 → `iceoryx2::prelude::ServiceName`（决策 ② 方案 A）。
//!
//! `docs/CHANGELOG.md` 2026-04-22 · Step 5 决策 ② 选定"原样喂"方案：topic
//! 字符串（如 `/<robot>/s0/<hw>/state`，§2.3）直接传给 `ServiceName::new`，
//! 不做点分 / 去斜杠等转换。macOS 烟测已验证 iceoryx2 0.8.1 接受 `/` 前缀与
//! `/` 分隔符；`ServiceName::new` 自身会拒绝超长 / 非法字符。
//!
//! 本模块的工作只是把 iceoryx2 的错误类型包成 [`TransportError::Backend`]，
//! 保持 crate 对外只暴露 `TransportError`。

use iceoryx2::prelude::ServiceName;

use crate::error::TransportError;

/// 把 §2.3 的 topic 字符串转成 iceoryx2 `ServiceName`。
///
/// 失败路径（字符集 / 长度超限）封装为 `TransportError::Backend`，保留原始
/// iceoryx2 错误诊断信息（`Display` 实现）。
///
/// # Errors
///
/// - 非法字符 / 空串 / 过长 → `TransportError::Backend(<诊断>)`
pub fn topic_to_service_name(topic: &str) -> Result<ServiceName, TransportError> {
    ServiceName::new(topic)
        .map_err(|e| TransportError::Backend(format!("ServiceName::new({topic:?}) failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_slash_prefixed_topic() {
        // §2.3 的标准命名空间；macOS 烟测已验证 iceoryx2 接受。
        let n = topic_to_service_name("/xlerobot_01/s0/left_arm/state").unwrap();
        assert_eq!(&*n, "/xlerobot_01/s0/left_arm/state");
    }

    #[test]
    fn accepts_plain_topic() {
        let n = topic_to_service_name("smoke/test").unwrap();
        assert_eq!(&*n, "smoke/test");
    }

    #[test]
    fn accepts_dotted_topic() {
        // 文档方案 B 字符集虽然弃用，但仍应能解析（方便与其他框架 topic 互通 / 临时测试）
        let n = topic_to_service_name("xlerobot_01.s0.left_arm.state").unwrap();
        assert_eq!(&*n, "xlerobot_01.s0.left_arm.state");
    }

    #[test]
    fn rejects_empty_string() {
        let err = topic_to_service_name("").unwrap_err();
        match err {
            TransportError::Backend(msg) => assert!(
                msg.contains("ServiceName::new"),
                "unexpected backend message: {msg}"
            ),
            other => panic!("expected Backend, got {other:?}"),
        }
    }
}
