//! `schema_lint` — 静态校验 robot-msg 的 schema 假设。
//!
//! 不依赖 cargo test；由 CI 单独跑：
//!
//! ```bash
//! cargo run -p robot-msg --bin schema_lint
//! ```
//!
//! ## 覆盖
//!
//! 1. `SCHEMA_REGISTRY`：id 严格升序、未碰撞、未与 `DEPRECATED_SCHEMA_IDS` 冲突，
//!    且 `lookup_name` 对每个 id 都能反查。
//! 2. `.fbs` 字段 id 单调：如果某 table 使用了任何显式 `(id: N)`，
//!    `FlatBuffers` 要求该 table 所有字段都必须带 id；本检查在 v0.2 schema
//!    集合上验证 "要么全没、要么全有并单调"。
//! 3. `.fbs` 字段 `(deprecated)` 标记：记录，为未来复用拦截打基础。
//! 4. `layout_id` 固定测试向量碰撞检测：几组典型 `robot_id × hardware_id`
//!    组合跑 blake3 截断到 u64，两两互异（`docs/00_design.md` §3.5.1）。
//!
//! ## v0.2 实现策略
//!
//! Regex 扫 `.fbs` 文本。schema 数量少（≤10 个 table），regex 足够；v0.3 若
//! 规模起来再换 `bfbs` 反射（`docs/CHANGELOG.md` 2026-04-22 Step 2 工具链决策）。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{env, fs};

use robot_msg::schema_id::{
    DEPRECATED_SCHEMA_IDS, SCHEMA_REGISTRY, SCHEMA_UNSPECIFIED, lookup_name,
};

fn main() -> ExitCode {
    let repo_root = match find_repo_root() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("schema_lint: unable to locate repo root: {e}");
            return ExitCode::FAILURE;
        }
    };
    let schemas_dir = repo_root.join("crates").join("robot-msg").join("schemas");

    let mut errors: Vec<String> = Vec::new();

    errors.extend(check_schema_registry());
    errors.extend(check_layout_id_fixture());

    match collect_fbs(&schemas_dir) {
        Ok(paths) => {
            for p in &paths {
                errors.extend(check_fbs_file(p));
            }
            eprintln!(
                "schema_lint: scanned {} .fbs file(s) under {}",
                paths.len(),
                schemas_dir.display()
            );
        }
        Err(e) => errors.push(format!(
            "failed to enumerate schemas under {}: {e}",
            schemas_dir.display()
        )),
    }

    if errors.is_empty() {
        eprintln!("schema_lint: all checks passed ✓");
        ExitCode::SUCCESS
    } else {
        eprintln!("schema_lint: {} error(s):", errors.len());
        for e in &errors {
            eprintln!("  - {e}");
        }
        ExitCode::FAILURE
    }
}

// ---------------------------------------------------------------------------
// Check #1 · SCHEMA_REGISTRY
// ---------------------------------------------------------------------------

fn check_schema_registry() -> Vec<String> {
    let mut errs = Vec::new();
    let mut prev: Option<u32> = None;

    for (id, name) in SCHEMA_REGISTRY {
        if *id == SCHEMA_UNSPECIFIED {
            errs.push(format!(
                "registry entry uses reserved SCHEMA_UNSPECIFIED (0x0): {name}"
            ));
        }
        if let Some(p) = prev
            && *id <= p
        {
            errs.push(format!(
                "SCHEMA_REGISTRY not strictly ascending at id={id:#010x} ({name}); previous={p:#010x}"
            ));
        }
        if let Some(found) = lookup_name(*id) {
            if found != *name {
                errs.push(format!(
                    "lookup_name({id:#010x}) mismatch: registry={name:?}, lookup={found:?}"
                ));
            }
        } else {
            errs.push(format!(
                "lookup_name({id:#010x}) returned None for registry entry {name:?}"
            ));
        }
        if DEPRECATED_SCHEMA_IDS.iter().any(|(d, _)| d == id) {
            errs.push(format!(
                "SCHEMA_REGISTRY reuses deprecated id {id:#010x} ({name})"
            ));
        }
        prev = Some(*id);
    }
    errs
}

// ---------------------------------------------------------------------------
// Check #2 · layout_id 固定测试向量不碰撞
// ---------------------------------------------------------------------------

