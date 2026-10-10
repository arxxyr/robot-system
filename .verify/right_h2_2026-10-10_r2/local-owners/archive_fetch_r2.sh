#!/bin/bash
# 仅流式读取本次终态原件；不打开 UART，不修改远端文件。
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12-linux-aarch64-gnu/bin/python3.12 - <<'PY'
import json, os, stat, sys, tarfile
from pathlib import Path
home = Path('/home/loosqk')
base = home / 'robot-checks/h2-exit-observation-20261009-r1'
roots = [base / name for name in (
    'right-h2-run-02', 'right-h2-cleanup-r2', 'register-readback-r2',
    'right-h2-final-readonly-r2', 'final-buses-r2', 'right-readonly-r2',
    'bootstrap/tmux-hardware-event-r2')]
roots += [base / 'bootstrap' / name for name in (
    'h2-driver-r2.log', 's52-right-physical-r2.json', 's52-right-inputs-r2.json',
    's52-alignment-r2.json', 's52-alignment-identity-r2.json',
    's52-alignment-source-r2.json', 's52-right-go-r2.json',
    's52-right-h2-ssh-wait-r2.json')]
roots += [base / 'bootstrap/control-v3' / name for name in (
    'prelaunch_go.py', 'post_h2_cleanup.py', 'ssh_owner.py',
    'tmux_h2_observer_event_once_r2.sh', 'h2_launch_r2.sh',
    'readback_serial_once_raw_r2.py')]
inputs = json.loads((base / 'bootstrap/s52-right-inputs-r2.json').read_bytes())
roots += [Path(inputs[key]['path']) for key in ('source', 'production', 'measurement', 'installed', 'calibration')]
plan_path = home / 'robot-checks/h2-right-ready-20261009-r2/bootstrap/s52-readback-plan-r1.json'
plan = json.loads(plan_path.read_bytes())
roots += [plan_path, Path(plan['binary_build_result']['path']), Path(plan['binary_build_receipt']['path'])]
files = {}
for root in roots:
    rows = sorted(root.rglob('*')) if root.is_dir() else [root]
    for path in rows:
        metadata = path.lstat()
        if stat.S_ISDIR(metadata.st_mode):
            continue
        if not stat.S_ISREG(metadata.st_mode) or path.is_symlink():
            raise ValueError('原件含非普通文件: ' + str(path))
        relative = path.relative_to(home)
        if path.name.endswith(('.tar', '.tar.gz', '.pyc')) or 'source' in relative.parts or 'artifacts' in relative.parts:
            raise ValueError('原件范围含源码/ELF/旧包: ' + str(path))
        with path.open('rb') as stream:
            if stream.read(4) == b'\x7fELF':
                raise ValueError('禁止归档 ELF: ' + str(path))
        files[path] = (str(relative), metadata)
with tarfile.open(fileobj=sys.stdout.buffer, mode='w|gz') as archive:
    for path, (name, before) in sorted(files.items()):
        with path.open('rb') as stream:
            info = archive.gettarinfo(str(path), arcname=name)
            archive.addfile(info, stream)
        after = path.stat()
        if (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns):
            raise ValueError('读取期间原件变化: ' + str(path))
PY
