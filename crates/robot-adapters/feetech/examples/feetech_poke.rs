//! One-shot Feetech wire-level 命令：读/写单个寄存器 / handshake ping。
//!
//! 用途：bring-up / 下使能手动测试 / 单寄存器诊断 —— 比开整个 `s0_daemon`
//! 轻，比改 `mock_s1` 通用。当前 S0 架构（见 `rt_thread.rs:148` 注释）里
//! `state_machine` **未接通** adapter enable/disable 执行链路（Part 2 TODO），
//! 本 example 是这个缺口补上之前的"手动下使能"入口。
//!
//! ## 编译
//!
//! 本 example 依赖 `real-hw` feature（拉 `serialport` crate + libudev）。
//!
//! ```bash
//! cargo build --release --example feetech_poke \
//!   -p robot-adapter-feetech --features real-hw
//! ```
//!
//! ## 用法
//!
//! ```bash
//! feetech_poke <right-device> 9 mixed-hold-left9 --arm-calibration <right_arm.json> --go-expires-at-ns <Unix纳秒>
//! feetech_poke <address> <motor_id> ping
//! feetech_poke <address> <motor_id> sync-zero
//! feetech_poke <address> <motor_id> sync-velocity <value>
//! feetech_poke <address> <motor_id> read  <register>
//! feetech_poke <address> <motor_id> read-raw <register>
//! feetech_poke <address> <motor_id> write <register> <value>
//! ```
//!
//! - `<address>`：串口绝对路径，推荐 `/dev/serial/by-id/usb-1a86_..._if00`
//! - `<motor_id>`：1..=253（0 = broadcast 不支持；254/255 保留）
//! - `<register>`：Feetech 寄存器名，参考 `src/registers.rs`（如
//!   `Goal_Position` / `Present_Position` / `Torque_Enable` / `Operating_Mode` /
//!   `Homing_Offset`）
//! - `<value>`：`i32`。sign-magnitude 编码由 bus 内部处理
//! - `read-raw`：单ID的 `SYNC_READ`；完整校验后输出原始无符号word和本次
//!   TX/RX JSON，不换算单位。`requested_register_address`是请求地址，
//!   状态包不回显该地址；`serial_device`是CLI路径，不证明物理会话身份。
//!   原`read`仍为单 `READ`，不能用新 `SYNC_READ` 回包回填历史读数或推定运动。
//! - `sync-zero`：固定发送单ID的 `Goal_Velocity=0`，地址46、宽度2字节。
//!   `SYNC_WRITE` 无ACK，不启用电机，也不证明电机已接收、实际停止或零速启扭安全。
//! - `sync-velocity`：发送单ID的原始 `Goal_Velocity`，范围-32767..=32767，
//!   地址46、宽度2字节；不附带扭矩、模式或Phase写入。`SYNC_WRITE`无ACK，
//!   发送成功不证明实际速度或固件对使能状态的影响，单位需另行核实。
//!
//! ## 实例
//!
//! ```bash
//! # 读当前位置
//! feetech_poke /dev/serial/by-id/usb-...5A7C121427-if00 6 read Present_Position
//!
//! # 把左夹爪推到闭合位（range_min raw=1408，dimos left_arm.json 标定）
//! feetech_poke /dev/serial/by-id/usb-...5A7C121427-if00 6 write Goal_Position 1408
//!
//! # 下使能（手动掰电机前做；注意此时电机会在重力 / 外力作用下自由运动）
//! feetech_poke /dev/serial/by-id/usb-...5A7C121427-if00 6 write Torque_Enable 0
//!
//! # 使能（危险！不配合 follow-current 防飞序列，motor 会直接追当前 Goal 寄存器值）
//! feetech_poke /dev/serial/by-id/usb-...5A7C121427-if00 6 write Torque_Enable 1
//! ```
//!
//! ## 安全警告
//!
//! - `mixed-hold-left9`是显式受维护入口控制的真实adapter路径，固定六臂锚点+左9原始500，非daemon验收；仅此模式输出单JSON并遵守GO/信号与独立全九断扭。
//! - 其余模式本工具是**裸 wire**，不走 `FeetechAdapter` 的防飞序列（`sync Goal=Present →
//!   sleep 50ms → Torque_Enable=1`）。手动 `write Torque_Enable 1` 前一定要先
//!   `write Goal_Position <当前 Present>`，否则 motor 会追寄存器里残留的旧 Goal。
//! - 写 EEPROM 区寄存器（`Homing_Offset` / `Min_Position_Limit` / `Max_Position_
//!   Limit` / `Operating_Mode` / `Lock`）的保存语义须独立确认。厂家表公开版本181
//!   规定Lock0写EPROM掉电保存、Lock1不保存；不能从中推定现场临时写一定生效。
//!   配置写入需断扭、明确授权及逐项读回；Phase、死区、最小启动扭矩、积分限制值、
//!   角度分辨率仍为本项目诊断只读，新增三项保留厂家名义原始字节，不换算现场单位。
//!   `Present_Load` 也是诊断只读，保留两个字节的 unsigned 原始值，不解码符号或单位。
//!   `Acceleration` / `Torque_Limit` 仅诊断读取原始 unsigned 值，禁止写入。
//! - 大幅度 `Goal_Position` 跳变（超过 Goal Acceleration 能追上的范围）可能引起
//!   机械冲击 —— bring-up 阶段尽量小步推进。

