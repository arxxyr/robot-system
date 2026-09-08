//! 资源观测只读本轮目录：全局数量供诊断，归属本轮 PID 的 Node 集合才是回收判据。

use std::collections::BTreeSet;
use std::io;
use std::path::Path;

use iceoryx2_bb_posix::unique_system_id::UniqueSystemId;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct StorageCounts {
    pub files: u64,
    /// 包含被扫描根目录自身；符号链接既不跟随，也不计为目录。
    pub directories: u64,
}

fn require_directory(path: &Path) -> io::Result<()> {
    if std::fs::symlink_metadata(path)?.file_type().is_dir() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} 必须是实际目录，不能是文件或符号链接",
            path.display()
        )))
    }
}

/// 根路径缺失是采样错误；递归中其它进程刚删除的条目允许忽略。
pub(super) fn count_storage(root: &Path) -> io::Result<StorageCounts> {
    require_directory(root)?;
    count_directory(root)
}

fn count_directory(root: &Path) -> io::Result<StorageCounts> {
    require_directory(root)?;
    let mut counts = StorageCounts {
        files: 0,
        directories: 1,
    };
    for entry in std::fs::read_dir(root)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let kind = match entry.file_type() {
            Ok(kind) => kind,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if kind.is_file() {
            counts.files += 1;
        } else if kind.is_dir() {
            match count_directory(&entry.path()) {
                Ok(child) => {
                    counts.files += child.files;
                    counts.directories += child.directories;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(counts)
}

fn node_pid(id: u128) -> Result<u32, String> {
    u32::try_from(UniqueSystemId::from(id).pid().value())
        .map_err(|error| format!("Node ID {id} 的 PID 无效：{error}"))
}

/// 只扫描一级十进制 Node ID；空目录也记录，不读取目录内文件来猜测归属。
/// 调用方必须在扫描前后确认所选进程仍活着，排除 PID 重用与退出时清理。
pub(super) fn scan_owned_nodes(root: &Path, pids: &[u32]) -> Result<BTreeSet<u128>, String> {
    if pids.is_empty() || pids.contains(&0) {
        return Err("Node 目录扫描必须指定非零 PID".to_owned());
    }
    require_directory(root).map_err(|error| format!("Node 根目录：{error}"))?;
    let entries = std::fs::read_dir(root)
        .map_err(|error| format!("读取 Node 根目录 {}：{error}", root.display()))?;
    let mut owned = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("枚举 Node 条目：{error}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let id = name
            .parse::<u128>()
            .map_err(|error| format!("无效十进制 Node ID {name}：{error}"))?;
        if !pids.contains(&node_pid(id)?) {
            continue;
        }
        require_directory(&entry.path())
            .map_err(|error| format!("本轮 Node {id} 扫描失败：{error}"))?;
        owned.insert(id);
    }
    Ok(owned)
}

pub(super) fn validate_baseline(nodes: &BTreeSet<u128>, pids: &[u32]) -> Result<(), String> {
    if pids.is_empty() || pids.contains(&0) {
        return Err("Node 基线必须指定非零 PID".to_owned());
    }
    let represented_pids = nodes
        .iter()
        .map(|id| node_pid(*id))
        .collect::<Result<BTreeSet<_>, _>>()?;
    for pid in pids {
        if !represented_pids.contains(pid) {
            return Err(format!("PID {pid} 的 Node 基线为空，不能声称回收通过"));
        }
    }
    Ok(())
}

pub(super) fn validate_no_new_nodes(
    baseline: &BTreeSet<u128>,
    final_nodes: &BTreeSet<u128>,
) -> Result<(), String> {
    let added: Vec<_> = final_nodes.difference(baseline).copied().collect();
    if added.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "grace 后本轮 PID 新增 Node 目录仍未回收：{added:?}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "post_e_node_oracle_{}",
                UniqueSystemId::new().unwrap().value()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn node(&self, id: u128) -> PathBuf {
            let path = self.0.join(id.to_string());
            std::fs::create_dir(&path).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // 仅删除本测试独占的临时模型目录，不操作 iceoryx2 全局资源。
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn empty_nodes_count_and_other_process_nodes_do_not_interfere() {
        const CHILD: &str = "ROBOT_TEST_NODE_ORACLE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            println!("node_oracle_id={}", UniqueSystemId::new().unwrap().value());
            return;
        }
        // 让另一个真实进程通过上游 API 生成身份，不伪造 Node ID 位布局。
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "resource_oracle::tests::empty_nodes_count_and_other_process_nodes_do_not_interfere",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let foreign_id = stdout
            .lines()
            .find_map(|line| line.strip_prefix("node_oracle_id="))
            .unwrap()
            .parse::<u128>()
            .unwrap();
        let own_id = UniqueSystemId::new().unwrap().value();
        let process_id = node_pid(own_id).unwrap();
        assert_ne!(process_id, node_pid(foreign_id).unwrap());
        let fixture = Fixture::new();
        fixture.node(own_id);
        fixture.node(foreign_id);
        let owned = scan_owned_nodes(&fixture.0, &[process_id]).unwrap();
        assert_eq!(owned, BTreeSet::from([own_id]));
        validate_baseline(&owned, &[process_id]).unwrap();
        assert!(validate_baseline(&owned, &[process_id, node_pid(foreign_id).unwrap()]).is_err());
    }

    #[test]
    fn new_empty_directory_and_same_count_replacement_are_failures() {
        let fixture = Fixture::new();
        let first = UniqueSystemId::new().unwrap().value();
        let second = UniqueSystemId::new().unwrap().value();
        let pid = node_pid(first).unwrap();
        let first_path = fixture.node(first);
        let baseline = scan_owned_nodes(&fixture.0, &[pid]).unwrap();
        fixture.node(second);
        let final_nodes = scan_owned_nodes(&fixture.0, &[pid]).unwrap();
        assert_eq!(count_storage(&fixture.0).unwrap().files, 0);
        assert!(validate_no_new_nodes(&baseline, &final_nodes).is_err());
        std::fs::remove_dir(first_path).unwrap();
        let replaced = scan_owned_nodes(&fixture.0, &[pid]).unwrap();
        assert_eq!(baseline.len(), replaced.len());
        assert!(validate_no_new_nodes(&baseline, &replaced).is_err());
        validate_no_new_nodes(&baseline, &baseline).unwrap();
        validate_no_new_nodes(&baseline, &BTreeSet::new()).unwrap();
    }

    #[test]
    fn missing_or_invalid_owned_paths_cannot_pass() {
        let fixture = Fixture::new();
        let id = UniqueSystemId::new().unwrap().value();
        let pid = node_pid(id).unwrap();
        assert!(scan_owned_nodes(&fixture.0.join("missing"), &[pid]).is_err());
        assert!(validate_baseline(&BTreeSet::new(), &[pid]).is_err());
        assert!(scan_owned_nodes(&fixture.0, &[0]).is_err());
        let path = fixture.0.join(id.to_string());
        std::fs::write(&path, []).unwrap();
        assert!(scan_owned_nodes(&path, &[pid]).is_err());
        assert!(scan_owned_nodes(&fixture.0, &[pid]).is_err());
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&fixture.0, &path).unwrap();
        assert!(scan_owned_nodes(&fixture.0, &[pid]).is_err());
    }

    #[test]
    fn global_counts_include_empty_directories_without_following_symlinks() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.0.join("empty")).unwrap();
        std::fs::write(fixture.0.join("file"), b"payload").unwrap();
        std::os::unix::fs::symlink(&fixture.0, fixture.0.join("cycle")).unwrap();
        assert_eq!(
            count_storage(&fixture.0).unwrap(),
            StorageCounts {
                files: 1,
                directories: 2
            }
        );
        assert!(count_storage(&fixture.0.join("missing")).is_err());
        assert!(count_storage(&fixture.0.join("cycle")).is_err());
        assert!(count_storage(&fixture.0.join("file")).is_err());
    }
}
