//! 独立心跳发布器只维持控制链路，不替代 S0、gateway 或执行器。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::{build_helpers::build_heartbeat, markers};
use robot_transport::iceoryx2::{TopicClass, publisher};
use robot_transport::traits::Publisher;

pub(super) struct HeartbeatFeed {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<(), String>>>,
}

impl HeartbeatFeed {
    pub(super) fn start(robot_id: &str) -> Result<Self, String> {
        let topic = format!("/{robot_id}/s1/heartbeat");
        let stop = Arc::new(AtomicBool::new(false));
        let running_stop = stop.clone();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("验收心跳".into())
            .spawn(move || {
                let node = NodeBuilder::new()
                    .create::<Service>()
                    .map_err(|e| format!("{e:?}"))?;
                let mut output =
                    publisher::<markers::Heartbeat>(&node, &topic, TopicClass::Heartbeat)
                        .map_err(|e| e.to_string())?;
                let mut sequence = 1;
                while !running_stop.load(Ordering::Acquire) {
                    let now = SystemClock.now().0;
                    let body = build_heartbeat(2, sequence, 50.0, now);
                    let mut loan = output.loan(1024 + body.len()).map_err(|e| e.to_string())?;
                    loan.set_seq(sequence);
                    loan.set_source_id(2);
                    loan.set_stamp_ns(now);
                    loan.set_body_bytes(&body);
                    loan.publish().map_err(|e| e.to_string())?;
                    if sequence == 1 {
                        let _ = sender.send(());
                    }
                    sequence += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        let feed = Self {
            stop,
            worker: Some(worker),
        };
        receiver
            .recv_timeout(Duration::from_secs(3))
            .map_err(|e| format!("心跳首样本失败：{e}"))?;
        Ok(feed)
    }

    pub(super) fn verify_running(&self) -> Result<(), String> {
        match self.worker.as_ref() {
            Some(worker) if !worker.is_finished() => Ok(()),
            _ => Err("验收心跳线程提前退出".to_owned()),
        }
    }
}

impl Drop for HeartbeatFeed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
