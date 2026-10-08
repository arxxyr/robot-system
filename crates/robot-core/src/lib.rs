// robot-core v0.1.2 frozen design
//
// 本 crate 是 robot-system 的内核：trait、错误、基础类型；零 runtime 依赖。
// 详见 docs/00_design.md §4.1 模块地图、§4.2 依赖铁律。
//
// 依赖外 FFI：仅 time / (未来) os 模块允许 import `libc`，登记见
// docs/CHANGELOG.md 2026-04-21 "robot-core §4.2 允许列表扩展"。

#![forbid(unused_must_use)]

pub mod adapter;
pub mod config;
// S48 A3a：纯权限原语（原 robot-s0::control_authority）归位；robot-s0 重导出同一模块。
pub mod control_authority;
pub mod diag;
pub mod error;
pub mod executor_ctrl;
pub mod factory;
pub mod rt_guard;
pub mod serial_services;
#[cfg(target_os = "linux")]
pub mod thread;
pub mod time;
pub mod wheel_velocity_profile;

// 只为本 crate 的 RT 禁分配测试安装真实拦截器，不改变生产 allocator。
#[cfg(all(test, feature = "rt_guard"))]
#[global_allocator]
static ALLOC_GUARD: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
