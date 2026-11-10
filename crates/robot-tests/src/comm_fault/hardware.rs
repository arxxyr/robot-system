//! 显式受监督现场入口；只在 real-hw 编译下可打开身份校验通过的 Feetech 总线。

use super::report::Report;
use super::{Args, Phase};

mod physical {
    use std::collections::HashMap;
    #[cfg(feature = "real-hw")]
    use std::io::Write;
    #[cfg(feature = "real-hw")]
    use std::os::unix::fs::FileTypeExt;
    #[cfg(feature = "real-hw")]
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use robot_adapter_feetech::adapter::FeetechAdapterConfig;
    use robot_adapter_feetech::transport::DrainReport;
    #[cfg(feature = "real-hw")]
    use robot_adapter_feetech::transport::real::RealSerialTransport;
    use robot_adapter_feetech::{FeetechAdapter, FeetechBus, FeetechError, Motor, SerialTransport};
    use robot_core::time::{RobotClock, SystemClock};
    #[cfg(feature = "real-hw")]
    use robot_s0::deploy::DeployArm;
    #[cfg(feature = "real-hw")]
    use robot_s0::deploy::identity::{ArmIdentity, expected, load_and_verify_arm_config};
    use serde_json::{Value, json};

    #[cfg(feature = "real-hw")]
    use super::super::report::file_hash;
    #[cfg(feature = "real-hw")]
    use super::Args;
    use super::Phase;

    const WARMUP: usize = 100;
    const SAMPLES: usize = 1000;
    const FRAME_CAPACITY: usize = 260;
    // 四次 IO / ping，留出设置与异常调用空间；满后只计丢失，绝不扩容。
    const RECORD_CAPACITY: usize = (WARMUP + SAMPLES) * 8 + 16;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum IoKind {
        Write,
        Read,
        SetTimeout,
        Discard,
        Drain,
    }

