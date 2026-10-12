"""爸爸，固定head1(left7) I0下P24候选的最小内存对照；不执行工具或UART。"""
import importlib.util
import copy
import hashlib
import json
import contextlib
import io
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("p_trial_candidate", Path(__file__).with_name("p24_trial_head1_wire_r6.py"))
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
        self.values["P_Coefficient"] = value

    def original(self, engine, side, motor, name, calibration):
        self.events.append(("original_position",))
        self.assertEqual(engine.plan, {"position_delta_raw": 24, "return_tolerance_raw": 4, "position_wait_ms": 800})
        self.assertEqual((side, motor, name), c.TARGET)
        self.assertEqual((self.values['I_Coefficient'], self.values['P_Coefficient'], self.values['D_Coefficient'],
                          self.values['Lock'], self.values['CW_Dead_Zone'], self.values['CCW_Dead_Zone']),
                         (0, 24, 43, 1, 1, 1))
        engine.rows.append({"status": "PASS", "cleanup_errors": [], "position_accuracy": {"status": "PASS"}})

    def run_candidate(self, original=None, writer=None):
        return c.run_trial(None, self.engine, *c.TARGET, {}, original or self.original,
                           reader=self.read, writer=writer or self.write)

    def test_happy_restore_keeps_original_gates_and_fresh_dynamic_values(self):
        self.run_candidate()
        self.assertEqual(self.values["I_Coefficient"], 0)
        self.assertEqual(self.values["P_Coefficient"], 16)
        self.assertEqual([e for e in self.events if e[0] == "write"], [("write", 24, False), ("write", 16, True)])
        self.assertEqual(self.engine.rows[0]["p_trial"]["restore_status"], "PASS")

    def test_p24_ack_uncertain_still_restores_without_motion(self):
        def uncertain(_m, _engine, value, cleanup=False):
            self.write(_m, _engine, value, cleanup)
            if value == 24:
                raise ValueError("P24可能已生效但ACK失败")
        with self.assertRaisesRegex(ValueError, "ACK失败"):
            self.run_candidate(writer=uncertain)
        self.assertEqual(self.values["I_Coefficient"], 0)
        self.assertEqual(self.values["P_Coefficient"], 16)
        self.assertNotIn(("original_position",), self.events)
        self.assertEqual(self.engine.rows[0]["p_trial"]["restore_status"], "PASS")
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
        self.assertEqual(self.values["P_Coefficient"], 16)
        self.assertEqual(self.engine.rows[0]["position_accuracy"]["status"], "FAIL")
        self.assertFalse(self.engine.manual_cut)

    def test_original_stop_uncertain_blocks_restore_and_future_uart(self):
        def stop_unknown(engine, *args):
            self.original(engine, *args)
            engine.manual_cut = True
            raise ValueError("原safe_off失败")
        with self.assertRaisesRegex(ValueError, "safe_off失败"):
            self.run_candidate(original=stop_unknown)
        self.assertEqual([e for e in self.events if e[0] == "write"], [("write", 24, False)])
        self.assertFalse(any(e[0] == "read" and e[-1] is True for e in self.events))
        for tool in ("position-tool", c.RAW_TOOL["path"]):
            with self.assertRaisesRegex(ValueError, "粘性拒绝"):
                self.io.command([tool, "read"], cleanup=True)
        self.io.command(["systemctl", "show"], cleanup=True)

    def test_restore_ack_uncertain_blocks_future_uart(self):
        def restore_unknown(_m, _engine, value, cleanup=False):
            self.write(_m, _engine, value, cleanup)
            if value == 16:
                raise ValueError("P16恢复ACK失败")
        with self.assertRaisesRegex(ValueError, "P对照失败"):
            self.run_candidate(writer=restore_unknown)
        self.assertTrue(self.engine.manual_cut)
        with self.assertRaisesRegex(ValueError, "粘性拒绝"):
            self.io.command(["position-tool", "read"], cleanup=True)
        self.assertEqual(self.engine.rows[0]["status"], "FAIL")

    def test_entry_torque_nonzero_blocks_all_later_uart_without_p_write(self):
        self.values["Torque_Enable"] = 1
        with self.assertRaisesRegex(ValueError, "入口停止未确认"):
            self.run_candidate()
        self.assertEqual(self.events, [("gate",), ("read", "Torque_Enable", False)])
        self.assertTrue(self.engine.manual_cut)
        with self.assertRaisesRegex(ValueError, "粘性拒绝"):
            self.io.command(["position-tool", "read"], cleanup=True)

    def test_entry_velocity_read_unknown_blocks_all_later_uart_without_p_write(self):
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

    def test_restore_lock_zero_blocks_p16_write_and_future_uart(self):
        def lock_changed(engine, *args):
            self.original(engine, *args)
            self.values["Lock"] = 0
        with self.assertRaisesRegex(ValueError, "P对照失败"):
            self.run_candidate(original=lock_changed)
        self.assertEqual([e for e in self.events if e[0] == "write"], [("write", 24, False)])
        self.assertTrue(self.engine.manual_cut)
        with self.assertRaisesRegex(ValueError, "粘性拒绝"):
            self.io.command([c.RAW_TOOL["path"], "read"], cleanup=True)

    def test_any_original_control_parameter_change_rejects_before_p_write(self):
        for parameter in ('I_Coefficient', 'P_Coefficient', 'D_Coefficient', 'Lock', 'CW_Dead_Zone', 'CCW_Dead_Zone', 'Phase', 'Operating_Mode'):
            with self.subTest(parameter=parameter):
                self.setUp()
                self.values[parameter] += 1
                with self.assertRaisesRegex(ValueError, '原备份不一致'):
                    self.run_candidate()
                self.assertFalse(any(e[0] in ('write', 'original_position') for e in self.events))

    def test_p24_readback_mismatch_prevents_motion_and_restores_p16(self):
        def mismatch(m, engine, value, cleanup=False):
            self.write(m, engine, value, cleanup)
            if value == 24:
                self.values['P_Coefficient'] = 23
        with self.assertRaisesRegex(ValueError, 'P24/Lock1读回不符'):
            self.run_candidate(writer=mismatch)
        self.assertNotIn(('original_position',), self.events)
        self.assertEqual(self.values['P_Coefficient'], 16)
        self.assertEqual(self.values['I_Coefficient'], 0)
        self.assertEqual(self.engine.rows[0]['p_trial']['restore_status'], 'PASS')

    def test_restore_velocity_nonzero_blocks_p16_and_future_uart(self):
        def still_moving(engine, *args):
            self.original(engine, *args)
            self.values['Present_Velocity'] = 50
        with self.assertRaisesRegex(ValueError, 'P对照失败'):
            self.run_candidate(original=still_moving)
        self.assertEqual([e for e in self.events if e[0] == 'write'], [('write', 24, False)])
        self.assertTrue(self.engine.manual_cut)
        with self.assertRaisesRegex(ValueError, '粘性拒绝'):
            self.io.command([c.RAW_TOOL['path'], 'read'], cleanup=True)

    def test_every_restored_control_parameter_mismatch_blocks_future_uart(self):
        for parameter in c.RESTORE_PARAMETERS:
            with self.subTest(parameter=parameter):
                self.setUp()
                def changed(m, engine, value, cleanup=False):
                    self.write(m, engine, value, cleanup)
                    if value == 16:
                        self.values[parameter] += 1
                with self.assertRaisesRegex(ValueError, 'P对照失败'):
                    self.run_candidate(writer=changed)
                self.assertTrue(self.engine.manual_cut)
                self.assertEqual([e for e in self.events if e[0] == 'write'], [('write', 24, False), ('write', 16, True)])
                with self.assertRaisesRegex(ValueError, '粘性拒绝'):
                    self.io.command(['position-tool', 'read'], cleanup=True)

    def test_only_fixed_head1_target_is_allowed(self):
        with self.assertRaisesRegex(ValueError, '仅head1'):
            c.run_trial(None, self.engine, 'left', 8, 'head_motor_2', {}, self.original,
                        reader=self.read, writer=self.write)
        self.assertEqual(self.events, [])


