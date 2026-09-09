//! 受检配对：借用真实 W1 锁核对 worker / standby 两个 store 的目录身份与规范 primary
//! 目标，产出 writer 槽位与 standby；兜底写入只能经真实 `join` 凭证换得的借用绑定、
//! 按值一次的 [`LastResortToken`]。
//!
//! 调用方（W3 的 daemon main）必须在 worker / standby 整个生命周期持锁至 process exit；
//! 配对只借用锁核对一次，不延长锁寿命。任何 [`PairingRefused`] / [`TokenRefused`]
//! 都不触碰义务字：错配不清责任。

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use robot_core::time::MonoRawNs;

use super::obligation::{IdExhausted, InstanceCounter};
use super::protocol::PersistShared;
use crate::clear_estop_transaction::{ClearTxnStore, FileIdentity, StageError, StepGuard};
use crate::latch_lock::{LatchObject, LatchTargetLock};

static WRITER_IDS: InstanceCounter = InstanceCounter::new();
static STANDBY_IDS: InstanceCounter = InstanceCounter::new();

/// 配对所用的三个实例计数器；生产只用全局静态，测试注入局部计数核耗尽映射。
pub(super) struct IdCounters<'a> {
    pub(super) writer: &'a InstanceCounter,
    pub(super) standby: &'a InstanceCounter,
    pub(super) obligation: &'a InstanceCounter,
}

impl IdCounters<'static> {
    fn global() -> Self {
        Self {
            writer: &WRITER_IDS,
            standby: &STANDBY_IDS,
            obligation: super::obligation::global_obligation_ids(),
        }
    }
}

/// writer 实例 ID；不回绕、不复用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WriterId(NonZeroU64);

impl WriterId {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// standby 实例 ID；不回绕、不复用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StandbyId(NonZeroU64);

impl StandbyId {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// 目标键：绑定目录身份 + 规范 primary 路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetKey {
    directory: FileIdentity,
    primary: PathBuf,
}

impl TargetKey {
    #[must_use]
    pub const fn directory(&self) -> FileIdentity {
        self.directory
    }

    #[must_use]
    pub fn primary(&self) -> &Path {
        &self.primary
    }
}

/// 配对被拒绝：不触碰义务字、不写文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PairingRefused {
    /// 锁绑定的目录身份与某个 store 的目录身份不同。
    #[error("锁目录身份与 store 目录身份不符")]
    LockDirectoryMismatch,
    /// 锁的规范 primary 目标与 worker store 的 latch 路径不同（同目录另一 basename 也算）。
    #[error("锁目标与 worker store 目标不符")]
    LockTargetMismatch,
    /// standby store 的目标与 worker store 不同。
    #[error("standby store 目标与 worker store 不符")]
    StoreTargetMismatch,
    /// writer / standby 实例 ID 或义务 ID 计数耗尽。
    #[error("实例 ID 计数耗尽")]
    IdExhausted,
}

impl From<IdExhausted> for PairingRefused {
    fn from(IdExhausted: IdExhausted) -> Self {
        Self::IdExhausted
    }
}

/// writer 槽位：worker store、共享字与实例 ID；只由 `spawn_writer` / `run_writer` 消费。
#[derive(Debug)]
pub struct WriterSlot {
    pub(super) store: ClearTxnStore,
    pub(super) shared: Arc<PersistShared>,
    pub(super) writer: WriterId,
    pub(super) target: TargetKey,
}

impl WriterSlot {
    #[must_use]
    pub const fn writer(&self) -> WriterId {
        self.writer
    }

    #[must_use]
    pub const fn shared(&self) -> &Arc<PersistShared> {
        &self.shared
    }

    #[must_use]
    pub const fn target(&self) -> &TargetKey {
        &self.target
    }

    #[must_use]
    pub const fn store(&self) -> &ClearTxnStore {
        &self.store
    }
}

/// 真实 `join` 的凭证：只由该槽位的 `WriterHandle::join` 产生（含 panic `Err`）。
///
/// ```compile_fail,E0451
/// use robot_s0::latch_writer::pairing::{StandbyStore, WriterJoined};
/// fn forge(standby: &mut StandbyStore, writer: robot_s0::latch_writer::pairing::WriterId) {
///     let joined = WriterJoined { writer, target: todo!() };
///     let _ = standby.after_join(joined);
/// }
/// ```
#[derive(Debug)]
pub struct WriterJoined {
    pub(super) writer: WriterId,
    pub(super) target: TargetKey,
}

