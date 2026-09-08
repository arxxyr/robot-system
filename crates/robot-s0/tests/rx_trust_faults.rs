//! 跨公共 API 的独立持久化与真实文件系统验收。

#[path = "support/post_e_marker.rs"]
mod post_e_marker;

use robot_s0::rx_trust::{
    MarkerCommand, MarkerCommandOutcome, RealMarkerFs, TrustLockError, acquire_trust_lock,
    consume_authorization, run_marker_command,
};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[test]
fn real_files_consume_once_with_private_permissions_and_independent_latch() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path();
    std::fs::write(dir.join("estop_latch"), b"1\n").unwrap();
    let lock = acquire_trust_lock(dir).unwrap();
    assert!(matches!(
        run_marker_command(&RealMarkerFs, dir, MarkerCommand::Revoke).unwrap(),
        MarkerCommandOutcome::Complete
    ));
    assert!(matches!(
        run_marker_command(&RealMarkerFs, dir, MarkerCommand::Authorize).unwrap(),
        MarkerCommandOutcome::Complete
    ));
    assert!(
        consume_authorization(&RealMarkerFs, dir)
            .unwrap()
            .authorized
    );
    assert!(
        !consume_authorization(&RealMarkerFs, dir)
            .unwrap()
            .authorized
    );
    for name in ["rx_trust_marker", "rx_trust_marker.bak", "rx_trust.lock"] {
        assert_eq!(
            std::fs::metadata(dir.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert_eq!(std::fs::read(dir.join("estop_latch")).unwrap(), b"1\n");
    drop(lock);
}

#[test]
fn real_lock_conflict_preserves_inode_and_existing_pid_and_release_reuses_file() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path();
    let lock = acquire_trust_lock(dir).unwrap();
    let path = dir.join("rx_trust.lock");
    let inode = std::fs::metadata(&path).unwrap().ino();
    let owner = std::fs::read(&path).unwrap();
    assert!(matches!(
        acquire_trust_lock(dir),
        Err(TrustLockError::Held { .. })
    ));
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(std::fs::read(&path).unwrap(), owner);
    drop(lock);
    let _second = acquire_trust_lock(dir).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
}
