set -eu
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
exec uv run --no-project python - <<'REMOTE_PY'
from pathlib import Path
import base64,hashlib,json,stat,time
base=Path('/home/loosqk/robot-checks/h2-exit-observation-20261009-r1')
paths=['right-h2-run-02/run.json','right-h2-run-02/report.json',
 'right-h2-run-02/orin_sla_right/final_stop.json','right-h2-run-02/orin_sla_right/final_trace.json',
 'right-h2-run-02/orin_sla_right/rt_preflight.json',
 'right-h2-run-02/orin_sla_right/sla_false_false.json','right-h2-run-02/orin_sla_right/sla_false_true.json',
 'right-h2-run-02/orin_sla_right/sla_true_false.json','right-h2-run-02/orin_sla_right/sla_true_true.json',
 'bootstrap/h2-driver-r2.log','bootstrap/tmux-hardware-event-r2/pane.txt','right-h2-cleanup-r2/result.json']
started=time.time_ns()
for name in paths:
 path=base/name
 before=path.lstat()
 assert path==path.resolve(strict=True) and stat.S_ISREG(before.st_mode) and before.st_nlink==1,name
 data=path.read_bytes();after=path.lstat()
 assert (before.st_dev,before.st_ino,before.st_size,before.st_mtime_ns,before.st_ctime_ns)==(after.st_dev,after.st_ino,after.st_size,after.st_mtime_ns,after.st_ctime_ns),name
 print(json.dumps({'path':name,'sha256':hashlib.sha256(data).hexdigest(),'bytes':len(data),'base64':base64.b64encode(data).decode()},separators=(',',':')),flush=True)
print(json.dumps({'terminal':True,'status':'PASS_READONLY_FETCH','started_ns':started,'finished_ns':time.time_ns(),'files':len(paths),'UART':False,'writes':0}),flush=True)
REMOTE_PY
