"""爸爸，仅本地派生本轮启动/观察/收集草稿；不运行产物、实机或GO。"""
import ast
import base64
import difflib
import hashlib
import json
from pathlib import Path
import re

ROOT = Path.cwd()
OLD = ROOT / 'target/position_return_2026-10-11_r17/preparation'
OUT = ROOT / 'target/base_zero_diagnostic_2026-10-12_r1/preparation'
BASE = '/home/loosqk/robot-checks/position-return-20261010-r1'
PHASE = BASE + '/base-zero-diagnostic-r1'
PYTHON = '/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3'
SESSION = 'base-zero-diagnostic-trial-20261012-r1'


def renamed(text):
    for old, new in (
        ('head2-p32-reverse-20261011-r17', 'base-zero-diagnostic-trial-20261012-r1'),
        ('head2-p32-reverse-launch-r17', 'base-zero-diagnostic-launch-r1'),
        ('head2-p32-reverse-seal-r17', 'base-zero-diagnostic-seal-r1'),
        ('head2-p32-reverse-r17', 'base-zero-diagnostic-r1'),
        ('head2_p32_reverse_trial_r17.py', 'base_zero_diagnostic_r1.py'),
        ('maintenance_bindings_head2_p32_reverse_r17.json', 'maintenance_bindings_base_zero_diagnostic_r1.json'),
        ('field_confirmation_r17.json', 'field_confirmation_base_zero_r1.json'),
        ('start_receipt_r17.json', 'start_receipt_base_zero_r1.json'),
        ('observer-event-r17', 'observer-event-base-zero-r1'),
        ('launch_head2_p32_reverse_r17.sh', 'launch_base_zero_diagnostic_r1.sh'),
        ('HEAD2_P32_REVERSE_R17', 'BASE_ZERO_DIAGNOSTIC_R1'),
        ('R17', 'BASE_ZERO_R1'),
    ):
        text = text.replace(old, new)
    return text


launcher = f'''#!/bin/bash
# 爸爸，独立launcher也必须由已核starter填全部本轮pins；纯草稿零I/O拒绝。
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec {PYTHON} - <<'PYLAUNCH'
import os,subprocess
FIELD_STAMP=None
FIELD_EXPECTED=None
GO_EXPECTED=None
BINDING_EXPECTED=None
PLAN_EXPECTED=None
PLAN_CANONICAL_SHA=None
NATIVE_PREPARATION_EXPECTED=None
assert type(FIELD_STAMP) is int and FIELD_STAMP>0 and all(v is not None for v in (FIELD_EXPECTED,GO_EXPECTED,BINDING_EXPECTED,PLAN_EXPECTED,NATIVE_PREPARATION_EXPECTED)) and type(PLAN_CANONICAL_SHA) is str and len(PLAN_CANONICAL_SHA)==64, '爸爸，零目标launcher未绑定本轮现场/GO/软件准备，零I/O拒绝。'
subprocess.run(['tmux','set-option','-w','-t',os.environ['TMUX_PANE'],'remain-on-exit','on'],check=True)
os.execv({PYTHON!r},[{PYTHON!r},{(PHASE+'/base_zero_diagnostic_r1.py')!r},'--raw-helper',{(PHASE+'/helpers/raw_right9_from_r14_r1.py')!r},'--maintenance',{(BASE+'/software-facts-r1/scripts/run_hardware_maintenance.py')!r},'--bindings',{(PHASE+'/maintenance_bindings_base_zero_diagnostic_r1.json')!r},'--output',{(PHASE+'/base-zero-diagnostic-r1')!r},'--artifact-root',{BASE!r},'--execute','--approval',{(PHASE+'/base-zero-diagnostic-r1.go.json')!r}])
PYLAUNCH
'''

