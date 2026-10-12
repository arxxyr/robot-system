#!/bin/bash
# 仅封存本轮原件、核真实终态和既有17项非UART后验。
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,importlib.util,json,os,stat,subprocess,tarfile,time
from pathlib import Path
# root只在本轮实际终态后绑定；未绑定时先拒绝，零目标文件I/O和child。
APPROVED_RUN_SHA256=None
APPROVED_RUN_BYTES=None
APPROVED_SESSION=None
APPROVED_WINDOW=None
APPROVED_PANE=None
APPROVED_DRIVER_PID=None
APPROVED_PANE_EXIT=None
APPROVED_HARDWARE_CHILD_COUNT=None
assert type(APPROVED_RUN_SHA256) is str and len(APPROVED_RUN_SHA256)==64 and type(APPROVED_RUN_BYTES) is int and APPROVED_RUN_BYTES>0, '爸爸，R16收集草稿未绑定本轮run原件，零I/O拒绝。'
assert all(type(v) is str and v for v in (APPROVED_SESSION,APPROVED_WINDOW,APPROVED_PANE)) and type(APPROVED_DRIVER_PID) is int and APPROVED_DRIVER_PID>0 and type(APPROVED_PANE_EXIT) is int and APPROVED_PANE_EXIT in (0,1) and type(APPROVED_HARDWARE_CHILD_COUNT) is int and APPROVED_HARDWARE_CHILD_COUNT>0, '爸爸，R16收集草稿未绑定本轮唯一终态，零I/O拒绝。'
base=Path('/home/loosqk/robot-checks/position-return-20261010-r1');phase=base/'head1-p32-reverse-r16';evidence=phase/'head1-p32-reverse-r16';launch=phase/'head1-p32-reverse-launch-r16';out=phase/'head1-p32-reverse-seal-r16';out.mkdir(mode=0o700,exist_ok=True);assert not any(out.iterdir())
runraw=(evidence/'run.json').read_bytes();run=json.loads(runraw)
assert hashlib.sha256(runraw).hexdigest()==APPROVED_RUN_SHA256 and len(runraw)==APPROVED_RUN_BYTES
assert len(run['owned_locks'])==4 and all(x['owner_pid']==APPROVED_DRIVER_PID for x in run['owned_locks']) and run['result'] in ('PASS','FAIL') and len(run['commands'])==APPROVED_HARDWARE_CHILD_COUNT and all(v['reaped'] is True for v in run['commands']) and run['evidence_errors']==[] and not run['child_active'] and not run['manual_power_cut_required']
assert run['position_verification']['status'] in ('PASS','FAIL') and run['stop_verification']['status']==run['identity_verification']['status']=='PASS'
# 实际报告保留一条位置测试及三条未执行的底盘项；分别精确核验。
position_probes=[p for p in run['probes'] if p.get('kind')=='position']
base_probes=[p for p in run['probes'] if p.get('kind')=='velocity']
assert len(run['probes'])==4 and len(position_probes)==1 and len(base_probes)==3
position_probe=position_probes[0]
assert (position_probe['side'],position_probe['motor'],position_probe['name'])==('left',7,'head_motor_1') and position_probe['p_trial']['restore_status']=='PASS'
assert position_probe['p_trial']['restore_raw']['P_Coefficient']['raw_unsigned']==16
assert {(p['side'],p['motor'],p['name']) for p in base_probes}=={('right',8,'base_front_wheel'),('right',7,'base_left_wheel'),('right',9,'base_right_wheel')}
assert all(p['decision']=='DEFERRED' and p['status']=='NOT_RUN' and p['reason_code']=='BASE_FIRMWARE_CONTRACT_UNVERIFIED' for p in base_probes)
assert APPROVED_PANE_EXIT==(0 if run['result']=='PASS' else 1)
start=json.loads((launch/'start_receipt_r16.json').read_text());assert start['tmux_identity']==' '.join((APPROVED_SESSION,APPROVED_WINDOW,APPROVED_PANE,str(APPROVED_DRIVER_PID)))
bindings=json.loads((phase/'maintenance_bindings_head1_p32_reverse_r16.json').read_text())
def pins():
 values=[]
 for ref in bindings['refs']:
  path=Path(ref['path']);raw=path.read_bytes();info=path.lstat();assert stat.S_ISREG(info.st_mode) and hashlib.sha256(raw).hexdigest()==ref['sha256'] and len(raw)==ref['bytes']
  values.append({'path':str(path),'sha256':hashlib.sha256(raw).hexdigest(),'bytes':len(raw),'mode':stat.S_IMODE(info.st_mode),'uid':info.st_uid,'gid':info.st_gid,'device':info.st_dev,'inode':info.st_ino,'mtime_ns':info.st_mtime_ns,'ctime_ns':info.st_ctime_ns})
 return values
before=pins();rows=[]
def save(name,value):
 with (out/name).open('x') as f:json.dump(value,f,ensure_ascii=False,indent=2);f.write('\n');f.flush();os.fsync(f.fileno())