class FixedWriteCases(unittest.TestCase):
    def setUp(self):
        self.events = []
        self.m = SimpleNamespace(DEVICES={'left': 'left-device'},
                                 verify_ref=lambda ref, **kw: self.events.append(('verify', ref, kw)))
        def command(argv, **kwargs):
            self.events.append(('command', argv, kwargs))
            return f'write P_Coefficient@7 <- {argv[-1]} ok\n'
        self.engine = SimpleNamespace(io=SimpleNamespace(command=command))

    def test_only_p24_and_restore_p16_fixed_command_can_be_written(self):
        for value, cleanup in ((24, False), (16, True)):
            c.write_p(self.m, self.engine, value, cleanup)
            self.assertEqual(self.events[-1], ('command', [c.RAW_TOOL['path'], 'left-device', '7',
                                                          'write', 'P_Coefficient', str(value)], {'cleanup': cleanup}))
        self.assertEqual(len([e for e in self.events if e[0] == 'command']), 2)

    def test_invalid_p_value_or_type_starts_no_command(self):
        for value in (0, 2, 15, 17, 23, 25, 254, True, 24.0, '24'):
            with self.subTest(value=value):
                with self.assertRaisesRegex(ValueError, '仅本轮P16/P24'):
                    c.write_p(self.m, self.engine, value)
        self.assertEqual(self.events, [])

    def test_other_register_ack_is_never_accepted(self):
        self.engine.io.command = lambda argv, **kwargs: 'write I_Coefficient@7 <- 24 ok\n'
        with self.assertRaisesRegex(ValueError, 'P写ACK'):
            c.write_p(self.m, self.engine, 24)


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


