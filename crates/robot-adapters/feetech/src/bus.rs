//! `FeetechBus` —— 一条 Feetech 串口总线的高层 API。
//!
//! 对应 dimos `feetech_io/bus.py` 全 386 行；行为与 `06_lerobot_replace_spec.md`
//! §3-§6 字面对齐。
//!
//! ## 公开 API
//!
//! - 生命周期：[`new`](FeetechBus::new) / [`connect`](FeetechBus::connect) /
//!   [`disconnect`](FeetechBus::disconnect) / [`is_connected`](FeetechBus::is_connected)
//! - ping：[`ping`](FeetechBus::ping)（返回 model_number 或 None）
//! - 单 motor 单寄存器：[`read`](FeetechBus::read) / [`write`](FeetechBus::write)
//! - 批量同寄存器：[`sync_read`](FeetechBus::sync_read) / [`sync_write`](FeetechBus::sync_write)
//!
//! ## sign-magnitude 透明
//!
//! 既有写入和解码读取接口使用 *带符号* 整数（i32），sign-magnitude 编解码由本
//! 模块内部按 [`STS3215_SIGN_BIT`](crate::registers::STS3215_SIGN_BIT) 表透明处理。
//! [`sync_read_by_id_raw_into`](FeetechBus::sync_read_by_id_raw_into) 单独保留无符号原字。
//!
//! ## 同步阻塞
//!
//! 所有 read/write 在调用线程内完成 wire RTT（write_all + read_exact）；
//! `FeetechAdapter::read_state` / `write_command` 实测延迟在 ms 量级，**不能**
//! 直接挂 `s0_rt_thread`（违反 §4.9 RT 铁律）。task 28 真机阶段实测后决定是否
//! 引入"adapter 内部 IO 线程 + atomic snapshot"双层架构。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use robot_core::diag::DiagSink;
use robot_core::serial_services::{BusLease, BusOpClass, LeasedPreemptProbe};

use crate::encoding::{
    combine_bytes_le, decode_sign_magnitude, encode_sign_magnitude, split_bytes_le,
};
use crate::error::{AbsoluteWriteFailure, AbsoluteWriteReason, FeetechError};
use crate::protocol::{
    StatusPacket, build_ping, build_read, build_sync_read, build_sync_write, build_write,
    parse_status,
};
use crate::registers::{
    MODEL_NUMBER_STS3215, ensure_not_diagnostic_write, ensure_not_firmware_readonly_write,
    get_register, sign_bit_for,
};
use crate::transport::{DrainReport, SerialTransport, SourceWriteDeadline};
use crate::types::Motor;

/// 一条 Feetech bus。`T` 是 [`SerialTransport`] 实现（`MockTransport` /
/// `serialport::SerialPort` 包装）。
pub struct FeetechBus<T: SerialTransport> {
    transport: T,
    /// motor 名 → `Motor`（含 id）。命令式 API 用名字，wire 层用 id。
    motors: HashMap<String, Motor>,
    /// motor id → 名（反向 lookup，用于 sync_read 解析）。
    name_by_id: HashMap<u8, String>,
    /// 当前 read_exact 超时配置；`set_timeout` 切换时同步给 transport。
    timeout: Duration,
    /// 构造期固定的确定性握手顺序，不在串口线程构造临时集合。
    motor_ids: Vec<u8>,
    preempt: Option<Arc<dyn LeasedPreemptProbe>>,
    op_class: BusOpClass,
    /// 仅为刚返回的 `StatusError` 保存已验 ID 的响应来源，不参与安全决策。
    device_status_motor: u8,
}

/// 一个完整 request + response 持有一次执行权；所有失败和解析退出由 Drop 归还。
struct BusTransactionLease<'a> {
    probe: Option<&'a dyn LeasedPreemptProbe>,
    lease: Option<BusLease>,
}

impl<'a> BusTransactionLease<'a> {
    fn acquire(
        probe: Option<&'a dyn LeasedPreemptProbe>,
        class: BusOpClass,
    ) -> Result<Self, FeetechError> {
        let lease = match probe {
            Some(probe) => Some(probe.begin_leased(class).map_err(FeetechError::Aborted)?),
            None => None,
        };
        Ok(Self { probe, lease })
    }

    fn write<T: SerialTransport>(&self, transport: &mut T, buf: &[u8]) -> Result<(), FeetechError> {
        let result = transport.write_all(buf);
        if let Some(probe) = self.probe {
            probe.checkpoint();
        }
        result
    }

    fn write_until<T: SerialTransport>(
        &self,
        transport: &mut T,
        buf: &[u8],
        deadline: SourceWriteDeadline<'_>,
    ) -> Result<(), FeetechError> {
        let result = transport.write_all_until(buf, deadline);
        if let Some(probe) = self.probe {
            probe.checkpoint();
        }
        result
    }

    fn read<T: SerialTransport>(
        &self,
        transport: &mut T,
        buf: &mut [u8],
    ) -> Result<(), FeetechError> {
        let result = transport.read_exact(buf);
        if let Some(probe) = self.probe {
            probe.checkpoint();
        }
        result
    }
}

impl Drop for BusTransactionLease<'_> {
    fn drop(&mut self) {
        if let (Some(probe), Some(lease)) = (self.probe, self.lease.take()) {
            probe.finish_leased(lease);
        }
    }
}

impl<T: SerialTransport> FeetechBus<T> {
    /// 用预先打开（或 mock）的 transport 构造 bus。**不**调用 `connect`。
    ///
    /// # Errors
    /// motor 列表里 id 重复 → [`FeetechError::DuplicateMotorId`]。
    pub fn new(transport: T, motors: HashMap<String, Motor>) -> Result<Self, FeetechError> {
        let mut name_by_id = HashMap::with_capacity(motors.len());
        for (name, motor) in &motors {
            if name_by_id.insert(motor.id, name.clone()).is_some() {
                return Err(FeetechError::DuplicateMotorId(motor.id));
            }
        }
        let mut motor_ids: Vec<_> = name_by_id.keys().copied().collect();
        motor_ids.sort_unstable();
        Ok(Self {
            transport,
            motors,
            name_by_id,
            timeout: Duration::from_millis(50),
            motor_ids,
            preempt: None,
            op_class: BusOpClass::NormalEffectful,
            device_status_motor: 0,
        })
    }

    /// 构造期服务安装。诊断由 adapter 使用同源时钟记录，bus 不另建时间源。
    pub fn set_services(
        &mut self,
        preempt: Option<Arc<dyn LeasedPreemptProbe>>,
        _diag: Option<Arc<dyn DiagSink>>,
    ) {
        self.preempt = preempt;
    }

    pub fn set_op_class(&mut self, class: BusOpClass) {
        self.op_class = class;
    }

    /// 只在本次总线调用返回 `StatusError` 时读取；其他错误不消费旧上下文。
    pub(crate) fn device_status_motor(&self) -> u8 {
        self.device_status_motor
    }