fn check_layout_id_fixture() -> Vec<String> {
    // 代表性组合：同机器人不同总线、不同机器人同硬件 kind、不同 joint 数量。
    let fixtures: &[(&str, &str, &[&str])] = &[
        (
            "xlerobot_01",
            "bus1_arms",
            &[
                "left_shoulder_pan",
                "left_shoulder_lift",
                "left_elbow_flex",
                "left_wrist_flex",
                "left_wrist_roll",
                "right_shoulder_pan",
                "right_shoulder_lift",
                "right_elbow_flex",
                "right_wrist_flex",
                "right_wrist_roll",
            ],
        ),
        (
            "xlerobot_01",
            "bus2_base",
            &["wheel_1", "wheel_2", "wheel_3"],
        ),
        (
            "xlerobot_02",
            "bus1_arms",
            &[
                "left_shoulder_pan",
                "left_shoulder_lift",
                "left_elbow_flex",
                "left_wrist_flex",
                "left_wrist_roll",
                "right_shoulder_pan",
                "right_shoulder_lift",
                "right_elbow_flex",
                "right_wrist_flex",
                "right_wrist_roll",
            ],
        ),
        (
            "piper_01",
            "arm",
            &["joint1", "joint2", "joint3", "joint4", "joint5", "joint6"],
        ),
    ];

    let mut ids: Vec<(u64, String)> = Vec::new();
    for (robot, hw, joints) in fixtures {
        let id = compute_layout_id(robot, hw, joints);
        ids.push((id, format!("{robot}/{hw} ({} joints)", joints.len())));
    }

    let mut errs = Vec::new();
    for i in 0..ids.len() {
        for j in (i + 1)..ids.len() {
            if ids[i].0 == ids[j].0 {
                errs.push(format!(
                    "layout_id collision ({:#018x}): {} VS {}",
                    ids[i].0, ids[i].1, ids[j].1
                ));
            }
        }
    }
    errs
}

/// `layout_id = blake3(robot_id || 0x00 || hardware_id || 0x00 || sorted_names).truncate(8)` → u64 LE。
///
/// 与 `docs/00_design.md` §3.5.1 对齐。名字列表**排序后**拼接以保证顺序无关。
fn compute_layout_id(robot_id: &str, hardware_id: &str, joint_names: &[&str]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(robot_id.as_bytes());
    hasher.update(&[0x00]);
    hasher.update(hardware_id.as_bytes());
    hasher.update(&[0x00]);
    let mut sorted: Vec<&&str> = joint_names.iter().collect();
    sorted.sort();
    for n in sorted {
        hasher.update(n.as_bytes());
        hasher.update(&[0x00]);
    }
    let h = hasher.finalize();
    let bytes = h.as_bytes();
    u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

// ---------------------------------------------------------------------------
// Check #3 · .fbs 字段 id 单调 + deprecated 记录
// ---------------------------------------------------------------------------

fn collect_fbs(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk_fbs(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk_fbs(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_fbs(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("fbs") {
            out.push(path);
        }
    }
    Ok(())
}

fn check_fbs_file(path: &Path) -> Vec<String> {
    let src = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => return vec![format!("{}: read failed: {e}", path.display())],
    };
    let stripped = strip_comments(&src);

    let mut errs = Vec::new();
    for (table_name, body) in extract_tables(&stripped) {
        errs.extend(check_table_body(path, &table_name, &body));
    }
    errs
}

fn check_table_body(path: &Path, table_name: &str, body: &str) -> Vec<String> {
    let mut errs = Vec::new();
    let mut explicit_ids: Vec<i32> = Vec::new();
    let mut implicit_count = 0usize;
    let mut deprecated_fields: Vec<String> = Vec::new();

    for raw_line in body.split(';') {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        // 字段形如：`name: Type [= default] [(attr, attr, ...)]`
        // 我们只关心 attr 中的 id 和 deprecated；name 取冒号前首个标识符。
        let Some(colon) = line.find(':') else {
            // 可能是 table / enum / struct / namespace / include 等顶层语法。
            continue;
        };
        let name = line[..colon].trim().trim_start_matches('/').trim();
        if name.is_empty() || !is_ident(name) {
            continue;
        }

        let attrs = extract_attrs(line);
        let has_id = attrs.iter().any(|a| a.starts_with("id"));
        let has_deprecated = attrs.iter().any(|a| a == "deprecated");

        if has_deprecated {
            deprecated_fields.push(name.to_owned());
        }

        if has_id {
            if let Some(id) = parse_id_attr(&attrs) {
                explicit_ids.push(id);
            } else {
                errs.push(format!(
                    "{}: table {}: field '{}' has malformed (id: ...) attribute",
                    path.display(),
                    table_name,
                    name
                ));
            }
        } else {
            implicit_count += 1;
        }
    }

    // FlatBuffers 规则：一个 table 要么全部字段隐式 id，要么全部显式。
    if !explicit_ids.is_empty() && implicit_count > 0 {
        errs.push(format!(
            "{}: table {}: mixing explicit and implicit field ids ({} explicit / {} implicit)",
            path.display(),
            table_name,
            explicit_ids.len(),
            implicit_count
        ));
    }

    // 显式 id 严格递增
    let mut sorted = explicit_ids.clone();
    sorted.sort_unstable();
    if sorted != explicit_ids {
        errs.push(format!(
            "{}: table {}: explicit field ids not strictly ascending: {:?}",
            path.display(),
            table_name,
            explicit_ids
        ));
    }
    // 无重复
    let mut dedup = explicit_ids.clone();
    dedup.sort_unstable();
    dedup.dedup();
    if dedup.len() != explicit_ids.len() {
        errs.push(format!(
            "{}: table {}: duplicate explicit field ids: {:?}",
            path.display(),
            table_name,
            explicit_ids
        ));
    }

    // deprecated 目前只记录；未来 CI 可让仓库保存一份 "known deprecated" 列表，
    // 禁止将同一 id 重新赋给新字段。v0.2 schema 集合为空，不落数据。
    let _ = deprecated_fields;

    errs
}

// ---------------------------------------------------------------------------
// .fbs 文本处理（轻量、足以覆盖 v0.2 schema 集合）
// ---------------------------------------------------------------------------

/// 去除 `//` 单行注释与 `/* */` 块注释。
fn strip_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let next = bytes.get(i + 1).copied().unwrap_or(0);
        if c == b'/' && next == b'/' {
            // 跳到行尾
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && next == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
        } else {
            out.push(c as char);
            i += 1;
        }
    }
    out
}

