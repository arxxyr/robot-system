//! 单写多读 seqlock —— **RT 侧 publish，非 RT 侧 load**。
//!
//! 对应 `docs/00_design.md` §4.9 snapshot 同步规范的 **`rt_thread` → `pub_thread`**
//! 方向；Step 6 Part 1 首次落地，S48 A3b 重做同步本体（PLAN P.6.14.4）。
//!
//! ### 为什么用 seqlock 而非 `arc-swap`
//!
//! arc-swap 的 `store()` 内部 `Arc::new(T)` 要分配堆，破 RT 铁律（§4.9）。本模块
//! 用预分配的原子字数组把"写 snapshot"做成固定数量的原子存储：零 alloc、零锁。
//!
//! 相反方向（非 RT 写 / RT 读）用 `super::command_slot::LatestCommandSlot`
//! —— arc-swap Guard 在 RT 侧 `load()` 不 bump refcount，方向是天然契合的。
//!
//! ### A3b 之前的问题（已由 Miri 实际报出，见 S48 A3b 复现证据）
//!
//! 旧实现把 `T` 放在 `UnsafeCell<T>` 里，用 `read_volatile` / `write_volatile`
//! 在线程间搬运非原子载荷，靠事后 version 重检丢弃撕裂值。Rust 内存模型下
//! `volatile` 不是原子：reader 读载荷之后 writer 的下一次写没有 happens-before
//! 边，是数据竞争（UB），重检只能丢值、不能消除竞争。此外 `!Clone` 的写端仍是
//! `publish(&self)`，经 `Arc` 共享即可多线程并发写。
//!
//! ### 现协议
//!
//! - 载荷是构造时预分配、预先写入初值的 `Box<[AtomicU64]>`，长度由
//!   [`SnapshotValue::Words`] 固定；`version: AtomicU64` 偶数 = 稳态、奇数 =
//!   writer 正在写。不存 `UnsafeCell<T>`，不用 volatile / transmute / 原始结构体
//!   字节，没有任何 `unsafe`。
//! - 写端 [`RtSnapshotPublisher::publish`] 取 `&mut self`（写端不 `Clone`，`Arc`
//!   共享只能得到 `&`，编译期即拒绝第二个写者）：先在栈上 `encode`，核当前版本为
//!   偶数且 `checked_add(2)` 仍在范围内，再依次 `SeqCst` 存奇版本、全部载荷字、
//!   偶版本。奇区间内只做固定数量的原子存储。
//! - 读端 [`RtSnapshotReader::load`]：栈上固定 `Words`，`SeqCst` 读偶版本 → 全部
//!   载荷字 → 再读版本；两次版本相等才 `decode`，否则重试。所有版本与载荷读写
//!   均为 `SeqCst`，初版不做混合弱序或手写 fence 优化，让一致性判断有一个共同
//!   全序：无版本回绕时，相同偶版本的两次读取之间不可能夹入一次发布。
//! - 版本初值 0，最大可发布的偶版本为 `u64::MAX - 1`；再发布返回
//!   [`SnapshotPublishError::VersionExhausted`]，上一偶版本与载荷完整保留，
//!   不回绕、不"停更却宣称成功"。
//! - reader 只供非 RT 线程使用；writer 被调度暂停时 reader 会自旋等待，本模块
//!   **不承诺** reader 的墙钟上界，也不给出未经测量的纳秒级性能断言。
//!
//! ### 编码契约
//!
//! [`SnapshotValue`] 是安全 Rust 的显式字段 codec：`decode(encode(value))` 必须
//! 保持全部业务字段；违反可逆契约是程序错误，实现不得靠 `unsafe` 构造无效枚举。
//! `Words` 只能是固定长度 `[u64; N]`（[`SnapshotWords`] 用私有 sealed 边界限制），
//! 不能用任意 `AsRef` 或动态集合伪装固定大小。RT 无分配结论只覆盖实际生产
//! codec 及经真实 allocation guard 测试的路径。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// 最大可发布的偶版本；`publish` 在版本无法再前进两步时返回错误。
pub const MAX_PUBLISHED_VERSION: u64 = u64::MAX - 1;

