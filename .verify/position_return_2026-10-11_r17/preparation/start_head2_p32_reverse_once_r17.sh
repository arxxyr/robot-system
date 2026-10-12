#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,importlib.util,json,os,subprocess,time
from pathlib import Path
FIELD_STAMP=1791776802071295000  # 仅root本次新现场事实可替换；草稿不能执行。
FIELD_EXPECTED=('8e9757468e530e362f15bed03526b11517fee705edf7e0d5e766bf077088eb4b', 1275)  # root固定新现场回执(sha256,bytes)。
GO_EXPECTED=('a3d2c0b86c7936df6864ba118b7ca0abc85aa575d0758929f092c95c3aad31cf', 1333)  # root固定本次新GO(sha256,bytes)，不能复用任何旧GO。
BINDING_EXPECTED=('274a2be1541725aaf18b4fff4b39f1c93f54e31ba6f142b7bb5c47af1e7840ef', 29452)  # native准备后由root固定本轮binding(sha256,bytes)。
PLAN_EXPECTED=('c2535fc2da98dc3ea357267903ab184a76ac39966c45429afe5efd34a718f3fd', 39147)  # native准备后由root固定完整PLAN stdout(sha256,bytes)。
PLAN_CANONICAL_SHA='46bf22742beb59c57ba465c1bde2f5e1afe0e9022be03381c4e97c9345d04c85'  # native准备后由root固定本轮canonical PLAN。
assert type(FIELD_STAMP) is int and FIELD_STAMP>0 and FIELD_EXPECTED is not None and GO_EXPECTED is not None, '爸爸，R17启动草稿未获root本次新GO，零启动。' 
assert BINDING_EXPECTED is not None and PLAN_EXPECTED is not None and type(PLAN_CANONICAL_SHA) is str and len(PLAN_CANONICAL_SHA)==64, '爸爸，R17原生binding/PLAN未固定，零I/O拒绝。'
base=Path('/home/loosqk/robot-checks/position-return-20261010-r1');phase=base/'head2-p32-reverse-r17'
expected={'head2_p32_reverse_trial_r17.py': ('32f4f9d5033710b6eeffeb47782c2156a402ba1d31f82cc285e08039802b71e5', 16246), 'maintenance_bindings_head2_p32_reverse_r17.json': BINDING_EXPECTED, 'full-plan-only.stdout': PLAN_EXPECTED, 'field_confirmation_r17.json': FIELD_EXPECTED, 'head2-p32-reverse-r17.go.json': GO_EXPECTED}
for name,(sha,size) in expected.items():
 path=phase/name;raw=path.read_bytes();assert path.is_file() and not path.is_symlink() and hashlib.sha256(raw).hexdigest()==sha and len(raw)==size
script=base/'software-facts-r1/scripts/run_hardware_maintenance.py'
assert hashlib.sha256(script.read_bytes()).hexdigest()=='d97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41'
spec=importlib.util.spec_from_file_location('head2_p32_reverse_start_binding',script);m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
bindings=m.strict_json((phase/'maintenance_bindings_head2_p32_reverse_r17.json').read_text())
for ref in bindings['refs']:m.verify_ref(ref)
for side,files in bindings['protection'].items():
 for name,ref in files.items():m.verify_protection_file(side,name,ref)
assert Path('/proc/sys/kernel/random/boot_id').read_text().strip()==bindings['host']['boot_id']
for side,ref in bindings['usb'].items():
 device=Path(ref['device']);assert str(device.resolve())==ref['resolved'] and device.stat().st_rdev==ref['rdev'] and device.stat().st_ino==ref['device_inode'] and device.lstat().st_ino==ref['link_inode']
