//! R4b1：原成功 Prepare 后，两次真实目录同步失败与一次原 Unsynced 重投。

use std::time::Duration;

use iceoryx2::prelude::{Node, NodeBuilder};
use iceoryx2::service::ipc::Service;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::{markers, msg};
use robot_s0::clear_estop_transaction::{ClearTxnStage, StepPhase};
use robot_s0::comm_fault_trace::SampleMode;
use robot_s0::comm_fault_trace::w4::{CommandKind, CompletionKind, TokenKind};
use robot_transport::iceoryx2::request_response::{Iceoryx2PendingResponse, Iceoryx2RrClient};
use serde_json::{Value, json};
use tokio::process::Child;

use super::Args;
use super::control::ControlPort;
use super::harness::{Case, wait_log};
use super::report::Report;
use super::sla::send_sample_request;
use super::w4_latch_crash::{
    CommandFact, TraceCapture, close_child, command_evidence, disarm_with_drain,
    receive_with_closed_tail, require_running,
};

const CASE_FILTER: &str = "R4-T8-RESYNC";
const CASE_NAME: &str = "ordinary_unsynced_resync_retry_retires_original_clear";
const OBSERVATION_NS: u64 = 3_000_000_000;

pub(super) async fn run(args: &Args, report: &mut Report) -> Result<(), String> {
    if !cfg!(target_os = "linux") || args.case.as_deref() != Some(CASE_FILTER) {
        return Err("R4b1 仅接受 Linux mock 的精确 R4-T8-RESYNC 入口".into());
    }
    let before = report.cases.len();
    report.record(
        CASE_NAME,
        CASE_FILTER,
        SystemClock.now().0,
        run_case(args).await,
    );
    if report.cases.len().checked_sub(before) != Some(1) {
        return Err("R4b1 未真实登记恰一个具名样例".into());
    }
    Ok(())
}

/// 原 RR、注入 worker 与 Node 跨所有 Result 出口保留至 Child 真 reap。
struct Owned {
    child: Child,
    control: Option<ControlPort>,
    pending: Option<Iceoryx2PendingResponse<markers::ClearEStopResponse>>,
    client: Option<Iceoryx2RrClient<markers::ClearEStopRequest, markers::ClearEStopResponse>>,
    node: Option<Node<Service>>,
    replies: u8,
}

