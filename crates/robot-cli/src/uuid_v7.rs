//! `request_id` 生成 —— `UUIDv7`（Q-CLI14(b)：直接用 `uuid` crate，**不**
//! 复制 smoke binary 的 `uuidv7_compat()` inline 实现）。

use uuid::Uuid;

/// 新 `request_id` = `UUIDv7` hyphenated lowercase。每次调用新生成。
///
/// `hyphenated` 默认输出 lowercase，无需再 `.to_lowercase()`。
#[must_use]
pub fn new_request_id() -> String {
    Uuid::now_v7().hyphenated().to_string()
}

#[cfg(test)]
mod tests {
    use super::new_request_id;
    use uuid::Uuid;

    #[test]
    fn request_id_is_hyphenated_v7_lowercase() {
        let id = new_request_id();
        // 36 字符 8-4-4-4-12 hyphenated。
        assert_eq!(id.len(), 36, "{id}");
        let lens: Vec<usize> = id.split('-').map(str::len).collect();
        assert_eq!(lens, [8, 4, 4, 4, 12]);
        // version nibble == 7。
        let parsed = Uuid::parse_str(&id).expect("parse uuid");
        assert_eq!(parsed.get_version_num(), 7);
        // lowercase。
        assert_eq!(id, id.to_lowercase());
    }

    #[test]
    fn request_ids_are_unique() {
        assert_ne!(new_request_id(), new_request_id());
    }
}
