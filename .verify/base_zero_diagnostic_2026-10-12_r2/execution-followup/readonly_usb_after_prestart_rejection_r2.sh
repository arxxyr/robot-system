#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import hashlib,json,os,stat
from pathlib import Path
base=Path('/home/loosqk/robot-checks/position-return-20261010-r1');phase=base/'base-zero-diagnostic-r1'
binding=json.loads((phase/'maintenance_bindings_base_zero_diagnostic_r1.json').read_text());rows=[]
for side,expected in binding['usb'].items():
 p=Path(expected['device']);row={'side':side,'expected':expected,'exists':p.exists(),'is_symlink':p.is_symlink()}
 if p.exists():
  st=p.stat();ls=p.lstat();row['actual']={'device':str(p),'resolved':str(p.resolve()),'rdev':st.st_rdev,'device_inode':st.st_ino,'link_inode':ls.st_ino,'mode':stat.S_IMODE(st.st_mode),'uid':st.st_uid,'gid':st.st_gid}
 rows.append(row)
print(json.dumps({'status':'READ_ONLY_USB_METADATA_AFTER_ZERO_START_REJECTION','boot_id':Path('/proc/sys/kernel/random/boot_id').read_text().strip(),'usb':rows,'r2_output_exists':(phase/'base-zero-diagnostic-r2').exists(),'r2_launch_exists':(phase/'base-zero-diagnostic-launch-r2').exists(),'r2_go_consumed_exists':(phase/'base-zero-diagnostic-r2.go.json.consumed').exists(),'UART':0,'writes':0},ensure_ascii=False),flush=True)
PYREMOTE
