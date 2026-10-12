#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,io,json,tarfile
from pathlib import Path
phase=Path('/home/loosqk/robot-checks/position-return-20261010-r1/base-zero-diagnostic-r1');names=['maintenance_bindings_base_zero_diagnostic_r4.json','full-plan-only-r4.stdout','full-plan-only-r4.stderr','full-plan-only-r4.receipt.json','prepare-result-base-zero-r4.json','usb-stat-before-r4.json','usb-stat-after-r4.json'];files=[]
for name in names:
 p=phase/name;r={'name':name,'exists':p.exists()}
 if p.is_file():
  b=p.read_bytes();r.update(sha256=hashlib.sha256(b).hexdigest(),bytes=len(b),base64=base64.b64encode(b).decode())
 files.append(r)
print(json.dumps({'status':'READONLY_R4_PREP_AFTER_SSH255','files':files,'hardware_output_exists':(phase/'base-zero-diagnostic-r4').exists(),'launch_exists':(phase/'base-zero-diagnostic-launch-r4').exists(),'GO_exists':(phase/'base-zero-diagnostic-r4.go.json').exists(),'GO_consumed_exists':(phase/'base-zero-diagnostic-r4.go.json.consumed').exists(),'new_UART':0,'new_writes':0},ensure_ascii=False),flush=True)
PYREMOTE
