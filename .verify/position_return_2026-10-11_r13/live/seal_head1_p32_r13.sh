#!/bin/bash
# 仅封存本轮原件、核真实终态和既有17项非UART后验。
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,importlib.util,json,os,stat,subprocess,tarfile,time
from pathlib import Path
base=Path('/home/loosqk/robot-checks/position-return-20261010-r1');phase=base/'head-p32-trial-r13';evidence=phase/'head1-p32-trial-r13';launch=phase/'head1-p32-trial-launch-r13';out=phase/'head1-p32-seal-r13';out.mkdir(mode=0o700)
runraw=(evidence/'run.json').read_bytes();run=json.loads(runraw)
assert hashlib.sha256(runraw).hexdigest()=='779e094205df5fb51168d4a911da95a3c70c00a74d406ba8226e5aa77e46df78' and len(runraw)==546834
assert run['pid']==641514 and run['status']=='FAIL' and len(run['commands'])==384 and all(v['reaped'] is True for v in run['commands']) and run['evidence_errors']==[] and not run['child_active'] and not run['manual_power_cut_required']
assert run['position_verification']['status']==run['stop_verification']['status']=='PASS'
start=json.loads((launch/'start_receipt_r13.json').read_text());assert start['tmux_identity']=='position-head1-p32-trial-20261011-r13 @74 %74 641514'
bindings=json.loads((phase/'maintenance_bindings_head_p32_r13.json').read_text())
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
state=command('terminal-owned-pane',['tmux','display-message','-p','-t','%74','#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}|#{pane_dead_status}']).decode().strip();assert state=='position-head1-p32-trial-20261011-r13|@74|%74|641514|1|1',state
command('reclaim-only-owned-dead-pane',['tmux','kill-pane','-t','%74'])
probe=base/'normal-read-r4/nonuart_probe_r4.sh';(out/'existing-nonuart-probe-r4.sh').write_bytes(probe.read_bytes());fresh=json.loads(command('independent-nonuart17',['bash',str(probe)]));assert fresh['status']=='PASS_NON_UART_CURRENT_FACTS' and len(fresh['checks'])==17 and all(v['status']=='PASS' for v in fresh['checks']) and len(fresh['commands'])==23 and all(v['reaped'] is True for v in fresh['commands'])
assert not fresh['uart_open_or_read'] and fresh['hardware_writes']==0 and fresh['errors']==fresh['received_signals']==[] and fresh['identity_before']==fresh['identity_after'] and fresh['files_before']==fresh['files_after'] and fresh['services']['valid']
assert fresh['identity_before']['boot_id']=='6cf7ee3e-a3c5-4c91-80bc-4ca924818b53';after=pins();assert before==after;save('bound-refs-before.json',before);save('bound-refs-after.json',after)
art=Path('/home/loosqk/robot-checks/h2-exit-observation-20261009-r1/source/scripts/post_e_h2_artifacts.py');assert hashlib.sha256(art.read_bytes()).hexdigest()=='bd2b3f7bb8527cd9ecec73894171638e9cf378656b89e284a585c3baa37b8731'
spec=importlib.util.spec_from_file_location('r13seal_existing_artifacts',art);a=importlib.util.module_from_spec(spec);spec.loader.exec_module(a)
source=base/'position-return-r7/source';expected=json.loads(Path('/home/loosqk/robot-checks/position-return-20261011-r12/native-quality/boot/host_source_final_r12.json').read_text());snapshot=a.snapshot_source(source,list(expected['files']));assert snapshot==expected;save('native-source-seal-window.json',snapshot)
report={'status':'PASS_R13_NONUART_CLEANUP_ORIGINAL_OVERALL_FAIL_PRESERVED','terminal':state,'driver_pid':641514,'original_run_status':run['status'],'original_run_sha256':hashlib.sha256(runraw).hexdigest(),'original_run_bytes':len(runraw),'original_error':run.get('error'),'position':run['position_verification'],'stop':run['stop_verification']['status'],'direct_hardware_children':384,'all_hardware_children_reaped':True,'evidence_errors':[],'manualcut':False,'independent_nonUART_checks':17,'independent_nonUART_children':23,'bound_refs':len(before),'bound_refs_before_after_equal':True,'native_source_files':597,'native_source_sha256':snapshot['source_manifest_sha256'],'collector_direct_children':rows,'new_UART':0,'new_writes':0,'new_GO':False};save('seal-result-r13.json',report)
selected=['head1_p32_trial_r13.py','maintenance_bindings_head_p32_r13.json','full-plan-only.stdout','field_confirmation_r13.json','head1-p32-trial-r13.go.json'];paths=sorted(x for d in [evidence,launch,out] for x in d.rglob('*') if x.is_file());paths += [phase/n for n in selected];assert all(x.is_file() and not x.is_symlink() for x in paths)
archive=phase/'hardware-originals-r13.tar.gz'
with tarfile.open(archive,'x:gz') as t:
 for path in paths:t.add(path,arcname=str(path.relative_to(phase)),recursive=False)
raw=archive.read_bytes();print(json.dumps({'status':'HARDWARE_R13_ARCHIVE_READY','sha256':hashlib.sha256(raw).hexdigest(),'bytes':len(raw),'files':len(paths),'source_bytes':sum(x.stat().st_size for x in paths),'report':report,'base64':base64.b64encode(raw).decode()},ensure_ascii=False),flush=True)
PYREMOTE
