"""仅用 Linux PTY 验证默认 S0 与受监督发送器；不连接物理串口。

显式设置 ROBOT_TEST_S0_BIN、ROBOT_TEST_MIXED_BIN 后运行；普通单测跳过。
"""

import errno
import json
import os
from pathlib import Path
import pty
import select
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest


def packet(motor, status, payload=b""):
    body = bytes((motor, len(payload) + 2, status)) + payload
    return b"\xff\xff" + body + bytes(((~sum(body)) & 0xff,))


class ServoPty:
    """固定九电机协议夹具，覆盖本次入口实际使用的寄存器。"""

    def __init__(self, fail_enable=False):
        self.master, self.slave = pty.openpty()
        self.path = os.ttyname(self.slave)
        if not self.path.startswith("/dev/pts/"):
            raise RuntimeError("本测试只允许新建 Linux PTY")
        self.fail_enable = fail_enable
        self.enabled = [0] * 10
        self.goals = [0] * 10
        self.positions = [2047.0] * 10
        self.last_update = time.monotonic()
        self.enable_attempts = 0
        self.nonzero_writes = []
        self.stop_writes = 0
        self.error = None
        self.stop = threading.Event()
        self.worker = threading.Thread(target=self.serve, name="robot-pty-servo")
        self.worker.start()

    def advance(self):
        now = time.monotonic()
        for motor in (7, 8, 9):
            if self.enabled[motor]:
                raw = self.goals[motor]
                speed = -(raw & 0x7fff) if raw & 0x8000 else raw
                self.positions[motor] = (self.positions[motor] + speed * (now - self.last_update)) % 4096
        self.last_update = now

    def read(self, motor, register, width):
        self.advance()
        values = {
            0: 3, 1: 10, 3: 777, 18: 12, 33: int(motor >= 7),
            40: self.enabled[motor], 42: round(self.positions[motor]),
            46: self.goals[motor], 56: round(self.positions[motor]) % 4096,
            58: self.goals[motor] if self.enabled[motor] else 0,
        }
        if register not in values:
            raise AssertionError(f"未预期读取: ID{motor} addr{register}/{width}")
        return int(values[register]).to_bytes(width, "little")

    def write(self, motor, register, payload):
        self.advance()
        value = int.from_bytes(payload, "little")
        if register == 40:
            if value:
                self.enable_attempts += 1
                if self.fail_enable:
                    self.fail_enable = False
                    return 8
            self.enabled[motor] = value
            self.stop_writes += int(value == 0)
        elif register == 42:
            if motor >= 7:
                raise AssertionError("轮子不能接收位置目标")
            if value != 2047:
                raise AssertionError("六臂原位目标被改变")
            self.positions[motor] = float(value)
        elif register == 46:
            if motor < 7:
                raise AssertionError("机械臂不能接收轮速目标")
            self.goals[motor] = value
            if value:
                self.nonzero_writes.append((motor, value, time.monotonic()))
        else:
            raise AssertionError(f"未授权写寄存器: ID{motor} addr{register}")
        return 0

    def respond(self, frame):
        if sum(frame[2:]) & 0xff != 0xff:
            raise AssertionError("请求校验和错误")
        motor, instruction, payload = frame[2], frame[4], frame[5:-1]
        if instruction == 1:
            return packet(motor, 0)
        if instruction == 2:
            return packet(motor, 0, self.read(motor, payload[0], payload[1]))
        if instruction == 3:
            return packet(motor, self.write(motor, payload[0], payload[1:]))
        if instruction == 0x82:
            register, width = payload[:2]
            return b"".join(packet(mid, 0, self.read(mid, register, width)) for mid in payload[2:])
        if instruction == 0x83:
            register, width = payload[:2]
            for offset in range(2, len(payload), width + 1):
                mid = payload[offset]
                status = self.write(mid, register, payload[offset + 1:offset + 1 + width])
                if status:
                    raise AssertionError("测试拒绝点仅用于普通Enable写")
            return b""
        raise AssertionError(f"未预期指令: {instruction:#x}")

    def serve(self):
        pending = bytearray()
        try:
            while not self.stop.is_set():
                readable, _, _ = select.select([self.master], [], [], 0.05)
                if not readable:
                    continue
                try:
                    chunk = os.read(self.master, 4096)
                except OSError as error:
                    if error.errno == errno.EIO:
                        continue
                    raise
                pending.extend(chunk)
                while len(pending) >= 4:
                    if pending[:2] != b"\xff\xff":
                        raise AssertionError(f"请求头错误: {pending[:8].hex()}")
                    size = pending[3] + 4
                    if len(pending) < size:
                        break
                    reply = self.respond(bytes(pending[:size]))
                    del pending[:size]
                    if reply:
                        os.write(self.master, reply)
        except BaseException as error:
            self.error = error

    def close(self):
        self.stop.set()
        self.worker.join(timeout=2)
        if self.worker.is_alive():
            raise AssertionError("PTY夹具未退出")
        os.close(self.master)
        os.close(self.slave)
        if self.error:
            raise self.error


