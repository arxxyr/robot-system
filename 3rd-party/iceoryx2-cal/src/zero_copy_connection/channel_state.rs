// RR R1（robot-system 本地补丁，2026-09-14）：channel 状态的同源转移原语。
//
// 本文件是 iceoryx2-cal 的私有模块，同时被 tools/rr-memory-model 通过 `#[path]` 原样包含。
// 因此只允许依赖 core 与本文件内的 trait：生产侧由 mod.rs 用 iceoryx2_bb_concurrency 的
// AtomicU64 适配，模型侧用 Loom 的 AtomicU64 适配；全部真实 load / compare_exchange 及其
// Ordering 只在此出现一次，两侧不得各自复制算法或只共享分类函数。
//
// 许可：与所在 crate 相同（MIT OR Apache-2.0）。

use core::sync::atomic::Ordering;

/// 终止哨兵：正常关闭。等于公开的 `CHANNEL_STATE_CLOSED`。
pub(crate) const CLOSED: u64 = u64::MAX;
/// 终止哨兵：第三方清理者写入，表示原端点未正常关闭。私有，不进入公开 `ChannelState`。
pub(crate) const ORPHANED: u64 = u64::MAX - 1;
/// 断开提示位。等于公开的 `CHANNEL_STATE_DISCONNECT_HINT_BIT`。
pub(crate) const HINT_BIT: u64 = 1u64 << 63;
/// 普通合法 `ChannelState` 上限（含）：2^62，等于 `ChannelState::max_value()`。
pub(crate) const MAX_LEGAL_REQUEST: u64 = 1u64 << 62;
/// strict 请求序号上限（不含）：2^62。普通域与 strict 域不得混淆。
pub(crate) const STRICT_REQUEST_LIMIT: u64 = 1u64 << 62;
/// 合法 hint 的最大值：3 * 2^62，与 ORPHANED / CLOSED 分离。
pub(crate) const MAX_LEGAL_HINT: u64 = HINT_BIT | MAX_LEGAL_REQUEST;

/// 状态字的最小原子抽象：只有 load 与强 compare_exchange，Ordering 由本文件决定。
pub(crate) trait AtomicState {
    fn load(&self, order: Ordering) -> u64;
    fn compare_exchange(
        &self,
        current: u64,
        new: u64,
        success: Ordering,
        failure: Ordering,
    ) -> Result<u64, u64>;
}

/// 原始状态值的分类。先识别两个终止哨兵，再看 hint 位与合法请求域；
/// 保留域之外的值归 `Invalid`，不做粗掩码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Value {
    Closed,
    Orphaned,
    /// 合法请求（0..=2^62），无 hint。
    Open(u64),
    /// 合法请求 | HINT_BIT。
    Hinted(u64),
    /// 任何合法域之外的原始值（受检 API 不会产生）。
    Invalid(u64),
}

pub(crate) const fn classify_value(raw: u64) -> Value {
    if raw == CLOSED {
        return Value::Closed;
    }
    if raw == ORPHANED {
        return Value::Orphaned;
    }
    if raw & HINT_BIT == 0 {
        if raw <= MAX_LEGAL_REQUEST {
            Value::Open(raw)
        } else {
            Value::Invalid(raw)
        }
    } else if raw <= MAX_LEGAL_HINT {
        Value::Hinted(raw & !HINT_BIT)
    } else {
        Value::Invalid(raw)
    }
}

pub(crate) const fn is_legal_request(request: u64) -> bool {
    request <= MAX_LEGAL_REQUEST
}

pub(crate) const fn is_strict_request(request: u64) -> bool {
    request < STRICT_REQUEST_LIMIT
}

