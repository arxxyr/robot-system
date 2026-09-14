//! 注入器独占线程持有 raw IPC publisher，避免提前 Drop 丢失待消费命令。

use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
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
    worker: Option<JoinHandle<Result<(), String>>>,
    ready_rx: Option<Receiver<Result<(), String>>>,
    ready_result: Option<Result<(), String>>,
    finish_result: Option<Result<(), String>>,
}

impl ControlPort {
    pub(super) fn new(robot_id: &str) -> Result<Self, String> {
        let mut port = Self::spawn(robot_id)?;
        match port.wait_ready() {
            Ok(()) => Ok(port),
            Err(ready_error) => {
                let finished = port.finish();
                Err(format!(
                    "注入器初始化失败：{ready_error}；真实收尾={finished:?}"
                ))
            }
        }
    }

    /// 线程成功创建即交出完整所有权；调用方可先存入拥有域，再等待初始化。
    pub(super) fn spawn(robot_id: &str) -> Result<Self, String> {
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
                if let Err(error) = &result {
                    // 初始化错误交给原 ready 通道；运行期错误仍保留在线程返回值。
                    // ready 成功消息尚未消费时不能阻塞发送第二条消息。
                    let _ = ready_tx.try_send(Err(error.clone()));
                }
                result
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
            ready_rx: Some(ready_rx),
            ready_result: None,
            finish_result: None,
        })
    }

    pub(super) fn wait_ready(&mut self) -> Result<(), String> {
        if let Some(result) = &self.ready_result {
            return result.clone();
        }
        let result = self
            .ready_rx
            .take()
            .ok_or_else(|| "原 ready 接收端缺失".to_owned())
            .and_then(|receiver| {
                receiver
                    .recv_timeout(Duration::from_secs(3))
                    .map_err(|error| format!("注入器 ready 等待失败：{error}"))?
            });
        self.ready_result = Some(result.clone());
        result
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

    /// 断原发送端后，只有原线程真 join 且成功才能返回 Ok；重复调用保留原结果。
    pub(super) fn finish(&mut self) -> Result<(), String> {
        if let Some(result) = &self.finish_result {
            return result.clone();
        }
        self.sender.take();
        let deadline = SystemClock.now().0 + 2_000_000_000;
        let Some(worker) = self.worker.as_ref() else {
            return Err("注入器原线程句柄缺失，不能证明真 join".into());
        };
        while !worker.is_finished() {
            if SystemClock.now().0 >= deadline {
                eprintln!("注入器原线程 2s 内未能真 join，禁止丢弃原句柄");
                std::process::abort();
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let worker = self.worker.take().ok_or("注入器真 join 前丢失原句柄")?;
        let joined = worker.join().unwrap_or_else(|panic| {
            let reason = match (panic.downcast_ref::<&str>(), panic.downcast_ref::<String>()) {
                (Some(reason), _) => (*reason).to_owned(),
                (_, Some(reason)) => reason.clone(),
                _ => "非字符串 panic payload".into(),
            };
            Err(format!("注入器原线程 panic：{reason}"))
        });
        let result = match (&self.ready_result, joined) {
            (Some(Err(ready_error)), joined) => Err(format!(
                "原 ready 失败：{ready_error}；原线程 join 结果={joined:?}"
            )),
            (_, joined) => joined,
        };
        self.ready_rx.take();
        self.finish_result = Some(result.clone());
        result
    }
}

impl Drop for ControlPort {
    fn drop(&mut self) {
        // 显式 finish 已把原错误交给调用方；旧调用方则不能靠 Drop 吞错后报 PASS。
        if self.finish_result.is_none()
            && let Err(error) = self.finish()
        {
            eprintln!("注入器隐式收尾失败：{error}");
            std::process::abort();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 使用真实线程、原 channel 与真实 join，仅验证拥有器，不代表 daemon 验收。
    fn port_with_worker<F>(run: F) -> ControlPort
    where
        F: FnOnce(Receiver<SendRequest>, SyncSender<Result<(), String>>) -> Result<(), String>
            + Send
            + 'static,
    {
        let (sender, receiver) = sync_channel(1);
        let (ready_tx, ready_rx) = sync_channel(1);
        let worker = std::thread::spawn(move || run(receiver, ready_tx));
        ControlPort {
            sender: Some(sender),
            worker: Some(worker),
            ready_rx: Some(ready_rx),
            ready_result: None,
            finish_result: None,
        }
    }

    #[test]
    fn ready_error_preserves_original_worker_until_sender_closed_and_real_join() {
        let mut port = port_with_worker(|receiver, ready| {
            ready.send(Err("真实初始化失败".into())).unwrap();
            assert!(receiver.recv().is_err());
            Err("真实 worker 初始化返回值".into())
        });
        assert!(port.wait_ready().unwrap_err().contains("真实初始化失败"));
        // 原 sender 尚在，worker 必须停留于真实 recv，不使用假完成标记。
        assert!(
            port.worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
        );
        let error = port.finish().unwrap_err();
        assert!(error.contains("真实初始化失败"));
        assert!(error.contains("真实 worker 初始化返回值"));
        assert!(port.worker.is_none());
        assert!(port.sender.is_none());
        assert_eq!(port.finish(), Err(error));
    }

    #[test]
    fn finish_closes_original_sender_and_only_accepts_real_successful_join() {
        let mut port = port_with_worker(|receiver, ready| {
            ready.send(Ok(())).unwrap();
            assert!(receiver.recv().is_err());
            Ok(())
        });
        assert_eq!(port.wait_ready(), Ok(()));
        assert_eq!(port.wait_ready(), Ok(()));
        assert!(
            port.worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
        );
        assert_eq!(port.finish(), Ok(()));
        assert!(port.worker.is_none());
        assert!(port.sender.is_none());
        assert_eq!(port.finish(), Ok(()));
    }

    #[test]
    fn ready_timeout_keeps_original_live_worker_until_explicit_join() {
        let mut port = port_with_worker(|receiver, _ready| {
            // 保持原 ready sender 活着且不发布，真实3s超时不能变成断连捷径。
            assert!(receiver.recv().is_err());
            Ok(())
        });
        assert!(port.wait_ready().is_err());
        assert!(
            port.worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
        );
        assert!(port.sender.is_some());
        assert!(port.finish().unwrap_err().contains("原 ready 失败"));
        assert!(port.worker.is_none());
    }

    #[test]
    fn runtime_error_survives_successful_ready_and_real_join() {
        let mut port = port_with_worker(|receiver, ready| {
            ready.send(Ok(())).unwrap();
            assert!(receiver.recv().is_err());
            Err("真实运行期失败".into())
        });
        assert_eq!(port.wait_ready(), Ok(()));
        assert_eq!(port.finish(), Err("真实运行期失败".into()));
        assert!(port.worker.is_none());
        assert_eq!(port.finish(), Err("真实运行期失败".into()));
    }

    #[test]
    fn panic_after_ready_is_returned_from_original_join() {
        let mut port = port_with_worker(|receiver, ready| {
            ready.send(Ok(())).unwrap();
            assert!(receiver.recv().is_err());
            panic!("真实运行期 panic");
        });
        assert_eq!(port.wait_ready(), Ok(()));
        let error = port.finish().unwrap_err();
        assert!(error.contains("真实运行期 panic"));
        assert!(port.worker.is_none());
        assert_eq!(port.finish(), Err(error));
    }

    #[test]
    fn initialization_panic_keeps_ready_disconnect_and_original_join_error() {
        let mut port = port_with_worker(|_receiver, _ready| {
            panic!("真实初始化 panic");
        });
        assert!(port.wait_ready().is_err());
        assert!(port.worker.is_some());
        let error = port.finish().unwrap_err();
        assert!(error.contains("原 ready 失败"));
        assert!(error.contains("真实初始化 panic"));
        assert!(port.worker.is_none());
    }
}
