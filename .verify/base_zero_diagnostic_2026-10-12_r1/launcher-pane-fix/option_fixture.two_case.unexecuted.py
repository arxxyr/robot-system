"""爸爸，两个自有纯软件tmux案例；自然观察，绝不signal/retry或改全局选项。"""
import base64
import hashlib
import io
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tarfile
import time

PHASE = Path('/home/loosqk/robot-checks/position-return-20261010-r1/launcher-pane-option-fixture-r1')
PYTHON = '/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3'
PHASE.mkdir(mode=0o700, exist_ok=False)
DRIVER_SOURCE_BASE64 = None  # 爸爸，仅由本机生成器填纯软件driver原件，不含现场/GO。
assert isinstance(DRIVER_SOURCE_BASE64, str)
driver = PHASE / 'option_driver.py'
with driver.open('xb') as stream:
    stream.write(base64.b64decode(DRIVER_SOURCE_BASE64, validate=True))
receipts = []
def command(name, argv):
    started = time.time_ns()
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    stdout, stderr = child.communicate(); code = child.wait()
    row = {'name': name, 'argv': argv, 'pid': child.pid, 'started_ns': started, 'finished_ns': time.time_ns(),
           'exit_code': code, 'reaped': True, 'signals_sent': [], 'stdout': stdout.decode(), 'stderr': stderr.decode()}
    receipts.append(row)
    return row

version = command('version', ['tmux', '-V'])
assert version['exit_code'] == 0
cases = []
for name, flag in (('old-w', '-w'), ('candidate-p', '-p')):
    case = PHASE / name; case.mkdir(mode=0o700)
    session_name = 'base-zero-pane-option-' + name + '-20261012-r1'
    shell = 'exec ' + shlex.join([PYTHON, str(driver), str(case), flag])
    start = command(name + '-start', ['tmux', 'new-session', '-d', '-s', session_name, '-P', '-F',
                     '#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}', shell])
    assert start['exit_code'] == 0 and not start['stderr']
    session, window, pane, pid = start['stdout'].strip().split('|'); pid = int(pid)
    assert session == session_name
    terminal = None
    gone = False
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        state = command(name + '-observe', ['tmux', 'display-message', '-p', '-t', pane,
                        '#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}|#{pane_dead_status}'])
        if state['exit_code'] != 0:
            gone = True
            break
        fields = state['stdout'].strip().split('|')
        assert fields[:4] == [session, window, pane, str(pid)]
        if fields[4] == '1':
            terminal = state['stdout'].strip()
            break
        time.sleep(0.05)
    # 超时保留仍活着的自有pane，绝不发信号或重试。
    assert terminal is not None or gone, '纯软件进程尚未自然终态，保留原pane'
    receipt_path = case / 'driver-receipt.json'
    deadline = time.monotonic() + 1
    while not receipt_path.exists() and time.monotonic() < deadline:
        time.sleep(0.05)
    data = json.loads(receipt_path.read_text()) if receipt_path.exists() else None
    row = {'case': name, 'flag': flag, 'session': session, 'window': window, 'pane': pane,
           'driver_pid': pid, 'terminal': terminal, 'pane_disappeared': gone,
           'driver_exit_code': int(terminal.split('|')[-1]) if terminal else 'UNKNOWN_PANE_REMOVED',
           'driver_receipt': data, 'software_marker_exists': (case / 'software-marker.txt').exists(),
           'owned_dead_pane_reclaimed': False}
    if data is not None:
        assert data['driver_pid'] == pid and data['pane'] == pane and all(c['reaped'] for c in data['children'])
    if terminal is not None:
        reclaim = command(name + '-reclaim-owned-dead', ['tmux', 'kill-pane', '-t', pane])
        assert reclaim['exit_code'] == 0
        row['owned_dead_pane_reclaimed'] = True
    cases.append(row)
old, fixed = cases
old_calls = [] if old['driver_receipt'] is None else [c for c in old['driver_receipt']['children'] if c['argv'][1] == 'set-option']
fixed_calls = [] if fixed['driver_receipt'] is None else [c for c in fixed['driver_receipt']['children'] if c['argv'][1] == 'set-option']
passed = (len(old_calls) == len(fixed_calls) == 1 and old_calls[0]['exit_code'] != 0
          and not old['software_marker_exists'] and fixed_calls[0]['exit_code'] == 0
          and fixed['driver_exit_code'] == 0 and fixed['software_marker_exists']
          and fixed['owned_dead_pane_reclaimed'])
result = {'status': 'PASS_SOFTWARE_ONLY_PANE_OPTION_FIXTURE' if passed else 'FAIL',
          'cases': cases, 'tmux_version': version, 'direct_children': receipts,
          'UART': 0, 'hardware_candidate_started': False, 'new_GO': False,
          'limitations': ['old自动移除pane时driver退出码UNKNOWN，不由消失推断或回填旧FAIL',
                          '只证明本次纯软件调用/retention，旧R17同式曾成功的具体原因仍UNCONFIRMED']}
with (PHASE / 'fixture-result.json').open('x') as stream:
    json.dump(result, stream, ensure_ascii=False, indent=2); stream.write('\n')
buffer = io.BytesIO()
with tarfile.open(fileobj=buffer, mode='w:gz') as tar:
    for path in sorted(PHASE.rglob('*')):
        assert not path.is_symlink()
        if path.is_file():
            tar.add(path, arcname=str(path.relative_to(PHASE)), recursive=False)
raw = buffer.getvalue()
print(json.dumps({'status': result['status'], 'result': result, 'archive_sha256': hashlib.sha256(raw).hexdigest(),
                  'archive_bytes': len(raw), 'archive_base64': base64.b64encode(raw).decode()}, ensure_ascii=False), flush=True)
sys.exit(0 if passed else 1)
