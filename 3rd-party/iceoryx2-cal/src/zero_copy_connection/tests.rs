// RR R1（robot-system 本地补丁，2026-09-14）：zero_copy_connection 的私有具名测试。
//
// 覆盖：同源 channel 状态转移（纯函数 + 真实原子）、公开哨兵参数的薄兼容边缘、
// 真实 push 回执与 used / borrow 所有权、第三方清理 CAS 与阻塞发送的两种终止、
// 两种清理胜者。所有连接测试对 process_local 与 posix_shared_memory 两个后端各跑一遍。
//
// 运行（仓库根）：
//   cargo test --locked --manifest-path 3rd-party/Cargo.toml --target-dir target/rr-vendor-tests \
//       -p iceoryx2-cal --lib --features std
//
// 许可：与所在 crate 相同（MIT OR Apache-2.0）。

use super::channel_state;
use super::*;
use crate::named_concept::*;
use crate::shared_memory::SegmentId;
use crate::testing::generate_isolated_config;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize};
use core::time::Duration;
use iceoryx2_bb_elementary_traits::testing::abandonable::Abandonable;
use std::time::Instant;

/// remove_port 的仅测试钩子：在清理者完成 ORPHANED CAS 之后、移除端点位之前调用。
/// 线程局部，只影响安装它的线程；生产构建不含此模块。
pub(crate) mod remove_port_hook {
    use core::cell::RefCell;

    thread_local! {
        static HOOK: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
    }

    pub(crate) fn install<F: FnMut() + 'static>(hook: F) {
        HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    }

    pub(crate) fn after_orphan_cas_before_port_removal() {
        let taken = HOOK.with(|slot| slot.borrow_mut().take());
        if let Some(mut hook) = taken {
            hook();
            HOOK.with(|slot| *slot.borrow_mut() = Some(hook));
        }
    }
}

const SAMPLE_SIZE: usize = 64;
const CH0: ChannelId = ChannelId::new(0);
const CH1: ChannelId = ChannelId::new(1);
const TWO_POW_62: u64 = 1u64 << 62;

fn offset(index: usize, segment: u8) -> PointerOffset {
    PointerOffset::from_offset_and_segment_id(index * SAMPLE_SIZE, SegmentId::new(segment))
}

fn state(value: u64) -> ChannelState {
    ChannelState::new(value).expect("合法 ChannelState")
}

fn wait_until<F: Fn() -> bool>(what: &str, condition: F) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "等待超时: {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

// ---------------------------------------------------------------------------
// 同源转移原语：纯函数 + 真实 AtomicU64 适配
// ---------------------------------------------------------------------------
mod channel_state_unit {
    use super::*;
    use channel_state::{
        AtomicState, FinishOutcome, HintOutcome, OrphanOutcome, PrepareError, Prepared, Value,
    };
    use core::cell::{Cell, RefCell};

    fn atomic(value: u64) -> AtomicU64 {
        AtomicU64::new(value)
    }

    fn raw(atomic: &AtomicU64) -> u64 {
        atomic.load(Ordering::Relaxed)
    }

    type Action<'a> = Box<dyn FnOnce(&AtomicU64) + 'a>;

