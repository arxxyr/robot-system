#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,importlib.util,json,os,stat,subprocess,time
from pathlib import Path
base=Path('/home/loosqk/robot-checks/position-return-20261010-r1');phase=base/'register-diagnostic-r7'
# 本稿无执行授权。root收到当次人类事实后，须固定最终完整config及新GO摘要；不能复用当前None草稿。
APPROVED_FINAL_CONFIG={'path': '/home/loosqk/robot-checks/position-return-20261010-r1/register-diagnostic-r7/config.readonly15.r7.json', 'sha256': '052a5c853be03c0b53f40f3d7c4dd0d0c1ce8519bd8c33f1ba40494adab537c8', 'bytes': 1915}
APPROVED_FINAL_PLAN_SHA256='fe535d70c29ec3ebf87f9c78679e56968607f60c33b45ad446b33f7c2c29dff4'
APPROVED_GO_REF={'path': '/home/loosqk/robot-checks/position-return-20261010-r1/register-diagnostic-r7/readonly15-r7.go.json', 'sha256': 'ea1ec7e8eef5b53dfca970acc53d492e96a5cc2487c4f2f0ada0de99fb3085e1', 'bytes': 1195}
APPROVED_FIELD_REF={'path':'/home/loosqk/robot-checks/position-return-20261010-r1/register-diagnostic-r7/field_confirmation_r7.json','sha256':'ba08d860abdff01517bfb307743f2881a8c149ee39f9ec2920d8400557f8e68a','bytes':520}
assert isinstance(APPROVED_FINAL_CONFIG,dict) and isinstance(APPROVED_FINAL_PLAN_SHA256,str) and isinstance(APPROVED_GO_REF,dict),'root尚未固定本轮最终config/plan，零tmux启动拒绝'
expected={'readonly15_r7.py':('189bb003f9ddd5abc0897c88d8d8dc301f4528a2de9fc68dbcc816b6d4ce927d',17224),
          'readonly15_bindings_r7.json':('fd7a70909e765f97988ee83ca989deef70404cfd79d3709cfb2f6557339aa123',24248)}
for name,(sha,size) in expected.items():
 path=phase/name;raw=path.read_bytes();assert path==path.resolve() and stat.S_ISREG(path.lstat().st_mode) and len(raw)==size and hashlib.sha256(raw).hexdigest()==sha
config_path=phase/'config.readonly15.r7.json';raw=config_path.read_bytes()
assert APPROVED_FINAL_CONFIG['path']==str(config_path) and stat.S_ISREG(config_path.lstat().st_mode) and len(raw)==APPROVED_FINAL_CONFIG['bytes'] and hashlib.sha256(raw).hexdigest()==APPROVED_FINAL_CONFIG['sha256']
candidate=phase/'readonly15_r7.py';spec=importlib.util.spec_from_file_location('readonly15_fixed_candidate',candidate);c=importlib.util.module_from_spec(spec);spec.loader.exec_module(c)
config=json.loads(raw);h=c.load_support(config['support']);m=h.load_source(config['maintenance']);config=m.strict_json(raw.decode())
bindings=m.strict_json(m.verify_ref(config['bindings']).decode());assert config['bindings']['path']==str(phase/'readonly15_bindings_r7.json') and config['bindings']['sha256']==expected['readonly15_bindings_r7.json'][0]
assert config['run_id']=='readonly-parameters-r7' and config['artifact_root']==str(base) and config['output']==str(phase/'readonly15-evidence-r7')
assert config['field_confirmation']==APPROVED_FIELD_REF and config['approval']==APPROVED_GO_REF
m.verify_ref(APPROVED_FIELD_REF);m.verify_ref(APPROVED_GO_REF)
field=c.confirmation(m,config['field_confirmation'],time.time_ns())
plan=c.make_plan(m,config,APPROVED_FINAL_CONFIG,c.own_reference(),bindings)
assert m.digest(plan)==APPROVED_FINAL_PLAN_SHA256 and plan['register_writes']==plan['motion_commands']==0 and len(plan['fixed_reads'])==15
go_raw=m.verify_ref(config['approval']);go=m.strict_json(go_raw.decode());m.validate_go(go,plan,time.time_ns())
h.host_identity(bindings);m.protected_and_refs(bindings);c.tool_identity(m,config['diagnostic_tool'])
for side,ref in bindings['usb'].items():
 device=Path(ref['device']);assert str(device.resolve())==ref['resolved'] and device.stat().st_rdev==ref['rdev'] and device.stat().st_ino==ref['device_inode'] and device.lstat().st_ino==ref['link_inode']
