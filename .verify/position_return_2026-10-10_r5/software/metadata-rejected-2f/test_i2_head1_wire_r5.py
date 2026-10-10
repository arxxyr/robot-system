"""爸爸，固定head1(left7) I2候选的最小内存对照；不执行工具或UART。"""
import importlib.util
import hashlib
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("i_trial_candidate", Path(__file__).with_name("i2_trial_head1_wire_r5.py"))
c = importlib.util.module_from_spec(spec)
spec.loader.exec_module(c)


class CandidateCases(unittest.TestCase):
    def setUp(self):
        self.values = dict(c.BACKUP)
        # 当前动态位置与旧备份不同仍允许；不追旧GP0/PP2642。
        self.values.update(Goal_Position=2648, Present_Position=2648)
        self.events = []
        self.io = SimpleNamespace(tool={"path": "position-tool"}, command=lambda argv, **kw: self.events.append(("command", argv)))
        self.engine = SimpleNamespace(io=self.io, rows=[], manual_cut=False, check_gate=lambda: self.events.append(("gate",)),
                                      plan={"position_delta_raw": 24, "return_tolerance_raw": 4, "position_wait_ms": 800})

    def read(self, _m, _engine, register, cleanup=False):
        self.events.append(("read", register, cleanup))
        return {"raw_unsigned": self.values[register]}

    def write(self, _m, _engine, value, cleanup=False):
        self.events.append(("write", value, cleanup))
        self.values["I_Coefficient"] = value

    def original(self, engine, side, motor, name, calibration):
        self.events.append(("original_position",))
        self.assertEqual(engine.plan, {"position_delta_raw": 24, "return_tolerance_raw": 4, "position_wait_ms": 800})
        self.assertEqual((side, motor, name), c.TARGET)
        engine.rows.append({"status": "PASS", "cleanup_errors": [], "position_accuracy": {"status": "PASS"}})

    def run_candidate(self, original=None, writer=None):
        return c.run_trial(None, self.engine, *c.TARGET, {}, original or self.original,
                           reader=self.read, writer=writer or self.write)

    def test_happy_restore_keeps_original_gates_and_fresh_dynamic_values(self):
        self.run_candidate()
        self.assertEqual(self.values["I_Coefficient"], 0)
        self.assertEqual([e for e in self.events if e[0] == "write"], [("write", 2, False), ("write", 0, True)])
        self.assertEqual(self.engine.rows[0]["i_trial"]["restore_status"], "PASS")

    def test_i2_ack_uncertain_still_restores_without_motion(self):
        def uncertain(_m, _engine, value, cleanup=False):
            self.write(_m, _engine, value, cleanup)
            if value == 2:
                raise ValueError("I2可能已生效但ACK失败")
        with self.assertRaisesRegex(ValueError, "ACK失败"):
            self.run_candidate(writer=uncertain)
        self.assertEqual(self.values["I_Coefficient"], 0)
        self.assertNotIn(("original_position",), self.events)
        self.assertEqual(self.engine.rows[0]["i_trial"]["restore_status"], "PASS")
        self.assertEqual(self.engine.rows[0]["status"], "FAIL")

    def test_accuracy_fail_stays_fail_after_confirmed_restore(self):
        def accuracy(engine, *args):
            self.original(engine, *args)
            engine.rows[-1]["status"] = "FAIL"
            engine.rows[-1]["position_accuracy"]["status"] = "FAIL"
            raise ValueError("POSITION_ACCURACY_TIMEOUT")
        with self.assertRaisesRegex(ValueError, "POSITION_ACCURACY_TIMEOUT"):
            self.run_candidate(original=accuracy)
        self.assertEqual(self.values["I_Coefficient"], 0)
        self.assertEqual(self.engine.rows[0]["position_accuracy"]["status"], "FAIL")
        self.assertFalse(self.engine.manual_cut)

    def test_original_stop_uncertain_blocks_restore_and_future_uart(self):
        def stop_unknown(engine, *args):
            self.original(engine, *args)
            engine.manual_cut = True
            raise ValueError("原safe_off失败")
        with self.assertRaisesRegex(ValueError, "safe_off失败"):
            self.run_candidate(original=stop_unknown)
        self.assertEqual([e for e in self.events if e[0] == "write"], [("write", 2, False)])
        self.assertFalse(any(e[0] == "read" and e[-1] is True for e in self.events))
        for tool in ("position-tool", c.RAW_TOOL["path"]):
            with self.assertRaisesRegex(ValueError, "粘性拒绝"):
                self.io.command([tool, "read"], cleanup=True)
        self.io.command(["systemctl", "show"], cleanup=True)

    def test_restore_ack_uncertain_blocks_future_uart(self):
        def restore_unknown(_m, _engine, value, cleanup=False):
            self.write(_m, _engine, value, cleanup)
            if value == 0:
                raise ValueError("I0恢复ACK失败")
        with self.assertRaisesRegex(ValueError, "I对照失败"):
            self.run_candidate(writer=restore_unknown)
        self.assertTrue(self.engine.manual_cut)
        with self.assertRaisesRegex(ValueError, "粘性拒绝"):
            self.io.command(["position-tool", "read"], cleanup=True)
        self.assertEqual(self.engine.rows[0]["status"], "FAIL")

    def test_entry_torque_nonzero_blocks_all_later_uart_without_i_write(self):
        self.values["Torque_Enable"] = 1
        with self.assertRaisesRegex(ValueError, "入口停止未确认"):
            self.run_candidate()
        self.assertEqual(self.events, [("gate",), ("read", "Torque_Enable", False)])
        self.assertTrue(self.engine.manual_cut)
        with self.assertRaisesRegex(ValueError, "粘性拒绝"):
            self.io.command(["position-tool", "read"], cleanup=True)

    def test_entry_velocity_read_unknown_blocks_all_later_uart_without_i_write(self):
        def unknown(m, engine, reg, cleanup=False):
            if reg == "Present_Velocity":
                raise ValueError("入口PV读回未知")
            return self.read(m, engine, reg, cleanup)
        with self.assertRaisesRegex(ValueError, "入口PV读回未知"):
            c.run_trial(None, self.engine, *c.TARGET, {}, self.original, reader=unknown, writer=self.write)
        self.assertFalse(any(e[0] in ("write", "original_position") for e in self.events))
        self.assertTrue(self.engine.manual_cut)
        with self.assertRaisesRegex(ValueError, "粘性拒绝"):
            self.io.command([c.RAW_TOOL["path"], "read"], cleanup=True)

    def test_restore_lock_zero_blocks_i0_write_and_future_uart(self):
        def lock_changed(engine, *args):
            self.original(engine, *args)
            self.values["Lock"] = 0
        with self.assertRaisesRegex(ValueError, "I对照失败"):
            self.run_candidate(original=lock_changed)
        self.assertEqual([e for e in self.events if e[0] == "write"], [("write", 2, False)])
        self.assertTrue(self.engine.manual_cut)
        with self.assertRaisesRegex(ValueError, "粘性拒绝"):
            self.io.command([c.RAW_TOOL["path"], "read"], cleanup=True)