    /// hygiene 属于 Confirm 自身，不取得普通事务执行权。
    pub fn confirm_hygiene(&mut self) -> Result<DrainReport, FeetechError> {
        let discard = self.transport.discard_input();
        if let Some(probe) = self.preempt.as_ref() {
            probe.checkpoint();
        }
        discard?;
        let drain = self
            .transport
            .drain_until_quiet(Duration::from_millis(50), Duration::from_millis(150));
        if let Some(probe) = self.preempt.as_ref() {
            probe.checkpoint();
        }
        drain
    }

    /// 是否已连接。
    pub fn is_connected(&self) -> bool {
        self.transport.is_connected()
    }

    /// 设置 transport 超时（影响 `read` / `sync_read` 的等包时间）。
    ///
    /// # Errors
    /// 透传 [`SerialTransport::set_timeout`]。
    pub fn set_timeout(&mut self, timeout: Duration) -> Result<(), FeetechError> {
        self.transport.set_timeout(timeout)?;
        self.timeout = timeout;
        Ok(())
    }

    // ── connect / disconnect ──────────────────────────────────────────

    /// 应用默认 timeout 到 transport，然后选择性 handshake（ping 每个 motor）。
    ///
    /// `handshake = true` 时检查每个 motor 的 model_number == 777。
    ///
    /// # Errors
    ///
    /// - transport 已断开 → [`FeetechError::NotConnected`]
    /// - 某 motor ping 不到或型号不对 → 类型化握手错误。
    pub fn connect(&mut self, handshake: bool) -> Result<(), FeetechError> {
        if !self.transport.is_connected() {
            return Err(FeetechError::NotConnected);
        }
        self.transport.set_timeout(self.timeout)?;
        if handshake {
            self.handshake()?;
        }
        Ok(())
    }

    /// 关闭 bus（不真断 transport，调用方自己释放 transport 资源）。
    pub fn disconnect(&mut self) {
        // v0.2 同步模式：不主动 close transport（mock 没 close；真 SerialPort
        // 由 owner Drop 关）。task 28 真硬件阶段评估是否要 explicit shutdown。
    }