mod sealed {
    pub trait Sealed {}
    impl<const N: usize> Sealed for [u64; N] {}
}

/// 固定长度的载荷字数组族：只有 `[u64; N]` 实现（sealed）。
pub trait SnapshotWords: sealed::Sealed + Copy + Send + Sync + 'static {
    /// 字数。
    const LEN: usize;
    /// 全零字。
    fn zeroed() -> Self;
    /// 只读视图，长度恒为 `LEN`。
    fn as_words(&self) -> &[u64];
    /// 可变视图，长度恒为 `LEN`。
    fn as_words_mut(&mut self) -> &mut [u64];
}

impl<const N: usize> SnapshotWords for [u64; N] {
    const LEN: usize = N;

    fn zeroed() -> Self {
        [0; N]
    }

    fn as_words(&self) -> &[u64] {
        self
    }

    fn as_words_mut(&mut self) -> &mut [u64] {
        self
    }
}

/// 快照载荷的显式字段 codec。
///
/// `encode` 只写入 `words`（调用方给的全零字），`decode` 只读取；两者都是安全
/// Rust 且必须互逆。实现者负责字段到字的固定布局，不得夹带过滤或修正。
pub trait SnapshotValue: Copy + Send + Sync + 'static {
    /// 固定长度的字数组。
    type Words: SnapshotWords;
    /// 把全部业务字段写入 `words`。
    fn encode(&self, words: &mut Self::Words);
    /// 从 `words` 恢复全部业务字段。
    fn decode(words: &Self::Words) -> Self;
}

/// 单字恒等 codec，供简单计数值与测试使用。
impl SnapshotValue for u64 {
    type Words = [u64; 1];

    fn encode(&self, words: &mut Self::Words) {
        words[0] = *self;
    }

    fn decode(words: &Self::Words) -> Self {
        words[0]
    }
}

/// `publish` 的失败原因。任何一种都表示本次**未**发布，上一偶版本与载荷保持不变。
///
/// 本文件保持只依赖标准库（最小 Miri 夹具可原样编译），故手写 `Display` / `Error`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotPublishError {
    /// 版本已到 [`MAX_PUBLISHED_VERSION`]，没有下一偶版本；不回绕。
    VersionExhausted,
    /// 进入 `publish` 时观察到奇版本。`&mut self` 单写下不可达，仍 fail-closed
    /// 返回错误而不是 panic 或继续写。
    VersionCorrupted,
}

impl std::fmt::Display for SnapshotPublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VersionExhausted => {
                write!(
                    f,
                    "snapshot 版本已耗尽（最大偶版本 {MAX_PUBLISHED_VERSION}）"
                )
            }
            Self::VersionCorrupted => {
                f.write_str("snapshot 版本在进入 publish 时为奇数，写端状态已损坏")
            }
        }
    }
}

impl std::error::Error for SnapshotPublishError {}

/// 只在测试 / 验收构造入口使用的初始版本错误。
#[cfg(any(test, feature = "test-comm-fault-hook"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotVersionError(pub u64);

#[cfg(any(test, feature = "test-comm-fault-hook"))]
impl std::fmt::Display for SnapshotVersionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "snapshot 初始版本必须是不超过 {MAX_PUBLISHED_VERSION} 的偶数，实际 {}",
            self.0
        )
    }
}

#[cfg(any(test, feature = "test-comm-fault-hook"))]
impl std::error::Error for SnapshotVersionError {}

/// 单写多读 seqlock 核心。内部使用，不对外导出。
struct Seqlock {
    version: AtomicU64,
    payload: Box<[AtomicU64]>,
}

impl Seqlock {
    fn new(initial_words: &[u64], version: u64) -> Self {
        Self {
            version: AtomicU64::new(version),
            payload: initial_words
                .iter()
                .map(|word| AtomicU64::new(*word))
                .collect(),
        }
    }
}

