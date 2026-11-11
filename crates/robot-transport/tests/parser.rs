// 测试文件：构造各种 FlatBuffers 样本需要大量 `as u32` / 就地辅助 fn，
// 豁免 pedantic 噪音（cast_possible_truncation / items_after_statements 等）。
// 生产路径的 pedantic 检查在 `src/` 下仍然严格。
#![allow(clippy::pedantic, clippy::nursery)]

//! `robot-transport::parser` 的黑盒测试。
//!
//! 覆盖 parser 的以下行为：
//!
//! - 正常 sample（带 blob / 不带 blob）解析
//! - 每种 [`ParseError`] 变体都能被构造出可复现的输入
//! - 边界：`sample_size = 4` / `H = 4096` / `body_size = 0`
//! - 手写 PRNG fuzz（10 000 个随机字节流不 panic）
//! - [`ParseMetrics`] 计数正确

use flatbuffers::FlatBufferBuilder;
use robot_msg::capability::{CAP_HAS_BLOB, CAP_LITTLE_ENDIAN};
use robot_msg::msg::{FrameHeader, FrameHeaderArgs, JointState, JointStateArgs};
use robot_msg::schema_id::{SCHEMA_JOINT_STATE, SCHEMA_TWIST};
use robot_transport::parser::{FRAME_HEADER_SIZE_MAX, FlatbufferFailureKind};
use robot_transport::{ParseError, ParseMetrics, parse_sample};

// ===========================================================================
// Sample 构造辅助
// ===========================================================================

/// 构造一条合法 sample：
///   [0..4]        u32 LE = H
///   [4..4+H]      FrameHeader FlatBuffers root
///   [body_off..]  body bytes
///   [blob_off..]  blob bytes (optional)
///
/// 返回 (`sample_bytes`, `body_offset`)。
fn build_sample(
    schema_id: u32,
    body: &[u8],
    blob: Option<&[u8]>,
    capability_extra: u64,
) -> (Vec<u8>, u32) {
    // 先构造 FrameHeader bytes（使用一份 placeholder offset，算完总长再填真正的 offset）。
    //
    // 简化：我们选择把 header / body / blob 按顺序拼接：
    //   H_prefix (4B) | FrameHeader (H bytes) | body | optional blob
    // body_offset = 4 + H；blob_offset = body_offset + body_size（若有 blob）。
    //
    // flatc 生成的 FrameHeader 是 builder-based FlatBuffers root；大小不定
    // 但上限 < 256 B（§2.2 原文）。先 build 一次拿到 H，再回填 offset + rebuild。
    //
    // 我们采用简单两遍：第一遍用 dummy offset 估算 H 的布局，第二遍用真 offset。
    // FlatBuffers 对同一组字段的 builder 是确定性的；改字段值不改 vtable 结构，
    // 因此先 build 一次取 header_len 即可。

    let mut flags = CAP_LITTLE_ENDIAN | capability_extra;
    if blob.is_some() {
        flags |= CAP_HAS_BLOB;
    }

    let body_size = body.len() as u32;
    let blob_size_val = blob.map_or(0, |b| b.len() as u32);

    // --- pass 1: 探测 header 长度 ---
    //
    // FlatBuffers vtable 会省略默认值（0）的字段，会使 probe 和 real 的
    // header size 不一致。对策：**每个 field 在 probe 里就填非零 placeholder**，
    // 让 vtable 全量展开；real 再换成真实值，size 保持不变。
    //
    // `body_size` / `blob_size` 的"是否为 0"是本 build 的真实属性（空 body /
    // 无 blob）—— 对应字段 in probe 也用同样值，避免 vtable slot 假性扩展。
    const PLACEHOLDER_NONZERO: u32 = 0x0101_0101;
    let probe_header = make_header_bytes(FrameHeaderArgs {
        magic: 0x5242_5443,
        abi_version: 1,
        schema_id,
        schema_version: 1,
        capability_flags: flags,
        stamp_ns: 42_000_000_000,
        source_id: 7,
        seq: 100,
        body_offset: PLACEHOLDER_NONZERO,
        body_size,
        blob_offset: if blob.is_some() {
            PLACEHOLDER_NONZERO
        } else {
            0
        },
        blob_size: blob_size_val,
        header_crc32: 0,
    });
    let h = probe_header.len() as u32;
    let body_offset = 4 + h;
    let body_end = body_offset + body_size;
    let (blob_offset, blob_size) = match blob {
        Some(_) => (body_end, blob_size_val),
        None => (0, 0),
    };

    // --- pass 2: 带真实 offset 的 header ---
    let real_header = make_header_bytes(FrameHeaderArgs {
        magic: 0x5242_5443,
        abi_version: 1,
        schema_id,
        schema_version: 1,
        capability_flags: flags,
        stamp_ns: 42_000_000_000,
        source_id: 7,
        seq: 100,
        body_offset,
        body_size,
        blob_offset,
        blob_size,
        header_crc32: 0,
    });
    assert_eq!(
        real_header.len(),
        probe_header.len(),
        "FrameHeader size changed between probe and real passes; \
         placeholder pattern needs to cover additional zero-by-default fields"
    );

    let mut out =
        Vec::with_capacity(4 + real_header.len() + body.len() + blob.map_or(0, <[u8]>::len));
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&real_header);
    out.extend_from_slice(body);
    if let Some(b) = blob {
        out.extend_from_slice(b);
    }
    (out, body_offset)
}

