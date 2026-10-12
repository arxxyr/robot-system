#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PY'
import pathlib,json,hashlib,os
p=pathlib.Path('/home/loosqk/robot-checks/position-return-20261010-r1/base-zero-diagnostic-r1')
for relative in ['base-zero-diagnostic-launch-r1/driver.log','base-zero-diagnostic-launch-r1/launch_base_zero_diagnostic_r1.sh','base-zero-diagnostic-r1/run.json','base-zero-diagnostic-r1.go.json.consumed']:
 f=p/relative
 if f.exists():
  b=f.read_bytes();print(json.dumps({'path':str(f),'bytes':len(b),'sha256':hashlib.sha256(b).hexdigest(),'content':b.decode() if len(b)<40000 else json.loads(b)},ensure_ascii=False),flush=True)
 else:print(json.dumps({'path':str(f),'exists':False}),flush=True)
proc=pathlib.Path('/proc/653725')
print(json.dumps({'owned_driver_proc_exists':proc.exists(),'proc_stat':(proc/'stat').read_text() if proc.exists() else None}),flush=True)
PY
