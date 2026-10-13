"""爸爸，只在本机派生一次只读runtime草稿；不上传、不SSH、不创建现场或GO。"""

import ast
import base64
import difflib
import hashlib
import json
from pathlib import Path
import shlex

HERE = Path(__file__).resolve().parent
ROOT = next(p for p in HERE.parents if (p / "scripts/run_hardware_maintenance.py").is_file())
OLD = ROOT / "target/base_zero_diagnostic_2026-10-12_r4"
LEGACY = "/home/loosqk/robot-checks/position-return-20261010-r1"
PHASE = "/home/loosqk/robot-checks/base-feedback-diagnostic-r1"
CANDIDATE = "/home/loosqk/robot-checks/base-feedback-r1-software/candidate/base_feedback_diagnostic_r1.py"
CANDIDATE_PIN = ("87e3c135e4c78f5232a8ba8aa88c999051d0826b339ed4c5b31795c794fb7b9c", 18648)
HELPER = LEGACY + "/base-zero-diagnostic-r1/helpers/raw_right9_from_r14_r1.py"
MAINTENANCE = LEGACY + "/software-facts-r1/scripts/run_hardware_maintenance.py"
BINDING = "maintenance_bindings_base_feedback_diagnostic_r1.json"
SESSION = "base-feedback-diagnostic-trial-20261012-r1"
OUTPUT = "base-feedback-diagnostic-r1"
LAUNCH = "base-feedback-diagnostic-launch-r1"
rows = []


def replace_one(source, old, new):
    assert source.count(old) == 1, (old, source.count(old))
    return source.replace(old, new)


def replacements(source):
    # 只替换已读冻结R4模板的具名标签/路径；不对不可信shell内容插值。
    for old, new in [
        ("BASE_ZERO_R4", "BASE_FEEDBACK_R1"),
        ("BASE_ZERO_DIAGNOSTIC_R4", "BASE_FEEDBACK_DIAGNOSTIC_R1"),
        ("base-zero-diagnostic-trial-20261012-r4", SESSION),
        ("maintenance_bindings_base_zero_diagnostic_r4.json", BINDING),
        ("base-zero-diagnostic-launch-r4", LAUNCH),
        ("base-zero-diagnostic-seal-r4", "base-feedback-diagnostic-seal-r1"),
        ("observer-event-base-zero-r4", "observer-event-base-feedback-r1"),
        ("base-zero-diagnostic-r4", OUTPUT),
        ("field_confirmation_base_zero_r4.json", "field_confirmation_base_feedback_r1.json"),
        ("full-plan-only-r4", "full-plan-only-r1"),
        ("prepare-result-base-zero-r4.json", "prepare-result-base-feedback-r1.json"),
        ("start_receipt_base_zero_r4.json", "start_receipt_base_feedback_r1.json"),
        ("launch_base_zero_diagnostic_r4.sh", "launch_base_feedback_diagnostic_r1.sh"),
        ("seal-result-base-zero-r4.json", "seal-result-base-feedback-r1.json"),
        ("hardware-originals-base-zero-r4.tar.gz", "hardware-originals-base-feedback-r1.tar.gz"),
        ("base-zero-diagnostic-r1", "base-feedback-diagnostic-r1"),
    ]:
        source = source.replace(old, new)
    return source


def shell_body(source):
    head, body = source.split("<<'PYREMOTE'\n", 1)
    body, footer = body.rsplit("\nPYREMOTE\n", 1)
    assert footer == ""
    return head + "<<'PYREMOTE'\n", body, "\nPYREMOTE\n"


