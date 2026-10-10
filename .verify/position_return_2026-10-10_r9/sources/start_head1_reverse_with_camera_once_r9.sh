#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,importlib.util,json,os,subprocess,time
from pathlib import Path
FIELD_STAMP=1791625743426329000  # 仅root新现场事实可替换；草稿不能执行。
base=Path('/home/loosqk/robot-checks/position-return-20261010-r1');phase=base/'head-reverse-r9'
expected={'head1_reverse_observation_r9.py': ('c30a0e129cc3435cc647480200c9a31ab83a72f0828c28caaf2efb60cd7a6aaa', 12658), 'maintenance_bindings_head_reverse_r9.json': ('165fdf7e38f98087a184840380c454ef5c8af986501c5a72b49a67939d1156c4', 25188), 'head1-reverse-observation-r9-plan-only.stdout': ('56073a0a6c9f8d74b3c419498511d832043d8fd8a97d5b40560723d168b58df6', 33181), 'field_confirmation_r9.json': ('92f9e48b491d2de1733b8949a58fc7113f7928b6529e2c653ebe9f7e27cbab18', 704), 'head1-reverse-observation-r9.go.json': ('4877e142cbd26089c49c7c823b6ccdce70b031d94d87a3d9699c3282bfd53f60', 1271)}
expected['capture_parallel_20s_r9.py']=('93bdc2b46008a376fd84a287d6331f1fd1e0297a0aed445796e5944b5c8d68b1', 9706)
for name,(sha,size) in expected.items():
 path=phase/name;raw=path.read_bytes();assert path.is_file() and not path.is_symlink() and hashlib.sha256(raw).hexdigest()==sha and len(raw)==size
script=base/'software-facts-r1/scripts/run_hardware_maintenance.py'
assert hashlib.sha256(script.read_bytes()).hexdigest()=='d97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41'
spec=importlib.util.spec_from_file_location('head1_i2_wire_start_binding',script);m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
bindings=m.strict_json((phase/'maintenance_bindings_head_reverse_r9.json').read_text())
for ref in bindings['refs']:m.verify_ref(ref)
for side,files in bindings['protection'].items():
 for name,ref in files.items():m.verify_protection_file(side,name,ref)
assert Path('/proc/sys/kernel/random/boot_id').read_text().strip()==bindings['host']['boot_id']
for side,ref in bindings['usb'].items():
 device=Path(ref['device']);assert str(device.resolve())==ref['resolved'] and device.stat().st_rdev==ref['rdev'] and device.stat().st_ino==ref['device_inode'] and device.lstat().st_ino==ref['link_inode']
plan=m.strict_json((phase/'head1-reverse-observation-r9-plan-only.stdout').read_text());assert plan['plan_sha256']=='c504ce371a83601936af9490711fbc4ad42cb91d067e95e70d748a4c39de470c' and m.digest(plan['plan'])==plan['plan_sha256']
go_path=phase/'head1-reverse-observation-r9.go.json';go_raw=go_path.read_bytes();assert go_path.is_file() and not go_path.is_symlink()
go=m.strict_json(go_raw.decode());now=time.time_ns();m.validate_go(go,plan['plan'],now)
assert 0<go['expires_at_ns']-go['issued_at_ns']<=600_000_000_000 and 0<=now-FIELD_STAMP<600_000_000_000
assert all(go['physical'][side]['confirmed_at_ns']==FIELD_STAMP for side in ('left','right'))
output=phase/'head1-reverse-observation-r9';record=phase/'head1-reverse-observation-launch-r9';assert not output.exists() and not record.exists()

