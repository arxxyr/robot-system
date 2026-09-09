//! 编译期负向测试（S48 A3b）：`RtSnapshotPublisher` 写端不可 `Clone`。

use robot_transport::RtSnapshotPublisher;

fn main() {
    let (publisher, _reader) = RtSnapshotPublisher::new(0u64);
    let _second_writer = publisher.clone();
}
