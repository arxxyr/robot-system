#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,importlib.util,json,os,subprocess,time
from pathlib import Path
FIELD_STAMP=1791779943356024000  # 仅root本次新现场事实可替换；草稿不能执行。
FIELD_EXPECTED=('1f50e2702a78732bf18f887469a4ed9c1e5d86d57cd38097bbbb6e4ca91f5c97', 1541)  # root固定新现场回执(sha256,bytes)。
GO_EXPECTED=('c38354601430df6f3c7c546ea880a88de39f43fc2134c108f22b91a5bef84235', 1732)  # root固定本次新GO(sha256,bytes)，不能复用任何旧GO。
BINDING_EXPECTED=('60c1ce479a89e198bc237cc3c29ccf3d9b9cd81434c47026b9cf2b5c1dd03e1c', 30051)  # native准备后由root固定本轮binding(sha256,bytes)。
PLAN_EXPECTED=('1d049b491e69d0756e074cee388beaf814edb8e95013fdfce1ecc9d8d4ee0555', 38231)  # native准备后由root固定完整PLAN stdout(sha256,bytes)。
NATIVE_PREPARATION_EXPECTED=('7cb595efc356e73626a558c6b83857427eee59c878a8e33c1d9a85875e451160', 9904)  # root固定本轮原生软件prepare-result(sha256,bytes)。
PLAN_CANONICAL_SHA='37bb533e461a88cfafebdef966e601a1f21b8d67c04ea6b03e7852d6b21f9323'  # native准备后由root固定本轮canonical PLAN。
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
launcher=base64.b64decode('IyEvYmluL2Jhc2gKIyDniLjniLjvvIzni6znq4tsYXVuY2hlcuS5n+W/hemhu+eUseW3suaguHN0YXJ0ZXLloavlhajpg6jmnKzova5waW5z77yb57qv6I2J56i/6Zu2SS9P5ouS57ud44CCCnNldCAtZXUKZXhwb3J0IFBZVEhPTkRPTlRXUklURUJZVEVDT0RFPTEKZXhlYyAvaG9tZS9sb29zcWsvLmxvY2FsL3NoYXJlL3V2L3B5dGhvbi9jcHl0aG9uLTMuMTIuMTMtbGludXgtYWFyY2g2NC1nbnUvYmluL3B5dGhvbjMgLSA8PCdQWUxBVU5DSCcKaW1wb3J0IG9zLHN1YnByb2Nlc3MKRklFTERfU1RBTVA9Tm9uZQpGSUVMRF9FWFBFQ1RFRD1Ob25lCkdPX0VYUEVDVEVEPU5vbmUKQklORElOR19FWFBFQ1RFRD1Ob25lClBMQU5fRVhQRUNURUQ9Tm9uZQpQTEFOX0NBTk9OSUNBTF9TSEE9Tm9uZQpOQVRJVkVfUFJFUEFSQVRJT05fRVhQRUNURUQ9Tm9uZQphc3NlcnQgdHlwZShGSUVMRF9TVEFNUCkgaXMgaW50IGFuZCBGSUVMRF9TVEFNUD4wIGFuZCBhbGwodiBpcyBub3QgTm9uZSBmb3IgdiBpbiAoRklFTERfRVhQRUNURUQsR09fRVhQRUNURUQsQklORElOR19FWFBFQ1RFRCxQTEFOX0VYUEVDVEVELE5BVElWRV9QUkVQQVJBVElPTl9FWFBFQ1RFRCkpIGFuZCB0eXBlKFBMQU5fQ0FOT05JQ0FMX1NIQSkgaXMgc3RyIGFuZCBsZW4oUExBTl9DQU5PTklDQUxfU0hBKT09NjQsICfniLjniLjvvIzpm7bnm67moIdsYXVuY2hlcuacque7keWumuacrOi9rueOsOWcui9HTy/ova/ku7blh4blpIfvvIzpm7ZJL0/mi5Lnu53jgIInCmxvZz1vcy5vcGVuKCcvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9iYXNlLXplcm8tZGlhZ25vc3RpYy1yMS9iYXNlLXplcm8tZGlhZ25vc3RpYy1sYXVuY2gtcjEvZHJpdmVyLmxvZycsb3MuT19XUk9OTFl8b3MuT19DUkVBVHxvcy5PX0VYQ0wsMG82MDApCm9zLmR1cDIobG9nLDEpO29zLmR1cDIobG9nLDIpO29zLmNsb3NlKGxvZykKc3VicHJvY2Vzcy5ydW4oWyd0bXV4Jywnc2V0LW9wdGlvbicsJy13JywnLXQnLG9zLmVudmlyb25bJ1RNVVhfUEFORSddLCdyZW1haW4tb24tZXhpdCcsJ29uJ10sY2hlY2s9VHJ1ZSkKb3MuZXhlY3YoJy9ob21lL2xvb3Nxay8ubG9jYWwvc2hhcmUvdXYvcHl0aG9uL2NweXRob24tMy4xMi4xMy1saW51eC1hYXJjaDY0LWdudS9iaW4vcHl0aG9uMycsWycvaG9tZS9sb29zcWsvLmxvY2FsL3NoYXJlL3V2L3B5dGhvbi9jcHl0aG9uLTMuMTIuMTMtbGludXgtYWFyY2g2NC1nbnUvYmluL3B5dGhvbjMnLCcvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9iYXNlLXplcm8tZGlhZ25vc3RpYy1yMS9iYXNlX3plcm9fZGlhZ25vc3RpY19yMS5weScsJy0tcmF3LWhlbHBlcicsJy9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL2Jhc2UtemVyby1kaWFnbm9zdGljLXIxL2hlbHBlcnMvcmF3X3JpZ2h0OV9mcm9tX3IxNF9yMS5weScsJy0tbWFpbnRlbmFuY2UnLCcvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9zb2Z0d2FyZS1mYWN0cy1yMS9zY3JpcHRzL3J1bl9oYXJkd2FyZV9tYWludGVuYW5jZS5weScsJy0tYmluZGluZ3MnLCcvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9iYXNlLXplcm8tZGlhZ25vc3RpYy1yMS9tYWludGVuYW5jZV9iaW5kaW5nc19iYXNlX3plcm9fZGlhZ25vc3RpY19yMS5qc29uJywnLS1vdXRwdXQnLCcvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9iYXNlLXplcm8tZGlhZ25vc3RpYy1yMS9iYXNlLXplcm8tZGlhZ25vc3RpYy1yMScsJy0tYXJ0aWZhY3Qtcm9vdCcsJy9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxJywnLS1leGVjdXRlJywnLS1hcHByb3ZhbCcsJy9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL2Jhc2UtemVyby1kaWFnbm9zdGljLXIxL2Jhc2UtemVyby1kaWFnbm9zdGljLXIxLmdvLmpzb24nXSkKUFlMQVVOQ0gK',validate=True)
for key,value in (('FIELD_STAMP',FIELD_STAMP),('FIELD_EXPECTED',FIELD_EXPECTED),('GO_EXPECTED',GO_EXPECTED),('BINDING_EXPECTED',BINDING_EXPECTED),('PLAN_EXPECTED',PLAN_EXPECTED),('PLAN_CANONICAL_SHA',PLAN_CANONICAL_SHA),('NATIVE_PREPARATION_EXPECTED',NATIVE_PREPARATION_EXPECTED)):
 marker=(key+'=None').encode();assert launcher.count(marker)==1
 launcher=launcher.replace(marker,(key+'='+repr(value)).encode())
motor_identity,motor_tmux=start_tmux('launch_base_zero_diagnostic_r1.sh','base-zero-diagnostic-trial-20261012-r1',launcher)
write_once('motor-pane.txt',(motor_identity+'\n').encode())
result={'status':'STARTED_SINGLE_BASE_ZERO_DIAGNOSTIC_R1','tmux_identity':motor_identity,'motor_identity':motor_identity,'motor_tmux':motor_tmux,'go_sha256':hashlib.sha256(go_raw).hexdigest(),'go_bytes':len(go_raw),'plan_sha256':go['plan_sha256'],'motor_launcher_sha256':hashlib.sha256(launcher).hexdigest(),'camera_processes_started':0}
write_once('start_receipt_base_zero_r1.json',(json.dumps(result,ensure_ascii=False,indent=2)+'\n').encode());print(json.dumps(result,ensure_ascii=False),flush=True)

PYREMOTE
