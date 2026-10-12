#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYLAUNCH'
import errno,json,os,subprocess,sys,time
from pathlib import Path
CASE=Path('/home/loosqk/robot-checks/base-zero-r3-launcher-repro-20261012-r1/old-heredoc')
PYTHON='/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3'
assert CASE.is_absolute() and CASE.is_dir()
context={'scope':'SOFTWARE_ONLY_LAUNCHER_REPRO','driver_pid':os.getpid(),'ppid':os.getppid(),'case':'old-heredoc','started_ns':time.time_ns(),'cwd':os.getcwd(),'env':{key:os.environ.get(key) for key in ('TMUX','TMUX_PANE','TMUX_TMPDIR','TERM','PATH')},'fd_before':{str(fd):{'target':os.readlink('/proc/self/fd/'+str(fd)),'isatty':os.isatty(fd),'mode':os.fstat(fd).st_mode} for fd in (0,1,2)}}
with (CASE/'context-before.json').open('x') as stream:json.dump(context,stream,ensure_ascii=False,indent=2);stream.write('\n')
log=os.open(str(CASE/'driver.log'),os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
os.dup2(log,1);os.dup2(log,2);os.close(log)
real_popen=subprocess.Popen
children=[]
def recording_popen(*args,**kwargs):
 child=real_popen(*args,**kwargs);children.append((child,time.time_ns()));return child
subprocess.Popen=recording_popen
row={'scope':'SOFTWARE_ONLY_LAUNCHER_REPRO','driver_pid':os.getpid(),'pane':os.environ['TMUX_PANE'],'after_option_reached':False,'children':[],'fd_after':{str(fd):{'target':os.readlink('/proc/self/fd/'+str(fd)),'isatty':os.isatty(fd)} for fd in (0,1,2)},'UART':0,'GO':False,'hardware_candidate_started':False}
try:
 subprocess.run(['tmux','set-option','-p','-t',os.environ['TMUX_PANE'],'remain-on-exit','on'],check=True)
 row['after_option_reached']=True
finally:
 subprocess.Popen=real_popen
 for child,started in children:
  returned=child.wait();reaped=False
  try:os.waitpid(child.pid,os.WNOHANG)
  except ChildProcessError as error:reaped=error.errno==errno.ECHILD
  row['children'].append({'argv':child.args,'pid':child.pid,'started_ns':started,'wait_returncode':returned,'exit_code':returned,'reaped':reaped,'signals_sent':[]})
 row['finished_ns']=time.time_ns()
 with (CASE/'driver-receipt.json').open('x') as stream:json.dump(row,stream,ensure_ascii=False,indent=2);stream.write('\n')
marker="import json,os;from pathlib import Path;Path("+repr(str(CASE/'marker.json'))+").write_text(json.dumps({'status':'SOFTWARE_ONLY_REACHED','pid':os.getpid(),'UART':0,'GO':False})+'\\n')"
os.execv(PYTHON,[PYTHON,'-c',marker])
PYLAUNCH
