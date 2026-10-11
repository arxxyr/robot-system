#!/usr/bin/env python3
"""仅本机定向软件验证；不访问串口或执行远端命令。"""
import datetime
import hashlib
import json
import pathlib
import subprocess
import sys
import time

ROOT = pathlib.Path('/Users/loosqk/repo/rust/robot-system')
OUT = pathlib.Path(__file__).parent
SOURCES = [
    'crates/robot-adapters/feetech/src/registers.rs',
    'crates/robot-adapters/feetech/src/bus.rs',
    'crates/robot-adapters/feetech/src/bus_raw_read_tests.rs',
    'crates/robot-adapters/feetech/src/diagnostic.rs',
    'crates/robot-adapters/feetech/examples/feetech_poke.rs',
    'Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', '.cargo/config.toml',
    'crates/robot-adapters/feetech/Cargo.toml',
]

def stamps():
    return {'utc': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'mono_ns': time.monotonic_ns()}

def refs():
    result = []
    for name in SOURCES:
        p = ROOT / name
        b = p.read_bytes()
        result.append({'path': name, 'bytes': len(b), 'sha256': hashlib.sha256(b).hexdigest()})
    return result

def save(name, data):
    with (OUT / name).open('x') as stream:
        json.dump(data, stream, indent=2, ensure_ascii=False)
        stream.write('\n')

commands = [
    ('compiler-before', ['rustc', '+nightly', '-Vv']),
    ('fmt', ['cargo', 'fmt', '--all']),
    ('crate-clippy', ['cargo', 'clippy', '-p', 'robot-adapter-feetech', '--all-targets', '--features', 'real-hw', '--', '-D', 'warnings']),
    ('lib-tests', ['cargo', 'test', '-p', 'robot-adapter-feetech', '--features', 'real-hw', '--lib']),
    ('poke-tests', ['cargo', 'test', '-p', 'robot-adapter-feetech', '--features', 'real-hw', '--example', 'feetech_poke']),
    ('compiler-after', ['rustc', '+nightly', '-Vv']),
]
rows = []
save('source-before.json', refs())
for name, argv in commands:
    before = stamps()
    with (OUT / f'{name}.stdout').open('xb') as stdout, (OUT / f'{name}.stderr').open('xb') as stderr:
        child = subprocess.Popen(argv, cwd=ROOT, stdout=stdout, stderr=stderr)
        rc = child.wait()
    row = {'name': name, 'argv': argv, 'pid': child.pid, 'started': before, 'finished': stamps(),
           'exit_code': rc, 'wait_returncode': rc, 'reaped': True, 'signals_sent': []}
    for which in ['stdout', 'stderr']:
        p = OUT / f'{name}.{which}'
        b = p.read_bytes()
        row[which] = {'path': str(p), 'bytes': len(b), 'sha256': hashlib.sha256(b).hexdigest()}
    rows.append(row)
    save(f'{name}.receipt.json', row)
    print(json.dumps({'name': name, 'pid': child.pid, 'exit_code': rc, 'reaped': True}), flush=True)
    if rc:
        break
save('source-after.json', refs())
save('result.json', {'scope': '本机fmt及Feetech定向软件门；无UART/远端/完整workspace验证',
                     'result': 'PASS' if len(rows) == len(commands) and all(r['exit_code'] == 0 for r in rows) else 'FAIL',
                     'commands': rows})
sys.exit(0 if len(rows) == len(commands) and all(r['exit_code'] == 0 for r in rows) else 1)
