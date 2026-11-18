//! 模拟底盘的连接所有权与短租期；仅新浏览器消息转发一次，不在服务端定时续租。

use std::sync::atomic::{AtomicBool, Ordering};

use super::{
    ActionKindProto, AuthContext, BaseHold, BaseView, BusView, CancelTaskCommand, CommandMeta,
    ControlState, InvokeTaskCommand, Operation, Reply, Request, RpcContext, RpcReply, Source,
    TaskView, WheelsRaw, accepted_message, definitely_rejected,
};
use robot_core::wheel_velocity_profile::COMMAND_TTL_MAX_NS;

pub(in crate::status_web) fn wheel_slots(source: &Source) -> Result<[usize; 3], String> {
    if !source.mixed_raw || source.layout.joints.len() != 9 {
        return Err("模拟底盘仅接受现有九槽混合总线".into());
    }
    let mut slots = [0; 3];
    for (index, name) in ["base_front_wheel", "base_left_wheel", "base_right_wheel"]
        .iter()
        .enumerate()
    {
        let slot = source
            .layout
            .joints
            .iter()
            .position(|joint| joint == name)
            .ok_or("配置缺少前、左、右轮角色")?;
        if !(7..=9).contains(&source.layout.motor_ids[slot]) {
            return Err("配置轮角色必须对应原混合轮电机".into());
        }
        slots[index] = slot;
    }
    Ok(slots)
}

fn ready(source: &Source, state: &BusView) -> Result<Vec<f64>, String> {
    if !cfg!(feature = "test-base-command") || !source.base_simulation {
        return Err("底盘模拟控制未开启".into());
    }
    wheel_slots(source)?;
    if state.data_status != "fresh"
        || state.fault_code != Some(0)
        || state.control_link.as_deref() != Some("Ok")
        || state.mode.as_deref() != Some("Enabled")
        || state.safety_state.as_deref() != Some("TorqueOn")
        || state.output_mode.as_deref() != Some("Active")
    {
        return Err("底盘要求新鲜、无故障、已使能且确认上扭的活动反馈".into());
    }
    let positions: Vec<_> = state
        .joints
        .iter()
        .map(|joint| {
            if (7..=9).contains(&joint.motor_id) {
                Some(0.0)
            } else {
                joint.position_rad
            }
        })
        .collect::<Option<_>>()
        .ok_or("底盘缺少右臂完整原位反馈")?;
    source
        .motion
        .as_ref()
        .ok_or("底盘缺少原位保持约束")?
        .check_hold_positions(source, &positions)?;
    Ok(positions)
}

impl ControlState {
    pub(super) fn close_base_for(&mut self, operation: Operation) {
        if matches!(
            operation,
            Operation::Cancel | Operation::Disable | Operation::Estop
        ) && let Some(base) = self.base.as_mut()
        {
            base.closed = true;
            // 仅记录原停止请求进入派发，不表示成功；未知结果仍由原租期兜底。
            base.cancel_sent = true;
        }
    }

