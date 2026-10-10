#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,importlib.util,json,os,subprocess,time
from pathlib import Path
FIELD_STAMP=1791630439551636000  # 仅root本次新现场事实可替换；草稿不能执行。
FIELD_EXPECTED=('814c5d43d916e455c032abfc6816132edb2957c7bc66455321362f231eb6b1f9', 704)  # root固定新现场回执(sha256,bytes)。
GO_EXPECTED=('19238585e98c69926ac6621a2adfbe56c05d15a651200725d774343cb75d0eb3', 1311)  # root固定本次新GO(sha256,bytes)，不能复用R9。
assert type(FIELD_STAMP) is int and FIELD_STAMP>0 and FIELD_EXPECTED is not None and GO_EXPECTED is not None, '爸爸，R10启动草稿未获root本次新GO，零启动。' 
base=Path('/home/loosqk/robot-checks/position-return-20261010-r1');phase=base/'head-load-observation-r10'
expected={'head1_load_observation_r10.py': ('8855037db957317ea1a5de8d9e517e314878376dc7b3beec505df25352788c31', 15017), 'maintenance_bindings_head_load_r10.json': ('c2416172179df7a4698dc30c17a22e037c3e6ef1af4208d454352ccf5232eb06', 26480), 'full-plan-only.stdout': ('860794e84aaede32f08e00b67904d0b0938781023b9c32b364dad11bce1ee2e4', 36498), 'field_confirmation_r10.json': FIELD_EXPECTED, 'head1-load-observation-r10.go.json': GO_EXPECTED}
for name,(sha,size) in expected.items():
 path=phase/name;raw=path.read_bytes();assert path.is_file() and not path.is_symlink() and hashlib.sha256(raw).hexdigest()==sha and len(raw)==size
script=base/'software-facts-r1/scripts/run_hardware_maintenance.py'
assert hashlib.sha256(script.read_bytes()).hexdigest()=='d97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41'
spec=importlib.util.spec_from_file_location('head1_i2_wire_start_binding',script);m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
bindings=m.strict_json((phase/'maintenance_bindings_head_load_r10.json').read_text())
for ref in bindings['refs']:m.verify_ref(ref)
for side,files in bindings['protection'].items():
 for name,ref in files.items():m.verify_protection_file(side,name,ref)
assert Path('/proc/sys/kernel/random/boot_id').read_text().strip()==bindings['host']['boot_id']
for side,ref in bindings['usb'].items():
 device=Path(ref['device']);assert str(device.resolve())==ref['resolved'] and device.stat().st_rdev==ref['rdev'] and device.stat().st_ino==ref['device_inode'] and device.lstat().st_ino==ref['link_inode']
load_tool=bindings['load_tool'];assert load_tool['sha256']=='2949ccb2a1735ee2091b2c562c428500461e31fbf5ec967a9c543eec0b1016f3' and load_tool['bytes']==1514376 and load_tool in bindings['refs'] and load_tool['path'] in bindings['binary_paths'];m.verify_ref(load_tool,executable=True)
plan=m.strict_json((phase/'full-plan-only.stdout').read_text());assert plan['plan_sha256']=='258aa3953afc67cde3cfd4f836749181892914dcc487ea78c2e554cee16209a5' and m.digest(plan['plan'])==plan['plan_sha256']
go_path=phase/'head1-load-observation-r10.go.json';go_raw=go_path.read_bytes();assert go_path.is_file() and not go_path.is_symlink()
go=m.strict_json(go_raw.decode());now=time.time_ns();m.validate_go(go,plan['plan'],now)
assert 0<go['expires_at_ns']-go['issued_at_ns']<=600_000_000_000 and 0<=now-FIELD_STAMP<600_000_000_000
assert all(go['physical'][side]['confirmed_at_ns']==FIELD_STAMP for side in ('left','right'))
output=phase/'head1-load-observation-r10';record=phase/'head1-load-observation-launch-r10';assert not output.exists() and not record.exists()

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
launcher=base64.b64decode('IyEvYmluL2Jhc2gKc2V0IC1ldQpleHBvcnQgUFlUSE9ORE9OVFdSSVRFQllURUNPREU9MQp0bXV4IHNldC1vcHRpb24gLXcgLXQgIiRUTVVYX1BBTkUiIHJlbWFpbi1vbi1leGl0IG9uCmV4ZWMgL2hvbWUvbG9vc3FrLy5sb2NhbC9zaGFyZS91di9weXRob24vY3B5dGhvbi0zLjEyLjEzLWxpbnV4LWFhcmNoNjQtZ251L2Jpbi9weXRob24zIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL2hlYWQtbG9hZC1vYnNlcnZhdGlvbi1yMTAvaGVhZDFfbG9hZF9vYnNlcnZhdGlvbl9yMTAucHkgLS1oZWxwZXIgL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvcG9zaXRpb24tcmV0dXJuLXI2L3AyNF90cmlhbF9oZWFkMV93aXJlX3I2LnB5IC0tcmV2ZXJzZS1oZWxwZXIgL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvaGVhZC1yZXZlcnNlLXI5L2hlYWQxX3JldmVyc2Vfb2JzZXJ2YXRpb25fcjkucHkgLS1tYWludGVuYW5jZSAvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9zb2Z0d2FyZS1mYWN0cy1yMS9zY3JpcHRzL3J1bl9oYXJkd2FyZV9tYWludGVuYW5jZS5weSAtLWJpbmRpbmdzIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL2hlYWQtbG9hZC1vYnNlcnZhdGlvbi1yMTAvbWFpbnRlbmFuY2VfYmluZGluZ3NfaGVhZF9sb2FkX3IxMC5qc29uIC0tb3V0cHV0IC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL2hlYWQtbG9hZC1vYnNlcnZhdGlvbi1yMTAvaGVhZDEtbG9hZC1vYnNlcnZhdGlvbi1yMTAgLS1ydW4taWQgaGVhZDEtbG9hZC1vYnNlcnZhdGlvbi1yMTAgLS1hcnRpZmFjdC1yb290IC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxIC0tZXhlY3V0ZSAtLWFwcHJvdmFsIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL2hlYWQtbG9hZC1vYnNlcnZhdGlvbi1yMTAvaGVhZDEtbG9hZC1vYnNlcnZhdGlvbi1yMTAuZ28uanNvbiA+IC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL2hlYWQtbG9hZC1vYnNlcnZhdGlvbi1yMTAvaGVhZDEtbG9hZC1vYnNlcnZhdGlvbi1sYXVuY2gtcjEwL2RyaXZlci5sb2cgMj4mMQo=',validate=True)
motor_identity,motor_tmux=start_tmux('launch_head1_load_observation_r10.sh','position-head1-load-observation-20261010-r10',launcher)
write_once('motor-pane.txt',(motor_identity+'\n').encode())
result={'status':'STARTED_SINGLE_HEAD1_LOAD_OBSERVATION','tmux_identity':motor_identity,'motor_identity':motor_identity,'motor_tmux':motor_tmux,'go_sha256':hashlib.sha256(go_raw).hexdigest(),'go_bytes':len(go_raw),'plan_sha256':go['plan_sha256'],'motor_launcher_sha256':hashlib.sha256(launcher).hexdigest(),'camera_processes_started':0}
write_once('start_receipt_r10.json',(json.dumps(result,ensure_ascii=False,indent=2)+'\n').encode());print(json.dumps(result,ensure_ascii=False),flush=True)

PYREMOTE
