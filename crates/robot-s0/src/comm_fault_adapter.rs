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
}

impl<A> CommFaultAdapter<A> {
    pub fn new(
        inner: A,
        clock: Arc<dyn RobotClock>,
        probe: Arc<dyn PreemptProbe>,
        observation: Option<Arc<ObservationPage>>,
    ) -> Self {
        Self {
            inner,
            clock,
            probe,
            observation,
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
        match script.error {
            RESULT_HARDWARE_FAULT => Err(RobotError::HardwareFault("测试注入设备错误")),
            RESULT_INVALID_PARAMETER => Err(RobotError::InvalidParameter("测试注入参数错误")),
            RESULT_SUCCESS => Ok(false),
            RESULT_TORQUE_ON if class == BusOpClass::Confirm => Ok(true),
            RESULT_TORQUE_ON => Err(RobotError::InvalidParameter("TorqueOn 仅适用于 Confirm")),
            value => Err(RobotError::Comm(CommErrorKind::from_u8(value))),
        }
    }
}

impl<A: SerialRtAdapter> ManipulatorAdapter for CommFaultAdapter<A> {
    fn info(&self) -> AdapterInfo {
        self.inner.info()
    }
    fn connect(&mut self) -> Result<(), RobotError> {
        self.before_call(self.probe.operation_class())?;
        self.inner.connect()
    }
    fn disconnect(&mut self) -> Result<(), RobotError> {
        self.before_call(self.probe.operation_class())?;
        self.inner.disconnect()
    }
    fn read_state(&mut self, out: &mut JointStateBuf) -> Result<(), RobotError> {
        self.before_call(self.probe.operation_class())?;
        self.inner.read_state(out)
    }
    fn write_command(&mut self, command: &JointCommandView<'_>) -> Result<(), RobotError> {
        self.before_call(self.probe.operation_class())?;
        self.inner.write_command(command)
    }
    fn set_mode(&mut self, mode: ControlMode) -> Result<(), RobotError> {
        self.before_call(self.probe.operation_class())?;
        self.inner.set_mode(mode)
    }
    fn enable(&mut self, enabled: bool) -> Result<(), RobotError> {
        self.before_call(self.probe.operation_class())?;
        self.inner.enable(enabled)
    }
    fn safety_stop(&mut self) -> Result<(), RobotError> {
        self.before_call(self.probe.operation_class())?;
        self.inner.safety_stop()
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
            Ok(false) => self.inner.read_torque_enable_mask(out),
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
