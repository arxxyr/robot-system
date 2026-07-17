//! `robot-tests` 共享 harness helper（v0.5 候选 E 段(2) 回修 P0-1 首立）。
//!
//! 本 crate 此前 bins-only；六个 smoke / e2e harness（crash_restart /
//! scripted / ik / pubsub_cleanup / clear_estop_e2e / robotctl_smoke）是
//! **"静态样板 config + 动态唯一 robot_id"** 消费面（Spec P.4 登记——Plan
//! 曾误写为动态 builder），段(2) 身份锁（`[robot].id` 必须 == `--robot-id`）
//! 要求它们改为 per-case 物化 config。共享入口统一在
//! [`config_fixture`]，禁止六份字符串替换各写各的。

// 中文 doc 块大量 identifier-like 字串（robot_id / config / smoke bin 名），
// 跟本 crate 各 bin 同款宽松 doc-markdown。
#![allow(clippy::doc_markdown)]

pub mod config_fixture;
