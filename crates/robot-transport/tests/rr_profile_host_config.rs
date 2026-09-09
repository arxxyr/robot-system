//! 单独测试进程只初始化一次全局配置，先证明真实 TOML 生效，再验证显式 RR profile。

#![cfg(feature = "iceoryx2")]

use std::io::Write;

use iceoryx2::prelude::{
    Config, FilePath, MessagingPattern, NodeBuilder, SemanticString, Service, ipc,
};
use robot_msg::markers;
use robot_transport::iceoryx2::{RrProfile, RrStaticLimits, rr_server_with, topic_to_service_name};

struct ConfigFile(std::path::PathBuf);

impl Drop for ConfigFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn host_toml_l16_is_loaded_then_explicit_profile_yields_l4() {
    let path = std::env::temp_dir().join(format!("robot_rr_profile_{}.toml", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    let cleanup = ConfigFile(path);
    file.write_all(b"[defaults.request-response]\nmax-active-requests-per-client = 16\n")
        .unwrap();
    drop(file);
    let config_path = FilePath::new(cleanup.0.as_os_str().as_encoded_bytes()).unwrap();
    let config = Config::setup_global_config_from_file(&config_path).unwrap();
    // 在创建任何 node 之前核真实加载值，不能让错误 TOML 键被默认值掩盖。
    assert_eq!(
        config
            .defaults
            .request_response
            .max_active_requests_per_client,
        16
    );
    let node = NodeBuilder::new().create::<ipc::Service>().unwrap();
    let topic = format!("/rr_profile_host_config_{}", std::process::id());
    let server = rr_server_with::<markers::ClearEStopRequest, markers::ClearEStopResponse>(
        &node,
        &topic,
        &RrProfile::S0_CONTROL,
    )
    .unwrap();
    assert_eq!(server.limits(), Some(RrStaticLimits { l: 4, c: 8 }));
    let details = ipc::Service::details(
        &topic_to_service_name(&topic).unwrap(),
        config,
        MessagingPattern::RequestResponse,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        details
            .static_details
            .request_response()
            .max_active_requests_per_client(),
        4
    );
    assert_eq!(details.static_details.request_response().max_clients(), 8);
    assert_eq!(details.static_details.request_response().max_servers(), 1);
    assert!(
        !details
            .static_details
            .request_response()
            .has_safe_overflow_for_requests()
    );
    assert!(
        details
            .static_details
            .request_response()
            .does_support_fire_and_forget_requests()
    );
    drop(server);
}
