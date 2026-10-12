#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,importlib.util,json,os,stat,subprocess,time
from pathlib import Path
base=Path('/home/loosqk/robot-checks/position-return-20261010-r1');phase=base/'base-firmware-read-r15'
# 本稿无执行授权。root收到当次人类事实后，须固定最终完整config及新GO摘要；不能复用当前None草稿。
APPROVED_FINAL_CONFIG={'path': '/home/loosqk/robot-checks/position-return-20261010-r1/base-firmware-read-r15/config.final.base_firmware_read6.r15.json', 'sha256': '2b29d86740b490b047844b9b594e3bc2de245cae0092672d48b70f6532061511', 'bytes': 1923}
APPROVED_FINAL_PLAN_SHA256='3c78194f6014d46523d86eec49772f98d514e1d40c7a93b710db05fc9b38dd90'
APPROVED_GO_REF={'path': '/home/loosqk/robot-checks/position-return-20261010-r1/base-firmware-read-r15/base_firmware_read6_r15.go.json', 'sha256': '370499711badde765ea2c0f6ba4dfd3a24e60f1aded42e05a5df08fb20b5c7be', 'bytes': 1190}
APPROVED_FIELD_REF={'path': '/home/loosqk/robot-checks/position-return-20261010-r1/base-firmware-read-r15/field_confirmation_r15.json', 'sha256': '852082bbeec8fec92a7557a9c8c2d7828e451970f66b600a0f1e4f0076a35464', 'bytes': 640}
assert isinstance(APPROVED_FINAL_CONFIG,dict) and isinstance(APPROVED_FINAL_PLAN_SHA256,str) and isinstance(APPROVED_GO_REF,dict),'root尚未固定本轮最终config/plan，零tmux启动拒绝'
APPROVED_CANDIDATE_REF={'path': '/home/loosqk/robot-checks/position-return-20261010-r1/base-firmware-read-r15/candidate/base_firmware_read6_r15.py', 'sha256': '39506725bc4008f018e9aa8af615e8f64a6bd3a7974f3433f585599df475b88b', 'bytes': 16721}
APPROVED_BINDINGS_REF={'path': '/home/loosqk/robot-checks/position-return-20261010-r1/base-firmware-read-r15/base_firmware_read6_bindings_r15.json', 'sha256': '34c7523c12ecc04c688738afb4dcd03779d819945c4e03e322bad80346eb4326', 'bytes': 28968}
assert isinstance(APPROVED_CANDIDATE_REF,dict) and isinstance(APPROVED_BINDINGS_REF,dict) and isinstance(APPROVED_FIELD_REF,dict),'本轮候选/完整绑定/新现场引用尚未固定，零tmux启动拒绝'
expected={'candidate/base_firmware_read6_r15.py':(APPROVED_CANDIDATE_REF['sha256'],APPROVED_CANDIDATE_REF['bytes']),
          'base_firmware_read6_bindings_r15.json':(APPROVED_BINDINGS_REF['sha256'],APPROVED_BINDINGS_REF['bytes'])}
for name,(sha,size) in expected.items():
 path=phase/name;raw=path.read_bytes();assert path==path.resolve() and stat.S_ISREG(path.lstat().st_mode) and len(raw)==size and hashlib.sha256(raw).hexdigest()==sha
