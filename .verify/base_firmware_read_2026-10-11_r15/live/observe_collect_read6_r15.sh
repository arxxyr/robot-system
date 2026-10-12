#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,io,json,subprocess,sys,tarfile,time
from pathlib import Path
PHASE=Path('/home/loosqk/robot-checks/position-return-20261010-r1/base-firmware-read-r15')
DRIVER_PID=646895
PANE='%81'
assert type(DRIVER_PID) is int and isinstance(PANE,str), '未绑定唯一已启动进程，拒绝观察和收集'
record=PHASE/'read6-launch-record-r15'
receipts=[]
def run(name,argv):
    started=time.time_ns();child=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
    stdout,stderr=child.communicate();code=child.wait()
    row={'name':name,'argv':argv,'pid':child.pid,'started_ns':started,'finished_ns':time.time_ns(),'exit_code':code,'reaped':True,'signals_sent':[],'stdout':stdout.decode(),'stderr':stderr.decode()}
    receipts.append(row);return row
deadline=time.monotonic()+120
terminal=None
while time.monotonic()<deadline:
    row=run('observe-owned-pane',['tmux','display-message','-p','-t',PANE,'#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}|#{pane_dead_status}'])
    assert row['exit_code']==0 and not row['stderr']
    fields=row['stdout'].strip().split('|')
    assert fields[0]=='base-firmware-read6-20261011-r15' and fields[2]==PANE and int(fields[3])==DRIVER_PID
    if fields[4]=='1':terminal=row['stdout'].strip();break
    time.sleep(0.25)
assert terminal is not None,'未自然退出，不发信号或重试'
report=json.loads((PHASE/'read6-evidence-r15/run.json').read_text())
code=int(terminal.split('|')[-1])
assert report['pid']==DRIVER_PID and not report['child_active'] and all(c['reaped'] for c in report['commands'])
assert code==(0 if report['status']=='PASS_BASE_FIRMWARE_READ6_CAPTURE' else 1)
(record/'terminal-r15.json').write_text(json.dumps({'terminal':terminal,'driver_pid':DRIVER_PID,'exit_code':code,'same_ssh_children':receipts},ensure_ascii=False,indent=2)+'\n')
reclaim=run('reclaim-only-natural-dead-owned-pane',['tmux','kill-pane','-t',PANE]);assert reclaim['exit_code']==0
(record/'reclaim-r15.json').write_text(json.dumps(reclaim,ensure_ascii=False,indent=2)+'\n')
fresh=run('independent-nonuart-only',['bash','/home/loosqk/robot-checks/position-return-20261010-r1/normal-read-r4/nonuart_probe_r4.sh'])
facts=json.loads(fresh['stdout'])
assert fresh['exit_code']==0 and not fresh['stderr'] and facts['status']=='PASS_NON_UART_CURRENT_FACTS' and not facts['errors']
(record/'independent-nonuart-r15.json').write_text(json.dumps(facts,ensure_ascii=False,indent=2)+'\n')
(record/'independent-nonuart-receipt-r15.json').write_text(json.dumps(fresh,ensure_ascii=False,indent=2)+'\n')
selected=[PHASE/'read6-evidence-r15',record,PHASE/'config.final.base_firmware_read6.r15.json',PHASE/'field_confirmation_r15.json',PHASE/'base_firmware_read6_r15.go.json']
buffer=io.BytesIO()
with tarfile.open(fileobj=buffer,mode='w:gz') as tar:
    for item in selected:
        paths=sorted(item.rglob('*')) if item.is_dir() else [item]
        for path in paths:
            assert not path.is_symlink()
            if path.is_file():tar.add(path,arcname=str(path.relative_to(PHASE)),recursive=False)
raw=buffer.getvalue()
with tarfile.open(fileobj=io.BytesIO(raw)) as tar:count=sum(m.isfile() for m in tar)
print(json.dumps({'status':report['status'],'manual_power_cut_requested':report['manual_power_cut_requested'],'driver_pid':DRIVER_PID,'exit_code':code,'readouts':report['readouts'],'direct_children':len(report['commands']),'independent_nonuart_checks':len(facts['checks']),'independent_nonuart_children':len(facts['commands']),'archive_files':count,'archive_bytes':len(raw),'archive_sha256':hashlib.sha256(raw).hexdigest(),'archive_base64':base64.b64encode(raw).decode()},ensure_ascii=False),flush=True)
sys.exit(code)
PYREMOTE
