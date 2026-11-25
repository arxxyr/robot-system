#!/usr/bin/env python3
"""现场维护用单位置关节校准。网页只访问 Unix socket，串口始终由本进程独占。"""

import argparse
import base64
import contextlib
import fcntl
import importlib.util
import json
import math
import os
import re
import shlex
from pathlib import Path
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import termios
import time
import tomllib
import types
import uuid

from artifact_checks import require, strict_json
from run_hardware_maintenance import DEVICES


SOCKET = Path("/run/robot-joint-calibration/session.sock")
STATE = Path("/var/lib/robot-joint-calibration")
SIDES = {"xlerobot_bus1_full": ("bus1", "left"), "xlerobot_bus2_full": ("bus2", "right")}
REGS = {"homing_offset": "Homing_Offset", "range_min": "Min_Position_Limit",
        "range_max": "Max_Position_Limit"}
TERMINAL = {"saved", "cancelled", "restored"}


def durable_replace(path, data, *, preserve=False):
    # 临时文件与目标在同一文件系统。JSON 落盘、rename 和目录落盘均在确认之前。
    info = path.stat() if preserve else None
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as tmp:
        temporary = Path(tmp.name)
        try:
            if info is not None:
                current = os.fstat(tmp.fileno())
                if (info.st_uid, info.st_gid) != (current.st_uid, current.st_gid):
                    os.fchown(tmp.fileno(), info.st_uid, info.st_gid)
                os.fchmod(tmp.fileno(), stat.S_IMODE(info.st_mode))
            tmp.write(data)
            tmp.flush()
            os.fsync(tmp.fileno())
            os.replace(temporary, path)
            sync_directory(path.parent)
        finally:
            temporary.unlink(missing_ok=True)


def sync_directory(directory):
    fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def encode_json(value):
    return (json.dumps(value, ensure_ascii=False, indent=2, allow_nan=False) + "\n").encode()


def private_directory(directory):
    directory.mkdir(mode=0o700, exist_ok=True)
    info = directory.lstat()
    require(directory.resolve() == directory and stat.S_ISDIR(info.st_mode)
            and info.st_uid == os.getuid() and stat.S_IMODE(info.st_mode) == 0o700,
            "维护目录必须是当前用户独占的真实目录（0700）")


def target(config, joint, motion_config=None):
    doc = tomllib.loads(config.read_text())
    bus, side = SIDES[doc["robot"]["id"]]
    hardware, = doc["hardware"]
    require(hardware["adapter"] == "feetech_sts3215" and hardware["auto_enable"] is False
            and hardware["address"] == DEVICES[side], "配置的适配器、默认使能或总线身份错误")
    motors = dict(zip(hardware["joints"], hardware["motor_ids"], strict=True))
    require(len(motors) == len(hardware["joints"]) == len(set(motors.values())),
            "关节名称和物理 ID 必须唯一")
    require(joint in motors and not joint.startswith("base_"), "仅支持该总线的位置关节")
    files = []
    for name in hardware["calibration_paths"]:
        path = Path(name)
        require(path.is_absolute() and path.resolve(strict=True) == path and path.is_file(),
                "校准文件必须是配置中的真实普通文件")
        data = strict_json(path.read_bytes())
        if joint in data:
            require(type(data[joint]["id"]) is int and data[joint]["id"] == motors[joint],
                    "校准文件与配置中的电机 ID 不一致")
            files.append(path)
    require(len(files) == 1, "关节必须恰好出现在一个校准文件中")
    motion = doc.get("s1", {}).get("single_joint")
    if motion_config is not None or motion is None:
        motion_file = motion_config or config.with_name(f"xlerobot-{bus}-motion.toml")
        motion_doc = tomllib.loads(motion_file.read_text())
        if "robot" in motion_doc:
            require(motion_doc["robot"]["id"] == doc["robot"]["id"], "到位容差配置的总线不匹配")
        motion = motion_doc["s1"]["single_joint"]
    entry, = (item for item in motion["joints"] if item["name"] == joint)
    tolerance = entry["tolerance_rad"]
    require(type(tolerance) in (int, float) and math.isfinite(tolerance)
            and 0 < tolerance < math.pi, "缺少有效的单关节到位容差")
    return bus, side, motors, files[0], tolerance


