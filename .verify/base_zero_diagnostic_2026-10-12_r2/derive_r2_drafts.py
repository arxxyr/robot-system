"""爸爸，只在本机target派生R2草稿；不调用SSH、tmux、候选或UART。"""
import ast
import base64
import difflib
import hashlib
import json
from pathlib import Path
import re

ROOT = Path('/Users/loosqk/repo/rust/robot-system')
OLD = ROOT / '.verify/base_zero_diagnostic_2026-10-12_r1'
OUT = ROOT / 'target/base_zero_diagnostic_2026-10-12_r2'
BASE = '/home/loosqk/robot-checks/position-return-20261010-r1'
PHASE = BASE + '/base-zero-diagnostic-r1'
PYTHON = '/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3'


def renamed(text):
    for old, new in (
        ('base-zero-diagnostic-trial-20261012-r1', 'base-zero-diagnostic-trial-20261012-r2'),
        ('base-zero-diagnostic-launch-r1', 'base-zero-diagnostic-launch-r2'),
        ('base-zero-diagnostic-seal-r1', 'base-zero-diagnostic-seal-r2'),
        ('base-zero-diagnostic-r1.go.json', 'base-zero-diagnostic-r2.go.json'),
        ('field_confirmation_base_zero_r1.json', 'field_confirmation_base_zero_r2.json'),
        ('full-plan-only.stdout', 'full-plan-only-r2.stdout'),
        ('prepare-result-base-zero-r1.json', 'prepare-result-base-zero-r2.json'),
        ('start_receipt_base_zero_r1.json', 'start_receipt_base_zero_r2.json'),
        ('observer-event-base-zero-r1', 'observer-event-base-zero-r2'),
        ('launch_base_zero_diagnostic_r1.sh', 'launch_base_zero_diagnostic_r2.sh'),
        ('seal-result-base-zero-r1.json', 'seal-result-base-zero-r2.json'),
        ('hardware-originals-base-zero-r1.tar.gz', 'hardware-originals-base-zero-r2.tar.gz'),
        ('BASE_ZERO_DIAGNOSTIC_R1', 'BASE_ZERO_DIAGNOSTIC_R2'),
        ('BASE_ZERO_R1', 'BASE_ZERO_R2'),
    ):
        text = text.replace(old, new)
    text = text.replace("output=phase/'base-zero-diagnostic-r1'", "output=phase/'base-zero-diagnostic-r2'")
    text = text.replace("evidence=phase/'base-zero-diagnostic-r1'", "evidence=phase/'base-zero-diagnostic-r2'")
    text = text.replace('/base-zero-diagnostic-r1/base-zero-diagnostic-r1', '/base-zero-diagnostic-r1/base-zero-diagnostic-r2')
    return text


manifest = []


def emit(name, text, source=None):
    path = OUT / name
    with path.open('x') as stream:
        stream.write(text)
    raw = path.read_bytes()
    manifest.append({'path': str(path), 'sha256': hashlib.sha256(raw).hexdigest(),
                     'bytes': len(raw), 'derived_from': str(source) if source else None})
    if source:
        diff = ''.join(difflib.unified_diff(source.read_text().splitlines(True), text.splitlines(True),
                                           fromfile=str(source), tofile=str(path)))
        with (OUT / (name + '.diff')).open('x') as stream:
            stream.write(diff)
    for marker in ('PYREMOTE', 'PYLAUNCH', 'PY'):
        if "<<'" + marker + "'\n" in text:
            body = text.split("<<'" + marker + "'\n", 1)[1].rsplit('\n' + marker, 1)[0]
            ast.parse(body, filename=str(path) + ':' + marker)
    return path


for relative, sha, size in (
    ('candidate/base_zero_diagnostic_r1.py', 'f59455a6b25f832efd5b725224d8f16229b421f28c1556cbe4416ed24ac6f314', 19556),
    ('candidate/raw_right9_from_r14_r1.py', '406295f0bf97ca4a482241a565500d2bae1e226c6a8959bcebbd3264c50b9dfa', 12216),
    ('native-preparation/maintenance_bindings_base_zero_diagnostic_r1.json', '60c1ce479a89e198bc237cc3c29ccf3d9b9cd81434c47026b9cf2b5c1dd03e1c', 30051),
):
    raw = (OLD / relative).read_bytes()
    assert len(raw) == size and hashlib.sha256(raw).hexdigest() == sha
    manifest.append({'path': str(OLD / relative), 'sha256': sha, 'bytes': size, 'reused_frozen': True})

