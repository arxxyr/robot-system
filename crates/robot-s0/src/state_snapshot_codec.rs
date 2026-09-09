//! `S0StateSnapshot` 的显式字段 codec（进程内快照编码；不改 wire / FlatBuffers 布局，
//! 不改业务枚举）。S48 A3b PLAN P.6.14.4。
//!
//! 3 字布局：
//!
//! | 字 | 位 | 内容 |
//! |---|---|---|
//! | 0 | 0..=2 | state tag：Safe=0 / Ready=1 / Enabled=2 / Fault=3 / Stopping=4 |
//! | 0 | 3..=4 | Enabled.sub（`EnabledSub` 声明顺序：Active=0 / CtrlLostHold=1 / CtrlLostZero=2 / CtrlLostStop=3） |
//! | 0 | 5 | Enabled.control_link（Ok=0 / Lost=1） |
//! | 0 | 6..=7 | Enabled.output_mode（Active=0 / Hold=1 / Zero=2 / Stop=3） |
//! | 0 | 8..=10 | Fault code（`FaultCode` 声明顺序：CommFault=0 … SafetyStopFailed=6） |
//! | 1 | 0..=10 | `Guards` 的 11 个 bool，按字段声明顺序 |
//! | 1 | 11..=13 | `Guards.command_mode`（`ControlMode` 声明顺序：Position=0 / Velocity=1 / Torque=2 / ServoPosition=3 / Stop=4） |
//! | 1 | 14..=15 | `Guards.on_timeout_hint`（`EnabledSub` 同上） |
//! | 2 | 0..=63 | `stamp_ns` |
//!
//! 未用位恒为 0。所有枚举都显式 `match` 编码 / 解码，不用 `as` 或 transmute 依赖
//! Rust 内部表示；Enabled 的三个字段互相独立编码，不借编码修正不匹配组合。
//! 解码只会看到本编码器写出的字；对不可能出现的位模式，解码走保守回退
//! （最严重 / 最保守的枚举值）而不是 `unsafe` 构造无效值——这属于程序错误的
//! fail-closed 表现，不是业务语义。

#![allow(clippy::doc_markdown)]

use robot_core::adapter::ControlMode;
use robot_transport::SnapshotValue;

use crate::state_machine::{
    ControlLink, EnabledSub, FaultCode, Guards, OutputMode, S0State, S0StateSnapshot,
};

/// `S0StateSnapshot` 的字数。
pub(crate) const STATE_SNAPSHOT_WORDS: usize = 3;

const TAG_SHIFT: u32 = 0;
const SUB_SHIFT: u32 = 3;
const LINK_SHIFT: u32 = 5;
const OUTPUT_SHIFT: u32 = 6;
const FAULT_SHIFT: u32 = 8;
const COMMAND_MODE_SHIFT: u32 = 11;
const TIMEOUT_HINT_SHIFT: u32 = 14;

const fn field(word: u64, shift: u32, bits: u32) -> u64 {
    (word >> shift) & ((1 << bits) - 1)
}

const fn tag_of(state: S0State) -> u64 {
    match state {
        S0State::Safe => 0,
        S0State::Ready => 1,
        S0State::Enabled { .. } => 2,
        S0State::Fault(_) => 3,
        S0State::Stopping => 4,
    }
}

const fn encode_sub(sub: EnabledSub) -> u64 {
    match sub {
        EnabledSub::Active => 0,
        EnabledSub::CtrlLostHold => 1,
        EnabledSub::CtrlLostZero => 2,
        EnabledSub::CtrlLostStop => 3,
    }
}

/// 不可能的位模式回退到最保守的 `CtrlLostStop`。
const fn decode_sub(raw: u64) -> EnabledSub {
    match raw {
        0 => EnabledSub::Active,
        1 => EnabledSub::CtrlLostHold,
        2 => EnabledSub::CtrlLostZero,
        _ => EnabledSub::CtrlLostStop,
    }
}

const fn encode_link(link: ControlLink) -> u64 {
    match link {
        ControlLink::Ok => 0,
        ControlLink::Lost => 1,
    }
}

const fn decode_link(raw: u64) -> ControlLink {
    match raw {
        0 => ControlLink::Ok,
        _ => ControlLink::Lost,
    }
}

const fn encode_output(output: OutputMode) -> u64 {
    match output {
        OutputMode::Active => 0,
        OutputMode::Hold => 1,
        OutputMode::Zero => 2,
        OutputMode::Stop => 3,
    }
}