plan=m.strict_json((phase/'full-plan-only.stdout').read_text());assert plan['plan_sha256']==PLAN_CANONICAL_SHA and m.digest(plan['plan'])==plan['plan_sha256']
go_path=phase/'head2-p32-reverse-r17.go.json';go_raw=go_path.read_bytes();assert go_path.is_file() and not go_path.is_symlink()
go=m.strict_json(go_raw.decode());now=time.time_ns();m.validate_go(go,plan['plan'],now)
assert 0<go['expires_at_ns']-go['issued_at_ns']<=600_000_000_000 and 0<=now-FIELD_STAMP<600_000_000_000
assert all(go['physical'][side]['confirmed_at_ns']==FIELD_STAMP for side in ('left','right'))
output=phase/'head2-p32-reverse-r17';record=phase/'head2-p32-reverse-launch-r17';assert not output.exists() and not record.exists()

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
launcher=base64.b64decode('IyEvYmluL2Jhc2gKc2V0IC1ldQpleHBvcnQgUFlUSE9ORE9OVFdSSVRFQllURUNPREU9MQp0bXV4IHNldC1vcHRpb24gLXcgLXQgIiRUTVVYX1BBTkUiIHJlbWFpbi1vbi1leGl0IG9uCmV4ZWMgL2hvbWUvbG9vc3FrLy5sb2NhbC9zaGFyZS91di9weXRob24vY3B5dGhvbi0zLjEyLjEzLWxpbnV4LWFhcmNoNjQtZ251L2Jpbi9weXRob24zIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL2hlYWQyLXAzMi1yZXZlcnNlLXIxNy9oZWFkMl9wMzJfcmV2ZXJzZV90cmlhbF9yMTcucHkgLS1oZWxwZXIgL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvaGVhZDItcDMyLXJldmVyc2UtcjE3L2hlbHBlcnMvcGFyYW1ldGVyX3N0b3BfaGVhZDJfcjE0LnB5IC0tb2JzZXJ2YXRpb24taGVscGVyIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL2hlYWQyLXAzMi1yZXZlcnNlLXIxNy9oZWxwZXJzL2hlYWQyX291dHdhcmRfb2JzZXJ2YXRpb25fcjE0LnB5IC0tbWFpbnRlbmFuY2UgL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvc29mdHdhcmUtZmFjdHMtcjEvc2NyaXB0cy9ydW5faGFyZHdhcmVfbWFpbnRlbmFuY2UucHkgLS1iaW5kaW5ncyAvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9oZWFkMi1wMzItcmV2ZXJzZS1yMTcvbWFpbnRlbmFuY2VfYmluZGluZ3NfaGVhZDJfcDMyX3JldmVyc2VfcjE3Lmpzb24gLS1vdXRwdXQgL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvaGVhZDItcDMyLXJldmVyc2UtcjE3L2hlYWQyLXAzMi1yZXZlcnNlLXIxNyAtLXJ1bi1pZCBoZWFkMi1wMzItcmV2ZXJzZS1yMTcgLS1hcnRpZmFjdC1yb290IC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxIC0tZXhlY3V0ZSAtLWFwcHJvdmFsIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL2hlYWQyLXAzMi1yZXZlcnNlLXIxNy9oZWFkMi1wMzItcmV2ZXJzZS1yMTcuZ28uanNvbiA+IC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL2hlYWQyLXAzMi1yZXZlcnNlLXIxNy9oZWFkMi1wMzItcmV2ZXJzZS1sYXVuY2gtcjE3L2RyaXZlci5sb2cgMj4mMQo=',validate=True)
motor_identity,motor_tmux=start_tmux('launch_head2_p32_reverse_r17.sh','position-head2-p32-reverse-20261011-r17',launcher)
write_once('motor-pane.txt',(motor_identity+'\n').encode())
result={'status':'STARTED_SINGLE_HEAD2_P32_REVERSE_R17','tmux_identity':motor_identity,'motor_identity':motor_identity,'motor_tmux':motor_tmux,'go_sha256':hashlib.sha256(go_raw).hexdigest(),'go_bytes':len(go_raw),'plan_sha256':go['plan_sha256'],'motor_launcher_sha256':hashlib.sha256(launcher).hexdigest(),'camera_processes_started':0}
write_once('start_receipt_r17.json',(json.dumps(result,ensure_ascii=False,indent=2)+'\n').encode());print(json.dumps(result,ensure_ascii=False),flush=True)

PYREMOTE
