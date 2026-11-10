//! 默认 S0 的轮原字日志；只消费打开后追加的完整组。

use std::fs::{File, Metadata, symlink_metadata};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use robot_core::time::RobotClock;
use serde_json::Value;

const OBSERVATION_TARGET: &str = "s0_wheel_observation";
const PUB_TARGET: &str = "robot_s0::pub_thread";
const READ_BUDGET: usize = 64 * 1024;
const LINE_BUDGET: usize = 256;
const MAX_LINE_BYTES: usize = 64 * 1024;
const GROUP_BUDGET_NS: u64 = 500_000_000;
const MOTION_CAPACITY: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WheelSample {
    pub(super) id: u8,
    pub(super) torque: u8,
    pub(super) goal: u16,
    pub(super) velocity: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ZeroSample {
    pub(super) started_ns: u64,
    pub(super) finished_ns: u64,
    pub(super) wheels: [WheelSample; 3],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MotionWheel {
    pub(super) id: u8,
    pub(super) position: u16,
    pub(super) velocity: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MotionSample {
    pub(super) started_ns: u64,
    pub(super) finished_ns: u64,
    pub(super) wheels: [MotionWheel; 3],
}

#[derive(Clone, Copy)]
struct RawWheel {
    id: u8,
    auxiliary: u8,
    low: u16,
    high: u16,
}

struct PendingGroup {
    site: u8,
    started_ns: u64,
    finished_ns: Option<u64>,
    wheels: [Option<RawWheel>; 3],
}

pub(super) struct Observations {
    path: PathBuf,
    file: File,
    identity: (u64, u64),
    observed_len: u64,
    offset: u64,
    wheel_ids: [u8; 3],
    buffer: Vec<u8>,
    pending: Option<PendingGroup>,
    last_stamp: [Option<u64>; 2],
    last_poll: Option<u64>,
    motion: Vec<MotionSample>,
    summary_count: u64,
    last_zero_summary_count: Option<u64>,
    failure: Option<String>,
}

impl Observations {
    pub(super) fn open(path: &Path, wheel_ids: [u8; 3]) -> Result<Self, String> {
        if wheel_ids[0] == wheel_ids[1]
            || wheel_ids[0] == wheel_ids[2]
            || wheel_ids[1] == wheel_ids[2]
        {
            return Err("观测轮 ID 必须各不相同".to_owned());
        }
        let path_metadata =
            symlink_metadata(path).map_err(|error| format!("检查观测日志：{error}"))?;
        let identity = regular_identity(&path_metadata)?;
        let mut file = File::open(path).map_err(|error| format!("打开观测日志：{error}"))?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("检查已打开日志：{error}"))?;
        if regular_identity(&metadata)? != identity {
            return Err("打开观测日志时文件已替换".to_owned());
        }
        let offset = file
            .seek(SeekFrom::End(0))
            .map_err(|error| format!("定位日志末尾：{error}"))?;
        Ok(Self {
            path: path.to_owned(),
            file,
            identity,
            observed_len: offset,
            offset,
            wheel_ids,
            buffer: Vec::new(),
            pending: None,
            last_stamp: [None; 2],
            last_poll: None,
            motion: Vec::with_capacity(MOTION_CAPACITY),
            summary_count: 0,
            last_zero_summary_count: None,
            failure: None,
        })
    }

    pub(super) fn poll(&mut self, clock: &dyn RobotClock) -> Result<Vec<ZeroSample>, String> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let result = self.poll_inner(clock);
        if let Err(error) = &result {
            self.failure = Some(error.clone());
            self.motion.clear();
        }
        result
    }

    pub(super) fn take_motion(&mut self) -> Vec<MotionSample> {
        std::mem::take(&mut self.motion)
    }

    pub(super) fn summary_count(&self) -> u64 {
        self.summary_count
    }

    pub(super) fn summary_after_last_zero(&self) -> bool {
        self.failure.is_none()
            && self.pending.is_none()
            && self.buffer.is_empty()
            && self.offset == self.observed_len
            && self
                .last_zero_summary_count
                .is_some_and(|count| self.summary_count > count)
    }

    fn poll_inner(&mut self, clock: &dyn RobotClock) -> Result<Vec<ZeroSample>, String> {
        // 先固定可读长度，再取时间；快照内新追加的合法记录不会被旧 now 误拒。
        let snapshot_len = self.check_file()?;
        let now_ns = clock.now().0;
        if self.last_poll.is_some_and(|last| now_ns < last) {
            return Err("观测轮询时钟回退".to_owned());
        }
        self.last_poll = Some(now_ns);
        let mut samples = Vec::new();
        let mut consumed = 0;
        let mut lines = 0;
        let mut read_bytes = 0;
        let mut scratch = [0_u8; 8192];
        loop {
            if lines == LINE_BUDGET {
                break;
            }
            if let Some(relative_end) = self.buffer[consumed..]
                .iter()
                .position(|byte| *byte == b'\n')
            {
                if relative_end > MAX_LINE_BYTES {
                    return Err("观测日志单行超过 64KiB".to_owned());
                }
                let end = consumed + relative_end;
                let value: Value = serde_json::from_slice(&self.buffer[consumed..end])
                    .map_err(|error| format!("观测日志 JSON 损坏：{error}"))?;
                self.consume_record(&value, now_ns, &mut samples)?;
                consumed = end + 1;
                lines += 1;
                continue;
            }
            if self.buffer.len() - consumed > MAX_LINE_BYTES {
                return Err("观测日志单行超过 64KiB".to_owned());
            }
            if read_bytes == READ_BUDGET || self.offset == snapshot_len {
                break;
            }
            // 只读本次快照已存在的字节；持续追加不会延长一次 poll。
            let available = usize::try_from(snapshot_len - self.offset).unwrap_or(usize::MAX);
            let count = scratch.len().min(READ_BUDGET - read_bytes).min(available);
            let count = self
                .file
                .read(&mut scratch[..count])
                .map_err(|error| format!("读取观测日志：{error}"))?;
            if count == 0 {
                return Err("观测日志在读取中被截短".to_owned());
            }
            self.offset +=
                u64::try_from(count).map_err(|error| format!("日志字节数越界：{error}"))?;
            read_bytes += count;
            self.buffer.extend_from_slice(&scratch[..count]);
        }
        self.buffer.copy_within(consumed.., 0);
        self.buffer.truncate(self.buffer.len() - consumed);
        // 沿用原 500ms 上界；主循环的整个零速窗口另有固定绝对截止。
        if self.pending.as_ref().is_some_and(|group| {
            group.site == 31 && now_ns.saturating_sub(group.started_ns) >= GROUP_BUDGET_NS
        }) {
            return Err("零速观测组在 500ms 内缺少轮记录".to_owned());
        }
        Ok(samples)
    }

    fn check_file(&mut self) -> Result<u64, String> {
        let path_metadata =
            symlink_metadata(&self.path).map_err(|error| format!("复查观测日志：{error}"))?;
        let metadata = self
            .file
            .metadata()
            .map_err(|error| format!("复查已打开日志：{error}"))?;
        if regular_identity(&path_metadata)? != self.identity
            || regular_identity(&metadata)? != self.identity
        {
            return Err("观测日志文件已替换".to_owned());
        }
        if path_metadata.len() < self.observed_len || metadata.len() < self.observed_len {
            return Err("观测日志长度回退".to_owned());
        }
        self.observed_len = path_metadata.len().max(metadata.len());
        Ok(self.observed_len)
    }

    fn consume_record(
        &mut self,
        value: &Value,
        now_ns: u64,
        samples: &mut Vec<ZeroSample>,
    ) -> Result<(), String> {
        let record = value
            .as_object()
            .ok_or_else(|| "观测日志 JSON 必须是对象".to_owned())?;
        let target = record.get("target").and_then(Value::as_str);
        if target == Some(PUB_TARGET)
            && record.get("message").and_then(Value::as_str) == Some("串口一秒摘要")
        {
            // dropped 是累计计数；初次看到非零同样不能证明本轮记录完整。
            if unsigned(value, "dropped")? != 0 {
                return Err("串口诊断摘要报告日志丢失".to_owned());
            }
            self.summary_count = self
                .summary_count
                .checked_add(1)
                .ok_or_else(|| "串口摘要计数溢出".to_owned())?;
            return Ok(());
        }
        if target != Some(OBSERVATION_TARGET) {
            if matches!(record.get("site").and_then(Value::as_u64), Some(31 | 32)) {
                return Err("site31/32 记录未使用观测 target，可能含未知 kind".to_owned());
            }
            return Ok(());
        }
        let site = u8_field(value, "site")?;
        let kind = u8_field(value, "kind")?;
        let a = u8_field(value, "a")?;
        let b = u8_field(value, "b")?;
        let seq =
            u32::try_from(unsigned(value, "seq")?).map_err(|_| "观测 seq 超过 u32".to_owned())?;
        let stamp = unsigned(value, "stamp")?;
        if !matches!(site, 31 | 32) || !matches!(kind, 0 | 1) {
            return Err("原字观测 site/kind 不受支持".to_owned());
        }
        let site_index = usize::from(site - 31);
        if stamp > now_ns
            || self.last_stamp[site_index]
                .is_some_and(|last| stamp < last || (kind == 0 && stamp == last))
        {
            return Err("原字观测时间戳在未来、回退或跨组未推进".to_owned());
        }
        if kind == 0 {
            if self.pending.is_some() {
                return Err("新 BEGIN 覆盖未完整的原字观测组".to_owned());
            }
            if a != 3 || b != 0 || seq != 0 {
                return Err("原字观测 BEGIN 字段不合法".to_owned());
            }
            self.pending = Some(PendingGroup {
                site,
                started_ns: stamp,
                finished_ns: None,
                wheels: [None; 3],
            });
        } else {
            self.consume_wheel(site, a, b, seq, stamp, samples)?;
        }
        self.last_stamp[site_index] = Some(stamp);
        Ok(())
    }

    fn consume_wheel(
        &mut self,
        site: u8,
        id: u8,
        torque: u8,
        seq: u32,
        stamp: u64,
        samples: &mut Vec<ZeroSample>,
    ) -> Result<(), String> {
        let index = self
            .wheel_ids
            .iter()
            .position(|configured| *configured == id)
            .ok_or_else(|| format!("原字观测包含未配置轮 ID {id}"))?;
        let group = self
            .pending
            .as_mut()
            .ok_or_else(|| "孤立的原字轮记录".to_owned())?;
        if site != group.site
            || stamp < group.started_ns
            || group.finished_ns.is_some_and(|finished| stamp != finished)
        {
            return Err("同组原字轮记录来源或完成时间不一致".to_owned());
        }
        if group.wheels[index].is_some() {
            return Err(format!("原字观测轮 ID {id} 重复"));
        }
        if site == 32 && torque != 0 {
            return Err("运动原字观测保留字段 b 必须为 0".to_owned());
        }
        if site == 31 && stamp - group.started_ns >= GROUP_BUDGET_NS {
            return Err("零速观测完成时间超出 500ms".to_owned());
        }
        group.finished_ns = Some(stamp);
        group.wheels[index] = Some(RawWheel {
            id,
            auxiliary: torque,
            low: u16::try_from(seq & 0xffff).map_err(|error| format!("低位原字越界：{error}"))?,
            high: u16::try_from(seq >> 16).map_err(|error| format!("高位原字越界：{error}"))?,
        });
        if group.wheels.iter().all(Option::is_some) {
            let group = self
                .pending
                .take()
                .ok_or_else(|| "原字观测组状态缺失".to_owned())?;
            let missing = || "原字观测组缺少轮记录".to_owned();
            let wheels = [
                group.wheels[0].ok_or_else(missing)?,
                group.wheels[1].ok_or_else(missing)?,
                group.wheels[2].ok_or_else(missing)?,
            ];
            self.complete_group(group.site, group.started_ns, stamp, wheels, samples)?;
        }
        Ok(())
    }

    fn complete_group(
        &mut self,
        site: u8,
        started_ns: u64,
        finished_ns: u64,
        wheels: [RawWheel; 3],
        samples: &mut Vec<ZeroSample>,
    ) -> Result<(), String> {
        if site == 31 {
            self.last_zero_summary_count = Some(self.summary_count);
            samples.push(ZeroSample {
                started_ns,
                finished_ns,
                wheels: wheels.map(|wheel| WheelSample {
                    id: wheel.id,
                    torque: wheel.auxiliary,
                    goal: wheel.low,
                    velocity: wheel.high,
                }),
            });
        } else {
            if self.motion.len() == MOTION_CAPACITY {
                return Err("待取运动原字观测超过 128 组".to_owned());
            }
            self.motion.push(MotionSample {
                started_ns,
                finished_ns,
                wheels: wheels.map(|wheel| MotionWheel {
                    id: wheel.id,
                    position: wheel.low,
                    velocity: wheel.high,
                }),
            });
        }
        Ok(())
    }
}

fn regular_identity(metadata: &Metadata) -> Result<(u64, u64), String> {
    if !metadata.file_type().is_file() {
        return Err("观测日志必须是普通文件，不能是符号链接或设备".to_owned());
    }
    Ok((metadata.dev(), metadata.ino()))
}

fn unsigned(value: &Value, field: &str) -> Result<u64, String> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("观测字段 {field} 缺失或不是无符号整数"))
}

