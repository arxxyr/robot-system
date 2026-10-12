#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,importlib.util,json,os,subprocess,time
from pathlib import Path
FIELD_STAMP=None  # 仅root本次新现场事实可替换；草稿不能执行。
FIELD_EXPECTED=None  # root固定新现场回执(sha256,bytes)。
GO_EXPECTED=None  # root固定本次新GO(sha256,bytes)，不能复用任何旧GO。
assert type(FIELD_STAMP) is int and FIELD_STAMP>0 and FIELD_EXPECTED is not None and GO_EXPECTED is not None, '爸爸，R14启动草稿未获root本次新GO，零启动。' 
base=Path('/home/loosqk/robot-checks/position-return-20261010-r1');phase=base/'head2-p32-trial-r14'
expected={'head2_p32_trial_r14.py': ('5053419d3659efbacafea0238e3d0c03d9351963c6b93224b3e99587889e9015', 12633), 'maintenance_bindings_head2_p32_r14.json': ('fb94a118feaf074ca495fe7311b710807668ee0eff0beba3c660b12e9183f660', 28598), 'full-plan-only.stdout': ('3334cf4e5cf73631301009f52959b63d9010ffdc25e08ef4921768eaaea5333f', 38072), 'field_confirmation_r14.json': FIELD_EXPECTED, 'head2-p32-trial-r14.go.json': GO_EXPECTED}
for name,(sha,size) in expected.items():
 path=phase/name;raw=path.read_bytes();assert path.is_file() and not path.is_symlink() and hashlib.sha256(raw).hexdigest()==sha and len(raw)==size
script=base/'software-facts-r1/scripts/run_hardware_maintenance.py'
assert hashlib.sha256(script.read_bytes()).hexdigest()=='d97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41'
spec=importlib.util.spec_from_file_location('head2_p32_wire_start_binding',script);m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
bindings=m.strict_json((phase/'maintenance_bindings_head2_p32_r14.json').read_text())
for ref in bindings['refs']:m.verify_ref(ref)
for side,files in bindings['protection'].items():
 for name,ref in files.items():m.verify_protection_file(side,name,ref)
assert Path('/proc/sys/kernel/random/boot_id').read_text().strip()==bindings['host']['boot_id']
for side,ref in bindings['usb'].items():
 device=Path(ref['device']);assert str(device.resolve())==ref['resolved'] and device.stat().st_rdev==ref['rdev'] and device.stat().st_ino==ref['device_inode'] and device.lstat().st_ino==ref['link_inode']
plan=m.strict_json((phase/'full-plan-only.stdout').read_text());assert plan['plan_sha256']=='faf87162281223bd9ff8dcbd406c306a4273a7b83059162af44f16e825e46d6d' and m.digest(plan['plan'])==plan['plan_sha256']
go_path=phase/'head2-p32-trial-r14.go.json';go_raw=go_path.read_bytes();assert go_path.is_file() and not go_path.is_symlink()
go=m.strict_json(go_raw.decode());now=time.time_ns();m.validate_go(go,plan['plan'],now)
assert 0<go['expires_at_ns']-go['issued_at_ns']<=600_000_000_000 and 0<=now-FIELD_STAMP<600_000_000_000
assert all(go['physical'][side]['confirmed_at_ns']==FIELD_STAMP for side in ('left','right'))
output=phase/'head2-p32-trial-r14';record=phase/'head2-p32-trial-launch-r14';assert not output.exists() and not record.exists()

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
launcher=base64.b64decode('IyEvYmluL2Jhc2gKc2V0IC1ldQpleHBvcnQgUFlUSE9ORE9OVFdSSVRFQllURUNPREU9MQp0bXV4IHNldC1vcHRpb24gLXcgLXQgIiRUTVVYX1BBTkUiIHJlbWFpbi1vbi1leGl0IG9uCmV4ZWMgL2hvbWUvbG9vc3FrLy5sb2NhbC9zaGFyZS91di9weXRob24vY3B5dGhvbi0zLjEyLjEzLWxpbnV4LWFhcmNoNjQtZ251L2Jpbi9weXRob24zIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL2hlYWQyLXAzMi10cmlhbC1yMTQvaGVhZDJfcDMyX3RyaWFsX3IxNC5weSAtLWhlbHBlciAvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9oZWFkMi1wMzItdHJpYWwtcjE0L2hlbHBlcnMvcGFyYW1ldGVyX3N0b3BfaGVhZDJfcjE0LnB5IC0tb2JzZXJ2YXRpb24taGVscGVyIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL2hlYWQyLXAzMi10cmlhbC1yMTQvaGVscGVycy9oZWFkMl9vdXR3YXJkX29ic2VydmF0aW9uX3IxNC5weSAtLW1haW50ZW5hbmNlIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL3NvZnR3YXJlLWZhY3RzLXIxL3NjcmlwdHMvcnVuX2hhcmR3YXJlX21haW50ZW5hbmNlLnB5IC0tYmluZGluZ3MgL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvaGVhZDItcDMyLXRyaWFsLXIxNC9tYWludGVuYW5jZV9iaW5kaW5nc19oZWFkMl9wMzJfcjE0Lmpzb24gLS1vdXRwdXQgL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvaGVhZDItcDMyLXRyaWFsLXIxNC9oZWFkMi1wMzItdHJpYWwtcjE0IC0tcnVuLWlkIGhlYWQyLXAzMi10cmlhbC1yMTQgLS1hcnRpZmFjdC1yb290IC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxIC0tZXhlY3V0ZSAtLWFwcHJvdmFsIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL2hlYWQyLXAzMi10cmlhbC1yMTQvaGVhZDItcDMyLXRyaWFsLXIxNC5nby5qc29uID4gL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvaGVhZDItcDMyLXRyaWFsLXIxNC9oZWFkMi1wMzItdHJpYWwtbGF1bmNoLXIxNC9kcml2ZXIubG9nIDI+JjEK',validate=True)
motor_identity,motor_tmux=start_tmux('launch_head2_p32_trial_r14.sh','position-head2-p32-trial-20261011-r14',launcher)
write_once('motor-pane.txt',(motor_identity+'\n').encode())
result={'status':'STARTED_SINGLE_HEAD2_P32_TRIAL','tmux_identity':motor_identity,'motor_identity':motor_identity,'motor_tmux':motor_tmux,'go_sha256':hashlib.sha256(go_raw).hexdigest(),'go_bytes':len(go_raw),'plan_sha256':go['plan_sha256'],'motor_launcher_sha256':hashlib.sha256(launcher).hexdigest(),'camera_processes_started':0}
write_once('start_receipt_r14.json',(json.dumps(result,ensure_ascii=False,indent=2)+'\n').encode());print(json.dumps(result,ensure_ascii=False),flush=True)

PYREMOTE
