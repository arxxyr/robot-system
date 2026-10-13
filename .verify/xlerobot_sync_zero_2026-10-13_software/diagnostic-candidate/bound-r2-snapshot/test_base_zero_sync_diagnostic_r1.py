"""爸爸，单轮SYNC零值候选的少量纯内存合同；无SSH/UART/真实GO或控制工具执行。"""
import contextlib
import copy
import hashlib
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


def module(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


c = module(HERE / 'base_zero_sync_diagnostic_r1.py', 'sync_zero_candidate_memory')
h = module(HERE / 'fixtures/raw_right9_from_r14_r2.py', 'sync_zero_raw_helper_memory')
SYNC_REF = {'path': '/home/loosqk/robot-checks/MEMORY_ONLY/new_sync_poke', 'sha256': 'a' * 64, 'bytes': 123}


class MemoryIO:
    def __init__(self, m, *, latencies=None, nonzero=None, bad_frame=None, unknown_write=None, goal_mismatch=False, stop_nonzero=False, unknown_stop_read=None, stop_te_nonzero=False):
        self.m = m
        self.tool = h.TOOL
        self.clock = 0
        self.sequence = 0
        self.commands = []
        self.events = []
        self.values = dict(c.EXPECTED)
        self.latencies = latencies or [30_000_000] * 3
        self.observation_calls = 0
        self.nonzero = nonzero
        self.bad_frame = bad_frame
        self.unknown_write = unknown_write
        self.goal_mismatch = goal_mismatch
        self.stop_nonzero = stop_nonzero
        self.unknown_stop_read = unknown_stop_read
        self.stop_te_nonzero = stop_te_nonzero
        self.goal_written = False
        self.evidence_errors = []
        self.child = None

    def now(self):
        return self.clock

    def check(self):
        pass

    def sleep(self, seconds, cleanup=False):
        self.events.append(('sleep', self.clock, seconds, cleanup))
        self.clock += round(seconds * 1_000_000_000)

    def command(self, argv, **kwargs):
        self.sequence += 1
        self.events.append(('command', list(argv), kwargs))
        if argv[0] == c.SYNC_TOOL['path']:
            self.clock += 10_000_000
            self.values['Goal_Velocity'] = 0
            self.goal_written = True
            text = 'ACK_UNKNOWN\n' if self.unknown_write == ('Goal_Velocity', 0) else c.SYNC_STDOUT
        elif argv[0] == h.RAW_TOOL['path']:
            register = argv[4]
            value = self.values[register]
            if register == 'Torque_Enable' and value == 1:
                value = getattr(self, 'enable_readback', value)
            if register == 'Goal_Velocity' and self.goal_written and self.goal_mismatch:
                value = 1
            observing = register == 'Present_Velocity' and self.values['Torque_Enable'] == 1
            if observing:
                value = self.nonzero if self.nonzero is not None else 0
                self.clock += self.latencies[min(self.observation_calls, len(self.latencies) - 1)]
                self.observation_calls += 1
            else:
                self.clock += getattr(self, 'goal_read_latency', 10_000_000) if register == 'Goal_Velocity' and self.goal_written else 10_000_000
            address, width = h.META[register]
            tx = [255, 255, 254, 5, 130, address, width, 9]
            tx.append((~sum(tx[2:])) & 255)
            rx = [255, 255, 9, width + 2, 0, *int(value).to_bytes(width, 'little')]
            rx.append((~sum(rx[2:])) & 255)
            if self.bad_frame == register:
                rx[-1] ^= 1
            text = json.dumps({'operation': 'sync_read', 'serial_device': self.m.DEVICES['right'],
                'motor_id': 9, 'register': register, 'requested_register_address': address, 'width': width,
                'raw_unsigned': value, 'tx': tx, 'rx': rx}) + '\n'
        else:
            self.clock += 10_000_000
            action, register = argv[3:5]
            if action == 'write':
                value = int(argv[5])
                self.values[register] = value
                if register == 'Goal_Velocity':
                    self.goal_written = True
                if register == 'Torque_Enable' and value == 0:
                    self.values['Present_Velocity'] = 50 if self.stop_nonzero else 0
                text = 'ACK_UNKNOWN\n' if self.unknown_write == (register, value) else f'write {register}@9 <- {value} ok\n'
            else:
                value = getattr(self, 'initial_fault', {}).get((argv[1], argv[2], register), 1 if register == 'Torque_Enable' and self.stop_te_nonzero else self.values.get(register, 0))
                text = 'READ_IDENTITY_UNKNOWN\n' if register == self.unknown_stop_read else f'read {register}@9 = {value}\n'
        self.commands.append({'stdout_bytes': len(text.encode()), 'stdout_sha256': 'MEMORY_ONLY'})
        return text

    def poke(self, side, motor, action, register, value=None, cleanup=False):
        argv = [h.TOOL['path'], self.m.DEVICES[side], str(motor), action, register]
        if action == 'write':
            argv.append(str(value))
        text = self.command(argv, cleanup=cleanup)
        if action == 'read':
            return int(text.split(' = ')[1])
        if text != f'write {register}@{motor} <- {value} ok\n':
            raise ValueError('MEMORY_WRITE_ACK_UNKNOWN')


class SyncZeroCases(unittest.TestCase):
    def setUp(self):
        self.pin = patch.object(c, 'SYNC_TOOL', dict(SYNC_REF))
        self.pin.start()
        self.addCleanup(self.pin.stop)
        self.m = module(HERE / 'fixtures/run_hardware_maintenance.py', 'sync_original_memory')
        self.bindings = json.loads((HERE / 'fixtures/maintenance_bindings_base_zero_diagnostic_r4.json').read_text())
        self.own, self.helper = c.reference(Path(c.__file__)), c.reference(Path(h.__file__))
        self.bindings['refs'] += [self.own, self.helper, dict(SYNC_REF)]
        self.bindings['binary_paths'].append(SYNC_REF['path'])
        with patch.object(self.m, 'verify_ref'):
            self.plan = c.make_plan(self.m, h, self.bindings,
                                   '/home/loosqk/robot-checks/MEMORY_ONLY/sync-zero-run',
                                   '/home/loosqk/robot-checks/MEMORY_ONLY', self.own, self.helper)
        c.install(self.m, h)

    def engine(self, **faults):
        executor = MemoryIO(self.m, **faults)
        return self.m.ProbeEngine(executor, copy.deepcopy(self.plan))

    def run_diag(self, engine):
        with patch.object(self.m, 'verify_ref'), contextlib.redirect_stdout(io.StringIO()):
            return c.run_zero_diag(self.m, h, engine)

    def writes(self, engine):
        return [event[1] for event in engine.io.events if event[0] == 'command'
                and (event[1][0] == SYNC_REF['path']
                     or (event[1][0] == h.TOOL['path'] and event[1][3] == 'write'))]

    def go(self):
        now = 1_000_000_000_000
        facts = {key: True for key in self.m.FACTS}
        facts['confirmed_at_ns'] = now
        physical = {'left': dict(facts), 'right': dict(facts)}
        physical['right'].update(all_three_wheels_off_ground=True, base_stably_supported=True,
                                base_confirmation_kind='CURRENT_SETUP_READY',
                                base_confirmation_answer='MEMORY_ONLY，不是真实授权',
                                base_confirmation_scope=c.FIELD_SCOPE)
        return now, {'schema': 'MAINTENANCE_GO_V2', 'issued_by': 'root', 'issued_at_ns': now,
                     'expires_at_ns': now + c.TTL_NS, 'plan_sha256': self.m.digest(self.plan),
                     'artifact_root': self.plan['artifact_root'], 'host': self.bindings['host'],
                     'source': 'MEMORY_ONLY，不是真实授权', 'physical': physical}

    def test_unbound_default_missing_ref_and_old_go_before_execution(self):
        with patch.object(c, 'SYNC_TOOL', None), patch.object(c, 'load_frozen', side_effect=AssertionError('不得读输入')):
            with contextlib.redirect_stdout(io.StringIO()) as out:
                self.assertEqual(c.main([]), 0)
            self.assertEqual(json.loads(out.getvalue())['actions_started'], 0)
            with self.assertRaisesRegex(ValueError, '未绑定'):
                c.main(['--execute', '--approval', '/MEMORY_ONLY/old-go'])
        with patch.object(c, 'load_frozen', side_effect=AssertionError('缺GO不得读取输入')):
            with self.assertRaisesRegex(ValueError, '新架空就绪GO'):
                c.main(['--execute', '--maintenance', '/MEMORY_ONLY/m', '--raw-helper', '/MEMORY_ONLY/h',
                        '--bindings', '/MEMORY_ONLY/b', '--output', '/MEMORY_ONLY/o', '--artifact-root', '/MEMORY_ONLY'])
        for key in ('refs', 'binary_paths'):
            bad = copy.deepcopy(self.bindings)
            bad[key] = ([row for row in bad[key] if row['path'] != SYNC_REF['path']] if key == 'refs'
                        else [row for row in bad[key] if row != SYNC_REF['path']])
            with patch.object(self.m, 'verify_ref', side_effect=AssertionError('缺引用不得读取ELF')):
                with self.subTest(key=key), self.assertRaisesRegex(ValueError, 'refs及binary_paths'):
                    c.make_plan(self.m, h, bad, self.plan['output'], self.plan['artifact_root'], self.own, self.helper)
        now, go = self.go()
        self.assertEqual(len(self.bindings['refs']), 58)
        self.assertEqual(self.bindings['binary_paths'][-1], SYNC_REF['path'])
        self.m.validate_go(go, self.plan, now)
        for mutate in ('digest', 'scope'):
            old = copy.deepcopy(go)
            if mutate == 'digest':
                old['plan_sha256'] = 'OLD_GO'
            else:
                old['physical']['right']['base_confirmation_scope'] = '旧裸Goal0范围'
            with self.subTest(mutate=mutate), patch.object(self.m, 'execute', side_effect=AssertionError('不执行')):
                with self.assertRaises(ValueError):
                    self.m.validate_go(old, self.plan, now)

    def test_complete_sequence_fifty_ms_no_ack_three_writes_and_trace(self):
        for goal_read_latency in (10_000_000, 70_000_000):
            engine = self.engine()
            engine.io.goal_read_latency = goal_read_latency
            self.run_diag(engine)
            row = engine.zero_diag
            self.assertEqual(row['status'], 'PASS')
            self.assertFalse(engine.manual_cut)
            self.assertEqual(self.writes(engine), [
                [SYNC_REF['path'], self.m.DEVICES['right'], '9', 'sync-zero'],
                [h.TOOL['path'], self.m.DEVICES['right'], '9', 'write', 'Torque_Enable', '1'],
                [h.TOOL['path'], self.m.DEVICES['right'], '9', 'write', 'Torque_Enable', '0']])
            first, enable, disable = row['write_attempts']
            self.assertEqual((first['operation'], first['ack'], first['result']), ('SYNC_WRITE', 'NONE', 'sent_no_ACK'))
            self.assertGreaterEqual(enable['started_ns'] - row['sync_sent_finished_ns'], c.SYNC_SETTLE_NS)
            self.assertEqual(disable['cleanup'], True)
            obs = row['observation']
            self.assertEqual([sample['requested_offset_ns'] for sample in obs['samples']], list(c.SAMPLE_OFFSETS_NS))
            self.assertEqual(obs['finished_ns'], obs['started_ns'] + c.BUDGET_NS)
            self.assertEqual(obs['in_budget_pv0_samples'], 3)
        with tempfile.TemporaryDirectory(dir=HERE) as directory:
            class TraceIO:
                def __init__(self):
                    self.output = Path(directory)
                    self.sequence, self.commands = 0, []
                def command(self, argv, **kwargs):
                    self.sequence += 1
                    self.commands.append({})
                    Path(argv[argv.index('-o') + 1]).write_text('MEMORY_ONLY +++ exited with 0 +++\n')
                    return c.SYNC_STDOUT
            fake = SimpleNamespace(RealExecutor=TraceIO, DEVICES=self.m.DEVICES, verify_ref=lambda *args, **kw: None)
            c.install_write_capture(fake, h)
            traced = fake.RealExecutor()
            traced.command([SYNC_REF['path'], self.m.DEVICES['right'], '9', 'sync-zero'])
            trace = traced.commands[-1]['diagnostic_write_trace']
            self.assertEqual((trace['instruction'], trace['requested_register_address'], trace['width'], trace['motor_ids']),
                             (0x83, 46, 2, [9]))
            self.assertEqual(trace['ack'], 'NONE_SYNC_WRITE')

    def test_constructor_initial_left7_stop_failure_sticky_without_later_uart(self):
        for register, value in (('Present_Velocity', -50), ('Torque_Enable', 1), ('Goal_Velocity', 1)):
            engine = self.engine()
            engine.io.initial_fault = {(self.m.DEVICES['left'], '7', register): value}
            with self.subTest(register=register), contextlib.redirect_stdout(io.StringIO()), self.assertRaises(ValueError):
                engine.read('left', 7, register)
            self.assertTrue(engine.manual_cut)
            before = len(engine.io.events)
            for argv in ([h.TOOL['path'], self.m.DEVICES['left'], '7', 'read', 'Present_Velocity'],
                         [SYNC_REF['path'], self.m.DEVICES['right'], '9', 'sync-zero']):
                with self.assertRaisesRegex(ValueError, '粘性'):
                    engine.io.command(argv)
            self.assertEqual(len(engine.io.events), before)
            self.assertEqual(self.writes(engine), [])
        engine = self.engine(unknown_stop_read='Present_Velocity')
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(ValueError):
            engine.read('left', 7, 'Present_Velocity')
        self.assertTrue(engine.manual_cut)

    def test_no_old_gv_write_unknown_tool_or_other_target(self):
        illegal = [
            [h.TOOL['path'], self.m.DEVICES['right'], '9', 'write', 'Goal_Velocity', '0'],
            [SYNC_REF['path'], self.m.DEVICES['right'], '8', 'sync-zero'],
            [SYNC_REF['path'], self.m.DEVICES['right'], '9', 'sync-zero', '0'],
            ['/MEMORY_ONLY/unknown', self.m.DEVICES['right'], '9', 'sync-zero'],
            ['/MEMORY_ONLY/unknown', self.m.DEVICES['right'], '9', 'other-op'],
            [h.STRACE['path'], h.TOOL['path'], self.m.DEVICES['right'], '9', 'write', 'Goal_Velocity', '0'],
            [h.TOOL['path'], self.m.DEVICES['right'], '9', 'write', 'Phase', '0'],
        ]
        for argv in illegal:
            engine = self.engine()
            with self.subTest(argv=argv), contextlib.redirect_stdout(io.StringIO()), self.assertRaises(ValueError):
                engine.io.command(argv)
            self.assertEqual(engine.io.events, [])
            self.assertTrue(engine.manual_cut)
        executor = MemoryIO(self.m)
        def metadata(argv, **kwargs):
            executor.events.append(('metadata', list(argv)))
            return 'MEMORY_METADATA_ONLY'
        executor.command = metadata
        engine = self.m.ProbeEngine(executor, copy.deepcopy(self.plan))
        device = self.m.DEVICES['right']
        for argv in (['fuser', device], ['sudo', '-n', 'fuser', device],
                     ['udevadm', 'info', '--query=property', '--name', device]):
            self.assertEqual(engine.io.command(argv), 'MEMORY_METADATA_ONLY')
        self.assertEqual(len(executor.events), 3)
        self.assertFalse(engine.manual_cut)
        self.assertEqual(self.writes(engine), [])

    def test_valid_nonzero_pv_keeps_te0_cleanup_unknown_frame_or_ack_latches(self):
        for raw in (50, 0x8032):
            engine = self.engine(nonzero=raw)
            with self.subTest(raw=raw), self.assertRaises(ValueError):
                self.run_diag(engine)
            self.assertEqual(len(engine.zero_diag['observation']['samples']), 1)
            self.assertEqual(engine.zero_diag['observation']['samples'][0]['raw']['raw_unsigned'], raw)
            self.assertEqual(self.writes(engine)[-1][-2:], ['Torque_Enable', '0'])
            self.assertFalse(engine.manual_cut)
        for fault in ({'bad_frame': 'Model_Number'}, {'goal_mismatch': True},
                      {'unknown_write': ('Goal_Velocity', 0)}, {'unknown_write': ('Torque_Enable', 1)},
                      {'unknown_write': ('Torque_Enable', 0)}, {'stop_nonzero': True}):
            engine = self.engine(**fault)
            with self.subTest(fault=fault), self.assertRaises(ValueError):
                self.run_diag(engine)
            self.assertTrue(engine.manual_cut)
            before = len(engine.io.events)
            with self.assertRaisesRegex(ValueError, '粘性'):
                engine.io.command([h.RAW_TOOL['path'], self.m.DEVICES['right'], '9', 'read-raw', 'Present_Velocity'])
            self.assertEqual(len(engine.io.events), before)

    def test_lower_bound_expiry_and_original_deferred_stop_unknown(self):
        engine = self.engine()
        with patch.object(self.m, 'verify_ref'):
            engine.io.command([SYNC_REF['path'], self.m.DEVICES['right'], '9', 'sync-zero'])
        before = len(engine.io.events)
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaisesRegex(ValueError, '50ms'):
            engine.write('right', 9, 'Torque_Enable', 1)
        self.assertEqual(len(engine.io.events), before)
        # 只在内存模拟墙钟；调用生产闭包及原validate_go，不注入私有到期异常。
        for expiry_offset, expected_writes, manual in ((150_000_000, 1, True), (160_000_000, 3, False)):
            fresh = module(HERE / 'fixtures/run_hardware_maintenance.py', 'sync_expiry_memory')
            now, go = self.go()
            go['expires_at_ns'] = now + expiry_offset
            c.install(fresh, h, go)
            executor = MemoryIO(fresh)
            engine = fresh.ProbeEngine(executor, copy.deepcopy(self.plan))
            with patch.object(fresh, 'verify_ref'), patch.object(c.time, 'time_ns', side_effect=lambda: now + executor.now()):
                with self.subTest(expiry_offset=expiry_offset), contextlib.redirect_stdout(io.StringIO()), self.assertRaises(ValueError):
                    c.run_zero_diag(fresh, h, engine)
            self.assertEqual(len(self.writes(engine)), expected_writes)
            self.assertEqual(engine.manual_cut, manual)
            self.assertEqual(engine.zero_diag['status'], 'FAIL')
            if not manual:
                self.assertEqual(self.writes(engine)[-1][-2:], ['Torque_Enable', '0'])
        # TE读回不符优先于同一返回点的数值过期，原粘性门不能被到期掩盖。
        for te_raw in (0, 2):
            fresh = module(HERE / 'fixtures/run_hardware_maintenance.py', 'sync_expiry_bad_te_memory')
            now, go = self.go()
            go['expires_at_ns'] = now + 170_000_000
            c.install(fresh, h, go)
            executor = MemoryIO(fresh)
            executor.enable_readback = te_raw
            engine = fresh.ProbeEngine(executor, copy.deepcopy(self.plan))
            with patch.object(fresh, 'verify_ref'), patch.object(c.time, 'time_ns', side_effect=lambda: now + executor.now()):
                with self.subTest(te_raw=te_raw), contextlib.redirect_stdout(io.StringIO()), self.assertRaises(ValueError):
                    c.run_zero_diag(fresh, h, engine)
            self.assertTrue(engine.manual_cut)
            self.assertEqual(len(self.writes(engine)), 2)
            self.assertFalse(any(attempt['cleanup'] for attempt in engine.zero_diag['write_attempts']))
        engine = self.engine()
        with patch.object(self.m, 'verify_ref'), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(ValueError, 'BASE_FIRMWARE_CONTRACT_UNVERIFIED'):
                self.m.run_motion_probes(engine, self.bindings, self.plan)
        self.assertEqual(engine.zero_diag['status'], 'PASS')
        self.assertTrue(all(row['decision'] == 'DEFERRED' and row['status'] == 'NOT_RUN' for row in engine.rows))
        report = {'probes': engine.rows, 'plan': self.plan, 'stop_verification': {'status': 'PASS'},
                  'identity_verification': {'status': 'PASS'}, 'position_verification': {'status': 'PASS'}}
        self.assertEqual(self.m.classify_run_result(report), 'FAIL')
        fresh = module(HERE / 'fixtures/run_hardware_maintenance.py', 'sync_finish_memory')
        fresh.finish_verification = lambda report, *args: report.update(stop_verification={'status': 'UNKNOWN'},
                                                                       identity_verification={'status': 'PASS'})
        c.install(fresh, h)
        with contextlib.redirect_stdout(io.StringIO()):
            fresh.finish_verification(report, engine.io, engine, self.bindings, None)
        self.assertTrue(engine.manual_cut)
        self.assertEqual(report['stop_verification']['status'], 'UNKNOWN')


if __name__ == '__main__':
    unittest.main(verbosity=2)


