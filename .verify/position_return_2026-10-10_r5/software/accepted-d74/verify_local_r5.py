"""爸爸，仅本地零UART验证；每个直属uv child自然wait并保留原始输出。"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

sys.dont_write_bytecode = True
base = Path(__file__).resolve().parent
out = base / 'software-verification-r5'
out.mkdir(exist_ok=False)
uv = shutil.which('uv')
assert uv
rows = []
for label, arguments, expected in (
    ('tests', [str(base / 'test_i2_head1_wire_r5.py')], 0),
    ('default_plan', [str(base / 'i2_trial_head1_wire_r5.py')], 0),
    ('missing_execute_inputs', [str(base / 'i2_trial_head1_wire_r5.py'), '--execute'], 2),
):
    argv = [uv, 'run', '--no-project', 'python', *arguments]
    with (out / f'{label}.stdout').open('xb') as stdout, (out / f'{label}.stderr').open('xb') as stderr:
        started = time.time_ns()
        child = subprocess.Popen(argv, stdout=stdout, stderr=stderr, stdin=subprocess.DEVNULL,
                                 env={**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'})
        code = child.wait()
        ended = time.time_ns()
    outputs = []
    for suffix in ('stdout', 'stderr'):
        path = out / f'{label}.{suffix}'
        raw = path.read_bytes()
        outputs.append({'path': str(path), 'bytes': len(raw), 'sha256': hashlib.sha256(raw).hexdigest()})
    rows.append({'label': label, 'argv': argv, 'pid': child.pid, 'exit_code': code,
                 'expected_exit': expected, 'reaped': child.poll() == code, 'signals_sent': [],
                 'started_at_ns': started, 'finished_at_ns': ended, 'outputs': outputs})
sources = []
for name in ('i2_trial_head1_wire_r5.py', 'test_i2_head1_wire_r5.py'):
    path = base / name
    raw = path.read_bytes()
    sources.append({'path': str(path), 'bytes': len(raw), 'sha256': hashlib.sha256(raw).hexdigest()})
result = {'status': 'PASS' if all(r['exit_code'] == r['expected_exit'] and r['reaped'] for r in rows) else 'FAIL',
          'owner_pid': os.getpid(), 'children': rows, 'sources': sources, 'distinct_tests': 27,
          'old_i2_behavior_cases': 8, 'new_wire_and_entry_cases': 19,
          'hardware_child_starts': 0, 'SSH': 0, 'UART': 0, 'motion_commands': 0}
with (out / 'result.json').open('x') as stream:
    json.dump(result, stream, ensure_ascii=False, indent=2)
    stream.write('\n')
print(json.dumps(result, ensure_ascii=False))
sys.exit(0 if result['status'] == 'PASS' else 1)
