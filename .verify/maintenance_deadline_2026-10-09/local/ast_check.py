import ast,json
from pathlib import Path
root=Path.cwd()
base=root/'target/h2_native_prepare_2026-10-09/triage-head-local'
paths=['scripts/run_hardware_maintenance.py','scripts/tests/test_hardware_maintenance.py']
parsed={p:ast.parse((root/p).read_text(),filename=p) for p in paths}
old=ast.parse((base/'baseline/scripts/tests/test_hardware_maintenance.py').read_text())
new=parsed[paths[1]]
added={'RecordingClockIO','DeadlineExhaustionTests'}
kept=ast.Module(body=[n for n in new.body if not isinstance(n,ast.ClassDef) or n.name not in added],type_ignores=[])
assert ast.dump(old,include_attributes=False)==ast.dump(kept,include_attributes=False)
methods=[n.name for n in ast.walk(new) if isinstance(n,ast.FunctionDef) and n.name.startswith('test_')]
assert len(methods)==44 and len(set(methods))==44
assert (root/paths[0]).read_bytes()==(base/'draft/scripts/run_hardware_maintenance.py').read_bytes()
print(json.dumps({'AST':'PASS','original_40_methods_and_other_original_nodes_unchanged':True,'distinct_test_methods':len(methods),'main_script_equals_validated_draft':True},ensure_ascii=False))