class WireCaptureCases(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.events = []
        self.value = 0
        self.error = None
        self.trace = b'1791618190.0 +++ exited with 0 +++\n'
        events = self.events
        case = self

        class MemoryExecutor:
            def __init__(self):
                self.output = case.directory
                self.sequence = 0
                self.commands = []
                self.evidence_errors = []

            def command(self, argv, **kwargs):
                self.sequence += 1
                self.commands.append({'argv': list(argv), 'exit_code': 1 if case.error else 0, 'reaped': True})
                events.append(('command', argv, kwargs))
                if argv[0] == c.STRACE['path']:
                    Path(argv[argv.index('-o') + 1]).write_bytes(case.trace)
                if case.error:
                    raise case.error
                return f'read Present_Velocity@{argv[-3]} = {case.value}\n'

        self.m = SimpleNamespace(RealExecutor=MemoryExecutor,
                                 verify_ref=lambda ref, **kw: self.events.append(('verify', ref, kw)))
        c.install_stop_wire_capture(self.m)
        self.io = self.m.RealExecutor()

    def argv(self, motor=8):
        return [c.TOOL['path'], c.RIGHT_DEVICE, str(motor), 'read', 'Present_Velocity']

    def assert_wrapped(self, motor):
        original = self.argv(motor)
        text = self.io.command(original, cleanup=True, allowed=(0,), input_raw=None)
        self.assertEqual(text, f'read Present_Velocity@{motor} = 0\n')
        self.assertEqual(len(self.io.commands), 1)
        row = self.io.commands[0]
        trace = self.directory / f'right{motor}-normal-read-command_0001.strace'
        self.assertEqual(row['argv'], [c.STRACE['path'], *c.STRACE_FLAGS, '-o', str(trace), *original])
        self.assertEqual(row['ordinary_read_trace']['original_argv'], original)
        self.assertEqual(row['ordinary_read_trace']['sha256'], hashlib.sha256(self.trace).hexdigest())
        self.assertEqual(self.events[-1][2], {'cleanup': True, 'allowed': (0,), 'input_raw': None})
        self.assertTrue(row['reaped'])

    def test_only_exact_right8_read_gets_one_wrapped_command(self):
        self.assert_wrapped(8)

    def test_only_exact_right9_read_gets_one_wrapped_command(self):
        self.assert_wrapped(9)

    def test_nonmatching_commands_are_passed_unchanged_without_trace(self):
        variants = [self.argv(7), self.argv(8) + ['extra'],
                    [c.RAW_TOOL['path'], c.RIGHT_DEVICE, '8', 'read', 'Present_Velocity'],
                    [c.TOOL['path'], 'left-device', '8', 'read', 'Present_Velocity'],
                    [c.TOOL['path'], c.RIGHT_DEVICE, '8', 'write', 'Present_Velocity'],
                    [c.TOOL['path'], c.RIGHT_DEVICE, '8', 'read', 'Goal_Velocity'],
                    [c.TOOL['path'], c.RIGHT_DEVICE, '08', 'read', 'Present_Velocity']]
        for argv in variants:
            self.io.command(argv, cleanup=True)
            self.assertIs(self.events[-1][1], argv)
        self.assertEqual(len(self.events), len(variants))
        self.assertEqual(list(self.directory.iterdir()), [])

    def test_nonzero_stdout_keeps_original_stop_value(self):
        self.value = -50
        self.assertEqual(self.io.command(self.argv()), 'read Present_Velocity@8 = -50\n')
        self.assertEqual(len(self.io.commands), 1)

    def test_child_nonzero_exception_is_not_swallowed_or_replaced(self):
        self.error = ValueError('原child非零退出')
        self.trace = b'1791618190.0 +++ exited with 1 +++\n'
        with self.assertRaises(ValueError) as raised:
            self.io.command(self.argv(), cleanup=True)
        self.assertIs(raised.exception, self.error)
        self.assertEqual(len(self.io.commands), 1)
        self.assertEqual(self.io.commands[0]['ordinary_read_trace']['bytes'], len(self.trace))
        self.assertEqual(self.io.evidence_errors, [])

    def test_preexisting_trace_is_never_overwritten_and_command_not_started(self):
        trace = self.directory / 'right8-normal-read-command_0001.strace'
        trace.write_bytes('原件'.encode())
        with self.assertRaises(FileExistsError):
            self.io.command(self.argv())
        self.assertEqual(trace.read_bytes(), '原件'.encode())
        self.assertEqual(self.io.commands, [])

    def test_symlink_trace_is_never_followed_and_command_not_started(self):
        target = self.directory / '原件'
        target.write_bytes('原件'.encode())
        (self.directory / 'right8-normal-read-command_0001.strace').symlink_to(target)
        with self.assertRaises(FileExistsError):
            self.io.command(self.argv())
        self.assertEqual(target.read_bytes(), '原件'.encode())
        self.assertEqual(self.io.commands, [])

    def test_repeated_existing_reads_keep_separate_trace_files(self):
        self.io.command(self.argv())
        self.io.command(self.argv())
        self.assertEqual(len(self.io.commands), 2)
        self.assertEqual({p.name for p in self.directory.iterdir()},
                         {'right8-normal-read-command_0001.strace', 'right8-normal-read-command_0002.strace'})

    def test_trace_evidence_failure_remains_failure(self):
        self.trace = b'1791618190.0 read(3, "", 1) = 0\n'
        with self.assertRaisesRegex(ValueError, '成功终态'):
            self.io.command(self.argv())
        self.assertEqual(len(self.io.commands), 1)
        self.assertEqual(len(self.io.evidence_errors), 1)

    def test_strace_identity_failure_starts_no_command(self):
        def reject(_ref, **_kw):
            raise ValueError('strace SHA变化')
        self.m.verify_ref = reject
        with self.assertRaisesRegex(ValueError, 'SHA变化'):
            self.io.command(self.argv())
        self.assertEqual(self.io.commands, [])
        self.assertEqual(list(self.directory.iterdir()), [])


class EntryCases(unittest.TestCase):
    def test_default_plan_only_never_loads_or_executes(self):
        with patch.object(c, 'load_source', side_effect=AssertionError('禁止加载')):
            self.assertEqual(c.main([]), 0)

    def reject_missing_binding(self, missing):
        with tempfile.TemporaryDirectory() as folder:
            directory = Path(folder)
            source = Path(c.__file__).resolve()
            raw = source.read_bytes()
            own = {'path': str(source), 'bytes': len(raw), 'sha256': hashlib.sha256(raw).hexdigest()}
            refs = [own, c.RAW_TOOL, c.TOOL, c.STRACE]
            paths = [c.TOOL['path'], c.RAW_TOOL['path'], c.STRACE['path']]
            if missing == 'strace_ref':
                refs.remove(c.STRACE)
            elif missing == 'source_ref':
                refs.remove(own)
            else:
                paths.remove(c.STRACE['path'])
            bindings = directory / 'bindings.json'
            bindings.write_text(json.dumps({'tool': c.TOOL, 'refs': refs, 'binary_paths': paths}))
            calls = []
            module = SimpleNamespace(strict_json=json.loads,
                                     make_plan=lambda *args: calls.append('make_plan'),
                                     execute=lambda *args: calls.append('execute'))
            argv = ['--maintenance', str(directory / 'maintenance.py'), '--bindings', str(bindings),
                    '--output', str(directory / 'output'), '--run-id', 'test-r5', '--artifact-root', str(directory),
                    '--execute', '--approval', str(directory / 'approval.json')]
            with patch.object(c, 'load_source', return_value=module):
                with self.assertRaisesRegex(ValueError, 'refs|binary_paths'):
                    c.main(argv)
            self.assertEqual(calls, [])
            self.assertFalse((directory / 'output').exists())

    def test_missing_strace_full_ref_is_zero_execute_rejection(self):
        self.reject_missing_binding('strace_ref')

    def test_missing_strace_binary_path_is_zero_execute_rejection(self):
        self.reject_missing_binding('strace_path')

    def test_missing_candidate_source_ref_is_zero_execute_rejection(self):
        self.reject_missing_binding('source_ref')


if __name__ == "__main__":
    unittest.main(verbosity=2)