/// 不可能的位模式回退到最保守的 `Stop`。
const fn decode_output(raw: u64) -> OutputMode {
    match raw {
        0 => OutputMode::Active,
        1 => OutputMode::Hold,
        2 => OutputMode::Zero,
        _ => OutputMode::Stop,
    }
}

const fn encode_fault(code: FaultCode) -> u64 {
    match code {
        FaultCode::CommFault => 0,
        FaultCode::AdapterErr => 1,
        FaultCode::CmdDeadlineExpired => 2,
        FaultCode::LimitViolation => 3,
        FaultCode::WatchdogTimeout => 4,
        FaultCode::RtOverrun => 5,
        FaultCode::SafetyStopFailed => 6,
    }
}

/// 不可能的位模式回退到最严重的 `SafetyStopFailed`。
const fn decode_fault(raw: u64) -> FaultCode {
    match raw {
        0 => FaultCode::CommFault,
        1 => FaultCode::AdapterErr,
        2 => FaultCode::CmdDeadlineExpired,
        3 => FaultCode::LimitViolation,
        4 => FaultCode::WatchdogTimeout,
        5 => FaultCode::RtOverrun,
        _ => FaultCode::SafetyStopFailed,
    }
}

const fn encode_control_mode(mode: ControlMode) -> u64 {
    match mode {
        ControlMode::Position => 0,
        ControlMode::Velocity => 1,
        ControlMode::Torque => 2,
        ControlMode::ServoPosition => 3,
        ControlMode::Stop => 4,
    }
}

/// 不可能的位模式回退到 `Stop`。
const fn decode_control_mode(raw: u64) -> ControlMode {
    match raw {
        0 => ControlMode::Position,
        1 => ControlMode::Velocity,
        2 => ControlMode::Torque,
        3 => ControlMode::ServoPosition,
        _ => ControlMode::Stop,
    }
}

fn encode_state(state: S0State) -> u64 {
    let mut word = tag_of(state) << TAG_SHIFT;
    match state {
        S0State::Enabled {
            sub,
            control_link,
            output_mode,
        } => {
            word |= encode_sub(sub) << SUB_SHIFT;
            word |= encode_link(control_link) << LINK_SHIFT;
            word |= encode_output(output_mode) << OUTPUT_SHIFT;
        }
        S0State::Fault(code) => word |= encode_fault(code) << FAULT_SHIFT,
        S0State::Safe | S0State::Ready | S0State::Stopping => {}
    }
    word
}

/// 未知 tag 回退到 `Fault(SafetyStopFailed)`（最保守）。
fn decode_state(word: u64) -> S0State {
    match field(word, TAG_SHIFT, 3) {
        0 => S0State::Safe,
        1 => S0State::Ready,
        2 => S0State::Enabled {
            sub: decode_sub(field(word, SUB_SHIFT, 2)),
            control_link: decode_link(field(word, LINK_SHIFT, 1)),
            output_mode: decode_output(field(word, OUTPUT_SHIFT, 2)),
        },
        3 => S0State::Fault(decode_fault(field(word, FAULT_SHIFT, 3))),
        4 => S0State::Stopping,
        _ => S0State::Fault(FaultCode::SafetyStopFailed),
    }
}

/// `Guards` 的 11 个 bool，按字段声明顺序。
const fn guard_flags(guards: &Guards) -> [bool; 11] {
    [
        guards.rt_serial,
        guards.estop_latched,
        guards.adapter_connected,
        guards.calibration_loaded,
        guards.joint_state_valid,
        guards.power_ok,
        guards.heartbeat_ok,
        guards.cmd_deadline_valid,
        guards.limits_valid,
        guards.hw_enable_ok,
        guards.adapter_fault_cleared,
    ]
}

fn encode_guards(guards: &Guards) -> u64 {
    let mut word = 0;
    for (bit, flag) in guard_flags(guards).into_iter().enumerate() {
        word |= u64::from(flag) << bit;
    }
    word |= encode_control_mode(guards.command_mode) << COMMAND_MODE_SHIFT;
    word |= encode_sub(guards.on_timeout_hint) << TIMEOUT_HINT_SHIFT;
    word
}