    /// 在第 n 次（1 起）CAS 之前注入一次并发动作（对端的真实 CAS，不布置最终值），用于
    /// 确定性地复现“finish 首 CAS 遇到同一请求的新 hint / 第二 CAS 遇到清理”这类交错；
    /// `cas_calls` 记录被测函数实际执行的 CAS 次数，用来断言走到了哪个分支。
    struct Interposing<'a> {
        inner: &'a AtomicU64,
        actions: RefCell<Vec<Option<Action<'a>>>>,
        cas_calls: Cell<usize>,
    }

    impl<'a> Interposing<'a> {
        fn new(inner: &'a AtomicU64) -> Self {
            Self {
                inner,
                actions: RefCell::new(Vec::new()),
                cas_calls: Cell::new(0),
            }
        }

        fn before_cas(self, nth: usize, action: impl FnOnce(&AtomicU64) + 'a) -> Self {
            assert!(nth >= 1);
            {
                let mut actions = self.actions.borrow_mut();
                if actions.len() < nth {
                    actions.resize_with(nth, || None);
                }
                assert!(actions[nth - 1].is_none(), "第 {nth} 次 CAS 已有注入动作");
                actions[nth - 1] = Some(Box::new(action));
            }
            self
        }

        fn cas_calls(&self) -> usize {
            self.cas_calls.get()
        }
    }

    impl AtomicState for Interposing<'_> {
        fn load(&self, order: Ordering) -> u64 {
            self.inner.load(order)
        }

        fn compare_exchange(
            &self,
            current: u64,
            new: u64,
            success: Ordering,
            failure: Ordering,
        ) -> Result<u64, u64> {
            let nth = self.cas_calls.get() + 1;
            self.cas_calls.set(nth);
            let action = self
                .actions
                .borrow_mut()
                .get_mut(nth - 1)
                .and_then(Option::take);
            if let Some(action) = action {
                action(self.inner);
            }
            self.inner.compare_exchange(current, new, success, failure)
        }
    }

    #[test]
    fn public_channel_state_new_accepts_zero_and_two_pow_62_rejects_next() {
        assert_eq!(ChannelState::new(0).map(|s| s.value()), Ok(0));
        assert_eq!(
            ChannelState::new(TWO_POW_62).map(|s| s.value()),
            Ok(TWO_POW_62)
        );
        assert_eq!(
            ChannelState::new(TWO_POW_62 + 1),
            Err(ChannelStateNewError::ValueOutOfBounds)
        );
        assert_eq!(ChannelState::max_value(), channel_state::MAX_LEGAL_REQUEST);
        assert_eq!(CHANNEL_STATE_CLOSED.value(), channel_state::CLOSED);
        assert_eq!(CHANNEL_STATE_OPEN.value(), 0);
    }

    #[test]
    fn classify_value_orders_sentinels_before_hint_and_request() {
        assert_eq!(channel_state::classify_value(u64::MAX), Value::Closed);
        assert_eq!(channel_state::classify_value(u64::MAX - 1), Value::Orphaned);
        assert_eq!(channel_state::classify_value(0), Value::Open(0));
        assert_eq!(
            channel_state::classify_value(TWO_POW_62),
            Value::Open(TWO_POW_62)
        );
        assert_eq!(
            channel_state::classify_value(TWO_POW_62 + 1),
            Value::Invalid(TWO_POW_62 + 1)
        );
        assert_eq!(
            channel_state::classify_value(channel_state::MAX_LEGAL_HINT),
            Value::Hinted(TWO_POW_62)
        );
        assert_eq!(channel_state::MAX_LEGAL_HINT, 3 * TWO_POW_62);
        assert_eq!(
            channel_state::classify_value(channel_state::HINT_BIT | 7),
            Value::Hinted(7)
        );
        assert_eq!(
            channel_state::classify_value(channel_state::HINT_BIT | (TWO_POW_62 + 1)),
            Value::Invalid(channel_state::HINT_BIT | (TWO_POW_62 + 1))
        );
        // ORPHANED 之下一位：带 hint 位但请求域越界 → Invalid，不会被粗掩码成任何请求。
        assert_eq!(
            channel_state::classify_value(u64::MAX - 2),
            Value::Invalid(u64::MAX - 2)
        );
        assert!(channel_state::is_terminal_value(u64::MAX));
        assert!(channel_state::is_terminal_value(u64::MAX - 1));
        assert!(!channel_state::is_terminal_value(u64::MAX - 2));
    }

    #[test]
    fn strict_request_domain_excludes_two_pow_62_but_normal_domain_keeps_it() {
        assert!(channel_state::is_strict_request(TWO_POW_62 - 1));
        assert!(!channel_state::is_strict_request(TWO_POW_62));
        assert!(channel_state::is_legal_request(TWO_POW_62));
        assert!(!channel_state::is_legal_request(TWO_POW_62 + 1));

        let a = atomic(channel_state::CLOSED);
        assert_eq!(
            channel_state::prepare_strict(&a, TWO_POW_62),
            Err(PrepareError::IllegalRequest)
        );
        assert_eq!(raw(&a), channel_state::CLOSED);
        assert_eq!(channel_state::prepare(&a, TWO_POW_62), Ok(()));
        assert_eq!(raw(&a), TWO_POW_62);

        let b = atomic(channel_state::CLOSED);
        assert_eq!(channel_state::prepare_strict(&b, TWO_POW_62 - 1), Ok(()));
        assert_eq!(raw(&b), TWO_POW_62 - 1);
    }

    #[test]
    fn prepare_only_from_closed_to_legal_request() {
        let a = atomic(channel_state::CLOSED);
        assert_eq!(channel_state::prepare(&a, 7), Ok(()));
        assert_eq!(raw(&a), 7);
        assert_eq!(
            channel_state::prepare(&a, 8),
            Err(PrepareError::NotClosed(Value::Open(7)))
        );
        assert_eq!(raw(&a), 7);

        let orphaned = atomic(channel_state::ORPHANED);
        assert_eq!(
            channel_state::prepare(&orphaned, 7),
            Err(PrepareError::NotClosed(Value::Orphaned))
        );
        assert_eq!(raw(&orphaned), channel_state::ORPHANED);

        let closed = atomic(channel_state::CLOSED);
        assert_eq!(
            channel_state::prepare(&closed, TWO_POW_62 + 1),
            Err(PrepareError::IllegalRequest)
        );
        assert_eq!(raw(&closed), channel_state::CLOSED);
    }

    #[test]
    fn set_hint_only_matches_original_request_and_never_from_terminals() {
        let a = atomic(7);
        assert_eq!(channel_state::set_hint(&a, 7), HintOutcome::Set);
        assert_eq!(raw(&a), channel_state::HINT_BIT | 7);
        assert_eq!(channel_state::set_hint(&a, 7), HintOutcome::AlreadyHinted);
        assert_eq!(
            channel_state::set_hint(&a, 8),
            HintOutcome::WrongGeneration(Value::Hinted(7))
        );
        assert_eq!(raw(&a), channel_state::HINT_BIT | 7);

        let other = atomic(9);
        assert_eq!(
            channel_state::set_hint(&other, 7),
            HintOutcome::WrongGeneration(Value::Open(9))
        );
        assert_eq!(raw(&other), 9);

        let closed = atomic(channel_state::CLOSED);
        assert_eq!(channel_state::set_hint(&closed, 7), HintOutcome::Closed);
        assert_eq!(raw(&closed), channel_state::CLOSED);

        let orphaned = atomic(channel_state::ORPHANED);
        assert_eq!(channel_state::set_hint(&orphaned, 7), HintOutcome::Orphaned);
        assert_eq!(raw(&orphaned), channel_state::ORPHANED);

        assert_eq!(
            channel_state::set_hint(&closed, TWO_POW_62 + 1),
            HintOutcome::IllegalRequest
        );
    }

    #[test]
    fn has_hint_and_has_state_classify_terminals_first() {
        let closed = atomic(channel_state::CLOSED);
        assert!(!channel_state::has_hint(&closed, 7));
        assert!(!channel_state::has_state(&closed, 7));
        let orphaned = atomic(channel_state::ORPHANED);
        assert!(!channel_state::has_hint(&orphaned, 7));
        assert!(!channel_state::has_state(&orphaned, 7));

        let open = atomic(7);
        assert!(channel_state::has_state(&open, 7));
        assert!(!channel_state::has_hint(&open, 7));
        assert!(!channel_state::has_state(&open, 8));

        let hinted = atomic(channel_state::HINT_BIT | 7);
        assert!(channel_state::has_state(&hinted, 7));
        assert!(channel_state::has_hint(&hinted, 7));
        assert!(!channel_state::has_hint(&hinted, 8));
        assert!(!channel_state::has_state(&hinted, 8));
    }

    #[test]
    fn finish_closes_from_request_or_its_hint_only() {
        let open = atomic(7);
        assert_eq!(channel_state::finish(&open, 7), FinishOutcome::Finished);
        assert_eq!(raw(&open), channel_state::CLOSED);

        let hinted = atomic(channel_state::HINT_BIT | 7);
        assert_eq!(
            channel_state::finish(&hinted, 7),
            FinishOutcome::FinishedHinted
        );
        assert_eq!(raw(&hinted), channel_state::CLOSED);

        let other = atomic(8);
        assert_eq!(
            channel_state::finish(&other, 7),
            FinishOutcome::WrongGeneration(Value::Open(8))
        );
        assert_eq!(raw(&other), 8);

        let closed = atomic(channel_state::CLOSED);
        assert_eq!(
            channel_state::finish(&closed, 7),
            FinishOutcome::AlreadyClosed
        );
        assert_eq!(raw(&closed), channel_state::CLOSED);

        let orphaned = atomic(channel_state::ORPHANED);
        assert_eq!(channel_state::finish(&orphaned, 7), FinishOutcome::Orphaned);
        assert_eq!(raw(&orphaned), channel_state::ORPHANED);

        assert_eq!(
            channel_state::finish(&open, TWO_POW_62 + 1),
            FinishOutcome::IllegalRequest
        );
    }

    #[test]
    fn finish_completes_when_hint_arrives_between_cas() {
        // 首个 CAS 之前对端真实设置 hint：finish 必须依据 CAS 返回值继续从 hint 形态关闭。
        let a = atomic(7);
        let sut = Interposing::new(&a).before_cas(1, |inner| {
            assert_eq!(channel_state::set_hint(inner, 7), HintOutcome::Set);
        });
        assert_eq!(
            channel_state::finish(&sut, 7),
            FinishOutcome::FinishedHinted
        );
        assert_eq!(
            sut.cas_calls(),
            2,
            "首 CAS 失败后必须再从 hint 形态 CAS 一次"
        );
        assert_eq!(raw(&a), channel_state::CLOSED);
    }

    #[test]
    fn finish_loses_to_cleanup_and_does_not_reopen() {
        // 首个 CAS 之前清理者真实 CAS 为 ORPHANED：迟到 finish 在首 CAS 失败后即退出，不改值。
        let a = atomic(7);
        let sut = Interposing::new(&a).before_cas(1, |inner| {
            assert_eq!(
                channel_state::mark_orphaned(inner),
                OrphanOutcome::Orphaned(Value::Open(7))
            );
        });
        assert_eq!(channel_state::finish(&sut, 7), FinishOutcome::Orphaned);
        assert_eq!(sut.cas_calls(), 1);
        assert_eq!(raw(&a), channel_state::ORPHANED);

        // 首 CAS 前对端设置 hint，第二 CAS 前清理者真实 CAS 为 ORPHANED：第二个 CAS 实际发生
        // 且失败，finish 同样不得改值。
        let b = atomic(7);
        let sut = Interposing::new(&b)
            .before_cas(1, |inner| {
                assert_eq!(channel_state::set_hint(inner, 7), HintOutcome::Set);
            })
            .before_cas(2, |inner| {
                assert_eq!(
                    channel_state::mark_orphaned(inner),
                    OrphanOutcome::Orphaned(Value::Hinted(7))
                );
            });
        assert_eq!(channel_state::finish(&sut, 7), FinishOutcome::Orphaned);
        assert_eq!(sut.cas_calls(), 2, "必须真实走到第二个 CAS 分支");
        assert_eq!(raw(&b), channel_state::ORPHANED);

        // 首 CAS 前对端设置 hint，第二 CAS 前对端已正常关闭：第二个 CAS 失败，报告 AlreadyClosed。
        let c = atomic(7);
        let sut = Interposing::new(&c)
            .before_cas(1, |inner| {
                assert_eq!(channel_state::set_hint(inner, 7), HintOutcome::Set);
            })
            .before_cas(2, |inner| {
                assert_eq!(
                    channel_state::finish(inner, 7),
                    FinishOutcome::FinishedHinted
                );
            });
        assert_eq!(channel_state::finish(&sut, 7), FinishOutcome::AlreadyClosed);
        assert_eq!(sut.cas_calls(), 2);
        assert_eq!(raw(&c), channel_state::CLOSED);
    }

    #[test]
    fn mark_orphaned_only_changes_open_or_hinted() {
        let open = atomic(7);
        assert_eq!(
            channel_state::mark_orphaned(&open),
            OrphanOutcome::Orphaned(Value::Open(7))
        );
        assert_eq!(raw(&open), channel_state::ORPHANED);
        assert_eq!(
            channel_state::mark_orphaned(&open),
            OrphanOutcome::KeptOrphaned
        );

        let hinted = atomic(channel_state::HINT_BIT | 7);
        assert_eq!(
            channel_state::mark_orphaned(&hinted),
            OrphanOutcome::Orphaned(Value::Hinted(7))
        );
        assert_eq!(raw(&hinted), channel_state::ORPHANED);

        let closed = atomic(channel_state::CLOSED);
        assert_eq!(
            channel_state::mark_orphaned(&closed),
            OrphanOutcome::KeptClosed
        );
        assert_eq!(raw(&closed), channel_state::CLOSED);

        let invalid = atomic(TWO_POW_62 + 1);
        assert_eq!(
            channel_state::mark_orphaned(&invalid),
            OrphanOutcome::KeptInvalid(TWO_POW_62 + 1)
        );
        assert_eq!(raw(&invalid), TWO_POW_62 + 1);
    }

    #[test]
    fn mark_orphaned_retries_after_concurrent_hint_and_yields_to_normal_close() {
        // 首个 CAS 之前对端设置 hint：清理者重读后仍把 hint 形态置为 ORPHANED。
        let a = atomic(7);
        let sut = Interposing::new(&a).before_cas(1, |inner| {
            assert_eq!(channel_state::set_hint(inner, 7), HintOutcome::Set);
        });
        assert_eq!(
            channel_state::mark_orphaned(&sut),
            OrphanOutcome::Orphaned(Value::Hinted(7))
        );
        assert_eq!(sut.cas_calls(), 2, "首 CAS 失败后必须重读并再 CAS 一次");
        assert_eq!(raw(&a), channel_state::ORPHANED);

        // 首个 CAS 之前正常关闭赢：清理者重读到 CLOSED 后退出，不覆盖。
        let b = atomic(7);
        let sut = Interposing::new(&b).before_cas(1, |inner| {
            assert_eq!(channel_state::finish(inner, 7), FinishOutcome::Finished);
        });
        assert_eq!(
            channel_state::mark_orphaned(&sut),
            OrphanOutcome::KeptClosed
        );
        assert_eq!(sut.cas_calls(), 1);
        assert_eq!(raw(&b), channel_state::CLOSED);
    }

    #[test]
    fn terminal_recognizes_closed_and_orphaned_only() {
        assert!(channel_state::terminal(&atomic(channel_state::CLOSED)));
        assert!(channel_state::terminal(&atomic(channel_state::ORPHANED)));
        assert!(!channel_state::terminal(&atomic(0)));
        assert!(!channel_state::terminal(&atomic(
            channel_state::HINT_BIT | 7
        )));
        assert!(!channel_state::terminal(&atomic(TWO_POW_62 + 1)));
    }

    #[test]
    fn classify_prepared_reports_closed_open_unknown() {
        assert_eq!(
            channel_state::classify_prepared(&atomic(channel_state::CLOSED), 7),
            Prepared::Closed
        );
        assert_eq!(
            channel_state::classify_prepared(&atomic(7), 7),
            Prepared::Open
        );
        assert_eq!(
            channel_state::classify_prepared(&atomic(channel_state::HINT_BIT | 7), 7),
            Prepared::Open
        );
        assert_eq!(
            channel_state::classify_prepared(&atomic(8), 7),
            Prepared::Unknown(Value::Open(8))
        );
        assert_eq!(
            channel_state::classify_prepared(&atomic(channel_state::ORPHANED), 7),
            Prepared::Unknown(Value::Orphaned)
        );
        assert_eq!(
            channel_state::classify_prepared(&atomic(TWO_POW_62 + 1), 7),
            Prepared::Unknown(Value::Invalid(TWO_POW_62 + 1))
        );
    }
}

