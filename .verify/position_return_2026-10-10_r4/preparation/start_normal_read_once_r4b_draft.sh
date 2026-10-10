#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,json,os,subprocess,time
from pathlib import Path
phase=Path('/home/loosqk/robot-checks/position-return-20261010-r1/normal-read-r4')
for name,sha,size in [('normal_read_capture_r4.py','5f57211caa487717672e317b9441ae5e4a77983bb28899c1fac3e081ab3f9bd7',14217),('config.live.r4b.json','324f8485e9239d689923f4a136d6149b5382029065d8f4843536c04739d6cb3f',1109),('field_confirmation_r4.json','1fb512645eb4399a1d0603627fe579a09344dc5bd98f437883e6451d22e81383',601),('normal_read_execution_bindings_r4b.json','42e8a811c2a9eac7f89ff41a1f72aaa90e6a96ce6cd2ab5f8311bf1b179e7789',23460)]:
 data=(phase/name).read_bytes();assert len(data)==size and hashlib.sha256(data).hexdigest()==sha
field=json.loads((phase/'field_confirmation_r4.json').read_bytes());now=time.time_ns();assert 0<=now-field['recorded_at_ns']<=600_000_000_000
assert not (phase/'evidence-r4b').exists() and not (phase/'evidence-r4b').is_symlink()
record=phase/'launch-record-r4b';record.mkdir(mode=0o700)
launcher=base64.b64decode('IyEvYmluL2Jhc2gKc2V0IC1ldQpleHBvcnQgUFlUSE9ORE9OVFdSSVRFQllURUNPREU9MQp0bXV4IHNldC1vcHRpb24gLXcgLXQgIiRUTVVYX1BBTkUiIHJlbWFpbi1vbi1leGl0IG9uCmV4ZWMgL2hvbWUvbG9vc3FrLy5sb2NhbC9zaGFyZS91di9weXRob24vY3B5dGhvbi0zLjEyLjEzLWxpbnV4LWFhcmNoNjQtZ251L2Jpbi9weXRob24zIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL25vcm1hbC1yZWFkLXI0L25vcm1hbF9yZWFkX2NhcHR1cmVfcjQucHkgLS1jb25maWcgL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvbm9ybWFsLXJlYWQtcjQvY29uZmlnLmxpdmUucjRiLmpzb24gLS1leGVjdXRlID4gL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvbm9ybWFsLXJlYWQtcjQvbGF1bmNoLXJlY29yZC1yNGIvZHJpdmVyLmxvZyAyPiYxCg==',validate=True)
with (record/'launch.sh').open('xb') as f:f.write(launcher);f.flush();os.fsync(f.fileno())
assert (record/'launch.sh').read_bytes()==launcher
argv=['tmux','new-session','-d','-s','position-normal-read-20261010-r4b','-P','-F','#{session_name} #{window_id} #{pane_id} #{pane_pid}','bash '+str(record/'launch.sh')]
started=time.time_ns();child=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE);out,err=child.communicate();code=child.wait()
row={'status':'SINGLE_NORMAL_READ_R4B_LAUNCH','argv':argv,'pid':child.pid,'started_ns':started,'finished_ns':time.time_ns(),'exit_code':code,'wait_returncode':code,'reaped':True,'stdout':out.decode(),'stderr':err.decode(),'signals_sent':[],'field_checked_at_ns':now,'field_recorded_at_ns':field['recorded_at_ns'],'launcher_sha256':hashlib.sha256(launcher).hexdigest()}
with (record/'start_receipt.json').open('x') as f:json.dump(row,f,ensure_ascii=False,indent=2);f.write('\n')
print(json.dumps(row,ensure_ascii=False),flush=True)
raise SystemExit(code)
PYREMOTE
