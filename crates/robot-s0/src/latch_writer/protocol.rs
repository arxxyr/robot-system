//! Mailbox 协议：IPC ↔ writer 的命令 / 完成类型、单信用通道与两端句柄。
//!
//! `cmd` 与 `done` 各为 `bounded(1)`；IPC 只持有一份逻辑信用，`try_dispatch` 先在
//! [`CommandAnnounce`] 发布公告再 `try_send`，`Full` / `Disconnected` 时先撤回公告再
//! 归还信用；任何完成（含 `Refused`）都归还信用。`EStop` 义务不走命令信用。
//!
//! 完成类型全部 `Copy`；真实阶段与相位随完成一起上报，不扁平化为字符串。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};
use robot_core::control_authority::{CleanupKind, RepairTarget};
use robot_core::time::{DurationNs, MonoRawNs};

use super::announce::{AnnounceError, CommandAnnounce};
use super::obligation::{EStopObligation, IdExhausted};
use super::progress::WriterProgress;
use crate::clear_estop_transaction::{ClearTxnStage, StepPhase};

/// Clear 事务身份（由 IPC 侧 owner 决定，writer 只核对不推断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClearTxnIdentity {
    pub txn_id: u128,
    pub expected_epoch: u64,
    pub original_latched: bool,
}

/// 磁盘命令（单信用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistCommand {
    PrepareClear {
        txn: ClearTxnIdentity,
    },
    RepairClear {
        txn: ClearTxnIdentity,
        target: RepairTarget,
    },
    RemoveMarker {
        txn: ClearTxnIdentity,
        kind: CleanupKind,
    },
    ResyncRemoval {
        txn: ClearTxnIdentity,
    },
}

impl PersistCommand {
    #[must_use]
    pub const fn txn(&self) -> ClearTxnIdentity {
        match self {
            Self::PrepareClear { txn }
            | Self::RepairClear { txn, .. }
            | Self::RemoveMarker { txn, .. }
            | Self::ResyncRemoval { txn } => *txn,
        }
    }
}

/// `PrepareFailed` 携带的所有权事实，镜像 W0 `PrepareFailureToken`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareTokenKind {
    NotOwned,
    Created,
    Durable,
}

/// `MarkerRemoveFailed` 携带的所有权事实，镜像 W0 `RemoveFailureToken`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveTokenKind {
    Retained,
    Unsynced,
    Lost,
}

/// writer 拒绝执行的原因（不做任何文件动作）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterRefusal {
    /// 命令身份与当前事务不符。
    IdentityMismatch,
    /// `EStop` 持久化义务未清偿，禁止开始清零事务。
    EStopPersistOutstanding,
    /// writer 正在停止。
    ShuttingDown,
    /// 没有当前事务却收到需要事务的命令。
    NoTransaction,
    /// `OriginalUnlatched` 修复要求双份真实 `0/0`，核验结果不符。
    PairMismatch,
    /// 当前令牌状态不允许该命令（例如对 `Created` 直接删除）。
    TokenStateMismatch,
}

/// 磁盘完成（单信用归还）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistCompletion {
    Prepared {
        txn: ClearTxnIdentity,
        at: MonoRawNs,
    },
    PrepareFailed {
        txn: ClearTxnIdentity,
        stage: ClearTxnStage,
        phase: StepPhase,
        token: PrepareTokenKind,
    },
    Repaired {
        txn: ClearTxnIdentity,
        target: RepairTarget,
        at: MonoRawNs,
    },
    RepairFailed {
        txn: ClearTxnIdentity,
        stage: ClearTxnStage,
        phase: StepPhase,
        credential_kept: bool,
    },
    MarkerRemoved {
        txn: ClearTxnIdentity,
        kind: CleanupKind,
        at: MonoRawNs,
    },
    MarkerRemoveFailed {
        txn: ClearTxnIdentity,
        stage: ClearTxnStage,
        phase: StepPhase,
        token: RemoveTokenKind,
    },
    Resynced {
        txn: ClearTxnIdentity,
    },
    Refused {
        txn: Option<ClearTxnIdentity>,
        reason: WriterRefusal,
    },
}

/// 关闭封口后的单次刷新结果，只由真实 writer 发布。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FlushOutcome {
    Complete = 1,
    ObligationUnsettled = 2,
    TransactionOpen = 3,
}

