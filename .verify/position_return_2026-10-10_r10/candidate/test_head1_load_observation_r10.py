"""爸爸，平台Load对照的定向内存测试；所有工具输出为夹具，不开串口或SSH。"""
import contextlib
import copy
import importlib.util
import io
import json
from pathlib import Path
from types import SimpleNamespace
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
ROOT = next(path for path in HERE.parents if (path / 'scripts/run_hardware_maintenance.py').is_file())
spec = importlib.util.spec_from_file_location('head1_load_memory', HERE / 'head1_load_observation_r10.py')
c = importlib.util.module_from_spec(spec)
spec.loader.exec_module(c)
R9 = ROOT / 'target/position_return_2026-10-10_r1/head-reverse-preparation-r9/head1_reverse_observation_r9.py'
HELPER = ROOT / 'target/position_return_2026-10-10_r1/head-p24-preparation-r6/p24_trial_head1_wire_r6.py'
r, _ = c.load_reverse(R9)
h, _ = r.load_helper(HELPER)
m = h.load_source(ROOT / 'scripts/run_hardware_maintenance.py')
PLAN = {'position_delta_raw': -24, 'minimum_excursion_raw': 12, 'return_tolerance_raw': 4, 'position_wait_ms': 800}
# ELF引用由root提供；以下只在内存夹具返回stdout，不访问该远端路径。
TOOL = {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/position-return-r7/load-readback-tool-r10/feetech_poke',
        'sha256': '2949ccb2a1735ee2091b2c562c428500461e31fbf5ec967a9c543eec0b1016f3', 'bytes': 1514376}


def raw_fixture(word=1040):
    tx = [255, 255, 254, 5, 130, 60, 2, 7]
    tx.append((~sum(tx[2:])) & 255)
    rx = [255, 255, 7, 4, 0, word & 255, word >> 8]
    rx.append((~sum(rx[2:])) & 255)
    return {'operation': 'sync_read', 'serial_device': m.DEVICES['left'], 'motor_id': 7,
            'register': 'Present_Load', 'requested_register_address': 60, 'width': 2,
            'raw_unsigned': word, 'tx': tx, 'rx': rx}


class MemoryIO:
    def __init__(self, positions=(2645, 2637, 2637), *, load_cost=2_000_000, defect=None):
        self.clock = 0
        self.tool = h.TOOL
        self.registers = {'Operating_Mode': 0, 'Torque_Enable': 0, 'Present_Velocity': 0,
                          'Present_Position': 2655, 'Goal_Position': 2655}
        self.positions = list(positions)
        self.position_reads = 0
        self.writes = []
        self.commands = []
        self.sleeps = []
        self.sequence = 0
        self.load_cost = load_cost
        self.load_completed = False
        self.after_load_read = False
        self.defect = defect

    def now(self):
        return self.clock

    def sleep(self, seconds, cleanup=False):
        self.sleeps.append((self.clock, seconds, cleanup))
        self.clock += round(seconds * 1_000_000_000)

    def command(self, argv, **kwargs):
        self.sequence += 1
        self.commands.append(list(argv))
        if argv[0] == TOOL['path']:
            self.clock += self.load_cost
            self.load_completed = True
            if self.defect == 'load_error':
                raise ValueError('MEMORY_LOAD_READ_ERROR')
            value = raw_fixture()
            if self.defect == 'bad_checksum':
                value['rx'][-1] ^= 1
            return json.dumps(value) + '\n'
        self.clock += 1_000_000
        action, register = argv[3:5]
        if action == 'write':
            value = int(argv[5])
            self.writes.append((self.clock, register, value, kwargs.get('cleanup', False)))
            self.registers[register] = value
            return 'OK'
        if register == 'Present_Position' and self.registers['Torque_Enable'] == 1:
            if self.registers['Goal_Position'] != 2655:
                if self.load_completed and not self.after_load_read:
                    self.after_load_read = True
                    if self.defect == 'after_pp_late':
                        self.clock += 801_000_000
                    if self.defect == 'after_pp_error':
                        raise ValueError('MEMORY_AFTER_PP_ERROR')
                    if self.defect == 'after_pp_excursion':
                        return '2622'
                index = min(self.position_reads, len(self.positions) - 1)
                self.position_reads += 1
                return str(self.positions[index])
            return '2655'
        return str(self.registers[register])

    def poke(self, side, motor, action, register, value=None, cleanup=False):
        argv = [self.tool['path'], m.DEVICES[side], str(motor), action, register]
        if action == 'write':
            argv.append(str(value))
        text = self.command(argv, cleanup=cleanup)
        return None if action == 'write' else int(text)


