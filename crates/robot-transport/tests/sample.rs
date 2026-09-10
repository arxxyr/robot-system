// 测试文件：大量 `as u32` / trait object 构造，豁免 pedantic 噪音。
#![allow(clippy::pedantic, clippy::nursery)]
#![cfg(feature = "mock")]

//! Step 4 集成测试：`SampleLease` / `SampleWriter` / mock 后端。
//!
//! 覆盖 `docs/01_implementation_steps.md` Step 4 "测试"：
//! - round-trip：publisher.loan → writer.publish → subscriber.latest → view
//! - SampleLease drop 归还 loan（mock 计数器）
//! - SampleWriter drop without publish = discard
//! - schema_id 不匹配报错（SchemaMismatch）
//! - blob alignment 不满足报错（BlobMisaligned）

use flatbuffers::FlatBufferBuilder;
use robot_msg::markers::{self, BodyMarker};
use robot_msg::msg::{
    BlobDesc, BlobDescArgs, BlobLayout, Compression, Dtype, Endian, ImageHeader as FbImageHeader,
    ImageHeaderArgs, JointState as FbJointState, JointStateArgs,
};
use robot_transport::TransportError;
use robot_transport::mock::MockChannel;
use robot_transport::traits::{LatestSubscriber, Publisher};

// ---------------------------------------------------------------------------
// 辅助：构造 JointState body bytes
// ---------------------------------------------------------------------------

fn build_joint_state_body() -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let pos = fbb.create_vector::<f64>(&[0.1, 0.2, 0.3]);
    let js = FbJointState::create(
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

/// 构造 ImageHeader body，允许调用方指定 alignment（用于测 BlobMisaligned）。
fn build_image_header_body(alignment: u16) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let shape = fbb.create_vector::<u32>(&[2, 2, 3]);
    let blob = BlobDesc::create(
        &mut fbb,
        &BlobDescArgs {
            offset: 0,
            size: 12,
            alignment,
            dtype: Dtype::U8,
            shape: Some(shape),
            strides: None,
            layout: BlobLayout::HWC,
            endian: Endian::Little,
            compression: Compression::None_,
            crc32: 0,
        },
    );
    let enc = fbb.create_string("rgb8");
    let co = fbb.create_string("RGB");
    let fid = fbb.create_string("head_cam");
    let ih = FbImageHeader::create(
        &mut fbb,
        &ImageHeaderArgs {
            blob: Some(blob),
            width: 2,
            height: 2,
            encoding: Some(enc),
            step: 6,
            color_order: Some(co),
            stamp_ns: 111,
            frame_id: Some(fid),
        },
    );
    fbb.finish(ih, None);
    fbb.finished_data().to_vec()
}

// ---------------------------------------------------------------------------
// round-trip
// ---------------------------------------------------------------------------

#[test]
fn publish_then_subscribe_roundtrip() {
    let ch = MockChannel::<markers::JointState>::new();
    let mut publisher = ch.publisher();
    let mut subscriber = ch.subscriber();

    let body = build_joint_state_body();
    let mut w = publisher.loan(65536).expect("loan");
    w.set_stamp_ns(42);
    w.set_seq(100);
    w.set_source_id(7);
    w.set_body_bytes(&body);
    w.publish().expect("publish");

    assert_eq!(ch.publishes(), 1);
    assert_eq!(ch.active_writers(), 0);

    let lease = subscriber
        .latest()
        .expect("latest ok")
        .expect("should have sample");
    assert_eq!(ch.active_leases(), 1);

    let view = lease.view().expect("view verifies");
    assert_eq!(view.header.schema_id(), markers::JointState::SCHEMA_ID);
    assert_eq!(view.header.stamp_ns(), 42);
    assert_eq!(view.header.seq(), 100);
    assert_eq!(view.header.source_id(), 7);
    assert!(!view.has_blob_flag());

    let js = view.body.get();
    assert_eq!(js.layout_id(), 0xCAFE_BABE_DEAD_BEEF);
    let pos = js.position_rad().unwrap();
    assert_eq!(pos.len(), 3);
    assert!((pos.get(1) - 0.2).abs() < 1e-12);

    // view 不 impl Drop，clippy 抱怨；让编译器根据 NLL 自己处理。
    let _ = view;
    drop(lease);
    assert_eq!(ch.active_leases(), 0);
}

#[test]
fn lease_drop_returns_loan_even_without_view() {
    let ch = MockChannel::<markers::JointState>::new();
    let mut publisher = ch.publisher();
    let mut subscriber = ch.subscriber();

    let mut w = publisher.loan(65536).unwrap();
    w.set_body_bytes(&build_joint_state_body());
    w.publish().unwrap();

    let lease1 = subscriber.latest().unwrap().unwrap();
    assert_eq!(ch.active_leases(), 1);
    let lease2 = subscriber.latest().unwrap().unwrap();
    assert_eq!(ch.active_leases(), 2);
    drop(lease1);
    assert_eq!(ch.active_leases(), 1);
    drop(lease2);
    assert_eq!(ch.active_leases(), 0);
}

