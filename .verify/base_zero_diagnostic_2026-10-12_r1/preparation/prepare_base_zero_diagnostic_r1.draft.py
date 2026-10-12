"""爸爸，底盘零目标R1复用R14原生软件准备形态；只做内存/PLAN/非UART，不生成GO或现场回执。"""
import hashlib,importlib.util,json,os,subprocess,time
from pathlib import Path
BASE=Path('/home/loosqk/robot-checks/position-return-20261010-r1');PHASE=BASE/'base-zero-diagnostic-r1'
PYTHON='/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3'
INPUT_REFS={'candidate/base_zero_diagnostic_r1.py': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/base-zero-diagnostic-r1/memory-root/candidate/base_zero_diagnostic_r1.py', 'sha256': 'f59455a6b25f832efd5b725224d8f16229b421f28c1556cbe4416ed24ac6f314', 'bytes': 19556}, 'candidate/test_base_zero_diagnostic_r1.py': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/base-zero-diagnostic-r1/memory-root/candidate/test_base_zero_diagnostic_r1.py', 'sha256': '304af8245696f60e457da3516b13f15956ac76fd5fd2a29c5f4892c78d6634f7', 'bytes': 18881}, 'candidate/raw_right9_from_r14_r1.py': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/base-zero-diagnostic-r1/memory-root/candidate/raw_right9_from_r14_r1.py', 'sha256': '406295f0bf97ca4a482241a565500d2bae1e226c6a8959bcebbd3264c50b9dfa', 'bytes': 12216}, 'scripts/run_hardware_maintenance.py': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/base-zero-diagnostic-r1/memory-root/scripts/run_hardware_maintenance.py', 'sha256': 'd97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41', 'bytes': 50168}, '.verify/position_return_2026-10-11_r14/helpers/parameter_stop_head2_r14.py': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/base-zero-diagnostic-r1/memory-root/.verify/position_return_2026-10-11_r14/helpers/parameter_stop_head2_r14.py', 'sha256': '0ceb8ed58ea22db416ddce9cf7df6f221c277df1ac619a4df0980e3c23805657', 'bytes': 12425}, 'target/position_return_2026-10-11_r17/native-preparation/maintenance_bindings_head2_p32_reverse_r17.json': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/base-zero-diagnostic-r1/memory-root/target/position_return_2026-10-11_r17/native-preparation/maintenance_bindings_head2_p32_reverse_r17.json', 'sha256': '274a2be1541725aaf18b4fff4b39f1c93f54e31ba6f142b7bb5c47af1e7840ef', 'bytes': 29452}}

assert PHASE.is_dir()  # 爸爸，唯一运输recipe已创建本轮独立phase及输入。
def new(path,raw):
 path.parent.mkdir(parents=True,exist_ok=True)
 with path.open('xb') as f:f.write(raw);f.flush();os.fsync(f.fileno())
def ref(path):
 raw=path.read_bytes();return {'path':str(path),'sha256':hashlib.sha256(raw).hexdigest(),'bytes':len(raw)}
def save(path,value):new(path,(json.dumps(value,ensure_ascii=False,indent=2)+'\n').encode())
def module(name,path):
 spec=importlib.util.spec_from_file_location(name,path);value=importlib.util.module_from_spec(spec);spec.loader.exec_module(value);return value
rows=[]
def command(name,argv,expected=0):
 started=time.time_ns();child=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE);stdout,stderr=child.communicate();code=child.wait()
 row={'name':name,'argv':argv,'pid':child.pid,'started_ns':started,'finished_ns':time.time_ns(),'exit_code':code,'expected_exit_code':expected,'wait_returncode':code,'reaped':True,'signals_sent':[]}
 for suffix,raw in [('stdout',stdout),('stderr',stderr)]:
  path=PHASE/(name+'.'+suffix);new(path,raw);row[suffix]=ref(path)
 save(PHASE/(name+'.receipt.json'),row);rows.append(row);assert code==expected,row
 return stdout,stderr
memory=PHASE/'memory-root'
fixture_refs=[]
for name,row in INPUT_REFS.items():
 path=memory/name;assert ref(path)==row;fixture_refs.append(ref(path))
