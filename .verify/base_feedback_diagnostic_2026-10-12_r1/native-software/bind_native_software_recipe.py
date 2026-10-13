"""爸爸，仅本地按root最终冻结清单生成一次recipe；不上传、不SSH、不执行候选。"""

import argparse
import ast
import base64
import hashlib
import json
from pathlib import Path
import sys

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
EXPECTED_FILES = {
    'candidate/base_feedback_diagnostic_r1.py',
    'candidate/test_base_feedback_diagnostic_r1.py',
    'candidate/fixtures/run_hardware_maintenance.py',
    'candidate/fixtures/raw_right9_from_r14_r1.py',
    'candidate/fixtures/maintenance_bindings_base_zero_diagnostic_r4.json',
}


def pins(raw):
    return {'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}


def substitute(source, replacements):
    tree = ast.parse(source)
    changes = []
    for node in tree.body:
        if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
            name = node.targets[0].id
            if name in replacements:
                assert ast.literal_eval(node.value) is None, name
                changes.append((node.value.lineno, node.value.col_offset, node.value.end_lineno,
                                node.value.end_col_offset, repr(replacements[name])))
    assert len(changes) == len(replacements), '冻结字段必须唯一完整匹配'
    lines = source.splitlines(keepends=True)
    starts = [0]
    for line in lines:
        starts.append(starts[-1] + len(line.encode()))
    raw = source.encode()
    for first, col, last, end_col, replacement in sorted(changes, reverse=True):
        start, end = starts[first - 1] + col, starts[last - 1] + end_col
        raw = raw[:start] + replacement.encode() + raw[end:]
    result = raw.decode()
    ast.parse(result)
    return result


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--root-frozen-inputs', type=Path, required=True)
args = parser.parse_args()
freeze_raw = args.root_frozen_inputs.read_bytes()
freeze = json.loads(freeze_raw)
assert freeze['root_freeze_authorized'] is True
assert set(freeze['files']) == EXPECTED_FILES
raw_files = {}
for relative, expected in freeze['files'].items():
    path = Path(expected['local_path'])
    assert path.is_absolute() and path == path.resolve() and path.is_file() and not path.is_symlink()
    raw = path.read_bytes()
    assert pins(raw) == {key: expected[key] for key in ('sha256', 'bytes')}, relative
    raw_files[relative] = raw
frozen = {name: pins(raw) for name, raw in raw_files.items()}
verifier = substitute((HERE / 'verify_pure_base_feedback_r1.draft.py').read_text(), {
    'FROZEN_INPUTS': frozen, 'EXPECTED_METHODS': freeze['expected_methods'],
    'EXPECTED_DEFAULT': freeze['expected_default'], 'MISSING_GO_PHRASE': freeze['missing_go_phrase'],
})
raw_files['boot/verify_pure_base_feedback_r1.py'] = verifier.encode()
quality_inputs = {name: pins(raw) for name, raw in raw_files.items()}
quality = substitute((HERE / 'native_software_quality_base_feedback_r1.draft.py').read_text(), {'FROZEN_INPUTS': quality_inputs})
raw_files['boot/native_software_quality_base_feedback_r1.py'] = quality.encode()
raw_files['boot/root-frozen-inputs.json'] = freeze_raw
payload = {name: {**pins(raw), 'base64': base64.b64encode(raw).decode()} for name, raw in raw_files.items()}
wrapper_draft = (HERE / 'single_ssh_native_software_base_feedback_r1.draft.sh').read_text()
header, body = wrapper_draft.split("<<'PYREMOTE'\n", 1)
body, footer = body.rsplit('\nPYREMOTE\n', 1)
assert footer == ''
wrapper = header + "<<'PYREMOTE'\n" + substitute(body, {'payload': payload}) + '\nPYREMOTE\n'
destinations = {
    HERE / 'native_software_quality_base_feedback_r1.actual.py': quality.encode(),
    HERE / 'verify_pure_base_feedback_r1.actual.py': verifier.encode(),
    HERE / 'single_ssh_native_software_base_feedback_r1.actual.sh': wrapper.encode(),
}
assert all(not path.exists() for path in destinations), '实际recipe已经存在；禁止覆盖或重复生成'
for path, raw in destinations.items():
    with path.open('xb') as stream:
        stream.write(raw)
receipt = {'status': 'GENERATED_ONLY_NO_SSH_EXECUTION', 'root_freeze': {'path': str(args.root_frozen_inputs), **pins(freeze_raw)},
           'uploaded_files': {name: pins(raw) for name, raw in raw_files.items()},
           'recipes': {str(path): pins(raw) for path, raw in destinations.items()},
           'source_expected': '068277d843b191bc8283bab38e667b0f6a4d30e296e8c5a99a97cfe19654fcf8',
           'ssh_executions': 0, 'UART_operations': 0, 'GO_created': False}
with (HERE / 'recipe-generation-receipt.json').open('x', encoding='utf-8') as stream:
    json.dump(receipt, stream, ensure_ascii=False, indent=2)
    stream.write('\n')
print(json.dumps(receipt, ensure_ascii=False))
