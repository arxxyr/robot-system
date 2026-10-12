#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,io,json,os,subprocess,tarfile,time
from pathlib import Path
PHASE=Path('/home/loosqk/robot-checks/base-zero-r3-launcher-repro-20261012-r1')
PAYLOAD={'old-heredoc': {'sha256': 'feb42e5a606d4429500aeef7d3fba32f04a074fc627cd88de31b1c5a75c9190c', 'bytes': 2516, 'base64': 'IyEvYmluL2Jhc2gKc2V0IC1ldQpleHBvcnQgUFlUSE9ORE9OVFdSSVRFQllURUNPREU9MQpleGVjIC9ob21lL2xvb3Nxay8ubG9jYWwvc2hhcmUvdXYvcHl0aG9uL2NweXRob24tMy4xMi4xMy1saW51eC1hYXJjaDY0LWdudS9iaW4vcHl0aG9uMyAtIDw8J1BZTEFVTkNIJwppbXBvcnQgZXJybm8sanNvbixvcyxzdWJwcm9jZXNzLHN5cyx0aW1lCmZyb20gcGF0aGxpYiBpbXBvcnQgUGF0aApDQVNFPVBhdGgoJy9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvYmFzZS16ZXJvLXIzLWxhdW5jaGVyLXJlcHJvLTIwMjYxMDEyLXIxL29sZC1oZXJlZG9jJykKUFlUSE9OPScvaG9tZS9sb29zcWsvLmxvY2FsL3NoYXJlL3V2L3B5dGhvbi9jcHl0aG9uLTMuMTIuMTMtbGludXgtYWFyY2g2NC1nbnUvYmluL3B5dGhvbjMnCmFzc2VydCBDQVNFLmlzX2Fic29sdXRlKCkgYW5kIENBU0UuaXNfZGlyKCkKY29udGV4dD17J3Njb3BlJzonU09GVFdBUkVfT05MWV9MQVVOQ0hFUl9SRVBSTycsJ2RyaXZlcl9waWQnOm9zLmdldHBpZCgpLCdwcGlkJzpvcy5nZXRwcGlkKCksJ2Nhc2UnOidvbGQtaGVyZWRvYycsJ3N0YXJ0ZWRfbnMnOnRpbWUudGltZV9ucygpLCdjd2QnOm9zLmdldGN3ZCgpLCdlbnYnOntrZXk6b3MuZW52aXJvbi5nZXQoa2V5KSBmb3Iga2V5IGluICgnVE1VWCcsJ1RNVVhfUEFORScsJ1RNVVhfVE1QRElSJywnVEVSTScsJ1BBVEgnKX0sJ2ZkX2JlZm9yZSc6e3N0cihmZCk6eyd0YXJnZXQnOm9zLnJlYWRsaW5rKCcvcHJvYy9zZWxmL2ZkLycrc3RyKGZkKSksJ2lzYXR0eSc6b3MuaXNhdHR5KGZkKSwnbW9kZSc6b3MuZnN0YXQoZmQpLnN0X21vZGV9IGZvciBmZCBpbiAoMCwxLDIpfX0Kd2l0aCAoQ0FTRS8nY29udGV4dC1iZWZvcmUuanNvbicpLm9wZW4oJ3gnKSBhcyBzdHJlYW06anNvbi5kdW1wKGNvbnRleHQsc3RyZWFtLGVuc3VyZV9hc2NpaT1GYWxzZSxpbmRlbnQ9Mik7c3RyZWFtLndyaXRlKCdcbicpCmxvZz1vcy5vcGVuKHN0cihDQVNFLydkcml2ZXIubG9nJyksb3MuT19XUk9OTFl8b3MuT19DUkVBVHxvcy5PX0VYQ0wsMG82MDApCm9zLmR1cDIobG9nLDEpO29zLmR1cDIobG9nLDIpO29zLmNsb3NlKGxvZykKcmVhbF9wb3Blbj1zdWJwcm9jZXNzLlBvcGVuCmNoaWxkcmVuPVtdCmRlZiByZWNvcmRpbmdfcG9wZW4oKmFyZ3MsKiprd2FyZ3MpOgogY2hpbGQ9cmVhbF9wb3BlbigqYXJncywqKmt3YXJncyk7Y2hpbGRyZW4uYXBwZW5kKChjaGlsZCx0aW1lLnRpbWVfbnMoKSkpO3JldHVybiBjaGlsZApzdWJwcm9jZXNzLlBvcGVuPXJlY29yZGluZ19wb3Blbgpyb3c9eydzY29wZSc6J1NPRlRXQVJFX09OTFlfTEFVTkNIRVJfUkVQUk8nLCdkcml2ZXJfcGlkJzpvcy5nZXRwaWQoKSwncGFuZSc6b3MuZW52aXJvblsnVE1VWF9QQU5FJ10sJ2FmdGVyX29wdGlvbl9yZWFjaGVkJzpGYWxzZSwnY2hpbGRyZW4nOltdLCdmZF9hZnRlcic6e3N0cihmZCk6eyd0YXJnZXQnOm9zLnJlYWRsaW5rKCcvcHJvYy9zZWxmL2ZkLycrc3RyKGZkKSksJ2lzYXR0eSc6b3MuaXNhdHR5KGZkKX0gZm9yIGZkIGluICgwLDEsMil9LCdVQVJUJzowLCdHTyc6RmFsc2UsJ2hhcmR3YXJlX2NhbmRpZGF0ZV9zdGFydGVkJzpGYWxzZX0KdHJ5Ogogc3VicHJvY2Vzcy5ydW4oWyd0bXV4Jywnc2V0LW9wdGlvbicsJy1wJywnLXQnLG9zLmVudmlyb25bJ1RNVVhfUEFORSddLCdyZW1haW4tb24tZXhpdCcsJ29uJ10sY2hlY2s9VHJ1ZSkKIHJvd1snYWZ0ZXJfb3B0aW9uX3JlYWNoZWQnXT1UcnVlCmZpbmFsbHk6CiBzdWJwcm9jZXNzLlBvcGVuPXJlYWxfcG9wZW4KIGZvciBjaGlsZCxzdGFydGVkIGluIGNoaWxkcmVuOgogIHJldHVybmVkPWNoaWxkLndhaXQoKTtyZWFwZWQ9RmFsc2UKICB0cnk6b3Mud2FpdHBpZChjaGlsZC5waWQsb3MuV05PSEFORykKICBleGNlcHQgQ2hpbGRQcm9jZXNzRXJyb3IgYXMgZXJyb3I6cmVhcGVkPWVycm9yLmVycm5vPT1lcnJuby5FQ0hJTEQKICByb3dbJ2NoaWxkcmVuJ10uYXBwZW5kKHsnYXJndic6Y2hpbGQuYXJncywncGlkJzpjaGlsZC5waWQsJ3N0YXJ0ZWRfbnMnOnN0YXJ0ZWQsJ3dhaXRfcmV0dXJuY29kZSc6cmV0dXJuZWQsJ2V4aXRfY29kZSc6cmV0dXJuZWQsJ3JlYXBlZCc6cmVhcGVkLCdzaWduYWxzX3NlbnQnOltdfSkKIHJvd1snZmluaXNoZWRfbnMnXT10aW1lLnRpbWVfbnMoKQogd2l0aCAoQ0FTRS8nZHJpdmVyLXJlY2VpcHQuanNvbicpLm9wZW4oJ3gnKSBhcyBzdHJlYW06anNvbi5kdW1wKHJvdyxzdHJlYW0sZW5zdXJlX2FzY2lpPUZhbHNlLGluZGVudD0yKTtzdHJlYW0ud3JpdGUoJ1xuJykKbWFya2VyPSJpbXBvcnQganNvbixvcztmcm9tIHBhdGhsaWIgaW1wb3J0IFBhdGg7UGF0aCgiK3JlcHIoc3RyKENBU0UvJ21hcmtlci5qc29uJykpKyIpLndyaXRlX3RleHQoanNvbi5kdW1wcyh7J3N0YXR1cyc6J1NPRlRXQVJFX09OTFlfUkVBQ0hFRCcsJ3BpZCc6b3MuZ2V0cGlkKCksJ1VBUlQnOjAsJ0dPJzpGYWxzZX0pKydcXG4nKSIKb3MuZXhlY3YoUFlUSE9OLFtQWVRIT04sJy1jJyxtYXJrZXJdKQpQWUxBVU5DSAo='}, 'new-c': {'sha256': 'a267736980f34fd607f7ec4763fc5623ba85e35e2118aaa5e299295840e89f1a', 'bytes': 3022, 'base64': 'IyEvYmluL2Jhc2gKc2V0IC1ldQpleHBvcnQgUFlUSE9ORE9OVFdSSVRFQllURUNPREU9MQpleGVjIC9ob21lL2xvb3Nxay8ubG9jYWwvc2hhcmUvdXYvcHl0aG9uL2NweXRob24tMy4xMi4xMy1saW51eC1hYXJjaDY0LWdudS9iaW4vcHl0aG9uMyAtYyAnaW1wb3J0IGVycm5vLGpzb24sb3Msc3VicHJvY2VzcyxzeXMsdGltZQpmcm9tIHBhdGhsaWIgaW1wb3J0IFBhdGgKQ0FTRT1QYXRoKCciJyInL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9iYXNlLXplcm8tcjMtbGF1bmNoZXItcmVwcm8tMjAyNjEwMTItcjEvbmV3LWMnIiciJykKUFlUSE9OPSciJyInL2hvbWUvbG9vc3FrLy5sb2NhbC9zaGFyZS91di9weXRob24vY3B5dGhvbi0zLjEyLjEzLWxpbnV4LWFhcmNoNjQtZ251L2Jpbi9weXRob24zJyInIicKYXNzZXJ0IENBU0UuaXNfYWJzb2x1dGUoKSBhbmQgQ0FTRS5pc19kaXIoKQpjb250ZXh0PXsnIiciJ3Njb3BlJyInIic6JyInIidTT0ZUV0FSRV9PTkxZX0xBVU5DSEVSX1JFUFJPJyInIicsJyInIidkcml2ZXJfcGlkJyInIic6b3MuZ2V0cGlkKCksJyInIidwcGlkJyInIic6b3MuZ2V0cHBpZCgpLCciJyInY2FzZSciJyInOiciJyInbmV3LWMnIiciJywnIiciJ3N0YXJ0ZWRfbnMnIiciJzp0aW1lLnRpbWVfbnMoKSwnIiciJ2N3ZCciJyInOm9zLmdldGN3ZCgpLCciJyInZW52JyInIic6e2tleTpvcy5lbnZpcm9uLmdldChrZXkpIGZvciBrZXkgaW4gKCciJyInVE1VWCciJyInLCciJyInVE1VWF9QQU5FJyInIicsJyInIidUTVVYX1RNUERJUiciJyInLCciJyInVEVSTSciJyInLCciJyInUEFUSCciJyInKX0sJyInIidmZF9iZWZvcmUnIiciJzp7c3RyKGZkKTp7JyInIid0YXJnZXQnIiciJzpvcy5yZWFkbGluaygnIiciJy9wcm9jL3NlbGYvZmQvJyInIicrc3RyKGZkKSksJyInIidpc2F0dHknIiciJzpvcy5pc2F0dHkoZmQpLCciJyInbW9kZSciJyInOm9zLmZzdGF0KGZkKS5zdF9tb2RlfSBmb3IgZmQgaW4gKDAsMSwyKX19CndpdGggKENBU0UvJyInIidjb250ZXh0LWJlZm9yZS5qc29uJyInIicpLm9wZW4oJyInIid4JyInIicpIGFzIHN0cmVhbTpqc29uLmR1bXAoY29udGV4dCxzdHJlYW0sZW5zdXJlX2FzY2lpPUZhbHNlLGluZGVudD0yKTtzdHJlYW0ud3JpdGUoJyInIidcbiciJyInKQpsb2c9b3Mub3BlbihzdHIoQ0FTRS8nIiciJ2RyaXZlci5sb2cnIiciJyksb3MuT19XUk9OTFl8b3MuT19DUkVBVHxvcy5PX0VYQ0wsMG82MDApCm9zLmR1cDIobG9nLDEpO29zLmR1cDIobG9nLDIpO29zLmNsb3NlKGxvZykKcmVhbF9wb3Blbj1zdWJwcm9jZXNzLlBvcGVuCmNoaWxkcmVuPVtdCmRlZiByZWNvcmRpbmdfcG9wZW4oKmFyZ3MsKiprd2FyZ3MpOgogY2hpbGQ9cmVhbF9wb3BlbigqYXJncywqKmt3YXJncyk7Y2hpbGRyZW4uYXBwZW5kKChjaGlsZCx0aW1lLnRpbWVfbnMoKSkpO3JldHVybiBjaGlsZApzdWJwcm9jZXNzLlBvcGVuPXJlY29yZGluZ19wb3Blbgpyb3c9eyciJyInc2NvcGUnIiciJzonIiciJ1NPRlRXQVJFX09OTFlfTEFVTkNIRVJfUkVQUk8nIiciJywnIiciJ2RyaXZlcl9waWQnIiciJzpvcy5nZXRwaWQoKSwnIiciJ3BhbmUnIiciJzpvcy5lbnZpcm9uWyciJyInVE1VWF9QQU5FJyInIiddLCciJyInYWZ0ZXJfb3B0aW9uX3JlYWNoZWQnIiciJzpGYWxzZSwnIiciJ2NoaWxkcmVuJyInIic6W10sJyInIidmZF9hZnRlciciJyInOntzdHIoZmQpOnsnIiciJ3RhcmdldCciJyInOm9zLnJlYWRsaW5rKCciJyInL3Byb2Mvc2VsZi9mZC8nIiciJytzdHIoZmQpKSwnIiciJ2lzYXR0eSciJyInOm9zLmlzYXR0eShmZCl9IGZvciBmZCBpbiAoMCwxLDIpfSwnIiciJ1VBUlQnIiciJzowLCciJyInR08nIiciJzpGYWxzZSwnIiciJ2hhcmR3YXJlX2NhbmRpZGF0ZV9zdGFydGVkJyInIic6RmFsc2V9CnRyeToKIHN1YnByb2Nlc3MucnVuKFsnIiciJ3RtdXgnIiciJywnIiciJ3NldC1vcHRpb24nIiciJywnIiciJy1wJyInIicsJyInIictdCciJyInLG9zLmVudmlyb25bJyInIidUTVVYX1BBTkUnIiciJ10sJyInIidyZW1haW4tb24tZXhpdCciJyInLCciJyInb24nIiciJ10sY2hlY2s9VHJ1ZSkKIHJvd1snIiciJ2FmdGVyX29wdGlvbl9yZWFjaGVkJyInIiddPVRydWUKZmluYWxseToKIHN1YnByb2Nlc3MuUG9wZW49cmVhbF9wb3BlbgogZm9yIGNoaWxkLHN0YXJ0ZWQgaW4gY2hpbGRyZW46CiAgcmV0dXJuZWQ9Y2hpbGQud2FpdCgpO3JlYXBlZD1GYWxzZQogIHRyeTpvcy53YWl0cGlkKGNoaWxkLnBpZCxvcy5XTk9IQU5HKQogIGV4Y2VwdCBDaGlsZFByb2Nlc3NFcnJvciBhcyBlcnJvcjpyZWFwZWQ9ZXJyb3IuZXJybm89PWVycm5vLkVDSElMRAogIHJvd1snIiciJ2NoaWxkcmVuJyInIiddLmFwcGVuZCh7JyInIidhcmd2JyInIic6Y2hpbGQuYXJncywnIiciJ3BpZCciJyInOmNoaWxkLnBpZCwnIiciJ3N0YXJ0ZWRfbnMnIiciJzpzdGFydGVkLCciJyInd2FpdF9yZXR1cm5jb2RlJyInIic6cmV0dXJuZWQsJyInIidleGl0X2NvZGUnIiciJzpyZXR1cm5lZCwnIiciJ3JlYXBlZCciJyInOnJlYXBlZCwnIiciJ3NpZ25hbHNfc2VudCciJyInOltdfSkKIHJvd1snIiciJ2ZpbmlzaGVkX25zJyInIiddPXRpbWUudGltZV9ucygpCiB3aXRoIChDQVNFLyciJyInZHJpdmVyLXJlY2VpcHQuanNvbiciJyInKS5vcGVuKCciJyIneCciJyInKSBhcyBzdHJlYW06anNvbi5kdW1wKHJvdyxzdHJlYW0sZW5zdXJlX2FzY2lpPUZhbHNlLGluZGVudD0yKTtzdHJlYW0ud3JpdGUoJyInIidcbiciJyInKQptYXJrZXI9ImltcG9ydCBqc29uLG9zO2Zyb20gcGF0aGxpYiBpbXBvcnQgUGF0aDtQYXRoKCIrcmVwcihzdHIoQ0FTRS8nIiciJ21hcmtlci5qc29uJyInIicpKSsiKS53cml0ZV90ZXh0KGpzb24uZHVtcHMoeyciJyInc3RhdHVzJyInIic6JyInIidTT0ZUV0FSRV9PTkxZX1JFQUNIRUQnIiciJywnIiciJ3BpZCciJyInOm9zLmdldHBpZCgpLCciJyInVUFSVCciJyInOjAsJyInIidHTyciJyInOkZhbHNlfSkrJyInIidcXG4nIiciJykiCm9zLmV4ZWN2KFBZVEhPTixbUFlUSE9OLCciJyInLWMnIiciJyxtYXJrZXJdKQonCg=='}}
PHASE.mkdir(mode=0o700,exist_ok=False)
all_receipts=[]
def command(name,argv):
 started=time.time_ns();child=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
 a,b=child.communicate();code=child.wait()
 row={'name':name,'argv':argv,'pid':child.pid,'started_ns':started,'finished_ns':time.time_ns(),'exit_code':code,'wait_returncode':code,'reaped':True,'signals_sent':[],'stdout':a.decode(),'stderr':b.decode()};all_receipts.append(row);return row