/// 写端（RT 线程持有，**不可 Clone**）。
///
/// `publish(&mut self, &T)`：栈上 encode + 固定数量的 `SeqCst` 原子存储，不分配。
/// `Arc<RtSnapshotPublisher<T>>` 只能得到 `&self`，无法调用 `publish`。
pub struct RtSnapshotPublisher<T: SnapshotValue> {
    inner: Arc<Seqlock>,
    _value: std::marker::PhantomData<fn() -> T>,
}

/// 读端（非 RT 线程持有，**可 Clone**）。
pub struct RtSnapshotReader<T: SnapshotValue> {
    inner: Arc<Seqlock>,
    _value: std::marker::PhantomData<fn() -> T>,
}

impl<T: SnapshotValue> RtSnapshotPublisher<T> {
    /// 创建一对 publisher / reader，初始值为 `initial`，版本 0。
    ///
    /// 内部 `Box` / `Arc` 分配在 caller（非 RT）执行；Publisher / Reader 持 `Arc`
    /// 后 RT 侧不再触发分配或 Arc 操作。
    #[must_use]
    pub fn new(initial: T) -> (Self, RtSnapshotReader<T>) {
        Self::build(initial, 0)
    }

    /// 测试 / 验收专用：以指定的偶数初始版本构造，用于逼近 [`MAX_PUBLISHED_VERSION`]
    /// 验证耗尽出口。生产组合不编译此入口。
    ///
    /// # Errors
    ///
    /// `version` 为奇数或超过 [`MAX_PUBLISHED_VERSION`] 时返回 [`SnapshotVersionError`]。
    #[cfg(any(test, feature = "test-comm-fault-hook"))]
    pub fn new_with_version(
        initial: T,
        version: u64,
    ) -> Result<(Self, RtSnapshotReader<T>), SnapshotVersionError> {
        if version & 1 != 0 || version > MAX_PUBLISHED_VERSION {
            return Err(SnapshotVersionError(version));
        }
        Ok(Self::build(initial, version))
    }

    fn build(initial: T, version: u64) -> (Self, RtSnapshotReader<T>) {
        let mut words = T::Words::zeroed();
        initial.encode(&mut words);
        let inner = Arc::new(Seqlock::new(words.as_words(), version));
        (
            Self {
                inner: Arc::clone(&inner),
                _value: std::marker::PhantomData,
            },
            RtSnapshotReader {
                inner,
                _value: std::marker::PhantomData,
            },
        )
    }

    /// 派生一个新的 reader（在非 RT 场景调用，会 `Arc::clone`）。
    #[must_use]
    pub fn reader(&self) -> RtSnapshotReader<T> {
        RtSnapshotReader {
            inner: Arc::clone(&self.inner),
            _value: std::marker::PhantomData,
        }
    }

    /// 发布新值。
    ///
    /// 栈上 encode → 核偶版本与 `checked_add(2)` → `SeqCst` 存奇版本、全部载荷
    /// 字、偶版本。不分配、不阻塞、不 panic。
    ///
    /// # Errors
    ///
    /// 版本耗尽或写端状态损坏时返回 [`SnapshotPublishError`]，此时**未**发布，
    /// 上一偶版本与载荷完整保留。调用方不得忽略。
    #[inline]
    pub fn publish(&mut self, value: &T) -> Result<(), SnapshotPublishError> {
        let mut words = T::Words::zeroed();
        value.encode(&mut words);
        let current = self.inner.version.load(Ordering::SeqCst);
        if current & 1 != 0 {
            return Err(SnapshotPublishError::VersionCorrupted);
        }
        let Some(next) = current.checked_add(2) else {
            return Err(SnapshotPublishError::VersionExhausted);
        };
        self.inner.version.store(current + 1, Ordering::SeqCst);
        for (slot, word) in self.inner.payload.iter().zip(words.as_words()) {
            slot.store(*word, Ordering::SeqCst);
        }
        self.inner.version.store(next, Ordering::SeqCst);
        Ok(())
    }

