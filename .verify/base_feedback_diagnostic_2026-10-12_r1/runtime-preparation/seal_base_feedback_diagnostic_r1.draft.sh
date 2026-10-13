#!/bin/bash
# 爸爸，仅封存本轮原件、核真实终态和既有17项非UART后验。
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,importlib.util,json,os,stat,subprocess,tarfile,time
from pathlib import Path
# 爸爸，root只在本轮实际终态后绑定；未绑定时先拒绝，零目标文件I/O和child。
APPROVED_RUN_SHA256=None
APPROVED_RUN_BYTES=None
APPROVED_SESSION=None
APPROVED_WINDOW=None
APPROVED_PANE=None
APPROVED_DRIVER_PID=None
APPROVED_PANE_EXIT=None
APPROVED_HARDWARE_CHILD_COUNT=None
FIELD_EXPECTED=None
GO_EXPECTED=None
BINDING_EXPECTED=None
PLAN_EXPECTED=None
PLAN_CANONICAL_SHA=None
NATIVE_PREPARATION_EXPECTED=None
assert type(APPROVED_RUN_SHA256) is str and len(APPROVED_RUN_SHA256)==64 and type(APPROVED_RUN_BYTES) is int and APPROVED_RUN_BYTES>0, '爸爸，BASE_FEEDBACK_R1收集草稿未绑定本轮run原件，零I/O拒绝。'
assert all(type(v) is str and v for v in (APPROVED_SESSION,APPROVED_WINDOW,APPROVED_PANE)) and type(APPROVED_DRIVER_PID) is int and APPROVED_DRIVER_PID>0 and type(APPROVED_PANE_EXIT) is int and APPROVED_PANE_EXIT in (0,1) and type(APPROVED_HARDWARE_CHILD_COUNT) is int and APPROVED_HARDWARE_CHILD_COUNT>0, '爸爸，BASE_FEEDBACK_R1收集草稿未绑定本轮唯一终态，零I/O拒绝。'
assert APPROVED_SESSION=='base-feedback-diagnostic-trial-20261012-r1', '爸爸，终态session不是本轮BASE_FEEDBACK_R1，零I/O拒绝。'
assert all(v is not None for v in (FIELD_EXPECTED,GO_EXPECTED,BINDING_EXPECTED,PLAN_EXPECTED,NATIVE_PREPARATION_EXPECTED)) and type(PLAN_CANONICAL_SHA) is str and len(PLAN_CANONICAL_SHA)==64, '爸爸，只读反馈收集草稿未绑定本轮现场/GO/软件准备原件，零I/O拒绝。'
legacy=Path('/home/loosqk/robot-checks/position-return-20261010-r1');base=Path('/home/loosqk/robot-checks/base-feedback-diagnostic-r1');phase=base;evidence=phase/'base-feedback-diagnostic-r1';launch=phase/'base-feedback-diagnostic-launch-r1';out=phase/'base-feedback-diagnostic-seal-r1'
runraw=(evidence/'run.json').read_bytes();run=json.loads(runraw)
assert hashlib.sha256(runraw).hexdigest()==APPROVED_RUN_SHA256 and len(runraw)==APPROVED_RUN_BYTES
assert run['schema']=='MAINTENANCE_RUN_V2' and run['plan']['run_id']=='base-feedback-diagnostic-r1'
assert run['result'] in ('FAIL','NEEDS_MANUAL_POWER_CUT') and type(run['manual_power_cut_required']) is bool
manual=run['manual_power_cut_required'];assert manual==(run['result']=='NEEDS_MANUAL_POWER_CUT')
assert len(run['commands'])==APPROVED_HARDWARE_CHILD_COUNT
assert APPROVED_PANE_EXIT==1
# 爸爸，停止/身份未确认只收真实原件和所属已退出pane，不做UART、动作或新的通过宣称。
if manual:print('爸爸，原报告仍要求人工断电，请立即切断两个电机电源并保持支撑；本收集器禁止UART和新动作。',flush=True)
else:
 assert len(run['owned_locks'])==4 and all(x['owner_pid']==APPROVED_DRIVER_PID for x in run['owned_locks'])
 assert all(v['reaped'] is True for v in run['commands']) and run['evidence_errors']==[] and not run['child_active'] and run['received_signal'] is None
 assert run['stop_verification']['status']==run['identity_verification']['status']=='PASS'
 assert run['position_verification']['status'] in ('PASS','FAIL','UNKNOWN')
 assert run['plan']['position_targets']==[] and run['plan']['firmware_contract_verified'] is False
 assert run['position_function_test']=='NOT_RUN_NO_ACTIVE_MOTION'
 expected_base={('right',8,'base_front_wheel'),('right',7,'base_left_wheel'),('right',9,'base_right_wheel')}
 planned_base=run['plan']['base_targets'];base_probes=run['probes']
 assert len(planned_base)==len(base_probes)==3
 for rows in (planned_base,base_probes):
  assert {(p['side'],p['motor'],p['name']) for p in rows}==expected_base
  assert all(p['decision']=='DEFERRED' and p['reason_code']=='BASE_FIRMWARE_CONTRACT_UNVERIFIED' for p in rows)
 assert all(p['kind']=='velocity' and p['status']=='NOT_RUN' for p in base_probes)
 feedback=run['feedback_diag'];observation=feedback['observation'];groups=observation['groups'];trial=run['plan']['feedback_diag']
 assert (feedback['side'],feedback['motor'],feedback['name'],feedback['kind'])==('right',9,'base_right_wheel','feedback_diag')
 assert feedback['status']=='PASS' and feedback['firmware_contract_verified'] is False and feedback['uart_blocked'] is False and feedback['errors']==[]
 assert feedback['register_writes']==feedback['motion_commands']==0 and trial['write_sequence']==[]
 assert len(feedback['preflight'])==7 and {p['register']:p['raw']['raw_unsigned'] for p in feedback['preflight']}=={k:v for k,v in trial['expected_raw'].items() if k!='Torque_Enable'}
 assert observation['budget_ns']==500000000 and observation['deadline_ns']==observation['started_ns']+500000000
 assert observation['status']=='PASS' and observation['in_budget_pv0_groups']==3 and len(groups)==3
 assert [g['requested_offset_ns'] for g in groups]==[0,200000000,400000000]
 for group in groups:
  assert group['scheduled_ns']==observation['started_ns']+group['requested_offset_ns'] and group['status']=='PASS' and group['completed_in_budget'] is True and group['raw_pv_zero'] is True
  assert [sample['register'] for sample in group['reads']]==['Torque_Enable','Present_Position','Present_Velocity']
  assert all(sample['status']=='READ_VALID' and sample['completed_in_budget'] is True and sample['finished_ns']<=observation['deadline_ns'] for sample in group['reads'])
  assert group['reads'][0]['raw']['raw_unsigned']==group['reads'][2]['raw']['raw_unsigned']==0
  assert all(type(sample['raw']['raw_unsigned']) is int for sample in group['reads'])