async fn run_case(args: &Args) -> Result<Value, String> {
    let case = Case::new(&args.evidence_dir, &args.bin_dir, CASE_NAME, 1)?;
    for path in [&case.latch, &case.directory.join("estop_latch.bak")] {
        std::fs::write(path, b"1\n").map_err(|error| error.to_string())?;
    }
    let mut command = case.command();
    command
        .args(["--notify-mode", "standalone"])
        .env("ROBOT_TEST_W4_FILE_FAULT", "ordinary-resync")
        .env("ROBOT_LOG_FORMAT", "json");
    let launch = json!({"program":command.as_std().get_program().to_string_lossy(),
        "argv":command.as_std().get_args().map(|v|v.to_string_lossy().into_owned()).collect::<Vec<_>>(),
        "env_changes":command.as_std().get_envs().map(|(k,v)|(k.to_string_lossy().into_owned(),v.map(|v|v.to_string_lossy().into_owned()))).collect::<std::collections::BTreeMap<_,_>>()});
    save_json(&case, "launch.json", &launch)?;
    let mut owned = Owned {
        child: case.spawn("s0", command)?,
        control: None,
        pending: None,
        client: None,
        node: None,
        replies: 0,
    };
    let mut capture = TraceCapture::open(&case);
    let result = match &mut capture {
        Ok(capture) => exercise(&case, &mut owned, capture).await,
        Err(error) => Err(error.clone()),
    };
    let cleanup = close_child(&mut owned.child, capture.as_mut().ok(), || {
        capture_reply(&case, owned.pending.as_ref(), &mut owned.replies)
    })
    .await;
    // close_child 无法真 reap 会中止验收父进程；成功或失败返回都仍保有全部原件。
    let reaped_at_ns = SystemClock.now().0;
    let control_cleanup = owned.control.as_mut().map(ControlPort::finish).transpose();
    let control_joined_at_ns = SystemClock.now().0;
    let final_trace = match &mut capture {
        Ok(capture) => {
            capture.drain();
            validate_capture(capture)
        }
        Err(error) => Err(error.clone()),
    };
    let final_state = final_state(&case, owned.replies, &mut owned.child);
    let capture_evidence = match &capture {
        Ok(capture) => capture.evidence(),
        Err(error) => json!({"open_error":error}),
    };
    let evidence = json!({"case":CASE_NAME,"directory":case.directory,"launch":launch,
        "result":result,"cleanup":cleanup,"reaped_at_ns":reaped_at_ns,
        "control_cleanup":control_cleanup,"control_joined_at_ns":control_joined_at_ns,
        "capture":capture_evidence,"final_trace":final_trace,"final_state":final_state,
        "fault_input":"原绑定目录同inode O_PATH别名实际fsync两次EBADF，第三次原fd"});
    save_json(&case, "result.json", &evidence)?;
    match (result, cleanup, control_cleanup, final_trace, final_state) {
        (Ok(_), Ok(()), Ok(_), Ok(_), Ok(_)) => Ok(evidence),
        (result, cleanup, control, trace, state) => Err(format!(
            "原场景={result:?}；Child真收尾={cleanup:?}；注入线程真join={control:?}；原trace={trace:?}；终态={state:?}；证据={}",
            case.directory.display()
        )),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "同一原RR和采样命令保持连续安装/观察顺序，所有错误回到外层统一真reap"
)]
async fn exercise(
    case: &Case,
    owned: &mut Owned,
    capture: &mut TraceCapture,
) -> Result<Value, String> {
    owned.control = Some(ControlPort::spawn(&case.robot_id)?);
    let port = owned.control.as_mut().ok_or("原ControlPort安装后丢失")?;
    port.wait_ready()?;
    wait_log(
        case,
        &mut owned.child,
        "s0",
        "boot_ready",
        Duration::from_secs(12),
    )
    .await?;
    let (sample, _) = send_sample_request(
        port,
        case,
        &mut owned.child,
        capture.page(),
        SampleMode::ArmInflight,
    )
    .await?;
    capture.set_sample(sample);
    capture.drain();
    owned.node = Some(
        NodeBuilder::new()
            .create::<Service>()
            .map_err(|error| error.to_string())?,
    );
    owned.client = Some(
        robot_transport::iceoryx2::rr_client_with::<
            markers::ClearEStopRequest,
            markers::ClearEStopResponse,
        >(
            owned.node.as_ref().ok_or("原Node安装后丢失")?,
            &format!("/{}/control/clear_estop", case.robot_id),
            &robot_transport::iceoryx2::RrProfile::S0_CONTROL,
        )
        .map_err(|error| error.to_string())?,
    );
    let request_id = format!("{}-clear", case.robot_id);
    let started = SystemClock.now().0;
    let body = robot_msg::build_helpers::build_clear_estop_request(
        &request_id,
        &request_id,
        "R4b1",
        msg::RpcRole::Maintainer,
        "确认真实停止",
        "核原Unsynced一次重投",
        false,
        started.saturating_add(5_000_000_000),
        started,
    );
    std::fs::write(case.directory.join("clear_request.bin"), &body)
        .map_err(|error| error.to_string())?;
    owned.pending = Some(
        owned
            .client
            .as_ref()
            .ok_or("原client丢失")?
            .send_typed(&body)
            .map_err(|error| format!("{error:?}"))?,
    );
    let pending = owned.pending.as_ref().ok_or("原Pending安装后丢失")?;
    if let Some(issue) = pending.delivery_issue() {
        return Err(format!(
            "原请求已入队但带诊断，保留Pending到真reap：{issue:?}"
        ));
    }
    let until = started.saturating_add(OBSERVATION_NS);
    let (reply, reply_accepted_at_ns) = loop {
        capture.drain();
        if let Some(reply) = receive_with_closed_tail(pending)? {
            owned.replies = 1;
            // 即使接纳时已超时，也保存这份实际收到的原响应后再返回失败。
            std::fs::write(case.directory.join("clear_response.bin"), &reply)
                .map_err(|error| error.to_string())?;
            let accepted_at = accept_observation(SystemClock.now().0, until)?;
            break (reply, accepted_at);
        }
        require_running(&mut owned.child, until, "原Clear唯一退休回复")?;
        tokio::time::sleep(Duration::from_millis(2)).await;
    };
    let response = verify_reply(&reply, &request_id)?;
    let (chain, chain_accepted_at_ns) = loop {
        capture.drain();
        match validate_capture(capture) {
            Ok(chain) => {
                let accepted_at = accept_observation(SystemClock.now().0, until)?;
                break (chain, accepted_at);
            }
            Err(error) if SystemClock.now().0 >= until => return Err(error),
            Err(error) => {
                require_running(&mut owned.child, until, "原writer完整末事件")
                    .map_err(|wait| format!("{error}；{wait}"))?;
            }
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    };
    disarm_with_drain(port, case, &mut owned.child, capture).await?;
    capture.drain();
    let _ = validate_capture(capture)?;
    verify_files(case)?;
    Ok(
        json!({"request_id":request_id,"sample_seq":sample,"request_started_ns":started,
        "observation_deadline_ns":until,"reply_accepted_at_ns":reply_accepted_at_ns,
        "chain_accepted_at_ns":chain_accepted_at_ns,"reply":response,"chain":chain}),
    )
}

/// 可用结果也须在原观察截止之前接纳；父调度延迟不产生新的预算。
fn accept_observation(observed_at: u64, until: u64) -> Result<u64, String> {
    if observed_at >= until {
        return Err(format!(
            "原观察已超时，拒绝接纳已有结果：observed={observed_at}，deadline={until}"
        ));
    }
    Ok(observed_at)
}

fn capture_reply(
    case: &Case,
    pending: Option<&Iceoryx2PendingResponse<markers::ClearEStopResponse>>,
    replies: &mut u8,
) -> Result<(), String> {
    if let Some(pending) = pending
        && let Some(bytes) = receive_with_closed_tail(pending)?
    {
        std::fs::write(
            case.directory
                .join(format!("clear_cleanup_response_{replies}.bin")),
            bytes,
        )
        .map_err(|error| error.to_string())?;
        *replies = replies.checked_add(1).ok_or("原RR响应计数溢出")?;
        if *replies != 1 {
            return Err("原Clear出现重复退休响应".into());
        }
    }
    Ok(())
}

fn verify_reply(bytes: &[u8], request_id: &str) -> Result<Value, String> {
    let response =
        flatbuffers::root::<msg::ClearEStopResponse>(bytes).map_err(|error| error.to_string())?;
    if response.request_id() != Some(request_id)
        || response.status() != msg::RpcStatus::Ok
        || response.domain() != msg::ClearEStopStatus::Cleared
        || response
            .error_msg()
            .is_some_and(|message| !message.is_empty())
        || !response.retired()
        || response.dry_run()
        || response.superseded()
        || response.effect() != msg::SideEffect::Committed
        || response.cleared_at_ns() == 0
    {
        return Err(format!(
            "原逻辑提交应唯一Ok/Cleared退休且业务首错误为空：{response:?}"
        ));
    }
    Ok(
        json!({"request_id":response.request_id(),"status":"Ok","domain":"Cleared",
        "error_msg":response.error_msg(),"retired":response.retired(),"effect":"Committed",
        "dry_run":response.dry_run(),"superseded":response.superseded(),"cleared_at_ns":response.cleared_at_ns()}),
    )
}

fn verify_files(case: &Case) -> Result<(), String> {
    for path in [&case.latch, &case.directory.join("estop_latch.bak")] {
        if std::fs::read(path).map_err(|error| error.to_string())? != b"0\n" {
            return Err(format!("原逻辑提交未保0/0：{}", path.display()));
        }
    }
    match std::fs::symlink_metadata(case.directory.join("estop_latch.clear_pending")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("无法证明原marker不存在：{error}")),
        Ok(_) => Err("原marker仍在".into()),
    }
}

