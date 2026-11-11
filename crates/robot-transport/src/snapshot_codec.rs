//! `RtSnapshot` 与 `JointCommandBuf` 的固定值codec，不改wire/FlatBuffers布局。
//!
//! 布局由 [`MAX_JOINTS`] 推导：每 2 个 `f32` 的 `to_bits` 打包为 1 个 `u64`（低半
//! 字 = 偶数下标，高半字 = 奇数下标；奇数长度时尾字高半置 0，不读 padding），
//! 顺序为 positions / velocities / torques；随后 `state.stamp_ns`、`joint_count`、
//! `tick_seq`、`tick_deadline_ns` 各占一字。当前 `MAX_JOINTS = 32` → 3 × 16 + 4 =
//! 52 字。保留全部浮点位模式（含 NaN / 负零 / 无穷）、无效区数组与完整 `u8`
//! 计数；不夹带标定、限幅或 NaN 过滤。

#![allow(clippy::doc_markdown)]

use robot_core::adapter::{
    ControlModeRepr, JointCommandBuf, JointSelection, JointStateBuf, MAX_JOINTS, RtSnapshot,
};

use crate::snapshot::SnapshotValue;

/// 每个 `[f32; MAX_JOINTS]` 占用的字数（向上取整）。
pub(crate) const F32_ARRAY_WORDS: usize = MAX_JOINTS.div_ceil(2);
/// `RtSnapshot` 的总字数。
pub(crate) const RT_SNAPSHOT_WORDS: usize = 3 * F32_ARRAY_WORDS + 4;

const STAMP_WORD: usize = 3 * F32_ARRAY_WORDS;
const JOINT_COUNT_WORD: usize = STAMP_WORD + 1;
const TICK_SEQ_WORD: usize = STAMP_WORD + 2;
const TICK_DEADLINE_WORD: usize = STAMP_WORD + 3;

fn encode_f32_array(values: &[f32; MAX_JOINTS], words: &mut [u64]) {
    for (word, pair) in words.iter_mut().zip(values.chunks(2)) {
        let low = u64::from(pair[0].to_bits());
        let high = pair.get(1).map_or(0, |value| u64::from(value.to_bits()));
        *word = low | (high << 32);
    }
}

fn decode_f32_array(words: &[u64], values: &mut [f32; MAX_JOINTS]) {
    for (word, pair) in words.iter().zip(values.chunks_mut(2)) {
        pair[0] = f32::from_bits(low_half(*word));
        if let Some(high) = pair.get_mut(1) {
            *high = f32::from_bits(high_half(*word));
        }
    }
}

const fn low_half(word: u64) -> u32 {
    (word & 0xffff_ffff) as u32
}

const fn high_half(word: u64) -> u32 {
    (word >> 32) as u32
}

impl SnapshotValue for RtSnapshot {
    type Words = [u64; RT_SNAPSHOT_WORDS];

    fn encode(&self, words: &mut Self::Words) {
        let (positions, rest) = words.split_at_mut(F32_ARRAY_WORDS);
        let (velocities, rest) = rest.split_at_mut(F32_ARRAY_WORDS);
        let (torques, tail) = rest.split_at_mut(F32_ARRAY_WORDS);
        encode_f32_array(&self.state.positions, positions);
        encode_f32_array(&self.state.velocities, velocities);
        encode_f32_array(&self.state.torques, torques);
        tail[0] = self.state.stamp_ns;
        tail[1] = u64::from(self.state.joint_count);
        tail[2] = self.tick_seq;
        tail[3] = self.tick_deadline_ns;
    }

    fn decode(words: &Self::Words) -> Self {
        let mut state = JointStateBuf::default();
        decode_f32_array(&words[..F32_ARRAY_WORDS], &mut state.positions);
        decode_f32_array(
            &words[F32_ARRAY_WORDS..2 * F32_ARRAY_WORDS],
            &mut state.velocities,
        );
        decode_f32_array(
            &words[2 * F32_ARRAY_WORDS..3 * F32_ARRAY_WORDS],
            &mut state.torques,
        );
        state.stamp_ns = words[STAMP_WORD];
        // 编码端只写入 u8；掩码截断是确定性的可逆解码，不是过滤。
        state.joint_count = (words[JOINT_COUNT_WORD] & 0xff) as u8;
        Self {
            state,
            tick_seq: words[TICK_SEQ_WORD],
            tick_deadline_ns: words[TICK_DEADLINE_WORD],
        }
    }
}