#![cfg(feature = "real-hw")]

#[path = "feetech_poke/mixed_maintenance.rs"]
mod mixed_maintenance;

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;

use robot_adapter_feetech::encoding::encode_sign_magnitude;
use robot_adapter_feetech::registers::{
    STS3215_CONTROL_TABLE, ensure_not_diagnostic_write, ensure_not_firmware_readonly_write,
    get_register, sign_bit_for,
};
use robot_adapter_feetech::transport::real::RealSerialTransport;
use robot_adapter_feetech::{DEFAULT_BAUDRATE, FeetechBus, FeetechError, Motor};

/// 把用户输入的 register 名对齐到 `STS3215_CONTROL_TABLE` 里的 `&'static str`。
///
/// `FeetechBus::read/write` 签名要求 `reg_name: &'static str`（编译期常量 API），
/// 而 CLI 拿到的是 `String`。查表 → 返回静态引用，顺带完成拼写校验。
fn static_register_name(name: &str) -> Option<&'static str> {
    STS3215_CONTROL_TABLE
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(n, _)| *n)
}

/// 纯参数预检，在打开串口前保留九个诊断拒写门并拒绝两个固件版本write。
/// 其他名称与参数继续由原分发处理；本门放行不表示写入合法。
fn preflight_diagnostic_write(args: &[String]) -> Result<(), FeetechError> {
    match args.get(3).map(String::as_str) {
        Some("write") => match args.get(4).and_then(|name| static_register_name(name)) {
            Some(
                name @ ("Phase"
                | "Minimum_Starting_Torque"
                | "Integral_Limit"
                | "CW_Dead_Zone"
                | "CCW_Dead_Zone"
                | "Angle_Resolution"
                | "Acceleration"
                | "Torque_Limit"
                | "Present_Load"),
            ) => ensure_not_diagnostic_write(name),
            Some(name @ ("Firmware_Major_Version" | "Firmware_Minor_Version")) => {
                ensure_not_firmware_readonly_write(name)
            }
            _ => Ok(()),
        },
        _ => Ok(()),
    }
}

fn usage(prog: &str) -> String {
    format!(
        "usage:\n  \
         {prog} <address> <motor_id> ping\n  \
         {prog} <address> <motor_id> sync-zero\n  \
         {prog} <address> <motor_id> sync-velocity <value>\n  \
         {prog} <address> <motor_id> read  <register>\n  \
         {prog} <address> <motor_id> read-raw <register>\n  \
         {prog} <address> <motor_id> write <register> <value>\n"
    )
}

/// 固定零值动作的纯参数门；main在开串口前调用，不接受寄存器或数值参数。
fn sync_zero_arguments(args: &[String], motor_id: u8) -> Result<(), String> {
    if args.len() != 4 {
        return Err("sync-zero不接受寄存器或数值参数".to_string());
    }
    Motor::new(motor_id).map_err(|error| format!("motor_id {motor_id}: {error}"))?;
    Ok(())
}

