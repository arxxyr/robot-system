//! Adapter 静态工厂注册 —— Step 7 task 27b 落地。
//!
//! ## 动机
//!
//! `docs/00_design.md` §4.5 "静态注册" + `CLAUDE.md` "`robot-kinematics-<robot>`
//! 按 feature flag 挂到 `robot-s1`" 的延伸：每种机器人配置（XLeRobot / ATOM01
//! / Piper / 四足 / 人形）需要 link 不同 adapter 子集；`s0_daemon` 启动期按
//! `configs/<robot>.toml` 里 `[[hardware]] adapter = "feetech_sts3215"` 字符串
//! 找对应 factory 实例化。
//!
//! ## 机制
//!
//! - 每个 adapter crate 在 `lib.rs` 写：
//!
//!   ```rust,ignore
//!   struct MyFactory;
//!   impl AdapterFactory for MyFactory { ... }
//!   inventory::submit! { &MyFactory as &dyn AdapterFactory }
//!   ```
//!
//! - `robot-s0` binary `Cargo.toml` 用 feature gate 控制 link 哪些 adapter
//!   crate（matrix 编译，详 task 27b CHANGELOG）。
//! - `inventory` crate 把 link 进来的 `submit!` 项收到一个 array section，
//!   `find_factory(name)` 启动期一次性查表（O(N) 但 N ≤ 10）。
//!
//! ## 零 runtime 开销
//!
//! `inventory::iter` 遍历的是**链接器构造的静态数组**，无 alloc / 无 IO。
//! 启动期一次性查表后，`s0_daemon` 把 `Box<dyn ManipulatorAdapter + Send>` move
//! 进 `spawn_rt_thread`，热路径不再触及 inventory。
//!
//! ## v0.2 范围
//!
//! - 仅 [`AdapterBuildContext`] / [`AdapterFactory`] / `find_factory` /
//!   `registered_factories` API。
//! - `AdapterBuildContext` 携带最小集字段（`instance_name` / `adapter_kind`
//!   / `joint_names`）。`baud` / `address` / `wheel_radius` 等 adapter-specific
//!   字段在 v0.4 真硬件 adapter 落地时扩展，**届时务必保证扩字段不破坏现有
//!   factory 的兼容**（加新字段只改 robot-s0 build 侧 + 新 factory，旧 factory
//!   忽略不读即可）。

#![allow(clippy::doc_markdown)]

use crate::adapter::{HardRtAdapter, ManipulatorAdapter, SerialRtAdapter};
use crate::diag::DiagSink;
use crate::error::RobotError;
use crate::serial_services::LeasedPreemptProbe;
use crate::time::{DurationNs, RobotClock};
use std::sync::Arc;

/// 构造时复制到 adapter 的服务句柄；工厂不持有上下文借用。
pub struct AdapterBuildServices {
    pub clock: Arc<dyn RobotClock>,
    pub preempt: Option<Arc<dyn LeasedPreemptProbe>>,
    pub diag: Option<Arc<dyn DiagSink>>,
    pub call_deadline: DurationNs,
}

impl std::fmt::Debug for AdapterBuildServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdapterBuildServices")
            .field("preempt", &self.preempt.is_some())
            .field("diag", &self.diag.is_some())
            .field("call_deadline", &self.call_deadline)
            .finish_non_exhaustive()
    }
}