assert not (phase/'camera-record-r9').exists() and not (phase/'camera-driver-r9.log').exists()
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
camera_launcher=base64.b64decode('IyEvYmluL2Jhc2gKc2V0IC1ldUMKZXhwb3J0IFBZVEhPTkRPTlRXUklURUJZVEVDT0RFPTEKdG11eCBzZXQtb3B0aW9uIC13IC10ICIkVE1VWF9QQU5FIiByZW1haW4tb24tZXhpdCBvbgpleGVjIC9ob21lL2xvb3Nxay8ubG9jYWwvc2hhcmUvdXYvcHl0aG9uL2NweXRob24tMy4xMi4xMy1saW51eC1hYXJjaDY0LWdudS9iaW4vcHl0aG9uMyAvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9oZWFkLXJldmVyc2UtcjkvY2FwdHVyZV9wYXJhbGxlbF8yMHNfcjkucHkgPiAvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9oZWFkLXJldmVyc2UtcjkvY2FtZXJhLWRyaXZlci1yOS5sb2cgMj4mMQo=',validate=True)
camera_identity,camera_tmux=start_tmux('launch_camera_parallel_r9.sh','position-camera-parallel-20261010-r9',camera_launcher)
write_once('camera-pane.txt',(camera_identity+'\n').encode())
ready_path=phase/'camera-record-r9/ready.json';ready=None;limit=time.monotonic_ns()+10_000_000_000
while time.monotonic_ns()<limit:
 if ready_path.exists():
  ready=m.strict_json(ready_path.read_text());break
 time.sleep(0.05)
assert ready is not None and ready['status']=='THREE_CAMERA_VALID_WARMUP_READY'
assert len(ready['captures'])==3 and {v['name'] for v in ready['captures']}=={'sonix-usb-2_1','sonix-usb-2_2','realsense-043323051347-rgb'}
for v in ready['captures']:
 assert v['live'] is True and v['span_us']>=2_000_000 and v['valid_frames']>1
 assert v['latest']['timestamp_monotonic_us']-v['first']['timestamp_monotonic_us']==v['span_us']
 # 相机supervisor对所有自有child先wait后写result；有result立即拒绝，不启动动作。
 assert not (phase/'camera-record-r9/result.json').exists()
 status=Path('/proc')/str(v['pid'])/'status';assert status.is_file()
 assert not any(line.startswith('State:') and 'Z' in line for line in status.read_text().splitlines())
 cmdline=(status.parent/'cmdline').read_bytes();assert b'/usr/bin/v4l2-ctl\0' in cmdline and b'--stream-to=' in cmdline
assert ready['common_valid_span_us']>=2_000_000
assert min(v['latest']['timestamp_monotonic_us'] for v in ready['captures'])-max(v['first']['timestamp_monotonic_us'] for v in ready['captures'])>=2_000_000
capture_start=min(v['capture_started_monotonic_ns'] for v in ready['captures'])
assert ready['recorded_monotonic_ns']-capture_start<=6_000_000_000
probe_argv=['bash',str(base/'normal-read-r4/nonuart_probe_r4.sh')]
started=time.time_ns();child=subprocess.Popen(probe_argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE);probe_out,probe_err=child.communicate();code=child.wait()
write_once('nonuart-during-camera.stdout',probe_out);write_once('nonuart-during-camera.stderr',probe_err)
probe_receipt={'argv':probe_argv,'pid':child.pid,'started_ns':started,'finished_ns':time.time_ns(),'exit_code':code,'wait_returncode':code,'reaped':True,'signals_sent':[]}
write_once('nonuart-during-camera.receipt.json',(json.dumps(probe_receipt,ensure_ascii=False,indent=2)+'\n').encode())
probe=m.strict_json(probe_out.decode());assert code==0 and probe_err==b'' and probe['status']=='PASS_NON_UART_CURRENT_FACTS'
assert len(probe['checks'])==17 and all(v['status']=='PASS' for v in probe['checks'])
assert time.monotonic_ns()-capture_start<=6_000_000_000
for v in ready['captures']:
 assert not (phase/'camera-record-r9/result.json').exists()
 status=Path('/proc')/str(v['pid'])/'status';assert status.is_file()
 assert not any(line.startswith('State:') and 'Z' in line for line in status.read_text().splitlines())
 cmdline=(status.parent/'cmdline').read_bytes();assert b'/usr/bin/v4l2-ctl\0' in cmdline and b'--stream-to=' in cmdline