def command(name,argv):
 row={'name':name,'argv':argv,'started_ns':time.time_ns(),'signals_sent':[],'reaped':False};child=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE);row['pid']=child.pid;save(name+'.launch.json',row);stdout,stderr=child.communicate();code=child.wait();row.update(finished_ns=time.time_ns(),exit_code=code,wait_returncode=code,reaped=True)
 for suffix,data in [('stdout',stdout),('stderr',stderr)]:
  path=out/(name+'.'+suffix);path.write_bytes(data);row[suffix]={'path':str(path),'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()}
 save(name+'.receipt.json',row);rows.append(row);assert code==0,row;return stdout
state=command('terminal-owned-pane',['tmux','display-message','-p','-t',APPROVED_PANE,'#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}|#{pane_dead_status}']).decode().strip();assert state=='|'.join((APPROVED_SESSION,APPROVED_WINDOW,APPROVED_PANE,str(APPROVED_DRIVER_PID),'1',str(APPROVED_PANE_EXIT))),state
command('reclaim-only-owned-dead-pane',['tmux','kill-pane','-t',APPROVED_PANE])
probe=base/'normal-read-r4/nonuart_probe_r4.sh';(out/'existing-nonuart-probe-r4.sh').write_bytes(probe.read_bytes());fresh=json.loads(command('independent-nonuart17',['bash',str(probe)]));assert fresh['status']=='PASS_NON_UART_CURRENT_FACTS' and len(fresh['checks'])==17 and all(v['status']=='PASS' for v in fresh['checks']) and len(fresh['commands'])==23 and all(v['reaped'] is True for v in fresh['commands'])
assert not fresh['uart_open_or_read'] and fresh['hardware_writes']==0 and fresh['errors']==fresh['received_signals']==[] and fresh['identity_before']==fresh['identity_after'] and fresh['files_before']==fresh['files_after'] and fresh['services']['valid']
assert fresh['identity_before']['boot_id']==bindings['host']['boot_id'];after=pins();assert before==after;save('bound-refs-before.json',before);save('bound-refs-after.json',after)
art=Path('/home/loosqk/robot-checks/h2-exit-observation-20261009-r1/source/scripts/post_e_h2_artifacts.py');assert hashlib.sha256(art.read_bytes()).hexdigest()=='bd2b3f7bb8527cd9ecec73894171638e9cf378656b89e284a585c3baa37b8731'
spec=importlib.util.spec_from_file_location('r16seal_existing_artifacts',art);a=importlib.util.module_from_spec(spec);spec.loader.exec_module(a)
source=base/'position-return-r7/source';expected=json.loads(Path('/home/loosqk/robot-checks/position-return-20261011-r12/native-quality/boot/host_source_final_r12.json').read_text());snapshot=a.snapshot_source(source,list(expected['files']));assert snapshot==expected;save('native-source-seal-window.json',snapshot)
report={'status':'PASS_R16_NONUART_CLEANUP_ORIGINAL_RESULT_PRESERVED','terminal':state,'driver_pid':APPROVED_DRIVER_PID,'original_run_status':run['result'],'original_run_sha256':hashlib.sha256(runraw).hexdigest(),'original_run_bytes':len(runraw),'original_error':run.get('error'),'position':run['position_verification'],'stop':run['stop_verification']['status'],'restore':position_probe['p_trial']['restore_status'],'identity':run['identity_verification']['status'],'direct_hardware_children':len(run['commands']),'all_hardware_children_reaped':True,'evidence_errors':[],'manualcut':False,'independent_nonUART_checks':17,'independent_nonUART_children':23,'bound_refs':len(before),'bound_refs_before_after_equal':True,'native_source_files':len(snapshot['files']),'native_source_sha256':snapshot['source_manifest_sha256'],'collector_direct_children':rows,'new_UART':0,'new_writes':0,'new_GO':False};save('seal-result-r16.json',report)
selected=['head1_p32_reverse_trial_r16.py','maintenance_bindings_head1_p32_reverse_r16.json','full-plan-only.stdout','field_confirmation_r16.json','head1-p32-reverse-r16.go.json'];paths=sorted(x for d in [evidence,launch,out] for x in d.rglob('*') if x.is_file());paths += [phase/n for n in selected];assert all(x.is_file() and not x.is_symlink() for x in paths)
archive=phase/'hardware-originals-r16.tar.gz'
with tarfile.open(archive,'x:gz') as t:
 for path in paths:t.add(path,arcname=str(path.relative_to(phase)),recursive=False)
raw=archive.read_bytes();print(json.dumps({'status':'HARDWARE_R16_ARCHIVE_READY','sha256':hashlib.sha256(raw).hexdigest(),'bytes':len(raw),'files':len(paths),'source_bytes':sum(x.stat().st_size for x in paths),'report':report,'base64':base64.b64encode(raw).decode()},ensure_ascii=False),flush=True)
PYREMOTE