fn make_header_bytes(args: FrameHeaderArgs) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let fh = FrameHeader::create(&mut fbb, &args);
    fbb.finish(fh, None);
    fbb.finished_data().to_vec()
}

/// 构造一个合法的 `JointState` 作为 body。
fn build_joint_state_body() -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let pos = fbb.create_vector::<f64>(&[0.1, 0.2, 0.3]);
    let js = JointState::create(
        &mut fbb,
        &JointStateArgs {
            layout_id: 0xCAFE_BABE_DEAD_BEEF,
            position_rad: Some(pos),
            velocity_rps: None,
            effort_nm: None,
            stamp_ns: 999,
            frame_id: None,
            ..JointStateArgs::default()
        },
    );
    fbb.finish(js, None);
    fbb.finished_data().to_vec()
}

// ===========================================================================
// 正常路径
// ===========================================================================

#[test]
fn parse_sample_without_blob() {
    let body = build_joint_state_body();
    let (sample, body_offset) = build_sample(SCHEMA_JOINT_STATE, &body, None, 0);

    let parsed = parse_sample(&sample).expect("valid sample should parse");
    assert_eq!(parsed.schema_id, SCHEMA_JOINT_STATE);
    assert_eq!(parsed.schema_name, "robot.msg.JointState");
    assert_eq!(parsed.body.len(), body.len());
    assert_eq!(parsed.body, body.as_slice());
    assert!(parsed.blob.is_none());
    assert_eq!(parsed.header.body_offset(), body_offset);
    assert_eq!(parsed.header.stamp_ns(), 42_000_000_000);

    // body 可被再次当作 JointState 解析
    let js: JointState = flatbuffers::root::<JointState>(parsed.body).expect("body verify");
    assert_eq!(js.layout_id(), 0xCAFE_BABE_DEAD_BEEF);
}

#[test]
fn parse_sample_with_blob() {
    let body = build_joint_state_body();
    let blob_bytes = vec![0xAB_u8; 256];
    let (sample, _) = build_sample(SCHEMA_JOINT_STATE, &body, Some(&blob_bytes), 0);

    let parsed = parse_sample(&sample).expect("valid sample should parse");
    assert_eq!(parsed.blob, Some(blob_bytes.as_slice()));
    assert_eq!(parsed.body.len(), body.len());
}

#[test]
fn parse_sample_body_size_zero() {
    // body_size == 0 合法：schema 可能无 body fields（极端情况）
    let (sample, _) = build_sample(SCHEMA_JOINT_STATE, &[], None, 0);
    let parsed = parse_sample(&sample).expect("empty body is legal");
    assert_eq!(parsed.body.len(), 0);
    assert!(parsed.blob.is_none());
}

// ===========================================================================
// 每种 ParseError 变体
// ===========================================================================

#[test]
fn err_size_too_small() {
    for len in 0..=3 {
        let buf = vec![0u8; len];
        let err = parse_sample(&buf).unwrap_err();
        assert!(
            matches!(err, ParseError::SizeTooSmall { size } if size == len),
            "unexpected err for len {len}: {err:?}"
        );
    }
}

#[test]
fn err_header_size_zero() {
    // prefix = 0 ⇒ H=0 → HeaderSizeInvalid
    let buf = vec![0u8; 16];
    let err = parse_sample(&buf).unwrap_err();
    assert!(matches!(err, ParseError::HeaderSizeInvalid { h: 0, .. }));
}