assert APPROVED_PANE_EXIT==(0 if run['result']=='PASS' else 1)
expected={'field_confirmation_base_feedback_r1.json':FIELD_EXPECTED,'base-feedback-diagnostic-r1.go.json':GO_EXPECTED,'maintenance_bindings_base_feedback_diagnostic_r1.json':BINDING_EXPECTED,'full-plan-only-r1.stdout':PLAN_EXPECTED,'prepare-result-base-feedback-r1.json':NATIVE_PREPARATION_EXPECTED}
for name,(sha,size) in expected.items():
 path=phase/name;raw=path.read_bytes();assert path.is_file() and not path.is_symlink() and hashlib.sha256(raw).hexdigest()==sha and len(raw)==size
full=json.loads((phase/'full-plan-only-r1.stdout').read_text());assert full['plan_sha256']==PLAN_CANONICAL_SHA and hashlib.sha256(json.dumps(full['plan'],sort_keys=True,ensure_ascii=False,separators=(',',':')).encode()).hexdigest()==PLAN_CANONICAL_SHA and run['plan']==full['plan'] and run['go']['plan_sha256']==PLAN_CANONICAL_SHA
out.mkdir(mode=0o700,exist_ok=True);assert not any(out.iterdir())
start=json.loads((launch/'start_receipt_base_feedback_r1.json').read_text());assert start['tmux_identity']==' '.join((APPROVED_SESSION,APPROVED_WINDOW,APPROVED_PANE,str(APPROVED_DRIVER_PID)))
bindings=json.loads((phase/'maintenance_bindings_base_feedback_diagnostic_r1.json').read_text())
def pins():
 values=[]
 for ref in bindings['refs']:
  path=Path(ref['path']);raw=path.read_bytes();info=path.lstat();assert stat.S_ISREG(info.st_mode) and hashlib.sha256(raw).hexdigest()==ref['sha256'] and len(raw)==ref['bytes']
  values.append({'path':str(path),'sha256':hashlib.sha256(raw).hexdigest(),'bytes':len(raw),'mode':stat.S_IMODE(info.st_mode),'uid':info.st_uid,'gid':info.st_gid,'device':info.st_dev,'inode':info.st_ino,'mtime_ns':info.st_mtime_ns,'ctime_ns':info.st_ctime_ns})
 return values