/// IPC 与 writer 共享的持久化责任与单次刷新观测。
#[derive(Debug)]
pub struct PersistShared {
    pub obligation: EStopObligation,
    pub progress: WriterProgress,
    pub announce: CommandAnnounce,
    flush_requested: AtomicBool,
    flush_result: AtomicU8,
}

impl PersistShared {
    /// 所有相对时间共用同一个 `base`（接线时取 `BootSupervisor::started_at()`）。
    ///
    /// # Errors
    ///
    /// 义务实例 ID 计数耗尽。
    pub fn new(base: MonoRawNs) -> Result<Self, IdExhausted> {
        Ok(Self {
            obligation: EStopObligation::new(base)?,
            progress: WriterProgress::new(base),
            announce: CommandAnnounce::new(base),
            flush_requested: AtomicBool::new(false),
            flush_result: AtomicU8::new(0),
        })
    }

    /// 同一构造路径，义务 ID 计数器可注入（测试用局部计数核耗尽映射）。
    pub(super) fn new_with_obligation_counter(
        base: MonoRawNs,
        counter: &super::obligation::InstanceCounter,
    ) -> Result<Self, IdExhausted> {
        Ok(Self {
            obligation: EStopObligation::new_with_counter(base, counter)?,
            progress: WriterProgress::new(base),
            announce: CommandAnnounce::new(base),
            flush_requested: AtomicBool::new(false),
            flush_result: AtomicU8::new(0),
        })
    }

    /// IPC 的真实输入/回复封口后，main 请求一次刷新；重复请求不刷新结果。
    pub fn request_flush(&self) {
        self.flush_requested.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn flush_outcome(&self) -> Option<FlushOutcome> {
        match self.flush_result.load(Ordering::Acquire) {
            1 => Some(FlushOutcome::Complete),
            2 => Some(FlushOutcome::ObligationUnsettled),
            3 => Some(FlushOutcome::TransactionOpen),
            _ => None,
        }
    }

    pub(super) fn flush_pending(&self) -> bool {
        self.flush_requested.load(Ordering::Acquire)
            && self.flush_result.load(Ordering::Acquire) == 0
    }

    pub(super) fn finish_flush(&self, outcome: FlushOutcome) {
        let _ = self.flush_result.compare_exchange(
            0,
            outcome as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

/// writer 端：接命令、送完成。
#[derive(Debug)]
pub struct PersistEndpoint {
    pub(super) commands: Receiver<PersistCommand>,
    pub(super) completions: Sender<PersistCompletion>,
}

/// `try_dispatch` 失败原因；信用未消耗。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchRefused {
    /// 上一命令尚未完成（单信用）。
    NoCredit,
    /// 公告未能发布（在途 / 溢出）。
    Announce(AnnounceError),
    /// 通道满：公告已撤回。
    Full,
    /// writer 端已关闭：公告已撤回。
    Disconnected,
}

/// IPC 端：单信用发送、完成接收。
///
/// `alive` 只表示**尚未观察到断开**（不是操作系统级存活证明）：`try_dispatch` /
/// `try_complete` 任一处观察到 `Disconnected` 即锁存为 `false`，之后永不回 `true`；
/// 断链**不归还**已消耗的在飞信用。消费方须先 `try_complete` 轮询，再读 `alive()`。
#[derive(Debug)]
pub struct PersistClient {
    commands: Sender<PersistCommand>,
    completions: Receiver<PersistCompletion>,
    credit: bool,
    alive: bool,
    shared: Arc<PersistShared>,
}

impl PersistClient {
    /// 公告先于 `try_send`；失败先撤公告再返回，信用不消耗。
    ///
    /// # Errors
    ///
    /// 见 [`DispatchRefused`]。
    pub fn try_dispatch(
        &mut self,
        command: PersistCommand,
        now: MonoRawNs,
    ) -> Result<(), DispatchRefused> {
        if !self.credit {
            return Err(DispatchRefused::NoCredit);
        }
        self.shared
            .announce
            .offer(now)
            .map_err(DispatchRefused::Announce)?;
        match self.commands.try_send(command) {
            Ok(()) => {
                self.credit = false;
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                self.shared.announce.withdraw();
                Err(DispatchRefused::Full)
            }
            Err(TrySendError::Disconnected(_)) => {
                self.shared.announce.withdraw();
                self.alive = false;
                Err(DispatchRefused::Disconnected)
            }
        }
    }

    /// 非阻塞取一个完成；取到即归还信用。writer 端已断开时先返回缓冲中的完成，
    /// 之后的空轮才观察到 `Disconnected` 并锁存 `alive = false`；断链不归还在飞信用。
    pub fn try_complete(&mut self) -> Option<PersistCompletion> {
        match self.completions.try_recv() {
            Ok(done) => {
                self.credit = true;
                Some(done)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.alive = false;
                None
            }
        }
    }

    /// 尚未观察到 writer 端断开；不是存活证明，读取前须先 `try_complete` 轮询。
    #[must_use]
    pub const fn alive(&self) -> bool {
        self.alive
    }

    #[must_use]
    pub const fn has_credit(&self) -> bool {
        self.credit
    }

    #[must_use]
    pub const fn shared(&self) -> &Arc<PersistShared> {
        &self.shared
    }
}

/// 建立单信用 mailbox：`cmd` / `done` 各 `bounded(1)`。
#[must_use]
pub fn mailbox(shared: Arc<PersistShared>) -> (PersistClient, PersistEndpoint) {
    let (command_tx, command_rx) = bounded(1);
    let (done_tx, done_rx) = bounded(1);
    (
        PersistClient {
            commands: command_tx,
            completions: done_rx,
            credit: true,
            alive: true,
            shared,
        },
        PersistEndpoint {
            commands: command_rx,
            completions: done_tx,
        },
    )
}

/// writer 的停止信号来源（接线时由 `StopFlags` 实现；本包只有测试实现）。
pub trait WriterStop: Send + Sync {
    fn running(&self) -> bool;
}

/// 文件步骤前的测试屏障钩子；在 `FileStep` 戳之后、义务检查之前调用。
pub trait WriterHook: Send {
    fn before_step(&mut self, stage: ClearTxnStage);
}

/// 无屏障。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoHook;

impl WriterHook for NoHook {
    fn before_step(&mut self, _stage: ClearTxnStage) {}
}

/// writer 主循环参数（继承已批准的预算与重试）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterConfig {
    /// 义务写入失败后的退避（分片睡眠，tick 仍推进）。
    pub retry_backoff: DurationNs,
    /// 无命令时的接收超时。
    pub idle_poll: DurationNs,
    /// 完成投递单次 `send_timeout`。
    pub deliver_timeout: DurationNs,
    /// 退避睡眠分片上限。
    pub sleep_slice: DurationNs,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            retry_backoff: DurationNs::from_millis(100),
            idle_poll: DurationNs::from_millis(20),
            deliver_timeout: DurationNs::from_millis(20),
            sleep_slice: DurationNs::from_millis(20),
        }
    }
}

