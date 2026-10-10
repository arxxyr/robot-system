#!/bin/bash
# 只读真实会话的现有 hook 范围；不创建 pane，不执行或修改 hook。
set -eu
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
python_path=$(uv python find 3.12)
"$python_path" - <<'PY'
import datetime
import json
import subprocess

rows = []
for scope, argv in (
    ('server全局session hook', ['tmux', 'show-hooks', '-g']),
    ('server全局window hook', ['tmux', 'show-hooks', '-gw']),
    ('真实session本地hook', ['tmux', 'show-hooks', '-t', 'robot-h2-20261009-proxy-r1']),
    ('该session当前window本地hook，不自动等同新window',
     ['tmux', 'show-hooks', '-w', '-t', 'robot-h2-20261009-proxy-r1']),
    ('真实session当前window身份', ['tmux', 'display-message', '-p', '-t',
                                   'robot-h2-20261009-proxy-r1',
                                   '#{session_name} #{window_id} #{window_index} #{window_name}']),
):
    child = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    stdout, stderr = child.communicate()
    text = stdout.decode()
    rows.append({'scope': scope, 'argv': argv, 'pid': child.pid,
                 'wait_returncode': child.returncode, 'naturally_reaped': True,
                 'sent_signals': [], 'stdout': text, 'stderr': stderr.decode(),
                 'pane_died_lines': [line for line in text.splitlines()
                                     if line.startswith('pane-died[') or line.startswith('pane-died ')]})
print(json.dumps({'schema': 'TMUX_HOOK_INHERITANCE_READONLY_V1',
                  'observed_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  'queries': rows}, ensure_ascii=False, indent=2), flush=True)
assert all(row['wait_returncode'] == 0 for row in rows)
PY
