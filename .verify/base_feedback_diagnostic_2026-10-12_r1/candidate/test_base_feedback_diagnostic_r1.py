"""爸爸，右9反馈诊断的离线内存合同；无SSH、串口、真实GO或控制工具。"""

import contextlib
import copy
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
ROOT = next((p for p in HERE.parents if (p / "scripts/run_hardware_maintenance.py").is_file()), None)
MAINTENANCE = (ROOT / "scripts/run_hardware_maintenance.py" if ROOT is not None
               else HERE / "fixtures/run_hardware_maintenance.py")
BINDINGS = (ROOT / ".verify/base_zero_diagnostic_2026-10-12_r4/native-preparation/maintenance_bindings_base_zero_diagnostic_r4.json"
            if ROOT is not None else HERE / "fixtures/maintenance_bindings_base_zero_diagnostic_r4.json")
HELPER = (ROOT / ".verify/base_zero_diagnostic_2026-10-12_r1/candidate/raw_right9_from_r14_r1.py"
          if ROOT is not None else HERE / "fixtures/raw_right9_from_r14_r1.py")
REGISTERS = {
    "Model_Number": (3, 2),
    "Firmware_Major_Version": (0, 1),
    "Firmware_Minor_Version": (1, 1),
    "Operating_Mode": (33, 1),
    "Torque_Enable": (40, 1),
    "Goal_Velocity": (46, 2),
    "Present_Position": (56, 2),
    "Present_Velocity": (58, 2),
    "Phase": (18, 1),
    "Lock": (55, 1),
}
EXPECTED_VALUES = {
    "Torque_Enable": 0,
    "Present_Velocity": 0,
    "Goal_Velocity": 0,
    "Model_Number": 777,
    "Firmware_Major_Version": 3,
    "Firmware_Minor_Version": 10,
    "Operating_Mode": 1,
    "Phase": 12,
    "Lock": 1,
    "Present_Position": 2087,
}


