//! 客户端早拒校验（AC-11）。

use robot_rpc::auth::{RequestIdError, validate_request_id};

/// 校验 `<action-id>` 是合法 `UUIDv7`（复用
/// `robot_rpc::auth::validate_request_id`）。
///
/// 失败返回人类可读消息（CLI 打 stderr + exit 2；AC-11）。exhaustive match
/// 锁住 [`RequestIdError`] 三 variant。
pub fn validate_action_id(s: &str) -> Result<(), String> {
    match validate_request_id(s) {
        Ok(_) => Ok(()),
        Err(RequestIdError::Empty) => Err("<action-id> must not be empty".to_owned()),
        Err(RequestIdError::Malformed(v)) => Err(format!("<action-id> is not a valid UUID: {v}")),
        Err(RequestIdError::WrongVersion { got }) => {
            Err(format!("<action-id> must be UUIDv7, got version={got}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::validate_action_id;

    #[test]
    fn accepts_uuid_v7() {
        let id = crate::uuid_v7::new_request_id();
        assert!(validate_action_id(&id).is_ok(), "{id}");
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(
            validate_action_id("").unwrap_err(),
            "<action-id> must not be empty"
        );
    }

    #[test]
    fn rejects_malformed() {
        let err = validate_action_id("not-a-uuid").unwrap_err();
        assert!(err.starts_with("<action-id> is not a valid UUID:"), "{err}");
    }

    #[test]
    fn rejects_v4() {
        // 版本 nibble = 4（第三组首字符）。
        let err = validate_action_id("550e8400-e29b-41d4-a716-446655440000").unwrap_err();
        assert_eq!(err, "<action-id> must be UUIDv7, got version=4");
    }
}
