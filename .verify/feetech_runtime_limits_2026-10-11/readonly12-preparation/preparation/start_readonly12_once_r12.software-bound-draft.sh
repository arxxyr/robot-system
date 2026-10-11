#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,importlib.util,json,os,stat,subprocess,time
from pathlib import Path
base=Path('/home/loosqk/robot-checks/position-return-20261010-r1');phase=base/'runtime-limits-r12'
# 本稿无执行授权。root收到当次人类事实后，须固定最终完整config及新GO摘要；不能复用当前None草稿。
APPROVED_FINAL_CONFIG=None
APPROVED_FINAL_PLAN_SHA256=None
APPROVED_GO_REF=None
APPROVED_FIELD_REF=None
assert isinstance(APPROVED_FINAL_CONFIG,dict) and isinstance(APPROVED_FINAL_PLAN_SHA256,str) and isinstance(APPROVED_GO_REF,dict),'root尚未固定本轮最终config/plan，零tmux启动拒绝'
APPROVED_CANDIDATE_REF={'path': '/home/loosqk/robot-checks/position-return-20261010-r1/runtime-limits-r12/readonly12_r12.py', 'sha256': '827e83378949f6f2dd458a113e4e3b122f74689ddf4fdaf6dbe3a70fda92938b', 'bytes': 17307}
APPROVED_BINDINGS_REF={'path': '/home/loosqk/robot-checks/position-return-20261010-r1/runtime-limits-r12/readonly12_bindings_r12.json', 'sha256': 'd6660df1a7693328dd13f87a7edcaa1e53222988ac42e57912a96429c4c6673d', 'bytes': 24253}
assert isinstance(APPROVED_CANDIDATE_REF,dict) and isinstance(APPROVED_BINDINGS_REF,dict) and isinstance(APPROVED_FIELD_REF,dict),'本轮候选/完整绑定/新现场引用尚未固定，零tmux启动拒绝'
expected={'readonly12_r12.py':(APPROVED_CANDIDATE_REF['sha256'],APPROVED_CANDIDATE_REF['bytes']),
          'readonly12_bindings_r12.json':(APPROVED_BINDINGS_REF['sha256'],APPROVED_BINDINGS_REF['bytes'])}
for name,(sha,size) in expected.items():
 path=phase/name;raw=path.read_bytes();assert path==path.resolve() and stat.S_ISREG(path.lstat().st_mode) and len(raw)==size and hashlib.sha256(raw).hexdigest()==sha
config_path=phase/'config.readonly12.r12.json';raw=config_path.read_bytes()
assert APPROVED_FINAL_CONFIG['path']==str(config_path) and stat.S_ISREG(config_path.lstat().st_mode) and len(raw)==APPROVED_FINAL_CONFIG['bytes'] and hashlib.sha256(raw).hexdigest()==APPROVED_FINAL_CONFIG['sha256']
candidate=phase/'readonly12_r12.py';spec=importlib.util.spec_from_file_location('readonly12_fixed_candidate',candidate);c=importlib.util.module_from_spec(spec);spec.loader.exec_module(c)
config=json.loads(raw);h=c.load_support(config['support']);m=h.load_source(config['maintenance']);config=m.strict_json(raw.decode())
bindings=m.strict_json(m.verify_ref(config['bindings']).decode());assert config['bindings']['path']==str(phase/'readonly12_bindings_r12.json') and config['bindings']['sha256']==expected['readonly12_bindings_r12.json'][0]
assert config['run_id']=='readonly-runtime-limits-r12' and config['artifact_root']==str(base) and config['output']==str(phase/'readonly12-evidence-r12')
assert config['field_confirmation']==APPROVED_FIELD_REF and config['approval']==APPROVED_GO_REF
m.verify_ref(APPROVED_FIELD_REF);m.verify_ref(APPROVED_GO_REF)
field=c.confirmation(m,config['field_confirmation'],time.time_ns())
plan=c.make_plan(m,config,APPROVED_FINAL_CONFIG,c.own_reference(),bindings)
assert m.digest(plan)==APPROVED_FINAL_PLAN_SHA256 and plan['register_writes']==plan['motion_commands']==0 and len(plan['fixed_reads'])==12
go_raw=m.verify_ref(config['approval']);go=m.strict_json(go_raw.decode());m.validate_go(go,plan,time.time_ns())
h.host_identity(bindings);m.protected_and_refs(bindings);c.tool_identity(m,config['diagnostic_tool'])
for side,ref in bindings['usb'].items():
 device=Path(ref['device']);assert str(device.resolve())==ref['resolved'] and device.stat().st_rdev==ref['rdev'] and device.stat().st_ino==ref['device_inode'] and device.lstat().st_ino==ref['link_inode']
