"""爸爸，八个定向内存方法；无串口、SSH、真实工具child或现场事实。"""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
ROOT = next(p for p in HERE.parents if (p / 'scripts/run_hardware_maintenance.py').is_file())


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


c = load('readonly15_candidate_memory', HERE / 'readonly15_r7.py')
m = load('readonly15_existing_executor_definitions', ROOT / 'scripts/run_hardware_maintenance.py')
FAKE_TOOL = {'path': '/MEMORY_ONLY/new-tool', 'sha256': '0' * 64, 'bytes': 1}


def frame(motor, name, value=0):
    address, width = c.META[name]
    tx = [255, 255, 254, 5, 130, address, width, motor]
    tx.append((~sum(tx[2:])) & 255)
    rx = [255, 255, motor, width + 2, 0, *value.to_bytes(width, 'little')]
    rx.append((~sum(rx[2:])) & 255)
    return {'operation': 'sync_read', 'serial_device': m.DEVICES['left'], 'motor_id': motor,
            'register': name, 'requested_register_address': address, 'width': width,
            'raw_unsigned': value, 'tx': tx, 'rx': rx}


class MemoryIO:
    def __init__(self, alter=None, failure=None):
        self.argv = []
        self.sequence = 0
        self.child = None
        self.evidence_errors = []
        self.alter = alter
        self.failure = failure
        self.interrupted = None

    def check(self):
        c.require(self.interrupted is None, 'MEMORY_SIGNAL')

    def command(self, argv):
        self.argv.append(argv)
        self.sequence += 1
        if self.failure:
            raise ValueError('MEMORY_CHILD_FAILED')
        motor, name = int(argv[-3]), argv[-1]
        value = {'Minimum_Starting_Torque': 32, 'Integral_Limit': 8,
                 'Angle_Resolution': 1}.get(name, 0)
        row = frame(motor, name, value)
        if self.alter:
            row = self.alter(self.sequence, row)
        return json.dumps(row) + '\n'


def report():
    return {'readouts': [], 'uart_locked_out': False, 'manual_power_cut_requested': False}


