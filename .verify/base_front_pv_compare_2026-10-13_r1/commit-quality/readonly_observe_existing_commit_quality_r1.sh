#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
from pathlib import Path
import json,subprocess,time
phase=Path('/home/loosqk/robot-checks/base-front-pv-compare-r1-commit-quality-20261013')
result={'status':'READ_ONLY_EXISTING_COMMIT_QUALITY_PHASE','phase':str(phase),'phase_exists':phase.exists(),'scope':'仅既有phase状态，不启动batch/不UART/USB/fuser/服务/GO'}
for name in ['boot/start.json','boot/terminal.json','boot/reclaim.json','quality/result.json']:
 p=phase/name
 if p.is_file():result[name]=json.loads(p.read_text())
for name in ['nightly_update','rustc_before','fmt','clippy','rustc_after']:
 p=phase/'quality'/name/'completion.json'
 if p.is_file():result['command_'+name]=json.loads(p.read_text())
start=result.get('boot/start.json')
if start is not None and 'boot/reclaim.json' not in result:
 session,window,pane,pid=start['terminal_identity']
 argv=['tmux','display-message','-p','-t',pane,'#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}|#{pane_dead_status}']
 child=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE);out,err=child.communicate();code=child.wait()
 result['existing_pane']={'argv':argv,'pid':child.pid,'exit_code':code,'reaped':True,'stdout':out.decode(),'stderr':err.decode()}
 if code==0:assert out.decode().strip().split('|')[:4]==[session,window,pane,str(pid)]
print(json.dumps(result,ensure_ascii=False),flush=True)
PYREMOTE