# 全部无UART核验后再检查原人类时间与原GO，不续期。
c.confirmation(m,config['field_confirmation'],time.time_ns());m.validate_go(go,plan,time.time_ns())
output=phase/'readonly12-evidence-r12';record=phase/'readonly12-launch-record-r12';assert not output.exists() and not record.exists()
launcher=base64.b64decode('IyEvYmluL2Jhc2gKc2V0IC1ldQp1bWFzayAwNzcKZXhwb3J0IFBZVEhPTkRPTlRXUklURUJZVEVDT0RFPTEKdG11eCBzZXQtb3B0aW9uIC1wIHJlbWFpbi1vbi1leGl0IG9uCmV4ZWMgL2hvbWUvbG9vc3FrLy5sb2NhbC9zaGFyZS91di9weXRob24vY3B5dGhvbi0zLjEyLjEzLWxpbnV4LWFhcmNoNjQtZ251L2Jpbi9weXRob24zIC9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL3J1bnRpbWUtbGltaXRzLXIxMi9yZWFkb25seTEyX3IxMi5weSAtLWNvbmZpZyAvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9ydW50aW1lLWxpbWl0cy1yMTIvY29uZmlnLnJlYWRvbmx5MTIucjEyLmpzb24gLS1leGVjdXRlID4gL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvcnVudGltZS1saW1pdHMtcjEyL3JlYWRvbmx5MTItbGF1bmNoLXJlY29yZC1yMTIvZHJpdmVyLmxvZyAyPiYxCg==',validate=True)
assert hashlib.sha256(launcher).hexdigest()=='3a517a053183a9481869b5be5d6ec9398ab4e276e0a4d406839b149fd54351c2'
record.mkdir(mode=0o700)
launch_path=record/'launch_readonly12_once_r12.sh'
with launch_path.open('xb') as f:f.write(launcher);f.flush();os.fsync(f.fileno())
assert launch_path.read_bytes()==launcher
argv=['tmux','new-session','-d','-s','position-readonly12-20261011-r12','-P','-F','#{session_name} #{window_id} #{pane_id} #{pane_pid}','bash '+str(launch_path)]
started=time.time_ns();child=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE);stdout,stderr=child.communicate();code=child.wait()
result={'status':'STARTED_SINGLE_READONLY12_R12' if code==0 else 'TMUX_START_FAIL','tmux_identity':stdout.decode().strip(),'tmux_launch':{'argv':argv,'pid':child.pid,'started_ns':started,'finished_ns':time.time_ns(),'exit_code':code,'wait_returncode':code,'reaped':True,'stdout':stdout.decode(),'stderr':stderr.decode(),'signals_sent':[]},'go_sha256':hashlib.sha256(go_raw).hexdigest(),'go_bytes':len(go_raw),'launcher_sha256':hashlib.sha256(launcher).hexdigest(),'plan_sha256':m.digest(plan),'config_ref':APPROVED_FINAL_CONFIG}
with (record/'start_receipt_r12.json').open('x') as f:json.dump(result,f,ensure_ascii=False,indent=2);f.write('\n');f.flush();os.fsync(f.fileno())
print(json.dumps(result,ensure_ascii=False),flush=True)
assert code==0 and not stderr

PYREMOTE