impl WriterJoined {
    #[must_use]
    pub const fn writer(&self) -> WriterId {
        self.writer
    }

    #[must_use]
    pub const fn target(&self) -> &TargetKey {
        &self.target
    }
}

/// standby store：正常运行绝不经它写入；只有 [`LastResortToken`] 存在时才能写一次。
#[derive(Debug)]
pub struct StandbyStore {
    store: ClearTxnStore,
    id: StandbyId,
    bound_writer: WriterId,
    target: TargetKey,
}

/// 兜底令牌被拒绝：零守门调用、零文件操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenRefused {
    /// `WriterJoined` 不属于本 standby 绑定的 writer。
    #[error("join 凭证属于别的 writer")]
    WriterMismatch,
    /// `WriterJoined` 的目标与本 standby 不同。
    #[error("join 凭证目标与 standby 不符")]
    TargetMismatch,
    /// 写前再核：令牌与 standby 实例不符。
    #[error("令牌与 standby 实例不符")]
    StandbyMismatch,
}

/// 兜底写入失败：配对拒绝与真实文件阶段错误分开，不扁平化。
#[derive(Debug, thiserror::Error)]
pub enum LastResortError {
    #[error("兜底写入被拒绝: {0}")]
    Refused(#[source] TokenRefused),
    #[error("兜底写入文件步骤失败: {0}")]
    Stage(#[source] StageError),
}

/// 借用绑定其 standby、按值一次的兜底令牌；类型上无其它写路径。
///
/// 合法 tokenA 不能交给 standbyB：令牌可变借用其 standby，交换 / 再借用都是编译错误。
///
/// ```compile_fail,E0499
/// use robot_s0::latch_writer::pairing::{StandbyStore, WriterJoined};
/// use robot_s0::clear_estop_transaction::NoGuard;
/// fn misuse(a: &mut StandbyStore, b: &mut StandbyStore, joined: WriterJoined) {
///     let token = a.after_join(joined).unwrap();
///     std::mem::swap(a, b);
///     let _ = token.persist_once(&mut NoGuard);
/// }
/// ```
///
/// 私有字段：不能绕过 `after_join` 构造。
///
/// ```compile_fail,E0451
/// use robot_s0::latch_writer::pairing::{LastResortToken, StandbyStore};
/// fn forge(standby: &mut StandbyStore) -> LastResortToken<'_> {
///     LastResortToken { standby, standby_id: todo!(), writer: todo!(), joined: todo!() }
/// }
/// ```
///
/// 按值一次：
///
/// ```compile_fail,E0382
/// use robot_s0::latch_writer::pairing::LastResortToken;
/// use robot_s0::clear_estop_transaction::NoGuard;
/// fn twice(token: LastResortToken<'_>) {
///     let _ = token.persist_once(&mut NoGuard);
///     let _ = token.persist_once(&mut NoGuard);
/// }
/// ```
#[must_use = "令牌只能用于一次兜底写入"]
#[derive(Debug)]
pub struct LastResortToken<'s> {
    standby: &'s mut StandbyStore,
    standby_id: StandbyId,
    writer: WriterId,
    /// 随令牌一起消费的真实 join 凭证；不可再次用于授权。
    joined: WriterJoined,
}

impl StandbyStore {
    #[must_use]
    pub const fn id(&self) -> StandbyId {
        self.id
    }

    #[must_use]
    pub const fn bound_writer(&self) -> WriterId {
        self.bound_writer
    }

    #[must_use]
    pub const fn target(&self) -> &TargetKey {
        &self.target
    }

    #[must_use]
    pub const fn store(&self) -> &ClearTxnStore {
        &self.store
    }

