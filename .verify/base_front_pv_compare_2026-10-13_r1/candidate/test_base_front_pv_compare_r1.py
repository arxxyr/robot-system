"""爸爸，最多三个纯内存方法；无网络、真实UART/GO或工具执行。"""
import contextlib
import copy
import importlib.util
import io
import json
from pathlib import Path
import stat
import sys
import tempfile
from types import SimpleNamespace
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


c = load('front_compare_memory', HERE / 'base_front_pv_compare_r1.py')
m = load('front_frozen_d97_memory', HERE / 'fixtures/run_hardware_maintenance.py')


def frame(register, value=0):
    address, width = c.META[register]
    tx = [255, 255, 254, 5, 130, address, width, 8]
    tx.append((~sum(tx[2:])) & 255)
    rx = [255, 255, 8, width + 2, 0, *value.to_bytes(width, 'little')]
    rx.append((~sum(rx[2:])) & 255)
    return {'operation': 'sync_read', 'serial_device': m.DEVICES['right'], 'motor_id': 8,
            'register': register, 'requested_register_address': address, 'width': width,
            'raw_unsigned': value, 'tx': tx, 'rx': rx}


def report():
    return {'readouts': [], 'uart_locked_out': False, 'manual_power_cut_requested': False}


class MemoryIO:
    def __init__(self, output, fault=None):
        self.output = Path(output)
        self.fault = fault
        self.argv = []
        self.sequence = 0
        self.child = None
        self.evidence_errors = []

    def check(self):
        pass

    def command(self, argv):
        self.argv.append(list(argv))
        self.sequence += 1
        trace = Path(argv[argv.index('-o') + 1])
        trace.write_text('MEMORY_ONLY +++ exited with 0 +++\n')
        operation, register = argv[-2:]
        if operation == 'read':
            return 'UNKNOWN\n' if self.fault == 'ordinary_unknown' else 'read Present_Velocity@8 = 0\n'
        value = 0
        if isinstance(self.fault, tuple) and self.fault[0] == register:
            value = self.fault[1]
        row = frame(register, value)
        if self.fault == 'bad_checksum':
            row['rx'][-1] ^= 1
        return json.dumps(row) + '\n'