def write(name, source, baseline):
    path = HERE / name
    with path.open("x", encoding="utf-8") as stream:
        stream.write(source)
    before = baseline.read_text()
    patch = "".join(difflib.unified_diff(before.splitlines(keepends=True), source.splitlines(keepends=True),
                                        fromfile=str(baseline), tofile=str(path)))
    with (HERE / (name + ".diff-vs-r4.patch")).open("x", encoding="utf-8") as stream:
        stream.write(patch)
    raw = source.encode()
    rows.append({"path": str(path), "sha256": hashlib.sha256(raw).hexdigest(), "bytes": len(raw),
                 "baseline": {"path": str(baseline), "sha256": hashlib.sha256(before.encode()).hexdigest(), "bytes": len(before.encode())}})


# 爸爸，保留已验证的-c及PTY行为，仅替换Python常量和exec参数，再整体shell quote。
launcher_old = OLD / "launch_base_zero_diagnostic_r4.fixed-unbound.sh"
text = launcher_old.read_text()
header, command = text.split("exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 -c ", 1)
args = shlex.split(command)
assert len(args) == 1
program = replacements(args[0])
program = program.replace(LEGACY + "/base-feedback-diagnostic-r1/", PHASE + "/")
program = program.replace(PHASE + "/base_zero_diagnostic_r1.py", CANDIDATE)
program = program.replace(PHASE + "/helpers/raw_right9_from_r14_r1.py", HELPER)
program = program.replace("'--artifact-root','" + LEGACY + "'", "'--artifact-root','" + PHASE + "'")
program = program.replace("零目标launcher", "只读反馈launcher")
ast.parse(program)
launcher = header.replace("电机入口", "只读候选入口") + "exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 -c " + shlex.quote(program) + "\n"
write("launch_base_feedback_diagnostic_r1.draft.sh", launcher, launcher_old)

starter_old = OLD / "start_base_zero_diagnostic_once_r4.fixed-unbound.sh"
header, body, footer = shell_body(starter_old.read_text())
body = replacements(body)
body = replace_one(body, "base=Path('" + LEGACY + "');phase=base/'base-feedback-diagnostic-r1'",
                   "legacy=Path('" + LEGACY + "');base=Path('" + PHASE + "');phase=base")
body = body.replace("script=base/'software-facts-r1/scripts/run_hardware_maintenance.py'", "script=legacy/'software-facts-r1/scripts/run_hardware_maintenance.py'")
body = body.replace("str(base/'normal-read-r4/nonuart_probe_r4.sh')", "str(legacy/'normal-read-r4/nonuart_probe_r4.sh')")
body = replace_one(body, "'base_zero_diagnostic_r1.py': ('f59455a6b25f832efd5b725224d8f16229b421f28c1556cbe4416ed24ac6f314', 19556), ", "")
body = replace_one(body, "candidate=phase/'base_zero_diagnostic_r1.py';helper=phase/'helpers/raw_right9_from_r14_r1.py'",
                   "candidate=Path('" + CANDIDATE + "');helper=Path('" + HELPER + "')\n" +
                   "for path,pin in ((candidate," + repr(CANDIDATE_PIN) + "),(helper,('406295f0bf97ca4a482241a565500d2bae1e226c6a8959bcebbd3264c50b9dfa',12216)),(legacy/'normal-read-r4/nonuart_probe_r4.sh',('e3962ec7841a9d5ed783ca8279ed718c8f29ed718701de528c226f0588b91527',20020))):\n raw=path.read_bytes();assert not path.is_symlink() and hashlib.sha256(raw).hexdigest()==pin[0] and len(raw)==pin[1]")
body = replace_one(body, "c.install(m,h)\n", "")
body = body.replace("preparation['round']==4", "preparation['round']==1")
body = body.replace("PASS_BASE_FEEDBACK_R1_NATIVE_SOFTWARE_PREPARATION_ONLY", "PASS_BASE_FEEDBACK_R1_NATIVE_RUNTIME_PREPARATION_ONLY")
body = replace_one(body, "go=m.strict_json(go_raw.decode());now=time.time_ns();m.validate_go(go,plan['plan'],now)",
                   "go=m.strict_json(go_raw.decode());c.install(m,h,go);now=time.time_ns();m.validate_go(go,plan['plan'],now)")
