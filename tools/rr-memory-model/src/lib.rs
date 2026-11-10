//! RR R1 同源内存模型（`tools/rr-memory-model`）。
//!
//! - 根工作区的测试成员，包元数据和依赖继承根清单；不进入 S0 / S1 / Gateway 生产依赖图。
//! - 只通过 `#[path]` 包含仓库里同一份
//!   `3rd-party/iceoryx2-cal/src/zero_copy_connection/channel_state.rs`：模型侧仅提供
//!   Loom 原子适配（直接转发同一 Ordering），不复制 CAS / 算法，不加任何更强同步。
//! - 三个正向模型完整探索；两个负对照只更换模型侧适配（削弱 finish 的 Release、削弱
//!   观察者的 Acquire），仍调用同源函数，必须各自找到"Closed 可见但尾不可见"的反例；
//!   一个能力边界测试记录 CAL 原语本身分不清初始 / 复用代次的同值 CLOSED（非零反例）。
//! - Loom 只探索其支持的调度与 C11 可见性子集，不覆盖全部弱内存重排：
//!   Release→Acquire 论证与 x86 / ARM 实测另列；模型 PASS 不等于真实 SHM / ARM 证明，
//!   也不代替 R2 的 IOX 所有权 / 映射 / pin 门与本方 IPC 门。
//! - 非 test 构建为空 lib；全部内容在 `#[cfg(test)]` 下。

#[cfg(test)]
#[allow(dead_code)]
#[allow(
    clippy::doc_markdown,
    reason = "保留同源 CAL 模块原文，仅在模型导入处豁免文档标记检查"
)]
#[path = "../../../3rd-party/iceoryx2-cal/src/zero_copy_connection/channel_state.rs"]
mod channel_state;