def check_installed_guard(bus, state, config, runner=subprocess.run):
    unit = f"robot-s0-{bus}.service"
    show = runner(["systemctl", "show", unit, "--property=LoadState,ActiveState,NeedDaemonReload"],
                  capture_output=True, text=True, timeout=5, check=True).stdout.splitlines()
    require(set(show) == {"LoadState=loaded", "ActiveState=inactive", "NeedDaemonReload=no"},
            "请先完成原停机交接并加载新 unit；维护工具不启停服务")
    # 读取管理器实际已加载的命令行，覆盖 drop-in 或同名旧副本的情形。
    invocation = runner(["systemctl", "show", unit, "--property=ExecStart", "--value"],
                        capture_output=True, text=True, timeout=5, check=True).stdout
    entries = re.findall(r"argv\[\]=(.*?) ; ignore_errors=", invocation)
    require(len(entries) == 1, "无法确认 S0 的有效启动参数")
    argv = shlex.split(entries[0])
    require(len(argv) > 2 and Path(argv[0]).name == "robot-deploy" and argv[1] == "guard"
            and "--" in argv, "实际 S0 入口不是已支持的部署 guard")
    separator = argv.index("--")
    def paths(option):
        require(not any(arg.startswith(option + "=") for arg in argv), "不支持的 S0 参数写法")
        return [Path(argv[i + 1]).resolve(strict=True) for i, arg in enumerate(argv[:-1]) if arg == option]
    require("--config" in argv[:separator] and paths("--config")
            and all(value == config.resolve(strict=True) for value in paths("--config")),
            "维护配置与 S0 实际加载的设备配置不一致")
    side = "left" if bus == "bus1" else "right"
    protect = Path(f"/var/lib/robot/xlerobot_{side}")
    for arg, value in (("--rx-trust-dir", protect), ("--latch-path", protect / "estop_latch")):
        indexes = [i for i, item in enumerate(argv) if item == arg]
        require(len(indexes) == 1 and indexes[0] + 1 < len(argv) and argv[indexes[0]+1] == str(value),
                "S0 有效参数未绑定固定保护域")
    lines = runner(["systemctl", "cat", unit], capture_output=True, text=True,
                   timeout=5, check=True).stdout.splitlines()
    expected = f"!{state}/{bus}.pending.json"
    section, guarded = "", False
    for line in map(str.strip, lines):
        if line.startswith("[") and line.endswith("]"):
            section = line
        elif section == "[Unit]" and line.startswith("Assert") and "=" in line:
            key, value = map(str.strip, line.split("=", 1))
            if not value:
                guarded = False
            elif key == "AssertPathExists" and value == expected:
                guarded = True
    require(guarded, "已安装的 S0 unit 缺少未完成校准阻止启动的断言")


def check_tty_owner(cmd, own_pid, runner=subprocess.run):
    probe = runner(cmd, capture_output=True, timeout=5)
    require(probe.returncode == 0 and probe.stdout.split() == [str(own_pid).encode()],
            "设备独占后仍有其他串口持有者或无法核实")


def load_sdk():
    # 原仓库的顶层 __init__ 会注册 dimos 适配器并创建 .pth；
    # 独立维护进程只载入已安装目录中的原版 Feetech 子包。
    if "xlerobot_dimos" not in sys.modules:
        spec = importlib.util.find_spec("xlerobot_dimos")
        require(spec is not None and spec.submodule_search_locations,
                "固定解释器找不到原版 xlerobot_dimos，请检查安装或 PYTHONPATH")
        package = types.ModuleType("xlerobot_dimos")
        package.__path__ = list(spec.submodule_search_locations)
        sys.modules[package.__name__] = package
    from xlerobot_dimos.common import feetech_io
    return feetech_io


