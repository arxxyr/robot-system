//! `feetech_bench` —— Step 7 task 27c criterion bench harness。
//!
//! ## 入口
//!
//! ```bash
//! cargo bench -p robot-adapter-feetech                 # 全部组
//! cargo bench -p robot-adapter-feetech -- encoding     # 仅 encoding 组
//! cargo bench -p robot-adapter-feetech -- bus_sync_read --measurement-time 30
//! ```
//!
//! 报告生成到 `target/criterion/<group>/<bench>/index.html`。
//!
//! ## bench 分组（5 组）
//!
//! 1. **encoding** —— `encode/decode_sign_magnitude` + `split/combine_bytes_le`
//!    的纯算术开销。
//! 2. **protocol** —— `build_ping/read/write/sync_read/sync_write` 的 packet
//!    序列化开销。
//! 3. **parse** —— `parse_status` 的 wire → `StatusPacket` 反序列化开销。
//! 4. **bus** —— `FeetechBus` 单 motor `read/write` 端到端（含 mock transport
//!    write_all + read_exact + parse_status）。
//! 5. **bus_sync** —— `sync_read` / `sync_write` 在 13 motor（XLeRobot 双臂 +
//!    三轮）规模下的开销 —— 这是 task 28 真机最关心的 hot path。
//!
//! ## baseline 数值入档
//!
//! 跑完一次后把关键指标（每组 mean / p99 / 最快 + 最慢 case）人工填进
//! `docs/10_verification.md` 附录。**criterion 本机数值**仅作回归对照，
//! 真机延迟以 task 28 Orin USB-RS485 实测为准。

#![allow(clippy::doc_markdown)]

use std::collections::HashMap;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use robot_adapter_feetech::encoding::{
    combine_bytes_le, decode_sign_magnitude, encode_sign_magnitude, split_bytes_le,
};
use robot_adapter_feetech::protocol::{
    build_ping, build_read, build_sync_read, build_sync_write, build_write, checksum, parse_status,
};
use robot_adapter_feetech::transport::{MockHandle, MockTransport};
use robot_adapter_feetech::{FeetechBus, Motor};

// ============================================================================
// Group 1 · encoding
// ============================================================================

fn bench_encoding(c: &mut Criterion) {
    let mut g = c.benchmark_group("encoding");

    g.bench_function("encode_sign_magnitude_pos", |b| {
        b.iter(|| black_box(encode_sign_magnitude(black_box(2048), 15)).unwrap());
    });
    g.bench_function("encode_sign_magnitude_neg", |b| {
        b.iter(|| black_box(encode_sign_magnitude(black_box(-2047), 11)).unwrap());
    });
    g.bench_function("decode_sign_magnitude", |b| {
        b.iter(|| black_box(decode_sign_magnitude(black_box(0x8190), 15)));
    });

    let mut buf = [0u8; 4];
    g.bench_function("split_bytes_le_2", |b| {
        b.iter(|| split_bytes_le(black_box(0xABCD), 2, &mut buf).unwrap());
    });
    g.bench_function("split_bytes_le_4", |b| {
        b.iter(|| split_bytes_le(black_box(0xDEAD_BEEF), 4, &mut buf).unwrap());
    });

    let buf2 = [0xCD, 0xAB];
    let buf4 = [0xEF, 0xBE, 0xAD, 0xDE];
    g.bench_function("combine_bytes_le_2", |b| {
        b.iter(|| combine_bytes_le(black_box(&buf2), 2).unwrap());
    });
    g.bench_function("combine_bytes_le_4", |b| {
        b.iter(|| combine_bytes_le(black_box(&buf4), 4).unwrap());
    });

    g.finish();
}

// ============================================================================
// Group 2 · protocol packet build
// ============================================================================

fn bench_protocol_build(c: &mut Criterion) {
    let mut g = c.benchmark_group("protocol_build");

    let mut buf = [0u8; 32];
    g.bench_function("build_ping", |b| {
        b.iter(|| build_ping(black_box(1), &mut buf).unwrap());
    });
    g.bench_function("build_read", |b| {
        b.iter(|| build_read(black_box(1), 56, 2, &mut buf).unwrap());
    });
    g.bench_function("build_write_2byte", |b| {
        b.iter(|| build_write(black_box(1), 42, &[0x00, 0x08], &mut buf).unwrap());
    });

    // 13-motor sync_write（XLeRobot 双臂 + 三轮规模）
    let mut sync_buf = vec![0u8; 256];
    let entries: Vec<(u8, &[u8])> = (1..=13).map(|i| (i, [0x00u8, 0x08u8].as_slice())).collect();
    g.bench_function("build_sync_write_13motor_2byte", |b| {
        b.iter(|| {
            // 重新借出 entries（Vec<(u8, &[u8])>）
            build_sync_write(42, 2, black_box(&entries), &mut sync_buf).unwrap();
        });
    });
    let ids: Vec<u8> = (1..=13).collect();
    g.bench_function("build_sync_read_13motor", |b| {
        b.iter(|| {
            build_sync_read(56, 2, black_box(&ids), &mut sync_buf).unwrap();
        });
    });

    g.finish();
}

