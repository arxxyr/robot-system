"""爸爸，六个定向内存方法；只核负向24 tick和原出程观察，不启动工具、串口或SSH。"""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
from types import SimpleNamespace
import sys
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
ROOT = next(path for path in HERE.parents if (path / 'scripts/run_hardware_maintenance.py').is_file())
spec = importlib.util.spec_from_file_location('head1_reverse_memory', HERE / 'head1_reverse_observation_r9.py')
c = importlib.util.module_from_spec(spec)
spec.loader.exec_module(c)
h, _ = c.load_helper(ROOT / 'target/position_return_2026-10-10_r1/head-p24-preparation-r6/p24_trial_head1_wire_r6.py')
m = h.load_source(ROOT / 'scripts/run_hardware_maintenance.py')
PLAN = {'position_delta_raw': -24, 'minimum_excursion_raw': 12, 'return_tolerance_raw': 4, 'position_wait_ms': 800}


class MemoryIO:
    def __init__(self, defect=None):
        self.clock = 0
        self.registers = {'Operating_Mode': 0, 'Torque_Enable': 0, 'Present_Velocity': 0,
                          'Present_Position': 2655, 'Goal_Position': 2655}
        self.writes = []
        self.sleeps = []
        self.outward_reads = 0
        self.defect = defect

    def now(self):
        return self.clock

    def sleep(self, seconds, cleanup=False):
        self.sleeps.append((self.clock, seconds, cleanup))
        self.clock += round(seconds * 1_000_000_000)

    def poke(self, side, motor, action, register, value=None, cleanup=False):
        if action == 'write':
            self.writes.append((self.clock, register, value, cleanup))
            self.clock += 1_000_000
            self.registers[register] = value
            return None
        self.clock += 1_000_000
        if register == 'Present_Position' and self.registers['Torque_Enable'] == 1:
            if self.registers['Goal_Position'] != 2655:
                self.outward_reads += 1
                if self.outward_reads == 2 and self.defect == 'read_error':
                    raise ValueError('MEMORY_READ_ERROR')
                if self.outward_reads == 2 and self.defect == 'excursion':
                    return 2622
                if self.defect == 'wrong_direction':
                    return 2675
                return 2635
            return 2655
        return self.registers[register]