    /// 最终使用点绑定：只有本 standby 绑定 writer 的真实 join 凭证才换得令牌。
    ///
    /// # Errors
    ///
    /// 见 [`TokenRefused`]；拒绝不触碰义务字与文件。
    pub fn after_join(
        &mut self,
        joined: WriterJoined,
    ) -> Result<LastResortToken<'_>, TokenRefused> {
        if joined.writer != self.bound_writer {
            return Err(TokenRefused::WriterMismatch);
        }
        if joined.target != self.target {
            return Err(TokenRefused::TargetMismatch);
        }
        Ok(LastResortToken {
            standby_id: self.id,
            writer: joined.writer,
            standby: self,
            joined,
        })
    }

    /// 仅内部测试：构造与本 standby 实例不符的令牌，核写前拒绝为零守门 / 零文件操作。
    #[cfg(test)]
    pub(super) fn forge_mismatched_token(&mut self) -> LastResortToken<'_> {
        let standby_id = StandbyId(STANDBY_IDS.next().expect("测试计数"));
        let writer = WriterId(WRITER_IDS.next().expect("测试计数"));
        let joined = WriterJoined {
            writer,
            target: self.target.clone(),
        };
        LastResortToken {
            standby: self,
            standby_id,
            writer,
            joined,
        }
    }
}

impl LastResortToken<'_> {
    /// 唯一写路径：写前再次核令牌与 standby 实例（零守门 / 零文件操作即拒绝），随后
    /// 经 standby store 严格写 `1` / `1`。
    ///
    /// # Errors
    ///
    /// 见 [`LastResortError`]：拒绝与真实阶段错误（保留 stage / phase）分开。
    pub fn persist_once(self, guard: &mut dyn StepGuard) -> Result<(), LastResortError> {
        if self.standby_id != self.standby.id
            || self.writer != self.standby.bound_writer
            || self.joined.writer != self.standby.bound_writer
        {
            return Err(LastResortError::Refused(TokenRefused::StandbyMismatch));
        }
        self.standby
            .store
            .persist_latched(guard)
            .map_err(LastResortError::Stage)
    }
}

/// 受检配对：借用真实 W1 锁核对目录身份与规范 primary 目标。
#[derive(Debug)]
pub struct WriterPairing {
    slot: WriterSlot,
    standby: StandbyStore,
}

impl WriterPairing {
    /// ① 锁目录身份与两个 store 的 `directory_identity()` 相同；② 锁的规范 primary
    /// 路径与 `worker.latch_path()`、`standby.latch_path()` 三者相同。不接受调用者自报的
    /// 身份 / 路径作为锁证明；借用只用于本次核对。
    ///
    /// # Errors
    ///
    /// 见 [`PairingRefused`]；拒绝时两个 store 原样丢弃，不写文件。
    pub fn new(
        worker: ClearTxnStore,
        standby: ClearTxnStore,
        lock: &LatchTargetLock,
        base: MonoRawNs,
    ) -> Result<Self, PairingRefused> {
        Self::new_with_counters(worker, standby, lock, base, &IdCounters::global())
    }

    /// 与 [`WriterPairing::new`] 完全相同的受检路径，只是实例计数器可注入。
    pub(super) fn new_with_counters(
        worker: ClearTxnStore,
        standby: ClearTxnStore,
        lock: &LatchTargetLock,
        base: MonoRawNs,
        counters: &IdCounters<'_>,
    ) -> Result<Self, PairingRefused> {
        let lock_directory = lock.directory_identity();
        if !lock_directory.matches(worker.directory_identity())
            || !lock_directory.matches(standby.directory_identity())
        {
            return Err(PairingRefused::LockDirectoryMismatch);
        }
        let lock_primary = lock.target().path_of(LatchObject::Primary);
        if worker.latch_path() != lock_primary {
            return Err(PairingRefused::LockTargetMismatch);
        }
        if standby.latch_path() != worker.latch_path()
            || standby.directory_identity() != worker.directory_identity()
        {
            return Err(PairingRefused::StoreTargetMismatch);
        }
        let target = TargetKey {
            directory: worker.directory_identity(),
            primary: lock_primary,
        };
        let shared = Arc::new(PersistShared::new_with_obligation_counter(
            base,
            counters.obligation,
        )?);
        let writer = WriterId(counters.writer.next()?);
        let standby_id = StandbyId(counters.standby.next()?);
        Ok(Self {
            slot: WriterSlot {
                store: worker,
                shared,
                writer,
                target: target.clone(),
            },
            standby: StandbyStore {
                store: standby,
                id: standby_id,
                bound_writer: writer,
                target,
            },
        })
    }

