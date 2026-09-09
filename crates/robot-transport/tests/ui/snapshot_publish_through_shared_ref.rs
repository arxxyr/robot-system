//! 编译期负向测试（S48 A3b）：`publish` 取 `&mut self`，共享引用不能发布。

use robot_transport::RtSnapshotPublisher;

fn publish_via_shared(publisher: &RtSnapshotPublisher<u64>) {
    publisher.publish(&1).unwrap();
}

fn main() {
    let (publisher, _reader) = RtSnapshotPublisher::new(0u64);
    publish_via_shared(&publisher);
}