class ObservationCases(unittest.TestCase):
    def test_default_zero_actions_and_execute_requires_new_inputs(self):
        with patch.object(c, 'load_helper', side_effect=AssertionError('不得加载辅助')), \
                contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(c.main([]), 0)
            with self.assertRaisesRegex(ValueError, '输入不完整'):
                c.main(['--execute'])
            with self.assertRaisesRegex(ValueError, '新头1动作GO'):
                c.main(['--execute', '--maintenance', '/MEMORY_ONLY/source', '--helper', '/MEMORY_ONLY/helper',
                        '--bindings', '/MEMORY_ONLY/bindings', '--output', '/MEMORY_ONLY/output',
                        '--run-id', 'memory-only-r9', '--artifact-root', '/MEMORY_ONLY'])
        row = json.loads(output.getvalue())
        self.assertEqual(row['status'], 'PLAN_ONLY')
        self.assertEqual(row['actions_started'], 0)
        self.assertEqual(row['parameter_writes'], 0)
        self.assertEqual(row['motion_commands'], 0)
        self.assertEqual(row['delta_raw'], -24)
        old_go = m.strict_json((ROOT / 'target/position_return_2026-10-10_r1/head-settle-preparation-r8/head1-outward-observation-r8.go.json').read_text())
        bindings = {'schema': 'MAINTENANCE_BINDINGS_V1', 'tool': h.TOOL,
                    'usb': {side: {'device': m.DEVICES[side], 'serial': m.SERIALS[side]} for side in m.SERIALS},
                    'host': old_go['host']}
        plan = m.make_plan(bindings, '/home/loosqk/robot-checks/memory-only-r9/evidence',
                           'memory-only-r9', '/home/loosqk/robot-checks/memory-only-r9', ['head-1'])
        plan['position_delta_raw'] = -24
        plan['head1_reverse_observation'] = {'wrapper': c.reference(c.__file__), 'direction_delta_raw': -24}
        with self.assertRaisesRegex(ValueError, 'root当次GO'):
            m.validate_go(old_go, plan, old_go['issued_at_ns'])

    def test_early_qualification_holds_original_deadline_before_return_GP(self):
        executor = MemoryIO()
        engine = m.ProbeEngine(executor, dict(PLAN))
        original = m.ProbeEngine.witness_position
        wrapper = lambda e, side, motor, target, anchor, stage, evidence: c.witness_position(
            m, e, side, motor, target, anchor, stage, evidence, original)
        with patch.object(m.ProbeEngine, 'witness_position', wrapper):
            c.reverse_position(m, engine, *c.TARGET, {'id': 7, 'range_min': 797, 'range_max': 3081})
        row = engine.rows[0]
        outward = row['position_accuracy']['stages']['outward']
        self.assertEqual(row['position_accuracy']['status'], 'PASS')
        self.assertEqual(outward['deadline_ns'], outward['started_ns'] + 800_000_000)
        self.assertEqual(outward['first_qualified_elapsed_ns'], 1_000_000)
        self.assertEqual(outward['first_qualified_sample'], outward['samples'][0])
        self.assertGreater(len(outward['samples']), 1)
        returned = [event for event in executor.writes if event[1:3] == ('Goal_Position', 2655)]
        self.assertEqual(len(returned), 2)
        self.assertGreaterEqual(returned[1][0], outward['deadline_ns'])
        for stamp, seconds, cleanup in executor.sleeps:
            if outward['started_ns'] <= stamp < outward['deadline_ns']:
                self.assertFalse(cleanup)
                self.assertLessEqual(round(seconds * 1_000_000_000), outward['deadline_ns'] - stamp)
        self.assertEqual([event[1] for event in executor.writes],
                         ['Goal_Position', 'Torque_Enable', 'Goal_Position', 'Goal_Position', 'Torque_Enable'])
        self.assertEqual([event[2] for event in executor.writes if event[1] == 'Goal_Position'], [2655, 2631, 2655])
        self.assertTrue(all(register in ('Goal_Position', 'Torque_Enable') for _, register, _, _ in executor.writes))

    def test_never_qualified_or_only_late_qualified_keeps_original_failure(self):
        for late in (False, True):
            executor = MemoryIO()
            def read(*args):
                executor.clock += 801_000_000 if late else 1_000_000
                return 2631 if late else 2637
            engine = SimpleNamespace(io=executor, plan=dict(PLAN), read=read)
            evidence = {}
            with self.subTest(late=late), self.assertRaises(m.PositionAccuracyError):
                c.witness_position(m, engine, 'left', 7, 2631, 2655, 'outward', evidence, None)
            self.assertEqual(evidence['outward']['status'], 'FAIL')
            self.assertIsNone(evidence['outward']['first_qualified_sample'])
            self.assertEqual(evidence['outward']['deadline_ns'], 800_000_000)
            self.assertEqual(evidence['outward']['failure_code'], 'POSITION_ACCURACY_TIMEOUT')

    def test_exception_or_excursion_after_early_success_uses_original_safe_off(self):
        for defect in ('read_error', 'excursion', 'wrong_direction'):
            executor = MemoryIO(defect)
            engine = m.ProbeEngine(executor, dict(PLAN))
            original = m.ProbeEngine.witness_position
            wrapper = lambda e, side, motor, target, anchor, stage, evidence: c.witness_position(
                m, e, side, motor, target, anchor, stage, evidence, original)
            with self.subTest(defect=defect), patch.object(m.ProbeEngine, 'witness_position', wrapper):
                with self.assertRaisesRegex(ValueError, '位置维护失败'):
                    c.reverse_position(m, engine, *c.TARGET, {'id': 7, 'range_min': 797, 'range_max': 3081})
            self.assertEqual(engine.rows[0]['status'], 'FAIL')
            self.assertEqual(executor.writes[-1][1:], ('Torque_Enable', 0, True))
            self.assertEqual(executor.registers['Torque_Enable'], 0)
            # 未进入回程；初始follow-current仍只有一条anchor GP。
            self.assertEqual(len([event for event in executor.writes if event[1:3] == ('Goal_Position', 2655)]), 1)
            self.assertEqual(engine.rows[0]['cleanup_errors'], [])
            if defect == 'wrong_direction':
                self.assertEqual(engine.rows[0]['accuracy_failure']['failure_code'], 'POSITION_ACCURACY_TIMEOUT')

    def test_return_is_original_and_prior_qualified_late_last_read_does_not_extend_deadline(self):
        marker, calls = object(), []
        def original(*args):
            calls.append(args)
            return marker
        engine = SimpleNamespace()
        evidence = {}
        self.assertIs(c.witness_position(m, engine, 'left', 7, 2655, 2655, 'return', evidence, original), marker)
        self.assertEqual(calls, [(engine, 'left', 7, 2655, 2655, 'return', evidence)])
        executor = MemoryIO()
        reads = []
        def read(*args):
            reads.append(True)
            executor.clock += 1_000_000 if len(reads) == 1 else 800_000_000
            return 2631 if len(reads) == 1 else 2637
        engine = SimpleNamespace(io=executor, plan=dict(PLAN), read=read)
        c.witness_position(m, engine, 'left', 7, 2631, 2655, 'outward', evidence, None)
        row = evidence['outward']
        self.assertEqual(row['status'], 'PASS')
        self.assertEqual(row['first_qualified_sample']['stamp_ns'], 1_000_000)
        self.assertGreater(row['samples'][-1]['stamp_ns'], row['deadline_ns'])
        self.assertEqual(row['deadline_ns'], 800_000_000)

    def test_original_parameters_read_only_and_unknown_stop_blocks_further_UART(self):
        for condition in ('clean', 'unsafe', 'limit'):
            values = {**h.BACKUP, 'Present_Position': 2661, 'Goal_Position': 2661}
            if condition == 'unsafe':
                values['Torque_Enable'] = 1
            if condition == 'limit':
                values['Present_Position'] = 820
            read_names, original_calls, commands = [], [], []
            executor = SimpleNamespace(tool={'path': 'MEMORY_f201'},
                                       command=lambda argv, **kwargs: commands.append(argv))
            engine = SimpleNamespace(io=executor, rows=[], manual_cut=False, check_gate=lambda: None,
                                     safe_off=lambda *args: [])
            def reader(_m, _engine, name):
                read_names.append(name)
                return {'raw_unsigned': values[name]}
            def original(e, *args):
                original_calls.append(args)
                e.rows.append({'status': 'PASS'})
            with self.subTest(condition=condition), patch.object(h, 'run_trial', side_effect=AssertionError('禁止P试验')), \
                    patch.object(h, 'write_p', side_effect=AssertionError('禁止写P')):
                if condition == 'unsafe':
                    with contextlib.redirect_stdout(io.StringIO()), self.assertRaisesRegex(ValueError, '入口停止'):
                        c.checked_position(m, h, engine, *c.TARGET, {}, original, reader)
                    self.assertEqual(read_names, ['Torque_Enable'])
                    self.assertEqual(original_calls, [])
                    self.assertTrue(engine.manual_cut)
                    with self.assertRaisesRegex(ValueError, '粘性'):
                        executor.command(['MEMORY_f201', 'read'], cleanup=True)
                    with self.assertRaisesRegex(ValueError, '粘性'):
                        executor.command([h.RAW_TOOL['path'], 'read'], cleanup=True)
                    self.assertEqual(commands, [])
                elif condition == 'limit':
                    with self.assertRaisesRegex(ValueError, '超原限位'):
                        c.checked_position(m, h, engine, *c.TARGET, {}, original, reader)
                    self.assertEqual(len(read_names), 19)
                    self.assertEqual(original_calls, [])
                    self.assertEqual(engine.rows[0]['status'], 'FAIL')
                else:
                    c.checked_position(m, h, engine, *c.TARGET, {}, original, reader)
                    self.assertEqual(set(read_names), set(h.META))
                    self.assertEqual(len(read_names), 19)
                    self.assertEqual(len(original_calls), 1)
                    self.assertEqual(engine.rows[0]['parameter_entry']['status'], 'PASS')
                    self.assertEqual(engine.rows[0]['parameter_entry']['parameter_writes'], 0)
                    self.assertEqual(commands, [])
        executor = MemoryIO()
        engine = m.ProbeEngine(executor, dict(PLAN))
        with self.assertRaisesRegex(ValueError, '位置维护失败'):
            c.reverse_position(m, engine, *c.TARGET, {'id': 7, 'range_min': 2640, 'range_max': 3081})
        self.assertEqual([event[1:] for event in executor.writes], [('Torque_Enable', 0, True)])
        self.assertEqual(engine.rows[0]['status'], 'FAIL')


if __name__ == '__main__':
    unittest.main(verbosity=2)
