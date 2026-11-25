#!/usr/bin/env python3
"""从本机网页接收一次校准请求，占用所选侧总线，结束后交还保护待机。"""

import contextlib
import fcntl
import http.client
import ipaddress
import json
import os
from pathlib import Path
import re
import shlex
import signal
import socket
import subprocess
import sys
import time
import tomllib

import joint_calibration as cal
from artifact_checks import require, strict_json
from run_hardware_maintenance import STOP_REGS, validate_stop_stability_value, wheel_stop_contract


def command(*args, timeout=5):
    result = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
    require(result.returncode == 0, f"{Path(args[0]).name} {args[1]} 执行失败：{result.stderr.strip()[:240]}")
    return result.stdout.strip()


def service_args(unit, executable):
    value = command("systemctl", "show", unit, "--property=ExecStart", "--value")
    entries = re.findall(r"argv\[\]=(.*?) ; ignore_errors=", value)
    require(len(entries) == 1, f"{unit} 未绑定唯一程序")
    args = shlex.split(entries[0])
    require(args and Path(args[0]).name == executable, f"{unit} 程序与预期不符")
    return args


def argument(args, name):
    values = {following for option, following in zip(args, args[1:]) if option == name}
    require(len(values) == 1, f"缺少或不一致的 {name}")
    return values.pop()


def unit_state(unit):
    props = command("systemctl", "show", unit,
                    "--property=LoadState,NeedDaemonReload,ActiveState,Result,ExecMainStatus")
    values = dict(line.split("=", 1) for line in props.splitlines())
    require(values["LoadState"] == "loaded" and values["NeedDaemonReload"] == "no",
            f"{unit} 配置尚未加载")
    require(values["Result"] == "success" and values["ExecMainStatus"] == "0",
            f"{unit} 上次启停失败，需先核对")
    return values["ActiveState"]


def markers(directory, name):
    values = []
    for filename in (name, name + ".bak"):
        path = directory / filename
        require(path.resolve(strict=True) == path and path.is_file(), "保护标记路径不符")
        values.append(path.read_bytes())
    return tuple(values)


def stopped_bus(hw, motors):
    # 原三寄存器停止合同按本次独占的总线检查。SDK 已将速度的符号位解码。
    for joint, motor in motors.items():
        def read(register):
            return hw.bus.read(register, joint, num_retry=0)

        torque, speed, goal = (read(register) for register in STOP_REGS)
        require(torque == goal == 0, f"{joint} 断扭或目标速度未归零")
        if speed == 0:
            continue
        require(joint.startswith("base_") and speed in (-50, 50), f"{joint} 速度未归零")
        contract = wheel_stop_contract(("right", motor, joint))
        positions = [read("Present_Position")]
        require(0 <= positions[0] <= 4095, f"{joint} 轮位置无效")
        deadline = time.monotonic_ns() + contract["capture_budget_ns"]

        def check(register):
            value = read(register)
            if register == "Present_Velocity" and value < 0:
                value = 0x8000 | -value
            validate_stop_stability_value(register, value, contract, positions)
            require(time.monotonic_ns() <= deadline, f"{joint} 停止复核超时")

        check("Torque_Enable")
        check("Goal_Velocity")
        start = time.monotonic_ns()
        for offset in contract["sample_offsets_ns"]:
            time.sleep(max(0, start + offset - time.monotonic_ns()) / 1e9)
            check("Present_Position")
            check("Present_Velocity")
        check("Goal_Velocity")
        check("Torque_Enable")