    /// 当前版本（偶数 = 稳态）。写端自查用。
    #[inline]
    #[must_use]
    pub fn version(&self) -> u64 {
        self.inner.version.load(Ordering::SeqCst)
    }
}

impl<T: SnapshotValue> RtSnapshotReader<T> {
    /// 读取当前值。
    ///
    /// 栈上固定 `Words`；读偶版本 → 全部载荷字 → 再读版本，一致后 `decode`，
    /// 否则 `spin_loop` 重试。只供非 RT 线程调用；不承诺墙钟上界。
    #[inline]
    #[must_use]
    pub fn load(&self) -> T {
        let mut words = T::Words::zeroed();
        loop {
            let v1 = self.inner.version.load(Ordering::SeqCst);
            if v1 & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            for (word, slot) in words
                .as_words_mut()
                .iter_mut()
                .zip(self.inner.payload.iter())
            {
                *word = slot.load(Ordering::SeqCst);
            }
            let v2 = self.inner.version.load(Ordering::SeqCst);
            if v1 == v2 {
                return T::decode(&words);
            }
            std::hint::spin_loop();
        }
    }

    /// 返回当前版本（偶数 = 稳态；奇数 = writer 正在写）。
    ///
    /// 用于诊断 / 计数"snapshot 未变化"场景（两次 `load` 之间版本未变 = 未更新）。
    #[inline]
    #[must_use]
    pub fn version(&self) -> u64 {
        self.inner.version.load(Ordering::SeqCst)
    }
}

