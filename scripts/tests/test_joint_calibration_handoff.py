"""以假 systemd 和串口验证网页接管、写入时机、收尾和恢复。"""

import contextlib
import json
import socket
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import joint_calibration_handoff as m
from test_joint_calibration import SimulatedMotor


class HandoffTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name).resolve()
        self.runtime = root / "run"
        self.state = root / "state"
        self.sock = self.runtime / "session.sock"
        self.runtime.mkdir(mode=0o700)
        self.state.mkdir(mode=0o700)
        self.web = root / "web.toml"
        self.web.write_text('listen = "127.0.0.1:18080"')
        self.joint = "right_arm_wrist_roll"
        self.others = {"right_arm_gripper": {"id": 6}}
        self.hw = SimulatedMotor()
        self.hw.bus = SimpleNamespace(read=lambda *_args, **_kw: 0)
        self.config = root / "s0.toml"
        self.profile = root / "s1.toml"
        self.calibration = root / "right_arm.json"
        self.calibration.write_text(json.dumps({self.joint: dict(self.hw.original, id=5), **self.others}))
        self.states = {f"robot-{layer}-bus2.service": "active" for layer in ("s0", "s1")}
        self.states["robot-s0-preflight.service"] = "active"
        self.trust = b"1\n"
        self.running = True
        self.events = []
        self.error = None

        def service_args(unit, program):
            common = ["--robot-id", "xlerobot_bus2_full", "--config", str(self.config)]
            if unit == "robot-status.service":
                return [program, "--config", str(self.web)]
            if unit == "robot-s0-bus2.service":
                return [program, "guard", *common, "--s0-bin", "/test/robot-s0", "--",
                        "--latch-path", "/var/lib/robot/xlerobot_right/estop_latch",
                        "--rx-trust-dir", "/var/lib/robot/xlerobot_right"]
            self.assertEqual(unit, "robot-s1-bus2.service")
            return [program, "--config", str(self.profile), *common[:2]]

        def target(config, joint, motion):
            self.assertEqual((config, joint, motion), (self.config, self.joint, self.profile))
            return "bus2", "right", {self.joint: 5}, self.calibration, 0.031

        def invoke(*args, **_opts):
            self.events.append(args)
            if args[:3] == ("sudo", "-n", "systemctl"):
                if args[3] == "stop":
                    self.assertEqual(set(args[4:]), {"robot-s0-bus2.service", "robot-s1-bus2.service"})
                    self.states.update({name: "inactive" for name in args[4:]})
                    self.trust = b"0\n"
                elif args[3] == "start":
                    if self.error:
                        raise ValueError(self.error)
                    self.states[args[4]] = "active"
                    if args[4] == "robot-s0-bus2.service":
                        self.trust = b"1\n"
            elif args[0] == "/test/robot-s0":
                self.assertEqual(args[1], "rx-trust")
                self.trust = b"0\n" if args[2] == "authorize" else b"1\n"
            elif args[:2] == ("systemctl", "show"):
                return self.states[args[2]]
            else:
                self.fail(f"意外命令 {args}")
            return ""

        def fresh(_handoff, after=0):
            if not self.running:
                raise ValueError("总线正在运动")
            return after + 1

        def fast_serve(path, session, **kw):
            return self.serve(path, session, 0, on_ready=kw["on_ready"])

        self.serve = m.cal.serve
        stack = self.enterContext(contextlib.ExitStack())
        for owner, name, value in (
            (m, "service_args", service_args), (m, "command", invoke),
            (m, "unit_state", self.states.__getitem__), (m, "markers", lambda _, name:
                    (b"1\n" if name == "estop_latch" else self.trust,) * 2),
            (m.cal, "check_installed_guard", lambda *_, **_kw: None),
            (m.cal, "STATE", self.state), (m.cal, "SOCKET", self.sock), (m.cal, "target", target),
            (m.cal, "protected_bus", lambda *_: contextlib.nullcontext(self.hw)),
            (m.Handoff, "standby", fresh), (m.cal, "serve", fast_serve),
        ):
            stack.enter_context(patch.object(owner, name, value))

    @staticmethod
    def exchange(connection, request):
        connection.settimeout(3)
        connection.sendall(json.dumps(request).encode() + b"\n")
        with connection.makefile("rb") as stream:
            return json.loads(stream.readline())

    def launch(self):
        client, accepted = socket.socketpair()
        def run():
            with accepted:
                m.handle(accepted)
        thread = threading.Thread(target=run, daemon=True)
        thread.start()
        with client:
            response = self.exchange(client, {"action": "start", "bus": "bus2", "joint": self.joint})
        return thread, response

    def operate(self, response, action):
        with socket.socket(socket.AF_UNIX) as client:
            client.connect(str(self.sock))
            return self.exchange(client, dict(action=action, bus="bus2", session=response["session"]))

    def done(self, thread):
        thread.join(3)
        self.assertFalse(thread.is_alive())
        return json.loads((self.runtime / "handoff.json").read_bytes())

    def test_web_start_center_two_manual_limits_and_protected_return(self):
        thread, response = self.launch()
        self.assertEqual((response["ok"], response["phase"]), (True, "ready"))
        self.assertEqual(self.hw.calls, [])
        duplicate, busy = self.launch()
        self.assertFalse(busy["ok"])
        duplicate.join(1)
        self.assertEqual(self.operate(response, "center")["phase"], "centered")
        self.hw.registers["Present_Position"] = 3140
        self.assertEqual(self.operate(response, "sample1")["sample1"], 3140)
        self.hw.registers["Present_Position"] = 2047
        self.assertTrue(self.operate(response, "status")["ok"])
        self.hw.registers["Present_Position"] = 981
        self.assertEqual(self.operate(response, "sample2")["sample2"], 981)
        self.assertEqual(self.operate(response, "save")["phase"], "saved")
        final = self.done(thread)
        self.assertEqual((final["phase"], final["calibration_result"]), ("returned", "saved"))
        self.assertEqual(self.hw.calls, ["center", "write"])
        result = json.loads(self.calibration.read_bytes())
        self.assertEqual((result[self.joint]["range_min"], result[self.joint]["range_max"]), (981, 3140))
        self.assertEqual(result["right_arm_gripper"], self.others["right_arm_gripper"])
        self.assertEqual([a[3:] for a in self.events if a[:2] == ("sudo", "-n")], [
            ("stop", "robot-s1-bus2.service", "robot-s0-bus2.service"),
            ("start", "robot-s0-bus2.service"), ("start", "robot-s1-bus2.service")])
        self.assertEqual([a[2] for a in self.events if a[0] == "/test/robot-s0"], ["revoke", "authorize"])

    def test_active_motion_prevents_stop_and_writes(self):
        self.running = False
        thread, response = self.launch()
        self.assertFalse(response["ok"])
        self.assertEqual(self.done(thread)["phase"], "failed")
        self.assertEqual(self.events, [])
        self.assertEqual(self.hw.calls, [])

    def test_uncertain_service_state_does_not_acquire_or_authorize(self):
        self.states["robot-s0-bus2.service"] = "inactive"
        thread, response = self.launch()
        self.assertFalse(response["ok"])
        self.assertEqual(self.done(thread)["phase"], "failed")
        self.assertEqual(self.events, [])

    def test_return_failure_reports_saved_separately_and_revokes_unused_trust(self):
        thread, response = self.launch()
        self.error = "保护待机启动失败"
        self.operate(response, "center")
        self.hw.registers["Present_Position"] = 2600
        self.operate(response, "sample1")
        self.hw.registers["Present_Position"] = 1450
        self.operate(response, "sample2")
        self.assertEqual(self.operate(response, "save")["phase"], "saved")
        final = self.done(thread)
        self.assertEqual(final["phase"], "failed")
        self.assertIn(self.error, final["error"])
        self.assertEqual(final["calibration_result"], "saved")
        self.assertEqual(self.trust, b"1\n")
        self.assertEqual(self.hw.calls, ["center", "write"])

    def test_reopening_interrupted_session_restores_without_replaying_center(self):
        initial = m.cal.Session("bus2", self.joint, 5, self.calibration, self.hw, self.state, 0.031)
        initial.command("center", initial.data["session"])
        self.states["robot-s0-bus2.service"] = self.states["robot-s1-bus2.service"] = "inactive"
        thread, recovered = self.launch()
        self.assertEqual(recovered["phase"], "recovery")
        self.assertFalse(self.operate(recovered, "center")["ok"])
        self.assertEqual(self.operate(recovered, "restore")["phase"], "restored")
        self.assertEqual(self.done(thread)["phase"], "returned")
        self.assertEqual(self.hw.calls, ["center", "write"])
        self.assertFalse(any("stop" in command for command in self.events))

    def test_other_pending_joint_is_not_handed_off(self):
        (self.state / "bus1.pending.json").write_text('{"joint":"head_motor_1"}')
        thread, response = self.launch()
        self.assertFalse(response["ok"])
        self.done(thread)
        self.assertEqual(self.events, [])


class StopTests(unittest.TestCase):
    def test_torque_and_velocity_must_be_zero_on_position_joint(self):
        for register in m.STOP_REGS:
            hw = SimpleNamespace(bus=SimpleNamespace(read=lambda key, _, **_kw: int(key == register)))
            with self.assertRaises(ValueError):
                m.stopped_bus(hw, {"right_arm_elbow_flex": 3})

    def test_wheel_noise_uses_position_stability_and_deadline(self):
        clock = [0]
        counts = []
        def read(key, _, **_kw):
            counts.append(key)
            return {"Present_Position": 1810, "Present_Velocity": -50}.get(key, 0)
        with patch.object(m.time, "monotonic_ns", lambda: clock[0]), patch.object(
                m.time, "sleep", lambda sec: clock.__setitem__(0, clock[0] + int(sec * 1e9))):
            m.stopped_bus(SimpleNamespace(bus=SimpleNamespace(read=read)), {"base_left_wheel": 9})
        self.assertEqual(clock, [1_000_000_000])
        self.assertEqual(counts.count("Present_Position"), 6)
