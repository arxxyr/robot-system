"""爸爸，扩展R6参数夹具并复用R8位置夹具；只做内存验证，不执行工具或UART。"""
import contextlib
import copy
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
ROOT = next(path for path in HERE.parents if (path / 'scripts/run_hardware_maintenance.py').is_file())
BASE = HERE.parent


def module(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


c = module(HERE / 'head1_d0_trial_r11.py', 'head1_d0_candidate_memory')
HELPER = BASE / 'head-p24-preparation-r6/p24_trial_head1_wire_r6.py'
OBSERVATION = BASE / 'head-settle-preparation-r8/head1_outward_observation_r8.py'
h, _ = c.load_frozen(HELPER, c.HELPER_SHA, c.HELPER_BYTES, 'head1_d0_original_parameter_memory')
observation, _ = c.load_frozen(OBSERVATION, c.OBSERVATION_SHA, c.OBSERVATION_BYTES, 'head1_d0_original_observation_memory')
m = h.load_source(ROOT / 'scripts/run_hardware_maintenance.py')
r6_fixture = module(BASE / 'head-p24-preparation-r6/test_p24_head1_wire_r6.py', 'head1_d0_r6_fixture')
r8_fixture = module(BASE / 'head-settle-preparation-r8/test_head1_outward_observation_r8.py', 'head1_d0_r8_fixture')
PLAN = {'position_delta_raw': 24, 'minimum_excursion_raw': 12, 'return_tolerance_raw': 4, 'position_wait_ms': 800}


class D0Cases(unittest.TestCase):
    def setUp(self):
        # 直接复用R6原值、事件表和简单engine，不引入新的执行器框架。
        r6_fixture.CandidateCases.setUp(self)
        self.engine.io.tool = h.TOOL
        self.engine.plan = dict(PLAN)
        self.engine.io.command = self.command

    read = r6_fixture.CandidateCases.read

    def command(self, argv, **kwargs):
        self.events.append(('command', list(argv), kwargs))
        if argv[:5] == [h.RAW_TOOL['path'], m.DEVICES['left'], '7', 'write', 'D_Coefficient']:
            value = int(argv[5])
            self.events.append(('write', value, kwargs.get('cleanup', False)))
            self.values['D_Coefficient'] = value
            return f'write D_Coefficient@7 <- {value} ok\n'
        return ''

    def write(self, module, engine, value, cleanup=False):
        c.write_d(m, h, engine, value, cleanup)

    def original(self, engine, side, motor, name, calibration):
        self.events.append(('original_position',))
        self.assertEqual(engine.plan, PLAN)
        self.assertEqual((side, motor, name), c.TARGET)
        self.assertEqual(tuple(self.values[key] for key in c.RESTORE_PARAMETERS), (16, 0, 0, 1, 1, 1))
        engine.rows.append({'status': 'PASS', 'cleanup_errors': [], 'position_accuracy': {'status': 'PASS'}})

    def run_candidate(self, original=None, reader=None, writer=None):
        with patch.object(m, 'verify_ref'):
            return c.run_trial(m, h, self.engine, *c.TARGET, {}, original or self.original,
                               reader=reader or self.read, writer=writer or self.write)

    def assert_sticky(self):
        self.assertTrue(self.engine.manual_cut)
        before = len(self.events)
        for tool in (h.TOOL['path'], h.RAW_TOOL['path']):
            with self.assertRaisesRegex(ValueError, '粘性拒绝'):
                self.engine.io.command([tool, m.DEVICES['left'], '7', 'read', 'Torque_Enable'], cleanup=True)
        self.assertEqual(len(self.events), before)
        self.assertEqual(self.engine.rows[0]['status'], 'FAIL')

    def test_default_zero_and_complete_missing_go_refuse_before_loading(self):
        with patch.object(c, 'load_frozen', side_effect=AssertionError('不得加载辅助')), \
                contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(c.main([]), 0)
            with self.assertRaisesRegex(ValueError, '输入不完整'):
                c.main(['--execute'])
            with self.assertRaisesRegex(ValueError, '新D0功能试验GO'):
                c.main(['--execute', '--maintenance', '/MEMORY_ONLY/source', '--helper', '/MEMORY_ONLY/helper',
                        '--observation-helper', '/MEMORY_ONLY/observation', '--bindings', '/MEMORY_ONLY/bindings',
                        '--output', '/MEMORY_ONLY/output', '--artifact-root', '/MEMORY_ONLY', '--run-id', 'memory-r11'])
        row = json.loads(output.getvalue())
        self.assertEqual(tuple(row[key] for key in ('actions_started', 'parameter_writes', 'motion_commands', 'load_raw_reads')),
                         (0, 0, 0, 0))
        self.assertEqual((row['temporary_d'], row['restore_d'], row['delta_raw'], row['tolerance_raw'], row['position_wait_ms']),
                         (0, 43, 24, 4, 800))

    def test_complete_plan_binds_frozen_helpers_and_rejects_old_go(self):
        bindings = json.loads((BASE / 'head-settle-preparation-r8/native-preparation/maintenance_bindings_head_settle_r8.json').read_text())
        refs = [c.reference(Path(c.__file__)), c.reference(HELPER), c.reference(OBSERVATION)]
        bindings['refs'].extend(refs)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'bindings.json'
            path.write_text(json.dumps(bindings))
            argv = ['--maintenance', str(ROOT / 'scripts/run_hardware_maintenance.py'), '--helper', str(HELPER),
                    '--observation-helper', str(OBSERVATION), '--bindings', str(path),
                    '--output', '/home/loosqk/robot-checks/memory-only-r11/run',
                    '--artifact-root', '/home/loosqk/robot-checks/memory-only-r11', '--run-id', 'memory-r11']
            with contextlib.redirect_stdout(io.StringIO()) as output, patch.object(m, 'execute', side_effect=AssertionError('不执行')):
                self.assertEqual(c.main(argv), 0)
            result = json.loads(output.getvalue())
            plan = result['plan']
            self.assertEqual(plan['position_delta_raw'], 24)
            self.assertEqual(plan['position_targets'], [list(c.TARGET)])
            trial = plan['head1_d0_trial']
            self.assertEqual(trial['wrapper'], refs[0])
            self.assertEqual(trial['restored_raw_registers'], list(c.RESTORE_PARAMETERS))
            self.assertEqual((trial['fixed_p'], trial['fixed_i'], trial['temporary_d'], trial['restore_d'], trial['load_raw_reads']),
                             (16, 0, 0, 43, 0))
            self.assertIn('不证明D因果', trial['interpretation'])
            for old in ('head-p24-preparation-r6/head1-p24-wire-r6.go.json',
                        'head-settle-preparation-r8/head1-outward-observation-r8.go.json',
                        'head-load-preparation-r10/head1-load-observation-r10.go.json'):
                go = json.loads((BASE / old).read_text())
                with self.subTest(old=old), self.assertRaises(ValueError):
                    m.validate_go(go, plan, go['issued_at_ns'])
            for missing in ('wrapper', 'helper', 'observation', 'raw_binary'):
                broken = copy.deepcopy(bindings)
                if missing == 'raw_binary':
                    broken['binary_paths'].remove(h.RAW_TOOL['path'])
                else:
                    broken['refs'].remove(refs[('wrapper', 'helper', 'observation').index(missing)])
                path.write_text(json.dumps(broken))
                with self.subTest(missing=missing), contextlib.redirect_stdout(io.StringIO()), self.assertRaisesRegex(ValueError, 'refs|binary_paths'):
                    c.main(argv)

    def test_happy_restore_reads_all_original_parameters_and_only_writes_d(self):
        self.run_candidate()
        self.assertEqual([event for event in self.events if event[0] == 'write'], [('write', 0, False), ('write', 43, True)])
        self.assertEqual(tuple(self.values[key] for key in c.RESTORE_PARAMETERS), (16, 43, 0, 1, 1, 1))
        trial = self.engine.rows[0]['d_trial']
        self.assertEqual((trial['status'], trial['restore_status']), ('PASS', 'PASS'))
        self.assertTrue(all(key in trial['restore_raw'] for key in c.RESTORE_PARAMETERS))
        self.assertEqual([event[1] for event in self.events if event[0] == 'read' and event[-1]],
                         ['Torque_Enable', 'Present_Velocity', 'Lock', *c.RESTORE_PARAMETERS])
        self.assertFalse(self.engine.manual_cut)

    def test_d0_ack_unknown_or_readback_mismatch_restores_without_motion(self):
        for defect in ('ack_unknown', 'readback'):
            self.setUp()
            def write(module, engine, value, cleanup=False):
                self.write(module, engine, value, cleanup)
                if value == 0:
                    if defect == 'ack_unknown':
                        raise ValueError('D0 ACK未知')
                    self.values['D_Coefficient'] = 1
            with self.subTest(defect=defect), self.assertRaisesRegex(ValueError, 'ACK未知|D0读回'):
                self.run_candidate(writer=write)
            self.assertNotIn(('original_position',), self.events)
            self.assertEqual(self.values['D_Coefficient'], 43)
            self.assertEqual(self.engine.rows[0]['d_trial']['restore_status'], 'PASS')
            self.assertEqual(self.engine.rows[0]['status'], 'FAIL')

    def test_original_accuracy_failure_remains_fail_after_successful_restore(self):
        def fail(engine, *args):
            self.original(engine, *args)
            engine.rows[0].update(status='FAIL', error_class='POSITION_ACCURACY_FAILED')
            engine.rows[0]['position_accuracy']['status'] = 'FAIL'
            raise ValueError('POSITION_ACCURACY_TIMEOUT')
        with self.assertRaisesRegex(ValueError, 'POSITION_ACCURACY_TIMEOUT'):
            self.run_candidate(original=fail)
        self.assertEqual(self.engine.rows[0]['status'], 'FAIL')
        self.assertEqual(self.engine.rows[0]['error_class'], 'POSITION_ACCURACY_FAILED')
        self.assertEqual(self.engine.rows[0]['d_trial']['restore_status'], 'PASS')
        self.assertEqual(self.values['D_Coefficient'], 43)

    def test_entry_and_post_d0_lock_failures_sticky_block_uart_and_restore(self):
        for stage, register, unknown in [('entry', reg, flag) for reg in ('Torque_Enable', 'Present_Velocity', 'Lock')
                                        for flag in (False, True)] + [('d0', 'Lock', False), ('d0', 'Lock', True)]:
            self.setUp()
            def read(module, engine, name, cleanup=False):
                is_d0 = any(event[:2] == ('write', 0) for event in self.events)
                if name == register and ((stage == 'd0') == is_d0):
                    if unknown:
                        raise ValueError('现场raw未知')
                    self.values[name] = 0 if name == 'Lock' else 1
                return self.read(module, engine, name, cleanup)
            with self.subTest(stage=stage, register=register, unknown=unknown), contextlib.redirect_stdout(io.StringIO()), \
                    self.assertRaises(ValueError):
                self.run_candidate(reader=read)
            self.assert_sticky()
            self.assertEqual([event for event in self.events if event[0] == 'write'],
                             [] if stage == 'entry' else [('write', 0, False)])
            self.assertNotIn(('original_position',), self.events)
            self.assertFalse(any(event[0] == 'read' and event[-1] for event in self.events))

    def test_stop_restore_ack_or_parameter_uncertainty_blocks_every_later_uart(self):
        cases = [('original_stop', None, False), ('restore_ack', None, False)]
        cases += [('restore_before', reg, unknown) for reg in ('Torque_Enable', 'Present_Velocity', 'Lock') for unknown in (False, True)]
        cases += [('restore_after', reg, unknown) for reg in c.RESTORE_PARAMETERS for unknown in (False, True)]
        for stage, register, unknown in cases:
            self.setUp()
            def original(engine, *args):
                self.original(engine, *args)
                if stage == 'original_stop':
                    engine.manual_cut = True
                    raise ValueError('原safe_off失败')
            def read(module, engine, name, cleanup=False):
                restored = any(event[:2] == ('write', 43) for event in self.events)
                if cleanup and name == register and ((stage == 'restore_after' and restored)
                                                     or (stage == 'restore_before' and not restored)):
                    if unknown:
                        raise ValueError('恢复raw未知')
                    self.values[name] = (0 if name == 'Lock' else self.values[name] + 1)
                return self.read(module, engine, name, cleanup)
            def write(module, engine, value, cleanup=False):
                self.write(module, engine, value, cleanup)
                if stage == 'restore_ack' and cleanup:
                    raise ValueError('D43 ACK未知')
            with self.subTest(stage=stage, register=register, unknown=unknown), contextlib.redirect_stdout(io.StringIO()), \
                    self.assertRaisesRegex(ValueError, 'safe_off失败|D对照失败'):
                self.run_candidate(original=original, reader=read, writer=write)
            self.assert_sticky()
            self.assertEqual(self.engine.rows[0]['d_trial']['restore_status'], 'FAIL')
            writes = [event for event in self.events if event[0] == 'write']
            self.assertEqual(writes, [('write', 0, False)] if stage in ('original_stop', 'restore_before')
                             else [('write', 0, False), ('write', 43, True)])

    def test_original_parameter_target_limit_and_write_whitelist_rejections(self):
        for key in ('P_Coefficient', 'D_Coefficient', 'I_Coefficient', 'Phase', 'CW_Dead_Zone', 'CCW_Dead_Zone', 'Operating_Mode'):
            self.setUp()
            self.values[key] += 1
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, '参数漂移'):
                self.run_candidate()
            self.assertFalse(any(event[0] == 'write' for event in self.events))
        self.setUp()
        self.values['Present_Position'] = 3070
        with self.assertRaisesRegex(ValueError, '内置限位'):
            self.run_candidate()
        self.assertFalse(any(event[0] == 'write' for event in self.events))
        self.setUp()
        with self.assertRaisesRegex(ValueError, '仅head1'):
            c.run_trial(m, h, self.engine, 'left', 8, 'head_motor_2', {}, self.original)
        self.assertEqual(self.events, [])
        self.run_candidate()
        rejected = [[h.RAW_TOOL['path'], m.DEVICES['left'], '7', 'write', name, value]
                    for name, value in (('P_Coefficient', '24'), ('I_Coefficient', '1'), ('Lock', '0'),
                                        ('Phase', '0'), ('CW_Dead_Zone', '0'), ('Goal_Position', '2672'), ('D_Coefficient', '1'))]
        rejected += [[h.TOOL['path'], m.DEVICES['left'], '7', 'write', 'D_Coefficient', '0'],
                     [h.TOOL['path'], m.DEVICES['right'], '7', 'write', 'Torque_Enable', '1'],
                     [h.TOOL['path'], m.DEVICES['left'], '7', 'write', 'Goal_Velocity', '20'],
                     [h.RAW_TOOL['path'], m.DEVICES['left'], '7', 'read-raw', 'Present_Load']]
        before = len(self.events)
        for argv in rejected:
            with self.subTest(argv=argv), self.assertRaises(ValueError):
                self.engine.io.command(argv)
        self.assertEqual(len(self.events), before)

    def test_exact_d_write_command_type_and_ack_contract(self):
        for value, cleanup in ((0, False), (43, True)):
            with patch.object(m, 'verify_ref') as verify:
                c.write_d(m, h, self.engine, value, cleanup)
            verify.assert_called_once_with(h.RAW_TOOL, executable=True)
            self.assertEqual(self.events[-2], ('command', [h.RAW_TOOL['path'], m.DEVICES['left'], '7', 'write',
                                                          'D_Coefficient', str(value)], {'cleanup': cleanup}))
        before = len(self.events)
        for invalid in (True, False, '0', 0.0, -1, 1, 42, 44, 254):
            with self.subTest(value=invalid), self.assertRaisesRegex(ValueError, '仅本轮D0/D43'):
                c.write_d(m, h, self.engine, invalid)
        self.assertEqual(len(self.events), before)
        self.engine.io.command = lambda argv, **kwargs: 'write P_Coefficient@7 <- 0 ok\n'
        with patch.object(m, 'verify_ref'), self.assertRaisesRegex(ValueError, 'D写ACK'):
            c.write_d(m, h, self.engine, 0)

    def test_original_position_and_r8_observation_holds_deadline_or_fails_without_return(self):
        for defect in (None, 'not_qualified', 'late_qualified', 'read_error', 'excursion', 'return_error'):
            # 原R8正向夹具直接复用；仅接入同一command guard和本轮D读写原值。
            executor = r8_fixture.MemoryIO(defect)
            executor.tool = h.TOOL
            executor.registers.update({key: value for key, value in h.BACKUP.items()
                                       if key not in ('Present_Position', 'Goal_Position')})
            old_poke = executor.poke
            commands, d_writes = [], []
            def command(argv, **kwargs):
                commands.append(list(argv))
                if argv[0] == h.RAW_TOOL['path']:
                    value = int(argv[5])
                    executor.registers['D_Coefficient'] = value
                    d_writes.append((value, kwargs.get('cleanup', False)))
                    return f'write D_Coefficient@7 <- {value} ok\n'
                action, register = argv[3:5]
                value = int(argv[5]) if action == 'write' else None
                result = old_poke('left', 7, action, register, value, kwargs.get('cleanup', False))
                if action == 'read' and register == 'Present_Position' and executor.registers['Torque_Enable'] == 1:
                    if executor.registers['Goal_Position'] != 2655:
                        if defect == 'not_qualified': return 2673
                        if defect == 'late_qualified':
                            executor.clock += 801_000_000
                            return 2679
                    elif defect == 'return_error':
                        return 2661
                return result
            executor.command = command
            def poke(side, motor, action, register, value=None, cleanup=False):
                argv = [h.TOOL['path'], m.DEVICES[side], str(motor), action, register]
                if action == 'write': argv.append(str(value))
                return executor.command(argv, cleanup=cleanup)
            executor.poke = poke
            engine = m.ProbeEngine(executor, dict(PLAN))
            def read(module, current, register, cleanup=False):
                return {'raw_unsigned': executor.registers[register]}
            original_witness = m.ProbeEngine.witness_position
            wrapper = lambda current, side, motor, target, anchor, stage, evidence: observation.witness_position(
                m, current, side, motor, target, anchor, stage, evidence, original_witness)
            with self.subTest(defect=defect), patch.object(m, 'verify_ref'), patch.object(m.ProbeEngine, 'witness_position', wrapper):
                if defect is None:
                    c.run_trial(m, h, engine, *c.TARGET, {'id': 7, 'range_min': 797, 'range_max': 3081},
                                m.ProbeEngine.position, reader=read)
                else:
                    with self.assertRaisesRegex(ValueError, '位置维护失败'):
                        c.run_trial(m, h, engine, *c.TARGET, {'id': 7, 'range_min': 797, 'range_max': 3081},
                                    m.ProbeEngine.position, reader=read)
            row = engine.rows[0]
            out = row['position_accuracy']['stages']['outward']
            self.assertEqual(out['deadline_ns'], out['started_ns'] + 800_000_000)
            self.assertEqual(d_writes, [(0, False), (43, True)])
            self.assertEqual(row['d_trial']['restore_status'], 'PASS')
            self.assertEqual(executor.writes[-1][1:], ('Torque_Enable', 0, True))
            self.assertFalse(engine.manual_cut)
            self.assertTrue(all(argv[4] in ('Goal_Position', 'Torque_Enable')
                                for argv in commands if argv[0] == h.TOOL['path'] and argv[3] == 'write'))
            anchors = [event for event in executor.writes if event[1:3] == ('Goal_Position', 2655)]
            if defect is None or defect == 'return_error':
                self.assertEqual(len(anchors), 2)
                self.assertGreaterEqual(anchors[1][0], out['deadline_ns'])
                self.assertIsNotNone(out['first_qualified_sample'])
                self.assertGreater(len(out['samples']), 1)
            else:
                self.assertEqual(len(anchors), 1)
                self.assertNotIn('return', row['position_accuracy']['stages'])
            self.assertEqual(row['status'], 'PASS' if defect is None else 'FAIL')


if __name__ == '__main__':
    unittest.main(verbosity=2)