class Fixed15MemoryCases(unittest.TestCase):
    def test_default_plan_only_has_no_loader_file_or_executor(self):
        with patch.object(c, 'load_support', side_effect=AssertionError('不得加载')), \
                patch.object(c, 'own_reference', side_effect=AssertionError('不得读取源码')), \
                contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(c.main([]), 0)
        row = json.loads(output.getvalue())
        self.assertEqual(row['status'], 'PLAN_ONLY')
        self.assertEqual(row['tool'], c.APPROVED_TOOL)
        self.assertFalse(row['output_created'])
        self.assertEqual(row['actions_started'], 0)

    def test_unbound_tool_or_missing_current_refs_reject_before_loader(self):
        with tempfile.TemporaryDirectory(dir=HERE) as directory:
            path = Path(directory) / 'config.json'
            path.write_text((HERE / 'config-template-r7.json').read_text())
            with patch.object(c, 'load_support', side_effect=AssertionError('不得加载')):
                with patch.object(c, 'APPROVED_TOOL', None):
                    with self.assertRaisesRegex(ValueError, '尚未绑定'):
                        c.main(['--config', str(path), '--execute'])
                config = json.loads(path.read_text())
                for missing in ('field_confirmation', 'approval', 'bindings', 'diagnostic_tool'):
                    complete = {**config, **{key: {'MEMORY_REF': True} for key in
                                ('field_confirmation', 'approval', 'bindings', 'diagnostic_tool')}}
                    complete[missing] = None
                    path.write_text(json.dumps(complete))
                    with self.subTest(missing=missing), patch.object(c, 'APPROVED_TOOL', FAKE_TOOL):
                        with self.assertRaisesRegex(ValueError, '缺当次'):
                            c.main(['--config', str(path), '--execute'])
            self.assertFalse((Path(directory) / 'output').exists())

    def test_expired_field_has_zero_read_starts(self):
        receipt = {'recorded_at_ns': 10, 'scope': c.FIELD_SCOPE, 'answer': 'MEMORY_ONLY',
                   'physical': dict.fromkeys(c.FACTS, True)}
        executor, result = MemoryIO(), report()
        with patch.object(m, 'verify_ref', return_value=json.dumps(receipt).encode()):
            gate = lambda: c.confirmation(m, {'MEMORY_REF': True}, 10 + c.TTL_NS + 1)
            with self.assertRaisesRegex(ValueError, '时效'):
                c.run_reads(m, executor, FAKE_TOOL, gate, result)
        self.assertEqual(executor.argv, [])
        self.assertTrue(result['uart_locked_out'])

    def test_fifteen_unique_reads_all_six_stop_fields_before_parameters(self):
        executor, result = MemoryIO(), report()
        c.run_reads(m, executor, FAKE_TOOL, lambda: None, result)
        self.assertEqual(len(executor.argv), 15)
        self.assertEqual(len(set(c.READS)), 15)
        self.assertEqual(c.READS[:6], tuple((motor, name) for motor in (5, 7, 8)
                         for name in ('Torque_Enable', 'Present_Velocity')))
        for argv, (motor, name) in zip(executor.argv, c.READS):
            self.assertEqual(argv, [FAKE_TOOL['path'], m.DEVICES['left'], str(motor), 'read-raw', name])
        self.assertEqual(len(result['readouts']), 15)
        self.assertFalse(result['uart_locked_out'])
        self.assertTrue(all(row['raw']['raw_unsigned'] == 0 for row in result['readouts'][:6]))

    def test_nonzero_TE_or_PV_stops_once_and_remains_latched(self):
        for call, value in ((1, 1), (2, 50), (2, 32768)):
            def alter(seq, row):
                return frame(row['motor_id'], row['register'], value) if seq == call else row
            executor, result = MemoryIO(alter=alter), report()
            with self.subTest(call=call, value=value), self.assertRaisesRegex(ValueError, 'TE/PV非零'):
                c.run_reads(m, executor, FAKE_TOOL, lambda: None, result)
            self.assertEqual(len(executor.argv), call)
            self.assertTrue(result['manual_power_cut_requested'])
            with self.assertRaisesRegex(ValueError, '粘性'):
                c.run_reads(m, executor, FAKE_TOOL, lambda: None, result)
            self.assertEqual(len(executor.argv), call)

    def test_unknown_or_wrong_frame_stops_before_second_read(self):
        def wrong_checksum(row):
            row['rx'][-1] ^= 1
            return row
        for change in (wrong_checksum, lambda r: {**r, 'motor_id': 7},
                       lambda r: {**r, 'requested_register_address': 25},
                       lambda r: {**r, 'raw_unsigned': None}):
            executor = MemoryIO(alter=lambda seq, row: change(row))
            result = report()
            with self.assertRaises(ValueError):
                c.run_reads(m, executor, FAKE_TOOL, lambda: None, result)
            self.assertEqual(len(executor.argv), 1)
            self.assertTrue(result['uart_locked_out'])
            self.assertTrue(result['manual_power_cut_requested'])

    def test_child_signal_or_evidence_failure_never_starts_second_read(self):
        executor, result = MemoryIO(failure=True), report()
        with self.assertRaisesRegex(ValueError, 'CHILD_FAILED'):
            c.run_reads(m, executor, FAKE_TOOL, lambda: None, result)
        self.assertEqual(len(executor.argv), 1)
        self.assertTrue(result['uart_locked_out'])
        for defect in ('signal', 'evidence'):
            executor, result = MemoryIO(), report()
            if defect == 'signal':
                executor.interrupted = 15
            else:
                executor.evidence_errors = ['MEMORY_EVIDENCE_FAILURE']
            def gate():
                executor.check()
                c.require(not executor.evidence_errors, 'MEMORY_EVIDENCE')
            with self.subTest(defect=defect), self.assertRaises(ValueError):
                c.run_reads(m, executor, FAKE_TOOL, gate, result)
            self.assertEqual(executor.argv, [])

    def test_existing_R6_motion_GO_cannot_authorize_new_fifteen_plan(self):
        bindings = {'schema': 'MAINTENANCE_BINDINGS_V1',
                    'tool': {'path': '/MEMORY_ONLY/f201', 'sha256': m.TOOL_SHA, 'bytes': m.TOOL_BYTES},
                    'usb': {side: {'device': m.DEVICES[side], 'serial': m.SERIALS[side]} for side in m.SERIALS},
                    'host': {'MEMORY_HOST': True}}
        config = {'artifact_root': '/home/loosqk/robot-checks/memory-only-r7',
                  'output': '/home/loosqk/robot-checks/memory-only-r7/evidence',
                  'run_id': 'readonly-parameters-r7', 'maintenance': {}, 'support': {},
                  'field_confirmation': None, 'diagnostic_tool': FAKE_TOOL}
        plan = c.make_plan(m, config, {'MEMORY_CONFIG': True}, {'MEMORY_SOURCE': True}, bindings)
        old = m.strict_json((ROOT / '.verify/position_return_2026-10-10_r6/preparation/head1-p24-wire-r6.go.json').read_text())
        with self.assertRaisesRegex(ValueError, 'root当次GO'):
            m.validate_go(old, plan, old['issued_at_ns'])
        self.assertEqual(m.position_targets_from_plan(plan), m.select_position_targets(['left-wrist', 'head-1', 'head-2']))
        self.assertNotIn('position_delta_raw', plan)
        self.assertEqual(plan['register_writes'], 0)


if __name__ == '__main__':
    unittest.main(verbosity=2)
