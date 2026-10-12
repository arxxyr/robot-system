#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,io,json,os,subprocess,tarfile,time
from pathlib import Path
PHASE=Path('/home/loosqk/robot-checks/base-zero-r3-launcher-repro-20261012-r1')
NEW_C_PAYLOAD={'sha256': 'a267736980f34fd607f7ec4763fc5623ba85e35e2118aaa5e299295840e89f1a', 'bytes': 3022, 'base64': 'IyEvYmluL2Jhc2gKc2V0IC1ldQpleHBvcnQgUFlUSE9ORE9OVFdSSVRFQllURUNPREU9MQpleGVjIC9ob21lL2xvb3Nxay8ubG9jYWwvc2hhcmUvdXYvcHl0aG9uL2NweXRob24tMy4xMi4xMy1saW51eC1hYXJjaDY0LWdudS9iaW4vcHl0aG9uMyAtYyAnaW1wb3J0IGVycm5vLGpzb24sb3Msc3VicHJvY2VzcyxzeXMsdGltZQpmcm9tIHBhdGhsaWIgaW1wb3J0IFBhdGgKQ0FTRT1QYXRoKCciJyInL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9iYXNlLXplcm8tcjMtbGF1bmNoZXItcmVwcm8tMjAyNjEwMTItcjEvbmV3LWMnIiciJykKUFlUSE9OPSciJyInL2hvbWUvbG9vc3FrLy5sb2NhbC9zaGFyZS91di9weXRob24vY3B5dGhvbi0zLjEyLjEzLWxpbnV4LWFhcmNoNjQtZ251L2Jpbi9weXRob24zJyInIicKYXNzZXJ0IENBU0UuaXNfYWJzb2x1dGUoKSBhbmQgQ0FTRS5pc19kaXIoKQpjb250ZXh0PXsnIiciJ3Njb3BlJyInIic6JyInIidTT0ZUV0FSRV9PTkxZX0xBVU5DSEVSX1JFUFJPJyInIicsJyInIidkcml2ZXJfcGlkJyInIic6b3MuZ2V0cGlkKCksJyInIidwcGlkJyInIic6b3MuZ2V0cHBpZCgpLCciJyInY2FzZSciJyInOiciJyInbmV3LWMnIiciJywnIiciJ3N0YXJ0ZWRfbnMnIiciJzp0aW1lLnRpbWVfbnMoKSwnIiciJ2N3ZCciJyInOm9zLmdldGN3ZCgpLCciJyInZW52JyInIic6e2tleTpvcy5lbnZpcm9uLmdldChrZXkpIGZvciBrZXkgaW4gKCciJyInVE1VWCciJyInLCciJyInVE1VWF9QQU5FJyInIicsJyInIidUTVVYX1RNUERJUiciJyInLCciJyInVEVSTSciJyInLCciJyInUEFUSCciJyInKX0sJyInIidmZF9iZWZvcmUnIiciJzp7c3RyKGZkKTp7JyInIid0YXJnZXQnIiciJzpvcy5yZWFkbGluaygnIiciJy9wcm9jL3NlbGYvZmQvJyInIicrc3RyKGZkKSksJyInIidpc2F0dHknIiciJzpvcy5pc2F0dHkoZmQpLCciJyInbW9kZSciJyInOm9zLmZzdGF0KGZkKS5zdF9tb2RlfSBmb3IgZmQgaW4gKDAsMSwyKX19CndpdGggKENBU0UvJyInIidjb250ZXh0LWJlZm9yZS5qc29uJyInIicpLm9wZW4oJyInIid4JyInIicpIGFzIHN0cmVhbTpqc29uLmR1bXAoY29udGV4dCxzdHJlYW0sZW5zdXJlX2FzY2lpPUZhbHNlLGluZGVudD0yKTtzdHJlYW0ud3JpdGUoJyInIidcbiciJyInKQpsb2c9b3Mub3BlbihzdHIoQ0FTRS8nIiciJ2RyaXZlci5sb2cnIiciJyksb3MuT19XUk9OTFl8b3MuT19DUkVBVHxvcy5PX0VYQ0wsMG82MDApCm9zLmR1cDIobG9nLDEpO29zLmR1cDIobG9nLDIpO29zLmNsb3NlKGxvZykKcmVhbF9wb3Blbj1zdWJwcm9jZXNzLlBvcGVuCmNoaWxkcmVuPVtdCmRlZiByZWNvcmRpbmdfcG9wZW4oKmFyZ3MsKiprd2FyZ3MpOgogY2hpbGQ9cmVhbF9wb3BlbigqYXJncywqKmt3YXJncyk7Y2hpbGRyZW4uYXBwZW5kKChjaGlsZCx0aW1lLnRpbWVfbnMoKSkpO3JldHVybiBjaGlsZApzdWJwcm9jZXNzLlBvcGVuPXJlY29yZGluZ19wb3Blbgpyb3c9eyciJyInc2NvcGUnIiciJzonIiciJ1NPRlRXQVJFX09OTFlfTEFVTkNIRVJfUkVQUk8nIiciJywnIiciJ2RyaXZlcl9waWQnIiciJzpvcy5nZXRwaWQoKSwnIiciJ3BhbmUnIiciJzpvcy5lbnZpcm9uWyciJyInVE1VWF9QQU5FJyInIiddLCciJyInYWZ0ZXJfb3B0aW9uX3JlYWNoZWQnIiciJzpGYWxzZSwnIiciJ2NoaWxkcmVuJyInIic6W10sJyInIidmZF9hZnRlciciJyInOntzdHIoZmQpOnsnIiciJ3RhcmdldCciJyInOm9zLnJlYWRsaW5rKCciJyInL3Byb2Mvc2VsZi9mZC8nIiciJytzdHIoZmQpKSwnIiciJ2lzYXR0eSciJyInOm9zLmlzYXR0eShmZCl9IGZvciBmZCBpbiAoMCwxLDIpfSwnIiciJ1VBUlQnIiciJzowLCciJyInR08nIiciJzpGYWxzZSwnIiciJ2hhcmR3YXJlX2NhbmRpZGF0ZV9zdGFydGVkJyInIic6RmFsc2V9CnRyeToKIHN1YnByb2Nlc3MucnVuKFsnIiciJ3RtdXgnIiciJywnIiciJ3NldC1vcHRpb24nIiciJywnIiciJy1wJyInIicsJyInIictdCciJyInLG9zLmVudmlyb25bJyInIidUTVVYX1BBTkUnIiciJ10sJyInIidyZW1haW4tb24tZXhpdCciJyInLCciJyInb24nIiciJ10sY2hlY2s9VHJ1ZSkKIHJvd1snIiciJ2FmdGVyX29wdGlvbl9yZWFjaGVkJyInIiddPVRydWUKZmluYWxseToKIHN1YnByb2Nlc3MuUG9wZW49cmVhbF9wb3BlbgogZm9yIGNoaWxkLHN0YXJ0ZWQgaW4gY2hpbGRyZW46CiAgcmV0dXJuZWQ9Y2hpbGQud2FpdCgpO3JlYXBlZD1GYWxzZQogIHRyeTpvcy53YWl0cGlkKGNoaWxkLnBpZCxvcy5XTk9IQU5HKQogIGV4Y2VwdCBDaGlsZFByb2Nlc3NFcnJvciBhcyBlcnJvcjpyZWFwZWQ9ZXJyb3IuZXJybm89PWVycm5vLkVDSElMRAogIHJvd1snIiciJ2NoaWxkcmVuJyInIiddLmFwcGVuZCh7JyInIidhcmd2JyInIic6Y2hpbGQuYXJncywnIiciJ3BpZCciJyInOmNoaWxkLnBpZCwnIiciJ3N0YXJ0ZWRfbnMnIiciJzpzdGFydGVkLCciJyInd2FpdF9yZXR1cm5jb2RlJyInIic6cmV0dXJuZWQsJyInIidleGl0X2NvZGUnIiciJzpyZXR1cm5lZCwnIiciJ3JlYXBlZCciJyInOnJlYXBlZCwnIiciJ3NpZ25hbHNfc2VudCciJyInOltdfSkKIHJvd1snIiciJ2ZpbmlzaGVkX25zJyInIiddPXRpbWUudGltZV9ucygpCiB3aXRoIChDQVNFLyciJyInZHJpdmVyLXJlY2VpcHQuanNvbiciJyInKS5vcGVuKCciJyIneCciJyInKSBhcyBzdHJlYW06anNvbi5kdW1wKHJvdyxzdHJlYW0sZW5zdXJlX2FzY2lpPUZhbHNlLGluZGVudD0yKTtzdHJlYW0ud3JpdGUoJyInIidcbiciJyInKQptYXJrZXI9ImltcG9ydCBqc29uLG9zO2Zyb20gcGF0aGxpYiBpbXBvcnQgUGF0aDtQYXRoKCIrcmVwcihzdHIoQ0FTRS8nIiciJ21hcmtlci5qc29uJyInIicpKSsiKS53cml0ZV90ZXh0KGpzb24uZHVtcHMoeyciJyInc3RhdHVzJyInIic6JyInIidTT0ZUV0FSRV9PTkxZX1JFQUNIRUQnIiciJywnIiciJ3BpZCciJyInOm9zLmdldHBpZCgpLCciJyInVUFSVCciJyInOjAsJyInIidHTyciJyInOkZhbHNlfSkrJyInIidcXG4nIiciJykiCm9zLmV4ZWN2KFBZVEhPTixbUFlUSE9OLCciJyInLWMnIiciJyxtYXJrZXJdKQonCg=='}
COLLECTOR=PHASE/'resume-collector-r1'
COLLECTOR.mkdir(mode=0o700,exist_ok=False)
COMMANDS=COLLECTOR/'commands';COMMANDS.mkdir(mode=0o700)
receipts=[]
def save(path,value):
 with path.open('x') as stream:
  json.dump(value,stream,ensure_ascii=False,indent=2);stream.write('\n');stream.flush();os.fsync(stream.fileno())