// ---------------------------------------------------------------------------
// 真实连接：两个后端
// ---------------------------------------------------------------------------
struct Params {
    buffer_size: usize,
    overflow: bool,
    max_borrow: usize,
    samples: usize,
    channels: usize,
    segments: u8,
    initial: ChannelState,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            buffer_size: 1,
            overflow: false,
            max_borrow: 1,
            samples: 8,
            channels: 1,
            segments: 1,
            initial: CHANNEL_STATE_OPEN,
        }
    }
}

fn builder<C: ZeroCopyConnection>(
    name: &FileName,
    cfg: &C::Configuration,
    params: &Params,
) -> C::Builder {
    <C::Builder as NamedConceptBuilder<C>>::new(name)
        .config(cfg)
        .buffer_size(params.buffer_size)
        .enable_safe_overflow(params.overflow)
        .receiver_max_borrowed_samples_per_channel(params.max_borrow)
        .number_of_samples_per_segment(params.samples)
        .number_of_channels(params.channels)
        .max_supported_shared_memory_segments(params.segments)
        .initial_channel_state(params.initial)
}

fn pair<C: ZeroCopyConnection>(
    name: &FileName,
    cfg: &C::Configuration,
    params: &Params,
) -> (C::Sender, C::Receiver) {
    let sender = builder::<C>(name, cfg, params)
        .create_sender()
        .expect("创建 sender");
    let receiver = builder::<C>(name, cfg, params)
        .create_receiver()
        .expect("创建 receiver");
    (sender, receiver)
}

