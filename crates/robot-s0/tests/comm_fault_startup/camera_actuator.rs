//! 真实 daemon 的软件并行接线验证；执行器为 mock，图像为固定协议夹具。

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service;
use robot_msg::{build_helpers, markers, msg};
use robot_transport::iceoryx2::request_response::Iceoryx2RrClient;
use robot_transport::iceoryx2::{
    Iceoryx2Subscriber, RrProfile, TopicClass, publisher, rr_client_with, subscriber,
};
use robot_transport::traits::{LatestSubscriber, Publisher};

use super::{CameraFixture, Duration, Process, RobotClock, SystemClock};

const A: [f64; 8] = [0.125, -0.25, 0.375, -0.5, 0.625, -0.75, 0.875, -1.0];
const B: [f64; 8] = [1.0, -0.875, 0.75, -0.625, 0.5, -0.375, 0.25, -0.125];

fn publish<T: markers::BodyMarker>(
    publisher: &mut impl Publisher<T>,
    body: &[u8],
    sequence: u64,
    stamp: u64,
) {
    let mut sample = publisher.loan(1024 + body.len()).unwrap();
    sample.set_seq(sequence);
    sample.set_source_id(2);
    sample.set_stamp_ns(stamp);
    sample.set_body_bytes(body);
    sample.publish().unwrap();
}

