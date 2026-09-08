//! 测试专用 adapter 包装：注入单次调用结果，不包含收敛或 IPC 状态机。

use std::sync::Arc;
use std::time::Duration;

use robot_core::adapter::{
    AdapterInfo, ControlMode, JointCommandView, JointLimits, JointStateBuf, ManipulatorAdapter,
    SerialRtAdapter,
};
use robot_core::error::RobotError;
use robot_core::executor_ctrl::OutputSafetyProbe;
use robot_core::serial_services::{BusOpClass, CommErrorKind, PreemptProbe};
use robot_core::time::RobotClock;

use crate::comm_fault_hook::{
    RESULT_HARDWARE_FAULT, RESULT_INVALID_PARAMETER, RESULT_SUCCESS, RESULT_TORQUE_ON,
    take_serial_result,
};
use crate::comm_fault_observation::ObservationPage;

pub struct CommFaultAdapter<A> {
    inner: A,
    clock: Arc<dyn RobotClock>,
    probe: Arc<dyn PreemptProbe>,
    observation: Option<Arc<ObservationPage>>,
    ctrl: Arc<crate::hard_rt_ctrl::HardRtCtrlInner>,
    mock_normal_lease: bool,
}

struct MockNormalLease<'a> {
    probe: Option<&'a dyn PreemptProbe>,
    class: BusOpClass,
}

impl<'a> MockNormalLease<'a> {
    fn acquire(
        enabled: bool,
        probe: &'a dyn PreemptProbe,
        class: BusOpClass,
    ) -> Result<Self, RobotError> {
        let probe = if enabled && class.is_normal() {
            probe
                .begin_transaction(class)
                .map_err(RobotError::Aborted)?;
            Some(probe)
        } else {
            None
        };
        Ok(Self { probe, class })
    }
}

impl Drop for MockNormalLease<'_> {
    fn drop(&mut self) {
        if let Some(probe) = self.probe {
            probe.finish_transaction(self.class);
        }
    }
}

impl<A: SerialRtAdapter> CommFaultAdapter<A> {
    pub fn new(
        inner: A,
        clock: Arc<dyn RobotClock>,
        probe: Arc<dyn PreemptProbe>,
        observation: Option<Arc<ObservationPage>>,
        ctrl: Arc<crate::hard_rt_ctrl::HardRtCtrlInner>,
    ) -> Self {
        // 仅补 mock 的真实调用执行权；Feetech 已在每个完整总线事务中持有 lease。
        let mock_normal_lease = inner.info().name == "mock_serial_rt";
        Self {
            inner,
            clock,
            probe,
            observation,
            ctrl,
            mock_normal_lease,
        }
    }

    fn before_call(&self, class: BusOpClass) -> Result<bool, RobotError> {
        if let Some(page) = &self.observation {
            page.record_serial_call(class, self.clock.now().0);
        }
        let Some(script) = take_serial_result(class) else {
            return Ok(false);
        };
        if script.duration_ms != 0 {
            // 故意不发 checkpoint，保留真实调用超期对 liveness 的影响。
            std::thread::sleep(Duration::from_millis(u64::from(script.duration_ms)));
        }
        let result = match script.error {
            RESULT_HARDWARE_FAULT => Err(RobotError::HardwareFault("测试注入设备错误")),
            RESULT_INVALID_PARAMETER => Err(RobotError::InvalidParameter("测试注入参数错误")),
            RESULT_SUCCESS => Ok(false),
            RESULT_TORQUE_ON if class == BusOpClass::Confirm => Ok(true),
            RESULT_TORQUE_ON => Err(RobotError::InvalidParameter("TorqueOn 仅适用于 Confirm")),
            value => Err(RobotError::Comm(CommErrorKind::from_u8(value))),
        };
        self.observe_result(result)
    }