    impl IoKind {
        #[cfg(feature = "real-hw")]
        const fn label(self) -> &'static str {
            match self {
                Self::Write => "write_all",
                Self::Read => "read_exact",
                Self::SetTimeout => "set_timeout",
                Self::Discard => "discard_input",
                Self::Drain => "drain_until_quiet",
            }
        }
    }

    #[derive(Clone, Copy)]
    struct CallContext {
        sample_id: usize,
        motor_id: Option<u8>,
        io_index: usize,
        transaction: usize,
    }

    #[derive(Clone)]
    struct IoSample {
        context: Option<CallContext>,
        kind: IoKind,
        started_ns: u64,
        finished_ns: u64,
        requested_bytes: usize,
        success: bool,
        raw: [u8; FRAME_CAPACITY],
        raw_len: usize,
    }

    struct IoRecorder {
        records: Vec<IoSample>,
        context: Option<CallContext>,
        dropped: usize,
        truncated: usize,
    }

    impl IoRecorder {
        fn new() -> Self {
            Self {
                records: Vec::with_capacity(RECORD_CAPACITY),
                context: None,
                dropped: 0,
                truncated: 0,
            }
        }

        fn begin(&mut self, sample_id: usize, motor_id: Option<u8>) -> usize {
            self.context = Some(CallContext {
                sample_id,
                motor_id,
                io_index: 0,
                transaction: 0,
            });
            self.records.len()
        }

        fn record(&mut self, mut record: IoSample) {
            if let Some(context) = self.context.as_mut() {
                context.io_index += 1;
                if record.kind == IoKind::Write {
                    context.transaction += 1;
                }
            }
            record.context = self.context;
            if record.requested_bytes > FRAME_CAPACITY {
                self.truncated += 1;
            }
            if self.records.len() == self.records.capacity() {
                self.dropped += 1;
                return;
            }
            self.records.push(record);
        }
    }

    struct MeasuredTransport<T> {
        inner: T,
        samples: Arc<Mutex<IoRecorder>>,
    }

    impl<T> MeasuredTransport<T> {
        fn record(
            &self,
            kind: IoKind,
            started_ns: u64,
            finished_ns: u64,
            requested_bytes: usize,
            success: bool,
            raw: &[u8],
        ) {
            let mut record = IoSample {
                context: None,
                kind,
                started_ns,
                finished_ns,
                requested_bytes,
                success,
                raw: [0; FRAME_CAPACITY],
                raw_len: raw.len().min(FRAME_CAPACITY),
            };
            record.raw[..record.raw_len].copy_from_slice(&raw[..record.raw_len]);
            self.samples.lock().unwrap().record(record);
        }
    }

    impl<T: SerialTransport> SerialTransport for MeasuredTransport<T> {
        fn write_all(&mut self, bytes: &[u8]) -> Result<(), FeetechError> {
            let start = SystemClock.now().0;
            let result = self.inner.write_all(bytes);
            let finish = SystemClock.now().0;
            // 原始 TX 表示本次 write_all 的完整入参；失败不冒充整帧已到达 UART。
            self.record(
                IoKind::Write,
                start,
                finish,
                bytes.len(),
                result.is_ok(),
                bytes,
            );
            result
        }
        fn read_exact(&mut self, bytes: &mut [u8]) -> Result<(), FeetechError> {
            let start = SystemClock.now().0;
            let result = self.inner.read_exact(bytes);
            let finish = SystemClock.now().0;
            // 失败时未知缓冲的有效前缀；保留请求长度和失败事实，不伪造 RX 字节。
            let received = match &result {
                Ok(()) => &*bytes,
                Err(_) => &[],
            };
            self.record(
                IoKind::Read,
                start,
                finish,
                bytes.len(),
                result.is_ok(),
                received,
            );
            result
        }
        fn set_timeout(&mut self, timeout: Duration) -> Result<(), FeetechError> {
            let start = SystemClock.now().0;
            let result = self.inner.set_timeout(timeout);
            self.record(
                IoKind::SetTimeout,
                start,
                SystemClock.now().0,
                0,
                result.is_ok(),
                &[],
            );
            result
        }
        fn is_connected(&self) -> bool {
            self.inner.is_connected()
        }
        fn discard_input(&mut self) -> Result<(), FeetechError> {
            let start = SystemClock.now().0;
            let result = self.inner.discard_input();
            self.record(
                IoKind::Discard,
                start,
                SystemClock.now().0,
                0,
                result.is_ok(),
                &[],
            );
            result
        }
        fn drain_until_quiet(
            &mut self,
            quiet: Duration,
            budget: Duration,
        ) -> Result<DrainReport, FeetechError> {
            let start = SystemClock.now().0;
            let result = self.inner.drain_until_quiet(quiet, budget);
            self.record(
                IoKind::Drain,
                start,
                SystemClock.now().0,
                0,
                result.is_ok(),
                &[],
            );
            result
        }
    }

    struct OperationSample {
        sample_id: usize,
        motor_id: Option<u8>,
        started_ns: u64,
        finished_ns: u64,
        record_start: usize,
        record_end: usize,
        model_number: Option<u16>,
        error: Option<String>,
    }

    #[cfg(any(feature = "real-hw", test))]
    fn percentile(values: &[u64], fraction: usize) -> u64 {
        values[(values.len() * fraction).div_ceil(100).saturating_sub(1)]
    }

    #[cfg(any(feature = "real-hw", test))]
    fn histogram(values: &[u64]) -> Value {
        if values.is_empty() {
            return Value::Null;
        }
        let mut ordered = values.to_vec();
        ordered.sort_unstable();
        json!({"count": values.len(), "p50_ns": percentile(&ordered, 50),
            "p99_ns": percentile(&ordered, 99), "max_ns": ordered[ordered.len()-1],
            "samples_ns": values})
    }

    #[cfg(any(feature = "real-hw", test))]
    fn property<'a>(properties: &'a str, key: &str) -> Result<&'a str, String> {
        let mut matches = properties
            .lines()
            .filter_map(|line| line.split_once('='))
            .filter_map(|(name, value)| (name == key).then_some(value));
        match (matches.next(), matches.next()) {
            (Some(value), None) => Ok(value),
            _ => Err(format!("udev 属性 {key} 缺失或重复")),
        }
    }

    #[cfg(any(feature = "real-hw", test))]
    fn check_usb_identity(properties: &str) -> Result<(), String> {
        for (key, value) in [
            ("ID_VENDOR_ID", "1a86"),
            ("ID_MODEL_ID", "55d3"),
            ("ID_USB_DRIVER", "cdc_acm"),
        ] {
            let actual = property(properties, key)?;
            if actual != value {
                return Err(format!("udev 身份不符：{key}={actual}，要求 {value}"));
            }
        }
        Ok(())
    }

    #[cfg(feature = "real-hw")]
    fn environment(address: &Path, directory: &Path) -> Result<Value, String> {
        let kernel = std::process::Command::new("uname")
            .arg("-a")
            .output()
            .map_err(|e| e.to_string())?;
        if !kernel.status.success() {
            return Err("uname 失败".to_owned());
        }
        let udev = std::process::Command::new("udevadm")
            .args(["info", "--query=property", "--name"])
            .arg(address)
            .output()
            .map_err(|e| e.to_string())?;
        if !udev.status.success() {
            return Err(format!(
                "无法取得总线 udev 身份：{}",
                String::from_utf8_lossy(&udev.stderr)
            ));
        }
        let properties = String::from_utf8(udev.stdout).map_err(|e| e.to_string())?;
        std::fs::write(directory.join("udev.properties"), &properties)
            .map_err(|e| e.to_string())?;
        check_usb_identity(&properties)?;
        let model = std::fs::read("/proc/device-tree/model")
            .map_err(|e| format!("读取 Orin 设备模型失败：{e}"))?;
        let model = String::from_utf8_lossy(&model)
            .trim_end_matches('\0')
            .to_owned();
        if !model.to_ascii_lowercase().contains("orin") {
            return Err(format!("现场判据仅用于 Orin，当前 model={model}"));
        }
        if !std::fs::metadata(address)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_char_device()
        {
            return Err("总线身份路径未指向字符设备".to_owned());
        }
        Ok(
            json!({"kernel": String::from_utf8_lossy(&kernel.stdout).trim(), "model": model,
            "address": address, "canonical_address": std::fs::canonicalize(address).map_err(|e| e.to_string())?,
            "udev": properties,
            "governor": std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor").ok(),
            "l4t": std::fs::read_to_string("/etc/nv_tegra_release").ok()}),
        )
    }

    fn check_frame(frame: &[u8]) -> Result<(), String> {
        if frame.len() < 6 || frame[..2] != [0xff, 0xff] {
            return Err("TX 协议头或最小长度错误".to_owned());
        }
        if usize::from(frame[3]) + 4 != frame.len() {
            return Err("TX 长度字段不符或包含额外帧".to_owned());
        }
        if frame[2..]
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_add(*byte))
            != 0xff
        {
            return Err("TX 校验和错误".to_owned());
        }
        Ok(())
    }

    fn check_stop_frame(frame: &[u8], motor_ids: &[u8]) -> Result<(), String> {
        check_frame(frame)?;
        if frame.len() != 8 + motor_ids.len() * 2
            || frame[2] != 0xfe
            || frame[4..7] != [0x83, 40, 1]
        {
            return Err("Safety 必须为单帧广播 SYNC_WRITE Torque_Enable，数据宽度为 1".to_owned());
        }
        let mut seen = [false; 256];
        for entry in frame[7..frame.len() - 1].as_chunks::<2>().0 {
            let id = entry[0];
            if !motor_ids.contains(&id) || seen[usize::from(id)] {
                return Err(format!("Safety 电机集合重复或出现配置外 ID：{id}"));
            }
            if entry[1] != 0 {
                return Err(format!(
                    "Safety 出现非零 Torque_Enable：motor={id} value={}",
                    entry[1]
                ));
            }
            seen[usize::from(id)] = true;
        }
        if motor_ids.iter().any(|id| !seen[usize::from(*id)]) {
            return Err("Safety 未覆盖全部配置电机".to_owned());
        }
        Ok(())
    }

    fn observed_enable_commands(recorder: &IoRecorder) -> Option<usize> {
        if recorder.dropped != 0 || recorder.truncated != 0 {
            return None;
        }
        recorder
            .records
            .iter()
            .filter(|record| record.kind == IoKind::Write)
            .try_fold(0, |count, record| {
                let frame = &record.raw[..record.raw_len];
                check_frame(frame).ok()?;
                let enables = match frame[4] {
                    1 if frame.len() == 6 => false,
                    2 if frame.len() == 8 => false,
                    0x83 if frame.len() >= 8
                        && frame[5..7] == [40, 1]
                        && (frame.len() - 8).is_multiple_of(2) =>
                    {
                        frame[7..frame.len() - 1]
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .any(|entry| entry[1] != 0)
                    }
                    _ => return None,
                };
                Some(count + usize::from(enables))
            })
    }

    fn check_ping_frame(frame: &[u8], motor_id: u8, model_read: bool) -> Result<(), String> {
        check_frame(frame)?;
        let valid = if model_read {
            frame.len() == 8 && frame[2] == motor_id && frame[4..7] == [2, 3, 2]
        } else {
            frame.len() == 6 && frame[2] == motor_id && frame[4] == 1
        };
        if !valid {
            return Err("ping 子事务不是选定电机的 PING / Model_Number 读取".to_owned());
        }
        Ok(())
    }

    fn check_operation(
        phase: Phase,
        operation: &OperationSample,
        records: &[IoSample],
        motor_ids: &[u8],
    ) -> Result<(), String> {
        let samples = records
            .get(operation.record_start..operation.record_end)
            .ok_or("调用记录区间无效")?;
        let mut previous_finish = operation.started_ns;
        let mut transaction = 0;
        for (index, sample) in samples.iter().enumerate() {
            let context = sample.context.ok_or("调用记录缺少样本关联")?;
            if sample.kind == IoKind::Write {
                transaction += 1;
            }
            if context.transaction != transaction
                || context.sample_id != operation.sample_id
                || context.motor_id != operation.motor_id
                || context.io_index != index + 1
                || sample.started_ns < previous_finish
                || sample.finished_ns < sample.started_ns
                || sample.finished_ns > operation.finished_ns
            {
                return Err("调用记录关联或起止时刻不一致".to_owned());
            }
            previous_finish = sample.finished_ns;
        }
        if samples.iter().any(|sample| !sample.success) {
            return Err("存在失败的 transport 调用".to_owned());
        }
        match phase {
            Phase::OrinBroadcast => {
                let [write] = samples else {
                    return Err(format!(
                        "Safety 必须恰好一次 write 且零 read，实际 IO 次数 {}",
                        samples.len()
                    ));
                };
                if write.kind != IoKind::Write {
                    return Err("Safety 唯一 IO 不是 write_all".to_owned());
                }
                check_stop_frame(&write.raw[..write.raw_len], motor_ids)
            }
            Phase::OrinCharacterize => {
                let [ping_write, ping_read, model_write, model_read] = samples else {
                    return Err("完整 ping 必须有两个 write/read 子事务".to_owned());
                };
                if [
                    ping_write.kind,
                    ping_read.kind,
                    model_write.kind,
                    model_read.kind,
                ] != [IoKind::Write, IoKind::Read, IoKind::Write, IoKind::Read]
                    || ping_read.requested_bytes != 6
                    || model_read.requested_bytes != 8
                {
                    return Err("ping 两个子事务的 IO 顺序或读取长度不符".to_owned());
                }
                let id = operation.motor_id.ok_or("ping 样本缺少电机身份")?;
                check_ping_frame(&ping_write.raw[..ping_write.raw_len], id, false)?;
                check_ping_frame(&model_write.raw[..model_write.raw_len], id, true)
            }
            _ => Err("非现场总线测量 phase".to_owned()),
        }
    }

    #[cfg(feature = "real-hw")]
    fn characterize<T: SerialTransport>(
        bus: &mut FeetechBus<T>,
        recorder: &Mutex<IoRecorder>,
        motor_ids: &[u8],
    ) -> Result<Vec<OperationSample>, String> {
        bus.confirm_hygiene().map_err(|e| e.to_string())?;
        let mut operations = Vec::with_capacity(WARMUP + SAMPLES);
        for sample_id in 0..WARMUP + SAMPLES {
            let motor_id = motor_ids[sample_id % motor_ids.len()];
            let record_start = recorder.lock().unwrap().begin(sample_id, Some(motor_id));
            let started_ns = SystemClock.now().0;
            let response = bus.ping(motor_id);
            let finished_ns = SystemClock.now().0;
            let (model_number, error) = match response {
                Ok(Some(model)) => (Some(model), None),
                other => (None, Some(format!("ping 响应错误：{other:?}"))),
            };
            let record_end = recorder.lock().unwrap().records.len();
            operations.push(OperationSample {
                sample_id,
                motor_id: Some(motor_id),
                started_ns,
                finished_ns,
                record_start,
                record_end,
                model_number,
                error,
            });
        }
        recorder.lock().unwrap().context = None;
        Ok(operations)
    }

    #[cfg(feature = "real-hw")]
    fn broadcast<T: SerialTransport + 'static>(
        mut adapter: FeetechAdapter<T>,
        recorder: &Mutex<IoRecorder>,
    ) -> Vec<OperationSample> {
        let mut operations = Vec::with_capacity(WARMUP + SAMPLES);
        for sample_id in 0..WARMUP + SAMPLES {
            let record_start = recorder.lock().unwrap().begin(sample_id, Some(0xfe));
            let started_ns = SystemClock.now().0;
            let response = adapter.safety_stop_fast();
            let finished_ns = SystemClock.now().0;
            let record_end = recorder.lock().unwrap().records.len();
            operations.push(OperationSample {
                sample_id,
                motor_id: Some(0xfe),
                started_ns,
                finished_ns,
                record_start,
                record_end,
                model_number: None,
                error: response.err().map(|error| error.to_string()),
            });
        }
        recorder.lock().unwrap().context = None;
        operations
    }

    #[cfg(feature = "real-hw")]
    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write;
        let mut text = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            write!(&mut text, "{byte:02x}").unwrap();
        }
        text
    }

    #[cfg(feature = "real-hw")]
    fn write_csv(
        directory: &Path,
        recorder: &IoRecorder,
        operations: &[OperationSample],
    ) -> Result<Value, String> {
        let transport_path = directory.join("transport.csv");
        let mut transport = std::fs::File::create(&transport_path).map_err(|e| e.to_string())?;
        writeln!(transport, "record_id,sample_id,warmup,motor_id,subtransaction,io_index,operation,requested_bytes,started_ns,finished_ns,elapsed_ns,success,raw_tx_argument_hex,successful_rx_hex")
            .map_err(|e| e.to_string())?;
        for (index, record) in recorder.records.iter().enumerate() {
            let (sample_id, warmup, motor_id, transaction, io_index) = match record.context {
                Some(context) => (
                    context.sample_id.to_string(),
                    (context.sample_id < WARMUP).to_string(),
                    context
                        .motor_id
                        .map_or_else(String::new, |id| id.to_string()),
                    context.transaction,
                    context.io_index,
                ),
                None => (String::new(), String::new(), String::new(), 0, 0),
            };
            let raw = hex(&record.raw[..record.raw_len]);
            let (tx, rx) = match record.kind {
                IoKind::Write => (raw.as_str(), ""),
                IoKind::Read => ("", raw.as_str()),
                _ => ("", ""),
            };
            writeln!(transport, "{index},{sample_id},{warmup},{motor_id},{transaction},{io_index},{},{},{},{},{},{},{tx},{rx}",
                record.kind.label(), record.requested_bytes, record.started_ns, record.finished_ns,
                record.finished_ns.saturating_sub(record.started_ns), record.success).map_err(|e| e.to_string())?;
        }
        transport.sync_all().map_err(|e| e.to_string())?;
        let operation_path = directory.join("operations.csv");
        let mut calls = std::fs::File::create(&operation_path).map_err(|e| e.to_string())?;
        writeln!(calls, "sample_id,warmup,motor_id,started_ns,finished_ns,elapsed_ns,record_start,record_end,model_number,adapter_call_success").map_err(|e| e.to_string())?;
        for call in operations {
            writeln!(
                calls,
                "{},{},{},{},{},{},{},{},{},{}",
                call.sample_id,
                call.sample_id < WARMUP,
                call.motor_id.map_or_else(String::new, |id| id.to_string()),
                call.started_ns,
                call.finished_ns,
                call.finished_ns.saturating_sub(call.started_ns),
                call.record_start,
                call.record_end,
                call.model_number
                    .map_or_else(String::new, |value| value.to_string()),
                call.error.is_none()
            )
            .map_err(|e| e.to_string())?;
        }
        calls.sync_all().map_err(|e| e.to_string())?;
        Ok(
            json!({"transport": transport_path, "operations": operation_path,
            "transport_sha256": file_hash(&transport_path)?, "operations_sha256": file_hash(&operation_path)?}),
        )
    }

    #[cfg(any(feature = "real-hw", test))]
    fn transaction_statistics(recorder: &IoRecorder, transaction: usize) -> Value {
        let selected = recorder.records.iter().filter(|sample| {
            sample.context.is_some_and(|context| {
                context.sample_id >= WARMUP && context.transaction == transaction
            })
        });
        let (mut writes, mut reads, mut round_trips) = (Vec::new(), Vec::new(), Vec::new());
        let mut last_write = None;
        for sample in selected {
            match sample.kind {
                IoKind::Write => {
                    writes.push(sample.finished_ns.saturating_sub(sample.started_ns));
                    last_write = sample
                        .context
                        .map(|context| (context.sample_id, sample.started_ns));
                }
                IoKind::Read => {
                    reads.push(sample.finished_ns.saturating_sub(sample.started_ns));
                    if let Some((sample_id, started_ns)) = last_write.take()
                        && sample
                            .context
                            .is_some_and(|context| context.sample_id == sample_id)
                    {
                        round_trips.push(sample.finished_ns.saturating_sub(started_ns));
                    }
                }
                _ => {}
            }
        }
        json!({"write_all": histogram(&writes), "read_exact": histogram(&reads), "request_response": histogram(&round_trips)})
    }

    #[cfg(any(feature = "real-hw", test))]
    fn summarize(
        phase: Phase,
        recorder: &IoRecorder,
        operations: &[OperationSample],
        motor_ids: &[u8],
    ) -> (Value, bool) {
        let mut errors = Vec::new();
        let mut verified = 0;
        for operation in operations {
            if let Some(error) = &operation.error {
                errors.push(json!({"sample_id": operation.sample_id, "warmup": operation.sample_id < WARMUP, "error": error}));
            }
            match check_operation(phase, operation, &recorder.records, motor_ids) {
                Ok(()) => verified += 1,
                Err(error) => errors.push(json!({"sample_id": operation.sample_id, "warmup": operation.sample_id < WARMUP, "wire_error": error})),
            }
        }
        let durations: Vec<_> = operations
            .iter()
            .filter(|operation| operation.sample_id >= WARMUP)
            .map(|operation| operation.finished_ns.saturating_sub(operation.started_ns))
            .collect();
        let exact_count = operations.len() == WARMUP + SAMPLES
            && durations.len() == SAMPLES
            && operations
                .iter()
                .enumerate()
                .all(|(index, operation)| index == operation.sample_id);
        let unexpected_setup_io = recorder
            .records
            .iter()
            .filter(|record| record.context.is_none())
            .any(|record| match (phase, record.kind) {
                (_, IoKind::SetTimeout)
                | (Phase::OrinCharacterize, IoKind::Discard | IoKind::Drain) => !record.success,
                _ => true,
            });
        let wire_valid = exact_count
            && verified == WARMUP + SAMPLES
            && !unexpected_setup_io
            && recorder.dropped == 0
            && recorder.truncated == 0;
        let latency_valid = phase != Phase::OrinBroadcast
            || durations.iter().all(|duration| *duration <= 10_000_000);
        let success = wire_valid && latency_valid && errors.is_empty();
        let transactions = match phase {
            Phase::OrinCharacterize => {
                json!({"1_ping": transaction_statistics(recorder, 1), "2_model_number_read": transaction_statistics(recorder, 2)})
            }
            _ => json!({"1_torque_disable_broadcast": transaction_statistics(recorder, 1)}),
        };
        // 从所有实际 write_all 入参推导使能帧数；未知帧或记录缺失返回 null，不猜测为零。
        let torque_enable_commands = observed_enable_commands(recorder);
        (
            json!({"warmup": WARMUP, "samples": SAMPLES, "operation_formal": histogram(&durations),
            "transactions_formal": transactions, "errors": errors, "latency_valid": latency_valid,
            "wire_audit": {"valid": wire_valid, "verified_calls_including_warmup": verified,
                "record_count": recorder.records.len(), "dropped": recorder.dropped, "truncated": recorder.truncated,
                "unexpected_setup_io": unexpected_setup_io,
                "torque_enable_commands": torque_enable_commands, "configured_motor_ids": motor_ids},
            "scope": if phase == Phase::OrinCharacterize { "完整 ping 含 PING 与 Model_Number READ 两个子事务；响应时延 characterization 不作为故障 deadline 判据" }
                else { "真实安全广播调用与 host write_all 返回时延；不代替 UART 发完或物理断扭确认" },
            "measurement": "transport 起止时刻包围真实底层调用，不含记录操作；完整调用时延包含固定容量记录开销。统计只含正式样本，预热原始数据与错误完整保留。"}),
            success,
        )
    }

    #[cfg(feature = "real-hw")]
    fn measure<T: SerialTransport + 'static>(
        phase: Phase,
        transport: T,
        identity: &ArmIdentity,
        clock: Arc<dyn RobotClock>,
        samples: &Mutex<IoRecorder>,
    ) -> Result<Vec<OperationSample>, String> {
        let motors: HashMap<_, _> = identity
            .joints
            .iter()
            .map(|name| (*name).to_owned())
            .zip(identity.motor_ids.iter().copied().map(|id| Motor { id }))
            .collect();
        let mut bus = FeetechBus::new(transport, motors).map_err(|e| e.to_string())?;
        bus.set_timeout(Duration::from_millis(50))
            .map_err(|e| e.to_string())?;
        match phase {
            Phase::OrinCharacterize => characterize(&mut bus, samples, identity.motor_ids),
            Phase::OrinBroadcast => {
                let adapter = FeetechAdapter::new(
                    FeetechAdapterConfig {
                        clock,
                        preempt: None,
                        diag: None,
                        call_deadline: Duration::from_millis(50),
                        adapter_name: "feetech_sts3215",
                        joint_names: identity
                            .joints
                            .iter()
                            .map(|name| (*name).to_owned())
                            .collect(),
                        // H3 只调用全配置电机的断扭广播，不执行角度命令与 connect。
                        calibrations: Vec::new(),
                        calibration_range_tolerances_raw: Vec::new(),
                    },
                    bus,
                )
                .map_err(|e| e.to_string())?;
                Ok(broadcast(adapter, samples))
            }
            _ => unreachable!(),
        }
    }

    #[cfg(feature = "real-hw")]
    pub(super) fn run(args: &Args) -> Result<Value, String> {
        let config_path = args.config.as_ref().ok_or("现场配置缺失")?;
        let arm = match args.hardware_id.as_deref() {
            Some("left_arm") => DeployArm::Left,
            Some("right_arm") => DeployArm::Right,
            _ => return Err("现场硬件身份只接受 left_arm / right_arm".to_owned()),
        };
        let config_sha256 = file_hash(config_path)?;
        let verified = load_and_verify_arm_config(arm, config_path).map_err(|e| e.to_string())?;
        if file_hash(config_path)? != config_sha256 {
            return Err("D2 校验期间配置发生变化".to_owned());
        }
        let identity = expected(arm);
        let address = PathBuf::from(&verified.address);
        let label = match args.phase {
            Phase::OrinCharacterize => "orin_characterize",
            Phase::OrinBroadcast => "orin_broadcast",
            _ => return Err("非总线测量 phase".to_owned()),
        };
        let directory = args.evidence_dir.join(label);
        std::fs::create_dir(&directory).map_err(|e| format!("现场证据目录必须新建：{e}"))?;
        let environment = environment(&address, &directory)?;
        let metadata = json!({"environment": environment, "config": config_path, "config_sha256": config_sha256,
            "identity": {"arm": arm.label(), "robot_id": verified.robot_id, "hardware_id": verified.hardware_id,
                "address": verified.address, "calibration_path": verified.calibration_path,
                "calibration_sha256": file_hash(Path::new(&verified.calibration_path))?,
                "motor_ids": identity.motor_ids, "joints": identity.joints, "auto_enable": false}});
        std::fs::write(
            directory.join("environment.json"),
            serde_json::to_vec_pretty(&metadata).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        // 与生产 S0 / 授权 CLI 使用同一独占锁；不消费或改写 RX 授权和急停 latch。
        let trust_directory = PathBuf::from("/var/lib/robot").join(&verified.robot_id);
        let _lock = robot_s0::rx_trust::acquire_trust_lock(&trust_directory)
            .map_err(|e| format!("总线实例锁失败：{e:?}"))?;
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let samples = Arc::new(Mutex::new(IoRecorder::new()));
        let transport = RealSerialTransport::open_with_clock(&address, 1_000_000, clock.clone())
            .map_err(|e| e.to_string())?;
        let transport = MeasuredTransport {
            inner: transport,
            samples: samples.clone(),
        };
        let (operations, setup_error) =
            match measure(args.phase, transport, identity, clock, &samples) {
                Ok(operations) => (operations, None),
                Err(error) => (Vec::new(), Some(error)),
            };
        let recorder = samples.lock().unwrap();
        let csv = write_csv(&directory, &recorder, &operations)?;
        let (measurement, success) =
            summarize(args.phase, &recorder, &operations, identity.motor_ids);
        let success = success && setup_error.is_none();
        let result = json!({"metadata": metadata, "measurement": measurement, "csv": csv, "setup_error": setup_error});
        std::fs::write(
            directory.join("measurement.json"),
            serde_json::to_vec_pretty(&result).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        if !success {
            return Err(format!(
                "现场测量含错误或不满足判据，完整样本见 {}",
                directory.display()
            ));
        }
        Ok(result)
    }

    /// 实际执行 Feetech 安全广播编码；底层只用内存传输，不形成现场测量结果。
    pub(super) fn software_protocol_probe() -> Result<Value, String> {
        use robot_adapter_feetech::transport::MockTransport;

        let motor_ids = [1, 3];
        let joint_names = vec!["joint_1".to_owned(), "joint_3".to_owned()];
        let motors = joint_names
            .iter()
            .cloned()
            .zip(motor_ids.map(|id| Motor { id }))
            .collect::<HashMap<_, _>>();
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let samples = Arc::new(Mutex::new(IoRecorder::new()));
        let (inner, handle) = MockTransport::new();
        let transport = MeasuredTransport {
            inner,
            samples: samples.clone(),
        };
        let bus = FeetechBus::new(transport, motors).map_err(|error| error.to_string())?;
        let mut adapter = FeetechAdapter::new(
            FeetechAdapterConfig {
                clock,
                preempt: None,
                diag: None,
                call_deadline: Duration::from_millis(50),
                adapter_name: "feetech_sts3215",
                joint_names,
                calibrations: Vec::new(),
                calibration_range_tolerances_raw: Vec::new(),
            },
            bus,
        )
        .map_err(|error| error.to_string())?;
        let record_start = samples.lock().unwrap().begin(0, Some(0xfe));
        let started_ns = SystemClock.now().0;
        let response = adapter.safety_stop_fast();
        let finished_ns = SystemClock.now().0;
        let recorder = samples.lock().unwrap();
        let operation = OperationSample {
            sample_id: 0,
            motor_id: Some(0xfe),
            started_ns,
            finished_ns,
            record_start,
            record_end: recorder.records.len(),
            model_number: None,
            error: response.err().map(|error| error.to_string()),
        };
        if let Some(error) = &operation.error {
            return Err(format!("软件 Feetech 安全广播失败：{error}"));
        }
        check_operation(
            Phase::OrinBroadcast,
            &operation,
            &recorder.records,
            &motor_ids,
        )?;
        let wire = handle.drain_tx();
        check_stop_frame(&wire, &motor_ids)?;
        let setup_is_timeout_only = recorder.records[..record_start]
            .iter()
            .all(|record| record.kind == IoKind::SetTimeout && record.success);
        if !setup_is_timeout_only
            || recorder.dropped != 0
            || recorder.truncated != 0
            || observed_enable_commands(&recorder) != Some(0)
        {
            return Err("软件安全广播出现未归属 IO、记录丢失或非零上扭命令".to_owned());
        }
        Ok(json!({
            "scope": "真实 FeetechAdapter 安全广播编码与内存传输记录；不测 UART 或物理断扭",
            "hardware_only": false, "physical_measurement": false,
            "configured_motor_ids": motor_ids, "actual_tx": wire,
            "transport_calls": recorder.records.len(),
            "write_calls": recorder.records.iter().filter(|record| record.kind == IoKind::Write).count(),
            "read_calls": recorder.records.iter().filter(|record| record.kind == IoKind::Read).count(),
            "started_ns": started_ns, "finished_ns": finished_ns,
            "model_number": operation.model_number,
            "dropped": recorder.dropped, "truncated": recorder.truncated
        }))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn software_protocol_entry_exercises_actual_feetech_encoder_without_a_device() {
            let evidence = software_protocol_probe().unwrap();
            assert_eq!(evidence["hardware_only"], false);
            assert_eq!(evidence["physical_measurement"], false);
            assert_eq!(evidence["configured_motor_ids"], json!([1, 3]));
            assert_eq!(evidence["write_calls"], 1);
            assert_eq!(evidence["read_calls"], 0);
            assert_eq!(evidence["transport_calls"], 2);
            assert_eq!(evidence["actual_tx"].as_array().unwrap().len(), 12);
        }

        fn frame_with_checksum(mut frame: Vec<u8>) -> Vec<u8> {
            frame[3] = u8::try_from(frame.len() - 3).unwrap();
            let checksum = !frame[2..]
                .iter()
                .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
            frame.push(checksum);
            frame
        }

        fn stop_frame(entries: &[(u8, u8)]) -> Vec<u8> {
            let mut frame = vec![0xff, 0xff, 0xfe, 0, 0x83, 40, 1];
            for (id, torque) in entries {
                frame.extend_from_slice(&[*id, *torque]);
            }
            frame_with_checksum(frame)
        }

        #[test]
        fn safety_wire_oracle_rejects_enable_duplicate_missing_and_extra_motors() {
            assert!(check_stop_frame(&stop_frame(&[(1, 0), (2, 0)]), &[1, 2]).is_ok());
            assert!(check_stop_frame(&stop_frame(&[(2, 0), (1, 0)]), &[1, 2]).is_ok());
            for entries in [
                &[(1, 1), (2, 0)][..],
                &[(1, 0), (1, 0)],
                &[(1, 0)],
                &[(1, 0), (3, 0)],
            ] {
                assert!(check_stop_frame(&stop_frame(entries), &[1, 2]).is_err());
            }
        }

        #[test]
        fn safety_wire_oracle_rejects_malformed_header_checksum_register_and_extra_frame() {
            let frame = stop_frame(&[(1, 0), (2, 0)]);
            for (index, replacement) in
                [(0, 0xfe), (2, 1), (3, 1), (4, 3), (5, 41), (6, 2), (11, 0)]
            {
                let mut changed = frame.clone();
                changed[index] = replacement;
                assert!(check_stop_frame(&changed, &[1, 2]).is_err());
            }
            let mut extra = frame.clone();
            extra.extend_from_slice(&frame);
            assert!(check_stop_frame(&extra, &[1, 2]).is_err());
        }

        fn recorded_stop(sample_id: usize, start: usize) -> (OperationSample, IoSample) {
            let frame = stop_frame(&[(1, 0), (2, 0)]);
            let mut raw = [0; FRAME_CAPACITY];
            raw[..frame.len()].copy_from_slice(&frame);
            let context = CallContext {
                sample_id,
                motor_id: Some(0xfe),
                io_index: 1,
                transaction: 1,
            };
            (
                OperationSample {
                    sample_id,
                    motor_id: Some(0xfe),
                    started_ns: 100,
                    finished_ns: 300,
                    record_start: start,
                    record_end: start + 1,
                    model_number: None,
                    error: None,
                },
                IoSample {
                    context: Some(context),
                    kind: IoKind::Write,
                    started_ns: 120,
                    finished_ns: 280,
                    requested_bytes: frame.len(),
                    success: true,
                    raw,
                    raw_len: frame.len(),
                },
            )
        }

        #[test]
        fn safety_call_oracle_rejects_extra_read_and_extra_write() {
            let (mut operation, record) = recorded_stop(0, 0);
            assert!(
                check_operation(
                    Phase::OrinBroadcast,
                    &operation,
                    std::slice::from_ref(&record),
                    &[1, 2]
                )
                .is_ok()
            );
            operation.finished_ns = 600;
            operation.record_end = 2;
            for kind in [IoKind::Read, IoKind::Write, IoKind::Drain] {
                let mut extra = record.clone();
                extra.kind = kind;
                extra.started_ns = 400;
                extra.finished_ns = 500;
                let context = extra.context.as_mut().unwrap();
                context.io_index = 2;
                context.transaction = if kind == IoKind::Write { 2 } else { 1 };
                assert!(
                    check_operation(
                        Phase::OrinBroadcast,
                        &operation,
                        &[record.clone(), extra],
                        &[1, 2]
                    )
                    .is_err()
                );
            }
        }

        #[test]
        fn warmup_failure_and_unassigned_tx_cannot_be_hidden_by_formal_statistics() {
            let mut recorder = IoRecorder::new();
            let mut operations = Vec::new();
            for index in 0..WARMUP + SAMPLES {
                let (operation, record) = recorded_stop(index, index);
                recorder.records.push(record);
                operations.push(operation);
            }
            let (good, passed) = summarize(Phase::OrinBroadcast, &recorder, &operations, &[1, 2]);
            assert!(passed);
            assert_eq!(good["operation_formal"]["count"], SAMPLES);
            assert_eq!(
                good["transactions_formal"]["1_torque_disable_broadcast"]["write_all"]["count"],
                SAMPLES
            );
            operations[0].error = Some("预热期间真实调用失败".to_owned());
            assert!(!summarize(Phase::OrinBroadcast, &recorder, &operations, &[1, 2]).1);
            operations[0].error = None;
            let (_, mut unassigned) = recorded_stop(0, 0);
            unassigned.context = None;
            recorder.records.push(unassigned);
            let (bad, passed) = summarize(Phase::OrinBroadcast, &recorder, &operations, &[1, 2]);
            assert!(!passed);
            assert_eq!(bad["wire_audit"]["unexpected_setup_io"], true);
        }

        #[test]
        fn enable_command_count_comes_from_tx_and_unknown_frames_remain_unknown() {
            let mut recorder = IoRecorder::new();
            let (_, mut record) = recorded_stop(0, 0);
            let frame = stop_frame(&[(1, 0), (2, 1)]);
            record.raw[..frame.len()].copy_from_slice(&frame);
            recorder.records.push(record.clone());
            assert_eq!(observed_enable_commands(&recorder), Some(1));
            record.raw[0] = 0;
            recorder.records.push(record);
            assert_eq!(observed_enable_commands(&recorder), None);
        }

        #[test]
        fn recorder_overflow_and_oversize_frame_invalidate_evidence_without_reallocation() {
            let mut recorder = IoRecorder::new();
            let capacity = recorder.records.capacity();
            let (_, record) = recorded_stop(0, 0);
            for _ in 0..capacity {
                recorder.record(record.clone());
            }
            recorder.record(record.clone());
            assert_eq!(recorder.records.len(), capacity);
            assert_eq!(recorder.records.capacity(), capacity);
            assert_eq!(recorder.dropped, 1);
            let mut oversized = record;
            oversized.requested_bytes = FRAME_CAPACITY + 1;
            recorder.record(oversized);
            assert_eq!(recorder.truncated, 1);
        }

        #[test]
        fn usb_identity_requires_exact_vid_pid_driver_and_unique_properties() {
            let valid = "ID_VENDOR_ID=1a86\nID_MODEL_ID=55d3\nID_USB_DRIVER=cdc_acm\n";
            assert!(check_usb_identity(valid).is_ok());
            for bad in [
                valid.replace("1a86", "1a87"),
                valid.replace("55d3", "55d4"),
                valid.replace("cdc_acm", "ch341"),
                format!("{valid}ID_USB_DRIVER=cdc_acm\n"),
            ] {
                assert!(check_usb_identity(&bad).is_err());
            }
        }

        #[test]
        fn measured_transport_keeps_two_ping_subtransactions_and_warmup_context() {
            use robot_adapter_feetech::transport::MockTransport;
            let (inner, handle) = MockTransport::new();
            let recorder = Arc::new(Mutex::new(IoRecorder::new()));
            let mut transport = MeasuredTransport {
                inner,
                samples: recorder.clone(),
            };
            recorder.lock().unwrap().begin(2, Some(1));
            for length in [6, 8] {
                transport.write_all(&vec![0xaa; length]).unwrap();
                handle.push_rx(&vec![0xbb; length]);
                transport.read_exact(&mut vec![0; length]).unwrap();
            }
            let records = recorder.lock().unwrap();
            assert_eq!(records.records.len(), 4);
            for (index, record) in records.records.iter().enumerate() {
                let context = record.context.unwrap();
                assert_eq!(context.sample_id, 2);
                assert_eq!(context.motor_id, Some(1));
                assert_eq!(context.transaction, index / 2 + 1);
                assert_eq!(context.io_index, index + 1);
                assert_eq!(record.raw_len, if index < 2 { 6 } else { 8 });
                assert!(record.finished_ns >= record.started_ns);
            }
        }
    }
}

pub(super) fn software_protocol_probe() -> Result<serde_json::Value, String> {
    physical::software_protocol_probe()
}

pub(super) fn run(args: &Args, report: &mut Report) -> Result<(), String> {
    #[cfg(feature = "real-hw")]
    {
        use robot_core::time::{RobotClock, SystemClock};
        let (label, ac) = match args.phase {
            Phase::OrinCharacterize => ("orin_characterize", "AC-2/H1"),
            Phase::OrinBroadcast => ("orin_broadcast", "AC-13/H3"),
            _ => return Err("现场 SLA 入口等待真实运行时采样接线".to_owned()),
        };
        let started = SystemClock.now().0;
        report.record(label, ac, started, physical::run(args));
        Ok(())
    }
    #[cfg(not(feature = "real-hw"))]
    {
        let _ = (args, report);
        Err("现场入口要求 real-hw 构建".to_owned())
    }
}