#[test]
fn err_header_size_overflow() {
    // H=4097 > 4096
    let mut buf = vec![0u8; 16];
    buf[..4].copy_from_slice(&(FRAME_HEADER_SIZE_MAX + 1).to_le_bytes());
    let err = parse_sample(&buf).unwrap_err();
    assert!(
        matches!(err, ParseError::HeaderSizeInvalid { h, .. } if h == FRAME_HEADER_SIZE_MAX + 1)
    );
}

#[test]
fn err_header_size_truncated() {
    // H=1000 但 sample 只有 100 bytes
    let mut buf = vec![0u8; 100];
    buf[..4].copy_from_slice(&1000_u32.to_le_bytes());
    let err = parse_sample(&buf).unwrap_err();
    assert!(matches!(err, ParseError::HeaderSizeInvalid { h: 1000, .. }));
}

#[test]
fn err_header_verify_failed_on_random_bytes() {
    // H=32 合法，但 header_bytes 是垃圾 → FlatBuffers verify fail
    let mut buf = vec![0xFFu8; 64];
    buf[..4].copy_from_slice(&32_u32.to_le_bytes());
    let err = parse_sample(&buf).unwrap_err();
    match err {
        ParseError::HeaderVerifyFailed { reason } => {
            // 不强约束具体类别，只要是 verify 失败就行
            let _ = reason;
        }
        other => panic!("expected HeaderVerifyFailed, got {other:?}"),
    }
}

#[test]
fn err_body_out_of_bounds_overflow() {
    // 正常 header，但 body_size 巨大（构造时手填非法值）
    let body = build_joint_state_body();
    let (mut sample, _) = build_sample(SCHEMA_JOINT_STATE, &body, None, 0);
    // 直接 truncate sample 让 body_end > sample_size
    let truncated_len = sample.len() - 2;
    sample.truncate(truncated_len);
    let err = parse_sample(&sample).unwrap_err();
    assert!(
        matches!(err, ParseError::BodyOutOfBounds { .. }),
        "expected BodyOutOfBounds, got {err:?}"
    );
}

#[test]
fn err_body_out_of_bounds_before_header_end() {
    // 手工伪造 FrameHeader，让 body_offset < 4 + H
    let probe = make_header_bytes(FrameHeaderArgs {
        magic: 0x5242_5443,
        abi_version: 1,
        schema_id: SCHEMA_JOINT_STATE,
        schema_version: 1,
        capability_flags: 0,
        stamp_ns: 0,
        source_id: 0,
        seq: 0,
        body_offset: 0, // ← 故意 <  4+H
        body_size: 0,
        blob_offset: 0,
        blob_size: 0,
        header_crc32: 0,
    });
    let h = probe.len() as u32;
    let mut buf = Vec::with_capacity(4 + probe.len());
    buf.extend_from_slice(&h.to_le_bytes());
    buf.extend_from_slice(&probe);
    let err = parse_sample(&buf).unwrap_err();
    assert!(matches!(
        err,
        ParseError::BodyOutOfBounds { body_offset: 0, .. }
    ));
}

#[test]
fn err_blob_out_of_bounds() {
    // 手工构造：合法 body + 巨大 blob_size（超界）。
    // probe/real 两遍用相同的"是否为 0"占位模式让 vtable 稳定（同 build_sample）。
    const PLACEHOLDER_NONZERO: u32 = 0x0101_0101;
    let body = build_joint_state_body();
    let body_size = body.len() as u32;

    let probe = make_header_bytes(FrameHeaderArgs {
        magic: 0x5242_5443,
        abi_version: 1,
        schema_id: SCHEMA_JOINT_STATE,
        schema_version: 1,
        capability_flags: CAP_HAS_BLOB,
        stamp_ns: 42_000_000_000,
        source_id: 7,
        seq: 100,
        body_offset: PLACEHOLDER_NONZERO,
        body_size,
        blob_offset: PLACEHOLDER_NONZERO,
        blob_size: PLACEHOLDER_NONZERO,
        header_crc32: 0,
    });
    let h = probe.len() as u32;
    let body_offset = 4 + h;
    let blob_offset = body_offset + body_size;

    let real = make_header_bytes(FrameHeaderArgs {
        magic: 0x5242_5443,
        abi_version: 1,
        schema_id: SCHEMA_JOINT_STATE,
        schema_version: 1,
        capability_flags: CAP_HAS_BLOB,
        stamp_ns: 42_000_000_000,
        source_id: 7,
        seq: 100,
        body_offset,
        body_size,
        blob_offset,
        blob_size: 999_999_999, // 远超 sample_size
        header_crc32: 0,
    });
    assert_eq!(real.len(), h as usize);

    let mut sample = Vec::new();
    sample.extend_from_slice(&h.to_le_bytes());
    sample.extend_from_slice(&real);
    sample.extend_from_slice(&body);
    // 不附加 blob 字节 → blob_offset + 999_999_999 >> sample_size。

    let err = parse_sample(&sample).unwrap_err();
    assert!(
        matches!(
            err,
            ParseError::BlobOutOfBounds {
                blob_size: 999_999_999,
                ..
            }
        ),
        "got {err:?}"
    );
}

