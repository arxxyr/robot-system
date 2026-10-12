#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,importlib.util,json,os,subprocess,time
from pathlib import Path
FIELD_STAMP=None  # 仅root本次新现场事实可替换；草稿不能执行。
FIELD_EXPECTED=None  # root固定新现场回执(sha256,bytes)。
GO_EXPECTED=None  # root固定本次新GO(sha256,bytes)，不能复用任何旧GO。
BINDING_EXPECTED=None  # native准备后由root固定本轮binding(sha256,bytes)。
PLAN_EXPECTED=None  # native准备后由root固定完整PLAN stdout(sha256,bytes)。
NATIVE_PREPARATION_EXPECTED=None  # root固定本轮原生软件prepare-result(sha256,bytes)。
PLAN_CANONICAL_SHA=None  # native准备后由root固定本轮canonical PLAN。
assert type(FIELD_STAMP) is int and FIELD_STAMP>0 and FIELD_EXPECTED is not None and GO_EXPECTED is not None, '爸爸，BASE_ZERO_R1启动草稿未获root本次新GO，零启动。' 
assert BINDING_EXPECTED is not None and PLAN_EXPECTED is not None and NATIVE_PREPARATION_EXPECTED is not None and type(PLAN_CANONICAL_SHA) is str and len(PLAN_CANONICAL_SHA)==64, '爸爸，BASE_ZERO_R1原生binding/PLAN未固定，零I/O拒绝。'
base=Path('/home/loosqk/robot-checks/position-return-20261010-r1');phase=base/'base-zero-diagnostic-r1'
expected={'base_zero_diagnostic_r1.py': ('f59455a6b25f832efd5b725224d8f16229b421f28c1556cbe4416ed24ac6f314', 19556), 'maintenance_bindings_base_zero_diagnostic_r1.json': BINDING_EXPECTED, 'full-plan-only.stdout': PLAN_EXPECTED, 'prepare-result-base-zero-r1.json': NATIVE_PREPARATION_EXPECTED, 'field_confirmation_base_zero_r1.json': FIELD_EXPECTED, 'base-zero-diagnostic-r1.go.json': GO_EXPECTED}
for name,(sha,size) in expected.items():
 path=phase/name;raw=path.read_bytes();assert path.is_file() and not path.is_symlink() and hashlib.sha256(raw).hexdigest()==sha and len(raw)==size
script=base/'software-facts-r1/scripts/run_hardware_maintenance.py'
assert hashlib.sha256(script.read_bytes()).hexdigest()=='d97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41'
spec=importlib.util.spec_from_file_location('base_zero_start_frozen_d97',script);m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
bindings=m.strict_json((phase/'maintenance_bindings_base_zero_diagnostic_r1.json').read_text())
for ref in bindings['refs']:m.verify_ref(ref)
for side,files in bindings['protection'].items():
 for name,ref in files.items():m.verify_protection_file(side,name,ref)
assert Path('/proc/sys/kernel/random/boot_id').read_text().strip()==bindings['host']['boot_id']
for side,ref in bindings['usb'].items():
 device=Path(ref['device']);assert str(device.resolve())==ref['resolved'] and device.stat().st_rdev==ref['rdev'] and device.stat().st_ino==ref['device_inode'] and device.lstat().st_ino==ref['link_inode']
candidate=phase/'base_zero_diagnostic_r1.py';helper=phase/'helpers/raw_right9_from_r14_r1.py'
for name,path in (('base_zero_start_fixed_candidate',candidate),('base_zero_start_fixed_raw_helper',helper)):
 spec=importlib.util.spec_from_file_location(name,path);value=importlib.util.module_from_spec(spec);spec.loader.exec_module(value)
 if path==candidate:c=value
 else:h=value
c.install(m,h)
preparation=m.strict_json((phase/'prepare-result-base-zero-r1.json').read_text())
assert preparation['status']=='PASS_BASE_ZERO_R1_NATIVE_SOFTWARE_PREPARATION_ONLY' and preparation['uart_operations']==preparation['motor_actions']==0 and preparation['go_created'] is False and preparation['field_confirmation_created'] is False
plan=m.strict_json((phase/'full-plan-only.stdout').read_text());assert plan['plan_sha256']==PLAN_CANONICAL_SHA and m.digest(plan['plan'])==plan['plan_sha256']
go_path=phase/'base-zero-diagnostic-r1.go.json';go_raw=go_path.read_bytes();assert go_path.is_file() and not go_path.is_symlink()
go=m.strict_json(go_raw.decode());now=time.time_ns();m.validate_go(go,plan['plan'],now)
assert 0<go['expires_at_ns']-go['issued_at_ns']<=600_000_000_000 and 0<=now-FIELD_STAMP<600_000_000_000
assert all(go['physical'][side]['confirmed_at_ns']==FIELD_STAMP for side in ('left','right'))
output=phase/'base-zero-diagnostic-r1';record=phase/'base-zero-diagnostic-launch-r1';assert not output.exists() and not record.exists()