#[cfg(test)]
mod models {
    use core::sync::atomic::Ordering;
    use std::fmt::Write as _;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicUsize, Ordering as StdOrdering};

    use loom::sync::Arc;
    use loom::sync::atomic::AtomicU64;
    use loom::thread;

    use super::channel_state::{
        self, CLOSED, FinishOutcome, HintOutcome, ORPHANED, OrphanOutcome, PrepareError, Prepared,
        Value,
    };

    /// 生产侧适配的同构：直接转发同一 Ordering，不加 fence、不改序。
    impl channel_state::AtomicState for AtomicU64 {
        fn load(&self, order: Ordering) -> u64 {
            AtomicU64::load(self, order)
        }

        fn compare_exchange(
            &self,
            current: u64,
            new: u64,
            success: Ordering,
            failure: Ordering,
        ) -> Result<u64, u64> {
            AtomicU64::compare_exchange(self, current, new, success, failure)
        }
    }

    /// 原请求与复用请求（均在 strict 域 0..<2^62 内）。
    const REQ: u64 = 7;
    const REQ2: u64 = 8;
    /// server 在 finish 之前发布的尾内容。
    const TAIL: u64 = 0xA5;
    /// 尾不可见反例的 panic 文本；负对照的 `should_panic(expected)` 与之匹配。
    const TAIL_INVISIBLE: &str = "观察到 Closed 后尾内容不可见";

    /// Builder 参数：含根线程共 4，worker ≤ 3；每次排列最多 4096 次线程切换。
    const MAX_THREADS: usize = 4;
    const MAX_BRANCHES: usize = 4096;
    /// Loom 默认值；没有 `checkpoint_file` 时不生效，仍显式赋值避免继承环境变量。
    const CHECKPOINT_INTERVAL: usize = 20_000;

    /// 显式覆盖 `Builder::new()` 读到的全部环境变量：不继承任何截断，不装 checkpoint。
    fn builder() -> loom::model::Builder {
        let mut builder = loom::model::Builder::new();
        builder.max_threads = MAX_THREADS;
        builder.max_branches = MAX_BRANCHES;
        builder.max_permutations = None;
        builder.max_duration = None;
        builder.preemption_bound = None;
        builder.checkpoint_file = None;
        builder.checkpoint_interval = CHECKPOINT_INTERVAL;
        builder.location = false;
        builder.log = false;
        builder.expect_explicit_explore = false;
        builder
    }

    /// std 原子计数器：只在断言 / join 之后累加（反例计数在断言失败前一行记录），
    /// 从不作为模型线程的条件或同步。
    #[derive(Default)]
    struct Counter(AtomicUsize);

    impl Counter {
        fn bump(&self) {
            self.0.fetch_add(1, StdOrdering::Relaxed);
        }

        fn get(&self) -> usize {
            self.0.load(StdOrdering::Relaxed)
        }
    }

    /// 打印一行可 grep 的报告；`fields` 为 (名字, 值) 按序输出。
    fn report(model: &str, kind: &str, fields: &[(&str, String)]) {
        let mut line = format!("MODEL {model} kind={kind}");
        for (name, value) in fields {
            let _ = write!(line, " {name}={value}");
        }
        println!("{line}");
    }

    fn field(name: &'static str, value: impl core::fmt::Display) -> (&'static str, String) {
        (name, value.to_string())
    }

    // ------------------------------------------------------------------
    // 模型 1：正常关闭发布尾内容（普通 / 预置 hint 两种初态）
    // ------------------------------------------------------------------

    #[derive(Default)]
    struct NormalCloseTally {
        iterations: Counter,
        closed_observed: Counter,
        open_observed: Counter,
        /// 观察到 Closed 但尾不可见（仅负对照应出现）。
        counterexample: Counter,
    }

    /// 模型 1 骨架。
    ///
    /// * `preset_hint`：是否在 server 启动**之前**由 client 预置 hint（普通 / hint 两种初态）。
    /// * `server_finish`：server 侧用哪种原子适配调用同源 `finish`（正常或负对照）。
    /// * `observe`：client 侧用哪种原子适配调用同源 `classify_prepared`（正常或负对照）。
    ///
    /// spawn 之后 client 只做一次 `classify_prepared`，然后读尾（Relaxed）；期间不 hint、
    /// 不 join、不读额外 ACK、无任何额外 Acquire。被测 HB 唯一来自同一 state 原子上
    /// finish 的 Release → `classify_prepared` 的 Acquire。并发 hint 竞争留给模型 3。
    ///
    /// `tally` 由调用方在 check 闭包外持有：负对照的反例 panic 逃出 `check` 后，调用方仍能
    /// 读到真实的"反例前已完成排列数"与反例计数。
    fn run_normal_close_model<F, O>(
        tally: &std::sync::Arc<NormalCloseTally>,
        preset_hint: bool,
        server_finish: F,
        observe: O,
    ) where
        F: Fn(&AtomicU64) -> FinishOutcome + Send + Sync + 'static,
        O: Fn(&AtomicU64) -> Prepared + Send + Sync + 'static,
    {
        let tally_in = std::sync::Arc::clone(tally);
        let server_finish = std::sync::Arc::new(server_finish);
        let observe = std::sync::Arc::new(observe);

        builder().check(move || {
            // 所有 Loom 原子在 check 闭包内创建。
            let state = Arc::new(AtomicU64::new(CLOSED));
            let tail = Arc::new(AtomicU64::new(0));

            // prepare 与可选 hint 都在 spawn 之前完成；本闭包持有该 prepare 资格记录。
            assert_eq!(channel_state::prepare(&*state, REQ), Ok(()));
            if preset_hint {
                assert_eq!(channel_state::set_hint(&*state, REQ), HintOutcome::Set);
            }

            let server = {
                let state = Arc::clone(&state);
                let tail = Arc::clone(&tail);
                let finish = std::sync::Arc::clone(&server_finish);
                thread::spawn(move || {
                    // 尾内容先发布（Relaxed），随后才 finish：HB 只能来自 finish 的 Release。
                    tail.store(TAIL, Ordering::Relaxed);
                    (*finish)(&state)
                })
            };

            // client：spawn 后只读一次严格状态，再读尾。
            let observed = (*observe)(&state);
            let saw_closed = match observed {
                Prepared::Closed => {
                    let tail_visible = tail.load(Ordering::Relaxed) == TAIL;
                    if !tail_visible {
                        // 反例计数写在断言失败之前，只用于报告，不参与同步。
                        tally_in.counterexample.bump();
                    }
                    assert!(tail_visible, "{TAIL_INVISIBLE}");
                    true
                }
                Prepared::Open => false,
                Prepared::Unknown(value) => panic!("本模型不应出现 Unknown: {value:?}"),
            };

            // 断言之后才 join：仅回收线程并取回结果，不参与被测 HB。
            let outcome = server.join().expect("server 线程不应 panic");
            let expected = if preset_hint {
                FinishOutcome::FinishedHinted
            } else {
                FinishOutcome::Finished
            };
            assert_eq!(outcome, expected, "server finish 结果与初态不符");
            if saw_closed {
                tally_in.closed_observed.bump();
            } else {
                tally_in.open_observed.bump();
            }
            tally_in.iterations.bump();
        });
    }

    /// 正向模型 1：普通与预置 hint 两种初态各自完整探索、各自计数、各自一行报告。
    #[test]
    fn model_normal_close_publishes_tail() {
        let plain = std::sync::Arc::new(NormalCloseTally::default());
        run_normal_close_model(
            &plain,
            false,
            |state| channel_state::finish(state, REQ),
            |state| channel_state::classify_prepared(state, REQ),
        );
        let hinted = std::sync::Arc::new(NormalCloseTally::default());
        run_normal_close_model(
            &hinted,
            true,
            |state| channel_state::finish(state, REQ),
            |state| channel_state::classify_prepared(state, REQ),
        );

        let normal_closed = plain.closed_observed.get();
        let plain_open = plain.open_observed.get();
        let hinted_closed = hinted.closed_observed.get();
        let hinted_open = hinted.open_observed.get();
        report(
            "model_normal_close_publishes_tail",
            "positive",
            &[
                field("variant", "plain"),
                field("iterations", plain.iterations.get()),
                field("normal_closed", normal_closed),
                field("open_observed", plain_open),
                field("counterexample", plain.counterexample.get()),
            ],
        );
        report(
            "model_normal_close_publishes_tail",
            "positive",
            &[
                field("variant", "hinted"),
                field("iterations", hinted.iterations.get()),
                field("hinted_closed", hinted_closed),
                field("open_observed", hinted_open),
                field("counterexample", hinted.counterexample.get()),
            ],
        );
        assert!(plain.iterations.get() > 0, "普通初态 check 未执行任何排列");
        assert!(
            hinted.iterations.get() > 0,
            "hint 初态 check 未执行任何排列"
        );
        assert!(normal_closed > 0, "从未观察到普通请求的 Closed");
        assert!(plain_open > 0, "普通初态从未观察到 Open（探索不完整）");
        assert!(hinted_closed > 0, "从未观察到 hint 形态的 Closed");
        assert!(hinted_open > 0, "hint 初态从未观察到 Open（探索不完整）");
        assert_eq!(plain.counterexample.get(), 0);
        assert_eq!(hinted.counterexample.get(), 0);
    }

    /// 模型侧负对照适配之一：把 CAS 的成功 / 失败序都强制降为 Relaxed，其余原样转发。
    /// 削弱的是适配层，共享算法不变；用于证明模型 1 对 finish 缺失 Release 敏感。
    struct WeakenedCas<'a>(&'a AtomicU64);

    impl channel_state::AtomicState for WeakenedCas<'_> {
        fn load(&self, order: Ordering) -> u64 {
            AtomicU64::load(self.0, order)
        }

        fn compare_exchange(
            &self,
            current: u64,
            new: u64,
            _success: Ordering,
            _failure: Ordering,
        ) -> Result<u64, u64> {
            AtomicU64::compare_exchange(self.0, current, new, Ordering::Relaxed, Ordering::Relaxed)
        }
    }

    /// 模型侧负对照适配之二：把 load 的序强制降为 Relaxed，CAS 原样转发。
    /// 用于证明模型 1 对观察者缺失 Acquire 敏感。
    struct WeakenedLoad<'a>(&'a AtomicU64);

    impl channel_state::AtomicState for WeakenedLoad<'_> {
        fn load(&self, _order: Ordering) -> u64 {
            AtomicU64::load(self.0, Ordering::Relaxed)
        }

        fn compare_exchange(
            &self,
            current: u64,
            new: u64,
            success: Ordering,
            failure: Ordering,
        ) -> Result<u64, u64> {
            AtomicU64::compare_exchange(self.0, current, new, success, failure)
        }
    }

    /// 负对照公共壳：计数器在 check 闭包外持有，反例 panic 被捕获后仍打印真实的
    /// "反例前已完成排列数"，再以预期文本重新 panic。找不到反例、或 panic 原因不是
    /// 尾不可见时以其它文本失败，使 `should_panic(expected)` 不匹配。
    fn run_negative_control<F, O>(name: &str, server_finish: F, observe: O)
    where
        F: Fn(&AtomicU64) -> FinishOutcome + Send + Sync + 'static,
        O: Fn(&AtomicU64) -> Prepared + Send + Sync + 'static,
    {
        let tally = std::sync::Arc::new(NormalCloseTally::default());
        let result = catch_unwind(AssertUnwindSafe(|| {
            run_normal_close_model(&tally, false, server_finish, observe);
        }));
        // 反例发生的那次排列不计入 iterations（计数在每次排列成功结束后才累加）。
        let iterations_before = tally.iterations.get();
        let counterexamples = tally.counterexample.get();
        match result {
            Ok(()) => {
                report(
                    name,
                    "negative_control",
                    &[
                        field("counterexample_found", false),
                        field("iterations", iterations_before),
                        field("counterexample", counterexamples),
                    ],
                );
                panic!("负对照未找到反例：模型 1 可能空过（{name}）");
            }
            Err(payload) => {
                let message = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
                    .unwrap_or_default();
                let found = message.contains(TAIL_INVISIBLE) && counterexamples > 0;
                report(
                    name,
                    "negative_control",
                    &[
                        field("counterexample_found", found),
                        field("iterations_before_counterexample", iterations_before),
                        field("counterexample", counterexamples),
                    ],
                );
                assert!(
                    found,
                    "负对照 panic 但不是尾不可见反例（{name}）: {message}"
                );
                panic!("负对照找到反例（{name}）: {TAIL_INVISIBLE}");
            }
        }
    }

    /// 负对照 1：server 的 finish 没有 Release 时，Loom 必须找到"Closed 可见但尾不可见"。
    #[test]
    #[should_panic(expected = "观察到 Closed 后尾内容不可见")]
    fn model_negative_control_relaxed_finish_is_detected() {
        run_negative_control(
            "model_negative_control_relaxed_finish_is_detected",
            |state| channel_state::finish(&WeakenedCas(state), REQ),
            |state| channel_state::classify_prepared(state, REQ),
        );
    }

    /// 负对照 2：观察者的 load 没有 Acquire 时，Loom 必须找到"Closed 可见但尾不可见"。
    #[test]
    #[should_panic(expected = "观察到 Closed 后尾内容不可见")]
    fn model_negative_control_relaxed_observer_is_detected() {
        run_negative_control(
            "model_negative_control_relaxed_observer_is_detected",
            |state| channel_state::finish(state, REQ),
            |state| channel_state::classify_prepared(&WeakenedLoad(state), REQ),
        );
    }

    // ------------------------------------------------------------------
    // 模型 2：清理不能证明尾内容已发布
    // ------------------------------------------------------------------

    #[derive(Default)]
    struct CleanupTally {
        iterations: Counter,
        closed_observed: Counter,
        unknown_observed: Counter,
        open_observed: Counter,
        unknown_with_tail_unpublished: Counter,
        normal_close_won: Counter,
        cleanup_won: Counter,
    }

    /// client 单次观察的结果（在 join 之前得到，join 之后再计数）。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Observation {
        Closed,
        Open,
        /// ORPHANED；附带此刻 Relaxed 读到的尾是否已是 TAIL（仅记录，不断言）。
        UnknownOrphaned {
            tail_published: bool,
        },
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "完整保留单一模型的并发操作、断言和结果计数，便于核对同步边界"
    )]
    fn model_cleanup_never_proves_tail_drained() {
        let tally = std::sync::Arc::new(CleanupTally::default());
        let tally_in = std::sync::Arc::clone(&tally);

        builder().check(move || {
            let state = Arc::new(AtomicU64::new(CLOSED));
            let tail = Arc::new(AtomicU64::new(0));
            assert_eq!(channel_state::prepare(&*state, REQ), Ok(()));

            let server = {
                let state = Arc::clone(&state);
                let tail = Arc::clone(&tail);
                thread::spawn(move || {
                    tail.store(TAIL, Ordering::Relaxed);
                    channel_state::finish(&*state, REQ)
                })
            };
            // 独立清理线程：只走同源 mark_orphaned；不 join server、不读任何额外 ACK。
            let cleaner = {
                let state = Arc::clone(&state);
                thread::spawn(move || channel_state::mark_orphaned(&*state))
            };

            // client 只读一次严格状态；之后的尾读取仅用于记录 / Closed 分支断言。
            let observation = match channel_state::classify_prepared(&*state, REQ) {
                Prepared::Closed => {
                    assert_eq!(tail.load(Ordering::Relaxed), TAIL, "{TAIL_INVISIBLE}");
                    Observation::Closed
                }
                Prepared::Open => Observation::Open,
                Prepared::Unknown(Value::Orphaned) => Observation::UnknownOrphaned {
                    // ORPHANED 不携带发布关系：这里只记录，不断言。
                    tail_published: tail.load(Ordering::Relaxed) == TAIL,
                },
                Prepared::Unknown(other) => panic!("本模型不应出现 Unknown({other:?})"),
            };

            let server_outcome = server.join().expect("server 线程不应 panic");
            let cleaner_outcome = cleaner.join().expect("cleaner 线程不应 panic");

            let normal_won = matches!(
                server_outcome,
                FinishOutcome::Finished | FinishOutcome::FinishedHinted
            );
            let cleanup_won = matches!(cleaner_outcome, OrphanOutcome::Orphaned(_));
            assert!(
                normal_won ^ cleanup_won,
                "恰有一个胜者: server={server_outcome:?} cleaner={cleaner_outcome:?}"
            );
            if normal_won {
                assert_eq!(cleaner_outcome, OrphanOutcome::KeptClosed);
            } else {
                assert_eq!(server_outcome, FinishOutcome::Orphaned);
            }
            match observation {
                Observation::Closed => assert!(normal_won, "看到 Closed 必是正常关闭赢"),
                Observation::UnknownOrphaned { .. } => {
                    assert!(cleanup_won, "看到 ORPHANED 必是清理赢");
                }
                Observation::Open => {}
            }

            // 计数只在全部断言与 join 之后。
            match observation {
                Observation::Closed => tally_in.closed_observed.bump(),
                Observation::Open => tally_in.open_observed.bump(),
                Observation::UnknownOrphaned { tail_published } => {
                    tally_in.unknown_observed.bump();
                    if !tail_published {
                        tally_in.unknown_with_tail_unpublished.bump();
                    }
                }
            }
            if normal_won {
                tally_in.normal_close_won.bump();
            } else {
                tally_in.cleanup_won.bump();
            }
            tally_in.iterations.bump();
        });

        let iterations = tally.iterations.get();
        let closed_observed = tally.closed_observed.get();
        let unknown_observed = tally.unknown_observed.get();
        let open_observed = tally.open_observed.get();
        let unknown_with_tail_unpublished = tally.unknown_with_tail_unpublished.get();
        let normal_close_won = tally.normal_close_won.get();
        let cleanup_won = tally.cleanup_won.get();
        report(
            "model_cleanup_never_proves_tail_drained",
            "positive",
            &[
                field("iterations", iterations),
                field("closed_observed", closed_observed),
                field("unknown_observed", unknown_observed),
                field("open_observed", open_observed),
                field(
                    "unknown_with_tail_unpublished",
                    unknown_with_tail_unpublished,
                ),
                field("normal_close_won", normal_close_won),
                field("cleanup_won", cleanup_won),
            ],
        );
        assert!(iterations > 0, "check 未执行任何排列");
        assert!(normal_close_won > 0, "正常关闭从未赢过 CAS");
        assert!(cleanup_won > 0, "清理从未赢过 CAS");
        assert!(closed_observed > 0, "从未观察到 Closed");
        assert!(unknown_observed > 0, "从未观察到 ORPHANED → Unknown");
        assert!(
            unknown_with_tail_unpublished > 0,
            "从未出现 ORPHANED 且尾未发布的排列：无法证明 ORPHANED 不携带发布关系"
        );
        assert_eq!(
            normal_close_won + cleanup_won,
            iterations,
            "每次排列恰一个胜者"
        );
    }

    // ------------------------------------------------------------------
    // 模型 3：hint / prepare / 清理竞争后，ORPHANED 不被迟到操作升级；合法复用时旧请求 Unknown
    // ------------------------------------------------------------------

    #[derive(Default)]
    struct GenerationTally {
        iterations: Counter,
        cleanup_won: Counter,
        normal_won: Counter,
        hint_set_before_close: Counter,
        hint_set_before_cleanup: Counter,
        hint_lost_to_close: Counter,
        hint_lost_to_cleanup: Counter,
        reuse_prepare_after_normal_close: Counter,
    }

    /// 同源状态机证明的范围：清理胜者后的 ORPHANED 不被迟到 finish / hint / prepare 升级；
    /// 未复用前的合法 prepare 竞争（hint 先于 / 晚于关闭或清理）；正常关闭赢后
    /// `prepare(REQ2)` 成功且此时 `classify_prepared(REQ)` 为 Unknown(Open(REQ2))。
    /// 复用后再次 CLOSED 的同值问题不在此证明，见能力边界测试。
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "完整保留单一模型的竞争、迟到操作和代次断言，避免拆散被测执行顺序"
    )]
    fn model_hint_prepare_cleanup_and_generation_never_upgrade_unknown() {
        let tally = std::sync::Arc::new(GenerationTally::default());
        let tally_in = std::sync::Arc::clone(&tally);

        builder().check(move || {
            let state = Arc::new(AtomicU64::new(CLOSED));
            assert_eq!(channel_state::prepare(&*state, REQ), Ok(()));

            // 竞争阶段：server finish、cleaner mark_orphaned、client hint 三方真实 CAS。
            let server = {
                let state = Arc::clone(&state);
                thread::spawn(move || channel_state::finish(&*state, REQ))
            };
            let cleaner = {
                let state = Arc::clone(&state);
                thread::spawn(move || channel_state::mark_orphaned(&*state))
            };
            let hint = channel_state::set_hint(&*state, REQ);

            // 本模型只验状态机（不验尾内容 HB），允许 join 之后再做迟到操作。
            let server_outcome = server.join().expect("server 线程不应 panic");
            let cleaner_outcome = cleaner.join().expect("cleaner 线程不应 panic");

            let normal_won = matches!(
                server_outcome,
                FinishOutcome::Finished | FinishOutcome::FinishedHinted
            );
            let cleanup_won = matches!(cleaner_outcome, OrphanOutcome::Orphaned(_));
            assert!(
                normal_won ^ cleanup_won,
                "恰有一个胜者: server={server_outcome:?} cleaner={cleaner_outcome:?}"
            );
            assert!(
                channel_state::terminal(&*state),
                "竞争结束后 channel 必须处于终止值"
            );
            let raw_before_late_ops = state.load(Ordering::Acquire);

            // 迟到阶段：旧 finish / 旧 hint / 新 prepare / 旧 prepare，再做同源分类。
            let late_finish = channel_state::finish(&*state, REQ);
            let late_hint = channel_state::set_hint(&*state, REQ);
            let reuse = channel_state::prepare(&*state, REQ2);
            let old_request_prepare = channel_state::prepare(&*state, REQ);
            let observed_req = channel_state::classify_prepared(&*state, REQ);
            let observed_req2 = channel_state::classify_prepared(&*state, REQ2);
            let terminal_after_late_ops = channel_state::terminal(&*state);
            let raw_after_late_ops = state.load(Ordering::Acquire);

            if cleanup_won {
                assert_eq!(raw_before_late_ops, ORPHANED, "清理赢时终值应为 ORPHANED");
                assert_eq!(raw_after_late_ops, ORPHANED, "迟到操作不得改变 ORPHANED");
                assert_eq!(server_outcome, FinishOutcome::Orphaned);
                assert!(
                    matches!(hint, HintOutcome::Set | HintOutcome::Orphaned),
                    "清理赢时 hint 只能是清理前 Set 或清理后 Orphaned: {hint:?}"
                );
                assert_eq!(late_finish, FinishOutcome::Orphaned, "迟到 finish 不得重开");
                assert_eq!(late_hint, HintOutcome::Orphaned, "迟到 hint 不得重开");
                assert_eq!(reuse, Err(PrepareError::NotClosed(Value::Orphaned)));
                assert_eq!(
                    old_request_prepare,
                    Err(PrepareError::NotClosed(Value::Orphaned))
                );
                assert_eq!(observed_req, Prepared::Unknown(Value::Orphaned));
                assert_eq!(observed_req2, Prepared::Unknown(Value::Orphaned));
                assert!(terminal_after_late_ops);
            } else {
                assert_eq!(raw_before_late_ops, CLOSED, "正常关闭赢时终值应为 CLOSED");
                assert_eq!(
                    raw_after_late_ops, REQ2,
                    "合法复用后 channel 应为 Open(REQ2)"
                );
                assert_eq!(cleaner_outcome, OrphanOutcome::KeptClosed);
                match hint {
                    HintOutcome::Set => assert_eq!(server_outcome, FinishOutcome::FinishedHinted),
                    HintOutcome::Closed => assert_eq!(server_outcome, FinishOutcome::Finished),
                    other => panic!("正常关闭赢时 hint 不应为 {other:?}"),
                }
                assert_eq!(late_finish, FinishOutcome::AlreadyClosed);
                assert_eq!(late_hint, HintOutcome::Closed);
                // 合法复用：CLOSED → REQ2；此时旧请求的同源分类为 Unknown（错代次），新请求 Open。
                assert_eq!(reuse, Ok(()));
                assert_eq!(
                    old_request_prepare,
                    Err(PrepareError::NotClosed(Value::Open(REQ2)))
                );
                assert_eq!(observed_req, Prepared::Unknown(Value::Open(REQ2)));
                assert_eq!(observed_req2, Prepared::Open);
                assert!(!terminal_after_late_ops, "复用后 channel 不再是终止值");
            }

            // 计数只在全部断言之后。
            if cleanup_won {
                tally_in.cleanup_won.bump();
                match hint {
                    HintOutcome::Set => tally_in.hint_set_before_cleanup.bump(),
                    _ => tally_in.hint_lost_to_cleanup.bump(),
                }
            } else {
                tally_in.normal_won.bump();
                tally_in.reuse_prepare_after_normal_close.bump();
                match hint {
                    HintOutcome::Set => tally_in.hint_set_before_close.bump(),
                    _ => tally_in.hint_lost_to_close.bump(),
                }
            }
            tally_in.iterations.bump();
        });

        let iterations = tally.iterations.get();
        let cleanup_won = tally.cleanup_won.get();
        let normal_won = tally.normal_won.get();
        let hint_set_before_close = tally.hint_set_before_close.get();
        let hint_set_before_cleanup = tally.hint_set_before_cleanup.get();
        let hint_lost_to_close = tally.hint_lost_to_close.get();
        let hint_lost_to_cleanup = tally.hint_lost_to_cleanup.get();
        let reuse_prepare_after_normal_close = tally.reuse_prepare_after_normal_close.get();
        report(
            "model_hint_prepare_cleanup_and_generation_never_upgrade_unknown",
            "positive",
            &[
                field("iterations", iterations),
                field("cleanup_won", cleanup_won),
                field("normal_won", normal_won),
                field("hint_set_before_close", hint_set_before_close),
                field("hint_set_before_cleanup", hint_set_before_cleanup),
                field("hint_lost_to_close", hint_lost_to_close),
                field("hint_lost_to_cleanup", hint_lost_to_cleanup),
                field(
                    "reuse_prepare_after_normal_close",
                    reuse_prepare_after_normal_close,
                ),
            ],
        );
        assert!(iterations > 0, "check 未执行任何排列");
        assert!(cleanup_won > 0, "清理从未赢过");
        assert!(normal_won > 0, "正常关闭从未赢过");
        assert!(hint_set_before_close > 0, "从未出现 hint 先于正常关闭");
        assert!(hint_set_before_cleanup > 0, "从未出现 hint 先于清理");
        assert!(hint_lost_to_close > 0, "从未出现正常关闭先于 hint");
        assert!(hint_lost_to_cleanup > 0, "从未出现清理先于 hint");
        assert!(
            reuse_prepare_after_normal_close > 0,
            "从未验证正常关闭后的合法复用"
        );
        assert_eq!(cleanup_won + normal_won, iterations, "每次排列恰一个胜者");
    }

    // ------------------------------------------------------------------
    // 能力边界：同值 CLOSED 分不清初始 / 复用代次（记录非零反例，不是生产保证）
    // ------------------------------------------------------------------

    #[derive(Default)]
    struct BoundaryTally {
        iterations: Counter,
        /// 从未 prepare 的初始 CLOSED 被同源分类报为 Closed。
        fresh_closed_reported_closed: Counter,
        /// 复用代次 REQ2 关闭后，旧请求 REQ 的同源分类再次报为 Closed。
        reused_closed_reported_closed: Counter,
        /// 复用代次仍 Open 时旧请求可分辨为 Unknown（对照）。
        reused_open_reported_unknown: Counter,
    }

    /// CAL 原语的能力边界（单线程，顺序执行）。
    ///
    /// `classify_prepared` 只看单一状态字：初始 CLOSED 与复用代次关闭后的 CLOSED 是同一个
    /// 值，它对旧请求同样返回 `Closed`。这不是生产保证通过，而是明确 R2 必须由原 route / pin
    /// 与 channel 复用所有权（未归还的 pin 阻止 prepare 复用）阻止该调用成为合法完成证据；
    /// 本包不新增伪 generation 原子、地址身份或自报 bool 来"解决"它。R1 不自动关闭原代次 /
    /// 映射缺陷。
    #[test]
    fn model_capability_boundary_reused_closed_is_indistinguishable() {
        let tally = std::sync::Arc::new(BoundaryTally::default());
        let tally_in = std::sync::Arc::clone(&tally);

        builder().check(move || {
            // 1) 从未 prepare 的初始 CLOSED：原语无法单独分辨，仍报 Closed。
            let fresh = AtomicU64::new(CLOSED);
            let fresh_observed = channel_state::classify_prepared(&fresh, REQ);
            assert_eq!(
                fresh_observed,
                Prepared::Closed,
                "初始 CLOSED 被原语报为 Closed（能力边界）"
            );

            // 2) 合法代次：prepare(REQ) → finish(REQ) → Closed 属正常证明路径。
            let state = AtomicU64::new(CLOSED);
            assert_eq!(channel_state::prepare(&state, REQ), Ok(()));
            assert_eq!(
                channel_state::classify_prepared(&state, REQ),
                Prepared::Open
            );
            assert_eq!(channel_state::finish(&state, REQ), FinishOutcome::Finished);
            assert_eq!(
                channel_state::classify_prepared(&state, REQ),
                Prepared::Closed
            );

            // 3) 复用代次仍 Open 时旧请求可分辨（对照）。
            assert_eq!(channel_state::prepare(&state, REQ2), Ok(()));
            let reused_open = channel_state::classify_prepared(&state, REQ);
            assert_eq!(reused_open, Prepared::Unknown(Value::Open(REQ2)));

            // 4) 复用代次关闭后：同值 CLOSED，旧请求再次被报为 Closed —— 反例。
            assert_eq!(channel_state::finish(&state, REQ2), FinishOutcome::Finished);
            let reused_closed = channel_state::classify_prepared(&state, REQ);
            assert_eq!(
                reused_closed,
                Prepared::Closed,
                "复用代次关闭后的同值 CLOSED 被原语报为 Closed（能力边界）"
            );
            // 其它严格观测同样无法区分：CLOSED 上 has_state / has_hint 恒为假，terminal 恒为真。
            assert!(!channel_state::has_state(&state, REQ));
            assert!(!channel_state::has_hint(&state, REQ));
            assert!(channel_state::terminal(&state));

            // 计数只在全部断言之后。
            tally_in.fresh_closed_reported_closed.bump();
            tally_in.reused_open_reported_unknown.bump();
            tally_in.reused_closed_reported_closed.bump();
            tally_in.iterations.bump();
        });

        let iterations = tally.iterations.get();
        let fresh = tally.fresh_closed_reported_closed.get();
        let reused_closed = tally.reused_closed_reported_closed.get();
        let reused_open = tally.reused_open_reported_unknown.get();
        report(
            "model_capability_boundary_reused_closed_is_indistinguishable",
            "capability_boundary",
            &[
                field("iterations", iterations),
                field("fresh_closed_reported_closed", fresh),
                field("reused_closed_reported_closed", reused_closed),
                field("reused_open_reported_unknown", reused_open),
            ],
        );
        assert!(iterations > 0, "check 未执行任何排列");
        assert!(fresh > 0, "未记录初始 CLOSED 的能力边界反例");
        assert!(
            reused_closed > 0,
            "未记录复用代次同值 CLOSED 的能力边界反例"
        );
        assert!(reused_open > 0, "未记录复用代次 Open 时的对照");
    }
}