class FullBindingCases(unittest.TestCase):
    def setUp(self):
        # 保留实际完整身份字典格式；本层仅找固定三字段，原维护层继续核全部身份。
        self.metadata = {'uid': 1000, 'gid': 1000, 'mode': 509, 'device': 66305,
                         'inode': 15618634, 'mtime_ns': 1790786705801501454,
                         'ctime_ns': 1790786705801501454, 'elf_machine': 183}
        path = Path(c.__file__).resolve()
        raw = path.read_bytes()
        self.own = {'path': str(path), 'bytes': len(raw), 'sha256': hashlib.sha256(raw).hexdigest()}
        self.expected = [self.own, c.RAW_TOOL, c.TOOL, c.STRACE]
        self.bindings = {'tool': {**c.TOOL, **self.metadata},
                         'refs': [{**ref, **self.metadata} for ref in self.expected],
                         'binary_paths': [c.TOOL['path'], c.RAW_TOOL['path'], c.STRACE['path']]}

    def test_full_tool_and_all_refs_accept_metadata_without_removing_identity(self):
        before = copy.deepcopy(self.bindings)
        c.required_references(self.bindings, self.own)
        self.assertEqual(self.bindings, before)
        for ref in self.bindings['refs']:
            for key, value in self.metadata.items():
                self.assertEqual(ref[key], value)

    def test_any_wrong_top_fixed_field_is_rejected(self):
        for key in ('path', 'sha256', 'bytes'):
            with self.subTest(field=key):
                broken = copy.deepcopy(self.bindings)
                broken['tool'][key] = '错误' if key != 'bytes' else 1
                with self.assertRaisesRegex(ValueError, '冻结f201'):
                    c.required_references(broken, self.own)

    def test_any_wrong_fixed_field_in_each_required_full_ref_is_rejected(self):
        for index, expected in enumerate(self.expected):
            for key in ('path', 'sha256', 'bytes'):
                with self.subTest(reference=expected['path'], field=key):
                    broken = copy.deepcopy(self.bindings)
                    broken['refs'][index][key] = '错误' if key != 'bytes' else 1
                    with self.assertRaisesRegex(ValueError, 'refs'):
                        c.required_references(broken, self.own)

    def test_any_missing_fixed_field_in_each_required_full_ref_is_rejected(self):
        for index, expected in enumerate(self.expected):
            for key in ('path', 'sha256', 'bytes'):
                with self.subTest(reference=expected['path'], field=key):
                    broken = copy.deepcopy(self.bindings)
                    del broken['refs'][index][key]
                    with self.assertRaisesRegex(ValueError, 'refs'):
                        c.required_references(broken, self.own)

    def test_full_metadata_bindings_reach_plan_only_without_execute(self):
        with tempfile.TemporaryDirectory() as folder:
            directory = Path(folder)
            bindings = directory / 'bindings.json'
            bindings.write_text(json.dumps(self.bindings))
            calls = []
            module = SimpleNamespace(strict_json=json.loads,
                                     make_plan=lambda *args: calls.append('make_plan') or {},
                                     digest=lambda plan: '本地计划',
                                     execute=lambda *args: calls.append('execute'))
            argv = ['--maintenance', str(directory / 'maintenance.py'), '--bindings', str(bindings),
                    '--output', str(directory / 'output'), '--run-id', 'test-r5', '--artifact-root', str(directory)]
            output = io.StringIO()
            with patch.object(c, 'load_source', return_value=module), contextlib.redirect_stdout(output):
                self.assertEqual(c.main(argv), 0)
            plan = json.loads(output.getvalue())['plan']
            self.assertNotIn('i_trial', plan)
            self.assertEqual((plan['p_trial']['fixed_i'], plan['p_trial']['temporary_p'], plan['p_trial']['restore_p']), (0, 24, 16))
            self.assertFalse(any(key in plan['p_trial'] for key in ('temporary_i', 'restore_i')))
            self.assertEqual(plan['p_trial']['ordinary_stop_wire']['additional_uart_reads'], 0)
            self.assertEqual(calls, ['make_plan'])
            self.assertFalse((directory / 'output').exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
