//! 编译期负向测试：`MessageView<'_, T>` 借用 lease 的 bytes；lease drop 后
//! 再用 view 必须编译失败。

use robot_msg::markers::JointState;
use robot_transport::mock::MockChannel;
use robot_transport::traits::{LatestSubscriber, Publisher};

fn main() {
    let ch = MockChannel::<JointState>::new();
    let mut publisher = ch.publisher();
    let mut subscriber = ch.subscriber();

    // 构造一条合法 sample
    let body = {
        let mut fbb = flatbuffers::FlatBufferBuilder::new();
        let js = robot_msg::msg::JointState::create(
            &mut fbb,
            &robot_msg::msg::JointStateArgs {
                layout_id: 1,
                position_rad: None,
                velocity_rps: None,
                effort_nm: None,
                stamp_ns: 0,
                frame_id: None,
            },
        );
        fbb.finish(js, None);
        fbb.finished_data().to_vec()
    };
    let mut w = publisher.loan().unwrap();
    w.set_body_bytes(&body);
    w.publish().unwrap();

    let lease = subscriber.latest().unwrap().unwrap();
    let view = lease.view().unwrap();
    drop(lease); // move-out / drop 掉 lease
    // view 借用 lease 内字节；lease 已不在 → 编译错误
    let _hdr = view.header;
}
