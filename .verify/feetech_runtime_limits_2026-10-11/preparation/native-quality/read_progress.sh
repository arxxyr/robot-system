#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PY'
import json
from pathlib import Path
base = Path('/home/loosqk/robot-checks/position-return-20261011-r12/native-quality/quality')
rows = []
for path in sorted(base.iterdir()):
    if not path.is_dir() or not (path / 'launch.json').is_file():
        continue
    launch = json.loads((path / 'launch.json').read_text())
    complete = path / 'completion.json'
    row = {'name': path.name, 'pid': launch['pid'], 'completed': complete.is_file()}
    if complete.is_file():
        value = json.loads(complete.read_text())
        row.update(exit_code=value.get('exit_code'), reaped=value.get('reaped'), signals=value.get('signals'), error=value.get('runner_error'))
    else:
        log = path / 'stderr.log'
        if log.is_file():
            with log.open('rb') as stream:
                stream.seek(max(0, log.stat().st_size - 1400))
                row['stderr_tail'] = stream.read().decode(errors='replace')
    rows.append(row)
result = base / 'result.json'
print(json.dumps({'scope': '只读软件进度，无重启或UART', 'terminal_result_present': result.is_file(), 'commands': rows}, ensure_ascii=False))
PY