/// 返回 `.fbs` 里的 `(table_name, body_text)` 列表；`body_text` 为 `{` 到 `}` 之间的内容。
fn extract_tables(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = src.as_bytes();
    let needle = b"table ";
    let mut i = 0;
    while i + needle.len() < bytes.len() {
        if &bytes[i..i + needle.len()] == needle && (i == 0 || !is_ident_byte(bytes[i - 1])) {
            let after = i + needle.len();
            // 解析 table 名
            let name_start = after + skip_ws(&bytes[after..]);
            let mut j = name_start;
            while j < bytes.len() && is_ident_byte(bytes[j]) {
                j += 1;
            }
            let name = std::str::from_utf8(&bytes[name_start..j])
                .unwrap_or("")
                .to_string();
            // 找 `{`
            let mut k = j;
            while k < bytes.len() && bytes[k] != b'{' {
                k += 1;
            }
            if k >= bytes.len() {
                break;
            }
            // 从 { 匹配配对的 }
            let body_start = k + 1;
            let mut depth = 1;
            let mut p = body_start;
            while p < bytes.len() && depth > 0 {
                match bytes[p] {
                    b'{' => depth += 1,
                    b'}' => depth -= 1,
                    _ => {}
                }
                p += 1;
            }
            if depth == 0 && p > body_start {
                let body = std::str::from_utf8(&bytes[body_start..p - 1])
                    .unwrap_or("")
                    .to_string();
                out.push((name, body));
                i = p;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// 提取字段行里 `(...)` 内的 attribute，去重空白后 split on `,`。
fn extract_attrs(line: &str) -> Vec<String> {
    let Some(open) = line.rfind('(') else {
        return Vec::new();
    };
    let Some(close) = line.rfind(')') else {
        return Vec::new();
    };
    if close <= open {
        return Vec::new();
    }
    let inside = &line[open + 1..close];
    inside
        .split(',')
        .map(|s| s.split_whitespace().collect::<String>())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_id_attr(attrs: &[String]) -> Option<i32> {
    for a in attrs {
        if let Some(rest) = a.strip_prefix("id:") {
            return rest.parse::<i32>().ok();
        }
    }
    None
}

fn is_ident(s: &str) -> bool {
    !s.is_empty() && s.as_bytes()[0].is_ascii_alphabetic() && s.bytes().all(is_ident_byte)
}

fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

fn skip_ws(bytes: &[u8]) -> usize {
    bytes.iter().take_while(|b| b.is_ascii_whitespace()).count()
}

// ---------------------------------------------------------------------------
// 仓库根探测
// ---------------------------------------------------------------------------

fn find_repo_root() -> std::io::Result<PathBuf> {
    // 从 $CARGO_MANIFEST_DIR 向上找 git 根。开发态走 cargo，$CARGO_MANIFEST_DIR
    // 总是存在；CI / 从 `cargo run` 启动亦然。
    let manifest = env::var("CARGO_MANIFEST_DIR")
        .map_err(|_| std::io::Error::other("CARGO_MANIFEST_DIR not set"))?;
    let mut cur = PathBuf::from(manifest);
    loop {
        if cur.join(".git").exists()
            || cur.join("Cargo.toml").is_file()
                && cur.join("Cargo.toml").metadata().is_ok()
                && read_is_workspace(&cur.join("Cargo.toml"))
        {
            return Ok(cur);
        }
        if !cur.pop() {
            return Err(std::io::Error::other("repo root not found"));
        }
    }
}

fn read_is_workspace(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|s| s.contains("[workspace]"))
}