    pub(in crate::status_web) fn base_view(
        &self,
        source: &Source,
        state: &BusView,
        now: u64,
    ) -> BaseView {
        let enabled = self.enabled && source.base_simulation && cfg!(feature = "test-base-command");
        let active = self
            .base
            .as_ref()
            .is_some_and(|base| !base.closed && now < base.deadline_ns);
        let check = if !enabled {
            Err("底盘模拟控制未开启".into())
        } else if self.task.as_ref().is_some_and(TaskView::busy) && !active {
            Err("总线任务尚未结束或结果未知".into())
        } else {
            ready(source, state).map(|_| ())
        };
        BaseView {
            enabled,
            can_hold: check.is_ok(),
            active,
            hold_id: self.base.as_ref().map(|base| base.id.clone()),
            seq: self.base.as_ref().map(|base| base.seq.to_string()),
            lease_deadline_ns: self.base.as_ref().map(|base| base.deadline_ns.to_string()),
            reason: check
                .err()
                .unwrap_or_else(|| "仅模拟三轮raw；按住发送新租期，松开取消".into()),
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "原请求与连接接收时点共同限制一次租期"
    )]
    pub(super) fn prepare_base(
        &self,
        request: &Request,
        source: &Source,
        state: &BusView,
        owner: &str,
        connected: &AtomicBool,
        received_ns: u64,
        now: u64,
    ) -> Result<(u64, Vec<f64>, bool), String> {
        request.validate_shape()?;
        if !request.is_base()
            || !self.enabled
            || !source.base_simulation
            || !cfg!(feature = "test-base-command")
        {
            return Err("底盘模拟入口未开启".into());
        }
        if !connected.load(Ordering::Acquire) || request.process_id != self.process_id {
            return Err("连接已关闭或Web实例已变化；不恢复旧底盘命令".into());
        }
        let id = request.hold_id.as_deref().ok_or("缺少底盘动作标识")?;
        let seq = request
            .seq
            .as_ref()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or("底盘序号无效")?;
        let same = self.base.as_ref().filter(|base| base.id == id);
        if request.operation == Operation::BaseStop {
            let base = same.ok_or("没有本次底盘动作可停止")?;
            if base.owner != owner || seq <= base.seq || base.cancel_sent {
                return Err("底盘停止来源不符或已消费".into());
            }
            return Ok((base.deadline_ns, base.anchor.clone(), false));
        }
        if request.revision != self.revision.to_string() {
            return Err("底盘控制版本已变化；旧包不续租".into());
        }
        let deadline = received_ns
            .checked_add(COMMAND_TTL_MAX_NS)
            .ok_or("底盘期限溢出")?;
        if received_ns > now || now >= deadline {
            return Err("底盘包排队已过期；不续租".into());
        }
        let current = ready(source, state)?;
        let raw = request.wheels_raw.ok_or("缺少三轮原始值")?;
        if ![raw.front, raw.left, raw.right]
            .into_iter()
            .all(robot_adapter_feetech::velocity_profile::Sts3215RawWheelProfile::accepts_raw)
        {
            return Err("三轮仅支持已验raw0/+500".into());
        }
        if let Some(base) = same {
            if base.owner != owner
                || seq != base.seq.checked_add(1).ok_or("底盘序号耗尽")?
                || base.closed
                || now >= base.deadline_ns
            {
                return Err("底盘租期已结束、序号不连续或不属于本连接；禁止复活".into());
            }
            Ok((deadline, base.anchor.clone(), true))
        } else {
            if seq != 1 || self.task.as_ref().is_some_and(TaskView::busy) {
                return Err("底盘首次提交需要新标识、序号1且总线空闲".into());
            }
            Ok((deadline, current, false))
        }
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "同一共享RPC连接上的一次接收、消费与回执，不引入新的任务或循环"
    )]
    pub(in crate::status_web) async fn dispatch_base(
        &mut self,
        request: Request,
        source: &Source,
        state: &BusView,
        owner: &str,
        connected: &AtomicBool,
        received_ns: u64,
        rpc: RpcContext<'_>,
    ) -> Reply {
        if *rpc.shutdown.borrow() {
            return Reply::rejected(Some(&request), self.revision, "Web正在关闭");
        }
        let now = rpc.clock.now().0;
        let (deadline, anchor, renew) =
            match self.prepare_base(&request, source, state, owner, connected, received_ns, now) {
                Ok(value) => value,
                Err(error) => return Reply::rejected(Some(&request), self.revision, error),
            };
        let Some(next_revision) = self.revision.checked_add(1) else {
            return Reply::rejected(Some(&request), self.revision, "控制版本耗尽");
        };
        let Some(next_sequence) = self.base_sequence.checked_add(1) else {
            return Reply::rejected(Some(&request), self.revision, "底盘来源序号耗尽");
        };
        let id = request.hold_id.clone().unwrap_or_default();
        let seq = request
            .seq
            .as_ref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        self.revision = next_revision;
        self.base_sequence = next_sequence.max(now);
        let stop = request.operation == Operation::BaseStop;
        self.base = Some(BaseHold {
            owner: owner.into(),
            id: id.clone(),
            seq,
            deadline_ns: deadline,
            anchor: anchor.clone(),
            closed: true,
            cancel_sent: stop,
        });
        let rpc_id = if stop {
            uuid::Uuid::now_v7().to_string()
        } else {
            id.clone()
        };
        let mut meta = CommandMeta {
            request_id: rpc_id.clone(),
            source: if renew {
                "web-base-renew"
            } else {
                "web-base-start"
            }
            .into(),
            auth: Some(AuthContext {
                token: rpc.token.into(),
                mtls_fingerprint: String::new(),
            }),
            timeout_ms: u64::try_from(rpc.timeout.as_millis()).unwrap_or(u64::MAX),
            idempotency_key: format!("web-base-{id}-{seq}"),
            dry_run: false,
            audit_reason: "仅隔离软件模拟底盘按住控制".into(),
        };
        let slots = match wheel_slots(source) {
            Ok(slots) => slots,
            Err(error) => return Reply::rejected(Some(&request), self.revision, error),
        };
        let command = async {
            if stop {
                meta.source = "robot-status".into();
                let mut command = tonic::Request::new(CancelTaskCommand {
                    meta: Some(meta),
                    action_id: id.clone(),
                });
                command.set_timeout(rpc.timeout);
                tokio::time::timeout(rpc.timeout, rpc.client.cancel_task(command))
                    .await
                    .map(|r| r.map(|ack| RpcReply::Ack(ack.into_inner())))
            } else {
                let raw = request.wheels_raw.unwrap_or(WheelsRaw {
                    front: 0,
                    left: 0,
                    right: 0,
                });
                let mut velocities = [0; 9];
                for (slot, value) in slots.into_iter().zip([raw.front, raw.left, raw.right]) {
                    velocities[slot] = value;
                }
                self.task = Some(TaskView {
                    action_id: id.clone(),
                    state: "unknown",
                    progress: None,
                    error: Some("底盘提交等待回执".into()),
                });
                let payload = robot_msg::build_helpers::build_mixed_joint_command(
                    source.layout.layout_id,
                    &anchor,
                    &velocities,
                    deadline,
                    10,
                    self.base_sequence,
                );
                let mut command = tonic::Request::new(InvokeTaskCommand {
                    meta: Some(meta),
                    kind: ActionKindProto::ActionKindScripted as i32,
                    deadline_ns: 0,
                    priority: 10,
                    payload_schema_id: robot_msg::schema_id::SCHEMA_JOINT_COMMAND,
                    payload,
                });
                command.set_timeout(rpc.timeout);
                tokio::time::timeout(rpc.timeout, rpc.client.invoke_task(command))
                    .await
                    .map(|r| r.map(|handle| RpcReply::Task(handle.into_inner())))
            }
        };
        let result = tokio::select! {
            _ = rpc.shutdown.changed() => Err("Web关闭时底盘RPC结果未知"),
            result = command => result.map_err(|_| "底盘RPC超时"),
        };
        let mut reply = Reply::rejected(Some(&request), self.revision, "");
        reply.action_id = Some(id.clone());
        reply.lease_deadline_ns = Some(deadline.to_string());
        match result {
            Ok(Ok(response)) if response.matches(&rpc_id) => {
                reply.status = "accepted";
                reply.message = accepted_message(request.operation).into();
                if !stop {
                    if let Some(base) = &mut self.base {
                        base.closed =
                            !connected.load(Ordering::Acquire) || rpc.clock.now().0 >= deadline;
                    }
                    self.task = Some(TaskView {
                        action_id: id,
                        state: "pending",
                        progress: None,
                        error: None,
                    });
                }
            }
            Ok(Err(error)) if definitely_rejected(error.code()) => {
                reply.message = format!("gRPC拒绝 {:?}：{}", error.code(), error.message());
                if !renew && !stop {
                    self.task = Some(TaskView {
                        action_id: id,
                        state: "failed",
                        progress: None,
                        error: Some(reply.message.clone()),
                    });
                }
            }
            _ => {
                reply.status = "unknown";
                reply.message = "底盘RPC结果未知，不重发、不续租；原期限兜底".into();
            }
        }
        self.last_request = Some(request);
        self.last_result = Some(reply.clone());
        reply
    }

    pub(in crate::status_web) async fn disconnect_base(
        &mut self,
        owner: &str,
        rpc: RpcContext<'_>,
    ) {
        let Some(base) = self
            .base
            .as_mut()
            .filter(|base| base.owner == owner && !base.cancel_sent)
        else {
            return;
        };
        base.closed = true;
        base.cancel_sent = true;
        let id = base.id.clone();
        let request_id = uuid::Uuid::now_v7().to_string();
        let mut request = tonic::Request::new(CancelTaskCommand {
            meta: Some(CommandMeta {
                request_id: request_id.clone(),
                source: "robot-status".into(),
                auth: Some(AuthContext {
                    token: rpc.token.into(),
                    mtls_fingerprint: String::new(),
                }),
                timeout_ms: u64::try_from(rpc.timeout.as_millis()).unwrap_or(u64::MAX),
                idempotency_key: format!("web-disconnect-{request_id}"),
                dry_run: false,
                audit_reason: "底盘所属浏览器连接已关闭；仅取消一次".into(),
            }),
            action_id: id,
        });
        request.set_timeout(rpc.timeout);
        // 无回执也不重发；S1最后一份固定租期仍独立生效。
        tokio::select! {
            _ = rpc.shutdown.changed() => {},
            _ = tokio::time::timeout(rpc.timeout, rpc.client.cancel_task(request)) => {},
        }
    }
}