/// 与总线共用符号幅度规则；main在打开串口前拒绝无效ID、值和参数数量。
fn sync_velocity_value(args: &[String], motor_id: u8) -> Result<i32, String> {
    if args.len() != 5 {
        return Err("sync-velocity要求一个原始速度参数".to_string());
    }
    Motor::new(motor_id).map_err(|error| format!("motor_id {motor_id}: {error}"))?;
    let value = args[4]
        .parse::<i32>()
        .map_err(|error| format!("value '{}' 解析失败: {error}", args[4]))?;
    let sign_bit =
        sign_bit_for("Goal_Velocity").ok_or_else(|| "Goal_Velocity缺少符号位定义".to_string())?;
    encode_sign_magnitude(value, sign_bit).map_err(|error| error.to_string())?;
    Ok(value)
}

fn do_sync_zero<T: robot_adapter_feetech::SerialTransport>(
    bus: &mut FeetechBus<T>,
    motor_id: u8,
    args: &[String],
    output: &mut impl std::io::Write,
) -> ExitCode {
    if let Err(error) = sync_zero_arguments(args, motor_id) {
        eprintln!("error: {error}");
        return ExitCode::from(2);
    }
    send_sync_velocity(bus, motor_id, 0, "sync-zero", output)
}

fn do_sync_velocity<T: robot_adapter_feetech::SerialTransport>(
    bus: &mut FeetechBus<T>,
    motor_id: u8,
    args: &[String],
    output: &mut impl std::io::Write,
) -> ExitCode {
    let value = match sync_velocity_value(args, motor_id) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(2);
        }
    };
    send_sync_velocity(bus, motor_id, value, "sync-velocity", output)
}

fn send_sync_velocity<T: robot_adapter_feetech::SerialTransport>(
    bus: &mut FeetechBus<T>,
    motor_id: u8,
    value: i32,
    action: &str,
    output: &mut impl std::io::Write,
) -> ExitCode {
    if let Err(error) = bus.sync_write_by_id("Goal_Velocity", &[(motor_id, value)]) {
        eprintln!("error: {action} Goal_Velocity@{motor_id}: {error}");
        return ExitCode::from(1);
    }
    match writeln!(
        output,
        "{action} Goal_Velocity@{motor_id} <- {value}: SYNC_WRITE已发送，无ACK；未确认电机接收或实际速度"
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: SYNC_WRITE已发送，但输出失败: {error}");
            ExitCode::from(1)
        }
    }
}

fn do_read<T: robot_adapter_feetech::SerialTransport>(
    bus: &mut FeetechBus<T>,
    motor_id: u8,
    args: &[String],
    prog: &str,
) -> ExitCode {
    if args.len() != 5 {
        eprint!("{}", usage(prog));
        return ExitCode::from(2);
    }
    let register = &args[4];
    let Some(reg_static) = static_register_name(register) else {
        eprintln!("error: unknown register '{register}'");
        return ExitCode::from(2);
    };
    match bus.read(reg_static, "m") {
        Ok(v) => {
            println!("read {register}@{motor_id} = {v}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: read {register}@{motor_id}: {e}");
            ExitCode::from(1)
        }
    }
}

/// 新动作的纯参数门；main在开串口前调用，执行函数复用相同校验。
fn read_raw_register(args: &[String], motor_id: u8) -> Result<&'static str, String> {
    if args.len() != 5 {
        return Err("read-raw要求一个寄存器参数".to_string());
    }
    Motor::new(motor_id).map_err(|error| format!("motor_id {motor_id}: {error}"))?;
    static_register_name(&args[4]).ok_or_else(|| format!("unknown register '{}'", args[4]))
}

fn do_read_raw<T: robot_adapter_feetech::SerialTransport>(
    bus: &mut FeetechBus<T>,
    motor_id: u8,
    args: &[String],
    output: &mut impl std::io::Write,
) -> ExitCode {
    let register = match read_raw_register(args, motor_id) {
        Ok(name) => name,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(2);
        }
    };
    let metadata = match get_register(register) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(2);
        }
    };
    let mut tx = [0_u8; 9];
    let mut rx = [0_u8; 10];
    let mut raw = [0_u32; 1];
    if let Err(error) =
        bus.sync_read_by_id_raw_into(register, &[motor_id], &mut raw, &mut tx, &mut rx)
    {
        eprintln!("error: read-raw {register}@{motor_id}: {error}");
        return ExitCode::from(1);
    }
    // 仅在整包校验成功后输出；这些是本次API实际填入的SYNC_READ片段。
    let report = serde_json::json!({
        "operation": "sync_read",
        "serial_device": args[1],
        "motor_id": motor_id,
        "register": register,
        "requested_register_address": metadata.addr,
        "width": metadata.length,
        "raw_unsigned": raw[0],
        "tx": tx.as_slice(),
        "rx": &rx[..6 + usize::from(metadata.length)],
    });
    match writeln!(output, "{report}") {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: read-raw JSON输出失败: {error}");
            ExitCode::from(1)
        }
    }
}