class Handoff:
    def __init__(self, bus, joint):
        require(bus in ("bus1", "bus2") and isinstance(joint, str) and 0 < len(joint) <= 64,
                "需要指定总线和位置关节")
        self.bus, self.joint = bus, joint
        self.calibration_result = None
        self.s0, self.s1 = (f"robot-{layer}-{bus}.service" for layer in ("s0", "s1"))
        guard = service_args(self.s0, "robot-deploy")
        s1 = service_args(self.s1, "robot-s1")
        require(guard[1] == "guard" and "--" in guard, "S0 缺少正式部署保护")
        self.config = Path(argument(guard, "--config"))
        self.daemon = Path(argument(guard[:guard.index("--")], "--s0-bin"))
        require(self.daemon.is_absolute() and self.daemon.name == "robot-s0", "S0 程序绑定错误")
        self.robot = argument(guard, "--robot-id")
        require(argument(s1, "--robot-id") == self.robot and cal.SIDES[self.robot][0] == bus,
                "运行的 S0、S1 与请求总线不一致")
        actual, self.side, self.motors, self.file, self.tolerance = cal.target(
            self.config, joint, Path(argument(s1, "--config")))
        require(actual == bus, "所选关节与已加载的总线配置不一致")
        self.protect = Path("/var/lib/robot") / f"xlerobot_{self.side}"
        require(argument(guard, "--rx-trust-dir") == str(self.protect)
                and argument(guard, "--latch-path") == str(self.protect / "estop_latch"),
                "已加载的 S0 保护目录不匹配")
        web_config = Path(argument(service_args("robot-status.service", "robot-status"), "--config"))
        listen = tomllib.loads(web_config.read_text())["listen"]
        host, port = listen.rsplit(":", 1)
        address = ipaddress.ip_address(host.strip("[]"))
        self.http = (str(address) if not address.is_unspecified else "127.0.0.1", int(port))

    def note(self, phase, error=None):
        record = dict(bus=self.bus, joint=self.joint, phase=phase, error=error)
        if self.calibration_result is not None:
            record["calibration_result"] = self.calibration_result
        cal.durable_replace(cal.SOCKET.parent / "handoff.json", cal.encode_json(record))

    def standby(self, after=0):
        with contextlib.closing(http.client.HTTPConnection(*self.http, timeout=3)) as client:
            client.request("GET", "/api/state")
            response = client.getresponse()
            require(response.status == 200, "无法读取本机网页总线状态")
            data = strict_json(response.read(524289))
        row, = (r for r in data["buses"] if r["id"] == self.bus)
        require(row["robot_id"] == self.robot and row["data_status"] == "fresh"
                and int(row["stamp_ns"] or 0) > after
                and row["mode"] == "Safe" and row["safety_state"] == "TorqueOffConfirmed"
                and row["control"]["enabled"] is True and row["control"]["busy"] is False,
                "请先在所选侧断扭并锁存急停，等待任务结束和实时反馈")
        require(markers(self.protect, "estop_latch") == (b"1\n",) * 2, "请先在所选侧锁存急停")

        return int(row["stamp_ns"])

    def trust(self, action):
        return command(str(self.daemon), "rx-trust", action, "--robot-id", self.robot,
                       "--rx-trust-dir", str(self.protect), "--latch-path", str(self.protect / "estop_latch"))

    def open(self, ready):
        pending = cal.STATE / f"{self.bus}.pending.json"
        for record in cal.STATE.glob("bus*.pending.json"):
            require(record == pending and strict_json(record.read_bytes()).get("joint") == self.joint,
                    "其他关节已有未完成校准，请先恢复该关节")
        states = unit_state(self.s0), unit_state(self.s1)
        stamp = 0
        if states == ("active", "active"):
            require(not pending.exists(), "待恢复校准与运行中的 S0 状态冲突")
            cal.check_installed_guard(self.bus, cal.STATE, self.config, active_state="active")
            stamp = self.standby()
            self.note("starting")
            # 顺序由原 systemd Requires/After 决定。在线确认丢失时不重发启停。
            command("sudo", "-n", "systemctl", "stop", self.s1, self.s0, timeout=45)
        else:
            require(states == ("inactive", "inactive") and pending.is_file(),
                    "该侧交接状态不完整，仅允许恢复原关节已登记的校准")
            self.note("starting")

        require(all(unit_state(unit) == "inactive" for unit in (self.s1, self.s0)),
                "所选侧正常停止尚未确认")
        cal.check_installed_guard(self.bus, cal.STATE, self.config)
        require(markers(self.protect, "estop_latch") == (b"1\n",) * 2, "急停锁存读回不一致")
        trust = markers(self.protect, "rx_trust_marker")
        require(trust == (b"0\n",) * 2 or (pending.is_file() and trust == (b"1\n",) * 2),
                "正常停机尚未留下预期的 RX 信任状态")
        if trust == (b"0\n",) * 2:
            self.trust("revoke")
        require(markers(self.protect, "rx_trust_marker") == (b"1\n",) * 2, "撤销 RX 授权未确认")
        with cal.protected_bus(self.bus, self.side, self.motors, self.joint, self.config) as hardware:
            stopped_bus(hardware, self.motors)
            session = cal.Session(self.bus, self.joint, self.motors[self.joint], self.file,
                                  hardware, cal.STATE, self.tolerance)

            def listening():
                self.note("serving")
                ready(session.reply({"action": "status"}))

            cal.serve(cal.SOCKET, session, terminal_grace=2, on_ready=listening)
            require(session.data["phase"] in cal.TERMINAL and not pending.exists(), "校准尚未收尾")
            self.calibration_result = session.data["phase"]
            self.note("returning")
            stopped_bus(hardware, self.motors)
        require(markers(self.protect, "estop_latch") == (b"1\n",) * 2, "归还前急停锁存异常")
        require(unit_state("robot-s0-preflight.service") == "active", "整机预检服务需现场恢复")
        try:
            self.trust("authorize")
            require(markers(self.protect, "rx_trust_marker") == (b"0\n",) * 2, "RX 归还授权未确认")
            command("sudo", "-n", "systemctl", "start", self.s0, timeout=60)
            require(unit_state(self.s0) == "active", "S0 未进入保护待机")
            self.wait_standby(stamp)
            command("sudo", "-n", "systemctl", "start", self.s1, timeout=35)
            require(unit_state(self.s1) == "active", "S1 未恢复")
            self.wait_standby(stamp)
        except BaseException:
            # 归还失败后取消本次可能尚在执行的启动，只在确认 S0 停止后撤销剩余授权。
            command("sudo", "-n", "systemctl", "stop", self.s1, self.s0, timeout=45)
            require(command("systemctl", "show", self.s0, "--property=ActiveState",
                            "--value") == "inactive", "归还失败且停止结果未确认")
            self.trust("revoke")
            raise
        self.note("returned")

    def wait_standby(self, stamp):
        deadline = time.monotonic() + 15
        while True:
            try:
                self.standby(after=stamp)
                return
            except (ValueError, OSError):
                if time.monotonic() >= deadline:
                    raise
                time.sleep(0.25)