    fn observe_result<T>(&self, result: Result<T, RobotError>) -> Result<T, RobotError> {
        if let Err(RobotError::Comm(kind)) = &result {
            let mut context = self.ctrl.trace_context();
            context.source = self.probe.operation_class() as u64;
            crate::comm_fault_trace::comm_error(self.clock.now().0, context, kind.to_u8());
        }
        result
    }
}

impl<A: SerialRtAdapter> ManipulatorAdapter for CommFaultAdapter<A> {
    fn info(&self) -> AdapterInfo {
        self.inner.info()
    }
    fn connect(&mut self) -> Result<(), RobotError> {
        let _lease = MockNormalLease::acquire(
            self.mock_normal_lease,
            self.probe.as_ref(),
            self.probe.operation_class(),
        )?;
        self.before_call(self.probe.operation_class())?;
        let result = self.inner.connect();
        self.observe_result(result)
    }
    fn disconnect(&mut self) -> Result<(), RobotError> {
        let _lease = MockNormalLease::acquire(
            self.mock_normal_lease,
            self.probe.as_ref(),
            self.probe.operation_class(),
        )?;
        self.before_call(self.probe.operation_class())?;
        let result = self.inner.disconnect();
        self.observe_result(result)
    }
    fn read_state(&mut self, out: &mut JointStateBuf) -> Result<(), RobotError> {
        let _lease = MockNormalLease::acquire(
            self.mock_normal_lease,
            self.probe.as_ref(),
            self.probe.operation_class(),
        )?;
        self.before_call(self.probe.operation_class())?;
        let result = self.inner.read_state(out);
        self.observe_result(result)
    }
    fn write_command(&mut self, command: &JointCommandView<'_>) -> Result<(), RobotError> {
        let _lease = MockNormalLease::acquire(
            self.mock_normal_lease,
            self.probe.as_ref(),
            self.probe.operation_class(),
        )?;
        self.before_call(self.probe.operation_class())?;
        let result = self.inner.write_command(command);
        self.observe_result(result)
    }
    fn set_mode(&mut self, mode: ControlMode) -> Result<(), RobotError> {
        let _lease = MockNormalLease::acquire(
            self.mock_normal_lease,
            self.probe.as_ref(),
            self.probe.operation_class(),
        )?;
        self.before_call(self.probe.operation_class())?;
        let result = self.inner.set_mode(mode);
        self.observe_result(result)
    }
    fn enable(&mut self, enabled: bool) -> Result<(), RobotError> {
        let _lease = MockNormalLease::acquire(
            self.mock_normal_lease,
            self.probe.as_ref(),
            self.probe.operation_class(),
        )?;
        self.before_call(self.probe.operation_class())?;
        let result = self.inner.enable(enabled);
        self.observe_result(result)
    }
    fn safety_stop(&mut self) -> Result<(), RobotError> {
        let _lease = MockNormalLease::acquire(
            self.mock_normal_lease,
            self.probe.as_ref(),
            self.probe.operation_class(),
        )?;
        self.before_call(self.probe.operation_class())?;
        let result = self.inner.safety_stop();
        self.observe_result(result)
    }
    fn limits(&self) -> &JointLimits {
        self.inner.limits()
    }
}

impl<A: SerialRtAdapter> OutputSafetyProbe for CommFaultAdapter<A> {
    fn read_torque_enable_mask(&mut self, out: &mut [bool]) -> Result<(), RobotError> {
        let count = self.inner.joint_count();
        if out.len() < count {
            return Err(RobotError::InvalidParameter("Confirm 缓冲太短"));
        }
        match self.before_call(BusOpClass::Confirm) {
            Ok(true) => {
                out[..count].fill(true);
                Ok(())
            }
            Ok(false) => {
                let result = self.inner.read_torque_enable_mask(out);
                self.observe_result(result)
            }
            Err(error) => {
                if let RobotError::Comm(kind) = error {
                    self.probe.report_comm_error(kind);
                }
                Err(error)
            }
        }
    }
    fn joint_count(&self) -> usize {
        self.inner.joint_count()
    }
}
