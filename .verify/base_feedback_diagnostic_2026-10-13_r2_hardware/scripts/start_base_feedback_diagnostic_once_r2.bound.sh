#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,importlib.util,json,os,shlex,subprocess,time
from pathlib import Path
FIELD_STAMP=1791870170000000000  # 仅root本次新现场事实可替换；草稿不能执行。
FIELD_EXPECTED=('4454145c392d430a99b0b31b796fab4ece625528438f11cfcffbb7fd6e0d0892', 1645)  # root固定新现场回执(sha256,bytes)。
GO_EXPECTED=('387d11aa8aeda953b774ee25cc8ac3779cd920a94929afc18c1e49d4c079da3d', 1782)  # root固定本次新GO(sha256,bytes)，不能复用任何旧GO。
BINDING_EXPECTED=('ac4c13d8c2c0d61216f8414ee141f7f8744f84a97d413d71c4a2ba91b624b669', 30529)  # native准备后由root固定本轮binding(sha256,bytes)。
PLAN_EXPECTED=('034a118fb85677bf204a1428385bc83820d7ee6b13577df1044de98117449735', 38653)  # native准备后由root固定完整PLAN stdout(sha256,bytes)。
NATIVE_PREPARATION_EXPECTED=('a929d202c53bd918ea8d3221a5ff1c9e9694be1d80b41ac5d7e69492887f44ea', 4809)  # root固定本轮原生软件prepare-result(sha256,bytes)。
PLAN_CANONICAL_SHA='bfd13186961ca75be190caa1c9ba8ae50ff843c3f9f35d8329e6fb0ca2ad1bfc'  # native准备后由root固定本轮canonical PLAN。
assert type(FIELD_STAMP) is int and FIELD_STAMP>0 and FIELD_EXPECTED is not None and GO_EXPECTED is not None, '爸爸，BASE_FEEDBACK_R2启动草稿未获root本次新GO，零启动。' 
assert BINDING_EXPECTED is not None and PLAN_EXPECTED is not None and NATIVE_PREPARATION_EXPECTED is not None and type(PLAN_CANONICAL_SHA) is str and len(PLAN_CANONICAL_SHA)==64, '爸爸，BASE_FEEDBACK_R2原生binding/PLAN未固定，零I/O拒绝。'
legacy=Path('/home/loosqk/robot-checks/position-return-20261010-r1');base=Path('/home/loosqk/robot-checks/base-feedback-diagnostic-r2');phase=base
expected={'maintenance_bindings_base_feedback_diagnostic_r2.json': BINDING_EXPECTED, 'full-plan-only-r2.stdout': PLAN_EXPECTED, 'prepare-result-base-feedback-r2.json': NATIVE_PREPARATION_EXPECTED, 'field_confirmation_base_feedback_r2.json': FIELD_EXPECTED, 'base-feedback-diagnostic-r2.go.json': GO_EXPECTED}
for name,(sha,size) in expected.items():
 path=phase/name;raw=path.read_bytes();assert path.is_file() and not path.is_symlink() and hashlib.sha256(raw).hexdigest()==sha and len(raw)==size
script=legacy/'software-facts-r1/scripts/run_hardware_maintenance.py'
assert hashlib.sha256(script.read_bytes()).hexdigest()=='d97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41'
spec=importlib.util.spec_from_file_location('base_zero_start_frozen_d97',script);m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
bindings=m.strict_json((phase/'maintenance_bindings_base_feedback_diagnostic_r2.json').read_text())
for ref in bindings['refs']:m.verify_ref(ref)
for side,files in bindings['protection'].items():
 for name,ref in files.items():m.verify_protection_file(side,name,ref)
assert Path('/proc/sys/kernel/random/boot_id').read_text().strip()==bindings['host']['boot_id']
for side,ref in bindings['usb'].items():
 device=Path(ref['device']);assert str(device.resolve())==ref['resolved'] and device.stat().st_rdev==ref['rdev'] and device.stat().st_ino==ref['device_inode'] and device.lstat().st_ino==ref['link_inode']
candidate=Path('/home/loosqk/robot-checks/base-feedback-r2-software-20261013/candidate/base_feedback_diagnostic_r2.py');helper=Path('/home/loosqk/robot-checks/base-feedback-r2-software-20261013/candidate/fixtures/raw_right9_from_r14_r2.py')
for path,pin in ((candidate,('cf651bb200d0ea56228282dd51b311bb090650945b631f51e8d7ff2f1e431b78', 18648)),(helper,('70d777bd5c2a19e2ce3f57e68f9abd40eaed2b8a9aa8c592e7fe53a940f1e38c',12220)),(legacy/'normal-read-r4/nonuart_probe_r4.sh',('e3962ec7841a9d5ed783ca8279ed718c8f29ed718701de528c226f0588b91527',20020))):
 raw=path.read_bytes();assert not path.is_symlink() and hashlib.sha256(raw).hexdigest()==pin[0] and len(raw)==pin[1]
