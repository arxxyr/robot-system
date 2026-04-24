// robot-core v0.1.2 frozen design
//
// 本 crate 是 robot-system 的内核：trait、错误、基础类型；零 runtime 依赖。
// 详见 docs/00_design.md §4.1 模块地图、§4.2 依赖铁律。
//
// 依赖外 FFI：仅 time / (未来) os 模块允许 import `libc`，登记见
// docs/CHANGELOG.md 2026-04-21 "robot-core §4.2 允许列表扩展"。

#![forbid(unused_must_use)]

pub mod adapter;
pub mod error;
pub mod rt_guard;
#[cfg(target_os = "linux")]
pub mod thread;
pub mod time;