    #[must_use]
    pub fn into_parts(self) -> (WriterSlot, StandbyStore) {
        (self.slot, self.standby)
    }

    #[must_use]
    pub const fn slot(&self) -> &WriterSlot {
        &self.slot
    }

    #[must_use]
    pub const fn standby(&self) -> &StandbyStore {
        &self.standby
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clear_estop_transaction::{ClearTxnStage, GuardInterrupt, NoGuard, StepPhase};
    use crate::latch_lock::acquire_latch_lock;
    use tempfile::TempDir;

    const BASE: MonoRawNs = MonoRawNs(1_000);

    fn open(latch: &Path) -> ClearTxnStore {
        ClearTxnStore::open(latch, "xlerobot_left", "left_arm").unwrap()
    }

    fn pair_fixture() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let latch = dir.path().join("estop_latch");
        std::fs::write(&latch, b"0\n").unwrap();
        std::fs::write(dir.path().join("estop_latch.bak"), b"0\n").unwrap();
        (dir, latch)
    }

    /// 在第一步就拒绝的守门：真实阶段 / Before 相原样保留。
    struct RefuseAll;

    impl StepGuard for RefuseAll {
        fn before(&mut self, _stage: ClearTxnStage) -> Result<(), GuardInterrupt> {
            Err(GuardInterrupt::new(
                crate::clear_estop_transaction::InterruptReason::ShuttingDown,
            ))
        }
    }

    struct CountingGuard(usize);

    impl StepGuard for CountingGuard {
        fn before(&mut self, _stage: ClearTxnStage) -> Result<(), GuardInterrupt> {
            self.0 += 1;
            Ok(())
        }
    }

    #[test]
    fn pairing_checks_lock_directory_then_lock_target_then_store_target() {
        let (dir_a, latch_a) = pair_fixture();
        let lock_a = acquire_latch_lock(&latch_a).unwrap();
        let pairing = WriterPairing::new(open(&latch_a), open(&latch_a), &lock_a, BASE).unwrap();
        assert_eq!(pairing.slot().target(), pairing.standby().target());
        assert_eq!(pairing.standby().bound_writer(), pairing.slot().writer());
        assert_eq!(
            pairing.slot().target().primary(),
            latch_a.canonicalize().unwrap()
        );

        // 持同目录 A 的锁配两个 B 的 store：目录身份不符。
        let (_dir_b, latch_b) = pair_fixture();
        assert_eq!(
            WriterPairing::new(open(&latch_b), open(&latch_b), &lock_a, BASE).unwrap_err(),
            PairingRefused::LockDirectoryMismatch
        );
        // 同目录不同合法 basename：各自可独立持锁，但不能冒充同一目标。
        let other = dir_a.path().join("gripper_latch");
        std::fs::write(&other, b"0\n").unwrap();
        let lock_other = acquire_latch_lock(&other).unwrap();
        assert_eq!(
            WriterPairing::new(open(&latch_a), open(&latch_a), &lock_other, BASE).unwrap_err(),
            PairingRefused::LockTargetMismatch
        );
        assert_eq!(
            WriterPairing::new(open(&latch_a), open(&other), &lock_a, BASE).unwrap_err(),
            PairingRefused::StoreTargetMismatch
        );
        assert_eq!(std::fs::read(&latch_a).unwrap(), b"0\n", "拒绝不写文件");
    }