def save(path,value):
 with path.open('x') as stream:json.dump(value,stream,ensure_ascii=False,indent=2);stream.write('\n')
results=[]
for name in ('old-heredoc','new-c'):
 case=PHASE/name;case.mkdir(mode=0o700)
 source=PAYLOAD[name];raw=base64.b64decode(source['base64'],validate=True)
 assert len(raw)==source['bytes'] and hashlib.sha256(raw).hexdigest()==source['sha256']
 launcher=case/'launcher.sh'
 with launcher.open('xb') as stream:stream.write(raw)
 session='base-zero-launcher-'+name+'-20261012-r1'
 start=command(name+':start',['tmux','new-session','-d','-s',session,'-P','-F','#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}','bash '+str(launcher)])
 result={'case':name,'first_start':start,'scope':'SOFTWARE_ONLY','status':'FAIL_FIRST_START','terminal':None,'driver_exit':'NOT_OBSERVED','reclaimed':False,'UART':0,'GO':False,'signals_sent':[]}
 if start['exit_code']==0 and not start['stderr']:
  own_session,window,pane,pid=start['stdout'].strip().split('|');pid=int(pid);assert own_session==session
  result.update(pane=pane,driver_pid=pid,window=window,status='NATURAL_TERMINAL_NOT_OBSERVED')
  deadline=time.monotonic()+10
  while time.monotonic()<deadline:
   observation=command(name+':observe',['tmux','display-message','-p','-t',pane,'#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}|#{pane_dead_status}'])
   if observation['exit_code']!=0 or observation['stderr']:
    result.update(status='FIRST_OBSERVATION_FAIL_PRESERVED',first_observation_failure=observation);break
   fields=observation['stdout'].strip().split('|');assert fields[:4]==[session,window,pane,str(pid)]
   if fields[-2]=='1':
    result.update(terminal=observation['stdout'].strip(),driver_exit=int(fields[-1]),status='OBSERVED_NATURAL_DEAD')
    reclaimed=command(name+':reclaim-only-owned-natural-dead',['tmux','kill-pane','-t',pane]);result.update(reclaim=reclaimed,reclaimed=reclaimed['exit_code']==0);break
   time.sleep(0.05)
  result['driver_proc_exists_after_observation']=Path('/proc/'+str(pid)).exists()
 for file in ('context-before.json','driver-receipt.json','marker.json'):
  path=case/file
  if path.exists():
   raw=path.read_bytes();value=json.loads(raw);result[file]={'sha256':hashlib.sha256(raw).hexdigest(),'bytes':len(raw),'value':value}
 if 'marker.json' in result:
  marker=result['marker.json']['value'];assert marker['status']=='SOFTWARE_ONLY_REACHED' and marker['pid']==result['driver_pid']
  receipt=result['driver-receipt.json']['value'];assert receipt['driver_pid']==result['driver_pid'] and receipt['pane']==result['pane']
  calls=receipt['children'];assert len(calls)==1 and calls[0]['argv']==['tmux','set-option','-p','-t',result['pane'],'remain-on-exit','on']
  assert calls[0]['exit_code']==0 and calls[0]['reaped'] is True and not calls[0]['signals_sent'] and receipt['after_option_reached'] is True
  result['same_driver_pid_marker_verified']=True
 if (case/'driver.log').exists():
  raw=(case/'driver.log').read_bytes();result['driver_log']={'sha256':hashlib.sha256(raw).hexdigest(),'bytes':len(raw),'text':raw.decode()}
 save(case/'case-result.json',result);results.append(result)