/// 启动期 build adapter 的最小上下文。
///
/// 字段都是借用，调用方负责生命周期。`AdapterFactory::build` 不持有 context
/// 引用 —— 仅在调用栈上读 + 拷出需要的字段。
///
/// **字段扩展规则**：每加一个字段都要保证旧 factory 忽略不读不破坏；新 factory
/// 按需消费。任何 *改语义* 的字段调整走 `docs/CHANGELOG.md` 评审。
#[derive(Clone, Copy, Debug)]
pub struct AdapterBuildContext<'a> {
    /// SerialRT 构造期服务；既有 HardRT 工厂可忽略。
    pub services: Option<&'a AdapterBuildServices>,
    /// `[[hardware]] id`，本实例在 robot 内的唯一名（如 `"left_arm"`）。
    pub instance_name: &'a str,
    /// `[[hardware]] adapter`，factory 名（如 `"mock_hard_rt"`）。`AdapterFactory::name`
    /// 必须返回**完全一致**的字符串才能被 `find_factory` 命中。
    pub adapter_kind: &'a str,
    /// `[[hardware]] joints`，按 joint index 顺序对齐 `JointStateBuf.positions`。
    /// 长度 ≤ [`crate::adapter::MAX_JOINTS`]。
    pub joint_names: &'a [String],
    /// `[[hardware]] address`：硬件位置串（如 `/dev/serial/by-id/usb-1a86_...`）。
    /// `mock_*` factory 忽略；`feetech_sts3215` / 真硬件 factory 必填。
    /// Step 7 task 28 加。
    pub address: Option<&'a str>,
    /// `[[hardware]] motor_ids`：每个 joint 对应的 motor ID（按 `joint_names` 顺序
    /// 一一对应）。空数组 = "缺省 1-N（即 joint 0 → ID 1, joint 1 → ID 2, ...）"。
    /// Step 7 task 28 加，给 dimos 移植场景留接口（dimos bus1 用 ID 1-8 / bus2
    /// 用 ID 1-9 含底盘，需要显式指定）。
    pub motor_ids: &'a [u8],
    /// `[[hardware]] calibration_paths`：一条总线的标定 JSON 路径数组（按 dimos
    ///   bus1 = `left_arm.json` + `head.json` 双文件合并语义，或 bus2 =
    ///   `right_arm.json` + `base.json`）。空数组 = "无标定"（`feetech_sts3215`
    ///   factory 会让 adapter 不做 silent clamp）。Step 7 task 29 加。
    ///
    ///   语义：factory 按顺序读所有路径 → 合并成 `motor_name → MotorCalibration`
    ///   map → 按 `joint_names` 顺序取出对应条目组成 `Vec<MotorCalibration>` 传给
    ///   adapter。任一 joint 在 map 中查不到 → `RobotError::InvalidParameter`。
    pub calibration_paths: &'a [String],
}

