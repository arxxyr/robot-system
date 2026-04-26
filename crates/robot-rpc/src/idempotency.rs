//! In-process TTL idempotency cache（A1-1 (3/3) 范围）。
//!
//! ## 安全边界（reviewer A1 第 3 个拍板硬边界）
//!
//! - **进程内**：仅 `robot-rpc` 进程持有；**不**持久化（A1 不解决 RPC
//!   重启重放问题，留 v0.5+ 完整 gateway 时再决定要不要持久化）。
//! - **cache key = `meta.idempotency_key`**：**不**用 `request_id`
//!   （`request_id` 每次重试改变，仅 audit；`idempotency_key` 跨重试稳定，
//!   是真正的去重 key）。
//! - **同 key 不同 payload 必须拒绝**：禁止复用错误请求的 cached Ack。
//!   payload 比较走 hash（blake3），cache 只记 hash 不存 payload bytes。
//! - **TTL 过期清理**：`get_or_insert` 每次调用顺手清过期项；不起后台清
//!   理线程（A1 简化）。
//! - **首次插入 vs 命中**：首次插入返 `Decision::FreshInsert(handle)`；命
//!   中且 hash 匹配返 `Decision::Replay(cached_ack)`；命中但 hash 不匹配
//!   返 `Decision::Conflict { cached_payload_hash, incoming_payload_hash }`。

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Cache 决策。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 第一次见到此 `idempotency_key`：调用方拿到 [`InsertHandle`]，必须在
    /// 处理完后调用 `commit(ack)` 写入 cached ack；如果调用方处理过程中
    /// drop handle（panic / 提前 return），下一次同 key 请求依然是
    /// `FreshInsert`（cache 没记本次的 ack）。
    FreshInsert(InsertHandle),
    /// 命中且 payload hash 匹配：可以直接返 cached ack（含 dry_run 状态、
    /// cleared_at_ns、tonic Status 等）。
    Replay(CachedAck),
    /// 命中但 payload hash 不匹配：reviewer 硬边界 — 拒绝复用，返
    /// `tonic::Code::AlreadyExists` 让客户端发现 key 冲突。
    Conflict {
        cached_payload_hash: [u8; 32],
        incoming_payload_hash: [u8; 32],
    },
}

/// `Decision::FreshInsert` 内部 handle —— 调用方处理完 RPC 后必须 commit
/// 一次，否则 cache 无记录（同 key 下次请求当 fresh）。
///
/// 类型只携带 cache key + payload hash + TTL；commit 时写入 ack + 时间戳。
/// **手动 commit / 手动 abandon**：不实现 Drop，让调用方明示意图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertHandle {
    key: String,
    payload_hash: [u8; 32],
}