result={'schema':'BASE_ZERO_R3_LAUNCHER_REPRO_SOFTWARE_V1','status':'COLLECTED_TWO_FIRST_SOFTWARE_CASE_RESULTS','cases':results,'direct_children':all_receipts,'UART':0,'GO':False,'hardware_candidate_started':False,'limitations':['collection exit0不等于旧case测试PASS','每case只执行一次set-option，无show-options前置调用，无retry或signal','无自然dead回执时保留UNKNOWN，不把proc不存在当wait/reap','两case相同插桩会改变少量启动时序，根因结论仍需原件判断']}
save(PHASE/'result.json',result)
buffer=io.BytesIO()
with tarfile.open(fileobj=buffer,mode='w:gz') as tar:
 for path in sorted(PHASE.rglob('*')):
  assert not path.is_symlink()
  if path.is_file():tar.add(path,arcname=str(path.relative_to(PHASE)),recursive=False)
raw=buffer.getvalue()
with tarfile.open(fileobj=io.BytesIO(raw)) as tar:count=sum(member.isfile() for member in tar)
print(json.dumps({'status':result['status'],'result':result,'archive_sha256':hashlib.sha256(raw).hexdigest(),'archive_bytes':len(raw),'archive_files':count,'archive_base64':base64.b64encode(raw).decode()},ensure_ascii=False),flush=True)
PYREMOTE