const COMMAND_RAW_WORDS: usize = MAX_JOINTS.div_ceil(4);
const COMMAND_TAIL: usize = 3 * F32_ARRAY_WORDS + COMMAND_RAW_WORDS;
const COMMAND_WORDS: usize = COMMAND_TAIL + 8;

impl SnapshotValue for JointCommandBuf {
    type Words = [u64; COMMAND_WORDS];

    fn encode(&self, words: &mut Self::Words) {
        let (positions, rest) = words.split_at_mut(F32_ARRAY_WORDS);
        let (velocities, rest) = rest.split_at_mut(F32_ARRAY_WORDS);
        let (torques, rest) = rest.split_at_mut(F32_ARRAY_WORDS);
        encode_f32_array(&self.positions, positions);
        encode_f32_array(&self.velocities, velocities);
        encode_f32_array(&self.torques, torques);
        let (raw, tail) = rest.split_at_mut(COMMAND_RAW_WORDS);
        for (word, chunk) in raw.iter_mut().zip(self.raw_velocities.chunks(4)) {
            *word = 0;
            for (index, value) in chunk.iter().enumerate() {
                *word |= u64::from(u16::from_ne_bytes(value.to_ne_bytes())) << (index * 16);
            }
        }
        tail[0] = self.mode as u64;
        (tail[1], tail[2], tail[3]) = match self.selection {
            JointSelection::All => (0, 0, 0),
            JointSelection::Mask(mask) => (1, u64::from(mask), 0),
            JointSelection::MixedPositionRawVelocity {
                position_mask,
                velocity_mask,
            } => (2, u64::from(position_mask), u64::from(velocity_mask)),
        };
        tail[4] = self.stamp_ns;
        tail[5] = self.deadline_ns;
        tail[6] = u64::from(self.joint_count);
        tail[7] = self.seq;
    }

