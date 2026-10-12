#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,json,subprocess,time
from pathlib import Path
phase=Path('/home/loosqk/robot-checks/position-return-20261010-r1/base-zero-diagnostic-r1');launch=phase/'base-zero-diagnostic-launch-r3';paths=[launch/'driver.log',launch/'launch_base_zero_diagnostic_r3.sh',phase/'base-zero-diagnostic-r3/run.json',phase/'base-zero-diagnostic-r3.go.json.consumed'];files=[]
for p in paths:
 r={'path':str(p),'exists':p.exists()}
 if p.is_file():
  raw=p.read_bytes();r.update(sha256=hashlib.sha256(raw).hexdigest(),bytes=len(raw),base64=base64.b64encode(raw).decode())
 files.append(r)
a=['tmux','list-panes','-a','-F','#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}|#{pane_dead_status}'];started=time.time_ns();c=subprocess.Popen(a,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE);x,y=c.communicate();code=c.wait();row={'argv':a,'pid':c.pid,'started_ns':started,'finished_ns':time.time_ns(),'exit_code':code,'wait_returncode':code,'reaped':True,'signals_sent':[],'stdout':x.decode(),'stderr':y.decode()}
print(json.dumps({'status':'READ_ONLY_R3_PROCESS_OBSERVATION_FAILURE_ORIGINALS','files':files,'tmux_panes':row,'driver_proc_exists':Path('/proc/654721').exists(),'output_exists':(phase/'base-zero-diagnostic-r3').exists(),'UART':0,'writes':0},ensure_ascii=False),flush=True)
PYREMOTE