def handle(connection):
    def reply(value):
        try:
            connection.sendall(json.dumps(value, ensure_ascii=False).encode() + b"\n")
        except OSError:
            pass  # 浏览器断开后，既有会话仍可按原 socket 查询和收尾。

    sent = False
    def ready(value):
        nonlocal sent
        sent = True
        reply(value)

    try:
        connection.settimeout(3)
        with connection.makefile("rb") as stream:
            line = stream.readline(1025)
        require(len(line) <= 1024 and line.endswith(b"\n"), "开启请求格式错误")
        req = strict_json(line)
        require(isinstance(req, dict) and set(req) == {"action", "bus", "joint"} and req["action"] == "start",
                "开启入口仅接受所选关节")
        cal.private_directory(cal.SOCKET.parent)
        cal.private_directory(cal.STATE)
        fd = os.open(cal.STATE, os.O_RDONLY | os.O_DIRECTORY)
        with contextlib.ExitStack() as close:
            close.callback(os.close, fd)
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise ValueError("已有维护会话或正在交接，请读取维护状态") from None
            current = Handoff(req["bus"], req["joint"])
            try:
                current.open(ready)
            except BaseException as failure:
                current.note("failed", str(failure)[:320] or "维护进程中断，需核对状态")
                raise
    except Exception as error:
        if not sent:
            reply(dict(ok=False, error=str(error)[:320]))
        print(f"维护交接：{error}", file=sys.stderr, flush=True)
        return 1
    return 0


if __name__ == "__main__":
    # Accept=yes 由 systemd 为每条请求传入已接受的 Unix stream。
    signal.signal(signal.SIGTERM, cal.raise_exit)
    signal.signal(signal.SIGINT, cal.raise_exit)
    with socket.socket(fileno=os.dup(0)) as client:
        sys.exit(handle(client))