def command(name,argv):
 started=time.time_ns();child=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
 a,b=child.communicate();code=child.wait()
 row={'name':name,'argv':argv,'pid':child.pid,'started_ns':started,'finished_ns':time.time_ns(),'exit_code':code,'wait_returncode':code,'reaped':True,'signals_sent':[],'stdout':a.decode(),'stderr':b.decode()}
 path=COMMANDS/(str(len(receipts)+1).zfill(3)+'-'+name+'.json');save(path,row)
 row['persisted_receipt']=str(path);receipts.append(row);return row
def snapshot(case):
 rows=[]
 for name in ('launcher.sh','context-before.json','driver.log','driver-receipt.json','marker.json','case-result.json'):
  path=case/name;row={'path':str(path),'exists':path.exists(),'is_symlink':path.is_symlink()}
  if row['exists'] and path.is_file() and not row['is_symlink']:
   raw=path.read_bytes();row.update(sha256=hashlib.sha256(raw).hexdigest(),bytes=len(raw),base64=base64.b64encode(raw).decode())
   if name.endswith('.json'):
    try:row['value']=json.loads(raw)
    except ValueError as error:row['json_error']=repr(error)
  rows.append(row)
 return rows
def parsed(rows,name):return next((row.get('value') for row in rows if Path(row['path']).name==name),None)
old_rows=snapshot(PHASE/'old-heredoc');save(COLLECTOR/'old-existing-files.json',old_rows)
context=parsed(old_rows,'context-before.json');old_pid=context.get('driver_pid') if isinstance(context,dict) else None
old_metadata={'observed_ns':time.time_ns(),'old_driver_pid_from_saved_context':old_pid,'old_driver_proc_exists':Path('/proc/'+str(old_pid)).exists() if isinstance(old_pid,int) else None,'boot_id':Path('/proc/sys/kernel/random/boot_id').read_text().strip(),'collector_env':{key:os.environ.get(key) for key in ('TMUX','TMUX_PANE','TMUX_TMPDIR','TERM','PATH')},'original_old_start_observe_receipts':'NOT_PERSISTED_AND_NOT_RECONSTRUCTED','original_collector_exit':1,'old_driver_wait_reap':'NOT_OBSERVED'}
old_metadata['current_outer_tmux_list']=command('old-readonly-list-panes',['tmux','list-panes','-a','-F','#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}|#{pane_dead_status}'])
save(COLLECTOR/'old-readonly-current-metadata.json',old_metadata)
case=PHASE/'new-c';session='base-zero-launcher-new-c-20261012-r1'
new={'case':'new-c','scope':'SOFTWARE_ONLY_FIRST_EXECUTION','status':'REFUSED_IF_ALREADY_EXISTS','terminal':None,'driver_exit':'NOT_OBSERVED','reclaimed':False,'signals_sent':[],'UART':0,'GO':False}
guard={'new_c_case_exists':case.exists(),'new_c_case_symlink':case.is_symlink(),'session':session,'payload_sha256':NEW_C_PAYLOAD['sha256'],'observed_ns':time.time_ns()};save(COLLECTOR/'new-c-once-guard.json',guard)
if not guard['new_c_case_exists'] and not guard['new_c_case_symlink']:
 case.mkdir(mode=0o700)
 raw=base64.b64decode(NEW_C_PAYLOAD['base64'],validate=True)
 assert len(raw)==NEW_C_PAYLOAD['bytes'] and hashlib.sha256(raw).hexdigest()==NEW_C_PAYLOAD['sha256']
 launcher=case/'launcher.sh'
 with launcher.open('xb') as stream:stream.write(raw);stream.flush();os.fsync(stream.fileno())
 start=command('new-c-first-start',['tmux','new-session','-d','-s',session,'-P','-F','#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}','bash '+str(launcher)]);new.update(first_start=start,status='FAIL_FIRST_START')
 fields=start['stdout'].strip().split('|');identity_ok=start['exit_code']==0 and not start['stderr'] and len(fields)==4 and fields[0]==session and fields[1].startswith('@') and fields[2].startswith('%') and fields[3].isdigit()
 if not identity_ok:new.update(status='FIRST_START_IDENTITY_FAIL_PRESERVED',first_start_identity_fields=fields)
 else:
  _,window,pane,pid=fields;pid=int(pid);new.update(window=window,pane=pane,driver_pid=pid,status='NATURAL_TERMINAL_NOT_OBSERVED')
  deadline=time.monotonic()+10
  while time.monotonic()<deadline:
   observation=command('new-c-observe',['tmux','display-message','-p','-t',pane,'#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}|#{pane_dead_status}']);fields=observation['stdout'].strip().split('|')
   match=observation['exit_code']==0 and not observation['stderr'] and len(fields)==6 and fields[:4]==[session,window,pane,str(pid)] and fields[4] in ('0','1')
   if not match:
    new.update(status='FIRST_OBSERVATION_FAIL_PRESERVED',first_observation_failure=observation,first_observation_fields=fields);break
   if fields[4]=='1':
    try:code=int(fields[5])
    except ValueError:
     new.update(status='FIRST_OBSERVATION_FAIL_PRESERVED',first_observation_failure=observation,first_observation_fields=fields);break
    new.update(terminal=observation['stdout'].strip(),driver_exit=code,status='OBSERVED_NATURAL_DEAD')
    reclaim=command('new-c-reclaim-only-owned-natural-dead',['tmux','kill-pane','-t',pane]);new.update(reclaim=reclaim,reclaimed=reclaim['exit_code']==0);break
   time.sleep(0.05)
  new['driver_proc_exists_after_observation']=Path('/proc/'+str(pid)).exists()