# 暖机用去的时间不能续现场/GO期限；再次真实校验，失败只等相机自然退出，无motor启动。
now=time.time_ns();m.validate_go(go,plan['plan'],now)
assert 0<FIELD_STAMP<=now and now-FIELD_STAMP<600_000_000_000
assert not output.exists()
launcher=base64.b64decode('IyEvYmluL2Jhc2gKc2V0IC1ldQpleHBvcnQgUFlUSE9ORE9OVFdSSVRFQllURUNPREU9MQp0bXV4IHNldC1vcHRpb24gLXcgLXQgIiRUTVVYX1BBTkUiIHJlbWFpbi1vbi1leGl0IG9uCmV4ZWMgL2hvbWUvbG9vc3FrLy5sb2NhbC9zaGFyZS91di9weXRob24vY3B5dGhvbi0zLjEyLjEzLWxpbnV4LWFhcmNoNjQtZ251L2Jpbi9weXRob24zIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL2hlYWQtcmV2ZXJzZS1yOS9oZWFkMV9yZXZlcnNlX29ic2VydmF0aW9uX3I5LnB5IC0taGVscGVyIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL3Bvc2l0aW9uLXJldHVybi1yNi9wMjRfdHJpYWxfaGVhZDFfd2lyZV9yNi5weSAtLW1haW50ZW5hbmNlIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL3NvZnR3YXJlLWZhY3RzLXIxL3NjcmlwdHMvcnVuX2hhcmR3YXJlX21haW50ZW5hbmNlLnB5IC0tYmluZGluZ3MgL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvaGVhZC1yZXZlcnNlLXI5L21haW50ZW5hbmNlX2JpbmRpbmdzX2hlYWRfcmV2ZXJzZV9yOS5qc29uIC0tb3V0cHV0IC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL2hlYWQtcmV2ZXJzZS1yOS9oZWFkMS1yZXZlcnNlLW9ic2VydmF0aW9uLXI5IC0tcnVuLWlkIGhlYWQxLXJldmVyc2Utb2JzZXJ2YXRpb24tcjkgLS1hcnRpZmFjdC1yb290IC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxIC0tZXhlY3V0ZSAtLWFwcHJvdmFsIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL2hlYWQtcmV2ZXJzZS1yOS9oZWFkMS1yZXZlcnNlLW9ic2VydmF0aW9uLXI5LmdvLmpzb24gPiAvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9oZWFkLXJldmVyc2UtcjkvaGVhZDEtcmV2ZXJzZS1vYnNlcnZhdGlvbi1sYXVuY2gtcjkvZHJpdmVyLmxvZyAyPiYxCg==',validate=True)
motor_identity,motor_tmux=start_tmux('launch_head1_reverse_observation_r9.sh','position-head1-reverse-observation-20261010-r9',launcher)
write_once('motor-pane.txt',(motor_identity+'\n').encode())
result={'status':'STARTED_SINGLE_HEAD1_REVERSE_WITH_SEPARATE_CAMERA','camera_identity':camera_identity,'motor_identity':motor_identity,'camera_tmux':camera_tmux,'motor_tmux':motor_tmux,'warmup_ready':ready,'go_sha256':hashlib.sha256(go_raw).hexdigest(),'go_bytes':len(go_raw),'plan_sha256':go['plan_sha256'],'motor_launcher_sha256':hashlib.sha256(launcher).hexdigest(),'camera_launcher_sha256':hashlib.sha256(camera_launcher).hexdigest(),'camera_work_in_motion_loop':0}
write_once('start_receipt_r9.json',(json.dumps(result,ensure_ascii=False,indent=2)+'\n').encode());print(json.dumps(result,ensure_ascii=False),flush=True)

PYREMOTE