/// adapter 工厂 trait。每个 adapter crate 实现 + 用 `inventory::submit!` 注册一个
/// 静态实例（典型为 `&'static`）。
///
/// **`Sync + 'static`**：`inventory::iter` 返回 `&'static dyn AdapterFactory`，
/// 多线程并发遍历需要 `Sync`。
pub trait AdapterFactory: Sync + 'static {
    /// factory 唯一名 —— 与 `configs/<robot>.toml` 里 `[[hardware]] adapter` 字符串
    /// 一致才能被启动期匹配。约定 `snake_case`，如 `"mock_hard_rt"` /
    /// `"feetech_sts3215"`。
    fn name(&self) -> &'static str;

    /// 实例化一个 adapter。`ctx` 见 [`AdapterBuildContext`]。
    ///
    /// 实现内部按 `ctx.adapter_kind` 应**双重检查** = `self.name()`（防 caller 传错；
    /// 一般 `find_factory` 已保证一致）。
    ///
    /// **本入口返回的 adapter 不带 [`crate::adapter::RtSafeCtrl`] 编译期
    /// 守门**，仅适合非 RT 路径（SerialRT / SoftRT）。HardRT path 必须调
    /// [`AdapterFactory::build_hard_rt`]。
    ///
    /// # Errors
    ///
    /// - 必填 context 字段缺失 / 不合法 → [`RobotError::InvalidParameter`]
    /// - 资源（串口 / SDK 句柄）打不开 → [`RobotError::HardwareFault`]
    fn build(
        &self,
        ctx: &AdapterBuildContext<'_>,
    ) -> Result<Box<dyn ManipulatorAdapter + Send>, RobotError>;

    /// **task-30 v9 finding #3 修复**：HardRT 专用构造入口。返回的 adapter
    /// **编译期保证**实现 [`crate::adapter::RtSafeCtrl`]（综合 trait
    /// [`HardRtAdapter`]）。
    ///
    /// `s0_daemon` 在 `RtClass::HardRT` 路径**必须**调本方法（而不是
    /// [`AdapterFactory::build`]）；本方法的返回值才能进 `spawn_rt_thread`。
    ///
    /// **default 实现**：返 `Err(RobotError::Unsupported(...))`。SerialRT /
    /// SoftRT factory 留 default 即可（它们不该走 HardRT 路径）；HardRT
    /// factory 必须 override 返综合 trait object。
    ///
    /// # Errors
    ///
    /// - factory 不支持 HardRT 路径 → [`RobotError::Unsupported`]
    /// - 透传 [`AdapterFactory::build`] 的错误。
    fn build_hard_rt(
        &self,
        _ctx: &AdapterBuildContext<'_>,
    ) -> Result<Box<dyn HardRtAdapter + Send>, RobotError> {
        Err(RobotError::Unsupported(
            "factory does not support HardRT path",
        ))
    }

    /// **task-30 v9 / M7 阶段 4 第二步**：SerialRT 专用构造入口。返回的
    /// adapter **编译期保证**实现 [`crate::executor_ctrl::OutputSafetyProbe`]
    /// （综合 trait [`SerialRtAdapter`]）。
    ///
    /// `s0_daemon` 在 `RtClass::SerialRT` 路径**必须**调本方法（而不是
    /// [`AdapterFactory::build`]）；本方法的返回值才能进
    /// `spawn_serial_io_thread`。
    ///
    /// **default 实现**：返 `Err(RobotError::Unsupported(...))`。HardRT /
    /// SoftRT factory 留 default 即可（它们不该走 SerialRT 路径）；SerialRT
    /// factory（如 `FeetechFactory`）必须 override 返综合 trait object。
    ///
    /// # Errors
    ///
    /// - factory 不支持 SerialRT 路径 → [`RobotError::Unsupported`]
    /// - 透传 [`AdapterFactory::build`] 的错误。
    fn build_serial_rt(
        &self,
        _ctx: &AdapterBuildContext<'_>,
    ) -> Result<Box<dyn SerialRtAdapter + Send>, RobotError> {
        Err(RobotError::Unsupported(
            "factory does not support SerialRT path",
        ))
    }
}

// `inventory` 收集所有 `submit!` 进来的 `&dyn AdapterFactory` 静态实例。
//
// 链接到 binary 的每个 adapter crate 各贡献一个 `inventory::submit!{ ... }`
// item；`iter::<&dyn AdapterFactory>()` 遍历全部。
inventory::collect!(&'static dyn AdapterFactory);

/// 全部已注册的 factory 迭代器。
///
/// 启动期 `s0_daemon` 用一次（按 TOML adapter 名查），之后不再调用。
///
/// `inventory::iter::<T>` 在 inventory 0.3 是 *struct*（lowercase），通过 deref /
/// `into_iter()` 拿迭代器；这里包装成 `Copied<...>` 把外层 `&` 去掉，让调用方
/// 直接拿 `&'static dyn AdapterFactory`。
pub fn registered_factories() -> impl Iterator<Item = &'static dyn AdapterFactory> {
    inventory::iter::<&'static dyn AdapterFactory>
        .into_iter()
        .copied()
}

