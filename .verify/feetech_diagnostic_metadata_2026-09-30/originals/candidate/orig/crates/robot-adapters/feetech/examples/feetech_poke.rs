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
//! feetech_poke <address> <motor_id> ping
//! feetech_poke <address> <motor_id> read  <register>
//! feetech_poke <address> <motor_id> write <register> <value>
//! ```
//!
//! - `<address>`：串口绝对路径，推荐 `/dev/serial/by-id/usb-1a86_..._if00`
//! - `<motor_id>`：1..=253（0 = broadcast 不支持；254/255 保留）
//! - `<register>`：Feetech 寄存器名，参考 `src/registers.rs`（如
//!   `Goal_Position` / `Present_Position` / `Torque_Enable` / `Operating_Mode` /
//!   `Homing_Offset`）
//! - `<value>`：`i32`。sign-magnitude 编码由 bus 内部处理
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
//! - 本工具是**裸 wire**，不走 `FeetechAdapter` 的防飞序列（`sync Goal=Present →
//!   sleep 50ms → Torque_Enable=1`）。手动 `write Torque_Enable 1` 前一定要先
//!   `write Goal_Position <当前 Present>`，否则 motor 会追寄存器里残留的旧 Goal。
//! - 写 EEPROM 区寄存器（`Homing_Offset` / `Min_Position_Limit` / `Max_Position_
//!   Limit` / `Operating_Mode` / `Lock`）需要先 `write Torque_Enable 0` + `write
//!   Lock 0`，否则**静默失败**（motor 不报错但不生效）。
//! - 大幅度 `Goal_Position` 跳变（超过 Goal Acceleration 能追上的范围）可能引起
//!   机械冲击 —— bring-up 阶段尽量小步推进。

#![cfg(feature = "real-hw")]

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;

use robot_adapter_feetech::registers::STS3215_CONTROL_TABLE;
use robot_adapter_feetech::transport::real::RealSerialTransport;
use robot_adapter_feetech::{DEFAULT_BAUDRATE, FeetechBus, Motor};

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

fn usage(prog: &str) -> String {
    format!(
        "usage:\n  \
         {prog} <address> <motor_id> ping\n  \
         {prog} <address> <motor_id> read  <register>\n  \
         {prog} <address> <motor_id> write <register> <value>\n"
    )
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
    // task-32：统一 tracing subscriber 初始化。
    let _ = robot_logging::init();

    let args: Vec<String> = std::env::args().collect();
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
        "read" => do_read(&mut bus, motor_id, &args, &prog),
        "write" => do_write(&mut bus, motor_id, &args, &prog),
        other => {
            eprintln!("error: unknown action '{other}'\n{}", usage(&prog));
            ExitCode::from(2)
        }
    }
}