fn do_write<T: robot_adapter_feetech::SerialTransport>(
    bus: &mut FeetechBus<T>,
    motor_id: u8,
    args: &[String],
    prog: &str,
) -> ExitCode {
    if args.len() != 6 {
        eprint!("{}", usage(prog));
        return ExitCode::from(2);
    }
    let register = &args[4];
    let Some(reg_static) = static_register_name(register) else {
        eprintln!("error: unknown register '{register}'");
        return ExitCode::from(2);
    };
    let value: i32 = match args[5].parse() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: value '{}' 解析失败: {e}", args[5]);
            return ExitCode::from(2);
        }
    };
    match bus.write(reg_static, "m", value) {
        Ok(()) => {
            println!("write {register}@{motor_id} <- {value} ok");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: write {register}@{motor_id} <- {value}: {e}");
            ExitCode::from(1)
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.get(3).map(String::as_str) == Some("mixed-hold-left9") {
        return mixed_maintenance::main(&args);
    }
    if args.get(3).map(String::as_str) == Some("mixed-hold-right9") {
        eprintln!("error: 旧轮位维护入口已停用；ID9为左轮，请重新生成当前计划");
        return ExitCode::from(2);
    }
    // 原裸wire入口继续使用原日志初始化，固定维护stdout只输出一份JSON。
    let _ = robot_logging::init();
    let prog = args
        .first()
        .cloned()
        .unwrap_or_else(|| "feetech_poke".into());

    if args.len() < 4 {
        eprint!("{}", usage(&prog));
        return ExitCode::from(2);
    }
    let address = &args[1];
    let motor_id: u8 = match args[2].parse() {
        Ok(id) => id,
        Err(e) => {
            eprintln!("error: motor_id '{}' 解析失败: {e}", args[2]);
            return ExitCode::from(2);
        }
    };
    let action = args[3].as_str();

    if let Err(error) = preflight_diagnostic_write(&args) {
        eprintln!("error: {error}");
        return ExitCode::from(2);
    }
    if action == "read-raw"
        && let Err(error) = read_raw_register(&args, motor_id)
    {
        eprintln!("error: {error}");
        return ExitCode::from(2);
    }

    if action == "sync-zero"
        && let Err(error) = sync_zero_arguments(&args, motor_id)
    {
        eprintln!("error: {error}");
        return ExitCode::from(2);
    }
    if action == "sync-velocity"
        && let Err(error) = sync_velocity_value(&args, motor_id)
    {
        eprintln!("error: {error}");
        return ExitCode::from(2);
    }

    // 构造 bus：单 motor 名统一叫 "m"（本工具不关心名字语义）
    let transport = match RealSerialTransport::open(Path::new(address), DEFAULT_BAUDRATE) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: open serial '{address}': {e}");
            return ExitCode::from(1);
        }
    };
    let mut motors = HashMap::new();
    let motor = match Motor::new(motor_id) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: motor_id {motor_id}: {e}");
            return ExitCode::from(2);
        }
    };
    motors.insert("m".to_string(), motor);
    let mut bus = match FeetechBus::new(transport, motors) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: FeetechBus::new: {e}");
            return ExitCode::from(1);
        }
    };

    // handshake=true 统一走：验证 motor_id 在线 + Model_Number=STS3215
    if let Err(e) = bus.connect(true) {
        eprintln!("error: bus.connect (handshake) motor_id={motor_id}: {e}");
        return ExitCode::from(1);
    }

    match action {
        "ping" => {
            // connect(true) 已经 handshake 过了，走到这里就是 ping 成功
            println!("ping ok: motor_id={motor_id} on {address} (Model_Number=STS3215 777)");
            ExitCode::SUCCESS
        }
        "sync-zero" => do_sync_zero(&mut bus, motor_id, &args, &mut std::io::stdout().lock()),
        "sync-velocity" => {
            do_sync_velocity(&mut bus, motor_id, &args, &mut std::io::stdout().lock())
        }
        "read" => do_read(&mut bus, motor_id, &args, &prog),
        "read-raw" => do_read_raw(&mut bus, motor_id, &args, &mut std::io::stdout().lock()),
        "write" => do_write(&mut bus, motor_id, &args, &prog),
        other => {
            eprintln!("error: unknown action '{other}'\n{}", usage(&prog));
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(action: &str, register: &str, value: Option<&str>) -> Vec<String> {
        let mut args = vec![
            "feetech_poke".to_string(),
            "/dev/never-open".to_string(),
            "1".to_string(),
            action.to_string(),
            register.to_string(),
        ];
        if let Some(value) = value {
            args.push(value.to_string());
        }
        args
    }

    fn raw_fixture() -> (
        FeetechBus<robot_adapter_feetech::transport::MockTransport>,
        robot_adapter_feetech::transport::MockHandle,
    ) {
        let (transport, handle) = robot_adapter_feetech::transport::MockTransport::new();
        let mut motors = HashMap::new();
        motors.insert("m".to_string(), Motor::new(9).unwrap());
        (FeetechBus::new(transport, motors).unwrap(), handle)
    }

    fn raw_arguments(register: &str) -> Vec<String> {
        let mut args = arguments("read-raw", register, None);
        args[2] = "9".to_string();
        args
    }

    fn status_frame(id: u8, params: &[u8]) -> Vec<u8> {
        let mut frame = vec![0xff, 0xff, id, u8::try_from(params.len() + 2).unwrap(), 0];
        frame.extend_from_slice(params);
        frame.push(robot_adapter_feetech::protocol::checksum(&frame[2..]));
        frame
    }

    fn velocity_arguments(value: &str) -> Vec<String> {
        let mut args = arguments("sync-velocity", value, None);
        args[2] = "9".to_string();
        args
    }

    #[test]
    fn sync_velocity_sends_only_two_velocity_bytes_without_ack_or_torque_write() {
        for (value, low, high, checksum) in [
            ("500", 0xf4, 0x01, 0x49),
            ("-500", 0xf4, 0x81, 0xc9),
            ("0", 0x00, 0x00, 0x3e),
            ("32767", 0xff, 0x7f, 0xc0),
            ("-32767", 0xff, 0xff, 0x40),
        ] {
            let (mut bus, handle) = raw_fixture();
            // 同步写不等ACK；后续READ的预置回包必须完整保留。
            handle.push_rx(&status_frame(9, &[0, 0]));
            let mut output = Vec::new();
            assert_eq!(
                do_sync_velocity(&mut bus, 9, &velocity_arguments(value), &mut output),
                ExitCode::SUCCESS
            );
            assert_eq!(
                handle.peek_tx(),
                [
                    0xff, 0xff, 0xfe, 0x07, 0x83, 0x2e, 0x02, 0x09, low, high, checksum
                ]
            );
            assert_eq!(
                String::from_utf8(output).unwrap(),
                format!(
                    "sync-velocity Goal_Velocity@9 <- {value}: SYNC_WRITE已发送，无ACK；未确认电机接收或实际速度\n"
                )
            );
            assert_eq!(bus.read("Present_Velocity", "m").unwrap(), 0);
        }
    }

    #[test]
    fn sync_velocity_rejects_bad_values_argument_counts_and_ids_before_mock_io() {
        let base = velocity_arguments("500");
        let mut invalid: Vec<_> = (0..5).map(|length| base[..length].to_vec()).collect();
        let mut extra = base.clone();
        extra.push("0".to_string());
        invalid.push(extra);
        for value in [
            "",
            "bad-value",
            "1.5",
            "Torque_Enable",
            "32768",
            "-32768",
            "2147483647",
            "-2147483648",
            "2147483648",
            "-2147483649",
        ] {
            invalid.push(velocity_arguments(value));
        }
        for (args, id) in invalid.into_iter().map(|args| (args, 9)).chain([
            (base.clone(), 0),
            (base.clone(), 254),
            (base, 255),
        ]) {
            assert!(sync_velocity_value(&args, id).is_err());
            let (mut bus, handle) = raw_fixture();
            let mut output = Vec::new();
            assert_eq!(
                do_sync_velocity(&mut bus, id, &args, &mut output),
                ExitCode::from(2)
            );
            assert!(handle.peek_tx().is_empty());
            assert!(output.is_empty());
        }
        for id in [1, 253] {
            assert_eq!(sync_velocity_value(&velocity_arguments("500"), id), Ok(500));
        }
    }

    #[test]
    fn sync_velocity_output_failure_does_not_resend_or_report_success() {
        struct FailedOutput;
        impl std::io::Write for FailedOutput {
            fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("模拟输出失败"))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (mut bus, handle) = raw_fixture();
        assert_eq!(
            do_sync_velocity(&mut bus, 9, &velocity_arguments("500"), &mut FailedOutput),
            ExitCode::from(1)
        );
        assert_eq!(
            handle.peek_tx(),
            [
                0xff, 0xff, 0xfe, 0x07, 0x83, 0x2e, 0x02, 0x09, 0xf4, 0x01, 0x49
            ]
        );
    }

    #[test]
    fn sync_zero_sends_exact_single_id_packet_without_ack_or_other_register_writes() {
        let args: Vec<String> = ["feetech_poke", "/dev/never-open", "9", "sync-zero"]
            .into_iter()
            .map(str::to_string)
            .collect();
        for pending_reply in [false, true] {
            let (mut bus, handle) = raw_fixture();
            if pending_reply {
                // 给下一次普通READ预置回包；零值SYNC_WRITE不得消费它。
                handle.push_rx(&status_frame(9, &[0, 0]));
            }
            let mut output = Vec::new();
            assert_eq!(
                do_sync_zero(&mut bus, 9, &args, &mut output),
                ExitCode::SUCCESS
            );
            assert_eq!(
                handle.peek_tx(),
                [
                    0xff, 0xff, 0xfe, 0x07, 0x83, 0x2e, 0x02, 0x09, 0x00, 0x00, 0x3e
                ]
            );
            assert_eq!(
                String::from_utf8(output).unwrap(),
                "sync-zero Goal_Velocity@9 <- 0: SYNC_WRITE已发送，无ACK；未确认电机接收或实际速度\n"
            );
            if pending_reply {
                assert_eq!(bus.read("Present_Velocity", "m").unwrap(), 0);
            }
        }
    }

    #[test]
    fn sync_zero_rejects_extra_arguments_and_invalid_ids_before_mock_io() {
        let base: Vec<String> = ["feetech_poke", "/dev/never-open", "9", "sync-zero"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let mut invalid = vec![base[..3].to_vec()];
        for extra in [
            vec!["1"],
            vec!["0"],
            vec!["Goal_Velocity"],
            vec!["Goal_Velocity", "0"],
            vec!["Torque_Enable", "1"],
        ] {
            let mut args = base.clone();
            args.extend(extra.into_iter().map(str::to_string));
            invalid.push(args);
        }
        for (args, id) in invalid.into_iter().map(|args| (args, 9)).chain([
            (base.clone(), 0),
            (base.clone(), 254),
            (base, 255),
        ]) {
            assert!(sync_zero_arguments(&args, id).is_err());
            let (mut bus, handle) = raw_fixture();
            let mut output = Vec::new();
            assert_eq!(
                do_sync_zero(&mut bus, id, &args, &mut output),
                ExitCode::from(2)
            );
            assert!(output.is_empty());
            assert!(handle.peek_tx().is_empty());
        }
    }

    #[test]
    fn read_raw_success_reports_requested_metadata_and_actual_sync_read_bytes() {
        for (register, address, data, value) in [
            ("Phase", 18, vec![12_u8], 12_u32),
            ("Minimum_Starting_Torque", 24, vec![255_u8], 255_u32),
            ("Integral_Limit", 25, vec![165_u8], 165_u32),
            ("Angle_Resolution", 30, vec![128_u8], 128_u32),
            ("Acceleration", 41, vec![255_u8], 255_u32),
            ("Torque_Limit", 48, vec![1_u8, 128], 32769_u32),
            ("Present_Velocity", 58, vec![0_u8, 128], 32768_u32),
            ("Present_Load", 60, vec![1_u8, 128], 32769_u32),
        ] {
            let (mut bus, handle) = raw_fixture();
            let response = status_frame(9, &data);
            handle.push_rx(&response);
            let mut output = Vec::new();
            assert_eq!(
                do_read_raw(&mut bus, 9, &raw_arguments(register), &mut output),
                ExitCode::SUCCESS
            );
            assert_eq!(output.iter().filter(|&&byte| byte == b'\n').count(), 1);
            let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(
                report,
                serde_json::json!({
                    "operation": "sync_read", "serial_device": "/dev/never-open", "motor_id": 9,
                    "register": register, "requested_register_address": address, "width": data.len(),
                    "raw_unsigned": value, "tx": handle.peek_tx(), "rx": response,
                })
            );
            assert_eq!(handle.peek_tx()[4], 0x82);
        }
        let (mut bus, handle) = raw_fixture();
        handle.push_rx(&[0xff, 0xff, 9, 4, 0, 0, 128, 114]);
        let mut output = Vec::new();
        assert_eq!(
            do_read_raw(&mut bus, 9, &raw_arguments("Present_Velocity"), &mut output),
            ExitCode::SUCCESS
        );
        assert_eq!(handle.peek_tx(), [0xff, 0xff, 0xfe, 5, 0x82, 58, 2, 9, 53]);
    }

    #[test]
    fn read_raw_keeps_bit15_words_unsigned_without_signed_or_si_fields() {
        for register in ["Present_Velocity", "Present_Load", "Torque_Limit"] {
            for word in [0x8000_u16, 0x8001, 0xffff, 0x7fff] {
                let (mut bus, handle) = raw_fixture();
                handle.push_rx(&status_frame(9, &word.to_le_bytes()));
                let mut output = Vec::new();
                assert_eq!(
                    do_read_raw(&mut bus, 9, &raw_arguments(register), &mut output),
                    ExitCode::SUCCESS
                );
                let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
                assert_eq!(report["raw_unsigned"], u32::from(word));
                assert_eq!(report.as_object().unwrap().len(), 9);
            }
        }
    }

    #[test]
    fn read_raw_bad_checksum_has_exit1_and_no_json() {
        let (mut bus, handle) = raw_fixture();
        let mut response = status_frame(9, &[0, 128]);
        *response.last_mut().unwrap() ^= 1;
        handle.push_rx(&response);
        let mut output = Vec::new();
        assert_eq!(
            do_read_raw(&mut bus, 9, &raw_arguments("Present_Velocity"), &mut output),
            ExitCode::from(1)
        );
        assert!(output.is_empty());
        assert_eq!(handle.peek_tx().len(), 9);
    }

    #[test]
    fn read_raw_wrong_response_id_has_exit1_and_no_json() {
        let (mut bus, handle) = raw_fixture();
        handle.push_rx(&status_frame(8, &[0, 128]));
        let mut output = Vec::new();
        assert_eq!(
            do_read_raw(&mut bus, 9, &raw_arguments("Present_Velocity"), &mut output),
            ExitCode::from(1)
        );
        assert!(output.is_empty());
        assert_eq!(handle.peek_tx().len(), 9);
    }

    #[test]
    fn read_raw_missing_or_partial_reply_has_exit1_and_no_json() {
        for response in [&[][..], &[0xff, 0xff, 9][..]] {
            let (mut bus, handle) = raw_fixture();
            handle.push_rx(response);
            let mut output = Vec::new();
            assert_eq!(
                do_read_raw(&mut bus, 9, &raw_arguments("Present_Velocity"), &mut output),
                ExitCode::from(1)
            );
            assert!(output.is_empty());
            assert_eq!(handle.peek_tx().len(), 9);
        }
    }

    #[test]
    fn read_raw_preflight_rejects_bad_count_register_and_id_with_zero_mock_io() {
        let missing = arguments("read-raw", "Present_Velocity", None)[..4].to_vec();
        let extra = arguments("read-raw", "Present_Velocity", Some("extra"));
        let unknown = raw_arguments("Unknown_Register");
        for (args, id) in [
            (missing, 9),
            (extra, 9),
            (unknown, 9),
            (raw_arguments("Present_Velocity"), 0),
            (raw_arguments("Present_Velocity"), 254),
            (raw_arguments("Present_Velocity"), 255),
        ] {
            assert!(read_raw_register(&args, id).is_err());
            let (mut bus, handle) = raw_fixture();
            let mut output = Vec::new();
            assert_eq!(
                do_read_raw(&mut bus, id, &args, &mut output),
                ExitCode::from(2)
            );
            assert!(output.is_empty());
            assert!(handle.peek_tx().is_empty());
        }
    }

    #[test]
    fn original_read_still_sends_single_read_data() {
        let (mut bus, handle) = raw_fixture();
        handle.push_rx(&status_frame(9, &[1, 128]));
        assert_eq!(
            do_read(
                &mut bus,
                9,
                &arguments("read", "Present_Velocity", None),
                "feetech_poke"
            ),
            ExitCode::SUCCESS
        );
        assert_eq!(handle.peek_tx(), [0xff, 0xff, 9, 4, 2, 58, 2, 180]);
    }

    #[test]
    fn diagnostic_write_preflight_denies_even_missing_or_invalid_values() {
        for name in [
            "Phase",
            "Minimum_Starting_Torque",
            "Integral_Limit",
            "CW_Dead_Zone",
            "CCW_Dead_Zone",
            "Angle_Resolution",
            "Acceleration",
            "Torque_Limit",
            "Present_Load",
        ] {
            for value in [None, Some("bad-value"), Some("255"), Some("2147483648")] {
                let args = arguments("write", name, value);
                assert!(matches!(
                    preflight_diagnostic_write(&args),
                    Err(FeetechError::DiagnosticRegisterWriteForbidden(got)) if got == name
                ));
            }
        }
    }

    #[test]
    fn firmware_version_preflight_rejects_write_and_preserves_read_before_open() {
        for name in ["Firmware_Major_Version", "Firmware_Minor_Version"] {
            assert_eq!(static_register_name(name), Some(name));
            for value in [None, Some("bad-value"), Some("255"), Some("2147483648")] {
                assert!(
                    matches!(preflight_diagnostic_write(&arguments("write", name, value)), Err(FeetechError::ReadOnlyRegisterWriteForbidden(got)) if got == name)
                );
            }
            assert!(preflight_diagnostic_write(&arguments("read", name, None)).is_ok());
        }
    }

    #[test]
    fn existing_write_preflight_keeps_supported_arguments() {
        for (register, value) in [
            ("Goal_Position", "-100"),
            ("Goal_Velocity", "20"),
            ("Torque_Enable", "0"),
            ("Operating_Mode", "1"),
            ("Model_Number", "777"),
        ] {
            assert!(preflight_diagnostic_write(&arguments("write", register, Some(value))).is_ok());
        }
    }

    #[test]
    fn existing_missing_invalid_and_unknown_arguments_remain_for_original_dispatch() {
        for register in ["Goal_Position", "Unknown_Register"] {
            for value in [None, Some("bad-value"), Some("2147483648")] {
                assert!(preflight_diagnostic_write(&arguments("write", register, value)).is_ok());
            }
        }
        let args: Vec<String> = ["feetech_poke", "/dev/never-open", "1", "write"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert!(preflight_diagnostic_write(&args).is_ok());
    }

    #[test]
    fn original_do_write_still_rejects_bad_arguments_without_mock_io() {
        let (transport, handle) = robot_adapter_feetech::transport::MockTransport::new();
        let mut motors = HashMap::new();
        motors.insert("m".to_string(), Motor::new(1).unwrap());
        let mut bus = FeetechBus::new(transport, motors).unwrap();
        for args in [
            arguments("write", "Goal_Position", None),
            arguments("write", "Goal_Position", Some("bad-value")),
            arguments("write", "Goal_Position", Some("2147483648")),
            arguments("write", "Unknown_Register", Some("1")),
        ] {
            assert!(preflight_diagnostic_write(&args).is_ok());
            assert_eq!(
                do_write(&mut bus, 1, &args, "feetech_poke"),
                ExitCode::from(2)
            );
        }
        assert_eq!(handle.peek_tx(), [0_u8; 0]);
    }

    #[test]
    fn read_and_ping_preflight_preserve_existing_dispatch() {
        for name in [
            "Phase",
            "Minimum_Starting_Torque",
            "Integral_Limit",
            "CW_Dead_Zone",
            "CCW_Dead_Zone",
            "Angle_Resolution",
            "Acceleration",
            "Torque_Limit",
            "Present_Load",
            "Present_Position",
        ] {
            for action in ["read", "read-raw"] {
                assert!(preflight_diagnostic_write(&arguments(action, name, None)).is_ok());
            }
        }
        let args: Vec<String> = ["feetech_poke", "/dev/never-open", "1", "ping"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert!(preflight_diagnostic_write(&args).is_ok());
    }
}
