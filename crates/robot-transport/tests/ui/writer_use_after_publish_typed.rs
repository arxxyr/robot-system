//! typed publish 同样按值消费 writer，不能重用已发送的借用。
use robot_msg::markers::JointState;
use robot_transport::mock::MockChannel;
use robot_transport::traits::Publisher;

fn main() {
    let channel = MockChannel::<JointState>::new();
    let mut publisher = channel.publisher();
    let mut writer = publisher.loan(1024).unwrap();
    let _ = writer.publish_typed();
    writer.set_seq(2);
}