class FrontCompareMemoryCases(unittest.TestCase):
    def test_default_unbound_new_inputs_and_frozen_go_compatibility(self):
        with patch.object(c, 'load_support', side_effect=AssertionError('不得加载')), \
                patch.object(c, 'own_reference', side_effect=AssertionError('不得读源码')):
            with contextlib.redirect_stdout(io.StringIO()) as out:
                self.assertEqual(c.main([]), 0)
            self.assertEqual(json.loads(out.getvalue())['actions_started'], 0)
            with patch.object(Path, 'read_bytes', side_effect=AssertionError('不得读config')):
                with self.assertRaisesRegex(ValueError, '读取config前零I/O'):
                    c.main(['--execute', '--config', '/MEMORY_ONLY/missing-config'])
            with tempfile.TemporaryDirectory(dir=HERE) as directory:
                path = Path(directory) / 'config.json'
                template = json.loads((HERE / 'config-template-r1.json').read_text())
                with patch.object(c, 'APPROVED_TOOL', dict(c.FROZEN_TOOL)):
                    for key in ('approval', 'field_confirmation', 'current_usb', 'current_tool_stat'):
                        config = copy.deepcopy(template)
                        for name in ('approval', 'field_confirmation', 'current_usb', 'current_tool_stat', 'bindings', 'diagnostic_tool'):
                            config[name] = {'MEMORY_ONLY': True}
                        config[key] = None
                        path.write_text(json.dumps(config))
                        with self.subTest(key=key), self.assertRaisesRegex(ValueError, '缺当次'):
                            c.main(['--execute', '--config', str(path)])
        # 模拟合法完整stat，只验证已有tool_identity；不读取或补真实Orin元数据。
        approved = {**c.FROZEN_TOOL, 'uid': 1000, 'gid': 1000, 'mode': 509,
                    'device': 1, 'inode': 2, 'mtime_ns': 3, 'ctime_ns': 4,
                    'nlink': 1, 'symlink': False, 'elf_machine': 183}
        info = SimpleNamespace(st_uid=1000, st_gid=1000, st_mode=stat.S_IFREG | 509,
                               st_dev=1, st_ino=2, st_mtime_ns=3, st_ctime_ns=4, st_nlink=1)
        raw = bytearray(64)
        raw[18:20] = (183).to_bytes(2, 'little')
        with patch.object(c, 'APPROVED_TOOL', approved), patch.object(m, 'verify_ref', return_value=bytes(raw)), \
                patch.object(Path, 'lstat', return_value=info), patch.object(Path, 'is_symlink', return_value=False):
            c.tool_identity(m, approved)
        bindings = json.loads((ROOT / '.verify/base_zero_sync_diagnostic_2026-10-13_r1_hardware/originals/maintenance_bindings_base_zero_sync_diagnostic_r1.json').read_text())
        config = json.loads((HERE / 'config-template-r1.json').read_text())
        config_ref = {'path': '/MEMORY_ONLY/config', 'sha256': '0' * 64, 'bytes': 1}
        plan = c.make_plan(m, config, config_ref, config_ref, bindings)
        self.assertTrue(plan['position_targets'])
        self.assertEqual(plan['position_targets_use'], 'FROZEN_GO_IDENTITY_METADATA_ONLY_NO_POSITION_OPERATION')
        now = 1_000_000_000_000
        facts = {key: True for key in m.FACTS}
        facts['confirmed_at_ns'] = now
        go = {'schema': 'MAINTENANCE_GO_V2', 'issued_by': 'root', 'issued_at_ns': now,
              'expires_at_ns': now + c.TTL_NS, 'artifact_root': plan['artifact_root'],
              'plan_sha256': m.digest(plan), 'host': bindings['host'], 'source': 'MEMORY_ONLY',
              'physical': {'left': dict(facts), 'right': dict(facts)}}
        m.validate_go(go, plan, now)
        with self.assertRaises(ValueError):
            m.validate_go({**go, 'plan_sha256': 'OLD_GO'}, plan, now)

    def test_three_zero_reads_and_comparison_without_motion_or_replay(self):
        with tempfile.TemporaryDirectory(dir=HERE) as directory:
            executor, result = MemoryIO(directory), report()
            c.run_reads(m, executor, c.FROZEN_TOOL, lambda: None, result)
            self.assertEqual(len(executor.argv), 3)
            self.assertEqual([argv[-5:] for argv in executor.argv], [
                [c.FROZEN_TOOL['path'], m.DEVICES['right'], '8', 'read-raw', 'Torque_Enable'],
                [c.FROZEN_TOOL['path'], m.DEVICES['right'], '8', 'read-raw', 'Present_Velocity'],
                [c.FROZEN_TOOL['path'], m.DEVICES['right'], '8', 'read', 'Present_Velocity']])
            self.assertTrue(all(row['trace']['bytes'] > 0 for row in result['readouts']))
            self.assertTrue(result['comparison']['representation_matches'])
            self.assertEqual(result['comparison']['physical_motion'], 'UNKNOWN')
            with self.assertRaisesRegex(ValueError, '只允许一次'):
                c.run_reads(m, executor, c.FROZEN_TOOL, lambda: None, result)
            self.assertEqual(len(executor.argv), 3)
        # 仅展示已有两输出的数值表示；没有绕过生产非零门采集这两对。
        for raw, ordinary in ((50, 50), (0x8032, -50)):
            comparison = c.comparison_from_readouts([
                {'operation': 'read-raw', 'register': 'Present_Velocity', 'raw': {'raw_unsigned': raw}},
                {'operation': 'read', 'register': 'Present_Velocity', 'decoded_integer': ordinary}])
            self.assertEqual(comparison['sync_raw_unsigned'], raw)
            self.assertEqual(comparison['ordinary_decoded_integer'], ordinary)
            self.assertTrue(comparison['representation_matches'])
            self.assertFalse(comparison['sampling_same_time'])

    def test_nonzero_unknown_or_expired_gate_sticky_without_pair_or_retry(self):
        for fault, count in ((('Torque_Enable', 1), 1), (('Present_Velocity', 50), 2),
                             (('Present_Velocity', 0x8032), 2), ('bad_checksum', 1), ('ordinary_unknown', 3)):
            with tempfile.TemporaryDirectory(dir=HERE) as directory:
                executor, result = MemoryIO(directory, fault), report()
                with self.subTest(fault=fault), self.assertRaises(ValueError):
                    c.run_reads(m, executor, c.FROZEN_TOOL, lambda: None, result)
                self.assertEqual(len(executor.argv), count)
                self.assertTrue(result['uart_locked_out'])
                self.assertTrue(result['manual_power_cut_requested'])
                with self.assertRaisesRegex(ValueError, '粘性'):
                    c.run_reads(m, executor, c.FROZEN_TOOL, lambda: None, result)
                self.assertEqual(len(executor.argv), count)
                self.assertNotIn('RECORDED_OUTPUTS_COMPARISON', result.get('comparison', {}).values())
        with tempfile.TemporaryDirectory(dir=HERE) as directory:
            executor, result = MemoryIO(directory), report()
            with self.assertRaises(ValueError):
                c.run_reads(m, executor, c.FROZEN_TOOL,
                            lambda: c.require(False, 'MEMORY_GO_EXPIRED'), result)
            self.assertEqual(executor.argv, [])
            self.assertTrue(result['uart_locked_out'])


if __name__ == '__main__':
    unittest.main(verbosity=2)
