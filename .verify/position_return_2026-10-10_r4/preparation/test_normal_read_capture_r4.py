"""爸爸，固定普通READ采集的离线门；只有内存Executor，不启动工具或串口。"""
import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch

BASE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('normal_read_candidate', BASE / 'normal_read_capture_r4.py')
c = importlib.util.module_from_spec(spec)
spec.loader.exec_module(c)


class MemoryExecutor:
    def __init__(self, values=(0, 0), failure=None, trace=b'1 +++ exited with 0 +++\n', signal_after_command=None):
        self.values, self.failure, self.trace = values, failure, trace
        self.calls = []
        self.sequence = 0
        self.child = None
        self.evidence_errors = []
        self.interrupted = None
        self.signal_after_command = signal_after_command

    def check(self):
        c.require(self.interrupted is None, '内存信号')

    def command(self, argv):
        self.calls.append(argv)
        self.sequence += 1
        if self.failure:
            raise ValueError(self.failure)
        Path(argv[argv.index('-o') + 1]).write_bytes(self.trace)
        self.interrupted = self.signal_after_command
        motor = int(argv[-3])
        return f'read Present_Velocity@{motor} = {self.values[self.sequence - 1]}\n'


class CaptureCases(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='memory-case-', dir=BASE)
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.output = self.directory / 'out'
        self.now = 1_000_000_000_000
        self.events = []
        self.rows = []

    def gate(self):
        self.events.append('gate')

    def run_reads(self, executor, gate=None):
        self.output.mkdir()
        c.run_reads(None, executor, self.output, gate or self.gate, self.rows)

    def receipt_ref(self, age=0, missing_fact=None):
        facts = dict.fromkeys(c.FIELD_FACTS, True)
        if missing_fact:
            facts.pop(missing_fact)
        value = {'recorded_at_ns': self.now - age, 'scope': c.FIELD_SCOPE,
                 'answer': '仅离线内存夹具，非现场事实', 'physical': facts}
        path = self.directory / 'MOCK-field.json'
        raw = json.dumps(value).encode()
        path.write_bytes(raw)
        return {'path': str(path), 'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}

    def entry(self, reference):
        # 只替换已冻结模块的读取接口；缺失/过期门应在构造Executor之前拒绝。
        def verify(ref, **_):
            raw = Path(ref['path']).read_bytes()
            c.require(len(raw) == ref['bytes'] and hashlib.sha256(raw).hexdigest() == ref['sha256'], '夹具引用错误')
            return raw
        m = SimpleNamespace(verify_ref=verify, strict_json=json.loads,
                            RealExecutor=lambda *_: self.events.append('executor'))
        config = {'field_confirmation': reference}
        with patch.object(c.time, 'time_ns', return_value=self.now):
            return c.execute(config, m, {}, {})

    def test_default_plan_only_does_not_load_source_or_create_output(self):
        with patch.object(c, 'load_source', side_effect=AssertionError('不应导入执行源')):
            with contextlib.redirect_stdout(io.StringIO()) as stream:
                self.assertEqual(c.main(['--config', str(self.directory / '不存在.json')]), 0)
        plan = json.loads(stream.getvalue())
        self.assertEqual(plan['status'], 'PLAN_ONLY')
        self.assertEqual(plan['actions_started'], 0)
        self.assertFalse(plan['output_created'])
        self.assertFalse(self.output.exists())

    def test_missing_confirmation_rejects_before_output_executor_and_uart(self):
        with self.assertRaisesRegex(ValueError, '现场回执缺失'):
            self.entry(None)
        self.assertEqual(self.events, [])
        self.assertFalse(self.output.exists())

    def test_expired_confirmation_rejects_before_output_executor_and_uart(self):
        with self.assertRaisesRegex(ValueError, '回执过期'):
            self.entry(self.receipt_ref(c.FIELD_TTL_NS + 1))
        self.assertEqual(self.events, [])
        self.assertFalse(self.output.exists())

    def test_watch_or_cut_fact_missing_rejects_before_output_executor(self):
        for fact in ('continuous_watch', 'immediate_manual_power_cut'):
            with self.subTest(fact=fact), self.assertRaisesRegex(ValueError, '事实不完整'):
                self.entry(self.receipt_ref(missing_fact=fact))
        self.assertEqual(self.events, [])
        self.assertFalse(self.output.exists())

    def test_fixed_order_two_zero_observations_preserves_trace(self):
        executor = MemoryExecutor()
        self.run_reads(executor)
        self.assertEqual([int(row[-3]) for row in executor.calls], [8, 9])
        self.assertEqual(self.events, ['gate', 'gate'])
        self.assertEqual([row['decoded_integer'] for row in self.rows], [0, 0])
        for argv, row in zip(executor.calls, self.rows):
            self.assertEqual(argv[0], c.STRACE['path'])
            self.assertEqual(argv[-5:-3], [c.TOOL['path'], c.DEVICE])
            self.assertEqual(argv[-2:], ['read', 'Present_Velocity'])
            raw = Path(row['trace']['path']).read_bytes()
            self.assertEqual(hashlib.sha256(raw).hexdigest(), row['trace']['sha256'])

    def test_first_nonzero_never_launches_second_read(self):
        for value in (-50, 50):
            self.output = self.directory / str(value)
            executor = MemoryExecutor(values=(value, 0))
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, 'PV非零'):
                self.run_reads(executor)
            self.assertEqual(len(executor.calls), 1)
            self.assertEqual(self.rows[-1]['decoded_integer'], value)

    def test_first_child_failure_never_launches_second_read(self):
        executor = MemoryExecutor(failure='child退出/预算失败')
        with self.assertRaisesRegex(ValueError, 'child退出'):
            self.run_reads(executor)
        self.assertEqual(len(executor.calls), 1)

    def test_trace_failure_or_signal_never_launches_second_read(self):
        executor = MemoryExecutor(trace='不完整原件'.encode())
        with self.assertRaisesRegex(ValueError, '未记录成功终态'):
            self.run_reads(executor)
        self.assertEqual(len(executor.calls), 1)
        self.output = self.directory / 'signal-out'
        executor = MemoryExecutor(signal_after_command=15)
        with self.assertRaisesRegex(ValueError, '内存信号'):
            self.run_reads(executor)
        self.assertEqual(len(executor.calls), 1)

    def test_identity_or_expiry_gate_before_command_has_zero_start(self):
        def expired_gate():
            raise ValueError('非UART准备后回执过期/身份变化')
        executor = MemoryExecutor()
        with self.assertRaisesRegex(ValueError, '身份变化'):
            self.run_reads(executor, gate=expired_gate)
        self.assertEqual(executor.calls, [])


if __name__ == '__main__':
    unittest.main(verbosity=2)