impl InsertHandle {
    /// 调用方处理完后 commit cached ack。下一次同 key 请求 → `Replay`。
    pub fn commit(self, cache: &mut IdempotencyCache, ack: CachedAck) {
        let now = Instant::now();
        cache.entries.insert(
            self.key,
            CacheEntry {
                payload_hash: self.payload_hash,
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
    payload_hash: [u8; 32],
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
    /// `payload_bytes` 是**完整** request payload（用于 hash 比较；A1 用
    /// blake3-256 hash）。
    ///
    /// 调用方需要持有 `&mut` 因为 GC 可能写表（写过期清理）。
    pub fn decide(&mut self, key: &str, payload_bytes: &[u8]) -> Decision {
        self.gc_expired(Instant::now());
        let incoming_hash = blake3::hash(payload_bytes).into();
        match self.entries.get(key) {
            Some(entry) => {
                if entry.payload_hash == incoming_hash {
                    Decision::Replay(entry.ack.clone())
                } else {
                    Decision::Conflict {
                        cached_payload_hash: entry.payload_hash,
                        incoming_payload_hash: incoming_hash,
                    }
                }
            }
            None => Decision::FreshInsert(InsertHandle {
                key: key.to_string(),
                payload_hash: incoming_hash,
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

    #[test]
    fn fresh_insert_then_commit_then_replay() {
        let mut cache = IdempotencyCache::new(Duration::from_mins(1));
        let payload = b"req-payload-1";
        let key = "idem-key-1";

        // 第一次：FreshInsert
        let dec = cache.decide(key, payload);
        let handle = match dec {
            Decision::FreshInsert(h) => h,
            other => panic!("expected FreshInsert, got {other:?}"),
        };

        // 调用方处理完后 commit
        handle.commit(
            &mut cache,
            CachedAck {
                cleared_at_ns: 1_234_567_890,
                dry_run: false,
            },
        );

        // 第二次：Replay
        let dec2 = cache.decide(key, payload);
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
        // 第一次拿 handle 不 commit（模拟 RPC handler panic / 提前 return）
        let handle = cache.decide(key, b"payload-a");
        drop(handle);
        assert!(cache.is_empty());

        // 第二次：仍然是 FreshInsert
        match cache.decide(key, b"payload-a") {
            Decision::FreshInsert(_) => {}
            other => panic!("expected FreshInsert (handle dropped), got {other:?}"),
        }
    }

    #[test]
    fn same_key_different_payload_returns_conflict() {
        let mut cache = IdempotencyCache::new(Duration::from_mins(1));
        let key = "idem-key-3";

        // 第一次：FreshInsert + commit payload "A"
        match cache.decide(key, b"payload-A") {
            Decision::FreshInsert(h) => h.commit(
                &mut cache,
                CachedAck {
                    cleared_at_ns: 100,
                    dry_run: false,
                },
            ),
            other => panic!("first call expected FreshInsert, got {other:?}"),
        }

        // 第二次同 key 但 payload "B"：必须 Conflict（reviewer 硬边界，**不**
        // 复用错误请求的 cached ack）
        match cache.decide(key, b"payload-B") {
            Decision::Conflict {
                cached_payload_hash,
                incoming_payload_hash,
            } => {
                assert_ne!(cached_payload_hash, incoming_payload_hash);
            }
            other => panic!("expected Conflict for same key + different payload, got {other:?}"),
        }
    }

    #[test]
    fn ttl_expiry_drops_entry() {
        let mut cache = IdempotencyCache::new(Duration::from_millis(50));
        let key = "idem-key-4";

        match cache.decide(key, b"payload") {
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

        // 强制把 entry 老化超过 TTL
        cache.force_age(key, Duration::from_millis(200));

        // 再 decide → 应该清掉旧的，给 FreshInsert
        match cache.decide(key, b"payload") {
            Decision::FreshInsert(_) => {}
            other => panic!("expected FreshInsert after TTL expiry, got {other:?}"),
        }
        // GC 应该跑过：除了刚塞进去的 0 个 commit，cache 应该是空的
        assert!(cache.is_empty());
    }

    /// 边界：同 key 同 payload 但 dry_run cached_ack 是 dry_run=true，重
    /// 放时**必须**回放 dry_run=true（client 不能误以为真清了 latch）。
    #[test]
    fn replay_preserves_dry_run_flag() {
        let mut cache = IdempotencyCache::new(Duration::from_mins(1));
        let key = "dry-run-replay-key";
        let payload = b"dry-run-payload";

        match cache.decide(key, payload) {
            Decision::FreshInsert(h) => h.commit(
                &mut cache,
                CachedAck {
                    cleared_at_ns: 999,
                    dry_run: true,
                },
            ),
            other => panic!("expected FreshInsert, got {other:?}"),
        }
        match cache.decide(key, payload) {
            Decision::Replay(ack) => {
                assert_eq!(ack.cleared_at_ns, 999);
                assert!(ack.dry_run, "dry_run must be preserved across replay");
            }
            other => panic!("expected Replay, got {other:?}"),
        }
    }

    #[test]
    fn empty_payload_works_consistently() {
        let mut cache = IdempotencyCache::new(Duration::from_mins(1));
        let key = "empty-payload-key";
        match cache.decide(key, b"") {
            Decision::FreshInsert(h) => h.commit(
                &mut cache,
                CachedAck {
                    cleared_at_ns: 0,
                    dry_run: false,
                },
            ),
            other => panic!("expected FreshInsert, got {other:?}"),
        }
        match cache.decide(key, b"") {
            Decision::Replay(_) => {}
            other => panic!("expected Replay, got {other:?}"),
        }
    }
}