for name,path in (('base_zero_start_fixed_candidate',candidate),('base_zero_start_fixed_raw_helper',helper)):
 spec=importlib.util.spec_from_file_location(name,path);value=importlib.util.module_from_spec(spec);spec.loader.exec_module(value)
 if path==candidate:c=value
 else:h=value
preparation=m.strict_json((phase/'prepare-result-base-feedback-r2.json').read_text())
assert preparation['round']==2 and preparation['profile_run_id']=='base-feedback-diagnostic-r2' and preparation['status']=='PASS_BASE_FEEDBACK_R2_NATIVE_RUNTIME_PREPARATION_ONLY' and preparation['uart_operations']==preparation['motor_actions']==0 and preparation['go_created'] is False and preparation['field_confirmation_created'] is False
plan=m.strict_json((phase/'full-plan-only-r2.stdout').read_text());assert plan['plan_sha256']==PLAN_CANONICAL_SHA and m.digest(plan['plan'])==plan['plan_sha256']
go_path=phase/'base-feedback-diagnostic-r2.go.json';go_raw=go_path.read_bytes();assert go_path.is_file() and not go_path.is_symlink()
go=m.strict_json(go_raw.decode());c.install(m,h,go);now=time.time_ns();m.validate_go(go,plan['plan'],now)
assert 0<go['expires_at_ns']-go['issued_at_ns']<=600_000_000_000 and 0<=now-FIELD_STAMP<600_000_000_000
assert all(go['physical'][side]['confirmed_at_ns']==FIELD_STAMP for side in ('left','right'))
output=phase/'base-feedback-diagnostic-r2';record=phase/'base-feedback-diagnostic-launch-r2';assert not output.exists() and not record.exists()

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
probe_argv=['bash',str(legacy/'normal-read-r4/nonuart_probe_r4.sh')]
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
launcher=base64.b64decode('IyEvYmluL2Jhc2gKIyDniLjniLjvvIzkv53mjIFzdGRpbui/nuaOpXRtdXjnmoRQVFnvvIzlj6rmnInmnKzova7lhajpg6hwaW5z57uR5a6a5ZCO5omN5omn6KGM77yb5q2j5paHL+WPquivu+WAmemAieWFpeWPo+S/neaMgeOAggpzZXQgLWV1CmV4cG9ydCBQWVRIT05ET05UV1JJVEVCWVRFQ09ERT0xCmV4ZWMgL2hvbWUvbG9vc3FrLy5sb2NhbC9zaGFyZS91di9weXRob24vY3B5dGhvbi0zLjEyLjEzLWxpbnV4LWFhcmNoNjQtZ251L2Jpbi9weXRob24zIC1jICdpbXBvcnQgb3Msc3VicHJvY2VzcwpGSUVMRF9TVEFNUD1Ob25lCkZJRUxEX0VYUEVDVEVEPU5vbmUKR09fRVhQRUNURUQ9Tm9uZQpCSU5ESU5HX0VYUEVDVEVEPU5vbmUKUExBTl9FWFBFQ1RFRD1Ob25lClBMQU5fQ0FOT05JQ0FMX1NIQT1Ob25lCk5BVElWRV9QUkVQQVJBVElPTl9FWFBFQ1RFRD1Ob25lCmFzc2VydCB0eXBlKEZJRUxEX1NUQU1QKSBpcyBpbnQgYW5kIEZJRUxEX1NUQU1QPjAgYW5kIGFsbCh2IGlzIG5vdCBOb25lIGZvciB2IGluIChGSUVMRF9FWFBFQ1RFRCxHT19FWFBFQ1RFRCxCSU5ESU5HX0VYUEVDVEVELFBMQU5fRVhQRUNURUQsTkFUSVZFX1BSRVBBUkFUSU9OX0VYUEVDVEVEKSkgYW5kIHR5cGUoUExBTl9DQU5PTklDQUxfU0hBKSBpcyBzdHIgYW5kIGxlbihQTEFOX0NBTk9OSUNBTF9TSEEpPT02NCwgJyInIifniLjniLjvvIzlj6ror7vlj43ppohsYXVuY2hlcuacque7keWumuacrOi9rueOsOWcui9HTy/ova/ku7blh4blpIfvvIzpm7ZJL0/mi5Lnu53jgIInIiciJwpsb2c9b3Mub3BlbignIiciJy9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvYmFzZS1mZWVkYmFjay1kaWFnbm9zdGljLXIyL2Jhc2UtZmVlZGJhY2stZGlhZ25vc3RpYy1sYXVuY2gtcjIvZHJpdmVyLmxvZyciJyInLG9zLk9fV1JPTkxZfG9zLk9fQ1JFQVR8b3MuT19FWENMLDBvNjAwKQpvcy5kdXAyKGxvZywxKTtvcy5kdXAyKGxvZywyKTtvcy5jbG9zZShsb2cpCnN1YnByb2Nlc3MucnVuKFsnIiciJ3RtdXgnIiciJywnIiciJ3NldC1vcHRpb24nIiciJywnIiciJy1wJyInIicsJyInIictdCciJyInLG9zLmVudmlyb25bJyInIidUTVVYX1BBTkUnIiciJ10sJyInIidyZW1haW4tb24tZXhpdCciJyInLCciJyInb24nIiciJ10sY2hlY2s9VHJ1ZSkKb3MuZXhlY3YoJyInIicvaG9tZS9sb29zcWsvLmxvY2FsL3NoYXJlL3V2L3B5dGhvbi9jcHl0aG9uLTMuMTIuMTMtbGludXgtYWFyY2g2NC1nbnUvYmluL3B5dGhvbjMnIiciJyxbJyInIicvaG9tZS9sb29zcWsvLmxvY2FsL3NoYXJlL3V2L3B5dGhvbi9jcHl0aG9uLTMuMTIuMTMtbGludXgtYWFyY2g2NC1nbnUvYmluL3B5dGhvbjMnIiciJywnIiciJy9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvYmFzZS1mZWVkYmFjay1yMi1zb2Z0d2FyZS0yMDI2MTAxMy9jYW5kaWRhdGUvYmFzZV9mZWVkYmFja19kaWFnbm9zdGljX3IyLnB5JyInIicsJyInIictLXJhdy1oZWxwZXInIiciJywnIiciJy9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvYmFzZS1mZWVkYmFjay1yMi1zb2Z0d2FyZS0yMDI2MTAxMy9jYW5kaWRhdGUvZml4dHVyZXMvcmF3X3JpZ2h0OV9mcm9tX3IxNF9yMi5weSciJyInLCciJyInLS1tYWludGVuYW5jZSciJyInLCciJyInL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvc29mdHdhcmUtZmFjdHMtcjEvc2NyaXB0cy9ydW5faGFyZHdhcmVfbWFpbnRlbmFuY2UucHknIiciJywnIiciJy0tYmluZGluZ3MnIiciJywnIiciJy9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvYmFzZS1mZWVkYmFjay1kaWFnbm9zdGljLXIyL21haW50ZW5hbmNlX2JpbmRpbmdzX2Jhc2VfZmVlZGJhY2tfZGlhZ25vc3RpY19yMi5qc29uJyInIicsJyInIictLW91dHB1dCciJyInLCciJyInL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9iYXNlLWZlZWRiYWNrLWRpYWdub3N0aWMtcjIvYmFzZS1mZWVkYmFjay1kaWFnbm9zdGljLXIyJyInIicsJyInIictLWFydGlmYWN0LXJvb3QnIiciJywnIiciJy9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvYmFzZS1mZWVkYmFjay1kaWFnbm9zdGljLXIyJyInIicsJyInIictLWV4ZWN1dGUnIiciJywnIiciJy0tYXBwcm92YWwnIiciJywnIiciJy9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvYmFzZS1mZWVkYmFjay1kaWFnbm9zdGljLXIyL2Jhc2UtZmVlZGJhY2stZGlhZ25vc3RpYy1yMi5nby5qc29uJyInIiddKScK',validate=True)
# 爸爸，先在Python正文填本轮pins，再整体shell引用，避免repr内单引号破坏-c参数。
launcher_header,launcher_command=launcher.decode().split('exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 -c ',1)
launcher_args=shlex.split(launcher_command);assert len(launcher_args)==1
launcher_program=launcher_args[0]
for key,value in (('FIELD_STAMP',FIELD_STAMP),('FIELD_EXPECTED',FIELD_EXPECTED),('GO_EXPECTED',GO_EXPECTED),('BINDING_EXPECTED',BINDING_EXPECTED),('PLAN_EXPECTED',PLAN_EXPECTED),('PLAN_CANONICAL_SHA',PLAN_CANONICAL_SHA),('NATIVE_PREPARATION_EXPECTED',NATIVE_PREPARATION_EXPECTED)):
 marker=key+'=None';assert launcher_program.count(marker)==1
 launcher_program=launcher_program.replace(marker,key+'='+repr(value))
launcher=(launcher_header+'exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 -c '+shlex.quote(launcher_program)+'\n').encode()
motor_identity,motor_tmux=start_tmux('launch_base_feedback_diagnostic_r2.sh','base-feedback-diagnostic-trial-20261013-r2',launcher)
write_once('motor-pane.txt',(motor_identity+'\n').encode())
result={'status':'STARTED_SINGLE_BASE_FEEDBACK_DIAGNOSTIC_R2','tmux_identity':motor_identity,'motor_identity':motor_identity,'motor_tmux':motor_tmux,'go_sha256':hashlib.sha256(go_raw).hexdigest(),'go_bytes':len(go_raw),'plan_sha256':go['plan_sha256'],'motor_launcher_sha256':hashlib.sha256(launcher).hexdigest(),'camera_processes_started':0}
write_once('start_receipt_base_feedback_r2.json',(json.dumps(result,ensure_ascii=False,indent=2)+'\n').encode());print(json.dumps(result,ensure_ascii=False),flush=True)

PYREMOTE
