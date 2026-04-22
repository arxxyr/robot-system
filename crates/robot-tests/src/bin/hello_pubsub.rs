//! Step 5 交付示例：同进程 iceoryx2 pub + sub，发 3 条 JointState，打印收到
//! 的 header + body 字段。
//!
//! 对应 `docs/01_implementation_steps.md` Step 5 交付 "同进程 pub → sub 收到
//! JointState"。跑通 = Step 5 最小闭环达成。
//!
//! ```
//! cargo run -p robot-tests --bin hello_pubsub
//! ```

use flatbuffers::FlatBufferBuilder;
use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::markers;
use robot_msg::msg::{JointState as FbJointState, JointStateArgs};
use robot_transport::iceoryx2::{TopicClass, channel};
use robot_transport::traits::{LatestSubscriber, Publisher};

const TOPIC: &str = "/demo_robot/s0/left_arm/state";

fn build_joint_state(stamp_ns: u64, positions: &[f64]) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let pos = fbb.create_vector::<f64>(positions);
    let js = FbJointState::create(
        &mut fbb,
        &JointStateArgs {
            layout_id: 0xCAFE_BABE_DEAD_BEEF,
            position_rad: Some(pos),
            velocity_rps: None,
            effort_nm: None,
            stamp_ns,
            frame_id: None,
        },
    );
    fbb.finish(js, None);
    fbb.finished_data().to_vec()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== hello_pubsub · iceoryx2 同进程 pub/sub 演示 ===");
    println!("topic: {TOPIC}");
    println!();

    let clock = SystemClock::new();
    let node = NodeBuilder::new().create::<IxService>()?;

    let (mut pub_, mut sub) = channel::<markers::JointState>(&node, TOPIC, TopicClass::S0State)?;

    let samples: [&[f64]; 3] = [&[0.0, 0.0, 0.0], &[0.1, 0.2, 0.3], &[0.7, 0.5, 0.3]];
    for (seq, positions) in samples.iter().enumerate() {
        let stamp = clock.now().0;
        let body = build_joint_state(stamp, positions);
        // Control 档上限 8 KiB；FrameHeader + JointState 此处远低于上限。
        let upper = 1024 + body.len();
        let mut w = pub_.loan(upper)?;
        w.set_stamp_ns(stamp);
        w.set_seq(seq as u64);
        w.set_source_id(1);
        w.set_body_bytes(&body);
        w.publish()?;
        println!(
            "  pub seq={seq} stamp_ns={stamp} positions={positions:?} body_len={}",
            body.len()
        );
    }

    // drain 所有样本，逐条打印（非 latest() —— 这里用 loop + receive 语义
    // 要走 Iceoryx2Subscriber::iceoryx2_latest，只拿最新一条。MVP 展示
    // latest 语义即可，history 回放留 Step 6+）。
    println!();
    if let Some(lease) = sub.latest()? {
        let view = lease.view()?;
        let js: robot_msg::msg::JointState<'_> = view.body.get();
        println!(
            "  sub latest → schema={} stamp_ns={} layout_id={:#018x} pos={:?}",
            view.header.schema_id(),
            view.header.stamp_ns(),
            js.layout_id(),
            js.position_rad()
                .map(|v| v.iter().collect::<Vec<f64>>())
                .unwrap_or_default(),
        );
    } else {
        println!("  sub latest → (empty; publisher 未到？)");
    }

    println!();
    println!("=== 完成 ===");
    Ok(())
}
