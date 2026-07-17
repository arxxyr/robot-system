//! per-case config 物化：结构化重写 `[robot].id`（v0.5 候选 E 段(2)
//! 回修 P0-1）。
//!
//! 语义：读静态样板 TOML → toml 结构化解析 → **只改 `[robot].id`** 为本
//! case 唯一 robot_id → 序列化落到 case 目录。其余字段（`[[hardware]]`
//! 的 `joints` / `motor_ids` / `auto_enable`、`[monitor]`、extras）**语义
//! 值保留**（`toml::to_string_pretty` 会重排格式、注释不保留——保的是值
//! 不是字节）——**保住 per-case 唯一 ID 隔离，不放宽身份锁、不退回固定
//! robot_id**。

use std::path::{Path, PathBuf};

/// 把 `template` 样板 config 重写 `[robot].id = robot_id` 后写入
/// `out_dir/<robot_id>_config.toml`，返回新路径。
///
/// # Errors
///
/// 读/写 I/O 失败、TOML 解析失败、样板缺 `[robot]` 段均返回
/// `std::io::Error`（harness 统一 String 化上抛）。
pub fn materialize_config_with_robot_id(
    template: &Path,
    robot_id: &str,
    out_dir: &Path,
) -> std::io::Result<PathBuf> {
    let text = std::fs::read_to_string(template)?;
    let mut doc: toml::Table = text
        .parse()
        .map_err(|e| std::io::Error::other(format!("parse {}: {e}", template.display())))?;
    let robot = doc
        .get_mut("robot")
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(|| std::io::Error::other(format!("{}: 缺 [robot] 段", template.display())))?;
    robot.insert("id".into(), toml::Value::String(robot_id.into()));
    let out = out_dir.join(format!("{robot_id}_config.toml"));
    let serialized = toml::to_string_pretty(&doc).map_err(std::io::Error::other)?;
    std::fs::write(&out, serialized)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::materialize_config_with_robot_id;

    const TEMPLATE: &str = r#"
[robot]
id             = "fixed_template_id"
config_version = "0.5.0-test"

[[hardware]]
id        = "smoke_arm"
adapter   = "mock_hard_rt"
joints    = ["j0", "j1"]
motor_ids = [1, 2]
auto_enable = false
baud      = 1000000
"#;

    /// tempdir 自己滚（本 crate 方针：不引 tempfile，沿 Cargo.toml 注释 +
    /// clear_estop_e2e 先例）。
    fn scratch_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "robot_tests_config_fixture_{}_{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn rewrites_robot_id_preserves_rest() {
        let dir = scratch_dir("rewrite");
        let tpl = dir.join("template.toml");
        std::fs::write(&tpl, TEMPLATE).unwrap();

        let out = materialize_config_with_robot_id(&tpl, "case_pid_7", &dir).unwrap();
        let doc: toml::Table = std::fs::read_to_string(&out).unwrap().parse().unwrap();

        assert_eq!(doc["robot"]["id"].as_str(), Some("case_pid_7"));
        assert_eq!(doc["robot"]["config_version"].as_str(), Some("0.5.0-test"));
        let hw = &doc["hardware"].as_array().unwrap()[0];
        assert_eq!(hw["joints"].as_array().unwrap().len(), 2);
        assert_eq!(
            hw["motor_ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_integer().unwrap())
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(hw["auto_enable"].as_bool(), Some(false));
        assert_eq!(hw["baud"].as_integer(), Some(1_000_000));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_robot_section_is_err() {
        let dir = scratch_dir("no_robot");
        let tpl = dir.join("no_robot.toml");
        std::fs::write(&tpl, "[s1]\nheartbeat_hz = 50.0\n").unwrap();
        let err = materialize_config_with_robot_id(&tpl, "x", &dir).unwrap_err();
        assert!(err.to_string().contains("[robot]"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
