//! 注入器独占线程持有 raw IPC publisher，避免提前 Drop 丢失待消费命令。

use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use iceoryx2::port::update_connections::UpdateConnections;
use iceoryx2::prelude::{NodeBuilder, ServiceName};
use iceoryx2::service::ipc::Service as IxService;
use robot_core::time::{RobotClock, SystemClock};
use robot_s0::comm_fault_hook::TestCommFaultCommand;

use super::harness::Case;

type SendRequest = (TestCommFaultCommand, SyncSender<Result<(), String>>);

pub(super) struct ControlPort {
    sender: Option<SyncSender<SendRequest>>,
    worker: Option<JoinHandle<()>>,
}

impl ControlPort {
    pub(super) fn new(robot_id: &str) -> Result<Self, String> {
        let topic = format!("/{robot_id}/test/comm_fault");
        let (sender, receiver) = sync_channel::<SendRequest>(1);
        let (ready_tx, ready_rx) = sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("验收注入器".into())
            .spawn(move || {
                let result = (|| -> Result<(), String> {
                    let node = NodeBuilder::new()
                        .create::<IxService>()
                        .map_err(|e| format!("{e:?}"))?;
                    let name = ServiceName::new(&topic).map_err(|e| format!("{e:?}"))?;
                    let service = node
                        .service_builder(&name)
                        .publish_subscribe::<TestCommFaultCommand>()
                        .max_publishers(1)
                        .max_subscribers(1)
                        .history_size(1)
                        .subscriber_max_buffer_size(8)
                        .enable_safe_overflow(false)
                        .open_or_create()
                        .map_err(|e| format!("{e:?}"))?;
                    let publisher = service
                        .publisher_builder()
                        .create()
                        .map_err(|e| format!("{e:?}"))?;
                    ready_tx.send(Ok(())).map_err(|e| e.to_string())?;
                    loop {
                        let (command, reply) = match receiver.recv_timeout(Duration::from_millis(1))
                        {
                            Ok(request) => request,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                                publisher
                                    .update_connections()
                                    .map_err(|e| format!("{e:?}"))?;
                                continue;
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        };
                        let result = publisher
                            .loan_uninit()
                            .map_err(|e| format!("{e:?}"))
                            .and_then(|sample| {
                                sample
                                    .write_payload(command)
                                    .send()
                                    .map(|_| ())
                                    .map_err(|e| format!("{e:?}"))
                            });
                        let _ = reply.send(result);
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    let _ = ready_tx.send(Err(error));
                }
            })
            .map_err(|e| e.to_string())?;
        ready_rx
            .recv_timeout(Duration::from_secs(3))
            .map_err(|e| e.to_string())??;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
        })
    }

    pub(super) fn send(&self, command: TestCommFaultCommand) -> Result<(), String> {
        let (reply, receiver) = sync_channel(1);
        self.sender
            .as_ref()
            .ok_or("注入器已关闭")?
            .try_send((command, reply))
            .map_err(|e| e.to_string())?;
        receiver
            .recv_timeout(Duration::from_secs(2))
            .map_err(|e| e.to_string())?
    }
}

impl Drop for ControlPort {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(super) async fn wait_applied(
    case: &Case,
    child: &mut tokio::process::Child,
    sequence: u64,
) -> Result<(), String> {
    let deadline = SystemClock.now().0 + 3_000_000_000;
    loop {
        let observation = case.observation("s0")?;
        if observation.hook_ready && observation.command_applied == sequence {
            return Ok(());
        }
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!("注入未确认，S0已退出：{status}"));
        }
        if SystemClock.now().0 >= deadline {
            return Err(format!("注入seq={sequence}未确认：{observation:?}"));
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}