/// 只限制夹具等待，不能作为吞吐或实机延迟验收门槛。
fn wait(process: &mut Process, description: &str, mut condition: impl FnMut() -> bool) {
    let deadline = SystemClock.now().0 + 5_000_000_000;
    loop {
        assert!(
            process.child.try_wait().unwrap().is_none(),
            "{description}前 daemon 退出：{}",
            process.log()
        );
        if condition() {
            return;
        }
        assert!(
            SystemClock.now().0 < deadline,
            "等待{description}超时：{}",
            process.log()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn target_feedback(
    subscriber: &mut Iceoryx2Subscriber<markers::JointState>,
    target: &[f64; 8],
) -> Option<(u64, u64)> {
    let lease = subscriber.latest().unwrap()?;
    let view = lease.view().unwrap();
    let state: msg::JointState = view.body.get();
    let position = state.position_rad().unwrap();
    assert_eq!(position.len(), 8);
    (state.s0_mode() == msg::S0Mode::Enabled
        && state.s0_output_mode() == msg::S0OutputMode::Active
        && state.s0_safety_state() == msg::S0SafetyState::TorqueOn
        && position
            .iter()
            .map(f64::to_bits)
            .eq(target.iter().copied().map(f64::to_bits)))
    .then_some((view.header.seq(), state.stamp_ns()))
}

fn heartbeat_sequence(
    subscriber: &mut Iceoryx2Subscriber<markers::Heartbeat>,
) -> Option<(u64, u64)> {
    let lease = subscriber.latest().unwrap()?;
    let view = lease.view().unwrap();
    let heartbeat: msg::Heartbeat = view.body.get();
    assert_eq!(heartbeat.source_id(), 1);
    assert_eq!(heartbeat.seq(), view.header.seq());
    Some((view.header.seq(), view.header.stamp_ns()))
}

fn assert_settled(bytes: &[u8], request_id: &str) {
    let response = flatbuffers::root::<msg::SetEnabledResponse>(bytes).unwrap();
    assert_eq!(response.request_id(), Some(request_id));
    assert_eq!(response.status(), msg::RpcStatus::Ok);
    assert_eq!(response.domain(), msg::SetEnabledStatus::Ok);
    assert_eq!(response.effect(), msg::SideEffect::Committed);
    assert!(response.retired());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "同一 daemon 内按顺序保留命令、图像、失流与真实回收的证据"
)]
fn three_cameras_isolate_stall_and_preserve_mock_feedback_until_clean_shutdown() {
    let mut process =
        Process::spawn_inner(8, &[], None, None, false, &[], &[CameraFixture::Frames; 3]);
    let node = NodeBuilder::new().create::<Service>().unwrap();
    let robot = &process.robot_id;
    let mut commands = publisher::<markers::JointCommand>(
        &node,
        &format!("/{robot}/s1/arm/command"),
        TopicClass::S0Command,
    )
    .unwrap();
    let mut heartbeat = publisher::<markers::Heartbeat>(
        &node,
        &format!("/{robot}/s1/heartbeat"),
        TopicClass::Heartbeat,
    )
    .unwrap();
    let mut state = subscriber::<markers::JointState>(
        &node,
        &format!("/{robot}/s0/arm/state"),
        TopicClass::S0State,
    )
    .unwrap();
    let mut alive = subscriber::<markers::Heartbeat>(
        &node,
        &format!("/{robot}/s0/heartbeat"),
        TopicClass::Heartbeat,
    )
    .unwrap();
    let mut images: Vec<_> = (0..3)
        .map(|index| {
            subscriber::<markers::ImageHeader>(
                &node,
                &format!("/{robot}/sensors/fixture_camera_{index}/color"),
                TopicClass::Sensor,
            )
            .unwrap()
        })
        .collect();
    let client: Iceoryx2RrClient<markers::SetEnabledRequest, markers::SetEnabledResponse> =
        rr_client_with(
            &node,
            &format!("/{robot}/control/set_enabled"),
            &RrProfile::S0_CONTROL,
        )
        .unwrap();
    for index in 0..3 {
        process.wait_camera_started_at(index);
    }
    for index in 0..2 {
        std::fs::write(process.camera_file_at(index, "camera-go"), b"go").unwrap();
    }
    let first_frames: Vec<_> = (0..2)
        .map(|index| process.camera_file_at(index, "camera-first-frame"))
        .collect();
    wait(&mut process, "前两路首帧", || {
        first_frames.iter().all(|file| file.exists())
    });
    assert!(!process.page.snapshot().ready_reached, "第三路无帧不能就绪");
    std::fs::write(process.camera_file_at(2, "camera-go"), b"go").unwrap();
    let ready = process.wait_ready(5000);
    for index in 0..3 {
        let stamp: u64 =
            std::fs::read_to_string(process.camera_file_at(index, "camera-first-frame"))
                .unwrap()
                .parse()
                .unwrap();
        assert!(ready.ready_ns >= stamp, "就绪必须在每一路首帧之后");
    }
    commands.update_connections().unwrap();
    heartbeat.update_connections().unwrap();
    let mut layout = None;
    wait(&mut process, "初始状态", || {
        if let Some(lease) = state.latest().unwrap() {
            let view = lease.view().unwrap();
            let joint: msg::JointState = view.body.get();
            assert_eq!(joint.position_rad().unwrap().len(), 8);
            layout = Some(joint.layout_id());
        }
        layout.is_some()
    });
    let layout = layout.unwrap();
    let mut sequence = 0;
    let mut last_input_ns = 0;
    // 同一测试线程在每个等待阶段续发；不另起生产者线程或改变原超时合同。
    let mut pump = |target: &[f64; 8]| {
        let now = SystemClock.now().0;
        if now.saturating_sub(last_input_ns) >= 20_000_000 {
            sequence += 1;
            let body = build_helpers::build_joint_command(
                layout,
                msg::ControlMode::Position,
                target,
                now + 500_000_000,
                10,
                msg::StaleAction::Hold,
                sequence,
            );
            publish(&mut commands, &body, sequence, now);
            let body = build_helpers::build_heartbeat(2, sequence, 50.0, now);
            publish(&mut heartbeat, &body, sequence, now);
            last_input_ns = now;
        }
    };
    pump(&A);
    {
        let now = SystemClock.now().0;
        let body = build_helpers::build_set_enabled_request(
            "camera-enable",
            "camera-enable",
            "camera-software-test",
            msg::RpcRole::Maintainer,
            true,
            false,
            now + 5_000_000_000,
            "仅 mock 执行器",
            now,
        );
        let pending = client.send_typed(&body).unwrap();
        assert!(pending.delivery_issue().is_none());
        wait(&mut process, "Enable 真实退休响应", || {
            pump(&A);
            pending.try_recv().unwrap().is_some_and(|bytes| {
                assert_settled(&bytes, "camera-enable");
                true
            })
        });
    }
    let mut a_feedback = None;
    let mut a_heartbeat = None;
    let mut image_counts = [0; 3];
    let mut last_image_stamps = [0; 3];
    let mut last_image_sequences = [None; 3];
    wait(
        &mut process,
        "A目标反馈与三路各两张推进的原始图像",
        || {
            pump(&A);
            a_feedback = target_feedback(&mut state, &A).or(a_feedback);
            a_heartbeat = heartbeat_sequence(&mut alive).or(a_heartbeat);
            for (index, images) in images.iter_mut().enumerate() {
                let Some(lease) = images.latest().unwrap() else {
                    continue;
                };
                let view = lease.view().unwrap();
                let image: msg::ImageHeader = view.body.get();
                assert_eq!((image.width(), image.height(), image.step()), (2, 1, 6));
                assert_eq!(image.encoding(), Some("bgr8"));
                assert_eq!(image.color_order(), Some("BGR"));
                let frame_id = format!("fixture_camera_{index}");
                assert_eq!(image.frame_id(), Some(frame_id.as_str()));
                assert_eq!(image.stamp_ns(), view.header.stamp_ns());
                assert_eq!(image.blob().unwrap().size(), 6);
                let offset = u8::try_from(index).unwrap();
                assert_eq!(
                    view.blob.unwrap().bytes(),
                    [1, 2, 3, 4, 5, 6].map(|value| value + offset)
                );
                assert!(image.stamp_ns() > last_image_stamps[index]);
                assert!(
                    last_image_sequences[index].is_none_or(|previous| view.header.seq() > previous)
                );
                last_image_stamps[index] = image.stamp_ns();
                last_image_sequences[index] = Some(view.header.seq());
                image_counts[index] += 1;
            }
            a_feedback.is_some()
                && a_heartbeat.is_some()
                && image_counts.iter().all(|&count| count >= 2)
        },
    );
    let a_feedback = a_feedback.unwrap();
    let a_heartbeat = a_heartbeat.unwrap();
    let helper_pid: u32 = std::fs::read_to_string(process.camera_file("camera-pid"))
        .unwrap()
        .parse()
        .unwrap();
    let helper_process = std::path::PathBuf::from(format!("/proc/{helper_pid}"));
    std::fs::write(process.camera_file("camera-stall"), b"stall").unwrap();
    let helper_stopped = process.camera_file("camera-stopped");
    wait(&mut process, "原无新帧期限触发相机回收", || {
        pump(&A);
        let _ = state.latest().unwrap();
        let _ = alive.latest().unwrap();
        for images in &mut images {
            let _ = images.latest().unwrap();
        }
        helper_stopped.is_file() && !helper_process.exists()
    });
    process.assert_camera_reaped();
    assert!(
        process
            .log()
            .contains("相机超过新鲜度期限未产新帧，停用并回收相机"),
        "{}",
        process.log()
    );
    // 故障前的 history 已消费；只比较相机回收之后的新执行器目标与存活序号。
    let stopped_at = SystemClock.now().0;
    let mut b_feedback = None;
    let mut b_heartbeat = None;
    let mut healthy_frames = [false; 2];
    wait(
        &mut process,
        "失流后的两路图像、B目标反馈与心跳推进",
        || {
            pump(&B);
            b_feedback = target_feedback(&mut state, &B).or(b_feedback);
            b_heartbeat = heartbeat_sequence(&mut alive).or(b_heartbeat);
            if let Some(lease) = images[0].latest().unwrap() {
                assert!(lease.view().unwrap().header.stamp_ns() <= stopped_at);
            }
            for (index, images) in images[1..].iter_mut().enumerate() {
                if let Some(lease) = images.latest().unwrap() {
                    let view = lease.view().unwrap();
                    healthy_frames[index] |= view.header.stamp_ns() > stopped_at;
                }
            }
            b_feedback.is_some_and(|(seq, stamp)| seq > a_feedback.0 && stamp > stopped_at)
                && b_heartbeat.is_some_and(|(seq, stamp)| seq > a_heartbeat.0 && stamp > stopped_at)
                && healthy_frames.iter().all(|seen| *seen)
        },
    );
    {
        let now = SystemClock.now().0;
        let body = build_helpers::build_set_enabled_request(
            "camera-disable",
            "camera-disable",
            "camera-software-test",
            msg::RpcRole::Maintainer,
            false,
            false,
            now + 5_000_000_000,
            "仅 mock 执行器",
            now,
        );
        let pending = client.send_typed(&body).unwrap();
        assert!(pending.delivery_issue().is_none());
        wait(&mut process, "Disable 真实退休响应", || {
            pump(&B);
            pending.try_recv().unwrap().is_some_and(|bytes| {
                assert_settled(&bytes, "camera-disable");
                true
            })
        });
    }
    wait(&mut process, "Disable 后断扭状态", || {
        pump(&B);
        state.latest().unwrap().is_some_and(|lease| {
            let view = lease.view().unwrap();
            let joint: msg::JointState = view.body.get();
            joint.s0_mode() == msg::S0Mode::Safe
                && joint.s0_safety_state() == msg::S0SafetyState::TorqueOffConfirmed
        })
    });
    process.stop();
    for index in 0..3 {
        process.assert_camera_reaped_at(index);
    }
    let observed = process.page.snapshot();
    assert_eq!(observed.stop_bitmap, 0xff);
    assert_eq!(observed.residual_bitmap, 0);
    assert_eq!(observed.exit_residual_bitmap, 0);
    assert_eq!(observed.fired_checkpoint, 0);
}
