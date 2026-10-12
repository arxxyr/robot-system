"""爸爸，固定十项纯内存合同；不打开串口，不生成GO，不执行SSH或工具。"""
import ast
import contextlib
import copy
import importlib.util
import io
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
ROOT = next(p for p in HERE.parents if (p / 'scripts/run_hardware_maintenance.py').is_file())


def module(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


c = module(HERE / 'base_zero_diagnostic_r1.py', 'zero_diag_candidate_memory')
h = module(HERE / 'raw_right9_from_r14_r1.py', 'zero_diag_raw_helper_memory')


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
        if argv[0] == h.RAW_TOOL['path']:
            register = argv[4]
            value = self.values[register]
            if register == 'Goal_Velocity' and self.goal_written and self.goal_mismatch:
                value = 1
            observing = register == 'Present_Velocity' and self.values['Torque_Enable'] == 1
            if observing:
                value = self.nonzero if self.nonzero is not None else 0
                self.clock += self.latencies[min(self.observation_calls, len(self.latencies) - 1)]
                self.observation_calls += 1
            else:
                self.clock += 10_000_000
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
                value = 1 if register == 'Torque_Enable' and self.stop_te_nonzero else self.values.get(register, 0)
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


class ZeroCases(unittest.TestCase):
    def setUp(self):
        self.m = module(ROOT / 'scripts/run_hardware_maintenance.py', 'zero_diag_original_fresh_memory')
        self.bindings = json.loads((ROOT / 'target/position_return_2026-10-11_r17/native-preparation/maintenance_bindings_head2_p32_reverse_r17.json').read_text())
        self.bindings['refs'] += [c.reference(Path(c.__file__)), c.reference(Path(h.__file__))]
        self.plan = c.make_plan(self.m, h, self.bindings, '/home/loosqk/robot-checks/MEMORY_ONLY/zero-run',
                                '/home/loosqk/robot-checks/MEMORY_ONLY', c.reference(Path(c.__file__)), c.reference(Path(h.__file__)))

    def engine(self, **faults):
        executor = MemoryIO(self.m, **faults)
        return self.m.ProbeEngine(executor, copy.deepcopy(self.plan))

    def run_diag(self, engine):
        with patch.object(self.m, 'verify_ref'):
            return c.run_zero_diag(self.m, h, engine)

    def writes(self, engine):
        return [event[1][4:] for event in engine.io.events if event[0] == 'command'
                and event[1][0] == h.TOOL['path'] and event[1][3] == 'write']

    def go(self):
        now = 1_000_000_000_000
        facts = {key: True for key in self.m.FACTS}
        facts['confirmed_at_ns'] = now
        physical = {'left': dict(facts), 'right': dict(facts)}
        physical['right'].update(all_three_wheels_off_ground=True, base_stably_supported=True,
                base_confirmation_kind='CURRENT_SETUP_READY', base_confirmation_answer='MEMORY_ONLY 当前就绪夹具',
                base_confirmation_scope=c.FIELD_SCOPE)
        return now, {'schema': 'MAINTENANCE_GO_V2', 'issued_by': 'root', 'issued_at_ns': now,
                     'expires_at_ns': now + c.TTL_NS, 'plan_sha256': self.m.digest(self.plan),
                     'artifact_root': self.plan['artifact_root'], 'host': self.bindings['host'],
                     'source': 'MEMORY_ONLY 非真实授权', 'physical': physical}

    def test_default_missing_go_refuse_before_loading(self):
        with patch.object(c, 'load_frozen', side_effect=AssertionError('零I/O')), contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(c.main([]), 0)
            with self.assertRaisesRegex(ValueError, '输入不完整'):
                c.main(['--execute'])
            with self.assertRaisesRegex(ValueError, '新架空就绪GO'):
                c.main(['--execute', '--maintenance', '/MEMORY_ONLY/m', '--raw-helper', '/MEMORY_ONLY/h',
                        '--bindings', '/MEMORY_ONLY/b', '--output', '/MEMORY_ONLY/o', '--artifact-root', '/MEMORY_ONLY'])
        self.assertEqual(json.loads(output.getvalue())['actions_started'], 0)

    def test_empty_position_and_current_airborne_go_strictly_bound(self):
        c.install(self.m, h)
        now, go = self.go()
        self.m.validate_go(go, self.plan, now)
        self.assertEqual(self.m.position_targets_from_plan(self.plan), [])
        for key, value in (('all_three_wheels_off_ground', False), ('base_stably_supported', False),
                           ('base_confirmation_kind', 'CAPABILITY_ONLY'), ('base_confirmation_scope', '旧范围'),
                           ('base_confirmation_answer', ''), ('confirmed_at_ns', now - c.TTL_NS - 1)):
            bad = copy.deepcopy(go); bad['physical']['right'][key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                self.m.validate_go(bad, self.plan, now)
        bad = copy.deepcopy(go); bad['plan_sha256'] = 'OLD_GO'
        with self.assertRaises(ValueError):
            self.m.validate_go(bad, self.plan, now)
        bad = copy.deepcopy(self.plan); bad['position_targets'] = [['left', 8, 'head_motor_2']]
        with self.assertRaises(ValueError):
            self.m.position_targets_from_plan(bad)
        self.assertFalse(self.plan['firmware_contract_verified'])
        self.assertTrue(all(row['decision'] == 'DEFERRED' for row in self.plan['base_targets']))

    def test_original_raw_frame_retarget_and_bad_frame_stop(self):
        engine = self.engine()
        self.run_diag(engine)
        self.assertEqual(set(h.META), set(c.EXPECTED))
        for row in engine.zero_diag['before_raw']:
            self.assertEqual((row['raw']['motor_id'], row['raw']['serial_device']), (9, self.m.DEVICES['right']))
        for register in ('Model_Number', 'Present_Velocity'):
            failed = self.engine(bad_frame=register)
            with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(ValueError):
                self.run_diag(failed)
            self.assertTrue(failed.manual_cut)
            self.assertEqual(self.writes(failed), [])

    def test_happy_fixed_distributed_samples_and_only_three_writes(self):
        engine = self.engine(); self.run_diag(engine)
        row = engine.zero_diag; observation = row['observation']
        self.assertEqual(row['status'], 'PASS')
        self.assertFalse(engine.manual_cut)
        self.assertEqual(self.writes(engine), [['Goal_Velocity', '0'], ['Torque_Enable', '1'], ['Torque_Enable', '0']])
        self.assertEqual(len(observation['samples']), 3)
        self.assertEqual([s['started_ns'] - observation['started_ns'] for s in observation['samples']], [0, 200_000_000, 400_000_000])
        self.assertEqual(observation['deadline_ns'], observation['started_ns'] + c.BUDGET_NS)
        self.assertEqual(observation['finished_ns'], observation['deadline_ns'])
        self.assertEqual(observation['in_budget_pv0_samples'], 3)
        self.assertFalse(row['firmware_contract_verified'])

    def test_valid_nonzero_raw_immediate_single_disable_no_retry(self):
        for raw in (50, 0x8032):
            engine = self.engine(nonzero=raw)
            with self.subTest(raw=raw), self.assertRaises(ValueError):
                self.run_diag(engine)
            self.assertEqual(len(engine.zero_diag['observation']['samples']), 1)
            self.assertEqual(engine.zero_diag['observation']['samples'][0]['raw']['raw_unsigned'], raw)
            self.assertEqual(self.writes(engine)[-1], ['Torque_Enable', '0'])
            self.assertFalse(engine.manual_cut)

    def test_original_deadline_late_or_missed_slot_keeps_fail_no_makeup(self):
        for latencies, requests in (([30_000_000, 30_000_000, 101_000_000], 3), ([201_000_000], 1)):
            engine = self.engine(latencies=latencies)
            with self.subTest(latencies=latencies), self.assertRaises(ValueError):
                self.run_diag(engine)
            self.assertEqual(engine.io.observation_calls, requests)
            self.assertEqual(engine.zero_diag['status'], 'FAIL')
            self.assertEqual(self.writes(engine)[-1], ['Torque_Enable', '0'])
            self.assertFalse(engine.manual_cut)
            if requests == 3:
                self.assertFalse(engine.zero_diag['observation']['samples'][-1]['completed_in_budget'])

    def test_uncertain_writes_latch_and_forbid_every_later_uart(self):
        for operation in c.WRITE_SEQUENCE:
            engine = self.engine(unknown_write=operation)
            with self.subTest(operation=operation), contextlib.redirect_stdout(io.StringIO()), self.assertRaises(ValueError):
                self.run_diag(engine)
            self.assertTrue(engine.manual_cut)
            before = len(engine.io.events)
            for argv in ([h.RAW_TOOL['path'], self.m.DEVICES['right'], '9', 'read-raw', 'Present_Velocity'],
                         [h.TOOL['path'], self.m.DEVICES['right'], '9', 'write', 'Torque_Enable', '0']):
                with self.assertRaisesRegex(ValueError, '粘性'):
                    engine.io.command(argv, cleanup=True)
            self.assertEqual(len(engine.io.events), before)

    def test_metadata_goal_readback_and_stop_failures_latch(self):
        for register in c.EXPECTED:
            engine = self.engine(); engine.io.values[register] += 1
            with self.subTest(register=register), contextlib.redirect_stdout(io.StringIO()), self.assertRaises(ValueError):
                self.run_diag(engine)
            self.assertTrue(engine.manual_cut); self.assertEqual(self.writes(engine), [])
        for faults in ({'goal_mismatch': True}, {'stop_nonzero': True},
                       {'unknown_stop_read': 'Torque_Enable'}, {'unknown_stop_read': 'Present_Velocity'},
                       {'stop_te_nonzero': True}):
            engine = self.engine(**faults)
            with self.subTest(faults=faults), contextlib.redirect_stdout(io.StringIO()), self.assertRaises(ValueError):
                self.run_diag(engine)
            self.assertTrue(engine.manual_cut)
            if faults.get('unknown_stop_read') == 'Torque_Enable' or faults.get('stop_te_nonzero'):
                ordinary = [event[1][4] for event in engine.io.events if event[0] == 'command'
                            and event[1][0] == h.TOOL['path'] and event[1][3] == 'read']
                self.assertEqual(ordinary, ['Torque_Enable'])
        engine = self.engine()
        engine.check_gate = lambda: (_ for _ in ()).throw(ValueError('MEMORY_IDENTITY_UNKNOWN'))
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(ValueError):
            self.run_diag(engine)
        self.assertTrue(engine.manual_cut)
        self.assertEqual(self.writes(engine), [])

    def test_write_whitelist_no_other_motor_nonzero_or_seven_byte_path(self):
        engine = self.engine(); self.run_diag(engine)
        for argv in ([h.TOOL['path'], self.m.DEVICES['right'], '8', 'write', 'Torque_Enable', '1'],
                     [h.TOOL['path'], self.m.DEVICES['right'], '9', 'write', 'Goal_Velocity', '20'],
                     [h.TOOL['path'], self.m.DEVICES['right'], '9', 'write', 'Goal_Position', '0'],
                     [h.RAW_TOOL['path'], self.m.DEVICES['right'], '9', 'write', 'Phase', '0'],
                     [h.RAW_TOOL['path'], self.m.DEVICES['right'], '9', 'read-raw', 'P_Coefficient'],
                     [h.STRACE['path'], h.TOOL['path'], self.m.DEVICES['right'], '9', 'write', 'Goal_Velocity', '0']):
            with self.subTest(argv=argv), self.assertRaises(ValueError):
                engine.io.command(argv)
        for terminal in (0, 1):
            with tempfile.TemporaryDirectory() as directory:
                class TraceMemoryIO:
                    def __init__(self):
                        self.output = Path(directory); self.sequence = 0; self.commands = []
                    def command(self, argv, **kwargs):
                        self.sequence += 1; self.commands.append({})
                        Path(argv[argv.index('-o') + 1]).write_text(f'MEMORY_ONLY +++ exited with {terminal} +++\n')
                        return 'write Goal_Velocity@9 <- 0 ok\n'
                fake = SimpleNamespace(RealExecutor=TraceMemoryIO, DEVICES=self.m.DEVICES, verify_ref=lambda *a, **k: None)
                c.install_write_capture(fake, h)
                traced = fake.RealExecutor()
                argv = [h.TOOL['path'], self.m.DEVICES['right'], '9', 'write', 'Goal_Velocity', '0']
                if terminal:
                    with self.assertRaisesRegex(ValueError, '未记录成功终态'):
                        traced.command(argv)
                else:
                    traced.command(argv)
                    self.assertEqual(traced.commands[-1]['diagnostic_write_trace']['width'], 2)
                    self.assertEqual(traced.commands[-1]['diagnostic_write_trace']['requested_register_address'], 46)

    def test_diag_separate_from_deferred_rows_and_original_finish_identity(self):
        c.install(self.m, h)
        engine = self.engine()
        with patch.object(self.m, 'verify_ref'), patch.object(self.m.ProbeEngine, 'wheel', side_effect=AssertionError('禁止wheel')), patch.object(self.m.ProbeEngine, 'pulse', side_effect=AssertionError('禁止pulse')):
            with self.assertRaisesRegex(ValueError, 'BASE_FIRMWARE_CONTRACT_UNVERIFIED'):
                self.m.run_motion_probes(engine, self.bindings, self.plan)
        self.assertEqual(engine.zero_diag['status'], 'PASS')
        self.assertEqual(len(engine.rows), 3)
        self.assertTrue(all(row['decision'] == 'DEFERRED' and row['status'] == 'NOT_RUN' for row in engine.rows))
        report = {'probes': engine.rows, 'plan': self.plan, 'stop_verification': {'status': 'PASS'},
                  'identity_verification': {'status': 'PASS'}, 'position_verification': {'status': 'PASS'}}
        self.assertEqual(self.m.classify_run_result(report), 'FAIL')
        for stop, identity in (('FAIL', 'PASS'), ('PASS', 'UNKNOWN')):
            fresh = module(ROOT / 'scripts/run_hardware_maintenance.py', 'zero_diag_finish_memory')
            def finish_original(report, io, engine, bindings, proc):
                report.update(stop_verification={'status': stop}, identity_verification={'status': identity})
            fresh.finish_verification = finish_original
            c.install(fresh, h)
            final_engine = self.engine()
            final_engine.zero_diag = {'status': 'PASS', 'errors': [], 'uart_blocked': False}
            final_report = {}
            with contextlib.redirect_stdout(io.StringIO()):
                fresh.finish_verification(final_report, final_engine.io, final_engine, self.bindings, None)
            self.assertTrue(final_engine.manual_cut)
            self.assertTrue(final_report['zero_diag']['uart_blocked'])
        def source(path, name):
            text = path.read_text(); node = next(n for n in ast.parse(text).body if isinstance(n, ast.FunctionDef) and n.name == name)
            return ast.get_source_segment(text, node)
        original = ROOT / '.verify/position_return_2026-10-11_r14/helpers/parameter_stop_head2_r14.py'
        old = source(original, 'raw_read').replace("m.DEVICES['left']", "m.DEVICES['right']").replace("device,'8'", "device,'9'").replace("device,8,register", "device,9,register").replace('width,8]', 'width,9]').replace('[255,255,8,width+2,0]', '[255,255,9,width+2,0]')
        self.assertEqual(source(Path(h.__file__), 'raw_read'), old)


if __name__ == '__main__':
    unittest.main(verbosity=2)