record.mkdir(mode=0o700)
def write_once(name,raw):
 with (record/name).open('xb') as stream:stream.write(raw);stream.flush();os.fsync(stream.fileno())
def start_tmux(name,session,launcher):
 path=record/name;write_once(name,launcher)
 argv=['tmux','new-session','-d','-s',session,'-P','-F','#{session_name} #{window_id} #{pane_id} #{pane_pid}','bash '+str(path)]
 started=time.time_ns();child=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE);a,b=child.communicate();code=child.wait()
 row={'argv':argv,'pid':child.pid,'started_ns':started,'finished_ns':time.time_ns(),'exit_code':code,'wait_returncode':code,'reaped':True,'stdout':a.decode(),'stderr':b.decode(),'signals_sent':[]}
 write_once(name+'.tmux-receipt.json',(json.dumps(row,ensure_ascii=False,indent=2)+'\n').encode());assert code==0 and b==b''
 return a.decode().strip(),row
probe_argv=['bash',str(base/'normal-read-r4/nonuart_probe_r4.sh')]
started=time.time_ns();child=subprocess.Popen(probe_argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE);probe_out,probe_err=child.communicate();code=child.wait()
write_once('nonuart-before-motor.stdout',probe_out);write_once('nonuart-before-motor.stderr',probe_err)
probe_receipt={'argv':probe_argv,'pid':child.pid,'started_ns':started,'finished_ns':time.time_ns(),'exit_code':code,'wait_returncode':code,'reaped':True,'signals_sent':[]}
write_once('nonuart-before-motor.receipt.json',(json.dumps(probe_receipt,ensure_ascii=False,indent=2)+'\n').encode())
probe=m.strict_json(probe_out.decode());assert code==0 and probe_err==b'' and probe['status']=='PASS_NON_UART_CURRENT_FACTS'
assert len(probe['checks'])==17 and all(v['status']=='PASS' for v in probe['checks'])
# 前置非UART门消耗的时间不能续现场/GO期限；再次真实校验后才允许唯一tmux。
for ref in bindings['refs']:m.verify_ref(ref)
for side,files in bindings['protection'].items():
 for name,ref in files.items():m.verify_protection_file(side,name,ref)
assert Path('/proc/sys/kernel/random/boot_id').read_text().strip()==bindings['host']['boot_id']
for side,ref in bindings['usb'].items():
 device=Path(ref['device']);assert str(device.resolve())==ref['resolved'] and device.stat().st_rdev==ref['rdev'] and device.stat().st_ino==ref['device_inode'] and device.lstat().st_ino==ref['link_inode']
