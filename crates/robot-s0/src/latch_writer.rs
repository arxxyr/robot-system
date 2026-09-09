//! S48 A3e / W2a：唯一磁盘 writer 的纯模块——义务字、进度戳、命令公告、mailbox 协议、
//! writer 本体、受检配对与运行期监督的纯机制。
//!
//! **无生产调用方**：本包只准备 writer 线程本体、协议、责任与监督机制，不 spawn 真实
//! 第八 worker，不修改 daemon / IPC / adapter / authority 生产接线、观测版本或 worker
//! 位图。生产权限迁移、身份退休、ACK、READY 运行期监督、关闭 D / 90、8 worker 与观测、
//! `EStop` / Clear / 退出唯一磁盘 writer 在 W3 统一激活。
//!
//! 依赖已提交的 W0（文件事务凭证）、W1（`LatchTargetLock`）与 W0.1（`inspect_pair`）
//! 实际 API；不复制其算法。
//!
//! | 模块 | 内容 |
//! |---|---|
//! | [`obligation`] | `EStopObligation` 单字 Pending / Active + 连续未清偿起点；`Claim` / `ClaimGuard` 唯一领取、实例绑定 |
//! | [`progress`] | `WriterProgress` 真实阶段戳；`ExitStamp` 最后发布 `Exited` |
//! | [`announce`] | `CommandAnnounce` 公告先于 `try_send`，`claim` / `withdraw` 恰一方成功 |
//! | [`protocol`] | 命令 / 完成类型、单信用 mailbox、`WriterStop` / `WriterHook` |
//! | [`core`] | `WriterGuard`、令牌 × 命令矩阵、义务服务、`run_writer` / `spawn_writer` |
//! | [`pairing`] | 借用真实 W1 锁的受检配对、`WriterJoined` 与借用绑定的 `LastResortToken` |
//! | [`supervision`] | 一次武装的关闭字、四候选最早截止与二次确认 |

pub mod announce;
pub mod core;
pub mod obligation;
pub mod pairing;
pub mod progress;
pub mod protocol;
pub mod supervision;

pub use announce::{AnnounceError, CommandAnnounce};
pub use core::{
    ObligationServiced, StepEnv, WriterCore, WriterGuard, WriterHandle, WriterToken,
    WriterTokenKind, WriterTxn, run_writer, spawn_writer, writes_latch_value,
};
pub use obligation::{
    Claim, ClaimGuard, ClaimMismatch, ClaimRefused, EStopObligation, IdExhausted, ObligationId,
    ObligationView, RequestOutcome, Settle,
};
pub use pairing::{
    LastResortError, LastResortToken, PairingRefused, StandbyId, StandbyStore, TargetKey,
    TokenRefused, WriterId, WriterJoined, WriterPairing, WriterSlot,
};
pub use progress::{
    ExitStamp, Phase, ProgressDecodeError, ProgressOverflow, ProgressView, WriterProgress,
};
pub use protocol::{
    ClearTxnIdentity, DispatchRefused, NoHook, PersistClient, PersistCommand, PersistCompletion,
    PersistEndpoint, PersistShared, PrepareTokenKind, RemoveTokenKind, WriterConfig, WriterExit,
    WriterHook, WriterRefusal, WriterStop, mailbox,
};
pub use supervision::{
    Budgets, Candidate, CandidateKind, Decision, Inputs, RuntimeWords, ShutdownTicket, arm,
    confirm, evaluate, obligation_overdue,
};