fn isolated<C: ZeroCopyConnection>(test: &str) -> (C::Configuration, FileName) {
    let cfg = generate_isolated_config::<C>();
    let name = FileName::new(format!("rr_r1_{test}").as_bytes()).expect("合法名字");
    (cfg, name)
}

/// 有异常路径也会置位的 RAII 收尾：断言失败 / 超时 panic 时同样把标志置位，
/// 让等待 `resume` 的清理线程或轮询 `abort` 的阻塞发送方有界退出，scope 的自动 join 不会永挂。
struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// 阻塞发送方的背压回调：正常 Retry；测试异常路径置 `abort` 后改为 DiscardPointerOffsetAndFail，
/// 让 blocking_send 以 UnableToDeliver 有界返回。回调每轮都会被调用，`retries` 用作真实握手。
fn retry_until_abort<'a>(
    retries: &'a AtomicUsize,
    abort: &'a AtomicBool,
) -> impl Fn(u64, Duration) -> BackpressureToReceiverAction + 'a {
    move |_, _| {
        retries.fetch_add(1, Ordering::Relaxed);
        if abort.load(Ordering::Acquire) {
            BackpressureToReceiverAction::DiscardPointerOffsetAndFail
        } else {
            BackpressureToReceiverAction::Retry
        }
    }
}

/// 真实 used 集合的终点检查（`acquire_used_offsets` 是破坏性操作，会清空登记）：
/// 只在所有借用已真实释放或 receiver 已 drop / 废弃、且之后不再发送时调用。
/// 活借用期间只能用 `test_only_is_used` 只读查询。
fn used_offsets<S: ZeroCopySender>(sender: &S) -> Vec<PointerOffset> {
    let mut used = Vec::new();
    unsafe { sender.acquire_used_offsets(|ptr| used.push(ptr)) };
    used
}

fn raw_state<P: ZeroCopyPortDetails>(port: &P, channel: ChannelId) -> u64 {
    port.__internal_get_channel_state(channel)
        .load(Ordering::Relaxed)
}