impl<T: SnapshotValue> Clone for RtSnapshotReader<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            _value: std::marker::PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::thread;

    /// 两字载荷：writer 每次同时改两个字段，不变量 a == b。
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Payload {
        a: u64,
        b: u64,
    }

    impl SnapshotValue for Payload {
        type Words = [u64; 2];

        fn encode(&self, words: &mut Self::Words) {
            words[0] = self.a;
            words[1] = self.b;
        }

        fn decode(words: &Self::Words) -> Self {
            Self {
                a: words[0],
                b: words[1],
            }
        }
    }

    #[test]
    fn publish_load_roundtrip_single_thread() {
        let (mut pubr, rdr) = RtSnapshotPublisher::new(0u64);
        assert_eq!(rdr.load(), 0);
        pubr.publish(&42).unwrap();
        assert_eq!(rdr.load(), 42);
        pubr.publish(&100).unwrap();
        assert_eq!(rdr.load(), 100);
    }

    #[test]
    fn version_advances_two_per_publish() {
        let (mut pubr, rdr) = RtSnapshotPublisher::new(0u64);
        assert_eq!(rdr.version(), 0);
        pubr.publish(&1).unwrap();
        assert_eq!(rdr.version(), 2);
        assert_eq!(pubr.version(), 2);
        pubr.publish(&2).unwrap();
        assert_eq!(rdr.version(), 4);
    }

    #[test]
    fn reader_clone_sees_same_data() {
        let (mut pubr, rdr1) = RtSnapshotPublisher::new(0u64);
        let rdr2 = rdr1.clone();
        pubr.publish(&777).unwrap();
        assert_eq!(rdr1.load(), 777);
        assert_eq!(rdr2.load(), 777);
    }

    #[test]
    fn reader_from_publisher_builder() {
        let (mut pubr, _rdr1) = RtSnapshotPublisher::new(0u64);
        let rdr_extra = pubr.reader();
        pubr.publish(&555).unwrap();
        assert_eq!(rdr_extra.load(), 555);
    }

    #[test]
    fn words_are_fixed_length_and_payload_preallocated() {
        assert_eq!(<[u64; 2] as SnapshotWords>::LEN, 2);
        assert_eq!(<[u64; 0] as SnapshotWords>::LEN, 0);
        let (pubr, rdr) = RtSnapshotPublisher::new(Payload { a: 7, b: 7 });
        assert_eq!(pubr.inner.payload.len(), 2);
        assert_eq!(rdr.load(), Payload { a: 7, b: 7 });
    }

    #[test]
    fn version_exhaustion_keeps_last_snapshot_and_never_wraps() {
        let (mut pubr, rdr) = RtSnapshotPublisher::new_with_version(
            Payload { a: 1, b: 1 },
            MAX_PUBLISHED_VERSION - 2,
        )
        .unwrap();
        pubr.publish(&Payload { a: 2, b: 2 }).unwrap();
        assert_eq!(rdr.version(), MAX_PUBLISHED_VERSION);
        assert_eq!(rdr.load(), Payload { a: 2, b: 2 });
        for _ in 0..3 {
            assert_eq!(
                pubr.publish(&Payload { a: 3, b: 3 }),
                Err(SnapshotPublishError::VersionExhausted)
            );
            assert_eq!(rdr.version(), MAX_PUBLISHED_VERSION, "不回绕");
            assert_eq!(rdr.load(), Payload { a: 2, b: 2 }, "上一完整快照保留");
        }
    }

    #[test]
    fn injected_version_must_be_even_and_in_range() {
        assert_eq!(
            RtSnapshotPublisher::new_with_version(0u64, 3).err(),
            Some(SnapshotVersionError(3))
        );
        assert_eq!(
            RtSnapshotPublisher::new_with_version(0u64, u64::MAX).err(),
            Some(SnapshotVersionError(u64::MAX))
        );
        let (pubr, _) = RtSnapshotPublisher::new_with_version(0u64, MAX_PUBLISHED_VERSION).unwrap();
        assert_eq!(pubr.version(), MAX_PUBLISHED_VERSION);
    }

    #[test]
    fn odd_version_at_entry_is_reported_not_written() {
        let (mut pubr, rdr) = RtSnapshotPublisher::new(5u64);
        pubr.inner.version.store(1, Ordering::SeqCst);
        assert_eq!(
            pubr.publish(&6),
            Err(SnapshotPublishError::VersionCorrupted)
        );
        assert_eq!(pubr.inner.payload[0].load(Ordering::SeqCst), 5);
        pubr.inner.version.store(0, Ordering::SeqCst);
        assert_eq!(rdr.load(), 5);
    }

    /// 多读压力：writer 高频 publish + 多个 reader 并发 load；每个读到的值都是
    /// 完整一致的（a == b），writer 结束后所有 reader 看到最终值。
    #[test]
    fn stress_concurrent_readers_see_consistent_values_and_final_value() {
        const PUBLISHES: u64 = 20_000;
        let (mut pubr, rdr) = RtSnapshotPublisher::new(Payload { a: 0, b: 0 });
        let stop = Arc::new(AtomicBool::new(false));

        let pub_handle = thread::spawn(move || {
            for i in 1..=PUBLISHES {
                pubr.publish(&Payload { a: i, b: i }).unwrap();
            }
            pubr
        });

        let mut rdr_handles = Vec::new();
        for _ in 0..3 {
            let rdr = rdr.clone();
            let stop = Arc::clone(&stop);
            rdr_handles.push(thread::spawn(move || {
                let mut observed = 0u64;
                let mut reads = 0u64;
                while !stop.load(Ordering::Relaxed) || reads < 1_000 {
                    let p = rdr.load();
                    assert_eq!(p.a, p.b, "seqlock tear detected: {}, {}", p.a, p.b);
                    assert!(p.a >= observed, "值必须单调不回退：{} < {observed}", p.a);
                    observed = p.a;
                    reads += 1;
                }
                observed
            }));
        }

        let pubr = pub_handle.join().unwrap();
        stop.store(true, Ordering::Relaxed);
        for handle in rdr_handles {
            let _ = handle.join().unwrap();
        }
        assert_eq!(pubr.version(), PUBLISHES * 2);
        assert_eq!(
            rdr.load(),
            Payload {
                a: PUBLISHES,
                b: PUBLISHES
            },
            "writer 完成后的最终值"
        );
    }
}