start_old = (OLD / 'start_head2_p32_reverse_once_r17.draft.sh').read_text()
start = renamed(start_old)
start = start.replace('PLAN_CANONICAL_SHA=None  #', 'NATIVE_PREPARATION_EXPECTED=None  # root固定本轮原生软件prepare-result(sha256,bytes)。\nPLAN_CANONICAL_SHA=None  #', 1)
start = start.replace('BINDING_EXPECTED is not None and PLAN_EXPECTED is not None and type(PLAN_CANONICAL_SHA)', 'BINDING_EXPECTED is not None and PLAN_EXPECTED is not None and NATIVE_PREPARATION_EXPECTED is not None and type(PLAN_CANONICAL_SHA)', 1)
start = start.replace("('32f4f9d5033710b6eeffeb47782c2156a402ba1d31f82cc285e08039802b71e5', 16246)", "('f59455a6b25f832efd5b725224d8f16229b421f28c1556cbe4416ed24ac6f314', 19556)", 1)
start = start.replace("'full-plan-only.stdout': PLAN_EXPECTED,", "'full-plan-only.stdout': PLAN_EXPECTED, 'prepare-result-base-zero-r1.json': NATIVE_PREPARATION_EXPECTED,", 1)
start = start.replace("'head2_p32_reverse_start_binding'", "'base_zero_start_frozen_d97'", 1)
install = """candidate=phase/'base_zero_diagnostic_r1.py';helper=phase/'helpers/raw_right9_from_r14_r1.py'
for name,path in (('base_zero_start_fixed_candidate',candidate),('base_zero_start_fixed_raw_helper',helper)):
 spec=importlib.util.spec_from_file_location(name,path);value=importlib.util.module_from_spec(spec);spec.loader.exec_module(value)
 if path==candidate:c=value
 else:h=value
c.install(m,h)
preparation=m.strict_json((phase/'prepare-result-base-zero-r1.json').read_text())
assert preparation['status']=='PASS_BASE_ZERO_R1_NATIVE_SOFTWARE_PREPARATION_ONLY' and preparation['uart_operations']==preparation['motor_actions']==0 and preparation['go_created'] is False and preparation['field_confirmation_created'] is False
"""
start = start.replace("plan=m.strict_json((phase/'full-plan-only.stdout').read_text());", install + "plan=m.strict_json((phase/'full-plan-only.stdout').read_text());", 1)
embedded = base64.b64encode(launcher.encode()).decode()
start = re.sub(r"launcher=base64\.b64decode\('[^']+',validate=True\)", "launcher=base64.b64decode('" + embedded + "',validate=True)\nfor key,value in (('FIELD_STAMP',FIELD_STAMP),('FIELD_EXPECTED',FIELD_EXPECTED),('GO_EXPECTED',GO_EXPECTED),('BINDING_EXPECTED',BINDING_EXPECTED),('PLAN_EXPECTED',PLAN_EXPECTED),('PLAN_CANONICAL_SHA',PLAN_CANONICAL_SHA),('NATIVE_PREPARATION_EXPECTED',NATIVE_PREPARATION_EXPECTED)):\n marker=(key+'=None').encode();assert launcher.count(marker)==1\n launcher=launcher.replace(marker,(key+'='+repr(value)).encode())", start, count=1)

observe_old = (OLD / 'observe_once_r17.draft.sh').read_text()
observe = renamed(observe_old)
observe = observe.replace('本轮BASE_ZERO_R1头2终态观察副本', '本轮BASE_ZERO_R1右9零目标终态观察副本', 1)
observe = observe.replace("pane_identity='__ACTUAL_PANE_PID_WINDOW__'", "pane_identity='None'  # root只能在实际单次启动回执后绑定pane/PID/window；首门拒绝草稿。", 1)
observe = observe.replace("'probes':j['probes'],", "'probes':j['probes'],'zero_diag':j.get('zero_diag'),", 1)
observe = observe.replace('停止/恢复未确认', '停止/身份未确认')

