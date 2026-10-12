"""爸爸，一例自有纯软件tmux调用；自然观察，不signal/retry，不改全局选项。"""
import base64
import hashlib
import io
import json
from pathlib import Path
import shlex
import subprocess
import tarfile
import time

DRIVER_SOURCE_BASE64 = None  # 爸爸，仅本机填入纯软件driver原件，不含现场/GO。
assert isinstance(DRIVER_SOURCE_BASE64, str)
PHASE = Path('/home/loosqk/robot-checks/position-return-20261010-r1/launcher-pane-option-fixture-r1')
PYTHON = '/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3'
PHASE.mkdir(mode=0o700, exist_ok=False)
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
case = PHASE / 'candidate-p'; case.mkdir(mode=0o700)
session_name = 'base-zero-pane-option-candidate-p-20261012-r1'
shell = 'exec ' + shlex.join([PYTHON, str(driver), str(case)])
start = command('start', ['tmux', 'new-session', '-d', '-s', session_name, '-P', '-F',
                 '#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}', shell])
assert start['exit_code'] == 0 and not start['stderr']
session, window, pane, pid = start['stdout'].strip().split('|'); pid = int(pid)
assert session == session_name
terminal = None
deadline = time.monotonic() + 10
while time.monotonic() < deadline:
    state = command('observe', ['tmux', 'display-message', '-p', '-t', pane,
                    '#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}|#{pane_dead_status}'])
    assert state['exit_code'] == 0 and not state['stderr']
    fields = state['stdout'].strip().split('|')
    assert fields[:4] == [session, window, pane, str(pid)]
    if fields[4] == '1':
        terminal = state['stdout'].strip()
        break
    time.sleep(0.05)
# 超时保留仍活着的自有pane，不发信号或重试。
assert terminal is not None, '10秒未观察到自然dead，保留自有pane，不signal/retry'
data = json.loads((case / 'driver-receipt.json').read_text())
calls = [c for c in data['children'] if c['argv'][1] == 'set-option']
assert data['driver_pid'] == pid and data['pane'] == pane and len(calls) == 1
assert calls[0]['argv'] == ['tmux', 'set-option', '-p', '-t', pane, 'remain-on-exit', 'on']
assert calls[0]['exit_code'] == 0 and all(c['reaped'] and c['exit_code'] == 0 for c in data['children'])
assert data['remain_on_exit_after'].strip() == 'on' and data['after_option_reached']
assert terminal.split('|')[-2:] == ['1', '0'] and (case / 'software-marker.txt').read_text() == 'SOFTWARE_ONLY_REACHED\n'
reclaim = command('reclaim-owned-dead', ['tmux', 'kill-pane', '-t', pane])
assert reclaim['exit_code'] == 0
result = {'status': 'PASS_SOFTWARE_ONLY_PANE_OPTION_FIXTURE', 'session': session, 'window': window,
          'pane': pane, 'driver_pid': pid, 'terminal': terminal, 'driver_receipt': data,
          'software_marker_exists': True, 'owned_dead_pane_reclaimed': True,
          'tmux_version': version, 'direct_children': receipts, 'UART': 0,
          'hardware_candidate_started': False, 'new_GO': False,
          'limitations': ['仅本次-p纯软件调用及retention；旧启动/观察FAIL保留',
                          '未重跑-w负控，不解释旧R17同式成功的具体原因']}
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
