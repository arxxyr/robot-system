#!/bin/bash
# 爸爸，仅新独占目录内的软件检查；未冻结时零child拒绝，不接触现场/USB/UART。
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64
import hashlib
import io
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tarfile
import time

payload = None
assert isinstance(payload, dict) and payload, '爸爸，root尚未冻结并绑定上传原件，不创建远端目录或child。'
phase = Path('/home/loosqk/robot-checks/base-feedback-r1-software')
phase.parent.mkdir(mode=0o700, exist_ok=True)
phase.mkdir(mode=0o700, exist_ok=False)
for name, row in payload.items():
    relative = Path(name)
    assert not relative.is_absolute() and '..' not in relative.parts
    raw = base64.b64decode(row['base64'], validate=True)
    assert len(raw) == row['bytes'] and hashlib.sha256(raw).hexdigest() == row['sha256']
    path = phase / relative
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    with path.open('xb') as stream:
        stream.write(raw)
        stream.flush()
        os.fsync(stream.fileno())
boot = phase / 'boot'


def save(path, value):
    with path.open('x', encoding='utf-8') as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2)
        stream.write('\n')


def input_pins():
    actual = {}
    for name, expected in payload.items():
        path = phase / name
        assert path == path.resolve() and stat.S_ISREG(path.lstat().st_mode) and path.stat().st_nlink == 1
        raw = path.read_bytes()
        actual[name] = {'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}
        assert actual[name] == {key: expected[key] for key in ('sha256', 'bytes')}
    return actual


before = input_pins()
save(boot / 'uploaded-inputs-before.json', before)
launcher = boot / 'launch_native_software_base_feedback_r1.sh'
with launcher.open('x', encoding='utf-8') as stream:
    stream.write('#!/bin/bash\nset -eu\nexport PYTHONDONTWRITEBYTECODE=1\ntmux set-option -p remain-on-exit on\nexec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 ' + str(boot / 'native_software_quality_base_feedback_r1.py') + ' > ' + str(boot / 'base-feedback-r1-software-driver.log') + ' 2>&1\n')
receipts = []


def run(name, argv):
    start = time.time_ns()
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    stdout, stderr = child.communicate()
    code = child.wait()
    row = {'name': name, 'argv': argv, 'pid': child.pid, 'started_ns': start, 'finished_ns': time.time_ns(),
           'exit_code': code, 'wait_returncode': code, 'reaped': True, 'signals_sent': [],
           'stdout': stdout.decode(), 'stderr': stderr.decode()}
    receipts.append(row)
    return row


start = run('start-software-only', ['tmux', 'new-session', '-d', '-s', 'base-feedback-r1-software-20261012-r1',
            '-P', '-F', '#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}', 'bash ' + str(launcher)])
assert start['exit_code'] == 0 and not start['stderr']
session, window, pane, pid = start['stdout'].strip().split('|')
pid = int(pid)
save(boot / 'start.json', {'terminal_identity': [session, window, pane, pid], 'start': start,
                         'scope': '仅纯软件；不接触UART/USB/服务/GO', 'observations': str(boot / 'observations.jsonl')})
deadline = time.monotonic() + 14400
terminal = None
with (boot / 'observations.jsonl').open('x', encoding='utf-8') as observations:
    while time.monotonic() < deadline:
        row = run('observe-owned-pane', ['tmux', 'display-message', '-p', '-t', pane,
                  '#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}|#{pane_dead_status}'])
        observations.write(json.dumps(row, ensure_ascii=False) + '\n')
        observations.flush()
        os.fsync(observations.fileno())
        assert row['exit_code'] == 0 and not row['stderr']
        fields = row['stdout'].strip().split('|')
        assert fields[:4] == [session, window, pane, str(pid)]
        if fields[4] == '1':
            terminal = row['stdout'].strip()
            break
        time.sleep(10)
assert terminal is not None, '软件进程未自然结束，保留pane，不发信号或重试'
exit_code = int(terminal.split('|')[-1])
save(boot / 'terminal.json', {'terminal': terminal, 'driver_pid': pid, 'exit_code': exit_code, 'same_ssh_children': receipts})
reclaim = run('reclaim-only-natural-dead-owned-pane', ['tmux', 'kill-pane', '-t', pane])
assert reclaim['exit_code'] == 0
save(boot / 'reclaim.json', reclaim)
after = input_pins()
save(boot / 'uploaded-inputs-after.json', after)
assert before == after
buffer = io.BytesIO()
with tarfile.open(fileobj=buffer, mode='w:gz') as archive:
    for path in sorted(phase.rglob('*')):
        assert not path.is_symlink()
        if path.is_file():
            archive.add(path, arcname=str(path.relative_to(phase)), recursive=False)
raw = buffer.getvalue()
with tarfile.open(fileobj=io.BytesIO(raw)) as archive:
    count = sum(member.isfile() for member in archive)
result_path = phase / 'quality/result.json'
result = json.loads(result_path.read_text()) if result_path.exists() else None
print(json.dumps({'status': 'BASE_FEEDBACK_R1_NATIVE_SOFTWARE_COLLECTED', 'driver_pid': pid, 'pane': pane,
                  'exit_code': exit_code, 'archive_sha256': hashlib.sha256(raw).hexdigest(), 'archive_bytes': len(raw),
                  'archive_files': count, 'result': result, 'archive_base64': base64.b64encode(raw).decode()}, ensure_ascii=False), flush=True)
sys.exit(exit_code)
PYREMOTE