def wrapper(engine, side, motor, target, anchor, stage, evidence):
    return c.witness_position(m, engine, side, motor, target, anchor, stage, evidence, ORIGINAL_WITNESS, TOOL)


ORIGINAL_WITNESS = m.ProbeEngine.witness_position


class LoadCases(unittest.TestCase):
    def run_reverse(self, executor, expected_success=False):
        engine = m.ProbeEngine(executor, dict(PLAN))
        def reader(_m, _engine, register):
            return {'raw_unsigned': 2655 if register in ('Present_Position', 'Goal_Position') else h.BACKUP[register]}
        with patch.object(m.ProbeEngine, 'witness_position', wrapper), patch.object(m, 'verify_ref'):
            if expected_success:
                c.checked_position(m, h, r, engine, *c.TARGET, {'id': 7, 'range_min': 797, 'range_max': 3081}, TOOL, reader)
            else:
                with self.assertRaisesRegex(ValueError, '位置维护失败'):
                    c.checked_position(m, h, r, engine, *c.TARGET, {'id': 7, 'range_min': 797, 'range_max': 3081}, TOOL, reader)
        return engine

    def test_default_and_complete_missing_go_refuse_before_helper_or_io(self):
        with patch.object(c, 'load_reverse', side_effect=AssertionError('禁止加载')), \
                contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(c.main([]), 0)
            with self.assertRaisesRegex(ValueError, '输入不完整'):
                c.main(['--execute'])
            with self.assertRaisesRegex(ValueError, '新Load对照GO'):
                c.main(['--execute', '--maintenance', '/MEMORY_ONLY/maintenance', '--helper', '/MEMORY_ONLY/helper',
                        '--reverse-helper', '/MEMORY_ONLY/reverse', '--bindings', '/MEMORY_ONLY/bindings',
                        '--output', '/MEMORY_ONLY/output', '--run-id', 'memory-r10', '--artifact-root', '/MEMORY_ONLY'])
        row = json.loads(output.getvalue())
        self.assertEqual((row['actions_started'], row['parameter_writes'], row['motion_commands'], row['load_raw_reads']),
                         (0, 0, 0, 0))
        self.assertEqual((row['position_wait_ms'], row['tolerance_raw'], row['delta_raw']), (800, 4, -24))

    def test_load_tool_requires_complete_explicit_binding_and_separate_binary(self):
        base = {'load_tool': {**TOOL, 'uid': 1000}, 'refs': [{**TOOL, 'uid': 1000}], 'binary_paths': [TOOL['path']]}
        self.assertEqual(c.load_tool_reference(base, h), base['load_tool'])
        for condition in ('missing', 'sha_mismatch', 'metadata_mismatch', 'conflicting_ref', 'binary_missing', 'alias', 'bad_bytes'):
            value = copy.deepcopy(base)
            if condition == 'missing':
                del value['load_tool']
            elif condition == 'sha_mismatch':
                value['refs'][0]['sha256'] = '2' * 64
            elif condition == 'metadata_mismatch':
                del value['refs'][0]['uid']
            elif condition == 'conflicting_ref':
                value['refs'].append({**TOOL, 'uid': 9})
            elif condition == 'binary_missing':
                value['binary_paths'] = []
            elif condition == 'alias':
                value['load_tool']['path'] = h.TOOL['path']
                value['refs'][0]['path'] = h.TOOL['path']
                value['binary_paths'] = [h.TOOL['path']]
            else:
                value['load_tool']['bytes'] = True
            with self.subTest(condition=condition), self.assertRaises(ValueError):
                c.load_tool_reference(value, h)

    def test_raw_unsigned_words_and_strict_identity_protocol_validation(self):
        for word in (0, 1040, 65535):
            executor = SimpleNamespace(now=lambda: 350_000_000, command=lambda argv: json.dumps(raw_fixture(word)) + '\n')
            with patch.object(m, 'verify_ref') as verify:
                result = c.read_load_raw(m, SimpleNamespace(io=executor), TOOL, 800_000_000)
            verify.assert_called_once_with(TOOL, executable=True)
            self.assertEqual(result['raw'], raw_fixture(word))
            self.assertEqual(result['raw']['raw_unsigned'], word)
            self.assertEqual(set(result), {'status', 'started_ns', 'finished_ns', 'raw'})
        for defect in ('id', 'address', 'width', 'device', 'register', 'operation', 'raw_word', 'bool_word',
                       'tx', 'rx_id', 'rx_error', 'rx_checksum', 'rx_short', 'rx_bool', 'extra', 'duplicate', 'line'):
            value = raw_fixture()
            if defect == 'id': value['motor_id'] = 8
            elif defect == 'address': value['requested_register_address'] = 69
            elif defect == 'width': value['width'] = 1
            elif defect == 'device': value['serial_device'] = m.DEVICES['right']
            elif defect == 'register': value['register'] = 'Present_Current'
            elif defect == 'operation': value['operation'] = 'read'
            elif defect == 'raw_word': value['raw_unsigned'] += 1
            elif defect == 'bool_word': value['raw_unsigned'] = True
            elif defect == 'tx': value['tx'][-1] ^= 1
            elif defect == 'rx_id': value['rx'][2] = 8
            elif defect == 'rx_error': value['rx'][4] = 1
            elif defect == 'rx_checksum': value['rx'][-1] ^= 1
            elif defect == 'rx_short': value['rx'].pop()
            elif defect == 'rx_bool': value['rx'][4] = False
            elif defect == 'extra': value['decoded_current'] = 9
            text = json.dumps(value) + '\n'
            if defect == 'duplicate': text = text.replace('"motor_id": 7', '"motor_id": 7, "motor_id": 7')
            if defect == 'line': text += text
            executor = SimpleNamespace(now=lambda: 350_000_000, command=lambda argv: text)
            with self.subTest(defect=defect), patch.object(m, 'verify_ref'), self.assertRaises(ValueError):
                c.read_load_raw(m, SimpleNamespace(io=executor), TOOL, 800_000_000)

    def test_platform_samples_once_bracketed_and_original_failure_safe_off(self):
        executor = MemoryIO()
        engine = self.run_reverse(executor)
        row = engine.rows[0]
        outward = row['position_accuracy']['stages']['outward']
        load = outward['load_diagnostic']
        self.assertEqual((row['status'], outward['status'], load['status']), ('FAIL', 'FAIL', 'SAMPLED'))
        self.assertEqual(load['raw_read_attempts'], 1)
        self.assertEqual(load['read']['raw'], raw_fixture())
        self.assertTrue(load['plateau_bracket_confirmed'])
        self.assertGreaterEqual(load['trigger_ns'] - outward['started_ns'], 300_000_000)
        self.assertLessEqual(load['trigger_ns'] - outward['started_ns'], 400_000_000)
        self.assertGreaterEqual(outward['deadline_ns'] - load['trigger_ns'], 400_000_000)
        self.assertLessEqual(load['before_sample']['stamp_ns'], load['read']['started_ns'])
        self.assertLessEqual(load['read']['finished_ns'], load['after_sample']['stamp_ns'])
        self.assertEqual(outward['deadline_ns'], outward['started_ns'] + 800_000_000)
        self.assertEqual(outward['failure_code'], 'POSITION_ACCURACY_TIMEOUT')
        self.assertEqual([event[1:3] for event in executor.writes],
                         [('Goal_Position', 2655), ('Torque_Enable', 1), ('Goal_Position', 2631), ('Torque_Enable', 0)])
        self.assertEqual([argv for argv in executor.commands if argv[0] == TOOL['path']],
                         [[TOOL['path'], m.DEVICES['left'], '7', 'read-raw', 'Present_Load']])
        self.assertEqual(row['cleanup_errors'], [])
        self.assertFalse(engine.manual_cut)
        with self.assertRaisesRegex(ValueError, '最多一次'):
            executor.command([TOOL['path'], m.DEVICES['left'], '7', 'read-raw', 'Present_Load'])

    def test_no_platform_or_late_platform_is_not_sampled_without_extra_iteration(self):
        for late in (False, True):
            executor = SimpleNamespace(clock=0)
            executor.now = lambda: executor.clock
            executor.sleep = lambda seconds: setattr(executor, 'clock', executor.clock + round(seconds * 1e9))
            count = 0
            def read(*args):
                nonlocal count
                count += 1
                executor.clock += 300_000_000 if late and count == 1 else 100_000_000 if late else 1_000_000
                return 2637 if late else 2660 - count
            engine = SimpleNamespace(io=executor, read=read, plan=dict(PLAN))
            evidence = {}
            with self.subTest(late=late), patch.object(c, 'read_load_raw', side_effect=AssertionError('禁止Load读取')), \
                    self.assertRaises(m.PositionAccuracyError):
                c.witness_position(m, engine, 'left', 7, 2631, 2655, 'outward', evidence, None, TOOL)
            load = evidence['outward']['load_diagnostic']
            self.assertEqual((load['status'], load['raw_read_attempts']), ('NOT_SAMPLED', 0))

    def test_original_early_qualification_and_return_keep_original_deadline(self):
        executor = MemoryIO(positions=(2645,) + (2637,) * 7 + (2631,))
        engine = self.run_reverse(executor, expected_success=True)
        row = engine.rows[0]
        out = row['position_accuracy']['stages']['outward']
        self.assertEqual(out['load_diagnostic']['status'], 'SAMPLED')
        self.assertEqual(out['status'], 'PASS')
        self.assertEqual(row['position_accuracy']['stages']['return']['status'], 'PASS')
        self.assertNotIn('load_diagnostic', row['position_accuracy']['stages']['return'])
        self.assertEqual(out['deadline_ns'], out['started_ns'] + 800_000_000)
        returned = [event for event in executor.writes if event[1:3] == ('Goal_Position', 2655)]
        self.assertEqual(len(returned), 2)
        self.assertGreaterEqual(returned[-1][0], out['deadline_ns'])
        self.assertEqual(sum(argv[0] == TOOL['path'] for argv in executor.commands), 1)
        self.assertTrue(all(event[1] in ('Goal_Position', 'Torque_Enable') for event in executor.writes))

    def test_load_and_bracket_failure_or_late_reads_never_turn_prior_qualification_into_pass(self):
        for defect in ('load_error', 'bad_checksum', 'after_pp_error', 'after_pp_late', 'after_pp_excursion', 'load_late'):
            executor = MemoryIO(positions=(2631, 2637, 2637), defect=defect,
                                load_cost=801_000_000 if defect == 'load_late' else 2_000_000)
            engine = self.run_reverse(executor)
            row = engine.rows[0]
            out = row['position_accuracy']['stages']['outward']
            load = out['load_diagnostic']
            with self.subTest(defect=defect):
                self.assertIsNotNone(out['first_qualified_sample'])
                self.assertEqual((out['status'], load['status'], load['raw_read_attempts']), ('FAIL', 'FAIL', 1))
                self.assertEqual(out['deadline_ns'], out['started_ns'] + 800_000_000)
                self.assertEqual(executor.writes[-1][1:], ('Torque_Enable', 0, True))
                self.assertEqual(sum(argv[0] == TOOL['path'] for argv in executor.commands), 1)
                self.assertNotIn('return', row['position_accuracy']['stages'])
                self.assertIn('error', load)
                self.assertIn('finished_ns', load)
                if defect == 'after_pp_excursion':
                    self.assertEqual(row['error_class'], 'POSITION_ACCURACY_FAILED')
                    self.assertIs(row['accuracy_failure'], out)
                    self.assertEqual(out['failure_code'], 'POSITION_EXCURSION_LIMIT')
                else:
                    self.assertEqual(row['error_class'], 'ACTION_FAILED')
                    self.assertNotIn('accuracy_failure', row)
                    self.assertNotIn('failure_code', out)
                if defect == 'load_late':
                    self.assertNotIn('after_sample', load)

    def test_trigger_boundary_and_tool_verification_expiry_refuse_late_start(self):
        executor = SimpleNamespace(clock=0)
        executor.now = lambda: executor.clock
        executor.sleep = lambda seconds: setattr(executor, 'clock', executor.clock + round(seconds * 1e9))
        reads = 0
        def read(*args):
            nonlocal reads
            reads += 1
            executor.clock += 349_000_000 if reads == 1 else 1_000_000
            return 2637
        engine = SimpleNamespace(io=executor, read=read, plan=dict(PLAN))
        def load(*args):
            self.assertEqual(executor.clock, 400_000_000)
            args[-1]['raw_read_attempts'] = 1
            executor.clock += 1_000_000
            return {'status': 'SAMPLED', 'started_ns': 400_000_000, 'finished_ns': 401_000_000, 'raw': raw_fixture()}
        evidence = {}
        with patch.object(c, 'read_load_raw', side_effect=load) as raw_read, self.assertRaises(m.PositionAccuracyError):
            c.witness_position(m, engine, 'left', 7, 2631, 2655, 'outward', evidence, None, TOOL)
        self.assertEqual(raw_read.call_count, 1)
        self.assertEqual(evidence['outward']['load_diagnostic']['status'], 'SAMPLED')
        commands = []
        executor.command = lambda argv: commands.append(argv)
        for stamp in (299_000_000, 401_000_000, 800_000_000):
            with self.subTest(stamp=stamp), patch.object(m, 'verify_ref',
                    side_effect=lambda *args, **kwargs: setattr(executor, 'clock', stamp)):
                result = c.read_load_raw(m, engine, TOOL, 800_000_000)
                self.assertEqual(result['status'], 'NOT_SAMPLED')
                self.assertEqual(result['reason_code'], 'LOAD_TOOL_VERIFICATION_MISSED_TRIGGER_WINDOW')
        self.assertEqual(commands, [])

    def test_startup_unchanged_short_or_wrong_direction_never_consumes_load_read(self):
        for position in (2655, 2645, 2673):
            executor = MemoryIO(positions=(position,))
            engine = self.run_reverse(executor)
            load = engine.rows[0]['position_accuracy']['stages']['outward']['load_diagnostic']
            with self.subTest(position=position):
                self.assertEqual((load['status'], load['raw_read_attempts'], load['probe_considered']),
                                 ('NOT_SAMPLED', 0, False))
                self.assertEqual(sum(argv[0] == TOOL['path'] for argv in executor.commands), 0)
        executor = MemoryIO(positions=(2655, 2655, 2645, 2637))
        engine = self.run_reverse(executor)
        out = engine.rows[0]['position_accuracy']['stages']['outward']
        load = out['load_diagnostic']
        self.assertEqual(load['status'], 'SAMPLED')
        self.assertGreaterEqual(load['trigger_ns'] - out['started_ns'], 300_000_000)
        self.assertEqual(load['before_sample']['position_raw'], 2637)
        self.assertEqual(load['raw_read_attempts'], 1)

    def test_post_verification_missed_window_is_not_sampled_and_not_retried(self):
        executor = MemoryIO()
        executor.registers.update(Torque_Enable=1, Goal_Position=2631)
        engine = m.ProbeEngine(executor, dict(PLAN))
        evidence = {}
        with patch.object(m, 'verify_ref', side_effect=lambda *args, **kwargs: setattr(executor, 'clock', 401_000_000)) as verify, \
                self.assertRaises(m.PositionAccuracyError):
            c.witness_position(m, engine, 'left', 7, 2631, 2655, 'outward', evidence, ORIGINAL_WITNESS, TOOL)
        load = evidence['outward']['load_diagnostic']
        self.assertEqual(verify.call_count, 1)
        self.assertEqual((load['status'], load['raw_read_attempts'], load['probe_considered']), ('NOT_SAMPLED', 0, True))
        self.assertEqual(load['skip']['reason_code'], 'LOAD_TOOL_VERIFICATION_MISSED_TRIGGER_WINDOW')
        self.assertNotIn('after_sample', load)
        self.assertEqual(sum(argv[0] == TOOL['path'] for argv in executor.commands), 0)
        self.assertGreater(len(evidence['outward']['samples']), 7)

    def test_return_is_original_and_non_load_late_sampling_semantics_unchanged(self):
        marker, calls = object(), []
        def original(*args):
            calls.append(args)
            return marker
        engine, evidence = SimpleNamespace(), {}
        self.assertIs(c.witness_position(m, engine, 'left', 7, 2655, 2655, 'return', evidence, original, TOOL), marker)
        self.assertEqual(calls, [(engine, 'left', 7, 2655, 2655, 'return', evidence)])
        executor = SimpleNamespace(clock=0)
        executor.now = lambda: executor.clock
        executor.sleep = lambda seconds: setattr(executor, 'clock', executor.clock + round(seconds * 1e9))
        reads = 0
        def read(*args):
            nonlocal reads
            reads += 1
            executor.clock += 1_000_000 if reads == 1 else 800_000_000
            return 2631 if reads == 1 else 2637
        engine = SimpleNamespace(io=executor, read=read, plan=dict(PLAN))
        with patch.object(c, 'read_load_raw', side_effect=AssertionError('禁止Load读取')):
            c.witness_position(m, engine, 'left', 7, 2631, 2655, 'outward', evidence, None, TOOL)
        self.assertEqual(evidence['outward']['status'], 'PASS')
        self.assertGreater(evidence['outward']['samples'][-1]['stamp_ns'], evidence['outward']['deadline_ns'])
        self.assertEqual(evidence['outward']['load_diagnostic']['status'], 'NOT_SAMPLED')

    def test_original_parameter_entry_and_write_guard_include_new_tool_and_sticky_stop(self):
        executor = MemoryIO(positions=(2631,))
        engine = m.ProbeEngine(executor, dict(PLAN))
        read_names = []
        def reader(_m, _engine, register):
            read_names.append(register)
            return {'raw_unsigned': 2655 if register in ('Present_Position', 'Goal_Position') else h.BACKUP[register]}
        with patch.object(m.ProbeEngine, 'witness_position', wrapper), patch.object(m, 'verify_ref'):
            c.checked_position(m, h, r, engine, *c.TARGET, {'id': 7, 'range_min': 797, 'range_max': 3081}, TOOL, reader)
        self.assertEqual(set(read_names), set(h.META))
        self.assertEqual(engine.rows[0]['parameter_entry']['parameter_writes'], 0)
        self.assertEqual(engine.rows[0]['parameter_entry']['status'], 'PASS')
        rejected = ([TOOL['path'], m.DEVICES['left'], '7', 'write', 'Goal_Position', '2631'],
                    [h.RAW_TOOL['path'], m.DEVICES['left'], '7', 'write', 'P_Coefficient', '24'],
                    [h.TOOL['path'], m.DEVICES['right'], '7', 'write', 'Torque_Enable', '1'],
                    [h.TOOL['path'], m.DEVICES['left'], '7', 'write', 'Goal_Velocity', '20'])
        for argv in rejected:
            with self.subTest(argv=argv), self.assertRaises(ValueError):
                executor.command(argv)
        engine.manual_cut = True
        for path in (TOOL['path'], h.TOOL['path'], h.RAW_TOOL['path']):
            with self.subTest(path=path), self.assertRaisesRegex(ValueError, '粘性'):
                executor.command([path, m.DEVICES['left'], '7', 'read', 'Present_Position'], cleanup=True)

    def test_unknown_entry_stop_blocks_original_and_load_tools_before_motion(self):
        executor = MemoryIO()
        engine = m.ProbeEngine(executor, dict(PLAN))
        def reader(_m, _engine, register):
            return {'raw_unsigned': 1 if register == 'Torque_Enable' else h.BACKUP[register]}
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaisesRegex(ValueError, '入口停止'):
            c.checked_position(m, h, r, engine, *c.TARGET, {'id': 7, 'range_min': 797, 'range_max': 3081}, TOOL, reader)
        self.assertTrue(engine.manual_cut)
        self.assertEqual(executor.commands, [])
        self.assertEqual(executor.writes, [])
        self.assertEqual(engine.rows[0]['position_accuracy']['status'], 'NOT_RUN')
        for path in (TOOL['path'], h.TOOL['path'], h.RAW_TOOL['path']):
            with self.subTest(path=path), self.assertRaisesRegex(ValueError, '粘性'):
                executor.command([path, m.DEVICES['left'], '7', 'read-raw', 'Present_Load'], cleanup=True)

    def test_complete_plan_binds_new_tool_and_rejects_old_r9_go(self):
        bindings = json.loads((R9.parent / 'native-preparation/maintenance_bindings_head_reverse_r9.json').read_text())
        bindings['load_tool'] = dict(TOOL)
        bindings['refs'] += [c.reference(c.__file__), c.reference(R9), c.reference(HELPER), dict(TOOL)]
        bindings['binary_paths'].append(TOOL['path'])
        native_root = '/home/loosqk/robot-checks/position-return-20261010-r1'
        with tempfile.TemporaryDirectory(prefix='head-load-memory-') as directory:
            path = Path(directory) / 'bindings.json'
            path.write_text(json.dumps(bindings))
            argv = ['--maintenance', str(ROOT / 'scripts/run_hardware_maintenance.py'), '--helper', str(HELPER),
                    '--reverse-helper', str(R9), '--bindings', str(path), '--output', native_root + '/MEMORY-r10',
                    '--run-id', 'head-load-memory-r10', '--artifact-root', native_root]
            with contextlib.redirect_stdout(io.StringIO()) as output, patch.object(c, 'install', side_effect=AssertionError('禁止执行')):
                self.assertEqual(c.main(argv), 0)
                old_go = R9.parent / 'head1-reverse-observation-r9.go.json'
                with self.assertRaisesRegex(ValueError, 'root当次GO'):
                    c.main(argv + ['--execute', '--approval', str(old_go)])
        plan = json.loads(output.getvalue())['plan']
        facts = plan['head1_load_observation']
        self.assertEqual(facts['load_tool'], TOOL)
        self.assertEqual(facts['load']['maximum_raw_reads'], 1)
        self.assertEqual((facts['load']['trigger_earliest_elapsed_ns'], facts['load']['trigger_latest_elapsed_ns'],
                          facts['load']['minimum_negative_excursion_raw'], facts['load']['minimum_remaining_ns']),
                         (300_000_000, 400_000_000, 12, 400_000_000))
        self.assertEqual((facts['fixed_p'], facts['fixed_d'], facts['fixed_i'], facts['parameter_writes']), (16, 43, 0, 0))
        self.assertEqual((plan['position_wait_ms'], plan['return_tolerance_raw'], plan['position_delta_raw']), (800, 4, -24))
        self.assertEqual(facts['return_witness'], 'UNMODIFIED_FROZEN_D97')


if __name__ == '__main__':
    unittest.main(verbosity=2)
