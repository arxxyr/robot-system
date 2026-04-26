//! In-process TTL idempotency cache（A1-1 (3/3) 范围）。
//!
//! ## 安全边界（reviewer A1 第 3 个拍板硬边界 + A1-1 (3/3) round 2 阻断）
//!
//! - **进程内**：仅 `robot-rpc` 进程持有；**不**持久化（A1 不解决 RPC
//!   重启重放问题，留 v0.5+ 完整 gateway 时再决定要不要持久化）。
//! - **cache key = `meta.idempotency_key`**：**不**用 `request_id`
//!   （`request_id` 每次重试改变，仅 audit；`idempotency_key` 跨重试稳定，
//!   是真正的去重 key）。
//! - **指纹比较走 [`Fingerprint`]**（reviewer A1-1 (3/3) round 2 阻断 1
//!   修正）：**不**直接 hash request payload bytes —— 那样会把
//!   `request_id` 等"每次重试可变"的 audit 字段算进 fingerprint，导致同
//!   `idempotency_key` + 新 `request_id` 的合法 retry 误走
//!   [`Decision::Conflict`]。**调用方负责构造 fingerprint**：把语义字段
//!   （`source` / `confirmation` / `audit_reason` / `dry_run` /
//!   `auth_token` 等）合成稳定 byte sequence 再 hash；显式排除 audit-only
//!   / 每次重试可变字段（`request_id` / `timeout_ms`）。`clear_estop`
//!   模块提供 [`crate::clear_estop::clear_estop_idempotency_fingerprint`]
//!   helper。
//! - **同 key 不同 fingerprint 必须拒绝**：禁止复用错误请求的 cached Ack。
//!   cache 只记 fingerprint 不存原 payload bytes。
//! - **TTL 过期清理**：`decide` 每次调用顺手清过期项；不起后台清理线程
//!   （A1 简化）。
//! - **首次插入 vs 命中**：首次插入返 `Decision::FreshInsert(handle)`；命
//!   中且 fingerprint 匹配返 `Decision::Replay(cached_ack)`；命中但
//!   fingerprint 不匹配返
//!   `Decision::Conflict { cached_fingerprint, incoming_fingerprint }`。

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 32-byte 指纹（一般是 blake3-256 hash）。**调用方**负责从语义字段
/// （**排除** `request_id` / `timeout_ms` 等 audit-only / 每次重试可变
/// 字段）合成稳定 byte sequence 再 hash 出来。详 [`crate::clear_estop::
/// clear_estop_idempotency_fingerprint`] 一类 helper。
pub type Fingerprint = [u8; 32];

/// Cache 决策。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 第一次见到此 `idempotency_key`：调用方拿到 [`InsertHandle`]，必须在
    /// 处理完后调用 `commit(ack)` 写入 cached ack；如果调用方处理过程中
    /// drop handle（panic / 提前 return），下一次同 key 请求依然是
    /// `FreshInsert`（cache 没记本次的 ack）。
    FreshInsert(InsertHandle),
    /// 命中且 fingerprint 匹配：可以直接返 cached ack（含 dry_run 状态、
    /// cleared_at_ns、tonic Status 等）。
    Replay(CachedAck),
    /// 命中但 fingerprint 不匹配：reviewer 硬边界 — 拒绝复用，调用方应
    /// 返 `tonic::Code::AlreadyExists` 让客户端发现 key 冲突。
    Conflict {
        cached_fingerprint: Fingerprint,
        incoming_fingerprint: Fingerprint,
    },
}

/// `Decision::FreshInsert` 内部 handle —— 调用方处理完 RPC 后必须 commit
/// 一次，否则 cache 无记录（同 key 下次请求当 fresh）。
///
/// 类型只携带 cache key + fingerprint + TTL；commit 时写入 ack + 时间戳。
/// **手动 commit / 手动 abandon**：不实现 Drop，让调用方明示意图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertHandle {
    key: String,
    fingerprint: Fingerprint,
}

impl InsertHandle {
    /// 调用方处理完后 commit cached ack。下一次同 key 请求 → `Replay`。
    pub fn commit(self, cache: &mut IdempotencyCache, ack: CachedAck) {
        let now = Instant::now();
        cache.entries.insert(
            self.key,
            CacheEntry {
                fingerprint: self.fingerprint,
                ack,
                inserted_at: now,
            },
        );
    }
}

/// 缓存的 ack 内容。**不**含 request_id（每次重试 request_id 不同；客户
/// 端 correlation 走自己的 request_id，不依赖 cache）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedAck {
    /// 跟 `proto::Ack.cleared_at_ns` 同语义。
    pub cleared_at_ns: u64,
    /// 跟 `proto::Ack.dry_run` 同语义。
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    fingerprint: Fingerprint,
    ack: CachedAck,
    inserted_at: Instant,
}