fn u8_field(value: &Value, field: &str) -> Result<u8, String> {
    u8::try_from(unsigned(value, field)?).map_err(|_| format!("观测字段 {field} 超过 u8"))
}

pub(super) fn validate_zero_sample(sample: &ZeroSample, final_sample: bool) -> Result<(), String> {
    for wheel in &sample.wheels {
        if wheel.torque != 1 || wheel.goal != 0 {
            return Err(format!(
                "轮 {} 零速 TE/GV 不满足 1/0：{}/{}",
                wheel.id, wheel.torque, wheel.goal
            ));
        }
        let valid_velocity = if final_sample {
            matches!(wheel.velocity, 0 | 50 | 0x8032)
        } else {
            wheel.velocity <= 1000 || wheel.velocity == 0x8032
        };
        if !valid_velocity {
            return Err(format!(
                "轮 {} 零速 PV 原字不合合同：{}",
                wheel.id, wheel.velocity
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;

    use std::sync::atomic::{AtomicBool, Ordering};

    use robot_core::time::{Deadline, DurationNs, MockClock, MonoRawNs};
    use serde_json::json;
    use tempfile::NamedTempFile;

    use super::*;

    fn at(now_ns: u64) -> MockClock {
        MockClock::new(MonoRawNs(now_ns))
    }

    struct AppendOnNow {
        clock: MockClock,
        path: PathBuf,
        bytes: Vec<u8>,
        appended: AtomicBool,
    }

    impl RobotClock for AppendOnNow {
        fn now(&self) -> MonoRawNs {
            if !self.appended.swap(true, Ordering::Relaxed) {
                append(&self.path, &self.bytes);
            }
            self.clock.now()
        }

        fn sleep_until(&self, deadline: Deadline) {
            self.clock.sleep_until(deadline);
        }

        fn sleep_for(&self, duration: DurationNs) {
            self.clock.sleep_for(duration);
        }
    }

    fn append(path: &Path, bytes: &[u8]) {
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    fn event(kind: u8, a: u64, b: u64, seq: u64, stamp: u64) -> Value {
        json!({"target": OBSERVATION_TARGET, "site": 31, "kind": kind, "a": a, "b": b, "seq": seq, "stamp": stamp, "message": "串口设备观测"})
    }

    fn line(value: &Value) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(value).unwrap();
        bytes.push(b'\n');
        bytes
    }

    fn group(start: u64, finish: u64, velocities: [u16; 3]) -> Vec<u8> {
        let mut bytes = line(&event(0, 3, 0, 0, start));
        for (id, velocity) in [7_u64, 8, 9].into_iter().zip(velocities) {
            bytes.extend(line(&event(1, id, 1, u64::from(velocity) << 16, finish)));
        }
        bytes
    }

    fn motion_event(kind: u8, id: u64, seq: u64, stamp: u64) -> Value {
        let mut value = event(kind, id, 0, seq, stamp);
        value["site"] = json!(32);
        value
    }

    fn motion_group(start: u64, finish: u64) -> Vec<u8> {
        let mut bytes = line(&motion_event(0, 3, 0, start));
        for (id, position, velocity) in [
            (7, 0x8000_u16, 0x8032_u16),
            (8, 4095, 1000),
            (9, u16::MAX, u16::MAX),
        ] {
            let seq = u64::from(position) | (u64::from(velocity) << 16);
            bytes.extend(line(&motion_event(1, id, seq, finish)));
        }
        bytes
    }

    #[test]
    fn starts_at_eof_and_preserves_partial_lines_multiple_groups_and_raw_words() {
        let file = NamedTempFile::new().unwrap();
        append(file.path(), &group(1, 2, [0; 3]));
        let mut observations = Observations::open(file.path(), [8, 9, 7]).unwrap();
        assert_eq!(observations.poll(&at(10)).unwrap(), [] as [ZeroSample; 0]);
        let mut bytes = group(11, 12, [0x8000, 0x8032, u16::MAX]);
        bytes.extend(group(13, 14, [0, 50, 1000]));
        append(file.path(), &bytes[..23]);
        assert_eq!(observations.poll(&at(14)).unwrap(), [] as [ZeroSample; 0]);
        append(file.path(), &bytes[23..]);
        let samples = observations.poll(&at(14)).unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!((samples[0].started_ns, samples[0].finished_ns), (11, 12));
        assert_eq!(samples[0].wheels.map(|wheel| wheel.id), [8, 9, 7]);
        assert_eq!(
            samples[0].wheels.map(|wheel| wheel.velocity),
            [0x8032, u16::MAX, 0x8000]
        );
        assert_eq!(samples[1].wheels.map(|wheel| wheel.velocity), [50, 1000, 0]);
        assert_eq!(observations.poll(&at(15)).unwrap(), [] as [ZeroSample; 0]);
        append(file.path(), &line(&event(0, 3, 0, 0, 16)));
        for id in [7, 8, 9] {
            append(
                file.path(),
                &line(&event(1, id, 255, u64::from(u32::MAX), 17)),
            );
        }
        let sample = observations.poll(&at(17)).unwrap().remove(0);
        assert!(sample.wheels.iter().all(|wheel| wheel.torque == 255
            && wheel.goal == u16::MAX
            && wheel.velocity == u16::MAX));
    }

    #[test]
    fn rejects_incomplete_repeated_or_unframed_groups_and_latches_failure() {
        let begin = event(0, 3, 0, 0, 10);
        let wheel = event(1, 7, 1, 0, 11);
        let cases = [
            vec![wheel.clone()],
            vec![begin.clone(), wheel.clone(), wheel],
            vec![begin.clone(), begin.clone()],
            vec![begin.clone(), event(1, 10, 1, 0, 11)],
            vec![begin, event(2, 7, 1, 0, 11)],
        ];
        for records in cases {
            let file = NamedTempFile::new().unwrap();
            let mut observations = Observations::open(file.path(), [7, 8, 9]).unwrap();
            for record in records {
                append(file.path(), &line(&record));
            }
            let failure = observations.poll(&at(20)).unwrap_err();
            append(file.path(), &group(21, 22, [0; 3]));
            assert_eq!(observations.poll(&at(22)).unwrap_err(), failure);
        }
        let file = NamedTempFile::new().unwrap();
        let mut observations = Observations::open(file.path(), [7, 8, 9]).unwrap();
        append(file.path(), &line(&event(0, 3, 0, 0, 100)));
        assert_eq!(observations.poll(&at(100)).unwrap(), [] as [ZeroSample; 0]);
        assert!(
            observations
                .poll(&at(100 + GROUP_BUDGET_NS))
                .unwrap_err()
                .contains("缺少")
        );
    }

    #[test]
    fn rejects_bad_fields_future_regressing_or_inconsistent_timestamps() {
        let mut wrong_site = event(0, 3, 0, 0, 10);
        wrong_site["site"] = json!(33);
        let mut negative = event(0, 3, 0, 0, 10);
        negative["stamp"] = json!(-1);
        let cases = [
            vec![event(0, 3, 1, 0, 10)],
            vec![event(0, 3, 0, 1, 10)],
            vec![event(0, 3, 0, 0, 21)],
            vec![event(0, 256, 0, 0, 10)],
            vec![event(0, 3, 256, 0, 10)],
            vec![event(0, 3, 0, u64::from(u32::MAX) + 1, 10)],
            vec![wrong_site],
            vec![negative],
            vec![event(0, 3, 0, 0, 10), event(1, 7, 1, 0, 9)],
            vec![
                event(0, 3, 0, 0, 10),
                event(1, 7, 1, 0, 11),
                event(1, 8, 1, 0, 12),
            ],
        ];
        for records in cases {
            let file = NamedTempFile::new().unwrap();
            let mut observations = Observations::open(file.path(), [7, 8, 9]).unwrap();
            for record in records {
                append(file.path(), &line(&record));
            }
            assert!(observations.poll(&at(20)).is_err());
        }
        let file = NamedTempFile::new().unwrap();
        let mut observations = Observations::open(file.path(), [7, 8, 9]).unwrap();
        append(file.path(), &group(10, 11, [0; 3]));
        observations.poll(&at(20)).unwrap();
        append(file.path(), &line(&event(0, 3, 0, 0, 10)));
        assert!(observations.poll(&at(20)).is_err());
    }

    #[test]
    fn summary_loss_unknown_kind_warning_and_malformed_json_fail_closed() {
        let cases = [
            line(&json!({"target": PUB_TARGET, "message": "串口一秒摘要", "dropped": 1})),
            line(&json!({"target": PUB_TARGET, "site": 31, "kind": 2, "message": "串口诊断事件"})),
            b"{broken}\n".to_vec(),
        ];
        for bytes in cases {
            let file = NamedTempFile::new().unwrap();
            let mut observations = Observations::open(file.path(), [7, 8, 9]).unwrap();
            append(
                file.path(),
                &line(&json!({"target": PUB_TARGET, "message": "串口一秒摘要", "dropped": 0})),
            );
            append(file.path(), &bytes);
            assert!(observations.poll(&at(20)).is_err());
        }
    }

    #[test]
    fn rejects_file_replacement_truncation_and_nonregular_paths() {
        let file = NamedTempFile::new().unwrap();
        assert!(Observations::open(file.path(), [7, 7, 9]).is_err());
        let directory = tempfile::tempdir().unwrap();
        assert!(Observations::open(directory.path(), [7, 8, 9]).is_err());
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(file.path(), &link).unwrap();
        assert!(Observations::open(&link, [7, 8, 9]).is_err());
        let mut observations = Observations::open(file.path(), [7, 8, 9]).unwrap();
        append(
            file.path(),
            &line(&json!({"target": "other", "message": "普通日志"})),
        );
        observations.poll(&at(20)).unwrap();
        file.as_file().set_len(0).unwrap();
        assert!(observations.poll(&at(20)).unwrap_err().contains("回退"));
        let path = directory.path().join("replace");
        std::fs::write(&path, []).unwrap();
        let mut observations = Observations::open(&path, [7, 8, 9]).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, []).unwrap();
        assert!(observations.poll(&at(20)).unwrap_err().contains("替换"));
    }

    #[test]
    fn each_poll_obeys_line_and_byte_budgets_and_rejects_oversized_line() {
        let file = NamedTempFile::new().unwrap();
        let mut observations = Observations::open(file.path(), [7, 8, 9]).unwrap();
        for _ in 0..LINE_BUDGET {
            append(file.path(), &line(&json!({"target": "other"})));
        }
        append(file.path(), &group(10, 11, [0; 3]));
        assert_eq!(observations.poll(&at(20)).unwrap(), [] as [ZeroSample; 0]);
        assert_eq!(observations.poll(&at(20)).unwrap().len(), 1);

        let file = NamedTempFile::new().unwrap();
        let mut observations = Observations::open(file.path(), [7, 8, 9]).unwrap();
        append(
            file.path(),
            &line(&json!({"target": "other", "message": "x".repeat(READ_BUDGET - 64)})),
        );
        append(file.path(), &group(10, 11, [0; 3]));
        assert_eq!(observations.poll(&at(20)).unwrap(), [] as [ZeroSample; 0]);
        assert_eq!(observations.offset, u64::try_from(READ_BUDGET).unwrap());
        assert_eq!(observations.poll(&at(20)).unwrap().len(), 1);

        let file = NamedTempFile::new().unwrap();
        let mut observations = Observations::open(file.path(), [7, 8, 9]).unwrap();
        append(file.path(), &vec![b'x'; MAX_LINE_BYTES + 1]);
        assert_eq!(observations.poll(&at(20)).unwrap(), [] as [ZeroSample; 0]);
        assert!(observations.poll(&at(20)).unwrap_err().contains("单行"));
    }

    #[test]
    fn zero_validation_keeps_unsigned_velocity_contract_for_all_wheels() {
        let mut sample = ZeroSample {
            started_ns: 10,
            finished_ns: 11,
            wheels: [7, 8, 9].map(|id| WheelSample {
                id,
                torque: 1,
                goal: 0,
                velocity: 0,
            }),
        };
        for velocity in [0, 50, 0x8032] {
            sample.wheels[2].velocity = velocity;
            validate_zero_sample(&sample, true).unwrap();
        }
        sample.wheels[1].velocity = 1000;
        validate_zero_sample(&sample, false).unwrap();
        assert!(validate_zero_sample(&sample, true).is_err());
        for velocity in [1001, 0x8000, u16::MAX] {
            sample.wheels[1].velocity = velocity;
            assert!(validate_zero_sample(&sample, false).is_err());
        }
        sample.wheels[1].velocity = 0;
        sample.wheels[0].goal = 0x8000;
        assert!(validate_zero_sample(&sample, false).is_err());
        sample.wheels[0].goal = 0;
        sample.wheels[0].torque = 0;
        assert!(validate_zero_sample(&sample, false).is_err());
    }

    #[test]
    fn motion_and_zero_groups_share_framing_but_keep_raw_fields_and_stream_times() {
        let file = NamedTempFile::new().unwrap();
        let mut observations = Observations::open(file.path(), [8, 9, 7]).unwrap();
        append(file.path(), &group(10, 14, [0; 3]));
        // 不同读组可以时间重叠，各 site 内的时间仍必须推进。
        append(file.path(), &motion_group(11, 12));
        let zero = observations.poll(&at(20)).unwrap();
        assert_eq!(zero.len(), 1);
        let motion = observations.take_motion();
        assert_eq!(motion.len(), 1);
        assert_eq!((motion[0].started_ns, motion[0].finished_ns), (11, 12));
        assert_eq!(motion[0].wheels.map(|wheel| wheel.id), [8, 9, 7]);
        assert_eq!(
            motion[0].wheels.map(|wheel| wheel.position),
            [4095, u16::MAX, 0x8000]
        );
        assert_eq!(
            motion[0].wheels.map(|wheel| wheel.velocity),
            [1000, u16::MAX, 0x8032]
        );
        assert_eq!(observations.take_motion(), [] as [MotionSample; 0]);
        append(file.path(), &line(&motion_event(0, 3, 0, 12)));
        assert!(observations.poll(&at(20)).is_err());
    }

    #[test]
    fn motion_queue_is_bounded_and_bad_or_cross_site_groups_fail_closed() {
        let file = NamedTempFile::new().unwrap();
        let mut observations = Observations::open(file.path(), [7, 8, 9]).unwrap();
        for index in 0..=u64::try_from(MOTION_CAPACITY).unwrap() {
            append(file.path(), &motion_group(1 + index * 2, 2 + index * 2));
        }
        let clock = MockClock::new(MonoRawNs(1000));
        assert_eq!(observations.poll(&clock).unwrap(), [] as [ZeroSample; 0]);
        assert_eq!(observations.poll(&clock).unwrap(), [] as [ZeroSample; 0]);
        assert_eq!(observations.motion.len(), MOTION_CAPACITY);
        let error = observations.poll(&clock).unwrap_err();
        assert!(error.contains("128"));
        assert_eq!(observations.take_motion(), [] as [MotionSample; 0]);
        assert_eq!(observations.poll(&clock).unwrap_err(), error);

        let mut bad_reserved = motion_event(1, 7, 0, 11);
        bad_reserved["b"] = json!(1);
        for records in [
            vec![motion_event(0, 3, 0, 10), bad_reserved],
            vec![motion_event(0, 3, 0, 10), event(1, 7, 1, 0, 11)],
            vec![
                motion_event(0, 3, 0, 10),
                motion_event(1, 7, 0, 11),
                motion_event(1, 7, 0, 11),
            ],
        ] {
            let file = NamedTempFile::new().unwrap();
            let mut observations = Observations::open(file.path(), [7, 8, 9]).unwrap();
            for record in records {
                append(file.path(), &line(&record));
            }
            assert!(observations.poll(&clock).is_err());
        }
    }

    #[test]
    fn file_snapshot_precedes_clock_cutoff_and_complete_zero_group_obeys_budget() {
        let file = NamedTempFile::new().unwrap();
        let mut observations = Observations::open(file.path(), [7, 8, 9]).unwrap();
        let first = group(10, 11, [0; 3]);
        append(file.path(), &first);
        let clock = AppendOnNow {
            clock: MockClock::new(MonoRawNs(12)),
            path: file.path().to_owned(),
            bytes: group(13, 14, [0; 3]),
            appended: AtomicBool::new(false),
        };
        assert_eq!(observations.poll(&clock).unwrap().len(), 1);
        assert_eq!(observations.offset, u64::try_from(first.len()).unwrap());
        assert_eq!(observations.poll(&at(20)).unwrap().len(), 1);
        let file = NamedTempFile::new().unwrap();
        let mut observations = Observations::open(file.path(), [7, 8, 9]).unwrap();
        append(file.path(), &group(100, 100 + GROUP_BUDGET_NS, [0; 3]));
        assert!(
            observations
                .poll(&at(100 + GROUP_BUDGET_NS))
                .unwrap_err()
                .contains("500ms")
        );
    }

    #[test]
    fn zero_loss_summary_must_follow_the_last_zero_group_in_read_order() {
        let file = NamedTempFile::new().unwrap();
        let mut observations = Observations::open(file.path(), [7, 8, 9]).unwrap();
        let summary = line(&json!({"target": PUB_TARGET, "message": "串口一秒摘要", "dropped": 0}));
        append(file.path(), &summary);
        observations.poll(&at(20)).unwrap();
        assert_eq!(observations.summary_count(), 1);
        assert!(!observations.summary_after_last_zero());
        append(file.path(), &group(10, 11, [0; 3]));
        observations.poll(&at(20)).unwrap();
        assert!(!observations.summary_after_last_zero());
        append(file.path(), &summary);
        observations.poll(&at(20)).unwrap();
        assert!(observations.summary_after_last_zero());
        append(file.path(), &group(12, 13, [0; 3]));
        append(file.path(), &summary);
        observations.poll(&at(20)).unwrap();
        assert_eq!(observations.summary_count(), 3);
        assert!(observations.summary_after_last_zero());
        append(
            file.path(),
            &line(&json!({"target": PUB_TARGET, "message": "串口一秒摘要", "dropped": 1})),
        );
        assert!(observations.poll(&at(20)).is_err());
        assert!(!observations.summary_after_last_zero());
    }

    #[test]
    fn summary_cannot_accept_a_pending_group_partial_line_or_unchecked_backlog() {
        for suffix in [line(&event(0, 3, 0, 0, 12)), b"{".to_vec()] {
            let file = NamedTempFile::new().unwrap();
            let mut observations = Observations::open(file.path(), [7, 8, 9]).unwrap();
            append(file.path(), &group(10, 11, [0; 3]));
            append(
                file.path(),
                &line(&json!({"target": PUB_TARGET, "message": "串口一秒摘要", "dropped": 0})),
            );
            append(file.path(), &suffix);
            observations.poll(&at(20)).unwrap();
            assert_eq!(observations.summary_count(), 1);
            assert!(!observations.summary_after_last_zero());
        }
        let file = NamedTempFile::new().unwrap();
        let mut observations = Observations::open(file.path(), [7, 8, 9]).unwrap();
        append(file.path(), &group(10, 11, [0; 3]));
        append(
            file.path(),
            &line(&json!({"target": PUB_TARGET, "message": "串口一秒摘要", "dropped": 0})),
        );
        for _ in 0..LINE_BUDGET {
            append(file.path(), &line(&json!({"target": "other"})));
        }
        append(
            file.path(),
            &line(&json!({"target": PUB_TARGET, "message": "串口一秒摘要", "dropped": 1})),
        );
        observations.poll(&at(20)).unwrap();
        assert_eq!(observations.summary_count(), 1);
        assert!(!observations.summary_after_last_zero());
        assert!(observations.poll(&at(20)).is_err());
    }
}