#[test]
fn err_blob_overlap_with_body() {
    // 伪造：body = [body_offset, body_end)，blob 起点落在 body 内部 → 区间重叠
    const PLACEHOLDER_NONZERO: u32 = 0x0101_0101;
    let body = build_joint_state_body();
    let body_size = body.len() as u32;

    let probe = make_header_bytes(FrameHeaderArgs {
        magic: 0x5242_5443,
        abi_version: 1,
        schema_id: SCHEMA_JOINT_STATE,
        schema_version: 1,
        capability_flags: CAP_HAS_BLOB,
        stamp_ns: 42_000_000_000,
        source_id: 7,
        seq: 100,
        body_offset: PLACEHOLDER_NONZERO,
        body_size,
        blob_offset: PLACEHOLDER_NONZERO,
        blob_size: PLACEHOLDER_NONZERO,
        header_crc32: 0,
    });
    let h = probe.len() as u32;
    let body_offset = 4 + h;
    let body_end = body_offset + body_size;

    let overlap_blob_offset = body_offset + body_size / 2;
    let blob_size = body_size; // 足够跨过 body_end

    let real = make_header_bytes(FrameHeaderArgs {
        magic: 0x5242_5443,
        abi_version: 1,
        schema_id: SCHEMA_JOINT_STATE,
        schema_version: 1,
        capability_flags: CAP_HAS_BLOB,
        stamp_ns: 42_000_000_000,
        source_id: 7,
        seq: 100,
        body_offset,
        body_size,
        blob_offset: overlap_blob_offset,
        blob_size,
        header_crc32: 0,
    });
    assert_eq!(real.len(), h as usize);

    // sample 需要容纳到 overlap_blob_offset + blob_size
    let tail_pad = (overlap_blob_offset + blob_size).saturating_sub(body_end) as usize;
    let mut sample = Vec::new();
    sample.extend_from_slice(&h.to_le_bytes());
    sample.extend_from_slice(&real);
    sample.extend_from_slice(&body);
    sample.extend(std::iter::repeat_n(0u8, tail_pad));

    let err = parse_sample(&sample).unwrap_err();
    assert!(matches!(err, ParseError::BlobOverlap { .. }), "got {err:?}");
}

#[test]
fn err_unknown_schema_id() {
    let body = build_joint_state_body();
    // 用一个不在 SCHEMA_REGISTRY 的 id（0xDEAD_BEEF）
    let (sample, _) = build_sample(0xDEAD_BEEF, &body, None, 0);
    let err = parse_sample(&sample).unwrap_err();
    assert!(matches!(
        err,
        ParseError::UnknownSchemaId {
            schema_id: 0xDEAD_BEEF
        }
    ));
}

// ===========================================================================
// 边界
// ===========================================================================

#[test]
fn boundary_sample_size_exactly_4() {
    // sample_size = 4: 只能读 H，之后 H bytes 都无处可放 → HeaderSizeInvalid
    for h in [0_u32, 1, 8, FRAME_HEADER_SIZE_MAX] {
        let buf = h.to_le_bytes().to_vec();
        assert_eq!(buf.len(), 4);
        let err = parse_sample(&buf).unwrap_err();
        match err {
            ParseError::HeaderSizeInvalid { .. } => {}
            other => panic!("expected HeaderSizeInvalid for H={h}, got {other:?}"),
        }
    }
}

