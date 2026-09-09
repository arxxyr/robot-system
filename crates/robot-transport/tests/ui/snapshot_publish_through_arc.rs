//! 编译期负向测试（S48 A3b）：经 `Arc` 共享的写端只能得到 `&self`，多线程各自
//! 调用 `publish` 必须编译失败（旧实现的 `publish(&self)` 允许这一点）。

use std::sync::Arc;
use std::thread;

use robot_transport::RtSnapshotPublisher;

fn main() {
    let (publisher, _reader) = RtSnapshotPublisher::new(0u64);
    let shared = Arc::new(publisher);
    let writers: Vec<_> = (0..2u64)
        .map(|id| {
            let shared = Arc::clone(&shared);
            thread::spawn(move || shared.publish(&id).unwrap())
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
}