tree = ast.parse(body)
launcher_value = next(n.value for n in tree.body if isinstance(n, ast.Assign) and isinstance(n.targets[0], ast.Name) and n.targets[0].id == "launcher")
start_lines = body.splitlines(keepends=True)
assert launcher_value.lineno == launcher_value.end_lineno
line = start_lines[launcher_value.lineno - 1]
old_call = line[launcher_value.col_offset:launcher_value.end_col_offset]
assert old_call.startswith("base64.b64decode(")
body = replace_one(body, old_call, "base64.b64decode(" + repr(base64.b64encode(launcher.encode()).decode()) + ",validate=True)")
body = body.replace("STARTED_SINGLE_BASE_ZERO_DIAGNOSTIC_R4", "STARTED_SINGLE_BASE_FEEDBACK_DIAGNOSTIC_R1")
write("start_base_feedback_diagnostic_once_r1.draft.sh", header + body + footer, starter_old)

observer_old = OLD / "observe_base_zero_diagnostic_once_r4.draft.sh"
observer = replacements(observer_old.read_text())
observer = observer.replace("base=" + LEGACY, "base=" + PHASE)
observer = observer.replace('"$base/base-feedback-diagnostic-r1/', '"$base/')
observer = observer.replace("zero_diag", "feedback_diag").replace("右9零目标", "右9只读反馈")
write("observe_base_feedback_diagnostic_once_r1.draft.sh", observer, observer_old)

seal_old = OLD / "seal_base_zero_diagnostic_r4.draft.sh"
header, body, footer = shell_body(seal_old.read_text())
body = replacements(body)
body = replace_one(body, "base=Path('" + LEGACY + "');phase=base/'base-feedback-diagnostic-r1'",
                   "legacy=Path('" + LEGACY + "');base=Path('" + PHASE + "');phase=base")
body = body.replace("NOT_RUN_NO_NEW_HEAD_OR_ARM_FUNCTION_RESULT", "NOT_RUN_NO_ACTIVE_MOTION")
start = body.index(" zero=run['zero_diag'];")
end = body.index("assert APPROVED_PANE_EXIT==(0 if run['result']=='PASS' else 1)", start)
body = body[:start] + """ feedback=run['feedback_diag'];observation=feedback['observation'];groups=observation['groups'];trial=run['plan']['feedback_diag']
 assert (feedback['side'],feedback['motor'],feedback['name'],feedback['kind'])==('right',9,'base_right_wheel','feedback_diag')
 assert feedback['status']=='PASS' and feedback['firmware_contract_verified'] is False and feedback['uart_blocked'] is False and feedback['errors']==[]
 assert feedback['register_writes']==feedback['motion_commands']==0 and trial['write_sequence']==[]
 assert len(feedback['preflight'])==7 and {p['register']:p['raw']['raw_unsigned'] for p in feedback['preflight']}=={k:v for k,v in trial['expected_raw'].items() if k!='Torque_Enable'}
 assert observation['budget_ns']==500000000 and observation['deadline_ns']==observation['started_ns']+500000000
 assert observation['status']=='PASS' and observation['in_budget_pv0_groups']==3 and len(groups)==3
 assert [g['requested_offset_ns'] for g in groups]==[0,200000000,400000000]
 for group in groups:
  assert group['scheduled_ns']==observation['started_ns']+group['requested_offset_ns'] and group['status']=='PASS' and group['completed_in_budget'] is True and group['raw_pv_zero'] is True
  assert [sample['register'] for sample in group['reads']]==['Torque_Enable','Present_Position','Present_Velocity']
  assert all(sample['status']=='READ_VALID' and sample['completed_in_budget'] is True and sample['finished_ns']<=observation['deadline_ns'] for sample in group['reads'])
  assert group['reads'][0]['raw']['raw_unsigned']==group['reads'][2]['raw']['raw_unsigned']==0
  assert all(type(sample['raw']['raw_unsigned']) is int for sample in group['reads'])
""" + body[end:]
body = body.replace("probe=base/'normal-read-r4/nonuart_probe_r4.sh'", "probe=legacy/'normal-read-r4/nonuart_probe_r4.sh'")
body = body.replace("source=base/'position-return-r7/source'", "source=legacy/'position-return-r7/source'")
body = body.replace("'zero_diag':run.get('zero_diag')", "'feedback_diag':run.get('feedback_diag'),'active_zero_speed_function':'NOT_RUN_NO_ENABLE_OR_WRITE'")
body = body.replace("'base_zero_diagnostic_r1.py',", "")
body = body.replace("'helpers/raw_right9_from_r14_r1.py',", "")
body = replace_one(body, "archive=phase/'hardware-originals-base-feedback-r1.tar.gz'", """extra_sources=[(Path('""" + CANDIDATE + """'),'candidate/base_feedback_diagnostic_r1.py'),(Path('""" + HELPER + """'),'helpers/raw_right9_from_r14_r1.py'),(Path('""" + MAINTENANCE + """'),'scripts/run_hardware_maintenance.py')]
assert all(path.is_file() and not path.is_symlink() for path,name in extra_sources)
archive=phase/'hardware-originals-base-feedback-r1.tar.gz'""")
body = replace_one(body, "for path in paths:t.add(path,arcname=str(path.relative_to(phase)),recursive=False)",
                   "for path in paths:t.add(path,arcname=str(path.relative_to(phase)),recursive=False)\n for path,name in extra_sources:t.add(path,arcname=name,recursive=False)")
