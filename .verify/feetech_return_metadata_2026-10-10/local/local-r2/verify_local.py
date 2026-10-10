"""爸爸，仅本机软件门；直属child逐个自然等待，失败即停止后续门。"""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time

sys.dont_write_bytecode = True
root = Path('/Users/loosqk/repo/rust/robot-system')
output = Path(__file__).resolve().parent / 'verification'
output.mkdir(exist_ok=False)
paths = [root / 'crates/robot-adapters/feetech' / name for name in (
    'src/registers.rs', 'src/bus.rs', 'src/diagnostic.rs', 'src/bus_raw_read_tests.rs',
    'examples/feetech_poke.rs')]

def sources():
    values = []
    for path in paths:
        raw = path.read_bytes()
        values.append({'path': str(path), 'bytes': len(raw), 'sha256': hashlib.sha256(raw).hexdigest()})
    return values

before = sources()
rows = []
for label, argv in (
    ('compiler_before', ['rustc', '-Vv']),
    ('fmt', ['cargo', 'fmt', '--all']),
    ('crate_clippy', ['cargo', 'clippy', '--locked', '-p', 'robot-adapter-feetech', '--all-targets',
                      '--features', 'real-hw', '--', '-D', 'warnings']),
    ('lib_tests', ['cargo', 'test', '--locked', '-p', 'robot-adapter-feetech', '--features', 'real-hw', '--lib']),
    ('poke_tests', ['cargo', 'test', '--locked', '-p', 'robot-adapter-feetech', '--features', 'real-hw',
                    '--example', 'feetech_poke']),
    ('compiler_after', ['rustc', '-Vv']),
):
    started = time.time_ns()
    with (output / f'{label}.stdout').open('xb') as stdout, (output / f'{label}.stderr').open('xb') as stderr:
        child = subprocess.Popen(argv, cwd=root, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr)
        code = child.wait()
    original_outputs = []
    for suffix in ('stdout', 'stderr'):
        path = output / f'{label}.{suffix}'
        raw = path.read_bytes()
        original_outputs.append({'path': str(path), 'bytes': len(raw), 'sha256': hashlib.sha256(raw).hexdigest()})
    rows.append({'label': label, 'argv': argv, 'pid': child.pid, 'exit_code': code,
                 'reaped': child.poll() == code, 'signals_sent': [], 'started_at_ns': started,
                 'finished_at_ns': time.time_ns(), 'outputs': original_outputs})
    print(json.dumps({'label': label, 'pid': child.pid, 'exit_code': code, 'reaped': child.poll() == code}), flush=True)
    if code:
        break
result = {'status': 'PASS' if len(rows) == 6 and all(r['exit_code'] == 0 and r['reaped'] for r in rows) else 'FAIL',
          'owner_pid': os.getpid(), 'commands': rows, 'sources_before_fmt': before, 'sources_after': sources(),
          'hardware_actions': 0, 'SSH': 0, 'UART': 0, 'register_writes': 0,
          'limits': '本机定向软件门；非Orin根Clippy、全仓测试、独立release或硬件验收'}
with (output / 'result.json').open('x') as stream:
    json.dump(result, stream, ensure_ascii=False, indent=2)
    stream.write('\n')
print(json.dumps({'status': result['status'], 'result': str(output / 'result.json')}), flush=True)
sys.exit(0 if result['status'] == 'PASS' else 1)