    fn handshake(&mut self) -> Result<(), FeetechError> {
        for index in 0..self.motor_ids.len() {
            let id = self.motor_ids[index];
            match self.ping(id)? {
                None => return Err(FeetechError::HandshakeMissing { id }),
                Some(got) if got != MODEL_NUMBER_STS3215 => {
                    return Err(FeetechError::HandshakeWrongModel { id, got });
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    // ── ping ──────────────────────────────────────────────────────────

    /// `ping` motor + 读 Model_Number。返回 `None` 表示无响应。
    ///
    /// 内部实现对应 dimos `bus.py:ping` —— scservo_sdk 的 ping 实际上做了
    /// "PING + 隐式 READ_DATA(addr=3, len=2) Model_Number"。本 crate 拆成
    /// 显式两步：① `build_ping` + `parse_status` 验 motor 在线；② `build_read`
    /// `Model_Number` 拿到具体型号。
    ///
    /// # Errors
    /// 透传 transport / 协议错误（**不**包括"motor 没回 = ping 失败"，那个返回
    /// `Ok(None)`）。
    pub fn ping(&mut self, motor_id: u8) -> Result<Option<u16>, FeetechError> {
        // ① PING
        let mut tx = [0u8; 6];
        let n = build_ping(motor_id, &mut tx)?;
        {
            let lease = BusTransactionLease::acquire(self.preempt.as_deref(), self.op_class)?;
            lease.write(&mut self.transport, &tx[..n])?;
            let mut rx = [0u8; 6];
            match lease.read(&mut self.transport, &mut rx) {
                Ok(()) => {
                    let pkt = parse_status(&rx)?;
                    validate_response(&pkt, motor_id, &mut self.device_status_motor)?;
                }
                Err(FeetechError::TransportTimeout { .. }) => return Ok(None),
                Err(e) => return Err(e),
            }
        }

        // ② READ_DATA Model_Number (addr=3, len=2)
        let mut tx2 = [0u8; 8];
        let n2 = build_read(motor_id, 3, 2, &mut tx2)?;
        let lease = BusTransactionLease::acquire(self.preempt.as_deref(), self.op_class)?;
        lease.write(&mut self.transport, &tx2[..n2])?;
        let mut rx2 = [0u8; 8]; // FF FF ID LEN ERR P0 P1 CHK = 8
        match lease.read(&mut self.transport, &mut rx2) {
            Ok(()) => {
                let pkt = parse_status(&rx2)?;
                validate_response(&pkt, motor_id, &mut self.device_status_motor)?;
                if pkt.params.len() != 2 {
                    return Err(FeetechError::ByteLengthMismatch {
                        expected: 2,
                        actual: pkt.params.len(),
                    });
                }
                let model = u16::try_from(combine_bytes_le(pkt.params, 2)?).unwrap_or(0);
                Ok(Some(model))
            }
            Err(FeetechError::TransportTimeout { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    // ── 单 motor read / write ─────────────────────────────────────────

    /// 读单 motor 单寄存器，返回 *带符号* 解码值。
    ///
    /// # Errors
    /// 透传协议 / transport / 编码错误。
    pub fn read(&mut self, reg_name: &'static str, motor_name: &str) -> Result<i32, FeetechError> {
        let reg = get_register(reg_name)?;
        let id = self.motor_id(motor_name)?;

        let mut tx = [0u8; 8];
        let n = build_read(id, reg.addr, reg.length, &mut tx)?;
        let lease = BusTransactionLease::acquire(self.preempt.as_deref(), self.op_class)?;
        lease.write(&mut self.transport, &tx[..n])?;

        let total = 6 + usize::from(reg.length); // FF FF ID LEN ERR <data..> CHK
        let mut rx = vec![0u8; total];
        lease.read(&mut self.transport, &mut rx)?;
        let pkt = parse_status(&rx)?;
        validate_response(&pkt, id, &mut self.device_status_motor)?;
        if pkt.params.len() != usize::from(reg.length) {
            return Err(FeetechError::ByteLengthMismatch {
                expected: usize::from(reg.length),
                actual: pkt.params.len(),
            });
        }
        let raw = combine_bytes_le(pkt.params, usize::from(reg.length))?;
        Ok(decode_value(reg_name, raw))
    }

    /// 写单 motor 单寄存器（等 ACK）。
    ///
    /// # Errors
    /// 透传协议 / transport / 编码错误，或 status `ERROR != 0` →
    /// [`FeetechError::StatusError`]。
    pub fn write(
        &mut self,
        reg_name: &'static str,
        motor_name: &str,
        value: i32,
    ) -> Result<(), FeetechError> {
        ensure_not_diagnostic_write(reg_name)?;
        ensure_not_firmware_readonly_write(reg_name)?;
        let reg = get_register(reg_name)?;
        let id = self.motor_id(motor_name)?;
        let encoded = encode_value(reg_name, value)?;
        let mut data_buf = [0u8; 4];
        let length_usize = usize::from(reg.length);
        split_bytes_le(encoded, length_usize, &mut data_buf[..length_usize])?;

        let mut tx = [0u8; 6 + 1 + 4]; // ping(6) + ADDR(1) + max DATA(4)
        let n = build_write(id, reg.addr, &data_buf[..length_usize], &mut tx)?;
        let lease = BusTransactionLease::acquire(self.preempt.as_deref(), self.op_class)?;
        lease.write(&mut self.transport, &tx[..n])?;

        // ACK 是一个空 payload status：FF FF ID 02 ERR CHK = 6 字节
        let mut rx = [0u8; 6];
        lease.read(&mut self.transport, &mut rx)?;
        let pkt = parse_status(&rx)?;
        validate_response(&pkt, id, &mut self.device_status_motor)?;
        Ok(())
    }

    // ── 批量 sync_read / sync_write ───────────────────────────────────

    /// 批量读同寄存器。返回 `motor_name → value`。
    ///
    /// # Errors
    /// 透传协议 / transport 错误；任一 motor 回包 `ERROR != 0` →
    /// [`FeetechError::StatusError`]。
    pub fn sync_read(
        &mut self,
        reg_name: &'static str,
        motor_names: &[&str],
    ) -> Result<HashMap<String, i32>, FeetechError> {
        let reg = get_register(reg_name)?;
        let ids: Vec<u8> = motor_names
            .iter()
            .map(|n| self.motor_id(n))
            .collect::<Result<_, _>>()?;

        // 发请求（一个包到 BROADCAST_ID）
        let mut tx = vec![0u8; 6 + 2 + ids.len()];
        let n = build_sync_read(reg.addr, reg.length, &ids, &mut tx)?;
        let lease = BusTransactionLease::acquire(self.preempt.as_deref(), self.op_class)?;
        lease.write(&mut self.transport, &tx[..n])?;

        // 每个 motor 回 1 个 status，按请求顺序
        let per_status_len = 6 + usize::from(reg.length);
        let total = per_status_len * ids.len();
        let mut rx = vec![0u8; total];
        lease.read(&mut self.transport, &mut rx)?;

        let mut result = HashMap::with_capacity(ids.len());
        let mut cursor = 0usize;
        for (idx, id) in ids.iter().enumerate() {
            let pkt: StatusPacket<'_> = parse_status(&rx[cursor..cursor + per_status_len])?;
            validate_response(&pkt, *id, &mut self.device_status_motor)?;
            let raw = combine_bytes_le(pkt.params, usize::from(reg.length))?;
            let name = motor_names[idx].to_string();
            result.insert(name, decode_value(reg_name, raw));
            cursor += pkt.consumed;
        }
        Ok(result)
    }

    /// 批量写同寄存器（**不**等 ACK，丢包不会报错）。critical 操作（Torque_Enable
    /// / Operating_Mode）应用 [`write`](FeetechBus::write)。
    ///
    /// # Errors
    /// 透传协议 / transport / 编码错误。
    pub fn sync_write(
        &mut self,
        reg_name: &'static str,
        values: &[(&str, i32)],
    ) -> Result<(), FeetechError> {
        ensure_not_diagnostic_write(reg_name)?;
        ensure_not_firmware_readonly_write(reg_name)?;
        let reg = get_register(reg_name)?;
        let length_usize = usize::from(reg.length);

        // 编码每个 motor 的字节
        let mut encoded_data: Vec<(u8, [u8; 4])> = Vec::with_capacity(values.len());
        for (name, value) in values {
            let id = self.motor_id(name)?;
            let encoded = encode_value(reg_name, *value)?;
            let mut buf = [0u8; 4];
            split_bytes_le(encoded, length_usize, &mut buf[..length_usize])?;
            encoded_data.push((id, buf));
        }
        let entries: Vec<(u8, &[u8])> = encoded_data
            .iter()
            .map(|(id, data)| (*id, &data[..length_usize]))
            .collect();

        let total_size = 6 + 2 + entries.len() * (1 + length_usize);
        let mut tx = vec![0u8; total_size];
        let n = build_sync_write(reg.addr, reg.length, &entries, &mut tx)?;
        let lease = BusTransactionLease::acquire(self.preempt.as_deref(), self.op_class)?;
        lease.write(&mut self.transport, &tx[..n])
    }

    // ── by-id hot-path API（Step 7 task 27c follow-up：消除 HashMap String compare）──
    //
    // 火焰图（docs/perf/2026-04-24_task27c_feetech_baseline）显示 `sync_write`
    // 的 hot frame 之一是 `__memcmp_evex_movbe` —— 来自 `motor_id(name)` 查
    // `HashMap<String, Motor>` 时的 hash + string compare。`FeetechAdapter`
    // 在 `connect()` 一次性把 `joint_names → joint_ids: Vec<u8>` 缓存后，
    // hot path 调下面这两个 by-id API，**完全跳过 HashMap lookup**，零
    // string compare。
    //
    // 现有 by-name `sync_read` / `sync_write` API 保留，给 calibration / 离线
    // 工具用（这些路径不在 hot path）。

    /// by-id 版 [`sync_read`](FeetechBus::sync_read)：结果按 `ids` 顺序写入
    /// `out`，避免 HashMap 分配 + String hash 计算。
    ///
    /// `out.len()` 必须 ≥ `ids.len()`（多余位置不写）。
    ///
    /// # Errors
    ///
    /// - `out.len() < ids.len()` → [`FeetechError::ByteLengthMismatch`]
    /// - 任一回包 id ≠ 期望 id → [`FeetechError::ResponseIdMismatch`]
    /// - 任一 status `ERROR != 0` → [`FeetechError::StatusError`]
    /// - 透传 transport / 编码错误
    pub fn sync_read_by_id_into(
        &mut self,
        reg_name: &'static str,
        ids: &[u8],
        out: &mut [i32],
    ) -> Result<(), FeetechError> {
        if out.len() < ids.len() {
            return Err(FeetechError::ByteLengthMismatch {
                expected: ids.len(),
                actual: out.len(),
            });
        }
        let reg = get_register(reg_name)?;

        // 发请求
        let mut tx = vec![0u8; 6 + 2 + ids.len()];
        let n = build_sync_read(reg.addr, reg.length, ids, &mut tx)?;
        let lease = BusTransactionLease::acquire(self.preempt.as_deref(), self.op_class)?;
        lease.write(&mut self.transport, &tx[..n])?;

        // 回包
        let per_status_len = 6 + usize::from(reg.length);
        let total = per_status_len * ids.len();
        let mut rx = vec![0u8; total];
        lease.read(&mut self.transport, &mut rx)?;

        let mut cursor = 0usize;
        for (idx, &id) in ids.iter().enumerate() {
            let pkt: StatusPacket<'_> = parse_status(&rx[cursor..cursor + per_status_len])?;
            validate_response(&pkt, id, &mut self.device_status_motor)?;
            let raw = combine_bytes_le(pkt.params, usize::from(reg.length))?;
            out[idx] = decode_value(reg_name, raw);
            cursor += pkt.consumed;
        }
        Ok(())
    }

    /// 固定缓冲批量读取原始无符号字，不进行 sign-magnitude 解码。
    ///
    /// IDs 必须非空、唯一且在 1..=253 内，最多 MAX_JOINTS 项。所有容量
    /// 在租约和 IO 之前校验；整组回包合法后才提交 out，尾部槽位保持原值。
    /// 一个租约覆盖请求及整组响应；仅沿用 transport 的调用预算，不赋予
    /// 来源绝对读取期限、USB 身份证明或新的权限。
    ///
    /// # Errors
    /// ID、数量、容量或寄存器宽度非法时返回原参数错误；回包非法或 IO
    /// 失败时透传原协议/通信错误，out 全部保持原值。
    pub fn sync_read_by_id_raw_into(
        &mut self,
        reg_name: &'static str,
        ids: &[u8],
        out: &mut [u32],
        tx_buf: &mut [u8],
        rx_buf: &mut [u8],
    ) -> Result<(), FeetechError> {
        let reg = get_register(reg_name)?;
        self.sync_read_raw_register_into(reg, ids, out, tx_buf, rx_buf)
    }

    fn sync_read_raw_register_into(
        &mut self,
        reg: crate::registers::Register,
        ids: &[u8],
        out: &mut [u32],
        tx_buf: &mut [u8],
        rx_buf: &mut [u8],
    ) -> Result<(), FeetechError> {
        let n = ids.len();
        if n == 0 || n > robot_core::adapter::MAX_JOINTS {
            return Err(FeetechError::ByteLengthMismatch {
                expected: if n == 0 {
                    1
                } else {
                    robot_core::adapter::MAX_JOINTS
                },
                actual: n,
            });
        }
        for (index, &id) in ids.iter().enumerate() {
            if !(1..=253).contains(&id) {
                return Err(FeetechError::InvalidMotorId(i32::from(id)));
            }
            if ids[..index].contains(&id) {
                return Err(FeetechError::DuplicateMotorId(id));
            }
        }
        let width = usize::from(reg.length);
        if !matches!(width, 1 | 2 | 4) {
            return Err(FeetechError::UnsupportedByteLength(width));
        }
        let request_len = 8 + n;
        let per_status_len = 6 + width;
        let response_len = n * per_status_len;
        for (expected, actual) in [
            (n, out.len()),
            (request_len, tx_buf.len()),
            (response_len, rx_buf.len()),
        ] {
            if actual < expected {
                return Err(FeetechError::ByteLengthMismatch { expected, actual });
            }
        }

        let written = build_sync_read(reg.addr, reg.length, ids, tx_buf)?;
        let lease = BusTransactionLease::acquire(self.preempt.as_deref(), self.op_class)?;
        lease.write(&mut self.transport, &tx_buf[..written])?;
        lease.read(&mut self.transport, &mut rx_buf[..response_len])?;

        let mut staged = [0_u32; robot_core::adapter::MAX_JOINTS];
        for (index, &id) in ids.iter().enumerate() {
            let start = index * per_status_len;
            let packet = parse_status(&rx_buf[start..start + per_status_len])?;
            validate_response(&packet, id, &mut self.device_status_motor)?;
            staged[index] = combine_bytes_le(packet.params, width)?;
        }
        out[..n].copy_from_slice(&staged[..n]);
        Ok(())
    }

    /// by-id 版 [`sync_write`](FeetechBus::sync_write)：直接接 `&[(id, value)]`，
    /// 跳过 HashMap lookup。**不**等 ACK。
    ///
    /// # Errors
    /// 透传协议 / transport / 编码错误。
    pub fn sync_write_by_id(
        &mut self,
        reg_name: &'static str,
        entries: &[(u8, i32)],
    ) -> Result<(), FeetechError> {
        ensure_not_diagnostic_write(reg_name)?;
        ensure_not_firmware_readonly_write(reg_name)?;
        let reg = get_register(reg_name)?;
        let length_usize = usize::from(reg.length);

        let mut encoded_data: Vec<(u8, [u8; 4])> = Vec::with_capacity(entries.len());
        for (id, value) in entries {
            let encoded = encode_value(reg_name, *value)?;
            let mut buf = [0u8; 4];
            split_bytes_le(encoded, length_usize, &mut buf[..length_usize])?;
            encoded_data.push((*id, buf));
        }
        let frame_entries: Vec<(u8, &[u8])> = encoded_data
            .iter()
            .map(|(id, data)| (*id, &data[..length_usize]))
            .collect();

        let total_size = 6 + 2 + frame_entries.len() * (1 + length_usize);
        let mut tx = vec![0u8; total_size];
        let n = build_sync_write(reg.addr, reg.length, &frame_entries, &mut tx)?;
        let lease = BusTransactionLease::acquire(self.preempt.as_deref(), self.op_class)?;
        lease.write(&mut self.transport, &tx[..n])
    }

    /// **task-30 v9 / M4**：`sync_write_by_id` 的预分配版本。`safety_stop_fast`
    /// 路径调本方法 → wire bytes 写入调用方传入的 `frame_buf`，**全程零堆 alloc**
    /// （`assert_no_alloc::assert_no_alloc(...)` 包裹下不 panic）。
    ///
    /// `frame_buf` 必须 `>= MAX_FRAME_LEN` byte。MAX_FRAME_LEN 公式：
    ///   = 6 (FF FF ID LEN INST CHKSUM) + max_params
    ///   max_params = 2 (addr + length) + N × (1 + length_byte)
    ///   N=MAX_JOINTS=32, length=4 → max_params = 162 → MAX_FRAME_LEN = 168
    /// 调用方推荐预分配 `[0u8; 256]` 留余量。
    ///
    /// 实现走两阶段 borrow（v9 §0 #4 修订）：
    /// - 阶段 1：mut 写 `data_bufs[..n]`，每个 i 独立无借用冲突；
    /// - 阶段 1 结束 mut borrow drop；
    /// - 阶段 2：shared 构造 `entry_refs[..n]`（&data_bufs[i][..]）—— 借用检查通过。
    ///
    /// # Errors
    ///
    /// - `entries.len() > MAX_JOINTS` → [`FeetechError::ByteLengthMismatch`]
    ///   `{ expected: MAX_JOINTS, actual: entries.len() }`（本函数顶层校验）。
    /// - `frame_buf.len() < total_size` →
    ///   [`FeetechError::ByteLengthMismatch`]`{ expected: total_size, actual:
    ///   frame_buf.len() }`（由 `build_instruction` 抛出）。
    ///   注：**不是** `BadPacketLength`；后者只在协议层 `params.len() >
    ///   MAX_PARAMS` 时出现。`sync_write_by_id_into` 内部已校验 `MAX_JOINTS`
    ///   足够小不会触发协议层 `BadPacketLength`，所以 frame_buf 不足只走
    ///   `ByteLengthMismatch`。
    /// - 透传协议 / transport / 编码错误。
    pub fn sync_write_by_id_into(
        &mut self,
        reg_name: &'static str,
        entries: &[(u8, i32)],
        frame_buf: &mut [u8],
    ) -> Result<(), FeetechError> {
        let n_bytes = prepare_sync_write_frame(reg_name, entries, frame_buf)?;
        let lease = BusTransactionLease::acquire(self.preempt.as_deref(), self.op_class)?;
        lease.write(&mut self.transport, &frame_buf[..n_bytes])
    }

    /// 显式来源期限的普通 broadcast 写入；必须显式提供探针，真实性由调用方保证。
    ///
    /// 不读取 ACK，不授权 Enable，也不自动接 Position/Safety/Confirm 或运行时。
    /// # Errors
    /// 保留原寄存器/编码/帧容量错误优先级；类别、缺探针、过期前置不申请lease。
    /// 透传原 Aborted 或固定绝对期限错误；传输未知实现不得回退旧 write_all。
    pub fn sync_write_by_id_into_until(
        &mut self,
        reg_name: &'static str,
        entries: &[(u8, i32)],
        frame_buf: &mut [u8],
        deadline: SourceWriteDeadline<'_>,
    ) -> Result<(), FeetechError> {
        let n_bytes = prepare_sync_write_frame(reg_name, entries, frame_buf)?;
        match self.op_class {
            BusOpClass::NormalEffectful | BusOpClass::PeriodicWrite => {}
            class => {
                return Err(FeetechError::AbsoluteWriteFailure(
                    AbsoluteWriteFailure::before_write(
                        AbsoluteWriteReason::InvalidOperationClass { class },
                        n_bytes,
                        deadline.source_deadline(),
                        None,
                    ),
                ));
            }
        }
        let Some(probe) = self.preempt.as_deref() else {
            return Err(FeetechError::AbsoluteWriteFailure(
                AbsoluteWriteFailure::before_write(
                    AbsoluteWriteReason::NoAuthorityProbe,
                    n_bytes,
                    deadline.source_deadline(),
                    None,
                ),
            ));
        };
        let now = deadline.clock().now();
        if now >= deadline.source_deadline().as_mono_raw_ns() {
            return Err(FeetechError::AbsoluteWriteFailure(
                AbsoluteWriteFailure::before_write(
                    AbsoluteWriteReason::SourceExpired,
                    n_bytes,
                    deadline.source_deadline(),
                    Some(now),
                ),
            ));
        }
        let lease = BusTransactionLease::acquire(Some(probe), self.op_class)?;
        lease.write_until(&mut self.transport, &frame_buf[..n_bytes], deadline)
    }

    // ── 工具 ──────────────────────────────────────────────────────────

    /// `motor_name` → `motor_id`，未知名 → [`FeetechError::UnknownMotor`]。
    ///
    /// # Errors
    /// 名字不在 motors 表 → [`FeetechError::UnknownMotor`]。
    pub fn motor_id(&self, motor_name: &str) -> Result<u8, FeetechError> {
        self.motors
            .get(motor_name)
            .map(|m| m.id)
            .ok_or_else(|| FeetechError::UnknownMotor(motor_name.to_string()))
    }

    /// `motor_id` → `motor_name`，未知 id → `None`。
    #[must_use]
    pub fn motor_name(&self, motor_id: u8) -> Option<&str> {
        self.name_by_id.get(&motor_id).map(String::as_str)
    }

    /// 全部 motor 名字（按 HashMap 内部顺序，不稳定；调用方需要稳定顺序自行 sort）。
    #[must_use]
    pub fn all_motor_names(&self) -> Vec<String> {
        self.motors.keys().cloned().collect()
    }
}

/// 原 `_into` 的纯固定栈编码准备；新旧入口共用原拒写和参数检查顺序。
fn prepare_sync_write_frame(
    reg_name: &'static str,
    entries: &[(u8, i32)],
    frame_buf: &mut [u8],
) -> Result<usize, FeetechError> {
    ensure_not_diagnostic_write(reg_name)?;
    ensure_not_firmware_readonly_write(reg_name)?;
    let reg = get_register(reg_name)?;
    let length_usize = usize::from(reg.length); // v9 fix：字段名 length
    let n = entries.len();
    if n > robot_core::adapter::MAX_JOINTS {
        return Err(FeetechError::ByteLengthMismatch {
            expected: robot_core::adapter::MAX_JOINTS,
            actual: n,
        });
    }

    // 栈数组（无 alloc）。`MAX_JOINTS = 32` 恒小，单帧最坏 32×4 = 128 byte。
    let mut data_bufs: [[u8; 4]; robot_core::adapter::MAX_JOINTS] =
        [[0; 4]; robot_core::adapter::MAX_JOINTS];

    // ===== 阶段 1：mut 写 data_bufs[..n]（每个 i 独立无借用冲突）=====
    for (i, (_id, value)) in entries.iter().enumerate() {
        let encoded = encode_value(reg_name, *value)?;
        split_bytes_le(encoded, length_usize, &mut data_bufs[i][..length_usize])?;
    }
    // 阶段 1 结束 —— mut borrow drop

    // ===== 阶段 2：shared 构造 entry_refs[..n]（data_bufs 不再 mut，借用 OK） =====
    let mut entry_refs: [(u8, &[u8]); robot_core::adapter::MAX_JOINTS] =
        [(0u8, &[] as &[u8]); robot_core::adapter::MAX_JOINTS];
    for (i, (id, _)) in entries.iter().enumerate() {
        entry_refs[i] = (*id, &data_bufs[i][..length_usize]);
    }

    build_sync_write(reg.addr, reg.length, &entry_refs[..n], frame_buf)
}

// ============================================================================
// Encode / decode helpers（按 SIGN_BIT 表自动分支）
// ============================================================================

fn validate_response(
    packet: &StatusPacket<'_>,
    expected: u8,
    device_status_motor: &mut u8,
) -> Result<(), FeetechError> {
    if packet.id != expected {
        return Err(FeetechError::ResponseIdMismatch {
            expected,
            got: packet.id,
        });
    }
    if packet.error != 0 {
        *device_status_motor = packet.id;
        return Err(FeetechError::StatusError(packet.error));
    }
    Ok(())
}

fn encode_value(reg_name: &'static str, value: i32) -> Result<u32, FeetechError> {
    if let Some(bit) = sign_bit_for(reg_name) {
        return encode_sign_magnitude(value, bit);
    }
    // 无符号寄存器，但调用方传入 i32 防接口拥挤；负值在此拒绝
    if value < 0 {
        return Err(FeetechError::ValueExceedsByteRange {
            value: 0, // raw 不存在符号，标 placeholder
            max_bytes: 0,
            max: 0,
        });
    }
    Ok(value as u32)
}

pub(crate) fn decode_value(reg_name: &'static str, raw: u32) -> i32 {
    sign_bit_for(reg_name).map_or(raw as i32, |bit| decode_sign_magnitude(raw, bit))
}

#[cfg(test)]
#[path = "bus_raw_read_tests.rs"]
mod raw_read_tests;

// ============================================================================
// Tests（用 MockTransport 喂虚假回包验 wire 正确性）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::checksum;
    use crate::transport::MockTransport;
    use robot_core::serial_services::{AbortReason, CommErrorKind, PreemptProbe};
    use robot_core::time::DurationNs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const DIAGNOSTIC_NAMES: [&str; 9] = [
        "Phase",
        "Minimum_Starting_Torque",
        "Integral_Limit",
        "CW_Dead_Zone",
        "CCW_Dead_Zone",
        "Angle_Resolution",
        "Acceleration",
        "Torque_Limit",
        "Present_Load",
    ];
    // 请求 checksum 是独立 golden，读回保留高位与超厂家名义范围的原始字节。
    const DIAGNOSTIC_READ_CASES: [(&str, u8, u8, u8, u8); 7] = [
        ("Phase", 18, 0xa5, 0xe5, 0x63),
        ("Minimum_Starting_Torque", 24, 0xff, 0xdf, 0x5d),
        ("Integral_Limit", 25, 0xa5, 0xde, 0x5c),
        ("CW_Dead_Zone", 26, 0xff, 0xdd, 0x5b),
        ("CCW_Dead_Zone", 27, 0x81, 0xdc, 0x5a),
        ("Angle_Resolution", 30, 0x80, 0xd9, 0x57),
        ("Acceleration", 41, 0xff, 0xce, 0x4c),
    ];

    #[derive(Default)]
    struct RejectOnLeaseProbe {
        attempts: AtomicUsize,
    }

    impl PreemptProbe for RejectOnLeaseProbe {
        fn should_abort(&self, _class: BusOpClass) -> Option<AbortReason> {
            panic!("诊断写入拒绝前不得查询服务")
        }

        fn checkpoint(&self) {
            panic!("诊断写入拒绝前不得进入 IO 检查点")
        }

        fn sleep_or_abort(&self, _class: BusOpClass, _dur: DurationNs) -> Result<(), AbortReason> {
            panic!("诊断写入拒绝前不得等待")
        }

        fn operation_class(&self) -> BusOpClass {
            panic!("诊断写入拒绝前不得查询操作域")
        }

        fn confirmation_trusted(&self) -> bool {
            panic!("诊断写入拒绝前不得查询信任状态")
        }

        fn report_comm_error(&self, _kind: CommErrorKind) {
            panic!("诊断写入拒绝不得变成通信故障")
        }
    }

    impl LeasedPreemptProbe for RejectOnLeaseProbe {
        fn begin_leased(&self, _class: BusOpClass) -> Result<BusLease, AbortReason> {
            self.attempts.fetch_add(1, Ordering::Relaxed);
            Err(AbortReason::Emergency)
        }

        fn finish_leased(&self, _lease: BusLease) {
            panic!("拒绝租约的夹具不得归还不存在的租约")
        }
    }

    fn make_bus(
        motors: &[(&str, u8)],
    ) -> (FeetechBus<MockTransport>, crate::transport::MockHandle) {
        let (transport, handle) = MockTransport::new();
        let mut motors_map = HashMap::new();
        for (name, id) in motors {
            motors_map.insert((*name).to_string(), Motor::new(*id).unwrap());
        }
        let bus = FeetechBus::new(transport, motors_map).unwrap();
        (bus, handle)
    }

    /// 构造一个 status packet：`FF FF ID LEN ERR <params...> CHK`
    fn fake_status(id: u8, error: u8, params: &[u8]) -> Vec<u8> {
        let len_field = u8::try_from(params.len() + 2).unwrap();
        let mut out = Vec::with_capacity(6 + params.len());
        out.extend_from_slice(&[0xFF, 0xFF, id, len_field, error]);
        out.extend_from_slice(params);
        out.push(checksum(&out[2..]));
        out
    }

    fn install_rejecting_lease(bus: &mut FeetechBus<MockTransport>) -> Arc<RejectOnLeaseProbe> {
        let probe = Arc::new(RejectOnLeaseProbe::default());
        let service: Arc<dyn LeasedPreemptProbe> = probe.clone();
        bus.set_services(Some(service), None);
        probe
    }

    fn assert_diagnostic_denied(
        result: Result<(), FeetechError>,
        expected_name: &'static str,
        handle: &crate::transport::MockHandle,
        probe: &RejectOnLeaseProbe,
    ) {
        let error = result.expect_err("诊断专用寄存器必须拒写");
        assert!(matches!(
            error,
            FeetechError::DiagnosticRegisterWriteForbidden(got) if got == expected_name
        ));
        assert_eq!(handle.peek_tx(), [0_u8; 0]);
        assert_eq!(probe.attempts.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn firmware_version_writes_are_denied_before_io_lease_and_argument_checks() {
        for name in ["Firmware_Major_Version", "Firmware_Minor_Version"] {
            let (mut bus, handle) = make_bus(&[("j1", 1)]);
            let probe = install_rejecting_lease(&mut bus);
            let mut buffer = [0xa5; 256];
            for result in [
                bus.write(name, "missing", i32::MIN),
                bus.sync_write(name, &[]),
                bus.sync_write(name, &[("missing", i32::MIN)]),
                bus.sync_write_by_id(name, &[]),
                bus.sync_write_by_id(name, &[(0, i32::MIN)]),
                bus.sync_write_by_id_into(name, &[], &mut []),
                bus.sync_write_by_id_into(name, &[(0, i32::MIN)], &mut buffer),
            ] {
                assert!(
                    matches!(result, Err(FeetechError::ReadOnlyRegisterWriteForbidden(got)) if got == name)
                );
            }
            assert_eq!(handle.peek_tx(), [0_u8; 0]);
            assert_eq!(probe.attempts.load(Ordering::Relaxed), 0);
            assert_eq!(buffer, [0xa5; 256]);
        }
    }

    #[test]
    fn firmware_version_reads_preserve_unsigned_byte_and_golden_request() {
        // 请求校验字节独立取自协议和：ID1+长度4+READ2+地址+数据宽1。
        for (name, address, raw, checksum) in [
            ("Firmware_Major_Version", 0, 0xa5, 0xf7),
            ("Firmware_Minor_Version", 1, 0xff, 0xf6),
        ] {
            let (mut bus, handle) = make_bus(&[("j1", 1)]);
            handle.push_rx(&fake_status(1, 0, &[raw]));
            assert_eq!(bus.read(name, "j1").unwrap(), i32::from(raw));
            assert_eq!(
                handle.drain_tx(),
                [0xff, 0xff, 1, 4, 2, address, 1, checksum]
            );
        }
    }

    #[test]
    fn diagnostic_single_write_is_rejected_before_io_or_lease() {
        for name in DIAGNOSTIC_NAMES {
            let (mut bus, handle) = make_bus(&[("j1", 1)]);
            let probe = install_rejecting_lease(&mut bus);
            assert_diagnostic_denied(bus.write(name, "j1", 7), name, &handle, &probe);
        }
    }

    #[test]
    fn diagnostic_sync_write_is_rejected_before_io_or_lease() {
        for name in DIAGNOSTIC_NAMES {
            let (mut bus, handle) = make_bus(&[("j1", 1)]);
            let probe = install_rejecting_lease(&mut bus);
            assert_diagnostic_denied(bus.sync_write(name, &[("j1", 7)]), name, &handle, &probe);
        }
    }

    #[test]
    fn diagnostic_sync_write_by_id_is_rejected_before_io_or_lease() {
        for name in DIAGNOSTIC_NAMES {
            let (mut bus, handle) = make_bus(&[("j1", 1)]);
            let probe = install_rejecting_lease(&mut bus);
            assert_diagnostic_denied(bus.sync_write_by_id(name, &[(1, 7)]), name, &handle, &probe);
        }
    }

    #[test]
    fn diagnostic_preallocated_write_is_rejected_before_io_or_lease() {
        for name in DIAGNOSTIC_NAMES {
            let (mut bus, handle) = make_bus(&[("j1", 1)]);
            let probe = install_rejecting_lease(&mut bus);
            let mut buffer = [0xa5; 256];
            assert_diagnostic_denied(
                bus.sync_write_by_id_into(name, &[(1, 7)], &mut buffer),
                name,
                &handle,
                &probe,
            );
            assert_eq!(buffer, [0xa5; 256]);
        }
    }

    #[test]
    fn diagnostic_denial_precedes_empty_batch_invalid_motor_value_and_buffer_checks() {
        for name in DIAGNOSTIC_NAMES {
            let (mut bus, handle) = make_bus(&[("j1", 1)]);
            let probe = install_rejecting_lease(&mut bus);
            assert_diagnostic_denied(bus.write(name, "missing", i32::MIN), name, &handle, &probe);
            assert_diagnostic_denied(bus.sync_write(name, &[]), name, &handle, &probe);
            assert_diagnostic_denied(
                bus.sync_write(name, &[("missing", i32::MIN)]),
                name,
                &handle,
                &probe,
            );
            assert_diagnostic_denied(bus.sync_write_by_id(name, &[]), name, &handle, &probe);
            assert_diagnostic_denied(
                bus.sync_write_by_id(name, &[(0, i32::MIN)]),
                name,
                &handle,
                &probe,
            );
            assert_diagnostic_denied(
                bus.sync_write_by_id_into(name, &[], &mut []),
                name,
                &handle,
                &probe,
            );
            assert_diagnostic_denied(
                bus.sync_write_by_id_into(name, &[(0, i32::MIN)], &mut []),
                name,
                &handle,
                &probe,
            );
        }
    }

    #[test]
    fn existing_write_still_reaches_the_installed_lease_service() {
        let (mut bus, handle) = make_bus(&[("j1", 1)]);
        let probe = install_rejecting_lease(&mut bus);
        assert!(matches!(
            bus.write("Goal_Position", "j1", 7),
            Err(FeetechError::Aborted(AbortReason::Emergency))
        ));
        assert_eq!(probe.attempts.load(Ordering::Relaxed), 1);
        assert_eq!(handle.peek_tx(), [0_u8; 0]);
    }

    #[test]
    fn unknown_register_writes_keep_the_original_lookup_error() {
        let (mut bus, handle) = make_bus(&[("j1", 1)]);
        for result in [
            bus.write("Unknown_Register", "j1", 7),
            bus.sync_write("Unknown_Register", &[("j1", 7)]),
            bus.sync_write_by_id("Unknown_Register", &[(1, 7)]),
            bus.sync_write_by_id_into("Unknown_Register", &[(1, 7)], &mut [0; 256]),
        ] {
            assert!(matches!(
                result,
                Err(FeetechError::UnknownRegister("Unknown_Register"))
            ));
        }
        assert_eq!(handle.peek_tx(), [0_u8; 0]);
    }

    #[test]
    fn diagnostic_single_read_preserves_raw_byte_and_exact_request() {
        for (name, address, raw, golden_checksum, _) in DIAGNOSTIC_READ_CASES {
            let (mut bus, handle) = make_bus(&[("j1", 1)]);
            handle.push_rx(&fake_status(1, 0, &[raw]));
            assert_eq!(bus.read(name, "j1").unwrap(), i32::from(raw));
            assert_eq!(
                handle.drain_tx(),
                [0xff, 0xff, 1, 4, 2, address, 1, golden_checksum]
            );
        }
    }

    #[test]
    fn diagnostic_word_single_read_preserves_unsigned_value_and_exact_request() {
        for (name, address, golden_checksum) in
            [("Present_Load", 60, 0xba), ("Torque_Limit", 48, 0xc6)]
        {
            for raw in [0x8000_u16, 0x8001, 0xffff] {
                let (mut bus, handle) = make_bus(&[("j1", 1)]);
                handle.push_rx(&fake_status(1, 0, &raw.to_le_bytes()));
                assert_eq!(bus.read(name, "j1").unwrap(), i32::from(raw));
                // ID1 + 长度4 + READ2 + 地址 + 数据宽2 的独立请求校验字节。
                assert_eq!(
                    handle.drain_tx(),
                    [0xff, 0xff, 1, 4, 2, address, 2, golden_checksum]
                );
            }
        }
    }

    #[test]
    fn diagnostic_sync_read_preserves_raw_bytes_and_exact_request() {
        for (name, address, raw, _, golden_checksum) in DIAGNOSTIC_READ_CASES {
            let (mut bus, handle) = make_bus(&[("j1", 1), ("j2", 2)]);
            handle.push_rx(&fake_status(1, 0, &[raw]));
            handle.push_rx(&fake_status(2, 0, &[0xff]));
            let result = bus.sync_read(name, &["j1", "j2"]).unwrap();
            assert_eq!(result.get("j1"), Some(&i32::from(raw)));
            assert_eq!(result.get("j2"), Some(&255));
            assert_eq!(
                handle.drain_tx(),
                [0xff, 0xff, 0xfe, 6, 0x82, address, 1, 1, 2, golden_checksum]
            );
        }
    }

    #[test]
    fn diagnostic_sync_read_by_id_preserves_raw_bytes_and_exact_request() {
        for (name, address, raw, _, golden_checksum) in DIAGNOSTIC_READ_CASES {
            let (mut bus, handle) = make_bus(&[("j1", 1), ("j2", 2)]);
            handle.push_rx(&fake_status(1, 0, &[raw]));
            handle.push_rx(&fake_status(2, 0, &[0xff]));
            let mut result = [i32::MIN; 2];
            bus.sync_read_by_id_into(name, &[1, 2], &mut result)
                .unwrap();
            assert_eq!(result, [i32::from(raw), 255]);
            assert_eq!(
                handle.drain_tx(),
                [0xff, 0xff, 0xfe, 6, 0x82, address, 1, 1, 2, golden_checksum]
            );
        }
    }

    // ── ping ──────────────────────────────────────────────────────────

    #[test]
    fn ping_known_motor_returns_model_number() {
        let (mut bus, handle) = make_bus(&[("a", 5)]);
        // ping status: empty payload
        handle.push_rx(&fake_status(5, 0, &[]));
        // model_number = 777 = 0x0309
        handle.push_rx(&fake_status(5, 0, &[0x09, 0x03]));

        let model = bus.ping(5).unwrap();
        assert_eq!(model, Some(777));

        // host 发了 PING + READ_DATA
        let tx = handle.drain_tx();
        assert_eq!(tx[0..6], [0xFF, 0xFF, 0x05, 0x02, 0x01, 0xF7]); // PING id=5
        // READ_DATA id=5, addr=3, len=2: FF FF 05 04 02 03 02 EF
        assert_eq!(tx[6..14], [0xFF, 0xFF, 0x05, 0x04, 0x02, 0x03, 0x02, 0xEF]);
    }

    #[test]
    fn ping_no_response_returns_none() {
        let (mut bus, _h) = make_bus(&[("a", 5)]);
        // 不喂任何 rx，第一个 read_exact 立即 timeout
        let model = bus.ping(5).unwrap();
        assert_eq!(model, None);
    }

    // ── read / write ──────────────────────────────────────────────────

    #[test]
    fn read_present_position_decodes_sign_magnitude() {
        let (mut bus, handle) = make_bus(&[("j1", 1)]);
        // -400 在 sign_bit=15 编码 = 0x8000 | 400 = 0x8190
        // LE bytes: [0x90, 0x81]
        handle.push_rx(&fake_status(1, 0, &[0x90, 0x81]));
        let v = bus.read("Present_Position", "j1").unwrap();
        assert_eq!(v, -400);

        let tx = handle.drain_tx();
        // READ_DATA id=1, addr=56 (0x38), len=2
        assert_eq!(tx, vec![0xFF, 0xFF, 0x01, 0x04, 0x02, 0x38, 0x02, 0xBE]);
    }

    #[test]
    fn write_goal_position_encodes_and_acks() {
        let (mut bus, handle) = make_bus(&[("j1", 1)]);
        handle.push_rx(&fake_status(1, 0, &[])); // empty ACK status
        bus.write("Goal_Position", "j1", 2048).unwrap();

        let tx = handle.drain_tx();
        // 2048 (positive) sign_bit=15 → 0x0800, LE [0x00, 0x08]
        // WRITE_DATA id=1, addr=42, data=[00,08]: FF FF 01 05 03 2A 00 08 C4
        assert_eq!(
            tx,
            vec![0xFF, 0xFF, 0x01, 0x05, 0x03, 0x2A, 0x00, 0x08, 0xC4]
        );
    }

    #[test]
    fn write_status_error_byte_propagates() {
        let (mut bus, handle) = make_bus(&[("j1", 1)]);
        // motor 回非零 ERROR
        handle.push_rx(&fake_status(1, 0x08, &[]));
        let err = bus.write("Goal_Position", "j1", 100).unwrap_err();
        assert!(matches!(err, FeetechError::StatusError(0x08)));
    }

    #[test]
    fn unknown_register_rejected_at_read_boundary() {
        let (mut bus, _h) = make_bus(&[("j1", 1)]);
        let err = bus.read("Bogus_Register", "j1").unwrap_err();
        assert!(matches!(
            err,
            FeetechError::UnknownRegister("Bogus_Register")
        ));
    }

    #[test]
    fn unknown_motor_rejected() {
        let (mut bus, _h) = make_bus(&[("j1", 1)]);
        let err = bus.read("Goal_Position", "nope").unwrap_err();
        assert!(matches!(err, FeetechError::UnknownMotor(ref n) if n == "nope"));
    }

    // ── sync_read ─────────────────────────────────────────────────────

    #[test]
    fn sync_read_present_position_two_motors() {
        let (mut bus, handle) = make_bus(&[("j1", 1), ("j2", 2)]);
        // motor 1 → 100 (positive, raw=100, LE [0x64, 0x00])
        handle.push_rx(&fake_status(1, 0, &[0x64, 0x00]));
        // motor 2 → -50 (sign_bit=15, encoded = 0x8032, LE [0x32, 0x80])
        handle.push_rx(&fake_status(2, 0, &[0x32, 0x80]));

        let res = bus.sync_read("Present_Position", &["j1", "j2"]).unwrap();
        assert_eq!(res.get("j1"), Some(&100));
        assert_eq!(res.get("j2"), Some(&-50));

        // 验请求帧
        let tx = handle.drain_tx();
        // SYNC_READ to 0xFE: FF FF FE 06 82 38 02 01 02 CHK
        assert_eq!(
            tx[0..9],
            [0xFF, 0xFF, 0xFE, 0x06, 0x82, 0x38, 0x02, 0x01, 0x02]
        );
    }

    #[test]
    fn sync_read_response_id_mismatch_rejected() {
        let (mut bus, handle) = make_bus(&[("j1", 1), ("j2", 2)]);
        // motor 1 回但用了 id=99 → mismatch
        handle.push_rx(&fake_status(99, 0, &[0x00, 0x00]));
        handle.push_rx(&fake_status(2, 0, &[0x00, 0x00]));
        let err = bus
            .sync_read("Present_Position", &["j1", "j2"])
            .unwrap_err();
        match err {
            FeetechError::ResponseIdMismatch {
                expected: 1,
                got: 99,
            } => {}
            other => panic!("预期响应 ID 不匹配，实际 {other:?}"),
        }
    }

    // ── sync_write ────────────────────────────────────────────────────

    #[test]
    fn sync_write_goal_position_two_motors_no_ack_required() {
        let (mut bus, handle) = make_bus(&[("j1", 1), ("j2", 2)]);
        // sync_write 不读 ACK，handle 不需要 push_rx
        bus.sync_write("Goal_Position", &[("j1", 2048), ("j2", -100)])
            .unwrap();

        let tx = handle.drain_tx();
        // SYNC_WRITE to 0xFE, addr=42, length=2
        // motor 1: 2048 → [00 08], motor 2: -100 → 0x8064 → [64 80]
        // PARAMS = [2A, 02, 01, 00, 08, 02, 64, 80] = 8 bytes, LEN=10
        assert_eq!(tx[0..5], [0xFF, 0xFF, 0xFE, 0x0A, 0x83]);
        assert_eq!(tx[5..13], [0x2A, 0x02, 0x01, 0x00, 0x08, 0x02, 0x64, 0x80]);
    }

    // ── handshake ─────────────────────────────────────────────────────

    #[test]
    fn handshake_passes_when_all_motors_respond_with_correct_model() {
        let (mut bus, handle) = make_bus(&[("a", 1), ("b", 2)]);
        // 两 motor 都回 ping ack + model 777
        for id in [1, 2] {
            handle.push_rx(&fake_status(id, 0, &[]));
            handle.push_rx(&fake_status(id, 0, &[0x09, 0x03]));
        }
        // 握手顺序在构造期按 ID 固定，每个回包必须属于实际请求电机。
        bus.handshake().unwrap();
    }

    #[test]
    fn handshake_fails_when_motor_silent() {
        let (mut bus, _h) = make_bus(&[("a", 1)]);
        // 不喂 rx → ping 立即 timeout → 视为 missing
        let err = bus.handshake().unwrap_err();
        match err {
            FeetechError::HandshakeMissing { id: 1 } => {}
            other => panic!("预期握手缺少电机，实际 {other:?}"),
        }
    }

    // ── duplicate id ──────────────────────────────────────────────────

    #[test]
    fn new_rejects_duplicate_motor_id() {
        let (transport, _h) = MockTransport::new();
        let mut motors = HashMap::new();
        motors.insert("a".to_string(), Motor::new(5).unwrap());
        motors.insert("b".to_string(), Motor::new(5).unwrap());
        let Err(err) = FeetechBus::new(transport, motors) else {
            panic!("expected DuplicateMotorId, got Ok");
        };
        assert!(matches!(err, FeetechError::DuplicateMotorId(5)));
    }
}
