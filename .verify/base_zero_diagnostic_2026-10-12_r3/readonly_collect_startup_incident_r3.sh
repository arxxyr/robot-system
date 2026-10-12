#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,io,json,tarfile
from pathlib import Path
phase=Path('/home/loosqk/robot-checks/position-return-20261010-r1/base-zero-diagnostic-r1');launch=phase/'base-zero-diagnostic-launch-r3';names=['maintenance_bindings_base_zero_diagnostic_r3.json','full-plan-only-r3.stdout','full-plan-only-r3.stderr','full-plan-only-r3.receipt.json','prepare-result-base-zero-r3.json','usb-stat-before-r3.json','usb-stat-after-r3.json','usb-refresh-nonuart-r3.stdout','usb-refresh-nonuart-r3.receipt.json','usb-refresh-nonuart-r3.stderr','field_confirmation_base_zero_r3.json','base-zero-diagnostic-r3.go.json']
paths=[phase/n for n in names]+sorted(p for p in launch.rglob('*') if p.is_file());assert all(p.is_file() and not p.is_symlink() for p in paths)
assert not (phase/'base-zero-diagnostic-r3').exists() and not (phase/'base-zero-diagnostic-r3.go.json.consumed').exists()
rawlog=(launch/'driver.log').read_bytes();assert rawlog==b'no such pane: %96\n';assert hashlib.sha256((launch/'launch_base_zero_diagnostic_r3.sh').read_bytes()).hexdigest()=='c78dc05a8914cdd880f86719337356002013a343f5dbfef5d747ccdac8a6299b'
manifest=[{'path':str(p.relative_to(phase)),'sha256':hashlib.sha256(p.read_bytes()).hexdigest(),'bytes':p.stat().st_size} for p in paths];b=io.BytesIO()
with tarfile.open(fileobj=b,mode='w:gz') as t:
 for p in paths:t.add(p,arcname=str(p.relative_to(phase)),recursive=False)
r=b.getvalue();print(json.dumps({'status':'R3_STARTUP_INCIDENT_ORIGINALS_READONLY_COLLECTED','sha256':hashlib.sha256(r).hexdigest(),'bytes':len(r),'files':len(paths),'manifest':manifest,'base64':base64.b64encode(r).decode(),'run_exists':False,'go_consumed_exists':False,'new_UART':0,'new_GO':False,'new_writes':0},ensure_ascii=False),flush=True)
PYREMOTE
