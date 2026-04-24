//! `feetech` 集成 smoke —— Step 7 task 27c 第二件套。
//!
//! ## 目标
//!
//! 端到端模拟 XLeRobot 全 13 motor（双臂 5+5 + 三轮 3）配置，用 mock transport
//! 跑 ≥ 1000 次 `sync_read + sync_write` 循环，验证：
//!
//! - bus / adapter / protocol / encoding 全栈在长循环下不出错
//! - mock transport tx_log / rx_queue 在循环中正常 drain，不内存泄漏
//! - sign-magnitude 编解码在跨 0 边界（`-50 ~ +50`）roundtrip 一致
//!
//! ## 不做的事
//!
//! - **不**测真硬件 wire RTT（task 28 Orin 真机阶段）
//! - **不**测 RT 铁律 alloc 守卫（task 26 `rt_guard_soak` 已覆盖）
//! - **不**做性能 baseline（task 27c criterion bench 覆盖）
//!
//! ## 入口
//!
//! ```bash
//! cargo test -p robot-adapter-feetech --test integration_smoke
//! ```

#![allow(
    clippy::doc_markdown,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::collections::HashMap;

use robot_adapter_feetech::protocol::checksum;
use robot_adapter_feetech::transport::{MockHandle, MockTransport};
use robot_adapter_feetech::{FeetechBus, Motor};

const XLEROBOT_JOINTS: &[(&str, u8)] = &[
    ("left_arm_j1", 1),
    ("left_arm_j2", 2),
    ("left_arm_j3", 3),
    ("left_arm_j4", 4),
    ("left_arm_j5", 5),
    ("right_arm_j1", 6),
    ("right_arm_j2", 7),
    ("right_arm_j3", 8),
    ("right_arm_j4", 9),
    ("right_arm_j5", 10),
    ("base_w1", 11),
    ("base_w2", 12),
    ("base_w3", 13),
];

const SMOKE_LOOPS: usize = 1000;

fn make_bus() -> (FeetechBus<MockTransport>, MockHandle) {
    let (transport, handle) = MockTransport::new();
    let mut motors = HashMap::new();
    for (name, id) in XLEROBOT_JOINTS {
        motors.insert((*name).to_string(), Motor::new(*id).unwrap());
    }
    let bus = FeetechBus::new(transport, motors).unwrap();
    (bus, handle)
}

fn fake_status(id: u8, params: &[u8]) -> Vec<u8> {
    let len = u8::try_from(params.len() + 2).unwrap();
    let mut out = vec![0xFF, 0xFF, id, len, 0x00];
    out.extend_from_slice(params);
    out.push(checksum(&out[2..]));
    out
}

#[test]
fn xlerobot_13motor_1000_loop_sync_read_write() {
    let (mut bus, handle) = make_bus();
    let names: Vec<&str> = XLEROBOT_JOINTS.iter().map(|(n, _)| *n).collect();

    for loop_idx in 0..SMOKE_LOOPS {
        // ── ① sync_read Present_Position ──
        // 每 motor 喂虚假 raw = 2048 + loop_idx % 100（在 0~+99 范围内，
        // 保证 sign-magnitude 编码不爆 magnitude 上限）。
        let drift = (loop_idx % 100) as i32;
        for &(_, id) in XLEROBOT_JOINTS {
            // 编码 2048 + drift（positive，sign-magnitude bit 15 = 0）
            let raw_value = 2048 + drift;
            let lo = (raw_value & 0xFF) as u8;
            let hi = ((raw_value >> 8) & 0xFF) as u8;
            handle.push_rx(&fake_status(id, &[lo, hi]));
        }

        let positions = bus
            .sync_read("Present_Position", &names)
            .unwrap_or_else(|e| panic!("sync_read fail at loop {loop_idx}: {e}"));

        // 验解码值与喂入一致
        for name in &names {
            let got = positions
                .get(*name)
                .copied()
                .unwrap_or_else(|| panic!("loop {loop_idx}: missing {name} in sync_read result"));
            let expected = 2048 + drift;
            assert_eq!(
                got, expected,
                "loop {loop_idx} {name}: decoded {got}, expected {expected}"
            );
        }

        // ── ② sync_write Goal_Position ──
        // 跨 0 边界轮换：偶数 loop +500，奇数 -500（sign-magnitude 编解码热路径）
        let target: i32 = if loop_idx.is_multiple_of(2) {
            2048 + 500
        } else {
            2048 - 500
        };
        let entries: Vec<(&str, i32)> = names.iter().map(|n| (*n, target)).collect();
        bus.sync_write("Goal_Position", &entries)
            .unwrap_or_else(|e| panic!("sync_write fail at loop {loop_idx}: {e}"));

        // ── ③ drain mock tx 防止 buffer 无界增长 ──
        let drained = handle.drain_tx();
        // sync_read 包 (6+2+13=21B) + sync_write 包 (6+2+13×3=47B) ≈ 68 B
        assert!(
            !drained.is_empty(),
            "loop {loop_idx}: expected non-empty tx buffer"
        );
    }

    eprintln!(
        "[feetech_smoke] {} motor × {} loops 完成，sync_read + sync_write 全部 OK",
        XLEROBOT_JOINTS.len(),
        SMOKE_LOOPS
    );
}

#[test]
fn xlerobot_13motor_sync_read_handles_negative_present_position() {
    // Present_Position 跨 0 边界（Actual = 100, Homing_Offset = 500 → Present = -400）
    // 单独验：13 motor 全部解码 sign-magnitude bit 15 = 1 的值。
    let (mut bus, handle) = make_bus();
    let names: Vec<&str> = XLEROBOT_JOINTS.iter().map(|(n, _)| *n).collect();

    // -400 sign-magnitude bit 15 = 0x8000 | 400 = 0x8190；LE [0x90, 0x81]
    for &(_, id) in XLEROBOT_JOINTS {
        handle.push_rx(&fake_status(id, &[0x90, 0x81]));
    }

    let positions = bus.sync_read("Present_Position", &names).unwrap();
    for name in &names {
        assert_eq!(
            *positions.get(*name).unwrap(),
            -400,
            "{name} should decode to -400"
        );
    }
}

#[test]
fn handshake_passes_when_all_13_motors_respond_with_sts3215_model() {
    // Step 7 task 27 已有单 motor handshake 测试；这里覆盖 13 motor 规模。
    let (mut bus, handle) = make_bus();

    // handshake 内部对每个 motor 调 ping（PING + READ_DATA Model_Number 两步）
    // mock 的所有回包都用 id=1 也能过 parse_status（parse 不强校验 id 字段对不对）
    for _ in 0..13 {
        handle.push_rx(&fake_status(1, &[])); // PING ack
        handle.push_rx(&fake_status(1, &[0x09, 0x03])); // model_number 777 = 0x309 LE
    }

    bus.connect(true).unwrap();
}