fn final_state(case: &Case, replies: u8, child: &mut Child) -> Result<Value, String> {
    let exit_code = child
        .try_wait()
        .map_err(|error| error.to_string())?
        .ok_or("原Child尚未真reap，不能写退出成功")?
        .code();
    let page = case.observation("s0")?;
    if exit_code != Some(0)
        || page.fired_checkpoint != 0
        || page.residual_bitmap != 0
        || replies != 1
    {
        return Err(format!(
            "真实清洁退出或原唯一回复不符：{page:?}，replies={replies}，exit={exit_code:?}"
        ));
    }
    verify_files(case)?;
    Ok(
        json!({"exit_code":exit_code,"residual_bitmap":page.residual_bitmap,
        "fired_checkpoint":page.fired_checkpoint,"clear_reply_count":replies,
        "latch_pair":[0,0],"marker_present":false}),
    )
}

fn validate_capture(capture: &TraceCapture) -> Result<Value, String> {
    let commands = capture.commands()?;
    validate_chain(&commands)?;
    Ok(command_evidence(&commands))
}

fn validate_chain(commands: &[CommandFact]) -> Result<(), String> {
    use ClearTxnStage as S;
    use CommandKind as C;
    use CompletionKind as R;
    use TokenKind as T;
    let expected = [
        (C::Prepare, T::None, T::Prepared, R::Prepared),
        (
            C::RemoveCommitted,
            T::Prepared,
            T::Unsynced,
            R::RemoveFailedUnsynced,
        ),
        (C::Resync, T::Unsynced, T::Unsynced, R::RemoveFailedUnsynced),
        (C::Resync, T::Unsynced, T::None, R::Resynced),
    ];
    if commands.len() != expected.len() {
        return Err("原四命令链不完整或出现额外重投".into());
    }
    let stages: [&[S]; 4] = [
        &[
            S::CreateMarker,
            S::SampleMarkerIdentity,
            S::WriteMarker,
            S::SyncMarkerFile,
            S::SyncMarkerDirectory,
            S::CreateBackup,
            S::WriteBackup,
            S::SyncBackupFile,
            S::RenameBackup,
            S::SyncBackupDirectory,
            S::CreatePrimary,
            S::WritePrimary,
            S::SyncPrimaryFile,
            S::RenamePrimary,
            S::SyncPrimaryDirectory,
        ],
        &[
            S::ReadMarker,
            S::ValidateMarker,
            S::RemoveMarker,
            S::SyncRemovalDirectory,
        ],
        &[S::SyncRemovalDirectory],
        &[S::SyncRemovalDirectory],
    ];
    for (index, (command, expected)) in commands.iter().zip(expected).enumerate() {
        if (
            command.command,
            command.before,
            command.after,
            command.completion,
        ) != expected
            || !command
                .steps
                .iter()
                .map(|step| step.stage)
                .eq(stages[index].iter().copied())
        {
            return Err(format!("原命令/凭证/完整步骤链不符：{command:?}"));
        }
        let slot = match index {
            1 => Some(0),
            2 => Some(1),
            _ => None,
        };
        let expected_failure = slot.map(|_| (S::SyncRemovalDirectory, StepPhase::Operation));
        if command.failure != expected_failure {
            return Err(format!("原completion错误相位不符：{command:?}"));
        }
        for step in &command.steps {
            let expected_slot = if step.stage == S::SyncRemovalDirectory {
                slot
            } else {
                None
            };
            if step.slot != expected_slot || step.succeeded != expected_slot.is_none() {
                return Err(format!("原槽/实际成功返回不符：{step:?}"));
            }
        }
    }
    Ok(())
}