now=time.time_ns();m.validate_go(go,plan['plan'],now)
assert 0<FIELD_STAMP<=now and now-FIELD_STAMP<600_000_000_000
assert not output.exists()
launcher=base64.b64decode('IyEvYmluL2Jhc2gKIyDniLjniLjvvIzni6znq4tsYXVuY2hlcuS5n+W/hemhu+eUseW3suaguHN0YXJ0ZXLloavlhajpg6jmnKzova5waW5z77yb57qv6I2J56i/6Zu2SS9P5ouS57ud44CCCnNldCAtZXUKZXhwb3J0IFBZVEhPTkRPTlRXUklURUJZVEVDT0RFPTEKZXhlYyAvaG9tZS9sb29zcWsvLmxvY2FsL3NoYXJlL3V2L3B5dGhvbi9jcHl0aG9uLTMuMTIuMTMtbGludXgtYWFyY2g2NC1nbnUvYmluL3B5dGhvbjMgLSA8PCdQWUxBVU5DSCcKaW1wb3J0IG9zLHN1YnByb2Nlc3MKRklFTERfU1RBTVA9Tm9uZQpGSUVMRF9FWFBFQ1RFRD1Ob25lCkdPX0VYUEVDVEVEPU5vbmUKQklORElOR19FWFBFQ1RFRD1Ob25lClBMQU5fRVhQRUNURUQ9Tm9uZQpQTEFOX0NBTk9OSUNBTF9TSEE9Tm9uZQpOQVRJVkVfUFJFUEFSQVRJT05fRVhQRUNURUQ9Tm9uZQphc3NlcnQgdHlwZShGSUVMRF9TVEFNUCkgaXMgaW50IGFuZCBGSUVMRF9TVEFNUD4wIGFuZCBhbGwodiBpcyBub3QgTm9uZSBmb3IgdiBpbiAoRklFTERfRVhQRUNURUQsR09fRVhQRUNURUQsQklORElOR19FWFBFQ1RFRCxQTEFOX0VYUEVDVEVELE5BVElWRV9QUkVQQVJBVElPTl9FWFBFQ1RFRCkpIGFuZCB0eXBlKFBMQU5fQ0FOT05JQ0FMX1NIQSkgaXMgc3RyIGFuZCBsZW4oUExBTl9DQU5PTklDQUxfU0hBKT09NjQsICfniLjniLjvvIzpm7bnm67moIdsYXVuY2hlcuacque7keWumuacrOi9rueOsOWcui9HTy/ova/ku7blh4blpIfvvIzpm7ZJL0/mi5Lnu53jgIInCnN1YnByb2Nlc3MucnVuKFsndG11eCcsJ3NldC1vcHRpb24nLCctdycsJy10Jyxvcy5lbnZpcm9uWydUTVVYX1BBTkUnXSwncmVtYWluLW9uLWV4aXQnLCdvbiddLGNoZWNrPVRydWUpCm9zLmV4ZWN2KCcvaG9tZS9sb29zcWsvLmxvY2FsL3NoYXJlL3V2L3B5dGhvbi9jcHl0aG9uLTMuMTIuMTMtbGludXgtYWFyY2g2NC1nbnUvYmluL3B5dGhvbjMnLFsnL2hvbWUvbG9vc3FrLy5sb2NhbC9zaGFyZS91di9weXRob24vY3B5dGhvbi0zLjEyLjEzLWxpbnV4LWFhcmNoNjQtZ251L2Jpbi9weXRob24zJywnL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvYmFzZS16ZXJvLWRpYWdub3N0aWMtcjEvYmFzZV96ZXJvX2RpYWdub3N0aWNfcjEucHknLCctLXJhdy1oZWxwZXInLCcvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9iYXNlLXplcm8tZGlhZ25vc3RpYy1yMS9oZWxwZXJzL3Jhd19yaWdodDlfZnJvbV9yMTRfcjEucHknLCctLW1haW50ZW5hbmNlJywnL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvc29mdHdhcmUtZmFjdHMtcjEvc2NyaXB0cy9ydW5faGFyZHdhcmVfbWFpbnRlbmFuY2UucHknLCctLWJpbmRpbmdzJywnL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvYmFzZS16ZXJvLWRpYWdub3N0aWMtcjEvbWFpbnRlbmFuY2VfYmluZGluZ3NfYmFzZV96ZXJvX2RpYWdub3N0aWNfcjEuanNvbicsJy0tb3V0cHV0JywnL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvYmFzZS16ZXJvLWRpYWdub3N0aWMtcjEvYmFzZS16ZXJvLWRpYWdub3N0aWMtcjEnLCctLWFydGlmYWN0LXJvb3QnLCcvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMScsJy0tZXhlY3V0ZScsJy0tYXBwcm92YWwnLCcvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9iYXNlLXplcm8tZGlhZ25vc3RpYy1yMS9iYXNlLXplcm8tZGlhZ25vc3RpYy1yMS5nby5qc29uJ10pClBZTEFVTkNICg==',validate=True)
for key,value in (('FIELD_STAMP',FIELD_STAMP),('FIELD_EXPECTED',FIELD_EXPECTED),('GO_EXPECTED',GO_EXPECTED),('BINDING_EXPECTED',BINDING_EXPECTED),('PLAN_EXPECTED',PLAN_EXPECTED),('PLAN_CANONICAL_SHA',PLAN_CANONICAL_SHA),('NATIVE_PREPARATION_EXPECTED',NATIVE_PREPARATION_EXPECTED)):
 marker=(key+'=None').encode();assert launcher.count(marker)==1
 launcher=launcher.replace(marker,(key+'='+repr(value)).encode())
motor_identity,motor_tmux=start_tmux('launch_base_zero_diagnostic_r1.sh','position-base-zero-diagnostic-trial-20261012-r1',launcher)
write_once('motor-pane.txt',(motor_identity+'\n').encode())
result={'status':'STARTED_SINGLE_BASE_ZERO_DIAGNOSTIC_R1','tmux_identity':motor_identity,'motor_identity':motor_identity,'motor_tmux':motor_tmux,'go_sha256':hashlib.sha256(go_raw).hexdigest(),'go_bytes':len(go_raw),'plan_sha256':go['plan_sha256'],'motor_launcher_sha256':hashlib.sha256(launcher).hexdigest(),'camera_processes_started':0}
write_once('start_receipt_base_zero_r1.json',(json.dumps(result,ensure_ascii=False,indent=2)+'\n').encode());print(json.dumps(result,ensure_ascii=False),flush=True)

PYREMOTE