// ---------------------------------------------------------------------------
// writer 资源守恒
// ---------------------------------------------------------------------------

#[test]
fn writer_drop_without_publish_discards_loan() {
    let ch = MockChannel::<markers::JointState>::new();
    let mut publisher = ch.publisher();

    {
        let mut w = publisher.loan(65536).unwrap();
        w.set_body_bytes(&build_joint_state_body());
        assert_eq!(ch.active_writers(), 1);
        assert_eq!(ch.loans(), 1);
        // drop without publish
    }
    assert_eq!(ch.active_writers(), 0);
    assert_eq!(ch.publishes(), 0); // 没 publish
    assert_eq!(ch.loans(), 1);
}

#[test]
fn writer_publish_consumes_and_increments_publish_count() {
    let ch = MockChannel::<markers::JointState>::new();
    let mut publisher = ch.publisher();

    let mut w = publisher.loan(65536).unwrap();
    w.set_body_bytes(&build_joint_state_body());
    assert_eq!(ch.active_writers(), 1);
    w.publish().unwrap();
    assert_eq!(ch.active_writers(), 0);
    assert_eq!(ch.publishes(), 1);
    assert_eq!(ch.loans(), 1);
}

#[test]
fn second_loan_after_publish_works() {
    // 验证 writer.publish(self) 后 publisher 借用已释放，可以再次 loan。
    let ch = MockChannel::<markers::JointState>::new();
    let mut publisher = ch.publisher();

    let mut w1 = publisher.loan(65536).unwrap();
    w1.set_body_bytes(&build_joint_state_body());
    w1.publish().unwrap();

    let mut w2 = publisher.loan(65536).unwrap();
    w2.set_body_bytes(&build_joint_state_body());
    w2.publish().unwrap();

    assert_eq!(ch.publishes(), 2);
    assert_eq!(ch.active_writers(), 0);
}

// ---------------------------------------------------------------------------
// schema_id 不匹配
//
// SchemaMismatch 的设计情景是 "wire 格式里的 schema_id 与订阅方 marker 不
// 一致"，例如 recorder replay 旧快照 / 跨机 bridge 编码 bug。当前 mock API
// 里 publisher 注入的 schema_id 永远 = T::SCHEMA_ID，且 SampleLease<T> 的 T
// 由 subscriber 类型绑死，用户代码无法在**正常** API 下构造 mismatch。
//
// Step 5 接入 iceoryx2 后可加 raw bytes inject helper 覆盖此路径；现在留
// TODO，SchemaMismatch 错误类型已在 src/error.rs 和 sample.rs 实现，单元
// 测试靠 Display 字符串验证（见 tests/parser.rs 的 schema_id 分类）。
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// blob alignment
// ---------------------------------------------------------------------------