/// In-process TTL idempotency cache。
#[derive(Debug)]
pub struct IdempotencyCache {
    entries: HashMap<String, CacheEntry>,
    ttl: Duration,
}

impl IdempotencyCache {
    /// 用给定 TTL 创建 cache。
    ///
    /// **典型 TTL**：5 分钟（A1 默认；客户端重试间隔通常 ≤ 30s，5 分钟
    /// 远大于 reasonable retry burst）。
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            ttl,
        }
    }

    /// 当前 cache 大小（用于诊断 / 测试）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 查 cache 决策 —— 顺手清过期项（lazy GC）。
    ///
    /// **调用方负责构造 [`Fingerprint`]**（reviewer A1-1 (3/3) round 2 阻
    /// 断 1 修正）：本函数**不**自己 hash payload bytes，避免把
    /// `request_id` 等"每次重试可变"字段算进 fingerprint 误杀合法 retry。
    /// 详 module doc 安全边界第 3 条 + [`crate::clear_estop::
    /// clear_estop_idempotency_fingerprint`]。
    ///
    /// 调用方需要持有 `&mut` 因为 GC 可能写表（过期清理）。
    pub fn decide(&mut self, key: &str, fingerprint: Fingerprint) -> Decision {
        self.gc_expired(Instant::now());
        match self.entries.get(key) {
            Some(entry) => {
                if entry.fingerprint == fingerprint {
                    Decision::Replay(entry.ack.clone())
                } else {
                    Decision::Conflict {
                        cached_fingerprint: entry.fingerprint,
                        incoming_fingerprint: fingerprint,
                    }
                }
            }
            None => Decision::FreshInsert(InsertHandle {
                key: key.to_string(),
                fingerprint,
            }),
        }
    }

    /// 清理过期项（lazy GC，[`decide`](Self::decide) 内部调用）。`now`
    /// 显式传入方便测试用 fake 时间。
    fn gc_expired(&mut self, now: Instant) {
        let ttl = self.ttl;
        self.entries
            .retain(|_, e| now.duration_since(e.inserted_at) < ttl);
    }

    #[cfg(test)]
    fn force_age(&mut self, key: &str, age: Duration) {
        if let Some(e) = self.entries.get_mut(key) {
            // 把 inserted_at 往前推 `age`，模拟过期。
            e.inserted_at -= age;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用 fingerprint 工厂：blake3 hash 任意 byte slice。**不**走
    /// production code path —— production 走 [`crate::clear_estop::
    /// clear_estop_idempotency_fingerprint`] 之类的 helper（排除
    /// `request_id` / `timeout_ms` 等 audit-only / 重试可变字段）。
    fn test_fp(bytes: &[u8]) -> Fingerprint {
        blake3::hash(bytes).into()
    }

    #[test]
    fn fresh_insert_then_commit_then_replay() {
        let mut cache = IdempotencyCache::new(Duration::from_mins(1));
        let fp = test_fp(b"req-payload-1");
        let key = "idem-key-1";

        let dec = cache.decide(key, fp);
        let handle = match dec {
            Decision::FreshInsert(h) => h,
            other => panic!("expected FreshInsert, got {other:?}"),
        };
        handle.commit(
            &mut cache,
            CachedAck {
                cleared_at_ns: 1_234_567_890,
                dry_run: false,
            },
        );

        let dec2 = cache.decide(key, fp);
        match dec2 {
            Decision::Replay(ack) => {
                assert_eq!(ack.cleared_at_ns, 1_234_567_890);
                assert!(!ack.dry_run);
            }
            other => panic!("expected Replay, got {other:?}"),
        }
    }

    #[test]
    fn handle_dropped_without_commit_means_next_call_is_still_fresh() {
        let mut cache = IdempotencyCache::new(Duration::from_mins(1));
        let key = "idem-key-2";
        let fp = test_fp(b"payload-a");
        let handle = cache.decide(key, fp);
        drop(handle);
        assert!(cache.is_empty());
        match cache.decide(key, fp) {
            Decision::FreshInsert(_) => {}
            other => panic!("expected FreshInsert (handle dropped), got {other:?}"),
        }
    }

    #[test]
    fn same_key_different_fingerprint_returns_conflict() {
        let mut cache = IdempotencyCache::new(Duration::from_mins(1));
        let key = "idem-key-3";
        let fp_a = test_fp(b"payload-A");
        match cache.decide(key, fp_a) {
            Decision::FreshInsert(h) => h.commit(
                &mut cache,
                CachedAck {
                    cleared_at_ns: 100,
                    dry_run: false,
                },
            ),
            other => panic!("first call expected FreshInsert, got {other:?}"),
        }

        let fp_b = test_fp(b"payload-B");
        match cache.decide(key, fp_b) {
            Decision::Conflict {
                cached_fingerprint,
                incoming_fingerprint,
            } => {
                assert_eq!(cached_fingerprint, fp_a);
                assert_eq!(incoming_fingerprint, fp_b);
                assert_ne!(cached_fingerprint, incoming_fingerprint);
            }
            other => {
                panic!("expected Conflict for same key + different fingerprint, got {other:?}")
            }
        }
    }

    /// **回归锁（reviewer A1-1 (3/3) round 2 阻断 1）**：cache 必须把"同
    /// `idempotency_key` + 同 `Fingerprint`"重试当 `Replay`，即使每次重试
    /// 客户端生成了新的 `request_id`（UUIDv7）+ 新 `timeout_ms`。fingerprint
    /// helper 排除那些 audit-only / 重试可变字段；cache 只看 fingerprint。
    ///
    /// 本测试从 cache 视角锁住 contract："只要调用方给同一 fingerprint，
    /// cache 必须 Replay；不能误走 Conflict"。`clear_estop` module 单测从
    /// helper 视角锁住"request_id 变 / timeout_ms 变 → fingerprint 不变"。
    /// 两端共同保证合法 retry 不被误杀。
    #[test]
    fn same_key_same_fingerprint_replays_across_retries() {
        let mut cache = IdempotencyCache::new(Duration::from_mins(1));
        let key = "retry-key";
        let canonical_fp = test_fp(b"canonical-payload-without-request-id-or-timeout");

        match cache.decide(key, canonical_fp) {
            Decision::FreshInsert(h) => h.commit(
                &mut cache,
                CachedAck {
                    cleared_at_ns: 12_345,
                    dry_run: false,
                },
            ),
            other => panic!("attempt 1 expected FreshInsert, got {other:?}"),
        }

        match cache.decide(key, canonical_fp) {
            Decision::Replay(ack) => {
                assert_eq!(ack.cleared_at_ns, 12_345, "retry must replay cached ack");
                assert!(!ack.dry_run);
            }
            Decision::Conflict { .. } => panic!(
                "REGRESSION: same idempotency_key + same canonical fingerprint must \
                 replay; got Conflict — fingerprint contract violated, cache 误杀合 \
                 法 retry（reviewer A1-1 (3/3) round 2 阻断 1）"
            ),
            Decision::FreshInsert(_) => {
                panic!("attempt 2 expected Replay, got FreshInsert (cache lost first commit)")
            }
        }
    }

    #[test]
    fn ttl_expiry_drops_entry() {
        let mut cache = IdempotencyCache::new(Duration::from_millis(50));
        let key = "idem-key-4";
        let fp = test_fp(b"payload");

        match cache.decide(key, fp) {
            Decision::FreshInsert(h) => h.commit(
                &mut cache,
                CachedAck {
                    cleared_at_ns: 0,
                    dry_run: true,
                },
            ),
            other => panic!("expected FreshInsert, got {other:?}"),
        }
        assert_eq!(cache.len(), 1);

        cache.force_age(key, Duration::from_millis(200));

        match cache.decide(key, fp) {
            Decision::FreshInsert(_) => {}
            other => panic!("expected FreshInsert after TTL expiry, got {other:?}"),
        }
        assert!(cache.is_empty());
    }

    /// 同 key + 同 fingerprint 但 cached ack 是 dry_run=true，重放时必须
    /// 回放 dry_run=true（client 不能误以为真清了 latch）。
    #[test]
    fn replay_preserves_dry_run_flag() {
        let mut cache = IdempotencyCache::new(Duration::from_mins(1));
        let key = "dry-run-replay-key";
        let fp = test_fp(b"dry-run-payload");

        match cache.decide(key, fp) {
            Decision::FreshInsert(h) => h.commit(
                &mut cache,
                CachedAck {
                    cleared_at_ns: 999,
                    dry_run: true,
                },
            ),
            other => panic!("expected FreshInsert, got {other:?}"),
        }
        match cache.decide(key, fp) {
            Decision::Replay(ack) => {
                assert_eq!(ack.cleared_at_ns, 999);
                assert!(ack.dry_run, "dry_run must be preserved across replay");
            }
            other => panic!("expected Replay, got {other:?}"),
        }
    }

    #[test]
    fn empty_fingerprint_works_consistently() {
        let mut cache = IdempotencyCache::new(Duration::from_mins(1));
        let key = "empty-fp-key";
        let fp = test_fp(b"");
        match cache.decide(key, fp) {
            Decision::FreshInsert(h) => h.commit(
                &mut cache,
                CachedAck {
                    cleared_at_ns: 0,
                    dry_run: false,
                },
            ),
            other => panic!("expected FreshInsert, got {other:?}"),
        }
        match cache.decide(key, fp) {
            Decision::Replay(_) => {}
            other => panic!("expected Replay, got {other:?}"),
        }
    }
}