body = body.replace("'files':len(paths),'source_bytes':sum(x.stat().st_size for x in paths)", "'files':len(paths)+len(extra_sources),'source_bytes':sum(x.stat().st_size for x in paths)+sum(x.stat().st_size for x,name in extra_sources)")
body = body.replace("零目标收集草稿", "只读反馈收集草稿")
write("seal_base_feedback_diagnostic_r1.draft.sh", header + body + footer, seal_old)

refresh_old = OLD / "readonly_nonuart17_usb_refresh_r4.sh"
header, body, footer = shell_body(refresh_old.read_text())
body = replace_one(body, "base=Path('" + LEGACY + "');phase=base/'base-zero-diagnostic-r1';probe=base/'normal-read-r4/nonuart_probe_r4.sh'", """FIELD_STAMP=None
FIELD_EXPECTED=None
assert type(FIELD_STAMP) is int and FIELD_STAMP>0 and isinstance(FIELD_EXPECTED,tuple) and len(FIELD_EXPECTED)==2, '爸爸，本轮恢复供电和新现场确认未绑定，零child/文件I/O。'
assert 0<=time.time_ns()-FIELD_STAMP<600_000_000_000, '爸爸，本轮新现场确认过期；不刷新或沿用旧确认。'
legacy=Path('""" + LEGACY + """');phase=Path('""" + PHASE + """');probe=legacy/'normal-read-r4/nonuart_probe_r4.sh'
phase.mkdir(mode=0o700,exist_ok=False)""")
body = body.replace("usb-refresh-nonuart-r4", "usb-refresh-nonuart-r1")
body = replace_one(body, "r={'pid':c.pid", "r={'field_stamp':FIELD_STAMP,'field_expected':FIELD_EXPECTED,'field_scope':'CURRENT_POWER_ON_AND_SUSPENDED_WHEELS_READONLY_READY','pid':c.pid")
body = replace_one(body, "assert code==0 and b==b'';v=json.loads(a);", "assert code==0 and b==b'';assert 0<=time.time_ns()-FIELD_STAMP<600_000_000_000;v=json.loads(a);")
write("readonly_nonuart17_usb_refresh_r1.draft.sh", header + body + footer, refresh_old)