pub(crate) const fn is_terminal_value(raw: u64) -> bool {
    raw == CLOSED || raw == ORPHANED
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrepareError {
    /// 请求不在允许域内；未执行任何原子操作。
    IllegalRequest,
    /// 当前不是 CLOSED；携带 CAS 失败时观察到的分类值。
    NotClosed(Value),
}

/// prepare：仅 CLOSED → 合法请求。成功 AcqRel，失败 Acquire。
/// 初始 CLOSED 本身不是任何请求的完成证据；strict 资格由调用方保存本次 prepare 记录。
pub(crate) fn prepare<S: AtomicState + ?Sized>(
    state: &S,
    request: u64,
) -> Result<(), PrepareError> {
    if !is_legal_request(request) {
        return Err(PrepareError::IllegalRequest);
    }
    match state.compare_exchange(CLOSED, request, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Ok(()),
        Err(observed) => Err(PrepareError::NotClosed(classify_value(observed))),
    }
}

/// strict prepare：请求序号另限 0..<2^62，其余与 [`prepare`] 相同。
pub(crate) fn prepare_strict<S: AtomicState + ?Sized>(
    state: &S,
    request: u64,
) -> Result<(), PrepareError> {
    if !is_strict_request(request) {
        return Err(PrepareError::IllegalRequest);
    }
    prepare(state, request)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HintOutcome {
    Set,
    AlreadyHinted,
    Closed,
    Orphaned,
    /// 当前是别的请求代次或非法值；未改变。
    WrongGeneration(Value),
    IllegalRequest,
}

/// hint：只匹配原请求（request → request | HINT_BIT）。不能从 ORPHANED / CLOSED 转换。
pub(crate) fn set_hint<S: AtomicState + ?Sized>(state: &S, request: u64) -> HintOutcome {
    if !is_legal_request(request) {
        return HintOutcome::IllegalRequest;
    }
    match state.compare_exchange(
        request,
        request | HINT_BIT,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => HintOutcome::Set,
        Err(observed) => match classify_value(observed) {
            Value::Hinted(r) if r == request => HintOutcome::AlreadyHinted,
            Value::Closed => HintOutcome::Closed,
            Value::Orphaned => HintOutcome::Orphaned,
            other => HintOutcome::WrongGeneration(other),
        },
    }
}

/// 严格观测（Acquire）：当前值是否恰为原请求的 hint 形态。
pub(crate) fn has_hint<S: AtomicState + ?Sized>(state: &S, request: u64) -> bool {
    matches!(classify_value(state.load(Ordering::Acquire)), Value::Hinted(r) if r == request)
}

/// 严格观测（Acquire）：当前值是否属于原请求（无论是否带 hint）。
pub(crate) fn has_state<S: AtomicState + ?Sized>(state: &S, request: u64) -> bool {
    match classify_value(state.load(Ordering::Acquire)) {
        Value::Open(r) | Value::Hinted(r) => r == request,
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FinishOutcome {
    /// 由本次调用从原请求关闭。
    Finished,
    /// 首个 CAS 遇到同一请求的新 hint，随后从 hint 形态关闭。
    FinishedHinted,
    AlreadyClosed,
    Orphaned,
    /// 当前是别的请求代次或非法值；未改变。
    WrongGeneration(Value),
    IllegalRequest,
}

/// 正常 finish：仅原请求或其 hint → CLOSED，成功 Release；失败 Acquire 后重新分类：
/// 遇同一请求的新 hint 继续完成一次，遇 CLOSED / ORPHANED / 其它代次不改值。
pub(crate) fn finish<S: AtomicState + ?Sized>(state: &S, request: u64) -> FinishOutcome {
    if !is_legal_request(request) {
        return FinishOutcome::IllegalRequest;
    }
    match state.compare_exchange(request, CLOSED, Ordering::Release, Ordering::Acquire) {
        Ok(_) => FinishOutcome::Finished,
        Err(observed) => match classify_value(observed) {
            Value::Hinted(r) if r == request => {
                match state.compare_exchange(
                    request | HINT_BIT,
                    CLOSED,
                    Ordering::Release,
                    Ordering::Acquire,
                ) {
                    Ok(_) => FinishOutcome::FinishedHinted,
                    Err(again) => match classify_value(again) {
                        Value::Closed => FinishOutcome::AlreadyClosed,
                        Value::Orphaned => FinishOutcome::Orphaned,
                        other => FinishOutcome::WrongGeneration(other),
                    },
                }
            }
            Value::Closed => FinishOutcome::AlreadyClosed,
            Value::Orphaned => FinishOutcome::Orphaned,
            other => FinishOutcome::WrongGeneration(other),
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OrphanOutcome {
    /// 由本次调用从合法 Open / hint 值改为 ORPHANED；携带被替换的分类值。
    Orphaned(Value),
    KeptClosed,
    KeptOrphaned,
    /// 合法域之外的原始值：清理者不触碰。
    KeptInvalid(u64),
}

/// 第三方清理：Acquire 读，仅合法 Open / hint → ORPHANED（成功 AcqRel，失败 Acquire 重读）。
/// CLOSED / ORPHANED 保持：清理不能覆盖正常关闭，重复清理不改值。
pub(crate) fn mark_orphaned<S: AtomicState + ?Sized>(state: &S) -> OrphanOutcome {
    let mut current = state.load(Ordering::Acquire);
    loop {
        match classify_value(current) {
            Value::Closed => return OrphanOutcome::KeptClosed,
            Value::Orphaned => return OrphanOutcome::KeptOrphaned,
            Value::Invalid(raw) => return OrphanOutcome::KeptInvalid(raw),
            observed @ (Value::Open(_) | Value::Hinted(_)) => {
                match state.compare_exchange(current, ORPHANED, Ordering::AcqRel, Ordering::Acquire)
                {
                    Ok(_) => return OrphanOutcome::Orphaned(observed),
                    Err(changed) => current = changed,
                }
            }
        }
    }
}

/// 通用终止识别（Acquire）：CLOSED 与 ORPHANED 都视为不可再等待。
pub(crate) fn terminal<S: AtomicState + ?Sized>(state: &S) -> bool {
    is_terminal_value(state.load(Ordering::Acquire))
}

/// 已具 prepare 资格的原请求的严格三态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Prepared {
    /// 正常关闭：与关闭方的 Release 形成 happens-before，关闭前入队的尾内容可见。
    Closed,
    /// 仍是原请求（含 hint 形态）：尚未观察到终结，不声称对端存活。
    Open,
    /// ORPHANED、其它代次或非法值：不能证明任何发布关系；携带观察到的分类值。
    Unknown(Value),
}

/// 严格分类（Acquire）。这只是原状态分类原语：调用方必须持有本次成功 prepare 的记录
/// 与原 route / pin；无 prepare 记录或 channel 已被复用时，CLOSED 值不证明原请求完成。
pub(crate) fn classify_prepared<S: AtomicState + ?Sized>(state: &S, request: u64) -> Prepared {
    match classify_value(state.load(Ordering::Acquire)) {
        Value::Closed => Prepared::Closed,
        Value::Open(r) | Value::Hinted(r) if r == request => Prepared::Open,
        other => Prepared::Unknown(other),
    }
}
