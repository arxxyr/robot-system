//! # robot-s1
//!
//! S1 快反射层。v0.2 Part 2 scope 内仅含 [`mock_s1`](crate::bin::mock_s1) 二进制 ——
//! 发 `JointCommand` + `Heartbeat` 给 S0 做最小闭环验证。v0.3 起扩成真 coordinator
//! （trajectory 调度、IK 分发、`CommandMeta` 幂等分流）。

#![forbid(unused_must_use)]

// 本 crate 暂只有 bin，没有 lib API。保留空 lib.rs 是为了让 `cargo test -p robot-s1`
// 能跑（无 [lib] 的 crate 单独测试不便）。未来 v0.3 起会在这里放 coordinator 的 trait
// + 纯逻辑；bin 变成门面。