#[test]
fn blob_misaligned_when_offset_not_multiple_of_alignment() {
    // ImageHeader blob 声明 alignment=16；写一个会落在 odd offset 的 blob
    // 使 blob_offset % 16 != 0 → BlobMisaligned。
    //
    // mock 的 blob_offset = body_offset + body_size；要让它不是 16 倍数，
    // 只要 body 长度不是 16 的倍数 + body_offset 不是 16 的倍数即可。
    let ch = MockChannel::<markers::ImageHeader>::new();
    let mut publisher = ch.publisher();
    let mut subscriber = ch.subscriber();

    let body = build_image_header_body(16); // alignment = 16
    let blob = vec![0xABu8; 12]; // 12 bytes，凑 body + blob 长度

    let mut w = publisher.loan(65536).unwrap();
    w.set_body_bytes(&body);
    w.set_blob_bytes(&blob);
    w.publish().unwrap();

    let lease = subscriber.latest().unwrap().unwrap();
    match lease.view() {
        Ok(_) => {
            // 如果碰巧 blob_offset 是 16 倍数（实际 header + body 尺寸决定），
            // 本用例就不触发；换 alignment=1024 再试一次。
            let body2 = build_image_header_body(1024);
            let blob2 = vec![0xABu8; 12];
            let mut w2 = publisher.loan(65536).unwrap();
            w2.set_body_bytes(&body2);
            w2.set_blob_bytes(&blob2);
            w2.publish().unwrap();
            let lease2 = subscriber.latest().unwrap().unwrap();
            match lease2.view() {
                Err(TransportError::BlobMisaligned {
                    blob_offset: _,
                    alignment: 1024,
                }) => {}
                other => panic!(
                    "expected BlobMisaligned(alignment=1024), got {:?}",
                    other.map(|_| "Ok(view)")
                ),
            }
        }
        Err(TransportError::BlobMisaligned { alignment: 16, .. }) => {
            // pass
        }
        Err(other) => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn blob_aligned_blob_passes_view() {
    // alignment = 1 对任意 blob_offset 都过。
    let ch = MockChannel::<markers::ImageHeader>::new();
    let mut publisher = ch.publisher();
    let mut subscriber = ch.subscriber();

    let body = build_image_header_body(1);
    let blob = vec![0xCDu8; 12];
    let mut w = publisher.loan(65536).unwrap();
    w.set_body_bytes(&body);
    w.set_blob_bytes(&blob);
    w.publish().unwrap();

    let lease = subscriber.latest().unwrap().unwrap();
    let view = lease.view().expect("alignment=1 always succeeds");
    let blob_view = view.blob.expect("blob present");
    assert_eq!(blob_view.desc().alignment, 1);
    assert_eq!(blob_view.desc().size, 12);
    assert_eq!(blob_view.bytes().len(), 12);
    assert_eq!(blob_view.bytes()[0], 0xCD);
}

// ---------------------------------------------------------------------------
// subscriber 空
// ---------------------------------------------------------------------------

#[test]
fn subscriber_empty_returns_ok_none() {
    let ch = MockChannel::<markers::JointState>::new();
    let mut subscriber = ch.subscriber();
    assert!(subscriber.latest().unwrap().is_none());
    assert_eq!(ch.active_leases(), 0);
}

// ---------------------------------------------------------------------------
// latest 语义：多次 latest 拿到同一 sample
// ---------------------------------------------------------------------------

#[test]
fn latest_returns_same_sample_repeatedly_until_new_publish() {
    let ch = MockChannel::<markers::JointState>::new();
    let mut publisher = ch.publisher();
    let mut subscriber = ch.subscriber();

    let mut w = publisher.loan(65536).unwrap();
    w.set_stamp_ns(1000);
    w.set_body_bytes(&build_joint_state_body());
    w.publish().unwrap();

    let l1 = subscriber.latest().unwrap().unwrap();
    let l2 = subscriber.latest().unwrap().unwrap();
    assert_eq!(l1.view().unwrap().header.stamp_ns(), 1000);
    assert_eq!(l2.view().unwrap().header.stamp_ns(), 1000);
    assert_eq!(ch.active_leases(), 2);

    // 新 publish 后 latest 拿到新 stamp
    let mut w2 = publisher.loan(65536).unwrap();
    w2.set_stamp_ns(2000);
    w2.set_body_bytes(&build_joint_state_body());
    w2.publish().unwrap();

    let l3 = subscriber.latest().unwrap().unwrap();
    assert_eq!(l3.view().unwrap().header.stamp_ns(), 2000);
}

// ---------------------------------------------------------------------------
// loan(size) 硬限：publish 超档返 SampleTooLarge（Step 5 A1 新语义）
// ---------------------------------------------------------------------------

#[test]
fn publish_exceeds_loan_max_sample_size_errors() {
    let ch = MockChannel::<markers::JointState>::new();
    let mut publisher = ch.publisher();

    // 声明 size=64 字节：FrameHeader 本身就远超此值，publish 必返 SampleTooLarge。
    let mut w = publisher.loan(64).unwrap();
    w.set_body_bytes(&build_joint_state_body());
    let err = w.publish().unwrap_err();
    match err {
        TransportError::SampleTooLarge { size, limit } => {
            assert!(
                size > limit,
                "expected size > limit, got size={size} limit={limit}"
            );
            assert_eq!(limit, 64);
        }
        other => panic!("expected SampleTooLarge, got {other:?}"),
    }
    // 失败后 writer 被消费（drop），active_writers 归零。
    assert_eq!(ch.active_writers(), 0);
    // 未成功写入 slot，publish_count 保持 0。
    assert_eq!(ch.publishes(), 0);
}

#[test]
fn typed_mock_receipt_is_stored_and_failure_releases_the_real_writer() {
    use robot_transport::{PublishFailure, PublishReceipt};
    let channel = MockChannel::<markers::JointState>::new();
    let mut publisher = channel.publisher();
    let mut subscriber = channel.subscriber();
    let body = build_joint_state_body();
    let mut writer = publisher.loan(1024).unwrap();
    writer.set_body_bytes(&body);
    writer.set_stamp_ns(123);
    assert_eq!(writer.publish_typed().unwrap(), PublishReceipt::MockStored);
    assert_eq!((channel.publishes(), channel.active_writers()), (1, 0));
    let lease = subscriber.latest().unwrap().unwrap();
    let view = lease.view().unwrap();
    assert_eq!(view.header.stamp_ns(), 123);
    assert_eq!(view.body.raw(), body);
    drop(lease);
    assert_eq!(channel.active_leases(), 0);
    let mut rejected = publisher.loan(1).unwrap();
    rejected.set_body_bytes(&body);
    assert!(matches!(
        rejected.publish_typed(),
        Err(PublishFailure::BeforeHistory(
            TransportError::SampleTooLarge { .. }
        ))
    ));
    assert_eq!((channel.publishes(), channel.active_writers()), (1, 0));
}
