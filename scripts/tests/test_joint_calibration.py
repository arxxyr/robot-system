"""单关节维护的模拟总线、持久化、归属校验与一次性命令测试。"""

import json
import math
from pathlib import Path
import socket
import stat
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import joint_calibration as m


class SimulatedMotor:
    def __init__(self):
        self.original = {"homing_offset": 100, "range_min": 950, "range_max": 3300}
        self.registers = {reg: self.original[key] for key, reg in m.REGS.items()}
        self.registers.update(Lock=1, Torque_Enable=0, Operating_Mode=0, Present_Position=2300,
                              Present_Velocity=0)
        self.calls = []
        self.center_feedback = 2047

    def get(self, register):
        return self.registers[register]

    def center(self):
        self.calls.append("center")
        self.registers.update(Homing_Offset=353, Min_Position_Limit=0, Max_Position_Limit=4095,
                              Present_Position=self.center_feedback)
        return 353

    def write(self, row):
        self.calls.append("write")
        self.registers.update({reg: row[key] for key, reg in m.REGS.items()}, Lock=1)


class SessionTests(unittest.TestCase):
    def setUp(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.directory = Path(folder.name).resolve()
        self.file = self.directory / "head.json"
        self.joint = "head_motor_1"
        self.hw = SimulatedMotor()
        self.original = {self.joint: dict(self.hw.original, id=7, drive_mode=1, vendor="保留"),
                         "head_motor_2": {"id": 8}}
        self.baseline = json.dumps(self.original, ensure_ascii=False).encode()
        self.file.write_bytes(self.baseline)
        self.file.chmod(0o640)
        self.session = self.open()

    def open(self):
        return m.Session("bus1", self.joint, 7, self.file, self.hw, self.directory,
                         0.030679702758789063)

    def call(self, action):
        return self.session.reply(dict(action=action, session=self.session.data["session"], bus="bus1"))

    def ok(self, action):
        result = self.call(action)
        self.assertTrue(result["ok"], result)
        return result

    def endpoints(self):
        self.ok("center")
        # 先采大值，程序根据记录值排序。
        self.hw.registers["Present_Position"] = 3100
        self.ok("sample1")
        self.hw.registers["Present_Position"] = 1100
        self.ok("sample2")

    def test_complete_duplicate_readback_and_single_entry_merge(self):
        self.assertEqual(self.ok("status")["position"], 2300)
        self.assertEqual(self.hw.calls, [])
        self.assertTrue(self.session.pending.exists())
        self.endpoints()
        self.ok("center")
        self.ok("sample1")
        self.ok("sample2")
        updated = json.loads(self.file.read_bytes())
        updated["head_motor_2"]["vendor"] = 8
        self.file.write_bytes(m.encode_json(updated))
        self.assertEqual(self.ok("save")["phase"], "saved")
        self.ok("save")
        self.ok("status")
        self.assertEqual(self.hw.calls, ["center", "write"])
        current = json.loads(self.file.read_bytes())
        self.assertEqual(current["head_motor_2"], updated["head_motor_2"])
        self.assertEqual(current[self.joint], dict(self.original[self.joint], homing_offset=353,
                                                   range_min=1100, range_max=3100))
        self.assertEqual(stat.S_IMODE(self.file.stat().st_mode), 0o640)
        self.assertFalse(self.session.pending.exists())
        self.assertEqual(m.strict_json(self.session.last.read_bytes())["phase"], "saved")

    def test_restore_after_process_interruption_and_atomic_replace(self):
        self.endpoints()
        # 仿真文件已替换、控制进程尚未结束的崩溃点。
        with patch.object(self.session, "finish", side_effect=OSError("断电")):
            self.assertFalse(self.call("save")["ok"])
        self.assertTrue(self.session.pending.exists())
        self.session = self.open()
        self.assertEqual(self.ok("status")["phase"], "recovery")
        for action in ("center", "sample1", "sample2", "save", "cancel"):
            self.assertFalse(self.call(action)["ok"])
        self.assertEqual(self.ok("restore")["phase"], "restored")
        self.ok("restore")
        self.assertEqual(self.file.read_bytes(), self.baseline)
        self.assertEqual(self.session.read_registers(), self.hw.original)

    def test_uncertain_center_is_never_replayed(self):
        with patch.object(self.hw, "center", side_effect=ConnectionError("没有回执")) as center:
            self.assertEqual(self.call("center")["phase"], "recovery")
            self.assertFalse(self.call("center")["ok"])
            center.assert_called_once_with()
        self.assertEqual(self.ok("restore")["phase"], "restored")

    def test_failed_hardware_readback_keeps_pending(self):
        self.endpoints()
        with patch.object(self.hw, "write") as write:
            failed = self.call("save")
            self.assertFalse(failed["ok"])
            self.assertEqual(failed["phase"], "recovery")
            write.assert_called_once()
        self.assertEqual(self.file.read_bytes(), self.baseline)
        self.assertTrue(self.session.pending.exists())

    def test_cancel_after_center_restores_without_modifying_other_motor(self):
        self.ok("center")
        self.assertEqual(self.ok("cancel")["phase"], "cancelled")
        self.ok("cancel")
        self.assertEqual(self.hw.calls, ["center", "write"])
        self.assertEqual(self.file.read_bytes(), self.baseline)
        self.assertEqual(self.session.read_registers(), self.hw.original)

    def test_cancel_before_center_never_writes(self):
        self.assertEqual(self.ok("cancel")["phase"], "cancelled")
        self.assertFalse(self.call("center")["ok"])
        self.assertEqual(self.hw.calls, [])

    def test_preexisting_pending_cannot_be_rebound(self):
        with self.assertRaisesRegex(ValueError, "原总线"):
            m.Session("bus1", "head_motor_2", 8, self.file, self.hw, self.directory, math.tau/4096)

    def test_stale_wrong_phase_and_same_side_endpoint(self):
        self.assertFalse(self.session.reply(dict(action="center", session="旧会话", bus="bus1"))["ok"])
        self.assertFalse(self.call("sample1")["ok"])
        self.assertEqual(self.hw.calls, [])
        self.ok("center")
        self.hw.registers["Present_Position"] = 2100
        self.ok("sample1")
        self.hw.registers["Present_Position"] = 2200
        self.assertFalse(self.call("sample2")["ok"])
        self.assertFalse(self.call("save")["ok"])
        self.assertEqual(self.hw.calls, ["center"])

    def test_feedback_jitter_cannot_be_saved_as_range(self):
        self.ok("center")
        for value in (2046, 2048, 2067):
            self.hw.registers["Present_Position"] = value
            self.assertFalse(self.call("sample1")["ok"])
        self.hw.registers["Present_Position"] = 2068
        self.ok("sample1")
        self.hw.registers["Present_Position"] = 2046
        self.assertFalse(self.call("sample2")["ok"])
        self.assertFalse(self.call("save")["ok"])
        self.assertEqual(self.hw.calls, ["center"])

    def test_crossing_single_turn_seam_in_either_direction_requires_restore(self):
        for positions in ((1000, 1, 0, 4095), (3000, 4094, 4095, 0), (1000, 1, -1)):
            with self.subTest(positions=positions):
                self.ok("center")
                for raw in positions[:-1]:
                    self.hw.registers["Present_Position"] = raw
                    self.assertEqual(self.ok("status")["phase"], "centered")
                self.hw.registers["Present_Position"] = positions[-1]
                failed = self.call("sample1")
                self.assertFalse(failed["ok"])
                self.assertEqual(failed["phase"], "recovery")
                self.assertIn("重新标零", failed["position_warning"])
                self.assertFalse(self.call("save")["ok"])
                self.session = self.open()
                self.assertEqual(self.ok("status")["position_warning"], failed["position_warning"])
                self.ok("restore")
                self.assertEqual(self.file.read_bytes(), self.baseline)
                self.session = self.open()

    def test_large_mechanical_range_via_middle_is_valid(self):
        self.ok("center")
        self.hw.registers["Present_Position"] = 4050
        self.ok("sample1")
        for raw in (3050, 2050, 1050):
            self.hw.registers["Present_Position"] = raw
            self.ok("status")
        self.hw.registers["Present_Position"] = 50
        self.ok("sample2")
        self.ok("save")
        self.assertEqual(self.session.read_registers()["range_min"], 50)
        self.assertEqual(self.session.read_registers()["range_max"], 4050)

    def test_lost_sampling_does_not_guess_a_range(self):
        self.ok("center")
        delayed = self.session.last_sample_at + m.MAX_SAMPLE_GAP_SECONDS + 0.01
        with patch.object(m.time, "monotonic", return_value=delayed):
            self.assertEqual(self.ok("status")["phase"], "recovery")
        self.assertIn("采样中断", self.ok("status")["position_warning"])
        self.assertFalse(self.call("sample1")["ok"])
        self.assertEqual(self.hw.calls, ["center"])

    def test_sampling_continues_while_browser_is_disconnected(self):
        self.ok("center")
        readings = iter([1000, 1, 4095])
        crossed = threading.Event()
        original = self.hw.get

        def get(register):
            if register == "Present_Position":
                value = next(readings, 4095)
                if value == 4095:
                    crossed.set()
                return value
            return original(register)

        path = self.directory / "s"
        with patch.object(self.hw, "get", side_effect=get):
            thread = threading.Thread(target=m.serve, args=(path, self.session, 0), daemon=True)
            thread.start()
            self.assertTrue(crossed.wait(timeout=3))
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
                connection.settimeout(2)
                connection.connect(str(path))
                request = dict(action="restore", bus="bus1", session=self.session.data["session"])
                connection.sendall(json.dumps(request).encode() + b"\n")
                result = json.loads(connection.makefile("rb").readline())
            self.assertEqual(result["phase"], "restored")
            self.assertIn("1 → 4095", result["position_warning"])
            thread.join(timeout=2)
            self.assertFalse(thread.is_alive())

    def test_wrong_mode_torque_motion_or_unrepresentable_offset_block_center(self):
        for reg, value in (("Torque_Enable", 1), ("Operating_Mode", 1), ("Present_Velocity", 30),
                           ("Present_Position", 4095 - self.hw.original["homing_offset"])):
            with self.subTest(register=reg), patch.dict(self.hw.registers, {reg: value}):
                self.assertFalse(self.call("center")["ok"])
                self.assertEqual(self.hw.calls, [])

    def test_center_readback_uses_motion_tolerance(self):
        self.hw.center_feedback = 2068
        self.assertFalse(self.call("center")["ok"])
        self.assertEqual(self.ok("restore")["phase"], "restored")
        self.session = self.open()
        self.hw.center_feedback = 2067
        self.assertEqual(self.ok("center")["center"], 2067)

    def test_external_target_edit_aborts_save_before_write(self):
        self.endpoints()
        self.file.write_text(json.dumps({**self.original, self.joint: {"id": 7, "range_min": 1500}}))
        self.assertFalse(self.call("save")["ok"])
        self.assertEqual(self.hw.calls, ["center"])
        self.assertTrue(self.session.pending.exists())

    def test_listener_permissions_protocol_and_disconnect(self):
        path = self.directory / "s"
        thread = threading.Thread(target=m.serve, args=(path, self.session, 0), daemon=True)
        thread.start()
        deadline = time.monotonic() + 2
        while not path.exists() and time.monotonic() < deadline:
            time.sleep(0.005)

        def send(data):
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
                sock.settimeout(1)
                sock.connect(str(path))
                sock.sendall(data)
                return json.loads(sock.makefile("rb").readline())
        status = send(b'{"action":"status"}\n')
        self.assertTrue(status["ok"])
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        self.assertFalse(send(b'x\n')["ok"])
        self.assertEqual(self.hw.calls, [])
        self.assertEqual(send(json.dumps(dict(action="cancel", session=status["session"], bus="bus1")).encode() + b"\n")["phase"],
                         "cancelled")
        thread.join(timeout=2)
        self.assertFalse(thread.is_alive())
        self.assertFalse(path.exists())

    def test_mutations_require_matching_bus(self):
        for bus in (None, "bus2"):
            self.assertFalse(self.session.reply(dict(action="center", session=self.session.data["session"],
                                                     bus=bus))["ok"])
        self.assertEqual(self.hw.calls, [])

    def test_installed_guard_load_state_and_dropin_reset(self):
        marker = f"AssertPathExists=!{self.directory}/bus1.pending.json"
        def check(unit, *, active="inactive", dirty="no", actual=None):
            invocation = ("{ path=/bin/robot-deploy ; argv[]=/bin/robot-deploy guard --config "
                          f"{actual or self.file} -- --rx-trust-dir /var/lib/robot/xlerobot_left "
                          "--latch-path /var/lib/robot/xlerobot_left/estop_latch ; ignore_errors=no ; }")
            def run(argv, **kwargs):
                if "--property=ExecStart" in argv:
                    result = invocation
                elif argv[1] == "show":
                    result = f"LoadState=loaded\nActiveState={active}\nNeedDaemonReload={dirty}\n"
                else:
                    result = unit
                return SimpleNamespace(stdout=result)
            m.check_installed_guard("bus1", self.directory, self.file, run)
        check("[Unit]\n" + marker)
        for unit in ("[Unit]\n" + marker + "\nAssertPathExists=", "[Service]\n" + marker):
            with self.assertRaises(ValueError):
                check(unit)
        other = self.directory / "other.toml"
        other.write_bytes(self.file.read_bytes())
        with self.assertRaisesRegex(ValueError, "实际加载"):
            check("[Unit]\n" + marker, actual=other)
        with self.assertRaises(ValueError):
            check("[Unit]\n" + marker, active="active")
        with self.assertRaises(ValueError):
            check("[Unit]\n" + marker, dirty="yes")

    def test_existing_other_tty_owner_after_exclusive_ioctl(self):
        def run(stdout):
            return lambda *_args, **_kwargs: SimpleNamespace(returncode=0, stdout=stdout)
        m.check_tty_owner(["fuser", "串口"], 15, run(b"    15"))
        for owners in (b"", b" 15 99", b"99"):
            with self.assertRaisesRegex(ValueError, "真实|持有者"):
                m.check_tty_owner(["fuser", "串口"], 15, run(owners))

    def test_config_binds_immutable_bus_file_and_motor_id(self):
        config = self.directory / "bus.toml"
        original = f"""
[robot]
id = "xlerobot_bus1_full"
[[hardware]]
adapter = "feetech_sts3215"
auto_enable = false
address = "{m.DEVICES['left']}"
joints = ["head_motor_1", "head_motor_2"]
motor_ids = [7, 8]
calibration_paths = ["{self.file}"]
[[s1.single_joint.joints]]
name = "head_motor_1"
tolerance_rad = 0.030679702758789063
"""
        config.write_text(original)
        self.assertEqual(m.target(config, self.joint)[:2], ("bus1", "left"))
        base, _, motion = original.partition("[[s1.single_joint.joints]]")
        motion = "[[s1.single_joint.joints]]" + motion
        config.write_text(base)
        separate = self.directory / "active-s1.toml"
        separate.write_text('[robot]\nid = "xlerobot_bus1_full"\n' + motion)
        self.assertEqual(m.target(config, self.joint, separate)[-1], 0.030679702758789063)
        separate.write_text('[robot]\nid = "xlerobot_bus2_full"\n' + motion)
        with self.assertRaisesRegex(ValueError, "总线不匹配"):
            m.target(config, self.joint, separate)
        for bad in (original.replace(m.DEVICES["left"], m.DEVICES["right"]),
                    original.replace("[7, 8]", "[8, 7]"),
                    original.replace("head_motor_1", "base_left_wheel")):
            config.write_text(bad)
            with self.assertRaises(ValueError):
                m.target(config, "base_left_wheel" if "base_left_wheel" in bad else self.joint)


if __name__ == "__main__":
    unittest.main()
