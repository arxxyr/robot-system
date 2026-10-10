#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,importlib.util,json,os,subprocess,time
from pathlib import Path
base=Path('/home/loosqk/robot-checks/position-return-20261010-r1');phase=base/'position-return-r5/wire-r2'
expected={'i2_trial_head1_wire_r5.py':('d74d390848e180f89ef27bc7123d3e026067af3f209d7ee9400d36ed88d38caf',12349),'maintenance_bindings_i2_wire_r5.json':('fba1705dafac62ab8ff63ad1d9a182740ecdf61946f7d6b88b1fbdd28c264daf',24716),'head1-i2-wire-plan-only.stdout':('125fcdda166f9a1d0ccf89801388c70adb50e0b3727c7f203cfc2d8f53314dac',32066),'field_confirmation_r5.json':('6ce3ad99b68a65b3b20a0b86855c11c32937ef42d35dfec57dcb461489a22637',748)}
for name,(sha,size) in expected.items():
 path=phase/name;raw=path.read_bytes();assert path.is_file() and not path.is_symlink() and hashlib.sha256(raw).hexdigest()==sha and len(raw)==size
script=base/'software-facts-r1/scripts/run_hardware_maintenance.py'
assert hashlib.sha256(script.read_bytes()).hexdigest()=='d97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41'
spec=importlib.util.spec_from_file_location('head1_i2_wire_start_binding',script);m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
bindings=m.strict_json((phase/'maintenance_bindings_i2_wire_r5.json').read_text())
for ref in bindings['refs']:m.verify_ref(ref)
for side,files in bindings['protection'].items():
 for name,ref in files.items():m.verify_protection_file(side,name,ref)
assert Path('/proc/sys/kernel/random/boot_id').read_text().strip()==bindings['host']['boot_id']
for side,ref in bindings['usb'].items():
 device=Path(ref['device']);assert str(device.resolve())==ref['resolved'] and device.stat().st_rdev==ref['rdev'] and device.stat().st_ino==ref['device_inode'] and device.lstat().st_ino==ref['link_inode']
plan=m.strict_json((phase/'head1-i2-wire-plan-only.stdout').read_text());assert plan['plan_sha256']=='152db1499304d154abc5ed6c45a04bfc63e70701a7070ffab40b07cff471565a' and m.digest(plan['plan'])==plan['plan_sha256']
go_path=phase/'head1-i2-wire-r5.go.json';go_raw=go_path.read_bytes();assert go_path.is_file() and not go_path.is_symlink()
go=m.strict_json(go_raw.decode());now=time.time_ns();m.validate_go(go,plan['plan'],now)
assert 0<go['expires_at_ns']-go['issued_at_ns']<=600_000_000_000 and 0<=now-1791619988515064000<600_000_000_000
assert all(go['physical'][side]['confirmed_at_ns']==1791619988515064000 for side in ('left','right'))
output=phase/'head1-i2-wire-r5';record=phase/'head1-i2-wire-launch-r5';assert not output.exists() and not record.exists()
launcher=base64.b64decode('IyEvYmluL2Jhc2gKc2V0IC1ldQpleHBvcnQgUFlUSE9ORE9OVFdSSVRFQllURUNPREU9MQp0bXV4IHNldC1vcHRpb24gLXcgLXQgIiRUTVVYX1BBTkUiIHJlbWFpbi1vbi1leGl0IG9uCmV4ZWMgL2hvbWUvbG9vc3FrLy5sb2NhbC9zaGFyZS91di9weXRob24vY3B5dGhvbi0zLjEyLjEzLWxpbnV4LWFhcmNoNjQtZ251L2Jpbi9weXRob24zIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL3Bvc2l0aW9uLXJldHVybi1yNS93aXJlLXIyL2kyX3RyaWFsX2hlYWQxX3dpcmVfcjUucHkgLS1tYWludGVuYW5jZSAvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9zb2Z0d2FyZS1mYWN0cy1yMS9zY3JpcHRzL3J1bl9oYXJkd2FyZV9tYWludGVuYW5jZS5weSAtLWJpbmRpbmdzIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL3Bvc2l0aW9uLXJldHVybi1yNS93aXJlLXIyL21haW50ZW5hbmNlX2JpbmRpbmdzX2kyX3dpcmVfcjUuanNvbiAtLW91dHB1dCAvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9wb3NpdGlvbi1yZXR1cm4tcjUvd2lyZS1yMi9oZWFkMS1pMi13aXJlLXI1IC0tcnVuLWlkIGhlYWQxLWkyLXdpcmUtcjUgLS1hcnRpZmFjdC1yb290IC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxIC0tZXhlY3V0ZSAtLWFwcHJvdmFsIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL3Bvc2l0aW9uLXJldHVybi1yNS93aXJlLXIyL2hlYWQxLWkyLXdpcmUtcjUuZ28uanNvbiA+IC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL3Bvc2l0aW9uLXJldHVybi1yNS93aXJlLXIyL2hlYWQxLWkyLXdpcmUtbGF1bmNoLXI1L2RyaXZlci5sb2cgMj4mMQo=',validate=True)
record.mkdir(mode=0o700)
with (record/'launch_head1_i2_wire_r5.sh').open('xb') as stream:stream.write(launcher);stream.flush();os.fsync(stream.fileno())
assert (record/'launch_head1_i2_wire_r5.sh').read_bytes()==launcher
argv=['tmux','new-session','-d','-s','position-head1-i2-wire-20261010-r5','-P','-F','#{session_name} #{window_id} #{pane_id} #{pane_pid}','bash '+str(record/'launch_head1_i2_wire_r5.sh')]
started=time.time_ns();child=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE);stdout,stderr=child.communicate();code=child.wait()
result={'status':'STARTED_SINGLE_HEAD1_I2_WIRE' if code==0 else 'TMUX_START_FAIL','tmux_identity':stdout.decode().strip(),'tmux_launch':{'argv':argv,'pid':child.pid,'started_ns':started,'finished_ns':time.time_ns(),'exit_code':code,'wait_returncode':code,'reaped':True,'stdout':stdout.decode(),'stderr':stderr.decode(),'signals_sent':[]},'go_sha256':hashlib.sha256(go_raw).hexdigest(),'go_bytes':len(go_raw),'launcher_sha256':hashlib.sha256(launcher).hexdigest(),'plan_sha256':go['plan_sha256']}
with (record/'start_receipt_r5.json').open('x') as stream:json.dump(result,stream,ensure_ascii=False,indent=2);stream.write('\n');stream.flush();os.fsync(stream.fileno())
print(json.dumps(result,ensure_ascii=False),flush=True)
assert code==0 and stderr==b''

PYREMOTE
