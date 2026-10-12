"""爸爸，R16复用R14原生软件准备形态；只做内存/PLAN/非UART，不生成GO或现场回执。"""
import hashlib,importlib.util,json,os,subprocess,time
from pathlib import Path
BASE=Path('/home/loosqk/robot-checks/position-return-20261010-r1');PHASE=BASE/'head1-p32-reverse-r16'
PYTHON='/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3'
INPUT_REFS={'candidate/head1_p32_reverse_trial_r16.py': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/head1-p32-reverse-r16/memory-root/candidate/head1_p32_reverse_trial_r16.py', 'sha256': 'cf6dc28a66dbff6677183d15c6cc0568ccf7514f8b58c745758470c5420c7923', 'bytes': 16254}, 'candidate/test_head1_p32_reverse_trial_r16.py': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/head1-p32-reverse-r16/memory-root/candidate/test_head1_p32_reverse_trial_r16.py', 'sha256': '3dfec31c39a0b58cbeea7891a374d0a617b6f86e84e34034090b0b96758da3be', 'bytes': 22885}, 'helpers/head1_outward_observation_r8.py': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/head1-p32-reverse-r16/memory-root/helpers/head1_outward_observation_r8.py', 'sha256': '11b86f6768359bc7f6f0063bd188868b08c8baf164a59d89a62acdb557d7fc5e', 'bytes': 9328}, 'helpers/p24_trial_head1_wire_r6.py': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/head1-p32-reverse-r16/memory-root/helpers/p24_trial_head1_wire_r6.py', 'sha256': '939e590b9e0ef829a465e9b3bc620d14cb114c01f7c2f2d591d3d34db3c47c81', 'bytes': 12425}, 'helpers/test_head1_outward_observation_r8.py': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/head1-p32-reverse-r16/memory-root/helpers/test_head1_outward_observation_r8.py', 'sha256': 'f490470be238efa5fbf75aa533af52dbee65929f904c76c1b9f34ef9c9a70e5f', 'bytes': 10821}, 'helpers/test_p24_head1_wire_r6.py': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/head1-p32-reverse-r16/memory-root/helpers/test_p24_head1_wire_r6.py', 'sha256': '51a6b11212289f7609b68018c51937fce6eabd30a06c45955757253d3186d2e6', 'bytes': 24774}, 'fixtures/head1-d0-trial-r11.go.json': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/head1-p32-reverse-r16/memory-root/fixtures/head1-d0-trial-r11.go.json', 'sha256': 'b02ca867dbe2b68725102a223b9bd4e2dd645ec7d9250fac80c20db3e0bd5a9a', 'bytes': 1308}, 'fixtures/head1-load-observation-r10.go.json': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/head1-p32-reverse-r16/memory-root/fixtures/head1-load-observation-r10.go.json', 'sha256': '19238585e98c69926ac6621a2adfbe56c05d15a651200725d774343cb75d0eb3', 'bytes': 1311}, 'fixtures/head1-outward-observation-r8.go.json': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/head1-p32-reverse-r16/memory-root/fixtures/head1-outward-observation-r8.go.json', 'sha256': '5dc195fffb9e1453547ae93476090a60bd29439e8c17857ede43d14f20d1350b', 'bytes': 1227}, 'fixtures/head1-p24-wire-r6.go.json': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/head1-p32-reverse-r16/memory-root/fixtures/head1-p24-wire-r6.go.json', 'sha256': 'f523c7bf976bf9ae78d18c902ca333f8156e2d1c374e2a9fb61a1b688bd344a6', 'bytes': 1211}, 'fixtures/head1-p32-trial-r13.go.json': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/head1-p32-reverse-r16/memory-root/fixtures/head1-p32-trial-r13.go.json', 'sha256': '943e166438dc063d8d444f2de694025ab4a2dc97982d7cb941c4766d3220fd6b', 'bytes': 1318}, 'fixtures/maintenance_bindings_head_settle_r8.json': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/head1-p32-reverse-r16/memory-root/fixtures/maintenance_bindings_head_settle_r8.json', 'sha256': '43b6f0d3e75db611ec2e64299c4f654b4979fa888cebb5941e75de4bd13b2311', 'bytes': 25186}, 'scripts/run_hardware_maintenance.py': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/head1-p32-reverse-r16/memory-root/scripts/run_hardware_maintenance.py', 'sha256': 'd97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41', 'bytes': 50168}, 'target/position_return_2026-10-10_r1/head-p24-preparation-r6/p24_trial_head1_wire_r6.py': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/head1-p32-reverse-r16/memory-root/target/position_return_2026-10-10_r1/head-p24-preparation-r6/p24_trial_head1_wire_r6.py', 'sha256': '939e590b9e0ef829a465e9b3bc620d14cb114c01f7c2f2d591d3d34db3c47c81', 'bytes': 12425}}

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
candidate=PHASE/'head1_p32_reverse_trial_r16.py';new(candidate,(memory/'candidate/head1_p32_reverse_trial_r16.py').read_bytes())
helper=PHASE/'helpers/p24_trial_head1_wire_r6.py';new(helper,(memory/'helpers/p24_trial_head1_wire_r6.py').read_bytes())
observation=PHASE/'helpers/head1_outward_observation_r8.py';new(observation,(memory/'helpers/head1_outward_observation_r8.py').read_bytes())
maintenance=BASE/'software-facts-r1/scripts/run_hardware_maintenance.py';m=module('r16_frozen_d97',maintenance)
assert ref(maintenance)['sha256']=='d97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41' and ref(maintenance)['bytes']==50168
old_path=Path('/home/loosqk/robot-checks/position-return-20261010-r1/head-p32-trial-r13/maintenance_bindings_head_p32_r13.json')
assert ref(old_path)['sha256']=='26195ada947329f6f74b0796b544323f8b0f1257dc57050586fae6a788002988' and ref(old_path)['bytes']==27768
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
references=[ref(candidate),ref(helper),ref(observation)]
bindings={**old,'refs':[*old['refs'],*references],'binary_paths':[*old['binary_paths'],str(candidate)]}
assert all(bindings[key]==old[key] for key in old if key not in ('refs','binary_paths'))
binding_path=PHASE/'maintenance_bindings_head1_p32_reverse_r16.json';save(binding_path,bindings)
mem,mem_err=command('native-memory10',[PYTHON,str(memory/'candidate/test_head1_p32_reverse_trial_r16.py')]);assert b'Ran 10 tests' in mem_err and b'\nOK\n' in mem_err
normal,normal_err=command('default-plan-only',[PYTHON,str(candidate)]);assert not normal_err
argv=[PYTHON,str(candidate),'--maintenance',str(maintenance),'--helper',str(helper),'--observation-helper',str(observation),'--bindings',str(binding_path),'--output',str(PHASE/'head1-p32-reverse-r16'),'--run-id','head1-p32-reverse-r16','--artifact-root',str(BASE)]
missing,missing_err=command('missing-go',argv+['--execute'],2);assert not missing and '新P32反向功能试验GO'.encode() in missing_err
full,full_err=command('full-plan-only',argv);assert not full_err
normal,full=m.strict_json(normal.decode()),m.strict_json(full.decode());plan=full['plan'];trial=plan['head1_p32_trial']
assert normal['status']==full['status']=='PLAN_ONLY' and normal['actions_started']==normal['parameter_writes']==normal['motion_commands']==normal['load_raw_reads']==0
assert full['plan_sha256']==m.digest(plan) and plan['position_targets']==[['left',7,'head_motor_1']]
assert (plan['position_delta_raw'],plan['minimum_excursion_raw'],plan['return_tolerance_raw'],plan['position_wait_ms'])==(-24,12,4,800)
assert (trial['fixed_d'],trial['fixed_i'],trial['temporary_p'],trial['restore_p'],trial['p_register_address'],trial['p_register_width'],trial['load_raw_reads'])==(43,0,32,16,21,1,0)
assert trial['write_whitelist']=={'raw_left7':{'P_Coefficient':[16,32]},'position_left7':['Goal_Position','Torque_Enable']}
assert not (PHASE/'head1-p32-reverse-r16').exists() and not (PHASE/'head1-p32-reverse-launch-r16').exists()
for row in fixture_refs:m.verify_ref(row)
m.protected_and_refs(bindings)
save(PHASE/'prepare-result-r16.json',{'status':'PASS_R16_NATIVE_SOFTWARE_PREPARATION_ONLY','run_id':'head1-p32-reverse-r16','output':str(PHASE/'head1-p32-reverse-r16'),'plan_sha256':full['plan_sha256'],'plan':ref(PHASE/'full-plan-only.stdout'),'candidate':ref(candidate),'helper':ref(helper),'observation_helper':ref(observation),'bindings':ref(binding_path),'maintenance':ref(maintenance),'fixture_refs':fixture_refs,'direct_children':rows,'nonuart_checks':len(fresh['checks']),'nonuart_commands':len(fresh['commands']),'uart_operations':0,'motor_actions':0,'parameter_writes':0,'go_created':False,'field_confirmation_created':False,'output_created':False,'launch_record_created':False})
print((PHASE/'prepare-result-r16.json').read_text(),end='',flush=True)
