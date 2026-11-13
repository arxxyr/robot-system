//! 两份当前总线配置经隔离网关回传状态；不启动S0/S1，不打开串口。
//! 测试身份与端口逐轮生成，禁止订阅实机robot ID。

#![cfg(all(target_os = "linux", feature = "client"))]
#![allow(clippy::doc_markdown)]

mod common;

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::fake_state::FakeStatePublisher;
use common::subprocess::{SubprocessHandle, build_gateway_command, locate_test_bin};
use robot_core::time::{RobotClock, SystemClock};
use robot_rpc::client::layout::{HardwareLayout, load_hardware_layout, validate_state_layout};
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{GetStateRequest, RobotState, S0SafetyState};
use tonic::Code;
use tonic::transport::{Channel, Endpoint};

const READY_TIMEOUT: Duration = Duration::from_secs(5);
const RPC_TIMEOUT: Duration = Duration::from_millis(500);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

struct BusFixture {
    robot_id: String,
    hardware_id: String,
    layout: HardwareLayout,
    client: RobotControlClient<Channel>,
    gateway: SubprocessHandle,
}

impl BusFixture {
    async fn start(root: &Path, bus: &str, gateway_bin: &Path, roles: &Path) -> Self {
        let original_path = workspace_root().join(format!("configs/xlerobot-{bus}.toml"));
        let mut config: toml::Value =
            toml::from_str(&std::fs::read_to_string(&original_path).expect("读取当前总线配置"))
                .expect("解析当前总线配置");
        let original_id = config["robot"]["id"]
            .as_str()
            .expect("原robot ID")
            .to_owned();
        let robot_id = format!(
            "status_bus_e2e_{}_{}_{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple(),
            bus
        );
        assert_ne!(robot_id, original_id, "隔离测试禁止使用原实机身份");
        config["robot"]["id"] = toml::Value::String(robot_id.clone());
        let hardware = config["hardware"].as_array().expect("hardware数组");
        assert_eq!(hardware.len(), 1, "当前总线配置仅选择一个hardware");
        let hardware_id = hardware[0]["id"].as_str().expect("hardware ID").to_owned();
        let config_path = root.join(format!("{bus}.toml"));
        std::fs::write(
            &config_path,
            toml::to_string(&config).expect("编码隔离配置"),
        )
        .expect("写入隔离配置");
        let layout = load_hardware_layout(&config_path, &hardware_id).expect("从配置派生布局");
        let original_layout =
            load_hardware_layout(&original_path, &hardware_id).expect("读取原配置布局");
        assert_eq!(layout.joints, original_layout.joints);
        assert_eq!(layout.motor_ids, original_layout.motor_ids);
        assert_ne!(layout.layout_id, original_layout.layout_id);

        let address = unused_loopback_address();
        let mut gateway = SubprocessHandle::spawn(
            "status-bus-gateway",
            build_gateway_command(gateway_bin, &robot_id, address, roles, Some(&config_path)),
        )
        .expect("启动隔离网关");
        gateway
            .wait_for_marker("rr_io_threads started", READY_TIMEOUT)
            .await
            .expect("隔离网关初始化完成");
        let client = connect(address).await;
        Self {
            robot_id,
            hardware_id,
            layout,
            client,
            gateway,
        }
    }

    async fn get_state(&mut self) -> Result<RobotState, tonic::Status> {
        let mut request = tonic::Request::new(GetStateRequest {
            hardware_id: self.hardware_id.clone(),
        });
        request.set_timeout(RPC_TIMEOUT);
        tokio::time::timeout(RPC_TIMEOUT, self.client.get_state(request))
            .await
            .expect("只读RPC必须有界返回")
            .map(tonic::Response::into_inner)
    }

