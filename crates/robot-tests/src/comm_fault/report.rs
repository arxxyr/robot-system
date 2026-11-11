//! 独立进程验收的证据记录；未知构建特性绝不推断为生产构建。

use std::path::{Path, PathBuf};

use super::Args;
use robot_core::time::{RobotClock, SystemClock};
use serde_json::{Value, json};

#[cfg(test)]
#[path = "report_acceptance.rs"]
mod acceptance;

pub(super) fn file_hash(path: &Path) -> Result<String, String> {
    let output = std::process::Command::new("sha256sum")
        .arg(path)
        .output()
        .map_err(|error| format!("计算 {} 的 SHA256 失败：{error}", path.display()))?;
    if !output.status.success() {
        return Err(format!(
            "SHA256失败：{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
    let hash = text.split_whitespace().next().ok_or("SHA256输出为空")?;
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("SHA256输出无效".to_owned());
    }
    Ok(hash.to_owned())
}

/// 清单只接受构建时明确记录的字段；不从路径、文件名或已有场景结果推断特性。
fn verify_build_manifest(name: &str, manifest: &Value, hash: &str) -> Result<(), String> {
    if manifest["sha256"].as_str() != Some(hash)
        || hash.len() != 64
        || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(format!("{name} 构建清单与实际产物 SHA256 不符"));
    }
    let features = manifest["features"]
        .as_array()
        .filter(|features| !features.is_empty())
        .ok_or_else(|| format!("{name} 构建 features 必须为非空数组"))?;
    for (index, value) in features.iter().enumerate() {
        let feature = value
            .as_str()
            .filter(|feature| {
                !feature.is_empty()
                    && !feature.chars().any(char::is_whitespace)
                    && !feature.eq_ignore_ascii_case("unknown")
            })
            .ok_or_else(|| format!("{name} 构建 feature 必须是明确的非空字符串"))?;
        if features[..index]
            .iter()
            .any(|prior| prior.as_str() == Some(feature))
        {
            return Err(format!("{name} 构建 feature 重复：{feature}"));
        }
    }
    let required = match name {
        "robot-s0" => Some("test-comm-fault-hook"),
        "comm_fault_e2e" => Some("comm-fault-e2e"),
        "robot-gateway" => None,
        _ => return Err(format!("未知验收产物角色：{name}")),
    };
    if let Some(required) = required
        && !features
            .iter()
            .any(|feature| feature.as_str() == Some(required))
    {
        return Err(format!("{name} 缺少明确构建 feature：{required}"));
    }
    if !matches!(
        manifest["profile"].as_str(),
        Some("dev" | "release" | "test")
    ) {
        return Err(format!("{name} 构建 profile 必须明确为 dev/release/test"));
    }
    let command = manifest["command"]
        .as_array()
        .filter(|command| !command.is_empty())
        .ok_or_else(|| format!("{name} 构建 command 必须为非空 argv 数组"))?;
    if command.iter().any(|argument| {
        argument.as_str().is_none_or(|argument| {
            argument.trim().is_empty() || argument.eq_ignore_ascii_case("unknown")
        })
    }) {
        return Err(format!("{name} 构建 command 每项必须是明确的非空字符串"));
    }
    if manifest["exit_code"].as_i64() != Some(0) {
        return Err(format!("{name} 构建清单缺少成功退出码 0"));
    }
    Ok(())
}

fn read_build_manifest(directory: &Path, name: &str, hash: &str) -> Result<Value, String> {
    let path = directory.join(format!("{name}.build.json"));
    let bytes = std::fs::read(&path)
        .map_err(|error| format!("读取 {} 的明确构建清单失败：{error}", path.display()))?;
    let manifest: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{} 构建清单无效：{error}", path.display()))?;
    verify_build_manifest(name, &manifest, hash)?;
    Ok(manifest)
}

pub(super) struct Report {
    pub(super) directory: PathBuf,
    pub(super) cases: Vec<Value>,
    pub(super) artifacts: Vec<Value>,
    invocation: Value,
    started_ns: u64,
}

impl Report {
    pub(super) fn new(args: &Args) -> Result<Self, String> {
        let directory = &args.evidence_dir;
        let bin_dir = &args.bin_dir;
        let current_exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let current_hash = file_hash(&current_exe)?;
        if current_hash != file_hash(&bin_dir.join("comm_fault_e2e"))? {
            return Err("执行中的验收工具与 --bin-dir 中工具 SHA256 不同".to_owned());
        }
        let invocation = json!({"phase": format!("{:?}", args.phase), "case_filter": args.case,
            "config": args.config, "config_sha256": args.config.as_deref().map(file_hash).transpose()?,
            "hardware_id": args.hardware_id, "current_exe": current_exe, "current_exe_sha256": current_hash});
        std::fs::create_dir_all(directory).map_err(|error| format!("创建证据目录失败：{error}"))?;
        let mut artifacts = Vec::new();
        for name in ["robot-s0", "robot-gateway", "comm_fault_e2e"] {
            let path = bin_dir.join(name);
            let hash = file_hash(&path)?;
            let build = read_build_manifest(bin_dir, name, &hash)?;
            artifacts.push(json!({"name": name, "path": path, "sha256": hash, "build": build}));
        }
        Ok(Self {
            directory: directory.to_owned(),
            cases: Vec::new(),
            artifacts,
            invocation,
            started_ns: SystemClock.now().0,
        })
    }

    pub(super) fn record(
        &mut self,
        name: &str,
        ac: &str,
        started_ns: u64,
        result: Result<Value, String>,
    ) {
        let elapsed_ns = SystemClock.now().0.saturating_sub(started_ns);
        let case = match result {
            Ok(evidence) => {
                json!({"case": name, "ac": ac, "status": "PASS", "elapsed_ns": elapsed_ns, "evidence": evidence})
            }
            Err(error) => {
                json!({"case": name, "ac": ac, "status": "FAIL", "elapsed_ns": elapsed_ns, "error": error})
            }
        };
        eprintln!(
            "case={name} ac={ac} status={}",
            case["status"].as_str().unwrap_or("FAIL")
        );
        self.cases.push(case);
    }

    pub(super) fn finish(&self) -> Result<bool, String> {
        let passed =
            !self.cases.is_empty() && self.cases.iter().all(|case| case["status"] == "PASS");
        let hardware_status = |key: &str| {
            self.cases
                .iter()
                .find(|case| {
                    case["ac"]
                        .as_str()
                        .is_some_and(|ac| ac.split('/').any(|part| part == key))
                })
                .and_then(|case| case["status"].as_str())
                .unwrap_or("NEEDS_HUMAN")
        };
        let payload = json!({
            "schema_version": 1, "status": if passed { "PASS" } else { "FAIL" },
            "started_monotonic_ns": self.started_ns, "finished_monotonic_ns": SystemClock.now().0,
            "artifacts": self.artifacts, "cases": self.cases,
            "invocation": self.invocation,
            "hardware": {"H1": hardware_status("H1"), "H2": hardware_status("H2"),
                "H3": hardware_status("H3"), "D15": "OPEN"}
        });
        let bytes = serde_json::to_vec_pretty(&payload).map_err(|error| error.to_string())?;
        let path = self.directory.join("report.json");
        std::fs::write(&path, bytes).map_err(|error| format!("写验收报告失败：{error}"))?;
        Ok(passed)
    }
}