@contextlib.contextmanager
def protected_bus(bus_name, side, motors, joint, config):
    # 与 S0 和既有维护工具共享同样的稳定锁，既不创建也不修改保护文件。
    directory = Path("/var/lib/robot") / f"xlerobot_{side}"
    require(directory.resolve(strict=True) == directory, "保护目录身份错误")
    with contextlib.ExitStack() as stack:
        for name in ("rx_trust.lock", "estop_latch.lock"):
            path = directory / name
            fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
            stack.callback(os.close, fd)
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            info = os.fstat(fd)
            require(stat.S_ISREG(info.st_mode) and info.st_nlink == 1
                    and os.path.samestat(info, path.lstat()), "保护锁身份变化")
        check_installed_guard(bus_name, STATE, config)
        for name in ("estop_latch", "estop_latch.bak", "rx_trust_marker", "rx_trust_marker.bak"):
            path = directory / name
            require(path.resolve(strict=True) == path and path.is_file() and path.read_bytes() == b"1\n",
                    "原交接尚未完成，要求双 Dirty 和双 EStop 锁存")
        port = DEVICES[side]
        # 与既有维护入口一致，检查所有用户的串口持有者。
        cmd = (["sudo", "-n"] if os.geteuid() else []) + ["fuser", port]
        probe = subprocess.run(cmd, capture_output=True, timeout=5)
        require(probe.returncode == 1 and not (probe.stdout or probe.stderr),
                "串口被使用，或无法核实设备独占")
        sdk = load_sdk()

        class CalibrationBus(sdk.FeetechBus):
            def write(self, register, name, value):
                require(name == joint and (register in REGS.values() or
                        register == "Torque_Enable" and value == 0 or register == "Lock" and value in (0, 1)),
                        "本次维护不能发送使能或运动目标")
                return super().write(register, name, value, num_retry=0)

        bus = CalibrationBus(port, {key: sdk.Motor(id=value) for key, value in motors.items()})
        bus.connect(handshake=False)
        stack.callback(bus.disconnect)
        # 固定版 FeetechBus 持有 scservo_sdk 的 PySerial 句柄。独占后才发第一个包。
        fd = bus._port_handler.ser.fileno()
        fcntl.ioctl(fd, termios.TIOCEXCL)
        require(os.fstat(fd).st_rdev == os.stat(port).st_rdev, "串口身份变化")
        # TIOCEXCL 阻止之后的普通 open；复核它生效之前未流入别的真实持有者。
        check_tty_owner(cmd, os.getpid())
        bus._handshake()
        for name in motors:
            require(bus.read("Torque_Enable", name) == 0, "总线上仍有电机上扭")
        yield Hardware(bus, joint, sdk)


class Hardware:
    def __init__(self, bus, joint, sdk):
        self.bus, self.joint, self.sdk = bus, joint, sdk

    def get(self, register):
        return self.bus.read(register, self.joint)

    def center(self):
        with self.bus.lock:
            return self.sdk.set_half_turn_homings(self.bus, [self.joint])[self.joint]

    def write(self, registers):
        value = self.sdk.MotorCalibration(id=self.bus.motors[self.joint].id, **registers)
        with self.bus.lock:
            self.sdk.write_calibration(self.bus, {self.joint: value})