// ============================================================================
// Group 3 · parse_status
// ============================================================================

fn bench_parse(c: &mut Criterion) {
    let mut g = c.benchmark_group("parse");

    // ping ack（空 params）
    let ping_status = [0xFFu8, 0xFF, 0x01, 0x02, 0x00, 0xFC];
    g.bench_function("parse_status_empty", |b| {
        b.iter(|| parse_status(black_box(&ping_status)).unwrap());
    });

    // 2-byte payload（model_number / Present_Position）
    let two_byte = [0xFFu8, 0xFF, 0x05, 0x04, 0x00, 0x09, 0x03, 0xEA];
    g.bench_function("parse_status_2byte", |b| {
        b.iter(|| parse_status(black_box(&two_byte)).unwrap());
    });

    // checksum 计算（独立 bench，因为 build/parse 的核心开销）
    let scratch = [0x01u8, 0x05, 0x03, 0x2A, 0x00, 0x08];
    g.bench_function("checksum_6byte", |b| {
        b.iter(|| checksum(black_box(&scratch)));
    });

    g.finish();
}

// ============================================================================
// Group 4 · bus 单 motor read / write
// ============================================================================

fn make_bus_with_motors(motor_specs: &[(&str, u8)]) -> (FeetechBus<MockTransport>, MockHandle) {
    let (transport, handle) = MockTransport::new();
    let mut motors = HashMap::new();
    for (name, id) in motor_specs {
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

fn bench_bus(c: &mut Criterion) {
    let mut g = c.benchmark_group("bus");

    g.bench_function("write_goal_position_1motor", |b| {
        let (mut bus, handle) = make_bus_with_motors(&[("j1", 1)]);
        b.iter(|| {
            // 每次 iter 喂一个 ACK + 调 write
            handle.push_rx(&fake_status(1, &[]));
            bus.write(black_box("Goal_Position"), "j1", black_box(2048))
                .unwrap();
            // drain tx 防止 buffer 无限增长
            let _ = handle.drain_tx();
        });
    });

    g.bench_function("read_present_position_1motor", |b| {
        let (mut bus, handle) = make_bus_with_motors(&[("j1", 1)]);
        b.iter(|| {
            handle.push_rx(&fake_status(1, &[0x00, 0x08]));
            black_box(bus.read("Present_Position", "j1").unwrap());
            let _ = handle.drain_tx();
        });
    });

    g.finish();
}

// ============================================================================
// Group 5 · bus_sync 13 motor（XLeRobot 双臂 + 三轮规模）
// ============================================================================

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

fn bench_bus_sync(c: &mut Criterion) {
    let mut g = c.benchmark_group("bus_sync");

    let names: Vec<&str> = XLEROBOT_JOINTS.iter().map(|(n, _)| *n).collect();

    // sync_read 13 motor（task 28 真机 hot path）
    g.bench_function("sync_read_present_position_13motor", |b| {
        let (mut bus, handle) = make_bus_with_motors(XLEROBOT_JOINTS);
        b.iter(|| {
            // 13 个独立 motor status，每个 raw=0x0800
            for &(_, id) in XLEROBOT_JOINTS {
                handle.push_rx(&fake_status(id, &[0x00, 0x08]));
            }
            black_box(bus.sync_read("Present_Position", &names).unwrap());
            let _ = handle.drain_tx();
        });
    });

    // sync_write 13 motor（不 ACK，无 push_rx）
    g.bench_function("sync_write_goal_position_13motor", |b| {
        let (mut bus, _handle) = make_bus_with_motors(XLEROBOT_JOINTS);
        let values: Vec<(&str, i32)> = names.iter().map(|n| (*n, 2048_i32)).collect();
        b.iter(|| {
            bus.sync_write(black_box("Goal_Position"), black_box(&values))
                .unwrap();
        });
    });

    g.finish();
}

// ============================================================================
// Criterion entrypoint
// ============================================================================

criterion_group!(
    benches,
    bench_encoding,
    bench_protocol_build,
    bench_parse,
    bench_bus,
    bench_bus_sync,
);
criterion_main!(benches);