@unittest.skipUnless(sys.platform == "linux" and os.environ.get("ROBOT_TEST_S0_BIN")
                     and os.environ.get("ROBOT_TEST_MIXED_BIN"), "需要显式绑定同源Linux软件测试产物")
class MixedSenderIntegration(unittest.TestCase):
    def run_case(self, fail_enable):
        daemon_bin = Path(os.environ["ROBOT_TEST_S0_BIN"]).resolve(strict=True)
        sender_bin = Path(os.environ["ROBOT_TEST_MIXED_BIN"]).resolve(strict=True)
        evidence_root = os.environ.get("ROBOT_TEST_EVIDENCE_DIR")
        # 指定证据目录时保留原始日志；否则使用可自动回收的单测临时目录。
        temporary = None if evidence_root else tempfile.TemporaryDirectory(prefix="robot-mixed-pty-")
        directory = Path(tempfile.mkdtemp(prefix="mixed-fail-" if fail_enable else "mixed-ok-", dir=evidence_root)) if evidence_root else Path(temporary.name)
        servo = ServoPty(fail_enable)
        daemon = None
        try:
            robot = f"mixed_pty_{os.getpid()}_{time.monotonic_ns()}"
            joints = [f"right_arm_{name}" for name in ("shoulder_pan", "shoulder_lift", "elbow_flex", "wrist_flex", "wrist_roll", "gripper")]
            joints += ["base_right_wheel", "base_front_wheel", "base_left_wheel"]
            calibration = directory / "calibration.json"
            calibration.write_text(json.dumps({name: {"id": index, "drive_mode": 0, "homing_offset": 0, "range_min": 0, "range_max": 4095}
                                               for index, name in enumerate(joints, 1)}))
            config = directory / "robot.toml"
            config.write_text(f'''[robot]
id = "{robot}"
config_version = "pty-only"
[[hardware]]
id = "right_arm_full_with_base"
adapter = "feetech_sts3215"
auto_enable = false
feetech_bus2_mixed_raw = true
address = "{servo.path}"
joints = {json.dumps(joints)}
motor_ids = [1,2,3,4,5,6,7,8,9]
calibration_paths = [{json.dumps(str(calibration))}]
calibration_range_tolerances_raw = [20,20,20,20,20,20,0,0,0]
''')
            trust = directory / "trust"
            trust.mkdir()
            latch = directory / "estop_latch"
            latch.write_text("1\n")
            Path(str(latch) + ".bak").write_text("1\n")
            environment = {key: value for key, value in os.environ.items()
                           if not key.startswith("ROBOT_TEST_") and key not in ("NOTIFY_SOCKET", "INVOCATION_ID", "WATCHDOG_USEC", "WATCHDOG_PID")}
            environment.update(ROBOT_LOG_FORMAT="json", RUST_LOG="info,s0_wheel_observation=debug", NO_COLOR="1")
            # 新建的夹具目录先经原入口规范化为Dirty，再作一次性授权。
            for operation in ("revoke", "authorize"):
                marker = subprocess.run([str(daemon_bin), "rx-trust", operation, "--robot-id", robot,
                                         "--rx-trust-dir", str(trust), "--latch-path", str(latch)],
                                        env=environment, capture_output=True, text=True, timeout=5)
                (directory / f"{operation}.log").write_text(marker.stdout + marker.stderr)
                self.assertEqual(marker.returncode, 0, marker.stdout + marker.stderr)
            log_path = directory / "s0.log"
            with log_path.open("wb") as log:
                daemon = subprocess.Popen([str(daemon_bin), "--config", str(config), "--hardware-id", "right_arm_full_with_base",
                                           "--latch-path", str(latch), "--rx-trust-dir", str(trust), "--notify-mode", "standalone"],
                                          env=environment, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
                until = time.monotonic() + 8
                while time.monotonic() < until and "boot_ready" not in log_path.read_text(errors="replace"):
                    self.assertIsNone(daemon.poll(), log_path.read_text(errors="replace"))
                    time.sleep(0.01)
                self.assertIn("boot_ready", log_path.read_text(errors="replace"))
                output = directory / "result.json"
                command = [str(sender_bin), "--s0-mixed-hold", "--config", str(config), "--hardware-id", "right_arm_full_with_base",
                           "--s0-log", str(log_path), "--output", str(output), "--ack-safety-checklist",
                           "--go-expires-at-unix-ns", str(time.time_ns() + 60_000_000_000)]
                with (directory / "sender.log").open("wb") as sender_log:
                    sender = subprocess.run(command, env=environment, stdout=sender_log, stderr=subprocess.STDOUT, timeout=35)
                self.assertEqual(sender.returncode, 1 if fail_enable else 0, (directory / "sender.log").read_text(errors="replace"))
                self.assertTrue(output.is_file())
                report = json.loads(output.read_text())
                self.assertGreater(servo.enable_attempts, 0)
                self.assertTrue(all(value == 0 for value in servo.enabled[1:]))
                if fail_enable:
                    self.assertFalse(servo.nonzero_writes)
                    self.assertEqual(report["result"], "FAIL")
                else:
                    self.assertEqual(report["result"], "SOFTWARE_PATH_PASS")
                    self.assertEqual(len(report["cycles"]), 400)
                    self.assertEqual(len(report["motion_samples"]), 79)
                    self.assertEqual(len(report["zero_samples"]), 3)
                    self.assertTrue(report["disable_retired"])
                    self.assertTrue(report["torque_off_confirmed"])
                    self.assertTrue(report["motion_position_verified"])
                    self.assertEqual(report["motion_velocity_raw"], "PASS")
                    self.assertEqual(report["stop51"], "NOT_RUN")
                    self.assertFalse(report["manual_power_cut_required"])
                    self.assertTrue(servo.nonzero_writes)
                    self.assertTrue(all(motor == 8 and raw == 500 for motor, raw, _ in servo.nonzero_writes))
                    elapsed = servo.nonzero_writes[-1][2] - servo.nonzero_writes[0][2]
                    self.assertGreater(elapsed, 19.8)
                    self.assertLess(elapsed, 20.1)
                daemon.send_signal(signal.SIGTERM)
                self.assertEqual(daemon.wait(timeout=8), 0, log_path.read_text(errors="replace"))
        finally:
            if daemon is not None and daemon.poll() is None:
                daemon.send_signal(signal.SIGTERM)
                try:
                    daemon.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                    daemon.wait(timeout=3)
            try:
                servo.close()
            finally:
                if temporary:
                    temporary.cleanup()

    def test_full_twenty_seconds_through_default_s0_and_pty(self):
        self.run_case(False)

    def test_enable_failure_stops_without_any_nonzero_command(self):
        self.run_case(True)


if __name__ == "__main__":
    unittest.main()