def load_module(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


c = load_module(HERE / "base_feedback_diagnostic_r1.py", "feedback_diag_candidate_memory")
h = load_module(HELPER, "feedback_diag_raw_helper_memory")


def frame(register, value, device):
    """独立构造协议原帧，字节小端且不解码符号或物理单位。"""
    address, width = REGISTERS[register]
    tx = [255, 255, 254, 5, 130, address, width, 9]
    tx.append((~sum(tx[2:])) & 255)
    rx = [255, 255, 9, width + 2, 0, *int(value).to_bytes(width, "little")]
    rx.append((~sum(rx[2:])) & 255)
    return {
        "operation": "sync_read", "serial_device": device, "motor_id": 9,
        "register": register, "requested_register_address": address, "width": width,
        "raw_unsigned": value, "tx": tx, "rx": rx,
    }


class FakeClock:
    """只推进内存整数；不等待真实时钟。"""

    def __init__(self):
        self.current_ns = 0
        self.sleeps = []
        self.oversleep_ns = 0
        self.now_step_ns = 0

    def now(self):
        value = self.current_ns
        self.current_ns += self.now_step_ns
        return value

    def advance(self, ns):
        self.current_ns += ns

    def sleep(self, seconds, cleanup=False):
        amount = round(seconds * 1_000_000_000)
        self.sleeps.append((self.current_ns, amount, cleanup))
        self.advance(amount + self.oversleep_ns)


class MemoryIO:
    """按真实具名命令返回帧；所有调用与失败均只保存在内存。"""

    def __init__(self, maintenance, *, replacements=None, latencies=None, transform=None,
                 read_error=None, ordinary_fault=None):
        self.m = maintenance
        self.tool = h.TOOL
        self.clock = FakeClock()
        self.sequence = 0
        self.commands = []
        self.events = []
        self.values = dict(EXPECTED_VALUES)
        self.replacements = replacements or {}
        self.latencies = latencies or {}
        self.transform = transform
        self.read_error = read_error
        self.ordinary_fault = ordinary_fault
        self.read_counts = {}
        self.evidence_errors = []
        self.child = None

    def now(self):
        return self.clock.now()

    def sleep(self, seconds, cleanup=False):
        self.clock.sleep(seconds, cleanup)

    def command(self, argv, **kwargs):
        self.sequence += 1
        started = self.now()
        event = {"argv": list(argv), "kwargs": dict(kwargs), "started_ns": started}
        self.events.append(event)
        operation, register = argv[3:5]
        key = (register, self.read_counts.get(register, 0))
        self.read_counts[register] = key[1] + 1
        self.clock.advance(self.latencies.get(key, 1_000_000))
        event["finished_ns"] = self.now()
        if operation == "write":
            raise AssertionError("内存替身不允许任何WRITE到达")
        if self.read_error == key:
            raise OSError("MEMORY_ONLY_READ_EXCEPTION")
        value = self.replacements.get(key, self.values.get(register, 0))
        if operation == "read-raw":
            row = frame(register, value, self.m.DEVICES["right"])
            if self.transform is not None:
                self.transform(row, key)
            text = json.dumps(row) + "\n"
        elif operation == "read":
            text = ("MEMORY_ONLY_BAD_STOP_STDOUT\n" if self.ordinary_fault == register
                    else f"read {register}@{argv[2]} = {value}\n")
        else:
            raise AssertionError("内存替身只接受具名READ")
        encoded = text.encode()
        self.commands.append({
            "stdout_bytes": len(encoded), "stdout_sha256": hashlib.sha256(encoded).hexdigest(),
            "started_ns": started, "finished_ns": self.now(),
        })
        return text

    def poke(self, side, motor, action, register, value=None, cleanup=False):
        argv = [h.TOOL["path"], self.m.DEVICES[side], str(motor), action, register]
        if action == "write":
            argv.append(str(value))
        text = self.command(argv, cleanup=cleanup)
        if action == "read":
            return int(text.split(" = ")[1])
        raise AssertionError("内存替身不允许任何WRITE到达")


class FeedbackCases(unittest.TestCase):
    def setUp(self):
        self.m = load_module(MAINTENANCE, "feedback_original_fresh_memory")
        c.prepare_helper(h)
        self.bindings = json.loads(BINDINGS.read_text())
        self.bindings["refs"] += [c.reference(Path(c.__file__)), c.reference(HELPER)]
        self.plan = c.make_plan(
            self.m, h, self.bindings,
            "/home/loosqk/robot-checks/MEMORY_ONLY/feedback-run",
            "/home/loosqk/robot-checks/MEMORY_ONLY",
            c.reference(Path(c.__file__)), c.reference(HELPER),
        )

    def engine(self, **faults):
        executor = MemoryIO(self.m, **faults)
        return self.m.ProbeEngine(executor, copy.deepcopy(self.plan))

    def run_diag(self, engine):
        with patch.object(self.m, "verify_ref"), contextlib.redirect_stdout(io.StringIO()):
            return c.run_feedback_diag(self.m, h, engine)

    def assert_blocked(self, engine):
        self.assertTrue(engine.manual_cut)
        self.assertTrue(engine.feedback_diag["uart_blocked"])
        count = len(engine.io.events)
        for argv in (
            [h.RAW_TOOL["path"], self.m.DEVICES["right"], "9", "read-raw", "Present_Position"],
            [h.TOOL["path"], self.m.DEVICES["left"], "1", "read", "Torque_Enable"],
            [h.TOOL["path"], self.m.DEVICES["right"], "9", "write", "Torque_Enable", "0"],
        ):
            with self.subTest(argv=argv), self.assertRaises(ValueError):
                engine.io.command(argv, cleanup=True)
        self.assertEqual(len(engine.io.events), count)

    def memory_go_fixture(self):
        """仅返回内存反例字典，不签发、写盘或传递任何真实授权。"""
        now = 1_000_000_000_000
        facts = {key: True for key in self.m.FACTS}
        facts["confirmed_at_ns"] = now
        physical = {"left": dict(facts), "right": dict(facts)}
        physical["right"].update(
            all_three_wheels_off_ground=True, base_stably_supported=True,
            base_confirmation_kind="CURRENT_SETUP_READY",
            base_confirmation_answer="MEMORY_ONLY 当前就绪夹具",
            base_confirmation_scope=c.FIELD_SCOPE,
        )
        return now, {
            "schema": "MAINTENANCE_GO_V2", "issued_by": "root", "issued_at_ns": now,
            "expires_at_ns": now + c.TTL_NS, "plan_sha256": self.m.digest(self.plan),
            "artifact_root": self.plan["artifact_root"], "host": self.bindings["host"],
            "source": "MEMORY_ONLY 非真实授权", "physical": physical,
        }

    def test_default_plan_has_zero_actions_before_loading(self):
        with patch.object(c, "load_frozen", side_effect=AssertionError("必须零I/O")), \
                contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(c.main([]), 0)
        result = json.loads(output.getvalue())
        self.assertEqual(result["status"], "PLAN_ONLY")
        self.assertEqual(result["actions_started"], 0)
        self.assertEqual(result["register_writes"], 0)
        self.assertEqual(result["motion_commands"], 0)
        self.assertFalse(result["new_GO"])

    def test_execute_without_complete_inputs_refuses_before_loading(self):
        with patch.object(c, "load_frozen", side_effect=AssertionError("必须零I/O")):
            with self.assertRaises(ValueError):
                c.main(["--execute"])

    def test_execute_without_go_refuses_before_loading(self):
        argv = [
            "--execute", "--maintenance", "/MEMORY_ONLY/m", "--raw-helper", "/MEMORY_ONLY/h",
            "--bindings", "/MEMORY_ONLY/b", "--output", "/MEMORY_ONLY/o",
            "--artifact-root", "/MEMORY_ONLY",
        ]
        with patch.object(c, "load_frozen", side_effect=AssertionError("必须零I/O")):
            with self.assertRaises(ValueError):
                c.main(argv)

    def test_plan_keeps_three_wheels_deferred_and_firmware_unverified(self):
        self.assertEqual(c.validate_plan(self.m, self.plan), [])
        self.assertEqual(self.plan["position_targets"], [])
        self.assertFalse(self.plan["firmware_contract_verified"])
        self.assertEqual(len(self.plan["base_targets"]), 3)
        self.assertTrue(all(row["decision"] == "DEFERRED" for row in self.plan["base_targets"]))
        for key, changed in (
            ("position_targets", [["left", 8, "head_motor_2"]]),
            ("firmware_contract_verified", True),
            ("run_id", "base-zero-diagnostic-r4"),
        ):
            bad = copy.deepcopy(self.plan)
            bad[key] = changed
            with self.subTest(key=key), self.assertRaises(ValueError):
                c.validate_plan(self.m, bad)

    def test_old_go_or_different_scope_is_rejected(self):
        c.install(self.m, h)
        now, fixture = self.memory_go_fixture()
        self.m.validate_go(fixture, self.plan, now)
        for section, key, value in (
            (None, "plan_sha256", "OLD_ZERO_DIAGNOSTIC_GO"),
            (None, "schema", "MAINTENANCE_GO_V1"),
            (None, "expires_at_ns", now),
            ("right", "base_confirmation_scope", "旧Goal0→Enable动作范围"),
            ("right", "all_three_wheels_off_ground", False),
            ("right", "base_stably_supported", False),
            ("right", "base_confirmation_kind", "CAPABILITY_ONLY"),
            ("right", "confirmed_at_ns", now - c.TTL_NS - 1),
        ):
            bad = copy.deepcopy(fixture)
            if section is None:
                bad[key] = value
            else:
                bad["physical"][section][key] = value
            with self.subTest(section=section, key=key), self.assertRaises(ValueError):
                self.m.validate_go(bad, self.plan, now)
        for phase in ("UART前过期", "READ完成后过期"):
            with self.subTest(expiry_phase=phase):
                self.setUp()
                now, current = self.memory_go_fixture()
                c.install(self.m, h, current)
                engine = self.engine()
                samples = []
                time_values = ([current["expires_at_ns"]] if phase == "UART前过期"
                               else [now, current["expires_at_ns"]])
                with patch.object(c.time, "time_ns", side_effect=time_values), \
                        patch.object(self.m, "verify_ref"), contextlib.redirect_stdout(io.StringIO()), \
                        self.assertRaises(ValueError):
                    c.timed_raw(self.m, h, engine, "Present_Position", samples)
                self.assertEqual(len(samples), 1)
                self.assertTrue(callable(engine.feedback_go_check))
                if phase == "UART前过期":
                    self.assertEqual(engine.io.events, [])
                    self.assertEqual(engine.io.sequence, 0)
                    self.assertEqual(samples[0]["status"], "UNKNOWN")
                    for field in ("raw", "command_sequence", "stdout_bytes", "stdout_sha256"):
                        self.assertNotIn(field, samples[0])
                else:
                    self.assertEqual(len(engine.io.events), 1)
                    self.assertEqual(samples[0]["raw"], frame("Present_Position", 2087, self.m.DEVICES["right"]))
                    self.assertEqual(samples[0]["status"], "FRAME_VALID_REJECTED")
                    self.assertEqual(samples[0]["command_sequence"], 1)
                    self.assertEqual(samples[0]["stdout_sha256"], engine.io.commands[0]["stdout_sha256"])
                self.assert_blocked(engine)

    def install_with_original_snapshot_finish(self):
        """沿真实17电机snapshot读取；身份后验留UNKNOWN，不接触系统。"""
        def original_finish(report, executor, engine, bindings, helper):
            values, errors = self.m.snapshot(executor, cleanup=True)
            report.update(
                after=values, after_errors=errors,
                stop_verification=self.m.classify_stop_snapshot(values, errors),
                identity_verification={"status": "UNKNOWN"},
            )
        self.m.finish_verification = original_finish
        c.install(self.m, h)

    def assert_final_snapshot_has_zero_uart(self, engine):
        count = len(engine.io.events)
        report = {"plan": self.plan, "probes": []}
        with patch.object(self.m, "snapshot", wraps=self.m.snapshot) as snapshot, \
                contextlib.redirect_stdout(io.StringIO()):
            self.m.finish_verification(report, engine.io, engine, self.bindings, None)
        snapshot.assert_called_once_with(engine.io, cleanup=True)
        self.assertEqual(len(engine.io.events), count)
        self.assertEqual(report["stop_verification"]["status"], "UNKNOWN")
        self.assertEqual(set(report["after"]), set(self.m.MOTOR_KEYS))
        self.assertEqual(len(report["after_errors"]), 17 * len(self.m.REGS))
        self.assertEqual(self.m.classify_run_result(report), "NEEDS_MANUAL_POWER_CUT")
        self.assert_blocked(engine)

    def test_constructor_installs_write_guard_before_first_probe(self):
        c.install(self.m, h)
        engine = self.engine()
        for register, value in (("Torque_Enable", 1), ("Torque_Enable", 0), ("Goal_Velocity", 0)):
            with self.subTest(register=register, value=value), self.assertRaises(ValueError):
                engine.write("right", 9, register, value)
        self.assertEqual(engine.io.events, [])

    def test_initial_ordinary_stop_nonzero_is_sticky_and_final_snapshot_has_zero_uart(self):
        for register, value in (("Torque_Enable", 1), ("Present_Velocity", 50), ("Present_Velocity", -50), ("Goal_Velocity", 1)):
            with self.subTest(register=register, value=value):
                self.setUp()
                self.install_with_original_snapshot_finish()
                engine = self.engine(replacements={(register, 0): value})
                try:
                    with self.assertRaises(ValueError), contextlib.redirect_stdout(io.StringIO()):
                        engine.read("right", 9, register)
                finally:
                    self.assert_final_snapshot_has_zero_uart(engine)
                self.assertEqual(len(engine.io.events), 1)

    def test_initial_ordinary_stdout_identity_unknown_is_sticky_and_final_snapshot_has_zero_uart(self):
        self.install_with_original_snapshot_finish()
        engine = self.engine(ordinary_fault="Present_Velocity")
        try:
            with self.assertRaises(ValueError), contextlib.redirect_stdout(io.StringIO()):
                engine.read("right", 9, "Present_Velocity")
        finally:
            self.assert_final_snapshot_has_zero_uart(engine)
        self.assertEqual(len(engine.io.events), 1)

    def test_bad_checksum_or_id_blocks_finally_snapshot_uart(self):
        mutations = {
            "checksum": lambda row: row["rx"].__setitem__(-1, row["rx"][-1] ^ 1),
            "identity": lambda row: row.__setitem__("motor_id", 8),
        }
        for kind, mutation in mutations.items():
            with self.subTest(kind=kind):
                self.setUp()
                self.install_with_original_snapshot_finish()
                def transform(row, key, mutation=mutation):
                    if key == ("Present_Position", 0):
                        mutation(row)
                engine = self.engine(transform=transform)
                try:
                    with self.assertRaises(ValueError):
                        self.run_diag(engine)
                finally:
                    self.assert_final_snapshot_has_zero_uart(engine)

    def test_success_has_three_te_pp_pv_groups_at_original_offsets(self):
        engine = self.engine()
        self.run_diag(engine)
        row = engine.feedback_diag
        self.assertEqual(row["status"], "PASS")
        self.assertFalse(engine.manual_cut)
        observation = row["observation"]
        samples = observation["groups"]
        self.assertEqual(len(samples), 3)
        self.assertEqual(
            [s["reads"][0]["started_ns"] - observation["started_ns"] for s in samples],
            [0, 200_000_000, 400_000_000],
        )
        self.assertEqual(observation["deadline_ns"], observation["started_ns"] + 500_000_000)
        self.assertFalse(row["firmware_contract_verified"])
        self.assertTrue(all(event["argv"][3] != "write" for event in engine.io.events))
        with self.subTest(clock_shape="每次now自然推进1ns"):
            progressing = self.engine()
            progressing.io.clock.now_step_ns = 1
            self.run_diag(progressing)
            actual = progressing.feedback_diag["observation"]
            groups = actual["groups"]
            self.assertEqual(progressing.feedback_diag["status"], "PASS")
            self.assertEqual(len(groups), 3)
            self.assertEqual([group["requested_offset_ns"] for group in groups], [0, 200_000_000, 400_000_000])
            self.assertEqual([group["scheduled_ns"] - actual["started_ns"] for group in groups], [0, 200_000_000, 400_000_000])
            self.assertGreater(groups[0]["started_ns"], actual["started_ns"])
            self.assertTrue(all(group["request_lateness_ns"] == group["started_ns"] - group["scheduled_ns"] for group in groups))
            self.assertEqual(actual["deadline_ns"], actual["started_ns"] + 500_000_000)
            self.assertTrue(all(sample["started_ns"] < actual["deadline_ns"]
                                and sample["finished_ns"] <= actual["deadline_ns"]
                                and sample["completed_in_budget"]
                                for group in groups for sample in group["reads"]))
            self.assertTrue(all(event["argv"][3] != "write" for event in progressing.io.events))
        with self.subTest(gate_shape="完整gate只在组窗外耗时一次"):
            gated = self.engine()
            gate_calls = []
            def costly_gate():
                gate_calls.append(gated.io.now())
                gated.io.clock.advance(250_000_000)
            gated.check_gate = costly_gate
            self.run_diag(gated)
            actual = gated.feedback_diag["observation"]
            self.assertEqual(len(gate_calls), 1)
            self.assertEqual(gate_calls[0], gated.feedback_diag["preflight"][-1]["finished_ns"])
            self.assertEqual(actual["started_ns"], gate_calls[0] + 250_000_000)
            self.assertEqual([group["reads"][0]["started_ns"] - actual["started_ns"]
                              for group in actual["groups"]], [0, 200_000_000, 400_000_000])
            self.assertEqual(actual["deadline_ns"], actual["started_ns"] + 500_000_000)
            self.assertEqual(gated.feedback_diag["status"], "PASS")

    def test_raw_group_retains_complete_frames_and_request_completion_times(self):
        engine = self.engine(replacements={
            ("Present_Position", 0): 2087, ("Present_Position", 1): 2088,
            ("Present_Position", 2): 2090,
        })
        self.run_diag(engine)
        groups = engine.feedback_diag["observation"]["groups"]
        previous = -1
        for group, expected_pp in zip(groups, (2087, 2088, 2090)):
            records = group["reads"]
            self.assertEqual([s["register"] for s in records], ["Torque_Enable", "Present_Position", "Present_Velocity"])
            for sample in records:
                self.assertGreaterEqual(sample["started_ns"], previous)
                self.assertGreater(sample["finished_ns"], sample["started_ns"])
                previous = sample["finished_ns"]
                raw = sample["raw"]
                self.assertEqual(raw, frame(sample["register"], raw["raw_unsigned"], self.m.DEVICES["right"]))
            self.assertEqual(records[1]["raw"]["raw_unsigned"], expected_pp)

    def test_pv_0032_immediately_blocks_and_preserves_same_group_pp(self):
        self.assert_nonzero_pv_blocks(0x0032)

    def test_pv_8032_immediately_blocks_and_preserves_same_group_pp(self):
        self.assert_nonzero_pv_blocks(0x8032)

    def assert_nonzero_pv_blocks(self, raw):
        engine = self.engine(replacements={
            ("Present_Position", 0): 3401, ("Present_Velocity", 0): raw,
        })
        with self.assertRaises(ValueError):
            self.run_diag(engine)
        groups = engine.feedback_diag["observation"]["groups"]
        self.assertEqual(len(groups), 1)
        records = groups[0]["reads"]
        self.assertEqual([s["register"] for s in records], ["Torque_Enable", "Present_Position", "Present_Velocity"])
        self.assertEqual(records[1]["raw"]["raw_unsigned"], 3401)
        self.assertEqual(records[2]["raw"]["raw_unsigned"], raw)
        self.assertEqual(records[2]["status"], "FRAME_VALID_REJECTED")
        self.assertEqual(records[2]["raw"]["rx"][5:7], list(raw.to_bytes(2, "little")))
        self.assertEqual(engine.io.events[-1]["argv"][-1], "Present_Velocity")
        self.assert_blocked(engine)

    def test_first_group_nonzero_te_is_sticky_before_pp_observation(self):
        engine = self.engine(replacements={("Torque_Enable", 0): 1})
        with self.assertRaises(ValueError):
            self.run_diag(engine)
        self.assertEqual(engine.io.read_counts.get("Present_Position", 0), 0)
        self.assert_blocked(engine)

    def test_observation_nonzero_te_stops_before_same_group_pp_or_pv(self):
        engine = self.engine(replacements={("Torque_Enable", 1): 1})
        with self.assertRaises(ValueError):
            self.run_diag(engine)
        self.assertEqual(engine.io.read_counts.get("Present_Position", 0), 1)
        self.assertEqual(engine.io.read_counts.get("Present_Velocity", 0), 1)
        groups = engine.feedback_diag["observation"]["groups"]
        self.assertEqual(len(groups), 2)
        self.assertEqual([s["register"] for s in groups[-1]["reads"]], ["Torque_Enable"])
        self.assert_blocked(engine)

    def test_pp_keeps_unsigned_two_byte_word_without_physical_inference(self):
        engine = self.engine(replacements={("Present_Position", 0): 0x8032})
        self.run_diag(engine)
        sample = engine.feedback_diag["observation"]["groups"][0]["reads"][1]
        self.assertEqual(sample["raw"]["raw_unsigned"], 0x8032)
        self.assertEqual(sample["raw"]["rx"][5:7], [0x32, 0x80])
        self.assertEqual(engine.feedback_diag["status"], "PASS")

    def test_all_fixed_metadata_mismatches_block(self):
        for register in EXPECTED_VALUES:
            if register in ("Present_Position", "Present_Velocity"):
                continue
            engine = self.engine(replacements={(register, 0): EXPECTED_VALUES[register] + 1})
            with self.subTest(register=register), self.assertRaises(ValueError):
                self.run_diag(engine)
            self.assert_blocked(engine)

    def test_bad_raw_frame_field_blocks_at_the_original_read(self):
        mutations = {
            "checksum": lambda row: row["rx"].__setitem__(-1, row["rx"][-1] ^ 1),
            "tx_checksum": lambda row: row["tx"].__setitem__(-1, row["tx"][-1] ^ 1),
            "motor_id": lambda row: row.__setitem__("motor_id", 8),
            "device": lambda row: row.__setitem__("serial_device", self.m.DEVICES["left"]),
            "address": lambda row: row.__setitem__("requested_register_address", 59),
            "width": lambda row: row.__setitem__("width", 1),
            "rx_header": lambda row: row["rx"].__setitem__(0, 254),
            "extra_byte": lambda row: row["rx"].append(0),
            "error_status": lambda row: row["rx"].__setitem__(4, 1),
            "raw_value": lambda row: row.__setitem__("raw_unsigned", 1),
            "extra_field": lambda row: row.__setitem__("untrusted_extra", True),
        }
        for kind, mutation in mutations.items():
            def transform(row, key, mutation=mutation):
                if key == ("Present_Position", 0):
                    mutation(row)
            engine = self.engine(transform=transform)
            with self.subTest(kind=kind), self.assertRaises(ValueError):
                self.run_diag(engine)
            self.assertEqual(engine.io.events[-1]["argv"][-1], "Present_Position")
            self.assert_blocked(engine)

    def test_raw_bool_value_does_not_pass_integer_zero(self):
        def transform(row, key):
            if key == ("Torque_Enable", 0):
                row["raw_unsigned"] = False
        engine = self.engine(transform=transform)
        with self.assertRaises(ValueError):
            self.run_diag(engine)
        self.assert_blocked(engine)

    def test_boolean_metadata_or_tx_byte_does_not_pass_integer_one(self):
        mutations = {
            "address": lambda row: row.__setitem__("requested_register_address", True),
            "width": lambda row: row.__setitem__("width", True),
            "tx_width": lambda row: row["tx"].__setitem__(6, True),
        }
        for kind, mutation in mutations.items():
            def transform(row, key, mutation=mutation):
                if key == ("Firmware_Minor_Version", 0):
                    mutation(row)
            engine = self.engine(transform=transform)
            with self.subTest(kind=kind), self.assertRaises(ValueError):
                self.run_diag(engine)
            self.assert_blocked(engine)

    def test_read_exception_latches_without_retry_or_fallback(self):
        for failed_read in (("Present_Position", 0), ("Present_Velocity", 0)):
            engine = self.engine(read_error=failed_read)
            with self.subTest(failed_read=failed_read), self.assertRaises(OSError):
                self.run_diag(engine)
            self.assertEqual(engine.io.events[-1]["argv"][-1], failed_read[0])
            self.assert_blocked(engine)

    def test_missed_second_slot_is_not_retimed_or_retried(self):
        engine = self.engine(latencies={("Present_Velocity", 0): 201_000_000})
        with self.assertRaises(ValueError):
            self.run_diag(engine)
        self.assertEqual(len(engine.feedback_diag["observation"]["groups"]), 1)
        self.assertEqual(engine.io.read_counts["Present_Velocity"], 1)
        self.assert_blocked(engine)
        self.assert_unstarted_read_has_no_previous_stdout(engine)

    def assert_unstarted_read_has_no_previous_stdout(self, engine, deadline=None):
        """被封锁或期限已到的请求不能冒领上一条stdout及sequence。"""
        self.assertTrue(engine.io.commands)
        count, sequence = len(engine.io.events), engine.io.sequence
        samples = []
        with patch.object(self.m, "verify_ref"), contextlib.redirect_stdout(io.StringIO()), \
                self.assertRaises(ValueError):
            c.timed_raw(self.m, h, engine, "Present_Position", samples, deadline)
        self.assertEqual(len(engine.io.events), count)
        self.assertEqual(engine.io.sequence, sequence)
        self.assertEqual(len(samples), 1)
        self.assertEqual(samples[0]["status"], "UNKNOWN")
        for field in ("raw", "command_sequence", "stdout_bytes", "stdout_sha256"):
            self.assertNotIn(field, samples[0])

    def test_completion_at_500ms_remains_inside_original_window(self):
        engine = self.engine(latencies={("Present_Velocity", 2): 98_000_000})
        self.run_diag(engine)
        observation = engine.feedback_diag["observation"]
        self.assertEqual(len(observation["groups"]), 3)
        self.assertEqual(engine.io.events[-1]["finished_ns"], observation["deadline_ns"])
        self.assertEqual(engine.feedback_diag["status"], "PASS")
        self.assertFalse(engine.manual_cut)

    def test_completion_after_500ms_is_outside_original_window(self):
        engine = self.engine(latencies={("Present_Velocity", 2): 99_000_000})
        with self.assertRaises(ValueError):
            self.run_diag(engine)
        observation = engine.feedback_diag["observation"]
        self.assertGreater(engine.io.events[-1]["finished_ns"], observation["deadline_ns"])
        self.assert_blocked(engine)
        self.assert_unstarted_read_has_no_previous_stdout(engine, engine.io.now())

    def test_oversleep_cannot_shift_original_sample_schedule(self):
        engine = self.engine()
        engine.io.clock.oversleep_ns = 200_000_001
        with self.assertRaises(ValueError):
            self.run_diag(engine)
        self.assertEqual(len(engine.feedback_diag["observation"]["groups"]), 1)
        self.assert_blocked(engine)

    def test_guard_rejects_every_write_enable_and_non_target_raw(self):
        engine = self.engine()
        self.run_diag(engine)
        for argv in (
            [h.TOOL["path"], self.m.DEVICES["right"], "9", "write", "Torque_Enable", "1"],
            [h.TOOL["path"], self.m.DEVICES["right"], "9", "write", "Torque_Enable", "0"],
            [h.TOOL["path"], self.m.DEVICES["right"], "9", "write", "Goal_Velocity", "0"],
            [h.RAW_TOOL["path"], self.m.DEVICES["right"], "9", "write", "Phase", "0"],
            [h.RAW_TOOL["path"], self.m.DEVICES["right"], "8", "read-raw", "Present_Position"],
            [h.RAW_TOOL["path"], self.m.DEVICES["left"], "9", "read-raw", "Present_Velocity"],
            [h.RAW_TOOL["path"], self.m.DEVICES["right"], "9", "read-raw", "P_Coefficient"],
            [h.STRACE["path"], h.TOOL["path"], self.m.DEVICES["right"], "9", "write", "Torque_Enable", "1"],
        ):
            count = len(engine.io.events)
            with self.subTest(argv=argv), self.assertRaises(ValueError):
                engine.io.command(argv)
            self.assertEqual(len(engine.io.events), count)

    def test_original_other_motor_stop_reads_remain_read_only(self):
        engine = self.engine()
        self.run_diag(engine)
        for side, motor in (("left", 1), ("left", 8), ("right", 1), ("right", 8), ("right", 9)):
            for register in ("Torque_Enable", "Present_Velocity", "Goal_Velocity"):
                text = engine.io.command([h.TOOL["path"], self.m.DEVICES[side], str(motor), "read", register], cleanup=True)
                self.assertEqual(text, f"read {register}@{motor} = 0\n")
        self.assertTrue(all(event["argv"][3] != "write" for event in engine.io.events))

    def test_unknown_original_stop_read_is_sticky(self):
        engine = self.engine(ordinary_fault="Present_Velocity")
        self.run_diag(engine)
        with self.assertRaises(ValueError), contextlib.redirect_stdout(io.StringIO()):
            engine.io.command([h.TOOL["path"], self.m.DEVICES["left"], "1", "read", "Present_Velocity"], cleanup=True)
        self.assert_blocked(engine)

    def test_stop51_unknown_cannot_be_reported_pass_or_firmware_verified(self):
        def original_finish(report, executor, engine, bindings, helper):
            report.update(stop_verification={"status": "UNKNOWN"}, identity_verification={"status": "PASS"})
        self.m.finish_verification = original_finish
        c.install(self.m, h)
        engine = self.engine()
        self.run_diag(engine)
        report = {"plan": self.plan, "probes": [], "position_verification": {"status": "PASS"}}
        count = len(engine.io.events)
        with contextlib.redirect_stdout(io.StringIO()):
            self.m.finish_verification(report, engine.io, engine, self.bindings, None)
        self.assertEqual(report["stop_verification"]["status"], "UNKNOWN")
        self.assertEqual(self.m.classify_run_result(report), "NEEDS_MANUAL_POWER_CUT")
        self.assertFalse(report["feedback_diag"]["firmware_contract_verified"])
        self.assertEqual(len(engine.io.events), count)
        self.assert_blocked(engine)

    def test_three_deferred_rows_are_independent_of_diagnostic_pass(self):
        c.install(self.m, h)
        engine = self.engine()
        with patch.object(self.m.ProbeEngine, "wheel", side_effect=AssertionError("禁止wheel")), \
                patch.object(self.m.ProbeEngine, "pulse", side_effect=AssertionError("禁止pulse")), \
                self.assertRaises(ValueError):
            self.run_diag_via_motion(engine)
        self.assertEqual(engine.feedback_diag["status"], "PASS")
        self.assertEqual(len(engine.rows), 3)
        self.assertTrue(all(row["decision"] == "DEFERRED" and row["status"] == "NOT_RUN" for row in engine.rows))
        self.assertFalse(self.plan["firmware_contract_verified"])

    def run_diag_via_motion(self, engine):
        with patch.object(self.m, "verify_ref"), contextlib.redirect_stdout(io.StringIO()):
            return self.m.run_motion_probes(engine, self.bindings, self.plan)


if __name__ == "__main__":
    unittest.main(verbosity=2)