macro_rules! backend_tests {
    ($module:ident, $connection:ty) => {
        mod $module {
            use super::*;
            use channel_state::{Prepared, Value};

            type Sut = $connection;

            fn enqueued(reclaimed: Option<PointerOffset>) -> TrySendReceipt {
                TrySendReceipt::Enqueued {
                    reclaimed_overflow: reclaimed,
                    issue: None,
                }
            }

            #[test]
            fn try_send_observed_nonoverflow_full_is_not_enqueued_and_used_unchanged() {
                let (cfg, name) = isolated::<Sut>("nonoverflow_full");
                let (sender, receiver) = pair::<Sut>(&name, &cfg, &Params::default());
                let a = offset(0, 0);
                let b = offset(1, 0);

                assert_eq!(sender.try_send_observed(a, SAMPLE_SIZE, CH0), enqueued(None));
                assert_eq!(
                    sender.try_send_observed(b, SAMPLE_SIZE, CH0),
                    TrySendReceipt::NotEnqueued(ZeroCopySendError::ReceiveBufferFull)
                );

                // 接收方只看到 A；B 从未进 submission queue，也从未登记 used。
                assert!(sender.test_only_is_used(a, CH0));
                assert!(!sender.test_only_is_used(b, CH0));
                assert_eq!(receiver.receive(CH0).unwrap(), Some(a));
                assert!(!receiver.has_data(CH0));
                receiver.release(a, CH0).unwrap();
                assert_eq!(receiver.receive(CH0).unwrap(), None);
                assert_eq!(sender.reclaim(CH0).unwrap(), Some(a));
                assert_eq!(sender.reclaim(CH0).unwrap(), None);
                // used 集合：A 已真实回收，B 从未登记。
                assert_eq!(used_offsets(&sender), Vec::<PointerOffset>::new());
            }

            #[test]
            fn try_send_observed_empty_slot_is_enqueued_without_reclaim() {
                let (cfg, name) = isolated::<Sut>("empty_slot");
                let params = Params {
                    buffer_size: 2,
                    ..Params::default()
                };
                let (sender, receiver) = pair::<Sut>(&name, &cfg, &params);
                let a = offset(0, 0);
                let b = offset(1, 0);
                assert_eq!(sender.try_send_observed(a, SAMPLE_SIZE, CH0), enqueued(None));
                assert_eq!(sender.try_send_observed(b, SAMPLE_SIZE, CH0), enqueued(None));
                assert!(receiver.has_data(CH0));
                assert_eq!(receiver.receive(CH0).unwrap(), Some(a));
                receiver.release(a, CH0).unwrap();
                assert_eq!(receiver.receive(CH0).unwrap(), Some(b));
                receiver.release(b, CH0).unwrap();
                assert_eq!(sender.reclaim(CH0).unwrap(), Some(a));
                assert_eq!(sender.reclaim(CH0).unwrap(), Some(b));
                assert_eq!(used_offsets(&sender), Vec::<PointerOffset>::new());
            }

            #[test]
            fn try_send_observed_overflow_reclaims_only_when_used_remove_succeeds() {
                let (cfg, name) = isolated::<Sut>("overflow_reclaim");
                let params = Params {
                    overflow: true,
                    ..Params::default()
                };
                let (sender, receiver) = pair::<Sut>(&name, &cfg, &params);
                let a = offset(0, 0);
                let b = offset(1, 0);

                assert_eq!(sender.try_send_observed(a, SAMPLE_SIZE, CH0), enqueued(None));
                // 溢出：真实收到被挤出的 A 且其 used 登记移除成功 → 列为回收。
                assert_eq!(
                    sender.try_send_observed(b, SAMPLE_SIZE, CH0),
                    enqueued(Some(a))
                );
                // 活借用期间只读核 used：A 已回收、B 仍登记。
                assert!(!sender.test_only_is_used(a, CH0));
                assert!(sender.test_only_is_used(b, CH0));
                assert_eq!(receiver.receive(CH0).unwrap(), Some(b));
                assert!(!receiver.has_data(CH0));
                // 真实释放借用后再做终点检查：used 只剩 B（尚未 reclaim）。
                receiver.release(b, CH0).unwrap();
                assert_eq!(used_offsets(&sender), vec![b]);
            }

            #[test]
            fn try_send_observed_post_push_used_removal_failure_is_enqueued_with_issue() {
                let (cfg, name) = isolated::<Sut>("post_push_issue");
                let params = Params {
                    overflow: true,
                    ..Params::default()
                };
                let (sender, receiver) = pair::<Sut>(&name, &cfg, &params);
                let a = offset(0, 0);
                let b = offset(1, 0);

                assert_eq!(sender.try_send_observed(a, SAMPLE_SIZE, CH0), enqueued(None));
                // 仅测试：真实移除 A 的 used 登记，让 post-push 的旧件回收真实失败。
                assert!(sender.test_only_forget_used_offset(a, CH0));

                assert_eq!(
                    sender.try_send_observed(b, SAMPLE_SIZE, CH0),
                    TrySendReceipt::Enqueued {
                        reclaimed_overflow: None,
                        issue: Some(EnqueueIssue::OverflowedOffsetNotInUsedList(a)),
                    }
                );
                // 新 offset 已发布并可被接收；新 used 登记保留；旧件不冒退款。
                assert!(!sender.test_only_is_used(a, CH0));
                assert!(sender.test_only_is_used(b, CH0));
                assert_eq!(receiver.receive(CH0).unwrap(), Some(b));
                assert!(!receiver.has_data(CH0));
                assert!(sender.test_only_is_used(b, CH0));
                receiver.release(b, CH0).unwrap();
                assert_eq!(used_offsets(&sender), vec![b]);
            }

            #[test]
            fn try_send_projection_preserves_original_results() {
                // 非溢出：Ok(None) 后满队列 Err(ReceiveBufferFull)，且未登记 B。
                let (cfg, name) = isolated::<Sut>("projection_nonoverflow");
                let (sender, receiver) = pair::<Sut>(&name, &cfg, &Params::default());
                let a = offset(0, 0);
                let b = offset(1, 0);
                assert_eq!(sender.try_send(a, SAMPLE_SIZE, CH0), Ok(None));
                assert_eq!(
                    sender.try_send(b, SAMPLE_SIZE, CH0),
                    Err(ZeroCopySendError::ReceiveBufferFull)
                );
                assert_eq!(receiver.receive(CH0).unwrap(), Some(a));
                assert!(!receiver.has_data(CH0));
                assert!(sender.test_only_is_used(a, CH0));
                assert!(!sender.test_only_is_used(b, CH0));
                drop(receiver);
                assert_eq!(used_offsets(&sender), vec![a]);
                drop(sender);

                // 溢出：Ok(Some(旧件))；post-push 旧 used 缺失时 Err(ConnectionCorrupted)，
                // 但新 offset 仍已投递（原行为）。
                let (cfg, name) = isolated::<Sut>("projection_overflow");
                let params = Params {
                    overflow: true,
                    ..Params::default()
                };
                let (sender, receiver) = pair::<Sut>(&name, &cfg, &params);
                let c = offset(2, 0);
                assert_eq!(sender.try_send(a, SAMPLE_SIZE, CH0), Ok(None));
                assert_eq!(sender.try_send(b, SAMPLE_SIZE, CH0), Ok(Some(a)));
                assert!(sender.test_only_forget_used_offset(b, CH0));
                assert_eq!(
                    sender.try_send(c, SAMPLE_SIZE, CH0),
                    Err(ZeroCopySendError::ConnectionCorrupted)
                );
                assert!(sender.test_only_is_used(c, CH0));
                assert!(!sender.test_only_is_used(b, CH0));
                assert_eq!(receiver.receive(CH0).unwrap(), Some(c));
                assert!(!receiver.has_data(CH0));
                receiver.release(c, CH0).unwrap();
                assert_eq!(used_offsets(&sender), vec![c]);
            }

            #[test]
            fn receive_release_reclaim_move_borrow_and_used_exactly_once_across_channels_and_segments()
             {
                let (cfg, name) = isolated::<Sut>("ownership");
                let params = Params {
                    buffer_size: 2,
                    max_borrow: 2,
                    channels: 2,
                    segments: 2,
                    ..Params::default()
                };
                let (sender, receiver) = pair::<Sut>(&name, &cfg, &params);
                let a = offset(0, 0);
                let b = offset(1, 1);

                assert_eq!(sender.try_send_observed(a, SAMPLE_SIZE, CH0), enqueued(None));
                assert_eq!(sender.try_send_observed(b, SAMPLE_SIZE, CH1), enqueued(None));

                assert_eq!(receiver.receive(CH0).unwrap(), Some(a));
                assert_eq!(receiver.borrow_count(CH0), 1);
                assert_eq!(receiver.borrow_count(CH1), 0);
                assert_eq!(receiver.receive(CH1).unwrap(), Some(b));
                assert_eq!(receiver.borrow_count(CH1), 1);
                assert_eq!(receiver.receive(CH0).unwrap(), None);
                assert_eq!(receiver.receive(CH1).unwrap(), None);

                // release 真入 completion queue 才减 borrow；reclaim 真移 used 一次，不串 channel。
                receiver.release(a, CH0).unwrap();
                assert_eq!(receiver.borrow_count(CH0), 0);
                assert_eq!(receiver.borrow_count(CH1), 1);
                assert_eq!(sender.reclaim(CH1).unwrap(), None);
                assert_eq!(sender.reclaim(CH0).unwrap(), Some(a));
                assert_eq!(sender.reclaim(CH0).unwrap(), None);

                receiver.release(b, CH1).unwrap();
                assert_eq!(receiver.borrow_count(CH1), 0);
                assert_eq!(sender.reclaim(CH1).unwrap(), Some(b));
                assert_eq!(sender.reclaim(CH1).unwrap(), None);
                assert_eq!(sender.reclaim(CH0).unwrap(), None);

                assert_eq!(used_offsets(&sender), Vec::<PointerOffset>::new());
            }

            #[test]
            fn release_failure_when_completion_queue_full_keeps_borrow() {
                // buffer 1 + max_borrow 1 → completion queue 容量 3。
                let (cfg, name) = isolated::<Sut>("completion_full");
                let (sender, receiver) = pair::<Sut>(&name, &cfg, &Params::default());
                let samples: Vec<PointerOffset> = (0..4).map(|i| offset(i, 0)).collect();

                for ptr in &samples[..3] {
                    assert_eq!(sender.try_send_observed(*ptr, SAMPLE_SIZE, CH0), enqueued(None));
                    assert_eq!(receiver.receive(CH0).unwrap(), Some(*ptr));
                    assert_eq!(receiver.borrow_count(CH0), 1);
                    receiver.release(*ptr, CH0).unwrap();
                    assert_eq!(receiver.borrow_count(CH0), 0);
                }

                let last = samples[3];
                assert_eq!(sender.try_send_observed(last, SAMPLE_SIZE, CH0), enqueued(None));
                assert_eq!(receiver.receive(CH0).unwrap(), Some(last));
                assert_eq!(receiver.borrow_count(CH0), 1);
                // completion queue 满：release 失败，borrow 不降，不拿对象析构当归还。
                assert_eq!(
                    receiver.release(last, CH0),
                    Err(ZeroCopyReleaseError::RetrieveBufferFull)
                );
                assert_eq!(receiver.borrow_count(CH0), 1);
                assert_eq!(
                    receiver.receive(CH0),
                    Err(ZeroCopyReceiveError::ReceiveWouldExceedMaxBorrowValue)
                );

                // 发送方真实回收三个后，release 才成功。
                for ptr in &samples[..3] {
                    assert_eq!(sender.reclaim(CH0).unwrap(), Some(*ptr));
                }
                receiver.release(last, CH0).unwrap();
                assert_eq!(receiver.borrow_count(CH0), 0);
                assert_eq!(sender.reclaim(CH0).unwrap(), Some(last));
                assert_eq!(sender.reclaim(CH0).unwrap(), None);
                assert_eq!(used_offsets(&sender), Vec::<PointerOffset>::new());
            }

            #[test]
            fn blocking_send_full_nonoverflow_waits_for_live_receiver_and_resumes_after_consumption()
             {
                let (cfg, name) = isolated::<Sut>("live_backpressure");
                let (sender, receiver) = pair::<Sut>(&name, &cfg, &Params::default());
                let a = offset(0, 0);
                let b = offset(1, 0);
                assert_eq!(sender.try_send(a, SAMPLE_SIZE, CH0), Ok(None));

                let finished = AtomicBool::new(false);
                let retries = AtomicUsize::new(0);
                let abort = AtomicBool::new(false);
                let result = std::thread::scope(|scope| {
                    // 异常路径：置 abort 让阻塞发送方有界退出，scope 的自动 join 不会永挂。
                    let abort_on_drop = SetOnDrop(&abort);
                    let worker = scope.spawn(|| {
                        let result = sender.blocking_send(
                            b,
                            SAMPLE_SIZE,
                            CH0,
                            retry_until_abort(&retries, &abort),
                            BackpressureToReceiverAction::Retry,
                        );
                        finished.store(true, Ordering::Release);
                        result
                    });
                    // 真实握手：背压回调至少被调用一次 = 发送方确实在等待活 receiver。
                    wait_until("背压回调被调用", || retries.load(Ordering::Relaxed) > 0);
                    assert!(!finished.load(Ordering::Acquire));
                    // 真实消费后继续投递。
                    assert_eq!(receiver.receive(CH0).unwrap(), Some(a));
                    receiver.release(a, CH0).unwrap();
                    wait_until("发送方在消费后返回", || finished.load(Ordering::Acquire));
                    let result = worker.join().expect("发送线程");
                    drop(abort_on_drop);
                    result
                });
                assert_eq!(result, Ok(None));
                assert_eq!(receiver.receive(CH0).unwrap(), Some(b));
                receiver.release(b, CH0).unwrap();
                assert_eq!(sender.reclaim(CH0).unwrap(), Some(a));
                assert_eq!(sender.reclaim(CH0).unwrap(), Some(b));
            }

            #[test]
            fn blocking_send_discard_fail_and_overflow_keep_original_classification() {
                let (cfg, name) = isolated::<Sut>("backpressure_classification");
                let (sender, receiver) = pair::<Sut>(&name, &cfg, &Params::default());
                let a = offset(0, 0);
                let b = offset(1, 0);
                assert_eq!(sender.try_send(a, SAMPLE_SIZE, CH0), Ok(None));

                let calls = AtomicUsize::new(0);
                let discard = |retries: u64, _: Duration| {
                    assert_eq!(retries, 0);
                    calls.fetch_add(1, Ordering::Relaxed);
                    BackpressureToReceiverAction::DiscardPointerOffset
                };
                assert_eq!(
                    sender.blocking_send(
                        b,
                        SAMPLE_SIZE,
                        CH0,
                        discard,
                        BackpressureToReceiverAction::Retry
                    ),
                    Err(ZeroCopySendError::ReceiveBufferFull)
                );
                assert_eq!(calls.load(Ordering::Relaxed), 1);

                assert_eq!(
                    sender.blocking_send(
                        b,
                        SAMPLE_SIZE,
                        CH0,
                        |_, _| BackpressureToReceiverAction::DiscardPointerOffsetAndFail,
                        BackpressureToReceiverAction::Retry
                    ),
                    Err(ZeroCopySendError::UnableToDeliver)
                );
                assert_eq!(
                    sender.blocking_send(
                        b,
                        SAMPLE_SIZE,
                        CH0,
                        |_, _| BackpressureToReceiverAction::FollowBackpressureyStrategy,
                        BackpressureToReceiverAction::DiscardPointerOffset
                    ),
                    Err(ZeroCopySendError::ReceiveBufferFull)
                );
                // B 从未入队。
                assert_eq!(receiver.receive(CH0).unwrap(), Some(a));
                assert!(!receiver.has_data(CH0));
                drop(receiver);
                assert_eq!(used_offsets(&sender), vec![a]);
                drop(sender);

                // 溢出连接：blocking_send 不等待，直接沿原 try_send 溢出语义。
                let (cfg, name) = isolated::<Sut>("backpressure_overflow");
                let params = Params {
                    overflow: true,
                    ..Params::default()
                };
                let (sender, receiver) = pair::<Sut>(&name, &cfg, &params);
                assert_eq!(sender.try_send(a, SAMPLE_SIZE, CH0), Ok(None));
                assert_eq!(
                    sender.blocking_send(
                        b,
                        SAMPLE_SIZE,
                        CH0,
                        |_, _| panic!("溢出连接不应调用背压回调"),
                        BackpressureToReceiverAction::Retry
                    ),
                    Ok(Some(a))
                );
                assert_eq!(receiver.receive(CH0).unwrap(), Some(b));
            }

            #[test]
            fn full_nonoverflow_after_endpoint_removed_reports_no_connected_receiver_first() {
                let (cfg, name) = isolated::<Sut>("endpoint_removed");
                let (sender, receiver) = pair::<Sut>(&name, &cfg, &Params::default());
                let a = offset(0, 0);
                let b = offset(1, 0);
                assert_eq!(sender.try_send(a, SAMPLE_SIZE, CH0), Ok(None));
                assert!(sender.is_connected());

                // 真实废弃 receiver（不再可用）后才允许强制移除。
                receiver.abandon();
                unsafe { Sut::remove_receiver(&name, &cfg) }.expect("移除 receiver");

                assert!(!sender.is_connected());
                assert_eq!(raw_state(&sender, CH0), channel_state::ORPHANED);
                assert!(sender.is_channel_closed(CH0));
                // 端点已清除：断链错误优先于终止错误。
                assert_eq!(
                    sender.blocking_send(
                        b,
                        SAMPLE_SIZE,
                        CH0,
                        |_, _| BackpressureToReceiverAction::Retry,
                        BackpressureToReceiverAction::Retry
                    ),
                    Err(ZeroCopySendError::NoConnectedReceiverAndBufferIsFull)
                );
                assert_eq!(used_offsets(&sender), vec![a]);
            }

            #[test]
            fn orphaned_full_nonoverflow_blocking_send_terminates_before_port_bit_removal() {
                let (cfg, name) = isolated::<Sut>("orphaned_before_bit_removal");
                let (sender, receiver) = pair::<Sut>(&name, &cfg, &Params::default());
                let a = offset(0, 0);
                let b = offset(1, 0);
                assert_eq!(sender.try_send(a, SAMPLE_SIZE, CH0), Ok(None));

                let finished = AtomicBool::new(false);
                let retries = AtomicUsize::new(0);
                let abort = AtomicBool::new(false);
                let paused = Arc::new(AtomicBool::new(false));
                let resume = Arc::new(AtomicBool::new(false));
                let send_result = std::thread::scope(|scope| {
                    // 异常路径：置 abort 让阻塞发送方有界退出。
                    let abort_on_drop = SetOnDrop(&abort);
                    let worker = scope.spawn(|| {
                        let result = sender.blocking_send(
                            b,
                            SAMPLE_SIZE,
                            CH0,
                            retry_until_abort(&retries, &abort),
                            BackpressureToReceiverAction::Retry,
                        );
                        finished.store(true, Ordering::Release);
                        result
                    });
                    // 真实握手：背压回调被调用 = 发送方确实在满队列上等待。
                    wait_until("背压回调被调用", || retries.load(Ordering::Relaxed) > 0);
                    assert!(!finished.load(Ordering::Acquire));

                    // receiver 先真实废弃，不再可用；之后清理者才走 remove_receiver。
                    receiver.abandon();

                    let cleaner = {
                        let paused = Arc::clone(&paused);
                        let resume = Arc::clone(&resume);
                        let cfg = cfg.clone();
                        scope.spawn(move || {
                            // 仅测试钩子：停在 ORPHANED CAS 之后、端点位移除之前。
                            remove_port_hook::install(move || {
                                paused.store(true, Ordering::Release);
                                while !resume.load(Ordering::Acquire) {
                                    std::thread::sleep(Duration::from_millis(1));
                                }
                            });
                            unsafe { Sut::remove_receiver(&name, &cfg) }
                        })
                    };

                    // 异常路径（断言失败 / 超时 panic）也会释放 resume：清理者完成端点位移除后，
                    // 阻塞发送方也因断链退出，scope 的自动 join 不会永挂。
                    let release_cleaner = SetOnDrop(&resume);

                    wait_until("清理者到达 CAS 后窗口", || paused.load(Ordering::Acquire));
                    // 窗口内：端点位仍在，状态已 ORPHANED，阻塞发送有界返回 ChannelIsClosed。
                    assert!(sender.is_connected());
                    assert_eq!(raw_state(&sender, CH0), channel_state::ORPHANED);
                    wait_until("阻塞发送方有界返回", || finished.load(Ordering::Acquire));
                    let send_result = worker.join().expect("发送线程");
                    assert!(sender.is_connected());

                    drop(release_cleaner);
                    cleaner.join().expect("清理线程").expect("移除 receiver");
                    assert!(!sender.is_connected());
                    drop(abort_on_drop);
                    send_result
                });
                assert_eq!(send_result, Err(ZeroCopySendError::ChannelIsClosed));
                // B 未投递：used 只有 A。
                assert_eq!(used_offsets(&sender), vec![a]);
            }

            #[test]
            fn normal_close_wins_cleanup_preserves_closed_and_tail_is_received() {
                for hinted in [false, true] {
                    let (cfg, name) =
                        isolated::<Sut>(if hinted { "normal_wins_hinted" } else { "normal_wins" });
                    let params = Params {
                        buffer_size: 2,
                        initial: CHANNEL_STATE_CLOSED,
                        ..Params::default()
                    };
                    let (sender, receiver) = pair::<Sut>(&name, &cfg, &params);
                    let request = state(7);
                    let tail = offset(0, 0);

                    // 原 prepare：CLOSED → 7。
                    assert!(receiver.set_channel_state(CH0, request));
                    assert!(sender.has_channel_state(CH0, request));
                    if hinted {
                        receiver.set_disconnect_hint(CH0, request);
                        assert!(sender.has_disconnect_hint(CH0, request));
                        assert!(sender.has_channel_state(CH0, request));
                    }

                    // 尾内容先真实入队，再正常关闭（finish Release）。
                    assert_eq!(sender.try_send(tail, SAMPLE_SIZE, CH0), Ok(None));
                    sender.close_channel(CH0, request);
                    assert_eq!(raw_state(&receiver, CH0), channel_state::CLOSED);

                    // 清理者随后到达：正常关闭赢，保 CLOSED。
                    sender.abandon();
                    unsafe { Sut::remove_sender(&name, &cfg) }.expect("移除 sender");
                    assert_eq!(raw_state(&receiver, CH0), channel_state::CLOSED);
                    assert!(receiver.is_channel_closed(CH0));
                    assert!(!receiver.has_channel_state(CH0, request));
                    assert!(!receiver.has_disconnect_hint(CH0, request));
                    assert_eq!(
                        channel_state::classify_prepared(
                            receiver.__internal_get_channel_state(CH0),
                            7
                        ),
                        Prepared::Closed
                    );
                    // Closed 之后按原 key 仍收到尾内容。
                    assert_eq!(receiver.receive(CH0).unwrap(), Some(tail));
                    assert!(!receiver.has_data(CH0));
                }
            }

            #[test]
            fn cleanup_wins_late_finish_hint_and_prepare_remain_unknown() {
                let (cfg, name) = isolated::<Sut>("cleanup_wins");
                let params = Params {
                    initial: CHANNEL_STATE_CLOSED,
                    ..Params::default()
                };
                let (sender, receiver) = pair::<Sut>(&name, &cfg, &params);
                let request = state(7);
                let other = state(8);

                assert!(receiver.set_channel_state(CH0, request));
                receiver.abandon();
                unsafe { Sut::remove_receiver(&name, &cfg) }.expect("移除 receiver");
                assert_eq!(raw_state(&sender, CH0), channel_state::ORPHANED);

                // 迟到的正常关闭 / hint / 新 prepare 都不得重开或升级。
                sender.close_channel(CH0, request);
                assert_eq!(raw_state(&sender, CH0), channel_state::ORPHANED);
                sender.set_disconnect_hint(CH0, request);
                assert_eq!(raw_state(&sender, CH0), channel_state::ORPHANED);
                assert!(!sender.set_channel_state(CH0, other));
                assert!(!sender.set_channel_state(CH0, request));
                assert_eq!(raw_state(&sender, CH0), channel_state::ORPHANED);

                assert!(!sender.has_channel_state(CH0, request));
                assert!(!sender.has_disconnect_hint(CH0, request));
                assert!(sender.is_channel_closed(CH0));
                assert_eq!(
                    channel_state::classify_prepared(sender.__internal_get_channel_state(CH0), 7),
                    Prepared::Unknown(Value::Orphaned)
                );
            }

            #[test]
            fn strict_prepare_and_status_bridge_follow_same_source_helpers() {
                let (cfg, name) = isolated::<Sut>("strict_bridge");
                let params = Params {
                    buffer_size: 2,
                    initial: CHANNEL_STATE_CLOSED,
                    ..Params::default()
                };
                let (sender, receiver) = pair::<Sut>(&name, &cfg, &params);
                let request = state(11);
                let tail = offset(0, 0);

                // 非法输入：普通上界 2^62 与 CLOSED 哨兵被拒且不触碰状态；查询直接 Unknown。
                assert_eq!(
                    receiver.strict_prepare(CH0, state(TWO_POW_62)),
                    Err(StrictPrepareError::IllegalRequest)
                );
                assert_eq!(
                    receiver.strict_prepare(CH0, CHANNEL_STATE_CLOSED),
                    Err(StrictPrepareError::IllegalRequest)
                );
                assert_eq!(raw_state(&receiver, CH0), channel_state::CLOSED);
                assert_eq!(
                    sender.strict_channel_status(CH0, state(TWO_POW_62)),
                    StrictChannelStatus::Unknown
                );
                assert_eq!(
                    sender.strict_channel_status(CH0, CHANNEL_STATE_CLOSED),
                    StrictChannelStatus::Unknown
                );
                // 通用 set(CLOSED) 的成功 no-op 保留，但它不是 strict prepare 资格。
                assert!(receiver.set_channel_state(CH0, CHANNEL_STATE_CLOSED));
                assert_eq!(raw_state(&receiver, CH0), channel_state::CLOSED);

                // 成功 prepare → Open；重复 / 错代 prepare 失败并报告相对本请求的三态。
                assert_eq!(receiver.strict_prepare(CH0, request), Ok(()));
                assert_eq!(raw_state(&receiver, CH0), 11);
                assert_eq!(
                    sender.strict_channel_status(CH0, request),
                    StrictChannelStatus::Open
                );
                assert_eq!(
                    receiver.strict_prepare(CH0, request),
                    Err(StrictPrepareError::NotClosed(StrictChannelStatus::Open))
                );
                assert_eq!(
                    receiver.strict_prepare(CH0, state(12)),
                    Err(StrictPrepareError::NotClosed(StrictChannelStatus::Unknown))
                );
                assert_eq!(
                    sender.strict_channel_status(CH0, state(12)),
                    StrictChannelStatus::Unknown
                );
                assert_eq!(raw_state(&receiver, CH0), 11);

                // hint 形态仍是 Open。
                receiver.set_disconnect_hint(CH0, request);
                assert_eq!(raw_state(&receiver, CH0), channel_state::HINT_BIT | 11);
                assert_eq!(
                    sender.strict_channel_status(CH0, request),
                    StrictChannelStatus::Open
                );
                assert_eq!(
                    receiver.strict_channel_status(CH0, request),
                    StrictChannelStatus::Open
                );

                // 尾内容先入队再正常关闭 → Closed，尾可收。
                assert_eq!(sender.try_send(tail, SAMPLE_SIZE, CH0), Ok(None));
                sender.close_channel(CH0, request);
                assert_eq!(
                    receiver.strict_channel_status(CH0, request),
                    StrictChannelStatus::Closed
                );
                assert_eq!(receiver.receive(CH0).unwrap(), Some(tail));
                // 复用：新 prepare 成功后，旧请求只能是 Unknown（错代次不升级）。
                receiver.release(tail, CH0).unwrap();
                assert_eq!(receiver.strict_prepare(CH0, state(12)), Ok(()));
                assert_eq!(
                    sender.strict_channel_status(CH0, request),
                    StrictChannelStatus::Unknown
                );
                assert_eq!(
                    sender.strict_channel_status(CH0, state(12)),
                    StrictChannelStatus::Open
                );
            }

            #[test]
            fn strict_status_reports_unknown_after_real_cleanup() {
                let (cfg, name) = isolated::<Sut>("strict_cleanup_unknown");
                let params = Params {
                    initial: CHANNEL_STATE_CLOSED,
                    ..Params::default()
                };
                let (sender, receiver) = pair::<Sut>(&name, &cfg, &params);
                let request = state(21);
                assert_eq!(receiver.strict_prepare(CH0, request), Ok(()));
                assert_eq!(
                    sender.strict_channel_status(CH0, request),
                    StrictChannelStatus::Open
                );

                receiver.abandon();
                unsafe { Sut::remove_receiver(&name, &cfg) }.expect("移除 receiver");
                assert_eq!(raw_state(&sender, CH0), channel_state::ORPHANED);

                // 通用 bool 认为已关闭，但 strict 三态只能是 Unknown，不映射为 Closed。
                assert!(sender.is_channel_closed(CH0));
                assert_eq!(
                    sender.strict_channel_status(CH0, request),
                    StrictChannelStatus::Unknown
                );
                assert_eq!(
                    sender.strict_prepare(CH0, state(22)),
                    Err(StrictPrepareError::NotClosed(StrictChannelStatus::Unknown))
                );
                assert_eq!(raw_state(&sender, CH0), channel_state::ORPHANED);
            }

            #[test]
            fn public_sentinel_compat_edges_are_preserved() {
                let (cfg, name) = isolated::<Sut>("sentinel_compat");
                let params = Params {
                    initial: CHANNEL_STATE_CLOSED,
                    ..Params::default()
                };
                let (sender, receiver) = pair::<Sut>(&name, &cfg, &params);

                // set_channel_state(CLOSED) 在 CLOSED 上仍是成功的 no-op（不是 prepare 资格）。
                assert!(receiver.set_channel_state(CH0, CHANNEL_STATE_CLOSED));
                assert_eq!(raw_state(&receiver, CH0), channel_state::CLOSED);
                // set_disconnect_hint(CLOSED) 不改变值；has_disconnect_hint(CLOSED) 在 CLOSED 上为真；
                // has_channel_state(CLOSED) 恒为假；close_channel(CLOSED) 不改变值。
                receiver.set_disconnect_hint(CH0, CHANNEL_STATE_CLOSED);
                assert_eq!(raw_state(&receiver, CH0), channel_state::CLOSED);
                assert!(sender.has_disconnect_hint(CH0, CHANNEL_STATE_CLOSED));
                assert!(!sender.has_channel_state(CH0, CHANNEL_STATE_CLOSED));
                sender.close_channel(CH0, CHANNEL_STATE_CLOSED);
                assert_eq!(raw_state(&receiver, CH0), channel_state::CLOSED);
                assert!(sender.is_channel_closed(CH0));

                // 真实 prepare 之后，哨兵参数不能改动其它值，也不再成功。
                let request = state(3);
                assert!(receiver.set_channel_state(CH0, request));
                assert!(!receiver.set_channel_state(CH0, CHANNEL_STATE_CLOSED));
                sender.close_channel(CH0, CHANNEL_STATE_CLOSED);
                receiver.set_disconnect_hint(CH0, CHANNEL_STATE_CLOSED);
                assert_eq!(raw_state(&receiver, CH0), 3);
                assert!(!sender.has_channel_state(CH0, CHANNEL_STATE_CLOSED));
                assert!(!sender.has_disconnect_hint(CH0, CHANNEL_STATE_CLOSED));
                assert!(!sender.is_channel_closed(CH0));

                // 合法请求走同源 hint / finish；has_channel_state 含 hint 形态。
                receiver.set_disconnect_hint(CH0, request);
                assert_eq!(raw_state(&receiver, CH0), channel_state::HINT_BIT | 3);
                assert!(sender.has_disconnect_hint(CH0, request));
                assert!(sender.has_channel_state(CH0, request));
                assert!(!sender.has_channel_state(CH0, state(4)));
                sender.close_channel(CH0, state(4));
                assert_eq!(raw_state(&receiver, CH0), channel_state::HINT_BIT | 3);
                sender.close_channel(CH0, request);
                assert_eq!(raw_state(&receiver, CH0), channel_state::CLOSED);
            }
        }
    };
}

backend_tests!(
    process_local,
    crate::zero_copy_connection::process_local::Connection
);
backend_tests!(
    posix_shared_memory,
    crate::zero_copy_connection::posix_shared_memory::Connection
);