    fn decode(words: &Self::Words) -> Self {
        let mut command = Self::default();
        decode_f32_array(&words[..F32_ARRAY_WORDS], &mut command.positions);
        decode_f32_array(
            &words[F32_ARRAY_WORDS..2 * F32_ARRAY_WORDS],
            &mut command.velocities,
        );
        decode_f32_array(
            &words[2 * F32_ARRAY_WORDS..3 * F32_ARRAY_WORDS],
            &mut command.torques,
        );
        for (word, chunk) in words[3 * F32_ARRAY_WORDS..COMMAND_TAIL]
            .iter()
            .zip(command.raw_velocities.chunks_mut(4))
        {
            for (index, value) in chunk.iter_mut().enumerate() {
                let bits = ((word >> (index * 16)) & 0xffff) as u16;
                *value = i16::from_ne_bytes(bits.to_ne_bytes());
            }
        }
        let tail = &words[COMMAND_TAIL..];
        command.mode = match tail[0] {
            1 => ControlModeRepr::Position,
            2 => ControlModeRepr::Velocity,
            3 => ControlModeRepr::Torque,
            4 => ControlModeRepr::ServoPosition,
            _ => ControlModeRepr::Stop,
        };
        command.selection = match tail[1] {
            0 => JointSelection::All,
            1 => JointSelection::Mask(low_half(tail[2])),
            2 => JointSelection::MixedPositionRawVelocity {
                position_mask: low_half(tail[2]),
                velocity_mask: low_half(tail[3]),
            },
            // 载荷只由上面的typed encode产生；保守处理无法由合法编码产生的tag。
            _ => JointSelection::Mask(0),
        };
        command.stamp_ns = tail[4];
        command.deadline_ns = tail[5];
        command.joint_count = (tail[6] & 0xff) as u8;
        command.seq = tail[7];
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{RtSnapshotPublisher, SnapshotWords};

    #[test]
    fn command_codec_preserves_every_field_and_float_bit_pattern() {
        for mode in [
            ControlModeRepr::Stop,
            ControlModeRepr::Position,
            ControlModeRepr::Velocity,
            ControlModeRepr::Torque,
            ControlModeRepr::ServoPosition,
        ] {
            for selection in [
                JointSelection::All,
                JointSelection::Mask(0x8000_0001),
                JointSelection::MixedPositionRawVelocity {
                    position_mask: 0xaaaa_aaaa,
                    velocity_mask: 0x5555_5555,
                },
            ] {
                let bits = [0x7fc0_1234, 0x8000_0000, 0x7f80_0000, 0xff80_0000];
                let raw = [i16::MIN, -500, -1, 0, 1, 500, i16::MAX];
                let command = JointCommandBuf {
                    positions: std::array::from_fn(|index| {
                        f32::from_bits(bits[index % bits.len()])
                    }),
                    velocities: std::array::from_fn(|index| {
                        f32::from_bits(bits[(index + 1) % bits.len()])
                    }),
                    torques: std::array::from_fn(|index| {
                        f32::from_bits(bits[(index + 2) % bits.len()])
                    }),
                    raw_velocities: std::array::from_fn(|index| raw[index % raw.len()]),
                    mode,
                    selection,
                    stamp_ns: u64::MAX,
                    deadline_ns: 0,
                    joint_count: u8::MAX,
                    seq: u64::MAX - 1,
                };
                let mut words = <JointCommandBuf as SnapshotValue>::Words::zeroed();
                command.encode(&mut words);
                let actual = JointCommandBuf::decode(&words);
                assert!(bits_equal(&actual.positions, &command.positions));
                assert!(bits_equal(&actual.velocities, &command.velocities));
                assert!(bits_equal(&actual.torques, &command.torques));
                assert_eq!(actual.raw_velocities, command.raw_velocities);
                assert_eq!(
                    (
                        actual.mode,
                        actual.selection,
                        actual.stamp_ns,
                        actual.deadline_ns,
                        actual.joint_count,
                        actual.seq
                    ),
                    (
                        command.mode,
                        command.selection,
                        command.stamp_ns,
                        command.deadline_ns,
                        command.joint_count,
                        command.seq
                    )
                );
            }
        }
    }

    fn roundtrip(snapshot: &RtSnapshot) -> RtSnapshot {
        let mut words = <[u64; RT_SNAPSHOT_WORDS] as SnapshotWords>::zeroed();
        snapshot.encode(&mut words);
        RtSnapshot::decode(&words)
    }

    fn bits_equal(a: &[f32; MAX_JOINTS], b: &[f32; MAX_JOINTS]) -> bool {
        a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
    }

    fn assert_bitwise_equal(a: &RtSnapshot, b: &RtSnapshot) {
        assert!(bits_equal(&a.state.positions, &b.state.positions));
        assert!(bits_equal(&a.state.velocities, &b.state.velocities));
        assert!(bits_equal(&a.state.torques, &b.state.torques));
        assert_eq!(a.state.stamp_ns, b.state.stamp_ns);
        assert_eq!(a.state.joint_count, b.state.joint_count);
        assert_eq!(a.tick_seq, b.tick_seq);
        assert_eq!(a.tick_deadline_ns, b.tick_deadline_ns);
    }

    #[test]
    fn layout_is_derived_from_max_joints() {
        assert_eq!(RT_SNAPSHOT_WORDS, 3 * MAX_JOINTS.div_ceil(2) + 4);
        assert_eq!(
            <[u64; RT_SNAPSHOT_WORDS] as SnapshotWords>::LEN,
            RT_SNAPSHOT_WORDS
        );
        if MAX_JOINTS == 32 {
            assert_eq!(RT_SNAPSHOT_WORDS, 52);
        }
    }

    #[test]
    fn special_floats_and_field_bounds_roundtrip_bitwise() {
        let specials = [
            f32::NAN,
            -f32::NAN,
            f32::from_bits(0x7fc0_0001), // 带载荷的 NaN
            f32::from_bits(0xffff_ffff),
            -0.0,
            0.0,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::MIN_POSITIVE,
            f32::from_bits(1), // 最小次正规数
            f32::MAX,
            f32::MIN,
            f32::EPSILON,
        ];
        let mut snapshot = RtSnapshot::default();
        for (index, value) in specials.iter().cycle().take(MAX_JOINTS).enumerate() {
            snapshot.state.positions[index] = *value;
            snapshot.state.velocities[MAX_JOINTS - 1 - index] = -*value;
            snapshot.state.torques[index] =
                f32::from(value.to_bits().wrapping_mul(2_654_435_761).to_le_bytes()[0]) + *value;
        }
        snapshot.state.stamp_ns = u64::MAX;
        snapshot.state.joint_count = u8::MAX;
        snapshot.tick_seq = u64::MAX - 1;
        snapshot.tick_deadline_ns = 1;
        assert_bitwise_equal(&roundtrip(&snapshot), &snapshot);

        let mut zero_count = snapshot;
        zero_count.state.joint_count = 0;
        zero_count.state.stamp_ns = 0;
        zero_count.tick_seq = 0;
        zero_count.tick_deadline_ns = u64::MAX;
        assert_bitwise_equal(&roundtrip(&zero_count), &zero_count);
        assert_bitwise_equal(&roundtrip(&RtSnapshot::default()), &RtSnapshot::default());
    }

    #[test]
    fn every_joint_slot_is_independent_including_invalid_region() {
        for index in 0..MAX_JOINTS {
            let mut snapshot = RtSnapshot::default();
            snapshot.state.joint_count = 3; // 有效区之外的槽同样保留
            let slot = u32::try_from(index).unwrap();
            snapshot.state.positions[index] = f32::from_bits(0xdead_beef ^ slot);
            snapshot.state.velocities[index] = f32::from_bits(0x8000_0000 | slot);
            snapshot.state.torques[index] = f32::from_bits(0x7f80_0001 | slot);
            let mut words = <[u64; RT_SNAPSHOT_WORDS] as SnapshotWords>::zeroed();
            snapshot.encode(&mut words);
            // 只有三个数组各自对应的一字非零（其余数组字为 0，尾部四字仅 joint_count）。
            let nonzero: Vec<usize> = words
                .iter()
                .enumerate()
                .filter(|(_, w)| **w != 0)
                .map(|(i, _)| i)
                .collect();
            let expected = vec![
                index / 2,
                F32_ARRAY_WORDS + index / 2,
                2 * F32_ARRAY_WORDS + index / 2,
                JOINT_COUNT_WORD,
            ];
            assert_eq!(nonzero, expected, "slot {index}");
            assert_bitwise_equal(&RtSnapshot::decode(&words), &snapshot);
        }
    }

    #[test]
    fn known_word_values_match_the_documented_layout() {
        let mut snapshot = RtSnapshot::default();
        snapshot.state.positions[0] = 1.0; // 0x3f80_0000
        snapshot.state.positions[1] = -2.0; // 0xc000_0000
        snapshot.state.velocities[MAX_JOINTS - 1] = f32::INFINITY; // 0x7f80_0000
        snapshot.state.torques[2] = f32::NAN;
        snapshot.state.stamp_ns = 0x1122_3344_5566_7788;
        snapshot.state.joint_count = 7;
        snapshot.tick_seq = 99;
        snapshot.tick_deadline_ns = 0xffff_ffff_ffff_fffe;
        let mut words = <[u64; RT_SNAPSHOT_WORDS] as SnapshotWords>::zeroed();
        snapshot.encode(&mut words);
        assert_eq!(words[0], 0x3f80_0000 | (0xc000_0000 << 32));
        let last_velocity_word = F32_ARRAY_WORDS + (MAX_JOINTS - 1) / 2;
        let expected_velocity_word = if MAX_JOINTS.is_multiple_of(2) {
            0x7f80_0000_u64 << 32
        } else {
            0x7f80_0000
        };
        assert_eq!(words[last_velocity_word], expected_velocity_word);
        assert_eq!(
            words[2 * F32_ARRAY_WORDS + 1],
            u64::from(f32::NAN.to_bits())
        );
        assert_eq!(words[STAMP_WORD], 0x1122_3344_5566_7788);
        assert_eq!(words[JOINT_COUNT_WORD], 7);
        assert_eq!(words[TICK_SEQ_WORD], 99);
        assert_eq!(words[TICK_DEADLINE_WORD], 0xffff_ffff_ffff_fffe);
    }

    #[test]
    fn rt_snapshot_roundtrips_through_the_seqlock() {
        let mut snapshot = RtSnapshot::default();
        snapshot.state.positions[5] = f32::NAN;
        snapshot.state.joint_count = 12;
        snapshot.tick_seq = 3;
        let (mut publisher, reader) = RtSnapshotPublisher::new(RtSnapshot::default());
        publisher.publish(&snapshot).unwrap();
        assert_bitwise_equal(&reader.load(), &snapshot);
        assert_eq!(reader.version(), 2);
    }
}
