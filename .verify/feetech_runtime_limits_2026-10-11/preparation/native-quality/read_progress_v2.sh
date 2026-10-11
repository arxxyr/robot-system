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
import hashlib, stat
extra = {}
for name in ('clippy', 'clippy_real_hw'):
    path = base / name / 'completion.json'
    if path.is_file():
        row = json.loads(path.read_text())
        extra[name] = {k: row.get(k) for k in ('argv', 'pid', 'started_at', 'finished_at', 'started_monotonic_ns', 'ended_monotonic_ns', 'exit_code', 'reaped', 'signals', 'runner_error')}
source = base / 'source_before.json'
if source.is_file():
    row = json.loads(source.read_text())
    extra['source'] = {'file_count': len(row['files']), 'source_manifest_sha256': row['source_manifest_sha256'], 'native_head': '894084738d95a7239611c2b8fbecfe081d3a7fc9', 'native_worktree': 'five changed sources; not clean'}
sidecar = base / 'frozen-tool.json'
if sidecar.is_file():
    raw = sidecar.read_bytes()
    extra['frozen_tool'] = json.loads(raw)
    extra['sidecar'] = {'path': str(sidecar), 'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}
    p = Path(extra['frozen_tool']['frozen_path'])
    info = p.stat()
    extra['frozen_ref'] = {'path': str(p), 'sha256': hashlib.sha256(p.read_bytes()).hexdigest(), 'bytes': info.st_size, 'mode': stat.S_IMODE(info.st_mode), 'device': info.st_dev, 'inode': info.st_ino, 'mtime_ns': info.st_mtime_ns, 'ctime_ns': info.st_ctime_ns, 'nlink': info.st_nlink, 'symlink': p.is_symlink()}
print(json.dumps({'scope': '只读软件进度，无重启或UART', 'terminal_result_present': result.is_file(), 'commands': rows, 'extra': extra}, ensure_ascii=False))
PY
