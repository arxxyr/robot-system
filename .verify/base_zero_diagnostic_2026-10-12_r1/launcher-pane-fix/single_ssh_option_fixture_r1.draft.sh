#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
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

DRIVER_SOURCE_BASE64 = 'IiIi54i454i477yM57qv6L2v5Lu2Zml4dHVyZeeahOiHquaciXBhbmXvvJvku4Xpqozor4EtcOWQjOW9ouiwg+eUqO+8jOS4jeWvvOWFpeacuuWZqOS6uuWAmemAieOAgiIiIgppbXBvcnQgZXJybm8KaW1wb3J0IGpzb24KaW1wb3J0IG9zCmZyb20gcGF0aGxpYiBpbXBvcnQgUGF0aAppbXBvcnQgc3VicHJvY2VzcwppbXBvcnQgc3lzCmltcG9ydCB0aW1lCgpjYXNlID0gUGF0aChzeXMuYXJndlsxXSkKZmxhZyA9ICctcCcKYXNzZXJ0IGxlbihzeXMuYXJndikgPT0gMgphc3NlcnQgY2FzZS5pc19hYnNvbHV0ZSgpIGFuZCBjYXNlLmlzX2RpcigpCmZkID0gb3Mub3BlbihjYXNlIC8gJ2RyaXZlci5sb2cnLCBvcy5PX1dST05MWSB8IG9zLk9fQ1JFQVQgfCBvcy5PX0VYQ0wsIDBvNjAwKQpvcy5kdXAyKGZkLCAxKTsgb3MuZHVwMihmZCwgMik7IG9zLmNsb3NlKGZkKQpyZWFsX3BvcGVuID0gc3VicHJvY2Vzcy5Qb3BlbgpjaGlsZHJlbiA9IFtdCmRlZiByZWNvcmRpbmdfcG9wZW4oKmFyZ3MsICoqa3dhcmdzKToKICAgIGNoaWxkID0gcmVhbF9wb3BlbigqYXJncywgKiprd2FyZ3MpCiAgICBjaGlsZHJlbi5hcHBlbmQoKGNoaWxkLCB0aW1lLnRpbWVfbnMoKSkpCiAgICByZXR1cm4gY2hpbGQKc3VicHJvY2Vzcy5Qb3BlbiA9IHJlY29yZGluZ19wb3Blbgpyb3cgPSB7J3Njb3BlJzogJ1NPRlRXQVJFX09OTFlfVE1VWF9PUFRJT05fRklYVFVSRScsICdkcml2ZXJfcGlkJzogb3MuZ2V0cGlkKCksCiAgICAgICAncGFuZSc6IG9zLmVudmlyb25bJ1RNVVhfUEFORSddLCAnZmxhZyc6IGZsYWcsICdzdGFydGVkX25zJzogdGltZS50aW1lX25zKCksCiAgICAgICAnYWZ0ZXJfb3B0aW9uX3JlYWNoZWQnOiBGYWxzZSwgJ2NoaWxkcmVuJzogW10sICdVQVJUJzogMCwgJ0dPJzogRmFsc2V9CmNvZGUgPSAxCnRyeToKICAgIGJlZm9yZSA9IHN1YnByb2Nlc3MucnVuKFsndG11eCcsICdzaG93LW9wdGlvbnMnLCAnLXAnLCAnLXYnLCAnLXQnLCBvcy5lbnZpcm9uWydUTVVYX1BBTkUnXSwKICAgICAgICAgICAgICAgICAgICAgICAgICAgICAncmVtYWluLW9uLWV4aXQnXSwgY2hlY2s9VHJ1ZSwgY2FwdHVyZV9vdXRwdXQ9VHJ1ZSwgdGV4dD1UcnVlKQogICAgcm93WydyZW1haW5fb25fZXhpdF9iZWZvcmUnXSA9IGJlZm9yZS5zdGRvdXQKICAgICMg5YWz6ZSu6LCD55So5LiO5aSx6LSlbGF1bmNoZXLlkIzlvaLmgIHvvIzllK/kuIDlt67liKvkuLotd+aUuS1w77yb5Y+q5oyH5ZCR6Ieq6LqrcGFuZeOAggogICAgc3VicHJvY2Vzcy5ydW4oWyd0bXV4JywgJ3NldC1vcHRpb24nLCAnLXAnLCAnLXQnLCBvcy5lbnZpcm9uWydUTVVYX1BBTkUnXSwKICAgICAgICAgICAgICAgICAgICAncmVtYWluLW9uLWV4aXQnLCAnb24nXSwgY2hlY2s9VHJ1ZSkKICAgIHJvd1snYWZ0ZXJfb3B0aW9uX3JlYWNoZWQnXSA9IFRydWUKICAgIGFmdGVyID0gc3VicHJvY2Vzcy5ydW4oWyd0bXV4JywgJ3Nob3ctb3B0aW9ucycsICctcCcsICctdicsICctdCcsIG9zLmVudmlyb25bJ1RNVVhfUEFORSddLAogICAgICAgICAgICAgICAgICAgICAgICAgICAgJ3JlbWFpbi1vbi1leGl0J10sIGNoZWNrPVRydWUsIGNhcHR1cmVfb3V0cHV0PVRydWUsIHRleHQ9VHJ1ZSkKICAgIHJvd1sncmVtYWluX29uX2V4aXRfYWZ0ZXInXSA9IGFmdGVyLnN0ZG91dAogICAgY29kZSA9IDAKZXhjZXB0IEJhc2VFeGNlcHRpb24gYXMgZXJyb3I6CiAgICByb3dbJ2Vycm9yJ10gPSByZXByKGVycm9yKQpmaW5hbGx5OgogICAgc3VicHJvY2Vzcy5Qb3BlbiA9IHJlYWxfcG9wZW4KICAgIGZvciBjaGlsZCwgc3RhcnRlZCBpbiBjaGlsZHJlbjoKICAgICAgICByZXR1cm5lZCA9IGNoaWxkLndhaXQoKQogICAgICAgIHJlYXBlZCA9IEZhbHNlCiAgICAgICAgdHJ5OgogICAgICAgICAgICBvcy53YWl0cGlkKGNoaWxkLnBpZCwgb3MuV05PSEFORykKICAgICAgICBleGNlcHQgQ2hpbGRQcm9jZXNzRXJyb3IgYXMgZXJyb3I6CiAgICAgICAgICAgIHJlYXBlZCA9IGVycm9yLmVycm5vID09IGVycm5vLkVDSElMRAogICAgICAgIHJvd1snY2hpbGRyZW4nXS5hcHBlbmQoeydhcmd2JzogY2hpbGQuYXJncywgJ3BpZCc6IGNoaWxkLnBpZCwgJ3N0YXJ0ZWRfbnMnOiBzdGFydGVkLAogICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICdleGl0X2NvZGUnOiByZXR1cm5lZCwgJ3JlYWxfd2FpdF9yZXR1cm5jb2RlJzogcmV0dXJuZWQsCiAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgJ3JlYXBlZCc6IHJlYXBlZCwgJ3NpZ25hbHNfc2VudCc6IFtdfSkKICAgIHJvdy51cGRhdGUoZmluaXNoZWRfbnM9dGltZS50aW1lX25zKCksIG9wdGlvbl93cmFwcGVyX2V4aXQ9Y29kZSkKICAgIHdpdGggKGNhc2UgLyAnZHJpdmVyLXJlY2VpcHQuanNvbicpLm9wZW4oJ3gnKSBhcyBzdHJlYW06CiAgICAgICAganNvbi5kdW1wKHJvdywgc3RyZWFtLCBlbnN1cmVfYXNjaWk9RmFsc2UsIGluZGVudD0yKTsgc3RyZWFtLndyaXRlKCdcbicpCmlmIGNvZGU6CiAgICBzeXMuZXhpdChjb2RlKQojIOaIkOWKn+aXtuS7hWV4ZWPnuq/ova/ku7ZtYXJrZXLvvIzkv53mjIHlkIxkcml2ZXIgUElE77yb57ud5LiN5omn6KGM56Gs5Lu25YCZ6YCJ44CCCm1hcmtlcl92YWx1ZSA9ICdTT0ZUV0FSRV9PTkxZX1JFQUNIRURcbicKbWFya2VyX2NvZGUgPSAiZnJvbSBwYXRobGliIGltcG9ydCBQYXRoOyBQYXRoKCIgKyByZXByKHN0cihjYXNlIC8gJ3NvZnR3YXJlLW1hcmtlci50eHQnKSkgKyAiKS53cml0ZV90ZXh0KCIgKyByZXByKG1hcmtlcl92YWx1ZSkgKyAiKSIKb3MuZXhlY3Yoc3lzLmV4ZWN1dGFibGUsIFtzeXMuZXhlY3V0YWJsZSwgJy1jJywgbWFya2VyX2NvZGVdKQo='  # 爸爸，仅本机填入纯软件driver原件，不含现场/GO。
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

PYREMOTE