fn save_json(case: &Case, name: &str, value: &Value) -> Result<(), String> {
    std::fs::write(
        case.directory.join(name),
        serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::super::w4_latch_crash::StepFact;
    use super::*;
    use robot_s0::latch_writer::ClearTxnIdentity;

    // 合成输入只核解析 oracle 的拒绝边界；真实文件与端点由具名进程门证明。
    fn observed_chain() -> Vec<CommandFact> {
        use ClearTxnStage as S;
        use CommandKind as C;
        use CompletionKind as R;
        use TokenKind as T;
        let txn = ClearTxnIdentity {
            txn_id: 1_u128 << 100,
            expected_epoch: 7,
            original_latched: true,
        };
        [
            (
                C::Prepare,
                T::None,
                T::Prepared,
                R::Prepared,
                vec![
                    S::CreateMarker,
                    S::SampleMarkerIdentity,
                    S::WriteMarker,
                    S::SyncMarkerFile,
                    S::SyncMarkerDirectory,
                    S::CreateBackup,
                    S::WriteBackup,
                    S::SyncBackupFile,
                    S::RenameBackup,
                    S::SyncBackupDirectory,
                    S::CreatePrimary,
                    S::WritePrimary,
                    S::SyncPrimaryFile,
                    S::RenamePrimary,
                    S::SyncPrimaryDirectory,
                ],
                None,
            ),
            (
                C::RemoveCommitted,
                T::Prepared,
                T::Unsynced,
                R::RemoveFailedUnsynced,
                vec![
                    S::ReadMarker,
                    S::ValidateMarker,
                    S::RemoveMarker,
                    S::SyncRemovalDirectory,
                ],
                Some(0),
            ),
            (
                C::Resync,
                T::Unsynced,
                T::Unsynced,
                R::RemoveFailedUnsynced,
                vec![S::SyncRemovalDirectory],
                Some(1),
            ),
            (
                C::Resync,
                T::Unsynced,
                T::None,
                R::Resynced,
                vec![S::SyncRemovalDirectory],
                None,
            ),
        ]
        .into_iter()
        .enumerate()
        .map(
            |(index, (command, before, after, completion, stages, slot))| CommandFact {
                source: u64::try_from(index).unwrap() + 1,
                txn,
                command,
                before,
                after,
                completion,
                failure: slot.map(|_| (S::SyncRemovalDirectory, StepPhase::Operation)),
                steps: stages
                    .into_iter()
                    .map(|stage| {
                        let slot = if stage == S::SyncRemovalDirectory {
                            slot
                        } else {
                            None
                        };
                        StepFact {
                            stage,
                            succeeded: slot.is_none(),
                            slot,
                        }
                    })
                    .collect(),
            },
        )
        .collect()
    }

    #[test]
    fn resync_observation_rejects_ready_results_at_or_after_original_deadline() {
        let original_deadline = 3_000_000_100;
        assert_eq!(
            accept_observation(original_deadline - 1, original_deadline),
            Ok(original_deadline - 1)
        );
        for observed_at in [
            original_deadline,
            original_deadline + 1,
            original_deadline + 1_000_000_000,
        ] {
            assert!(accept_observation(observed_at, original_deadline).is_err());
        }
    }

    #[test]
    fn resync_oracle_rejects_missing_extra_and_repair_commands() {
        let mut commands = observed_chain();
        assert!(validate_chain(&commands).is_ok());
        let mut prefix_only = observed_chain();
        prefix_only[0].steps.truncate(5);
        assert!(validate_chain(&prefix_only).is_err());
        for missing in 5..15 {
            let mut missing_prepare = observed_chain();
            let _ = missing_prepare[0].steps.remove(missing);
            assert!(validate_chain(&missing_prepare).is_err());
        }
        assert!(validate_chain(&commands[..3]).is_err());
        commands.push(observed_chain().remove(3));
        assert!(validate_chain(&commands).is_err());
        commands.truncate(4);
        commands[2].command = CommandKind::RepairLatched;
        assert!(validate_chain(&commands).is_err());
    }

    #[test]
    fn resync_oracle_rejects_repeated_unlink_wrong_slot_and_false_success() {
        let mut commands = observed_chain();
        commands[3].steps.insert(
            0,
            StepFact {
                stage: ClearTxnStage::RemoveMarker,
                succeeded: true,
                slot: None,
            },
        );
        assert!(validate_chain(&commands).is_err());
        let mut commands = observed_chain();
        commands[2].steps[0].slot = Some(0);
        assert!(validate_chain(&commands).is_err());
        commands[2].steps[0].slot = Some(1);
        commands[2].steps[0].succeeded = true;
        assert!(validate_chain(&commands).is_err());
    }

    #[test]
    fn resync_oracle_rejects_lost_unsynced_prepare_failure_and_non_operation_error() {
        let mut commands = observed_chain();
        commands[2].before = TokenKind::None;
        assert!(validate_chain(&commands).is_err());
        let mut commands = observed_chain();
        commands[0].completion = CompletionKind::PrepareCreated;
        assert!(validate_chain(&commands).is_err());
        let mut commands = observed_chain();
        commands[1].failure = None;
        assert!(validate_chain(&commands).is_err());
    }
}