fn decode_guards(word: u64) -> Guards {
    let flag = |bit: u32| field(word, bit, 1) != 0;
    Guards {
        rt_serial: flag(0),
        estop_latched: flag(1),
        adapter_connected: flag(2),
        calibration_loaded: flag(3),
        joint_state_valid: flag(4),
        power_ok: flag(5),
        heartbeat_ok: flag(6),
        cmd_deadline_valid: flag(7),
        limits_valid: flag(8),
        hw_enable_ok: flag(9),
        adapter_fault_cleared: flag(10),
        command_mode: decode_control_mode(field(word, COMMAND_MODE_SHIFT, 3)),
        on_timeout_hint: decode_sub(field(word, TIMEOUT_HINT_SHIFT, 2)),
    }
}

impl SnapshotValue for S0StateSnapshot {
    type Words = [u64; STATE_SNAPSHOT_WORDS];

    fn encode(&self, words: &mut Self::Words) {
        words[0] = encode_state(self.state);
        words[1] = encode_guards(&self.guards_summary);
        words[2] = self.stamp_ns;
    }

    fn decode(words: &Self::Words) -> Self {
        Self {
            state: decode_state(words[0]),
            guards_summary: decode_guards(words[1]),
            stamp_ns: words[2],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use robot_transport::{RtSnapshotPublisher, SnapshotWords};

    const SUBS: [EnabledSub; 4] = [
        EnabledSub::Active,
        EnabledSub::CtrlLostHold,
        EnabledSub::CtrlLostZero,
        EnabledSub::CtrlLostStop,
    ];
    const LINKS: [ControlLink; 2] = [ControlLink::Ok, ControlLink::Lost];
    const OUTPUTS: [OutputMode; 4] = [
        OutputMode::Active,
        OutputMode::Hold,
        OutputMode::Zero,
        OutputMode::Stop,
    ];
    const FAULTS: [FaultCode; 7] = [
        FaultCode::CommFault,
        FaultCode::AdapterErr,
        FaultCode::CmdDeadlineExpired,
        FaultCode::LimitViolation,
        FaultCode::WatchdogTimeout,
        FaultCode::RtOverrun,
        FaultCode::SafetyStopFailed,
    ];
    const MODES: [ControlMode; 5] = [
        ControlMode::Position,
        ControlMode::Velocity,
        ControlMode::Torque,
        ControlMode::ServoPosition,
        ControlMode::Stop,
    ];

    fn roundtrip(snapshot: S0StateSnapshot) -> S0StateSnapshot {
        let mut words = <[u64; STATE_SNAPSHOT_WORDS] as SnapshotWords>::zeroed();
        snapshot.encode(&mut words);
        S0StateSnapshot::decode(&words)
    }

    fn words_of(snapshot: S0StateSnapshot) -> [u64; STATE_SNAPSHOT_WORDS] {
        let mut words = <[u64; STATE_SNAPSHOT_WORDS] as SnapshotWords>::zeroed();
        snapshot.encode(&mut words);
        words
    }

    fn all_guards_set() -> Guards {
        Guards {
            rt_serial: true,
            estop_latched: true,
            adapter_connected: true,
            calibration_loaded: true,
            joint_state_valid: true,
            power_ok: true,
            heartbeat_ok: true,
            cmd_deadline_valid: true,
            limits_valid: true,
            hw_enable_ok: true,
            adapter_fault_cleared: true,
            command_mode: ControlMode::Stop,
            on_timeout_hint: EnabledSub::CtrlLostStop,
        }
    }

    #[test]
    fn every_state_variant_roundtrips_including_all_faults_and_enabled_combinations() {
        let mut states = vec![S0State::Safe, S0State::Ready, S0State::Stopping];
        states.extend(FAULTS.iter().map(|code| S0State::Fault(*code)));
        for sub in SUBS {
            for control_link in LINKS {
                for output_mode in OUTPUTS {
                    // 三个字段互相独立：包括与 `OutputMode::from_sub` 不匹配的组合。
                    states.push(S0State::Enabled {
                        sub,
                        control_link,
                        output_mode,
                    });
                }
            }
        }
        assert_eq!(states.len(), 3 + 7 + 32);
        for state in states {
            for stamp_ns in [0, 1, u64::MAX] {
                let snapshot = S0StateSnapshot::new(state, Guards::default(), stamp_ns);
                assert_eq!(roundtrip(snapshot), snapshot, "{state:?} @ {stamp_ns}");
            }
        }
    }

    #[test]
    fn every_guard_bit_and_enum_field_is_independent() {
        for bit in 0..11 {
            let mut guards = Guards::default();
            match bit {
                0 => guards.rt_serial = true,
                1 => guards.estop_latched = true,
                2 => guards.adapter_connected = true,
                3 => guards.calibration_loaded = true,
                4 => guards.joint_state_valid = true,
                5 => guards.power_ok = true,
                6 => guards.heartbeat_ok = true,
                7 => guards.cmd_deadline_valid = true,
                8 => guards.limits_valid = true,
                9 => guards.hw_enable_ok = true,
                10 => guards.adapter_fault_cleared = true,
                _ => unreachable!(),
            }
            let snapshot = S0StateSnapshot::new(S0State::Safe, guards, 7);
            let words = words_of(snapshot);
            let default_word =
                words_of(S0StateSnapshot::new(S0State::Safe, Guards::default(), 7))[1];
            assert_eq!(words[1] ^ default_word, 1 << bit, "guard bit {bit}");
            assert_eq!(roundtrip(snapshot), snapshot);
        }
        for mode in MODES {
            for hint in SUBS {
                let guards = Guards {
                    command_mode: mode,
                    on_timeout_hint: hint,
                    ..Guards::default()
                };
                let snapshot = S0StateSnapshot::new(S0State::Ready, guards, 9);
                let words = words_of(snapshot);
                assert_eq!(
                    field(words[1], COMMAND_MODE_SHIFT, 3),
                    encode_control_mode(mode)
                );
                assert_eq!(field(words[1], TIMEOUT_HINT_SHIFT, 2), encode_sub(hint));
                assert_eq!(roundtrip(snapshot), snapshot, "{mode:?} / {hint:?}");
            }
        }
        let full = S0StateSnapshot::new(S0State::Safe, all_guards_set(), 11);
        assert_eq!(roundtrip(full), full);
        assert_eq!(words_of(full)[1] >> 16, 0, "未用位恒为 0");
    }

    #[test]
    fn known_word_values_match_the_documented_layout() {
        let snapshot = S0StateSnapshot::new(
            S0State::Enabled {
                sub: EnabledSub::CtrlLostZero,
                control_link: ControlLink::Lost,
                output_mode: OutputMode::Hold,
            },
            Guards {
                estop_latched: true,
                power_ok: true,
                adapter_fault_cleared: true,
                command_mode: ControlMode::ServoPosition,
                on_timeout_hint: EnabledSub::CtrlLostHold,
                ..Guards::default()
            },
            0x0102_0304_0506_0708,
        );
        let words = words_of(snapshot);
        // tag=2, sub=2<<3, link=1<<5, output=1<<6
        assert_eq!(words[0], 2 | (2 << 3) | (1 << 5) | (1 << 6));
        // bits 1,5,10 | mode 3<<11 | hint 1<<14
        assert_eq!(
            words[1],
            (1 << 1) | (1 << 5) | (1 << 10) | (3 << 11) | (1 << 14)
        );
        assert_eq!(words[2], 0x0102_0304_0506_0708);

        let fault =
            S0StateSnapshot::new(S0State::Fault(FaultCode::RtOverrun), Guards::default(), 0);
        assert_eq!(words_of(fault)[0], 3 | (5 << 8));
        assert_eq!(
            words_of(S0StateSnapshot::new(
                S0State::Stopping,
                Guards::default(),
                0
            ))[0],
            4
        );
        assert_eq!(words_of(S0StateSnapshot::default())[0], 0);
    }

    #[test]
    fn impossible_bit_patterns_decode_to_conservative_values_without_unsafe() {
        assert_eq!(decode_state(7), S0State::Fault(FaultCode::SafetyStopFailed));
        assert_eq!(
            decode_state(3 | (7 << FAULT_SHIFT)),
            S0State::Fault(FaultCode::SafetyStopFailed)
        );
        assert_eq!(decode_control_mode(7), ControlMode::Stop);
        assert_eq!(decode_sub(3), EnabledSub::CtrlLostStop);
        assert_eq!(decode_output(3), OutputMode::Stop);
    }

    #[test]
    fn state_snapshot_roundtrips_through_the_seqlock() {
        let snapshot =
            S0StateSnapshot::new(S0State::Fault(FaultCode::CommFault), all_guards_set(), 42);
        let (mut publisher, reader) = RtSnapshotPublisher::new(S0StateSnapshot::default());
        publisher.publish(&snapshot).unwrap();
        assert_eq!(reader.load(), snapshot);
    }
}
