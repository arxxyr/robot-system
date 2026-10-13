#!/bin/bash
# 爸爸，仅读取同一软件phase及所属pane；不启动batch、不kill、不访问硬件。
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import hashlib
import json
from pathlib import Path
import subprocess
import time

phase = Path('/home/loosqk/robot-checks/base-feedback-r1-software')
assert phase == phase.resolve() and phase.is_dir()
start = json.loads((phase / 'boot/start.json').read_text())
session, window, pane, pid = start['terminal_identity']
assert session == 'base-feedback-r1-software-20261012-r1' and type(pid) is int
argv = ['tmux', 'display-message', '-p', '-t', pane, '#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}|#{pane_dead_status}']
started = time.time_ns()
child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
stdout, stderr = child.communicate()
code = child.wait()
receipt = {'argv': argv, 'pid': child.pid, 'started_ns': started, 'finished_ns': time.time_ns(),
           'exit_code': code, 'wait_returncode': code, 'reaped': True, 'signals_sent': [],
           'stdout': stdout.decode(), 'stderr': stderr.decode()}
assert code == 0 and stderr == b'', receipt
fields = stdout.decode().strip().split('|')
assert fields[:4] == [session, window, pane, str(pid)]


def ref(path):
    raw = path.read_bytes()
    return {'path': str(path), 'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}


rows = []
quality = phase / 'quality'
if quality.is_dir():
    for path in sorted(quality.glob('*/launch.json')):
        row = json.loads(path.read_text())
        complete = path.parent / 'completion.json'
        item = {'name': row['name'], 'pid': row['pid'], 'argv': row['argv'], 'launch': ref(path)}
        if complete.is_file():
            end = json.loads(complete.read_text())
            item.update(completion=ref(complete), exit_code=end['exit_code'], reaped=end['reaped'], signals=end['signals'])
        else:
            proc = Path('/proc') / str(row['pid'])
            try:
                text = (proc / 'stat').read_text()
                rest = text[text.rfind(')') + 2:].split()
                actual = {'pid': row['pid'], 'ppid': int(rest[1]), 'pgid': int(rest[2]), 'sid': int(rest[3]), 'start_ticks': int(rest[19]), 'state': rest[0], 'boot_id': Path('/proc/sys/kernel/random/boot_id').read_text().strip()}
                expected = row['identity']
                item.update(active_proc=actual, exact_saved_identity_equal=all(actual.get(key) == expected[key] for key in ('pid', 'pgid', 'sid', 'start_ticks', 'boot_id')))
            except FileNotFoundError:
                item['active_proc'] = 'MISSING_AT_THIS_OBSERVATION'
        rows.append(item)
result_path = quality / 'result.json'
result = json.loads(result_path.read_text()) if result_path.is_file() else None
print(json.dumps({'status': 'READ_ONLY_EXISTING_NATIVE_SOFTWARE_HANDLE_OBSERVED', 'terminal': stdout.decode().strip(),
                  'same_owned_identity': True, 'pane_dead': fields[4], 'commands': rows, 'result': result,
                  'direct_children': [receipt], 'new_batch_starts': 0, 'UART_operations': 0, 'services_touched': 0}, ensure_ascii=False), flush=True)
PYREMOTE