    /// R2：耗尽只在局部计数器上发生，经与生产相同的受检分配路径映射为
    /// `PairingRefused::IdExhausted`（writer / standby / 义务三处各一），不触碰全局静态。
    #[test]
    fn writer_ids_are_unique_and_exhaustion_is_explicit() {
        let (_dir, latch) = pair_fixture();
        let lock = acquire_latch_lock(&latch).unwrap();
        let first = WriterPairing::new(open(&latch), open(&latch), &lock, BASE).unwrap();
        let second = WriterPairing::new(open(&latch), open(&latch), &lock, BASE).unwrap();
        assert_ne!(first.slot().writer(), second.slot().writer());
        assert_ne!(first.standby().id(), second.standby().id());
        assert!(second.slot().writer().get() > first.slot().writer().get());

        for exhausted in ["writer", "standby", "obligation"] {
            let start = |name: &str| {
                InstanceCounter::starting_at(if name == exhausted { u64::MAX } else { 0 })
            };
            let (writer, standby, obligation) =
                (start("writer"), start("standby"), start("obligation"));
            let counters = IdCounters {
                writer: &writer,
                standby: &standby,
                obligation: &obligation,
            };
            let error = WriterPairing::new_with_counters(
                open(&latch),
                open(&latch),
                &lock,
                BASE,
                &counters,
            )
            .unwrap_err();
            assert_eq!(error, PairingRefused::IdExhausted, "{exhausted}");
            let again = WriterPairing::new_with_counters(
                open(&latch),
                open(&latch),
                &lock,
                BASE,
                &counters,
            )
            .unwrap_err();
            assert_eq!(
                again,
                PairingRefused::IdExhausted,
                "{exhausted}: 耗尽后不回绕"
            );
        }
        // 局部耗尽后全局配对仍可用，且 ID 继续单调、不复用。
        let third = WriterPairing::new(open(&latch), open(&latch), &lock, BASE).unwrap();
        assert!(third.slot().writer().get() > second.slot().writer().get());
    }

    /// R2 回归：任何耗尽测试都不得污染生产静态计数——本用例名字排序在耗尽测试之后，
    /// `--test-threads=1` 下紧随其后运行，全局配对必须仍然可用。
    #[test]
    fn writer_ids_z_global_pairing_still_usable_after_exhaustion_test() {
        let (_dir, latch) = pair_fixture();
        let lock = acquire_latch_lock(&latch).unwrap();
        WriterPairing::new(open(&latch), open(&latch), &lock, BASE)
            .expect("耗尽测试不得把生产静态计数永久置为 MAX");
    }

    #[test]
    fn mismatched_token_is_refused_before_any_guard_or_file_operation() {
        let (dir, latch) = pair_fixture();
        let lock = acquire_latch_lock(&latch).unwrap();
        let (slot, mut standby) = WriterPairing::new(open(&latch), open(&latch), &lock, BASE)
            .unwrap()
            .into_parts();
        let obligation = &slot.shared().obligation;
        obligation.request(MonoRawNs(2_000));
        let before = std::fs::read(&latch).unwrap();
        let mut guard = CountingGuard(0);
        let error = standby
            .forge_mismatched_token()
            .persist_once(&mut guard)
            .unwrap_err();
        assert!(matches!(
            error,
            LastResortError::Refused(TokenRefused::StandbyMismatch)
        ));
        assert_eq!(guard.0, 0, "零守门调用");
        assert_eq!(std::fs::read(&latch).unwrap(), before, "零文件操作");
        assert_eq!(
            std::fs::read(dir.path().join("estop_latch.bak")).unwrap(),
            b"0\n"
        );
        assert!(!obligation.is_settled(), "错配不清责任");
    }

    #[test]
    fn real_file_failure_keeps_stage_and_phase() {
        let (_dir, latch) = pair_fixture();
        let lock = acquire_latch_lock(&latch).unwrap();
        let (slot, mut standby) = WriterPairing::new(open(&latch), open(&latch), &lock, BASE)
            .unwrap()
            .into_parts();
        let joined = WriterJoined {
            writer: slot.writer(),
            target: slot.target().clone(),
        };
        // 守门在第一步拒绝：真实阶段 / Before 相原样保留。
        let token = standby.after_join(joined).unwrap();
        let error = token.persist_once(&mut RefuseAll).unwrap_err();
        let LastResortError::Stage(stage) = error else {
            panic!("必须是阶段错误");
        };
        assert_eq!(stage.stage, ClearTxnStage::CreatePrimary);
        assert_eq!(stage.phase, StepPhase::Before);
        // 合法令牌成功写 1/1。
        let joined = WriterJoined {
            writer: slot.writer(),
            target: slot.target().clone(),
        };
        standby
            .after_join(joined)
            .unwrap()
            .persist_once(&mut NoGuard)
            .unwrap();
        assert_eq!(std::fs::read(&latch).unwrap(), b"1\n");
    }
}
