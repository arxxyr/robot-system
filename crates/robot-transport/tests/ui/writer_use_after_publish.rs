//! 编译期负向测试：`SampleWriter::publish(self)` 按值消费 writer；publish 后
//! 继续写 body 必须编译失败（"use after move"）。

use robot_msg::markers::JointState;
use robot_transport::mock::MockChannel;
use robot_transport::traits::Publisher;

fn main() {
    let ch = MockChannel::<JointState>::new();
    let mut publisher = ch.publisher();

    let mut w = publisher.loan(65536).unwrap();
    w.set_body_bytes(&[]);
    w.publish().unwrap();
    // publish 消费 self，继续用 w 应编译失败
    w.set_body_bytes(&[]);
}
