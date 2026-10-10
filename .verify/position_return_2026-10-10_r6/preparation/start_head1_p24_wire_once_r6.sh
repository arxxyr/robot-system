#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,importlib.util,json,os,subprocess,time
from pathlib import Path
base=Path('/home/loosqk/robot-checks/position-return-20261010-r1');phase=base/'position-return-r6'
expected={'p24_trial_head1_wire_r6.py':('939e590b9e0ef829a465e9b3bc620d14cb114c01f7c2f2d591d3d34db3c47c81',12425),'maintenance_bindings_p24_wire_r6.json':('f7649ee42c25494b6238f85606b414be46735aeec66dbda57797400d5b5b299d',24951),'head1-p24-wire-plan-only.stdout':('5e88ef4d9c9ee72aa6f4b4a6e56cb86cd521817ce706efcaf093eb5e9f04644e',32330),'field_confirmation_r6.json':('d4273c4a3377f06016ea130bec28607ec9552f7cbf97932255e8fe4826c1819b',573)}
for name,(sha,size) in expected.items():
 path=phase/name;raw=path.read_bytes();assert path.is_file() and not path.is_symlink() and hashlib.sha256(raw).hexdigest()==sha and len(raw)==size
script=base/'software-facts-r1/scripts/run_hardware_maintenance.py'
assert hashlib.sha256(script.read_bytes()).hexdigest()=='d97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41'
spec=importlib.util.spec_from_file_location('head1_i2_wire_start_binding',script);m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
bindings=m.strict_json((phase/'maintenance_bindings_p24_wire_r6.json').read_text())
for ref in bindings['refs']:m.verify_ref(ref)
for side,files in bindings['protection'].items():
 for name,ref in files.items():m.verify_protection_file(side,name,ref)
assert Path('/proc/sys/kernel/random/boot_id').read_text().strip()==bindings['host']['boot_id']
for side,ref in bindings['usb'].items():
 device=Path(ref['device']);assert str(device.resolve())==ref['resolved'] and device.stat().st_rdev==ref['rdev'] and device.stat().st_ino==ref['device_inode'] and device.lstat().st_ino==ref['link_inode']
plan=m.strict_json((phase/'head1-p24-wire-plan-only.stdout').read_text());assert plan['plan_sha256']=='2807016c97c9db6637931662c162d008c7188d95aedfab61c6e751975b881852' and m.digest(plan['plan'])==plan['plan_sha256']
go_path=phase/'head1-p24-wire-r6.go.json';go_raw=go_path.read_bytes();assert go_path.is_file() and not go_path.is_symlink()
go=m.strict_json(go_raw.decode());now=time.time_ns();m.validate_go(go,plan['plan'],now)
assert 0<go['expires_at_ns']-go['issued_at_ns']<=600_000_000_000 and 0<=now-1791620690978077000<600_000_000_000
assert all(go['physical'][side]['confirmed_at_ns']==1791620690978077000 for side in ('left','right'))
output=phase/'head1-p24-wire-r6';record=phase/'head1-p24-wire-launch-r6';assert not output.exists() and not record.exists()
launcher=base64.b64decode('IyEvYmluL2Jhc2gKc2V0IC1ldQpleHBvcnQgUFlUSE9ORE9OVFdSSVRFQllURUNPREU9MQp0bXV4IHNldC1vcHRpb24gLXcgLXQgIiRUTVVYX1BBTkUiIHJlbWFpbi1vbi1leGl0IG9uCmV4ZWMgL2hvbWUvbG9vc3FrLy5sb2NhbC9zaGFyZS91di9weXRob24vY3B5dGhvbi0zLjEyLjEzLWxpbnV4LWFhcmNoNjQtZ251L2Jpbi9weXRob24zIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL3Bvc2l0aW9uLXJldHVybi1yNi9wMjRfdHJpYWxfaGVhZDFfd2lyZV9yNi5weSAtLW1haW50ZW5hbmNlIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL3NvZnR3YXJlLWZhY3RzLXIxL3NjcmlwdHMvcnVuX2hhcmR3YXJlX21haW50ZW5hbmNlLnB5IC0tYmluZGluZ3MgL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvcG9zaXRpb24tcmV0dXJuLXI2L21haW50ZW5hbmNlX2JpbmRpbmdzX3AyNF93aXJlX3I2Lmpzb24gLS1vdXRwdXQgL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvcG9zaXRpb24tcmV0dXJuLXI2L2hlYWQxLXAyNC13aXJlLXI2IC0tcnVuLWlkIGhlYWQxLXAyNC13aXJlLXI2IC0tYXJ0aWZhY3Qtcm9vdCAvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMSAtLWV4ZWN1dGUgLS1hcHByb3ZhbCAvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9wb3NpdGlvbi1yZXR1cm4tcjYvaGVhZDEtcDI0LXdpcmUtcjYuZ28uanNvbiA+IC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL3Bvc2l0aW9uLXJldHVybi1yNi9oZWFkMS1wMjQtd2lyZS1sYXVuY2gtcjYvZHJpdmVyLmxvZyAyPiYxCg==',validate=True)
record.mkdir(mode=0o700)
with (record/'launch_head1_p24_wire_r6.sh').open('xb') as stream:stream.write(launcher);stream.flush();os.fsync(stream.fileno())
assert (record/'launch_head1_p24_wire_r6.sh').read_bytes()==launcher
argv=['tmux','new-session','-d','-s','position-head1-p24-wire-20261010-r6','-P','-F','#{session_name} #{window_id} #{pane_id} #{pane_pid}','bash '+str(record/'launch_head1_p24_wire_r6.sh')]
started=time.time_ns();child=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE);stdout,stderr=child.communicate();code=child.wait()
result={'status':'STARTED_SINGLE_HEAD1_P24_WIRE' if code==0 else 'TMUX_START_FAIL','tmux_identity':stdout.decode().strip(),'tmux_launch':{'argv':argv,'pid':child.pid,'started_ns':started,'finished_ns':time.time_ns(),'exit_code':code,'wait_returncode':code,'reaped':True,'stdout':stdout.decode(),'stderr':stderr.decode(),'signals_sent':[]},'go_sha256':hashlib.sha256(go_raw).hexdigest(),'go_bytes':len(go_raw),'launcher_sha256':hashlib.sha256(launcher).hexdigest(),'plan_sha256':go['plan_sha256']}
with (record/'start_receipt_r6.json').open('x') as stream:json.dump(result,stream,ensure_ascii=False,indent=2);stream.write('\n');stream.flush();os.fsync(stream.fileno())
print(json.dumps(result,ensure_ascii=False),flush=True)
assert code==0 and stderr==b''

PYREMOTE