before=[] if manual else pins();rows=[]
def save(name,value):
 with (out/name).open('x') as f:json.dump(value,f,ensure_ascii=False,indent=2);f.write('\n');f.flush();os.fsync(f.fileno())
def command(name,argv):
 row={'name':name,'argv':argv,'started_ns':time.time_ns(),'signals_sent':[],'reaped':False};child=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE);row['pid']=child.pid;save(name+'.launch.json',row);stdout,stderr=child.communicate();code=child.wait();row.update(finished_ns=time.time_ns(),exit_code=code,wait_returncode=code,reaped=True)
 for suffix,data in [('stdout',stdout),('stderr',stderr)]:
  path=out/(name+'.'+suffix);path.write_bytes(data);row[suffix]={'path':str(path),'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()}
 save(name+'.receipt.json',row);rows.append(row);assert code==0,row;return stdout
state=command('terminal-owned-pane',['tmux','display-message','-p','-t',APPROVED_PANE,'#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}|#{pane_dead_status}']).decode().strip();assert state=='|'.join((APPROVED_SESSION,APPROVED_WINDOW,APPROVED_PANE,str(APPROVED_DRIVER_PID),'1',str(APPROVED_PANE_EXIT))),state
command('reclaim-only-owned-dead-pane',['tmux','kill-pane','-t',APPROVED_PANE])
fresh=None;after=[];snapshot=None
if not manual:
 probe=legacy/'normal-read-r4/nonuart_probe_r4.sh';(out/'existing-nonuart-probe-r4.sh').write_bytes(probe.read_bytes());fresh=json.loads(command('independent-nonuart17',['bash',str(probe)]));assert fresh['status']=='PASS_NON_UART_CURRENT_FACTS' and len(fresh['checks'])==17 and all(v['status']=='PASS' for v in fresh['checks']) and len(fresh['commands'])==23 and all(v['reaped'] is True for v in fresh['commands'])
 assert not fresh['uart_open_or_read'] and fresh['hardware_writes']==0 and fresh['errors']==fresh['received_signals']==[] and fresh['identity_before']==fresh['identity_after'] and fresh['files_before']==fresh['files_after'] and fresh['services']['valid']
 assert fresh['identity_before']['boot_id']==bindings['host']['boot_id'];after=pins();assert before==after;save('bound-refs-before.json',before);save('bound-refs-after.json',after)
 art=Path('/home/loosqk/robot-checks/h2-exit-observation-20261009-r1/source/scripts/post_e_h2_artifacts.py');assert hashlib.sha256(art.read_bytes()).hexdigest()=='bd2b3f7bb8527cd9ecec73894171638e9cf378656b89e284a585c3baa37b8731'
 spec=importlib.util.spec_from_file_location('base_zero_r1_seal_existing_artifacts',art);a=importlib.util.module_from_spec(spec);spec.loader.exec_module(a)
 source=legacy/'position-return-r7/source';expected=json.loads(Path('/home/loosqk/robot-checks/position-return-20261011-r12/native-quality/boot/host_source_final_r12.json').read_text());snapshot=a.snapshot_source(source,list(expected['files']));assert snapshot==expected;save('native-source-seal-window.json',snapshot)
report={'status':'BASE_FEEDBACK_R1_INCIDENT_ORIGINALS_COLLECTED_MANUAL_CUT_REQUIRED' if manual else 'PASS_BASE_FEEDBACK_R1_NONUART_COLLECTION_ORIGINAL_RESULT_PRESERVED','terminal':state,'driver_pid':APPROVED_DRIVER_PID,'original_run_status':run['result'],'original_run_sha256':hashlib.sha256(runraw).hexdigest(),'original_run_bytes':len(runraw),'original_error':run.get('error'),'feedback_diag':run.get('feedback_diag'),'active_zero_speed_function':'NOT_RUN_NO_ENABLE_OR_WRITE','position':run.get('position_verification'),'stop':run.get('stop_verification'),'identity':run.get('identity_verification'),'probes':run.get('probes'),'direct_hardware_children':len(run['commands']),'all_hardware_children_reaped':all(v['reaped'] is True for v in run['commands']),'child_active':run['child_active'],'evidence_errors':run['evidence_errors'],'manualcut':manual,'independent_nonUART_checks':len(fresh['checks']) if fresh else None,'independent_nonUART_children':len(fresh['commands']) if fresh else None,'manual_incident_extra_postchecks':'NOT_RUN_ORIGINAL_FAILURE_PRESERVED' if manual else None,'bound_refs':len(bindings['refs']),'bound_refs_before_after_equal':before==after if not manual else None,'native_source_files':len(snapshot['files']) if snapshot else None,'native_source_sha256':snapshot['source_manifest_sha256'] if snapshot else None,'collector_direct_children':rows,'new_UART':0,'new_writes':0,'new_GO':False};save('seal-result-base-feedback-r1.json',report)
selected=['maintenance_bindings_base_feedback_diagnostic_r1.json','full-plan-only-r1.stdout','prepare-result-base-feedback-r1.json','field_confirmation_base_feedback_r1.json','base-feedback-diagnostic-r1.go.json','base-feedback-diagnostic-r1.go.json.consumed'];paths=sorted(x for d in [evidence,launch,out] for x in d.rglob('*') if x.is_file());paths += [phase/n for n in selected];assert all(x.is_file() and not x.is_symlink() for x in paths)
extra_sources=[(Path('/home/loosqk/robot-checks/base-feedback-r1-software/candidate/base_feedback_diagnostic_r1.py'),'candidate/base_feedback_diagnostic_r1.py'),(Path('/home/loosqk/robot-checks/position-return-20261010-r1/base-zero-diagnostic-r1/helpers/raw_right9_from_r14_r1.py'),'helpers/raw_right9_from_r14_r1.py'),(Path('/home/loosqk/robot-checks/position-return-20261010-r1/software-facts-r1/scripts/run_hardware_maintenance.py'),'scripts/run_hardware_maintenance.py')]
assert all(path.is_file() and not path.is_symlink() for path,name in extra_sources)
archive=phase/'hardware-originals-base-feedback-r1.tar.gz'
with tarfile.open(archive,'x:gz') as t:
 for path in paths:t.add(path,arcname=str(path.relative_to(phase)),recursive=False)
 for path,name in extra_sources:t.add(path,arcname=name,recursive=False)
raw=archive.read_bytes();print(json.dumps({'status':'HARDWARE_BASE_FEEDBACK_R1_ARCHIVE_READY','sha256':hashlib.sha256(raw).hexdigest(),'bytes':len(raw),'files':len(paths)+len(extra_sources),'source_bytes':sum(x.stat().st_size for x in paths)+sum(x.stat().st_size for x,name in extra_sources),'report':report,'base64':base64.b64encode(raw).decode()},ensure_ascii=False),flush=True)
PYREMOTE