new_rows=snapshot(case);save(COLLECTOR/'new-c-existing-files.json',new_rows)
marker=parsed(new_rows,'marker.json');driver=parsed(new_rows,'driver-receipt.json')
checks={'natural_dead_exit0':new['driver_exit']==0,'same_driver_pid_marker':isinstance(marker,dict) and marker.get('status')=='SOFTWARE_ONLY_REACHED' and marker.get('pid')==new.get('driver_pid'),'driver_receipt_matches':isinstance(driver,dict) and driver.get('driver_pid')==new.get('driver_pid') and driver.get('pane')==new.get('pane'),'single_option_exit0_reaped':False}
if checks['driver_receipt_matches']:
 calls=driver.get('children',[]);checks['single_option_exit0_reaped']=len(calls)==1 and calls[0].get('argv')==['tmux','set-option','-p','-t',new['pane'],'remain-on-exit','on'] and calls[0].get('exit_code')==0 and calls[0].get('reaped') is True and not calls[0].get('signals_sent') and driver.get('after_option_reached') is True
new['software_checks']=checks
if all(checks.values()) and new['reclaimed']:new['software_result']='PASS_FIRST_NEW_C_SOFTWARE_MARKER'
else:new['software_result']='FAIL_OR_NOT_OBSERVED_FIRST_RESULT_PRESERVED'
save(COLLECTOR/'new-c-first-result.json',new)
result={'schema':'BASE_ZERO_R3_LAUNCHER_REPRO_RESUME_V1','status':'COLLECTED_OLD_READONLY_NEW_C_FIRST_RESULT','old_existing_files':old_rows,'old_readonly_current_metadata':old_metadata,'new_c_first_result':new,'direct_children':receipts,'old_case_restarted':False,'new_c_option_retried':False,'UART':0,'GO':False,'hardware_candidate_started':False,'limitations':['原collector line31身份断言FAIL保留；旧start/observe回执未持久化，不补造','本次old只读快照不补原driver wait/reap、终态或因果','PTY假设需两次原件事实独立分析；observer控制FAIL不能单独证明根因','collection退出0不等于oldcase或new-c软件测试PASS；new-c失败不自动重试']}
save(COLLECTOR/'result.json',result)
buffer=io.BytesIO()
with tarfile.open(fileobj=buffer,mode='w:gz') as tar:
 for path in sorted(PHASE.rglob('*')):
  assert not path.is_symlink()
  if path.is_file():tar.add(path,arcname=str(path.relative_to(PHASE)),recursive=False)
raw=buffer.getvalue()
with tarfile.open(fileobj=io.BytesIO(raw)) as tar:count=sum(member.isfile() for member in tar)
print(json.dumps({'status':result['status'],'result':result,'archive_sha256':hashlib.sha256(raw).hexdigest(),'archive_bytes':len(raw),'archive_files':count,'archive_base64':base64.b64encode(raw).decode()},ensure_ascii=False),flush=True)
PYREMOTE