config_path=phase/'config.final.base_firmware_read6.r15.json';raw=config_path.read_bytes()
assert APPROVED_FINAL_CONFIG['path']==str(config_path) and stat.S_ISREG(config_path.lstat().st_mode) and len(raw)==APPROVED_FINAL_CONFIG['bytes'] and hashlib.sha256(raw).hexdigest()==APPROVED_FINAL_CONFIG['sha256']
candidate=phase/'candidate/base_firmware_read6_r15.py';spec=importlib.util.spec_from_file_location('base_firmware_read6_fixed_candidate',candidate);c=importlib.util.module_from_spec(spec);spec.loader.exec_module(c)
config=json.loads(raw);h=c.load_support(config['support']);m=h.load_source(config['maintenance']);config=m.strict_json(raw.decode())
bindings=m.strict_json(m.verify_ref(config['bindings']).decode());assert config['bindings']['path']==str(phase/'base_firmware_read6_bindings_r15.json') and config['bindings']['sha256']==expected['base_firmware_read6_bindings_r15.json'][0]
assert config['run_id']=='base-firmware-read6-r15' and config['artifact_root']==str(base) and config['output']==str(phase/'read6-evidence-r15')
assert config['field_confirmation']==APPROVED_FIELD_REF and config['approval']==APPROVED_GO_REF
m.verify_ref(APPROVED_FIELD_REF);m.verify_ref(APPROVED_GO_REF)
field=c.confirmation(m,config['field_confirmation'],time.time_ns())
plan=c.make_plan(m,config,APPROVED_FINAL_CONFIG,c.own_reference(),bindings)
assert m.digest(plan)==APPROVED_FINAL_PLAN_SHA256 and plan['register_writes']==plan['motion_commands']==0 and len(plan['fixed_reads'])==6
go_raw=m.verify_ref(config['approval']);go=m.strict_json(go_raw.decode());m.validate_go(go,plan,time.time_ns())
h.host_identity(bindings);m.protected_and_refs(bindings);c.tool_identity(m,config['diagnostic_tool'])
for side,ref in bindings['usb'].items():
 device=Path(ref['device']);assert str(device.resolve())==ref['resolved'] and device.stat().st_rdev==ref['rdev'] and device.stat().st_ino==ref['device_inode'] and device.lstat().st_ino==ref['link_inode']
# 全部无UART核验后再检查原人类时间与原GO，不续期。
c.confirmation(m,config['field_confirmation'],time.time_ns());m.validate_go(go,plan,time.time_ns())
output=phase/'read6-evidence-r15';record=phase/'read6-launch-record-r15';assert not output.exists() and not record.exists()
launcher=b'#!/bin/bash\nset -eu\numask 077\nexport PYTHONDONTWRITEBYTECODE=1\ntmux set-option -p remain-on-exit on\nexec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 /home/loosqk/robot-checks/position-return-20261010-r1/base-firmware-read-r15/candidate/base_firmware_read6_r15.py --config /home/loosqk/robot-checks/position-return-20261010-r1/base-firmware-read-r15/config.final.base_firmware_read6.r15.json --execute > /home/loosqk/robot-checks/position-return-20261010-r1/base-firmware-read-r15/read6-launch-record-r15/driver.log 2>&1\n'
assert hashlib.sha256(launcher).hexdigest()=='181d6229529bffea21551e43e363ce19639d4079fa6e71f26506eb0aa5df18f2'
record.mkdir(mode=0o700)
launch_path=record/'launch_base_firmware_read6_once_r15.sh'
with launch_path.open('xb') as f:f.write(launcher);f.flush();os.fsync(f.fileno())
assert launch_path.read_bytes()==launcher
argv=['tmux','new-session','-d','-s','base-firmware-read6-20261011-r15','-P','-F','#{session_name} #{window_id} #{pane_id} #{pane_pid}','bash '+str(launch_path)]
started=time.time_ns();child=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE);stdout,stderr=child.communicate();code=child.wait()
result={'status':'STARTED_SINGLE_BASE_FIRMWARE_READ6_R15' if code==0 else 'TMUX_START_FAIL','tmux_identity':stdout.decode().strip(),'tmux_launch':{'argv':argv,'pid':child.pid,'started_ns':started,'finished_ns':time.time_ns(),'exit_code':code,'wait_returncode':code,'reaped':True,'stdout':stdout.decode(),'stderr':stderr.decode(),'signals_sent':[]},'go_sha256':hashlib.sha256(go_raw).hexdigest(),'go_bytes':len(go_raw),'launcher_sha256':hashlib.sha256(launcher).hexdigest(),'plan_sha256':m.digest(plan),'config_ref':APPROVED_FINAL_CONFIG}
with (record/'start_receipt_r15.json').open('x') as f:json.dump(result,f,ensure_ascii=False,indent=2);f.write('\n');f.flush();os.fsync(f.fileno())
print(json.dumps(result,ensure_ascii=False),flush=True)
assert code==0 and not stderr

PYREMOTE