class Session:
    def __init__(self, bus, joint, motor_id, path, hw, state, tolerance):
        self.path, self.hw, self.tolerance = path, hw, tolerance
        self.pending = state / f"{bus}.pending.json"
        self.last = state / f"{bus}.last.json"
        self.position = None
        identity = dict(bus=bus, joint=joint, motor_id=motor_id, path=str(path))
        if self.pending.exists():
            self.data = strict_json(self.pending.read_bytes())
            require(all(self.data[k] == v for k, v in identity.items()),
                    "存在未完成校准，须以原总线、文件和关节进入恢复")
            self.data["phase"] = "recovery"
            return
        self.guard(locked=True)
        raw = path.read_bytes()
        original = strict_json(raw)[joint]
        require(original["id"] == motor_id, "目标关节 ID 与文件不符")
        registers = self.read_registers()
        require(-2047 <= registers["homing_offset"] <= 2047 and
                0 <= registers["range_min"] <= registers["range_max"] <= 4095,
                "原始硬件校准不可恢复")
        self.data = dict(identity, session=str(uuid.uuid4()), phase="ready",
                         original=base64.b64encode(raw).decode(), registers=registers,
                         center=None, offset=None, sample1=None, sample2=None)
        self.persist()

    def guard(self, *, locked):
        require(self.hw.get("Torque_Enable") == 0 and self.hw.get("Operating_Mode") == 0,
                "要求目标位置关节保持断扭")
        require(self.hw.get("Present_Velocity") == 0, "请放手并等待关节停稳")
        if locked:
            require(self.hw.get("Lock") == 1, "EEPROM 尚未锁定")

    def read_registers(self):
        return {key: self.hw.get(reg) for key, reg in REGS.items()}

    def persist(self):
        durable_replace(self.pending, encode_json(self.data))

    def finish(self, phase):
        self.guard(locked=True)
        final = dict(self.data, phase=phase)
        durable_replace(self.last, encode_json(final))
        self.pending.unlink()
        sync_directory(self.last.parent)
        self.data["phase"] = phase

    def begin_write(self):
        self.data["phase"] = "recovery"
        self.persist()

    def original_file(self):
        return base64.b64decode(self.data["original"], validate=True)

    def candidate(self):
        row = self.data
        values = row["sample1"], row["sample2"]
        return dict(homing_offset=row["offset"], range_min=min(values), range_max=max(values))

    def check_registers(self, expected):
        require(self.read_registers() == expected and self.hw.get("Lock") == 1,
                "电机寄存器读回不一致")

    def file_row(self, expected, replacement):
        key = self.data["joint"]
        current = self.path.read_bytes()
        document = strict_json(current)
        require(document.get(key) in (expected, replacement), "校准文件的目标关节被外部修改")
        if document[key] != replacement:
            document[key] = replacement
            original = self.original_file()
            data = original if strict_json(original) == document else encode_json(document)
            durable_replace(self.path, data, preserve=True)
        require(strict_json(self.path.read_bytes()) == document, "校准文件读回不一致")

    def command(self, action, token):
        row = self.data
        require(action in ("status", "center", "sample1", "sample2", "save", "cancel", "restore"),
                "未知校准操作")
        require((action == "status" and not token) or token == row["session"], "会话已过期")
        phase = row["phase"]
        if action == "status":
            if phase not in TERMINAL and phase != "recovery":
                self.position = self.hw.get("Present_Position")
            return
        if phase in TERMINAL:
            require((action, phase) in (("save", "saved"), ("cancel", "cancelled"),
                                        ("restore", "restored")), "会话已结束")
            return
        if action in ("cancel", "restore"):
            if phase == "ready":
                self.finish("cancelled" if action == "cancel" else "restored")
                return
            require(phase != "recovery" or action == "restore", "状态不确定，请显式恢复备份")
            self.guard(locked=False)
            self.begin_write()
            if self.read_registers() != row["registers"] or self.hw.get("Lock") != 1:
                self.hw.write(row["registers"])
            self.check_registers(row["registers"])
            old = strict_json(self.original_file())[row["joint"]]
            last = old | self.candidate() if row["sample2"] is not None else old
            self.file_row(last, old)
            self.finish("cancelled" if action == "cancel" else "restored")
            return
        require(phase != "recovery", "状态不确定，请先恢复备份")
        if action in ("center", "sample1", "sample2") and row[action] is not None:
            return
        if action == "center":
            require(phase == "ready", "请先确认参考中位")
            self.guard(locked=True)
            current = self.hw.get("Present_Position")
            physical = (current + self.hw.get("Homing_Offset")) % 4096
            require(physical != 4095, "当前位置的半转零偏 2048 无法编码，请稍微调整中位")
            # 原版标零会立即改写 EEPROM，包括临时清除原限位。
            self.begin_write()
            row["offset"] = self.hw.center()
            self.check_registers(dict(homing_offset=row["offset"], range_min=0, range_max=4095))
            self.position = self.hw.get("Present_Position")
            require(0 <= self.position <= 4095 and
                    abs(self.position - 2047) * math.tau / 4096 <= self.tolerance,
                    "中位读回偏离 2047，超出该关节到位容差")
            row.update(phase="centered", center=self.position)
        elif action in ("sample1", "sample2"):
            expected = "centered" if action == "sample1" else "sampled1"
            require(phase == expected, "请按顺序采集两个端点")
            self.guard(locked=True)
            value = self.hw.get("Present_Position")
            distance = min(abs(value - row["center"]), abs(value - 2047))
            require(0 <= value <= 4095 and distance * math.tau / 4096 > self.tolerance,
                    "端点需超出本关节的中位容差，请实际移动到可用行程端点")
            if action == "sample2":
                low, high = sorted((value, row["sample1"]))
                require(low < row["center"] < high and low < 2047 < high, "两个端点须位于中位两侧")
            row[action] = self.position = value
            row["phase"] = "sampled1" if action == "sample1" else "sampled2"
        elif action == "save":
            require(phase == "sampled2", "请先采集两个端点")
            self.guard(locked=True)
            self.check_registers(dict(homing_offset=row["offset"], range_min=0, range_max=4095))
            original = strict_json(self.original_file())[row["joint"]]
            require(strict_json(self.path.read_bytes()).get(row["joint"]) == original, "目标文件已改变")
            registers = self.candidate()
            self.begin_write()
            self.hw.write(registers)
            self.check_registers(registers)
            self.file_row(original, original | registers)
            self.finish("saved")
            return
        self.persist()

    def reply(self, request):
        error = None
        try:
            require(isinstance(request, dict) and set(request) <= {"action", "session", "bus"}, "请求结构错误")
            require(request.get("action") == "status" or request.get("bus") == self.data["bus"],
                    "请求必须绑定本次维护总线")
            self.command(request.get("action"), request.get("session"))
        except OSError as exc:
            if self.data["phase"] not in TERMINAL:
                self.data["phase"] = "recovery"
            error = str(exc)
        except Exception as exc:
            error = str(exc)
        result = {k: self.data[k] for k in ("session", "bus", "joint", "phase", "center", "sample1", "sample2")}
        return dict(result, ok=error is None, position=self.position, error=error)