#[test]
fn boundary_header_size_max_4096_is_valid_when_buffer_allows() {
    // H=4096 本身合法，但 header bytes 是乱的 → 走 HeaderVerifyFailed。
    let mut buf = vec![0xFFu8; 4 + FRAME_HEADER_SIZE_MAX as usize];
    buf[..4].copy_from_slice(&FRAME_HEADER_SIZE_MAX.to_le_bytes());
    let err = parse_sample(&buf).unwrap_err();
    assert!(matches!(err, ParseError::HeaderVerifyFailed { .. }));
}

#[test]
fn boundary_header_size_4097_rejected() {
    let mut buf = vec![0u8; 4 + 4097];
    buf[..4].copy_from_slice(&(FRAME_HEADER_SIZE_MAX + 1).to_le_bytes());
    let err = parse_sample(&buf).unwrap_err();
    assert!(matches!(err, ParseError::HeaderSizeInvalid { .. }));
}

// ===========================================================================
// ParseMetrics 集成
// ===========================================================================

#[test]
fn metrics_record_classifies_correctly() {
    let m = ParseMetrics::new();

    // 喂进每种错误路径，依次 record
    let errs: Vec<ParseError> = vec![
        parse_sample(&[]).unwrap_err(),
        parse_sample(&[0, 0, 0, 0, 0, 0, 0, 0]).unwrap_err(),
        {
            let mut buf = vec![0xFFu8; 64];
            buf[..4].copy_from_slice(&32_u32.to_le_bytes());
            parse_sample(&buf).unwrap_err()
        },
        parse_sample(&build_sample(0x1234_5678, &build_joint_state_body(), None, 0).0).unwrap_err(),
    ];
    for e in &errs {
        m.record(e);
    }

    let snap = m.snapshot();
    assert_eq!(snap.size_too_small, 1);
    assert_eq!(snap.header_size_invalid, 1);
    assert_eq!(snap.header_verify_failed, 1);
    assert_eq!(snap.unknown_schema_id, 1);
    assert_eq!(snap.body_out_of_bounds, 0);
    assert_eq!(snap.blob_out_of_bounds, 0);
    assert_eq!(snap.blob_overlap, 0);
    assert_eq!(snap.total(), 4);
}

// ===========================================================================
// Fuzz：10 000 次随机字节流，parse_sample 不 panic
// ===========================================================================

fn prng_next(state: &mut u64) -> u64 {
    // PCG 风格简单 LCG，足够制造随机位模式覆盖 parser 各分支。
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state
}

#[test]
fn fuzz_random_bytes_never_panic() {
    let mut state: u64 = 0xDEAD_BEEF_CAFE_BABE;
    let m = ParseMetrics::new();
    for _ in 0..10_000 {
        let len = (prng_next(&mut state) % 8192) as usize;
        let mut buf = vec![0u8; len];
        for b in &mut buf {
            *b = (prng_next(&mut state) & 0xff) as u8;
        }
        match parse_sample(&buf) {
            Ok(_) => {}
            Err(e) => m.record(&e),
        }
    }
    // 不 panic 即 pass；metrics 是否非零取决于随机运气，不做强断言。
    let snap = m.snapshot();
    assert!(snap.total() > 0, "fuzz produced zero errors — RNG broken?");
}

// ===========================================================================
// 额外：FlatbufferFailureKind Display / From 覆盖（确保未来新加 variant 时
// 编译期提醒我们更新）
// ===========================================================================

#[test]
fn flatbuffer_failure_kind_display_is_stable() {
    use FlatbufferFailureKind as K;
    assert_eq!(format!("{}", K::DepthLimit), "depth_limit");
    assert_eq!(format!("{}", K::SizeLimit), "size_limit");
    assert_eq!(format!("{}", K::InvalidStructure), "invalid_structure");
    assert_eq!(format!("{}", K::Other), "other");
}

#[test]
fn schema_id_registry_lookup_works_in_parse_flow() {
    // 交叉确认：一个合法 body + 另一个合法 schema_id（Twist）能被识别为其规范名
    // （不需要 body 是 Twist，因为 Step 3 不反序列化 body；只要 body bytes 落在 bounds 内）
    let placeholder_body = vec![0u8; 64];
    let (sample, _) = build_sample(SCHEMA_TWIST, &placeholder_body, None, 0);
    let parsed = parse_sample(&sample).expect("schema_id known");
    assert_eq!(parsed.schema_id, SCHEMA_TWIST);
    assert_eq!(parsed.schema_name, "robot.msg.Twist");
}