seal_old = (OLD / 'seal_head2_p32_reverse_r17.draft-r2.sh').read_text()
seal = renamed(seal_old)
seal = seal.replace('import base64,hashlib,importlib.util,json,os,stat,subprocess,tarfile,time', 'import base64,hashlib,importlib.util,json,os,stat,subprocess,tarfile,time', 1)
seal = seal.replace('APPROVED_HARDWARE_CHILD_COUNT=None', '''APPROVED_HARDWARE_CHILD_COUNT=None
FIELD_EXPECTED=None
GO_EXPECTED=None
BINDING_EXPECTED=None
PLAN_EXPECTED=None
PLAN_CANONICAL_SHA=None
NATIVE_PREPARATION_EXPECTED=None''', 1)
seal = seal.replace("base=Path('/home/loosqk/robot-checks/position-return-20261010-r1');", "assert all(v is not None for v in (FIELD_EXPECTED,GO_EXPECTED,BINDING_EXPECTED,PLAN_EXPECTED,NATIVE_PREPARATION_EXPECTED)) and type(PLAN_CANONICAL_SHA) is str and len(PLAN_CANONICAL_SHA)==64, '爸爸，零目标收集草稿未绑定本轮现场/GO/软件准备原件，零I/O拒绝。'\nbase=Path('/home/loosqk/robot-checks/position-return-20261010-r1');", 1)
seal = seal.replace(";out.mkdir(mode=0o700,exist_ok=True);assert not any(out.iterdir())", '', 1)
first = seal.index("assert len(run['owned_locks'])")
last = seal.index("start=json.loads", first)
seal = seal[:first] + '''assert run['result'] in ('FAIL','NEEDS_MANUAL_POWER_CUT') and type(run['manual_power_cut_required']) is bool
manual=run['manual_power_cut_required'];assert manual==(run['result']=='NEEDS_MANUAL_POWER_CUT')
assert len(run['commands'])==APPROVED_HARDWARE_CHILD_COUNT
assert APPROVED_PANE_EXIT==1
# 爸爸，停止/身份未确认只收真实原件和所属已退出pane，不做UART、动作或新的通过宣称。
if manual:print('爸爸，原报告仍要求人工断电，请立即切断两个电机电源并保持支撑；本收集器禁止UART和新动作。',flush=True)
else:
 assert len(run['owned_locks'])==4 and all(x['owner_pid']==APPROVED_DRIVER_PID for x in run['owned_locks'])
 assert all(v['reaped'] is True for v in run['commands']) and run['evidence_errors']==[] and not run['child_active'] and run['received_signal'] is None
 assert run['stop_verification']['status']==run['identity_verification']['status']=='PASS'
 assert run['position_verification']['status'] in ('PASS','FAIL','UNKNOWN')
 assert run['plan']['position_targets']==[] and run['plan']['firmware_contract_verified'] is False
 assert run['position_function_test']=='NOT_RUN_NO_NEW_HEAD_OR_ARM_FUNCTION_RESULT'
 expected_base={('right',8,'base_front_wheel'),('right',7,'base_left_wheel'),('right',9,'base_right_wheel')}
 planned_base=run['plan']['base_targets'];base_probes=run['probes']
 assert len(planned_base)==len(base_probes)==3
 for rows in (planned_base,base_probes):
  assert {(p['side'],p['motor'],p['name']) for p in rows}==expected_base
  assert all(p['decision']=='DEFERRED' and p['reason_code']=='BASE_FIRMWARE_CONTRACT_UNVERIFIED' for p in rows)
 assert all(p['kind']=='velocity' and p['status']=='NOT_RUN' for p in base_probes)
 zero=run['zero_diag'];observation=zero['observation'];samples=observation['samples'];trial=run['plan']['zero_diag']
 assert (zero['side'],zero['motor'],zero['name'],zero['kind'])==('right',9,'base_right_wheel','zero_diag')
 assert zero['status'] in ('PASS','FAIL') and zero['firmware_contract_verified'] is False and zero['uart_blocked'] is False and zero['cleanup_errors']==[]
 assert [(p['register'],p['value'],p['cleanup']) for p in zero['write_attempts']]==[('Goal_Velocity',0,False),('Torque_Enable',1,False),('Torque_Enable',0,True)]
 assert len(zero['before_raw'])==9 and {p['register']:p['raw']['raw_unsigned'] for p in zero['before_raw']}==trial['expected_raw']
 assert [(p['register'],p['raw']['raw_unsigned']) for p in zero['verification_raw']]==[('Goal_Velocity',0),('Torque_Enable',1)]
 assert all(p['status']=='READ_VALID' for p in zero['before_raw']+zero['verification_raw']+samples)
 assert observation['budget_ns']==500000000 and observation['minimum_in_budget_pv0_samples']==3
 assert observation['deadline_ns']==observation['started_ns']+500000000 and len(samples)<=3
 assert [p['requested_offset_ns'] for p in samples]==[0,200000000,400000000][:len(samples)]
 assert all(p['scheduled_ns']==observation['started_ns']+p['requested_offset_ns'] and p['completed_in_budget']==(p['finished_ns']<=observation['deadline_ns']) and p['raw_zero']==(p['raw']['raw_unsigned']==0) for p in samples)
 qualified=lambda p:p['raw_zero'] is True and p['completed_in_budget'] is True
 if zero['status']=='PASS':
  assert zero['errors']==[] and observation['status']=='PASS' and len(samples)==3 and all(qualified(p) for p in samples) and observation['in_budget_pv0_samples']==3
 else:
  assert observation['status']=='FAIL' and len(zero['errors'])==1
  error=zero['errors'][0]
  nonzero=bool(samples) and all(qualified(p) for p in samples[:-1]) and samples[-1]['raw']['raw_unsigned']!=0
  late=bool(samples) and all(qualified(p) for p in samples[:-1]) and samples[-1]['raw_zero'] is True and samples[-1]['completed_in_budget'] is False
  missed=len(samples) in (1,2) and all(qualified(p) for p in samples) and observation['finished_ns']>observation['started_ns']+[0,200000000,400000000][len(samples)]
  expired=len(samples)<3 and all(qualified(p) for p in samples) and observation['finished_ns']>=observation['deadline_ns']
  assert ((error==repr(ValueError('观察PV原帧非零，立即Disable收尾')) and nonzero)
          or (error==repr(ValueError('PV原帧完成越过500ms原期限')) and late)
          or (error==repr(ValueError('原分散请求时刻已错过，不补读')) and missed)
          or (error==repr(ValueError('500ms原期限已到，不补读')) and expired)), '爸爸，仅收合法PV非零/原期限失败；未知ACK、raw、停止和信号不归为普通诊断失败。'
assert APPROVED_PANE_EXIT==(0 if run['result']=='PASS' else 1)
expected={'field_confirmation_base_zero_r1.json':FIELD_EXPECTED,'base-zero-diagnostic-r1.go.json':GO_EXPECTED,'maintenance_bindings_base_zero_diagnostic_r1.json':BINDING_EXPECTED,'full-plan-only.stdout':PLAN_EXPECTED,'prepare-result-base-zero-r1.json':NATIVE_PREPARATION_EXPECTED}
for name,(sha,size) in expected.items():
 path=phase/name;raw=path.read_bytes();assert path.is_file() and not path.is_symlink() and hashlib.sha256(raw).hexdigest()==sha and len(raw)==size
full=json.loads((phase/'full-plan-only.stdout').read_text());assert full['plan_sha256']==PLAN_CANONICAL_SHA
out.mkdir(mode=0o700,exist_ok=True);assert not any(out.iterdir())
''' + seal[last:]
seal = seal.replace('before=pins();rows=[]', 'before=[] if manual else pins();rows=[]', 1)
first = seal.index("probe=base/'normal-read-r4/nonuart_probe_r4.sh'")
last = seal.index("selected=", first)
seal = seal[:first] + '''fresh=None;after=[];snapshot=None
if not manual:
 probe=base/'normal-read-r4/nonuart_probe_r4.sh';(out/'existing-nonuart-probe-r4.sh').write_bytes(probe.read_bytes());fresh=json.loads(command('independent-nonuart17',['bash',str(probe)]));assert fresh['status']=='PASS_NON_UART_CURRENT_FACTS' and len(fresh['checks'])==17 and all(v['status']=='PASS' for v in fresh['checks']) and len(fresh['commands'])==23 and all(v['reaped'] is True for v in fresh['commands'])
 assert not fresh['uart_open_or_read'] and fresh['hardware_writes']==0 and fresh['errors']==fresh['received_signals']==[] and fresh['identity_before']==fresh['identity_after'] and fresh['files_before']==fresh['files_after'] and fresh['services']['valid']
 assert fresh['identity_before']['boot_id']==bindings['host']['boot_id'];after=pins();assert before==after;save('bound-refs-before.json',before);save('bound-refs-after.json',after)
 art=Path('/home/loosqk/robot-checks/h2-exit-observation-20261009-r1/source/scripts/post_e_h2_artifacts.py');assert hashlib.sha256(art.read_bytes()).hexdigest()=='bd2b3f7bb8527cd9ecec73894171638e9cf378656b89e284a585c3baa37b8731'
 spec=importlib.util.spec_from_file_location('base_zero_r1_seal_existing_artifacts',art);a=importlib.util.module_from_spec(spec);spec.loader.exec_module(a)
 source=base/'position-return-r7/source';expected=json.loads(Path('/home/loosqk/robot-checks/position-return-20261011-r12/native-quality/boot/host_source_final_r12.json').read_text());snapshot=a.snapshot_source(source,list(expected['files']));assert snapshot==expected;save('native-source-seal-window.json',snapshot)
report={'status':'BASE_ZERO_R1_INCIDENT_ORIGINALS_COLLECTED_MANUAL_CUT_REQUIRED' if manual else 'PASS_BASE_ZERO_R1_NONUART_COLLECTION_ORIGINAL_RESULT_PRESERVED','terminal':state,'driver_pid':APPROVED_DRIVER_PID,'original_run_status':run['result'],'original_run_sha256':hashlib.sha256(runraw).hexdigest(),'original_run_bytes':len(runraw),'original_error':run.get('error'),'zero_diag':run.get('zero_diag'),'position':run.get('position_verification'),'stop':run.get('stop_verification'),'identity':run.get('identity_verification'),'probes':run.get('probes'),'direct_hardware_children':len(run['commands']),'all_hardware_children_reaped':all(v['reaped'] is True for v in run['commands']),'child_active':run['child_active'],'evidence_errors':run['evidence_errors'],'manualcut':manual,'independent_nonUART_checks':len(fresh['checks']) if fresh else None,'independent_nonUART_children':len(fresh['commands']) if fresh else None,'manual_incident_extra_postchecks':'NOT_RUN_ORIGINAL_FAILURE_PRESERVED' if manual else None,'bound_refs':len(bindings['refs']),'bound_refs_before_after_equal':before==after if not manual else None,'native_source_files':len(snapshot['files']) if snapshot else None,'native_source_sha256':snapshot['source_manifest_sha256'] if snapshot else None,'collector_direct_children':rows,'new_UART':0,'new_writes':0,'new_GO':False};save('seal-result-base-zero-r1.json',report)
''' + seal[last:]
seal = seal.replace("'full-plan-only.stdout','field_confirmation_base_zero_r1.json'", "'helpers/raw_right9_from_r14_r1.py','full-plan-only.stdout','prepare-result-base-zero-r1.json','field_confirmation_base_zero_r1.json'", 1)
seal = seal.replace('hardware-originals-r17.tar.gz', 'hardware-originals-base-zero-r1.tar.gz')
seal = seal.replace('HARDWARE_R17_ARCHIVE_READY', 'HARDWARE_BASE_ZERO_R1_ARCHIVE_READY')

