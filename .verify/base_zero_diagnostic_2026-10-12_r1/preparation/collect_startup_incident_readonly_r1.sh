#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PY'
import pathlib,tarfile,io,json,hashlib,base64
phase=pathlib.Path('/home/loosqk/robot-checks/position-return-20261010-r1/base-zero-diagnostic-r1')
files=sorted(p for p in phase.rglob('*') if p.is_file());assert all(not p.is_symlink() for p in files)
buffer=io.BytesIO()
with tarfile.open(fileobj=buffer,mode='w:gz') as tar:
 for p in files:tar.add(p,arcname=str(p.relative_to(phase)),recursive=False)
raw=buffer.getvalue()
print(json.dumps({'status':'READ_ONLY_STARTUP_INCIDENT_ORIGINALS','files':len(files),'sha256':hashlib.sha256(raw).hexdigest(),'bytes':len(raw),'original_GO_consumed':(phase/'base-zero-diagnostic-r1.go.json.consumed').exists(),'original_output_exists':(phase/'base-zero-diagnostic-r1').exists(),'driver_proc_exists':pathlib.Path('/proc/653725').exists(),'base64':base64.b64encode(raw).decode()}),flush=True)
PY
