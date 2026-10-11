#!/bin/bash
# 只核终态、回收本次自有dead pane并封存；不访问UART。
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,importlib.util,json,os,stat,subprocess,tarfile,time
from pathlib import Path
base=Path('/home/loosqk/robot-checks/position-return-20261010-r1'); phase=base/'runtime-limits-r12'; evidence=phase/'readonly12-evidence-r12'; launch=phase/'readonly12-launch-record-r12'; out=phase/'readonly12-seal-r12'; out.mkdir(mode=0o700)
runraw=(evidence/'run.json').read_bytes(); run=json.loads(runraw)
assert hashlib.sha256(runraw).hexdigest()=='99c5652288551d5618a0e80438b75fe20884d32416f80f2ba31cd63ef1330354' and len(runraw)==420119
assert run['pid']==640784 and run['status']=='PASS_READONLY12_CAPTURE' and len(run['commands'])==270 and all(v['reaped'] is True for v in run['commands']) and run['errors']==run['evidence_errors']==[] and not run['child_active'] and not run['manual_power_cut_requested']
assert len(run['readouts'])==12 and run['register_writes']==run['motion_commands']==0
start=json.loads((launch/'start_receipt_r12.json').read_text()); assert start['tmux_identity']=='position-readonly12-20261011-r12 @71 %71 640784'
rows=[]
def save(name,value):
 with (out/name).open('x') as f:json.dump(value,f,ensure_ascii=False,indent=2);f.write('\n');f.flush();os.fsync(f.fileno())
def command(name,argv,accepted=(0,)):
 row={'name':name,'argv':argv,'started_ns':time.time_ns(),'signals_sent':[],'reaped':False}
 child=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE);row['pid']=child.pid;save(name+'.launch.json',row)
 stdout,stderr=child.communicate();code=child.wait()
 row.update(finished_ns=time.time_ns(),exit_code=code,wait_returncode=code,reaped=True)
 for suffix,data in [('stdout',stdout),('stderr',stderr)]:
  path=out/(name+'.'+suffix);path.write_bytes(data);row[suffix]={'path':str(path),'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()}
 save(name+'.receipt.json',row);rows.append(row);assert code in accepted,row;return stdout
state=command('terminal-owned-pane',['tmux','display-message','-p','-t','%71','#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}|#{pane_dead_status}']).decode().strip()
assert state=='position-readonly12-20261011-r12|@71|%71|640784|1|0',state
command('reclaim-only-owned-dead-pane',['tmux','kill-pane','-t','%71'])
probe=base/'normal-read-r4/nonuart_probe_r4.sh';probe_raw=probe.read_bytes();(out/'existing-nonuart-probe-r4.sh').write_bytes(probe_raw)
fresh=json.loads(command('independent-nonuart17',['bash',str(probe)]))
assert fresh['status']=='PASS_NON_UART_CURRENT_FACTS' and len(fresh['checks'])==17 and all(x['status']=='PASS' for x in fresh['checks'])
assert fresh['uart_open_or_read']==fresh['hardware_writes']==0 and fresh['errors']==[] and fresh['received_signals']==[]
assert len(fresh['commands'])==23 and all(x['reaped'] is True for x in fresh['commands'])
assert fresh['identity_before']==fresh['identity_after'] and fresh['identity_before']['boot_id']=='6cf7ee3e-a3c5-4c91-80bc-4ca924818b53'
assert fresh['files_before']==fresh['files_after'] and fresh['services']['valid']
art=Path('/home/loosqk/robot-checks/h2-exit-observation-20261009-r1/source/scripts/post_e_h2_artifacts.py');assert hashlib.sha256(art.read_bytes()).hexdigest()=='bd2b3f7bb8527cd9ecec73894171638e9cf378656b89e284a585c3baa37b8731'
spec=importlib.util.spec_from_file_location('r12seal_existing_artifacts',art);a=importlib.util.module_from_spec(spec);spec.loader.exec_module(a)
source=base/'position-return-r7/source'; expected=json.loads(Path('/home/loosqk/robot-checks/position-return-20261011-r12/native-quality/boot/host_source_final_r12.json').read_text()); snapshot=a.snapshot_source(source,list(expected['files']));assert snapshot==expected;save('native-source-seal-window.json',snapshot)
report={'status':'PASS_READONLY12_TERMINAL_CLEANUP_AND_INDEPENDENT_NONUART17','driver_pid':640784,'pane':'%71','window':'@71','terminal':state,'run_sha256':hashlib.sha256(runraw).hexdigest(),'run_bytes':len(runraw),'driver_children':270,'driver_children_reaped':270,'readouts':[(r['motor_id'],r['register'],r['raw']['raw_unsigned']) for r in run['readouts']],'independent_non_uart_checks':17,'independent_non_uart_children':23,'all_non_uart_children_reaped':True,'direct_collection_children':rows,'native_source_sha256':snapshot['source_manifest_sha256'],'native_source_files':len(snapshot['files']),'UART_operations_after_run':0,'hardware_writes_after_run':0,'GO_created_after_run':False,'old_hardware_failures_unchanged':True}
save('seal-result-r12.json',report)
selected=['readonly12_r12.py','readonly12_bindings_r12.json','config.readonly12.r12.json','config.readonly12.prep-backup.r12.json','PLAN.readonly12.final.r12.json','root-go-r12.json','field-confirmation-r12.json','final-config-plan.r12.stdout','final-config-plan.r12.stderr','final-upload-r12.json','final-plan-refs-r12.json']
paths=sorted(x for d in [evidence,launch,out] for x in d.rglob('*') if x.is_file())
for name in selected:
 x=phase/name
 if x.exists():paths.append(x)
assert all(not x.is_symlink() for x in paths)
archive=phase/'hardware-originals-r12.tar.gz'
with tarfile.open(archive,'x:gz') as t:
 for x in paths:t.add(x,arcname=str(x.relative_to(phase)),recursive=False)
raw=archive.read_bytes();print(json.dumps({'status':'HARDWARE_R12_ARCHIVE_READY','sha256':hashlib.sha256(raw).hexdigest(),'bytes':len(raw),'files':len(paths),'source_bytes':sum(x.stat().st_size for x in paths),'run_sha256':report['run_sha256'],'report':report,'base64':base64.b64encode(raw).decode()},ensure_ascii=False),flush=True)
PYREMOTE