artifacts = [
    ('launch_base_zero_diagnostic_r1.draft.sh', launcher, OLD / 'launch_head2_p32_reverse_r17.draft.sh'),
    ('start_base_zero_diagnostic_once_r1.draft.sh', start, OLD / 'start_head2_p32_reverse_once_r17.draft.sh'),
    ('observe_base_zero_diagnostic_once_r1.draft.sh', observe, OLD / 'observe_once_r17.draft.sh'),
    ('seal_base_zero_diagnostic_r1.draft.sh', seal, OLD / 'seal_head2_p32_reverse_r17.draft-r2.sh'),
]
manifest = []
for name, text, old_path in artifacts:
    path = OUT / name
    with path.open('x') as stream:
        stream.write(text)
    diff = ''.join(difflib.unified_diff(old_path.read_text().splitlines(True), text.splitlines(True), fromfile=str(old_path), tofile=str(path)))
    with (OUT / (name+'.diff-vs-r17.patch')).open('x') as stream:
        stream.write(diff)
    raw=path.read_bytes()
    manifest.append({'path':str(path),'sha256':hashlib.sha256(raw).hexdigest(),'bytes':len(raw),'derived_from':str(old_path)})
    if "<<'PYREMOTE'" in text:
        body=text.split("<<'PYREMOTE'\n",1)[1].rsplit('\nPYREMOTE',1)[0]
        ast.parse(body,filename=str(path)+':PYREMOTE')
    if "<<'PYLAUNCH'" in text:
        body=text.split("<<'PYLAUNCH'\n",1)[1].rsplit('\nPYLAUNCH',1)[0]
        ast.parse(body,filename=str(path)+':PYLAUNCH')
    if "<<'PY'" in text:
        body=text.split("<<'PY'\n",1)[1].split('\nPY\n',1)[0]
        ast.parse(body,filename=str(path)+':PY')
report={'status':'LOCAL_DRAFTS_WRITTEN_AST_PARSE_ONLY','base':BASE,'phase':PHASE,'hardware_session':SESSION,'artifacts':manifest,'field_GO_created':False,'actual_scripts_started':False,'SSH':False,'UART':False,'candidate_changed':False,'pins':'ALL_UNKNOWN_ACTUAL_PINS_NONE_OR_NONE_PANE_PLACEHOLDER'}
with (OUT/'live-draft-generation-base-zero-r1.json').open('x') as stream:
    json.dump(report,stream,ensure_ascii=False,indent=2);stream.write('\n')
print(json.dumps(report,ensure_ascii=False,indent=2))