/// writer 线程退出方式（正常返回；panic 由 `join` 的 `Err` 表达）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterExit {
    /// 收到停止信号后退出。
    Stopped,
    /// IPC 端已断开，之后收到停止信号退出。
    StoppedAfterIpcDisconnect,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Arc<PersistShared> {
        Arc::new(PersistShared::new(MonoRawNs(1_000)).unwrap())
    }

    fn txn() -> ClearTxnIdentity {
        ClearTxnIdentity {
            txn_id: 7,
            expected_epoch: 3,
            original_latched: true,
        }
    }

    #[test]
    fn dispatch_offers_before_send_and_single_credit() {
        let shared = shared();
        let (mut client, endpoint) = mailbox(Arc::clone(&shared));
        assert!(client.has_credit());
        client
            .try_dispatch(
                PersistCommand::PrepareClear { txn: txn() },
                MonoRawNs(2_000),
            )
            .unwrap();
        assert!(!client.has_credit());
        assert_eq!(
            shared.announce.offered_at(),
            Some(MonoRawNs(2_000)),
            "公告先于发送且成功不撤"
        );
        assert_eq!(
            client
                .try_dispatch(
                    PersistCommand::ResyncRemoval { txn: txn() },
                    MonoRawNs(2_100)
                )
                .unwrap_err(),
            DispatchRefused::NoCredit
        );
        assert_eq!(client.try_complete(), None);
        let command = endpoint.commands.try_recv().unwrap();
        assert_eq!(command, PersistCommand::PrepareClear { txn: txn() });
        assert_eq!(shared.announce.claim(), Some(MonoRawNs(2_000)));
        endpoint
            .completions
            .try_send(PersistCompletion::Refused {
                txn: Some(txn()),
                reason: WriterRefusal::ShuttingDown,
            })
            .unwrap();
        assert!(client.try_complete().is_some());
        assert!(client.has_credit(), "任何完成归还信用");
    }

    #[test]
    fn full_and_disconnected_withdraw_offer_and_keep_credit() {
        let shared = shared();
        let (mut client, endpoint) = mailbox(Arc::clone(&shared));
        // 让通道满：占用信用后强行再发一次（绕过信用，模拟 Full）。
        client
            .try_dispatch(
                PersistCommand::PrepareClear { txn: txn() },
                MonoRawNs(2_000),
            )
            .unwrap();
        client.credit = true;
        shared.announce.claim();
        assert_eq!(
            client
                .try_dispatch(
                    PersistCommand::PrepareClear { txn: txn() },
                    MonoRawNs(2_100)
                )
                .unwrap_err(),
            DispatchRefused::Full
        );
        assert_eq!(shared.announce.offered_at(), None, "Full 先撤回公告");
        assert!(client.has_credit(), "信用未消耗");
        drop(endpoint);
        assert_eq!(
            client
                .try_dispatch(
                    PersistCommand::PrepareClear { txn: txn() },
                    MonoRawNs(2_200)
                )
                .unwrap_err(),
            DispatchRefused::Disconnected
        );
        assert_eq!(
            shared.announce.offered_at(),
            None,
            "Disconnected 先撤回公告"
        );
        assert!(client.has_credit());
    }

    /// R7：断开只在真实观察到 `Disconnected` 时锁存；缓冲中的完成先返回并归还信用，
    /// 在飞（未完成）信用在断链后不归还。
    #[test]
    fn disconnect_is_latched_after_buffered_completion_and_in_flight_credit_is_not_returned() {
        // 缓冲 Done 后断开：先取到完成（信用归还），下一轮才观察到断开。
        {
            let shared = shared();
            let (mut client, endpoint) = mailbox(Arc::clone(&shared));
            client
                .try_dispatch(
                    PersistCommand::PrepareClear { txn: txn() },
                    MonoRawNs(2_000),
                )
                .unwrap();
            endpoint
                .completions
                .try_send(PersistCompletion::Prepared {
                    txn: txn(),
                    at: MonoRawNs(2_500),
                })
                .unwrap();
            drop(endpoint);
            assert!(client.alive());
            assert!(client.try_complete().is_some(), "先返回缓冲中的完成");
            assert!(client.has_credit());
            assert!(client.alive(), "取到完成的这一轮尚未观察到断开");
            assert_eq!(client.try_complete(), None);
            assert!(!client.alive(), "空 / 断开轮锁存 alive = false");
            assert_eq!(client.try_complete(), None);
            assert!(!client.alive(), "锁存后永不回 true");
        }
        // 在飞信用：命令已发出、未完成即断开 → 信用不归还。
        {
            let shared = shared();
            let (mut client, endpoint) = mailbox(Arc::clone(&shared));
            client
                .try_dispatch(
                    PersistCommand::PrepareClear { txn: txn() },
                    MonoRawNs(2_000),
                )
                .unwrap();
            drop(endpoint);
            assert_eq!(client.try_complete(), None);
            assert!(!client.alive());
            assert!(!client.has_credit(), "断链不归还在飞信用");
            assert_eq!(
                client
                    .try_dispatch(
                        PersistCommand::ResyncRemoval { txn: txn() },
                        MonoRawNs(2_100)
                    )
                    .unwrap_err(),
                DispatchRefused::NoCredit
            );
        }
        // try_dispatch 观察到断开同样锁存。
        {
            let shared = shared();
            let (mut client, endpoint) = mailbox(Arc::clone(&shared));
            drop(endpoint);
            assert!(client.alive());
            assert_eq!(
                client
                    .try_dispatch(
                        PersistCommand::PrepareClear { txn: txn() },
                        MonoRawNs(2_000)
                    )
                    .unwrap_err(),
                DispatchRefused::Disconnected
            );
            assert!(!client.alive());
            assert!(client.has_credit(), "未消耗的信用保留");
        }
    }

    #[test]
    fn announce_in_flight_refuses_dispatch_without_send() {
        let shared = shared();
        let (mut client, endpoint) = mailbox(Arc::clone(&shared));
        shared.announce.offer(MonoRawNs(1_500)).unwrap();
        assert_eq!(
            client
                .try_dispatch(
                    PersistCommand::PrepareClear { txn: txn() },
                    MonoRawNs(2_000)
                )
                .unwrap_err(),
            DispatchRefused::Announce(AnnounceError::AlreadyOffered)
        );
        assert!(endpoint.commands.is_empty(), "公告失败不得发送");
        assert!(client.has_credit());
    }
}