def serve(path, session, terminal_grace=30):
    # 父目录由当前用户独占，上一轮意外退出只会留下 socket 节点。
    if path.exists():
        require(stat.S_ISSOCK(path.lstat().st_mode), "维护 socket 路径被占用")
        path.unlink()
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as server:
        server.bind(str(path))
        try:
            path.chmod(0o600)
            server.listen(4)
            server.settimeout(1)
            terminal_since = None
            while terminal_since is None or time.monotonic() - terminal_since < terminal_grace:
                try:
                    connection, _ = server.accept()
                except TimeoutError:
                    continue
                with connection:
                    connection.settimeout(2)
                    try:
                        with connection.makefile("rb") as stream:
                            line = stream.readline(1025)
                        require(len(line) <= 1024 and line.endswith(b"\n"), "维护请求超长或不完整")
                        response = session.reply(strict_json(line))
                    except (ValueError, OSError) as exc:
                        response = dict(ok=False, error=str(exc))
                    if session.data["phase"] in TERMINAL and terminal_since is None:
                        terminal_since = time.monotonic()
                    try:
                        connection.sendall(json.dumps(response, ensure_ascii=False).encode() + b"\n")
                    except OSError:
                        pass  # 断开后的结果仍由同一会话回答，绝不重放写入。
        finally:
            path.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, type=Path)
    parser.add_argument("--joint", required=True)
    parser.add_argument("--motion-config", type=Path, help="现有 S1 配置或该总线运动片段")
    parser.add_argument("--socket", type=Path, default=SOCKET)
    args = parser.parse_args()
    bus, side, motors, path, tolerance = target(args.config, args.joint, args.motion_config)
    private_directory(args.socket.parent)
    private_directory(STATE)
    # 同机所有总线共用一个入口；锁目录本身，避免移除另一会话的 socket。
    fd = os.open(STATE, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        with protected_bus(bus, side, motors, args.joint, args.config) as hw:
            session = Session(bus, args.joint, motors[args.joint], path, hw, STATE, tolerance)
            serve(args.socket, session)
    finally:
        os.close(fd)


def raise_exit(*_):
    raise SystemExit(1)


if __name__ == "__main__":
    # SIGTERM 沿 finally 释放串口；待恢复的持久记录保留并阻止 S0 重新接管。
    signal.signal(signal.SIGTERM, raise_exit)
    main()