    async fn wait_for_state(&mut self) -> RobotState {
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            match self.get_state().await {
                Ok(state) => return state,
                Err(status)
                    if status.code() == Code::Unavailable
                        && tokio::time::Instant::now() < deadline => {}
                Err(status) => panic!("假状态未能送达隔离网关：{status}"),
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn shutdown(self, root: &Path) {
        drop(self.client);
        let status = self
            .gateway
            .shutdown_with_dump(READY_TIMEOUT, root.join(format!("{}.log", self.robot_id)))
            .await
            .expect("回收隔离网关和输出任务");
        assert!(status.success(), "隔离网关退出失败：{status}");
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace根目录")
        .to_path_buf()
}

fn unused_loopback_address() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .expect("分配本机临时端口")
        .local_addr()
        .expect("查询本机临时端口")
}

async fn connect(address: SocketAddr) -> RobotControlClient<Channel> {
    let endpoint = Endpoint::from_shared(format!("http://{address}"))
        .expect("本机端点")
        .connect_timeout(RPC_TIMEOUT)
        .timeout(RPC_TIMEOUT);
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        if let Ok(Ok(channel)) = tokio::time::timeout(RPC_TIMEOUT, endpoint.connect()).await {
            return RobotControlClient::new(channel);
        }
        assert!(tokio::time::Instant::now() < deadline, "连接隔离网关超时");
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[tokio::test]
async fn current_bus_layouts_round_trip_without_s0_or_s1() {
    let root = tempfile::tempdir().expect("隔离配置与日志目录");
    let gateway_bin = locate_test_bin("robot-gateway").expect("定位同一隔离target的网关产物");
    let roles = workspace_root().join("configs/rpc-roles.toml");
    let mut bus1 = BusFixture::start(root.path(), "bus1", &gateway_bin, &roles).await;
    let mut bus2 = BusFixture::start(root.path(), "bus2", &gateway_bin, &roles).await;
    assert_eq!(bus1.layout.joints.len(), 8);
    assert_eq!(bus2.layout.joints.len(), 9);
    assert_ne!(bus1.robot_id, bus2.robot_id);

    for bus in [&mut bus1, &mut bus2] {
        assert_eq!(
            bus.get_state()
                .await
                .expect_err("发布前不得伪造状态")
                .code(),
            Code::Unavailable
        );
    }

    let stamp = SystemClock.now().0;
    let mut publishers = Vec::new();
    for bus in [&bus1, &bus2] {
        publishers.push(
            FakeStatePublisher::spawn(
                &bus.robot_id,
                &bus.hardware_id,
                bus.layout.joints.len(),
                bus.layout.layout_id,
                Some(stamp),
            )
            .expect("启动隔离假状态源"),
        );
    }
    for bus in [&mut bus1, &mut bus2] {
        let state = bus.wait_for_state().await;
        assert_eq!(state.robot_id, bus.robot_id);
        assert_eq!(state.hardware_id, bus.hardware_id);
        validate_state_layout(&bus.layout, &state).expect("实际配置布局一致");
        assert_eq!(state.position_rad.len(), bus.layout.joints.len());
        assert_eq!(state.position_rad, vec![0.0; bus.layout.joints.len()]);
        assert_eq!(state.stamp_ns, stamp);
        assert_eq!(state.safety_state, S0SafetyState::TorqueUnknown as i32);
    }

    for publisher in publishers {
        publisher.shutdown();
    }
    // 已读到首帧后停止来源。重复查询仍是该源时间戳，不能用RPC收包时间续鲜。
    for _ in 0..3 {
        tokio::time::sleep(POLL_INTERVAL).await;
        for bus in [&mut bus1, &mut bus2] {
            let cached = bus.get_state().await.expect("保留最后一次状态缓存");
            assert_eq!(cached.stamp_ns, stamp);
            assert!(SystemClock.now().0 > cached.stamp_ns);
            assert_eq!(cached.safety_state, S0SafetyState::TorqueUnknown as i32);
        }
    }

    bus1.shutdown(root.path()).await;
    bus2.shutdown(root.path()).await;
}