candidate=PHASE/'base_zero_diagnostic_r1.py';new(candidate,(memory/'candidate/base_zero_diagnostic_r1.py').read_bytes())
helper=PHASE/'helpers/raw_right9_from_r14_r1.py';new(helper,(memory/'candidate/raw_right9_from_r14_r1.py').read_bytes())
maintenance=BASE/'software-facts-r1/scripts/run_hardware_maintenance.py';m=module('base_zero_r1_frozen_d97',maintenance)
assert ref(maintenance)['sha256']=='d97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41' and ref(maintenance)['bytes']==50168
old_path=Path('/home/loosqk/robot-checks/position-return-20261010-r1/head2-p32-reverse-r17/maintenance_bindings_head2_p32_reverse_r17.json')
assert ref(old_path)['sha256']=='274a2be1541725aaf18b4fff4b39f1c93f54e31ba6f142b7bb5c47af1e7840ef' and ref(old_path)['bytes']==29452
old=m.strict_json(old_path.read_text());m.protected_and_refs(old)
fresh_raw,fresh_stderr=command('fresh-nonuart',['bash',str(BASE/'normal-read-r4/nonuart_probe_r4.sh')]);assert not fresh_stderr
fresh=m.strict_json(fresh_raw.decode());assert fresh['status']=='PASS_NON_UART_CURRENT_FACTS' and not fresh['errors']
for key in ('hostname','uid','gid','boot_id'):assert fresh['identity_before'][key]==old['host'][key],('host',key)
for side,record in old['usb'].items():
 actual=fresh['devices'][side]
 for key in ('device','resolved','rdev','device_inode','link_inode','mode','uid','gid','link_target'):assert actual[key]==record[key],('USB',side,key)
 fields=dict(line.split('=',1) for line in actual['udev_command']['stdout'].splitlines() if '=' in line)
 assert fields.get('ID_SERIAL_SHORT')==record['serial'] and fields.get('DEVPATH')==record['devpath']
m.protected_and_refs(old)
references=[ref(candidate),ref(helper)]
bindings={**old,'refs':[*old['refs'],*references],'binary_paths':[*old['binary_paths'],str(candidate)]}
assert all(bindings[key]==old[key] for key in old if key not in ('refs','binary_paths'))
assert len(old['refs'])==53 and len(bindings['refs'])==55
binding_path=PHASE/'maintenance_bindings_base_zero_diagnostic_r1.json';save(binding_path,bindings)
mem,mem_err=command('native-memory10',[PYTHON,str(memory/'candidate/test_base_zero_diagnostic_r1.py')]);assert b'Ran 10 tests' in mem_err and b'\nOK\n' in mem_err
normal,normal_err=command('default-plan-only',[PYTHON,str(candidate)]);assert not normal_err
argv=[PYTHON,str(candidate),'--maintenance',str(maintenance),'--raw-helper',str(helper),'--bindings',str(binding_path),'--output',str(PHASE/'base-zero-diagnostic-r1'),'--artifact-root',str(BASE)]
missing,missing_err=command('missing-go',argv+['--execute'],2);assert not missing and '新架空就绪GO'.encode() in missing_err
full,full_err=command('full-plan-only',argv);assert not full_err
normal,full=m.strict_json(normal.decode()),m.strict_json(full.decode());plan=full['plan'];trial=plan['zero_diag']
assert normal['status']==full['status']=='PLAN_ONLY' and normal['actions_started']==normal['register_writes']==normal['motion_commands']==0
assert full['plan_sha256']==m.digest(plan) and plan['position_targets']==[] and plan['firmware_contract_verified'] is False
assert plan['base_targets']==[{'side':side,'motor':motor,'name':name,'decision':'DEFERRED','reason_code':'BASE_FIRMWARE_CONTRACT_UNVERIFIED'} for side,motor,name in m.BASE_TARGETS]
assert trial['target']==['right',9,'base_right_wheel'] and trial['firmware_contract_verified'] is False
assert (trial['goal_velocity_address'],trial['goal_velocity_width'],trial['budget_ns'],trial['requested_pv_samples'],trial['minimum_in_budget_pv0_samples'])==(46,2,500000000,3,3)
assert trial['sample_offsets_ns']==[0,200000000,400000000] and trial['write_sequence']==[['Goal_Velocity',0],['Torque_Enable',1],['Torque_Enable',0]]
assert not (PHASE/'base-zero-diagnostic-r1').exists() and not (PHASE/'base-zero-diagnostic-launch-r1').exists()
for row in fixture_refs:m.verify_ref(row)
m.protected_and_refs(bindings)
save(PHASE/'prepare-result-base-zero-r1.json',{'status':'PASS_BASE_ZERO_R1_NATIVE_SOFTWARE_PREPARATION_ONLY','run_id':'base-zero-diagnostic-r1','output':str(PHASE/'base-zero-diagnostic-r1'),'plan_sha256':full['plan_sha256'],'plan':ref(PHASE/'full-plan-only.stdout'),'candidate':ref(candidate),'helper':ref(helper),'bindings':ref(binding_path),'maintenance':ref(maintenance),'fixture_refs':fixture_refs,'direct_children':rows,'nonuart_checks':len(fresh['checks']),'nonuart_commands':len(fresh['commands']),'uart_operations':0,'motor_actions':0,'parameter_writes':0,'go_created':False,'field_confirmation_created':False,'output_created':False,'launch_record_created':False})
print((PHASE/'prepare-result-base-zero-r1.json').read_text(),end='',flush=True)