prepare_old = OLD / "prepare_base_zero_diagnostic_r4.draft.py"
prepare = replacements(prepare_old.read_text()).replace("R4", "只读R1")
prepare = replace_one(prepare, "BASE = Path('" + LEGACY + "')\nPHASE = BASE / 'base-feedback-diagnostic-r1'", "LEGACY = Path('" + LEGACY + "')\nBASE = Path('" + PHASE + "')\nPHASE = BASE")
prepare = replace_one(prepare, "CANDIDATE = PHASE / 'base_zero_diagnostic_r1.py'", "CANDIDATE = Path('" + CANDIDATE + "')")
prepare = replace_one(prepare, "HELPER = PHASE / 'helpers/raw_right9_from_r14_r1.py'", "HELPER = Path('" + HELPER + "')")
prepare = replace_one(prepare, "OLD_BINDING = PHASE / 'maintenance_bindings_base_zero_diagnostic_r3.json'", "OLD_BINDING = LEGACY / 'base-zero-diagnostic-r1/maintenance_bindings_base_zero_diagnostic_r4.json'")
prepare = replace_one(prepare, "MAINTENANCE = BASE / 'software-facts-r1/scripts/run_hardware_maintenance.py'", "MAINTENANCE = LEGACY / 'software-facts-r1/scripts/run_hardware_maintenance.py'")
prepare = prepare.replace("usb-refresh-nonuart-r4", "usb-refresh-nonuart-r1").replace("usb-stat-before-r4", "usb-stat-before-r1").replace("usb-stat-after-r4", "usb-stat-after-r1")
prepare = prepare.replace("('f59455a6b25f832efd5b725224d8f16229b421f28c1556cbe4416ed24ac6f314', 19556)", repr(CANDIDATE_PIN))
prepare = replace_one(prepare, "    PHASE / 'full-plan-only.stdout': ('1d049b491e69d0756e074cee388beaf814edb8e95013fdfce1ecc9d8d4ee0555', 38231),\n", "")
prepare = replace_one(prepare, "NONUART_STDOUT_PATH = None", "FIELD_STAMP = None\nFIELD_EXPECTED = None\nassert type(FIELD_STAMP) is int and FIELD_STAMP>0 and isinstance(FIELD_EXPECTED,tuple) and len(FIELD_EXPECTED)==2, '爸爸，本轮恢复供电及新现场确认未固定，零I/O和child。'\nNONUART_STDOUT_PATH = None")
prepare = replace_one(prepare, "assert NONUART_STDOUT_EXPECTED[0] != 'f497a4b2b49d94428096419cc24d034ee86913f8e6462408acd768ea28ada06e'\nassert NONUART_RECEIPT_EXPECTED[0] != '6533a306d71958a3cfb9ed39c721b77de6c0ec67873deacdaed1de63663c1709'", "assert NONUART_STDOUT_EXPECTED[0] not in ('f497a4b2b49d94428096419cc24d034ee86913f8e6462408acd768ea28ada06e','928e5e978179d95a6a34436ccc0abd1ff545c65529290ea2a404e68e623fad88')\nassert NONUART_RECEIPT_EXPECTED[0] not in ('6533a306d71958a3cfb9ed39c721b77de6c0ec67873deacdaed1de63663c1709','7c117798c001bda3c6fd54bd698425e3277ce19bd70d3b2316293427031010ec')")
prepare = replace_one(prepare, "assert receipt['reaped'] is True and receipt['signals_sent'] == []", "assert receipt['reaped'] is True and receipt['signals_sent'] == []\nassert receipt['field_stamp']==FIELD_STAMP and tuple(receipt['field_expected'])==FIELD_EXPECTED\nassert receipt['field_scope']=='CURRENT_POWER_ON_AND_SUSPENDED_WHEELS_READONLY_READY'\nassert FIELD_STAMP<=receipt['started_ns']<receipt['finished_ns']<=time.time_ns()<FIELD_STAMP+600_000_000_000")
prepare = replace_one(prepare, "assert all(bindings[key] == old[key] for key in old if key != 'usb')", "assert all(bindings[key] == old[key] for key in old if key != 'usb')\ncandidate_ref=ref(CANDIDATE)\nassert not any(row['path']==candidate_ref['path'] for row in bindings['refs'])\nbindings['refs'].append(candidate_ref)\nassert bindings['refs'][:-1]==old['refs'] and len(bindings['refs'])==56")
start = prepare.index("old_plan = m.strict_json(")
end = prepare.index("m.protected_and_refs(bindings)", start)
prepare = prepare[:start] + """spec=importlib.util.spec_from_file_location('feedback_r1_native_plan_candidate',CANDIDATE)
c=importlib.util.module_from_spec(spec);spec.loader.exec_module(c)
spec=importlib.util.spec_from_file_location('feedback_r1_native_plan_helper',HELPER)
h=importlib.util.module_from_spec(spec);spec.loader.exec_module(h)
expected=c.make_plan(m,h,bindings,OUTPUT,BASE,ref(CANDIDATE),ref(HELPER))
assert full['status']=='PLAN_ONLY' and full['actions_started']==0 and full['plan']==expected
assert full['plan_sha256']==m.digest(expected) and expected['run_id']=='base-feedback-diagnostic-r1'
assert expected['position_targets']==[] and expected['firmware_contract_verified'] is False
assert len(expected['base_targets'])==3 and all(row['decision']=='DEFERRED' for row in expected['base_targets'])
assert expected['feedback_diag']['sample_offsets_ns']==[0,200000000,400000000] and expected['feedback_diag']['budget_ns']==500000000
assert expected['feedback_diag']['group_read_sequence']==['Torque_Enable','Present_Position','Present_Velocity'] and expected['feedback_diag']['write_sequence']==[]
""" + prepare[end:]
prepare = prepare.replace("PASS_BASE_FEEDBACK_R1_NATIVE_SOFTWARE_PREPARATION_ONLY", "PASS_BASE_FEEDBACK_R1_NATIVE_RUNTIME_PREPARATION_ONLY")
prepare = prepare.replace("'round': 4", "'round': 1").replace("'bound_refs': 55", "'bound_refs': 56")
prepare = prepare.replace("本recipe不读取/沿用R3现场或GO，不创建/续期只读R1现场或GO", "本recipe仅接受本次恢复供电后的新非UART原件及新现场固定pin；不生成/续期GO或现场文件")
write("prepare_base_feedback_diagnostic_r1.draft.py", prepare, prepare_old)
shell = "#!/bin/bash\n# 爸爸，当前输入未绑定即先拒绝；仅准备只读PLAN，不UART/GO。\nset -eu\nexport PYTHONDONTWRITEBYTECODE=1\nexec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'\n" + prepare + "\nPYREMOTE\n"
write("minimal_runtime_prepare_base_feedback_r1.draft.sh", shell, OLD / "minimal_software_prepare_base_zero_r4.draft.sh")

receipt = {"status": "LOCAL_RUNTIME_DRAFTS_GENERATED_NOT_EXECUTED", "drafts": rows,
           "new_root": PHASE, "new_session": SESSION, "candidate": {"path": CANDIDATE, "sha256": CANDIDATE_PIN[0], "bytes": CANDIDATE_PIN[1]},
           "SSH_executions": 0, "UART_operations": 0, "GO_created": False, "field_created": False,
           "R4_status": "人工断电已独立确认，旧NEEDS_MANUAL_POWER_CUT不回填，不授权本轮供电或动作",
           "scope": "只读feedback_diag三组；原整体FAIL/三轮DEFERRED保持，不能宣称主动零速或物理停止PASS"}
with (HERE / "runtime-draft-generation-receipt.json").open("x", encoding="utf-8") as stream:
    json.dump(receipt, stream, ensure_ascii=False, indent=2)
    stream.write("\n")
print(json.dumps(receipt, ensure_ascii=False))