/// 按 `name` 查 factory。返回 `None` 表示该 adapter 类型未在当前 binary
/// link（feature 没 enable）或拼写错。
#[must_use]
pub fn find_factory(name: &str) -> Option<&'static dyn AdapterFactory> {
    registered_factories().find(|f| f.name() == name)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{
        AdapterInfo, AdapterKind, ControlMode, JointCommandView, JointLimits, JointStateBuf,
        RtClass,
    };

    // 一个本测试模块内自用的占位 adapter + factory，验 inventory 路径。
    struct StubAdapter;

    impl ManipulatorAdapter for StubAdapter {
        fn info(&self) -> AdapterInfo {
            AdapterInfo {
                name: "stub_adapter",
                kind: AdapterKind::Manipulator,
                rt_class: RtClass::HardRT,
                vendor_sdk: false,
            }
        }
        fn connect(&mut self) -> Result<(), RobotError> {
            Ok(())
        }
        fn disconnect(&mut self) -> Result<(), RobotError> {
            Ok(())
        }
        fn read_state(&mut self, _: &mut JointStateBuf) -> Result<(), RobotError> {
            Ok(())
        }
        fn write_command(&mut self, _: &JointCommandView) -> Result<(), RobotError> {
            Ok(())
        }
        fn set_mode(&mut self, _: ControlMode) -> Result<(), RobotError> {
            Ok(())
        }
        fn enable(&mut self, _: bool) -> Result<(), RobotError> {
            Ok(())
        }
        fn safety_stop(&mut self) -> Result<(), RobotError> {
            Ok(())
        }
        fn limits(&self) -> &JointLimits {
            const L: JointLimits = JointLimits {
                position_min: [0.0; crate::adapter::MAX_JOINTS],
                position_max: [0.0; crate::adapter::MAX_JOINTS],
                velocity_max: [0.0; crate::adapter::MAX_JOINTS],
                torque_max: [0.0; crate::adapter::MAX_JOINTS],
            };
            &L
        }
    }

    struct StubFactory;
    impl AdapterFactory for StubFactory {
        fn name(&self) -> &'static str {
            "robot_core_test_stub"
        }
        fn build(
            &self,
            _ctx: &AdapterBuildContext<'_>,
        ) -> Result<Box<dyn ManipulatorAdapter + Send>, RobotError> {
            Ok(Box::new(StubAdapter))
        }
    }
    inventory::submit! { &StubFactory as &dyn AdapterFactory }

    #[test]
    fn stub_factory_self_registered_and_findable() {
        let f = find_factory("robot_core_test_stub").expect("stub factory not registered");
        assert_eq!(f.name(), "robot_core_test_stub");
        let ctx = AdapterBuildContext {
            services: None,
            instance_name: "test",
            adapter_kind: "robot_core_test_stub",
            joint_names: &[],
            address: None,
            motor_ids: &[],
            calibration_paths: &[],
        };
        let adapter = f.build(&ctx).unwrap();
        assert_eq!(adapter.info().name, "stub_adapter");
    }

    #[test]
    fn unknown_factory_returns_none() {
        assert!(find_factory("definitely_not_registered_xyz").is_none());
    }

    #[test]
    fn registered_factories_includes_stub() {
        let names: Vec<&str> = registered_factories().map(AdapterFactory::name).collect();
        assert!(
            names.contains(&"robot_core_test_stub"),
            "registered_factories 应包含 stub: got {names:?}"
        );
    }

    #[test]
    fn build_context_passes_through_fields() {
        let names = vec!["a".to_string(), "b".to_string()];
        let ids = [3u8, 7];
        let cal_paths = vec![
            "/tmp/left_arm.json".to_string(),
            "/tmp/head.json".to_string(),
        ];
        let ctx = AdapterBuildContext {
            services: None,
            instance_name: "left_arm",
            adapter_kind: "robot_core_test_stub",
            joint_names: &names,
            address: Some("/dev/serial/by-id/usb-test"),
            motor_ids: &ids,
            calibration_paths: &cal_paths,
        };
        assert_eq!(ctx.instance_name, "left_arm");
        assert_eq!(ctx.adapter_kind, "robot_core_test_stub");
        assert_eq!(ctx.joint_names.len(), 2);
        assert_eq!(ctx.address, Some("/dev/serial/by-id/usb-test"));
        assert_eq!(ctx.motor_ids, &[3u8, 7]);
        assert_eq!(ctx.calibration_paths.len(), 2);
        assert_eq!(ctx.calibration_paths[0], "/tmp/left_arm.json");
    }
}