# 全部无UART核验后再检查原人类时间与原GO，不续期。
c.confirmation(m,config['field_confirmation'],time.time_ns());m.validate_go(go,plan,time.time_ns())
output=phase/'readonly15-evidence-r7';record=phase/'readonly15-launch-record-r7';assert not output.exists() and not record.exists()
launcher=base64.b64decode('IyEvYmluL2Jhc2gKc2V0IC1ldQp1bWFzayAwNzcKZXhwb3J0IFBZVEhPTkRPTlRXUklURUJZVEVDT0RFPTEKdG11eCBzZXQtb3B0aW9uIC1wIHJlbWFpbi1vbi1leGl0IG9uCmV4ZWMgL2hvbWUvbG9vc3FrLy5sb2NhbC9zaGFyZS91di9weXRob24vY3B5dGhvbi0zLjEyLjEzLWxpbnV4LWFhcmNoNjQtZ251L2Jpbi9weXRob24zIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL3JlZ2lzdGVyLWRpYWdub3N0aWMtcjcvcmVhZG9ubHkxNV9yNy5weSAtLWNvbmZpZyAvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9yZWdpc3Rlci1kaWFnbm9zdGljLXI3L2NvbmZpZy5yZWFkb25seTE1LnI3Lmpzb24gLS1leGVjdXRlID4gL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvcmVnaXN0ZXItZGlhZ25vc3RpYy1yNy9yZWFkb25seTE1LWxhdW5jaC1yZWNvcmQtcjcvZHJpdmVyLmxvZyAyPiYxCg==',validate=True)
assert hashlib.sha256(launcher).hexdigest()=='d200d9e2f7f1fb8126bd200125dc3a3f6cf3513a02094c84fe538b359c500ccb'
record.mkdir(mode=0o700)
launch_path=record/'launch_readonly15_once_r7.sh'
with launch_path.open('xb') as f:f.write(launcher);f.flush();os.fsync(f.fileno())
assert launch_path.read_bytes()==launcher
argv=['tmux','new-session','-d','-s','position-readonly15-20261010-r7','-P','-F','#{session_name} #{window_id} #{pane_id} #{pane_pid}','bash '+str(launch_path)]
started=time.time_ns();child=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE);stdout,stderr=child.communicate();code=child.wait()
result={'status':'STARTED_SINGLE_READONLY15_R7' if code==0 else 'TMUX_START_FAIL','tmux_identity':stdout.decode().strip(),'tmux_launch':{'argv':argv,'pid':child.pid,'started_ns':started,'finished_ns':time.time_ns(),'exit_code':code,'wait_returncode':code,'reaped':True,'stdout':stdout.decode(),'stderr':stderr.decode(),'signals_sent':[]},'go_sha256':hashlib.sha256(go_raw).hexdigest(),'go_bytes':len(go_raw),'launcher_sha256':hashlib.sha256(launcher).hexdigest(),'plan_sha256':m.digest(plan),'config_ref':APPROVED_FINAL_CONFIG}
with (record/'start_receipt_r7.json').open('x') as f:json.dump(result,f,ensure_ascii=False,indent=2);f.write('\n');f.flush();os.fsync(f.fileno())
print(json.dumps(result,ensure_ascii=False),flush=True)
assert code==0 and not stderr

PYREMOTE