recipe = OUT / 'prepare_base_zero_diagnostic_r2.draft.py'
ast.parse(recipe.read_text())
software = '#!/bin/bash\n# 爸爸，R2原生软件准备，仅新PLAN和一次非UART17，不做tmux或硬件。\nset -eu\nexport PYTHONDONTWRITEBYTECODE=1\nexec ' + PYTHON + " - <<'PYREMOTE'\n" + recipe.read_text() + '\nPYREMOTE\n'
emit('minimal_software_prepare_base_zero_r2.draft.sh', software)
source = OLD / 'launcher-pane-fix/launch_base_zero_diagnostic_future.fixed-unbound.sh'
launcher = renamed(source.read_text())
assert "['tmux','set-option','-p','-t'," in launcher
assert "['tmux','set-option','-w','-t'," not in launcher
emit('launch_base_zero_diagnostic_r2.draft.sh', launcher, source)
source = OLD / 'preparation/start_base_zero_diagnostic_once_r1.draft.sh'
start = renamed(source.read_text())
start = start.replace("assert preparation['status']=='PASS_BASE_ZERO_R2_NATIVE_SOFTWARE_PREPARATION_ONLY'", "assert preparation['round']==2 and preparation['profile_run_id']=='base-zero-diagnostic-r1' and preparation['status']=='PASS_BASE_ZERO_R2_NATIVE_SOFTWARE_PREPARATION_ONLY'", 1)
start = re.sub(r"launcher=base64\.b64decode\('[^']+',validate=True\)",
               "launcher=base64.b64decode('" + base64.b64encode(launcher.encode()).decode() + "',validate=True)", start, count=1)
assert base64.b64decode(re.search(r"launcher=base64.b64decode\('([^']+)'", start)[1]).decode() == launcher
emit('start_base_zero_diagnostic_once_r2.draft.sh', start, source)
source = OLD / 'preparation/observe_base_zero_diagnostic_once_r1.draft.sh'
emit('observe_base_zero_diagnostic_once_r2.draft.sh', renamed(source.read_text()), source)
source = OLD / 'preparation/seal_base_zero_diagnostic_r1.draft.sh'
emit('seal_base_zero_diagnostic_r2.draft.sh', renamed(source.read_text()), source)

report = {
    'status': 'LOCAL_BASE_ZERO_R2_DRAFTS_AST_ONLY_NOT_EXECUTED', 'round': 2,
    'base': BASE, 'phase': PHASE, 'profile_run_id': 'base-zero-diagnostic-r1',
    'new_output': PHASE + '/base-zero-diagnostic-r2',
    'new_launch': PHASE + '/base-zero-diagnostic-launch-r2',
    'new_go': PHASE + '/base-zero-diagnostic-r2.go.json',
    'new_field': PHASE + '/field_confirmation_base_zero_r2.json',
    'new_plan': PHASE + '/full-plan-only-r2.stdout',
    'new_prepare_result': PHASE + '/prepare-result-base-zero-r2.json',
    'artifacts': manifest, 'actual_pins': '全部None，observer的pane_identity为None占位',
    'native_recipe_started': False, 'SSH': False, 'GO_created': False, 'UART': False,
    'hardware_started': False, 'candidate_helper_binding_changed': False,
    'software_recipe': '验证冻结输入与55refs→无execute生成一次新fullPLAN→一次非UART17→独占保存与真实wait/reap',
    'reused_software': 'R1同SHA10方法/默认零动作/缺GO拒绝及-p纯软件fixture，不重跑',
    'profile_semantics': 'zero_diag可单项PASS，三轮仍DEFERRED，候选总体run仍FAIL，不宣称零速合同完成',
}
with (OUT / 'generation-r2.json').open('x') as stream:
    json.dump(report, stream, ensure_ascii=False, indent=2)
    stream.write('\n')
print(json.dumps({key: value for key, value in report.items() if key != 'artifacts'}, ensure_ascii=False, indent=2))
