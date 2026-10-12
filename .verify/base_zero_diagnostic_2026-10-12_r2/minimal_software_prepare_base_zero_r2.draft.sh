#!/bin/bash
# 爸爸，R2原生软件准备，仅新PLAN和一次非UART17，不做tmux或硬件。
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
"""爸爸，R2只复用冻结R1正文，生成新PLAN并做一次非UART17；不创建现场/GO。"""
import base64
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tarfile
import time

sys.dont_write_bytecode = True
BASE = Path('/home/loosqk/robot-checks/position-return-20261010-r1')
PHASE = BASE / 'base-zero-diagnostic-r1'
PYTHON = '/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3'
OUTPUT = PHASE / 'base-zero-diagnostic-r2'
LAUNCH = PHASE / 'base-zero-diagnostic-launch-r2'
GO = PHASE / 'base-zero-diagnostic-r2.go.json'
FIELD = PHASE / 'field_confirmation_base_zero_r2.json'
PLAN = PHASE / 'full-plan-only-r2.stdout'
RESULT = PHASE / 'prepare-result-base-zero-r2.json'
CANDIDATE = PHASE / 'base_zero_diagnostic_r1.py'
HELPER = PHASE / 'helpers/raw_right9_from_r14_r1.py'
BINDING = PHASE / 'maintenance_bindings_base_zero_diagnostic_r1.json'
MAINTENANCE = BASE / 'software-facts-r1/scripts/run_hardware_maintenance.py'
NONUART = BASE / 'normal-read-r4/nonuart_probe_r4.sh'
FROZEN = {
    CANDIDATE: ('f59455a6b25f832efd5b725224d8f16229b421f28c1556cbe4416ed24ac6f314', 19556),
    HELPER: ('406295f0bf97ca4a482241a565500d2bae1e226c6a8959bcebbd3264c50b9dfa', 12216),
    BINDING: ('60c1ce479a89e198bc237cc3c29ccf3d9b9cd81434c47026b9cf2b5c1dd03e1c', 30051),
    MAINTENANCE: ('d97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41', 50168),
    PHASE / 'full-plan-only.stdout': ('1d049b491e69d0756e074cee388beaf814edb8e95013fdfce1ecc9d8d4ee0555', 38231),
    NONUART: ('e3962ec7841a9d5ed783ca8279ed718c8f29ed718701de528c226f0588b91527', 20020),
}
owned_files = []
rows = []


def ref(path):
    info = path.lstat()
    assert stat.S_ISREG(info.st_mode) and path == path.resolve()
    raw = path.read_bytes()
    return {'path': str(path), 'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}


def new(path, raw):
    with path.open('xb') as stream:
        stream.write(raw)
        stream.flush()
        os.fsync(stream.fileno())
    owned_files.append(path)


def save(path, value):
    new(path, (json.dumps(value, ensure_ascii=False, indent=2) + '\n').encode())


def command(name, argv):
    started = time.time_ns()
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    stdout, stderr = child.communicate()
    code = child.wait()
    row = {'name': name, 'argv': argv, 'pid': child.pid, 'started_ns': started,
           'finished_ns': time.time_ns(), 'exit_code': code, 'wait_returncode': code,
           'reaped': True, 'signals_sent': []}
    for suffix, raw in [('stdout', stdout), ('stderr', stderr)]:
        path = PHASE / (name + '.' + suffix)
        new(path, raw)
        row[suffix] = ref(path)
    save(PHASE / (name + '.receipt.json'), row)
    rows.append(row)
    assert code == 0 and stderr == b'', row
    return stdout


assert PHASE.is_dir() and PHASE == PHASE.resolve()
for path in (OUTPUT, LAUNCH, GO, FIELD, PLAN, RESULT,
             PHASE / 'full-plan-only-r2.stderr', PHASE / 'full-plan-only-r2.receipt.json',
             PHASE / 'fresh-nonuart-r2.stdout', PHASE / 'fresh-nonuart-r2.stderr',
             PHASE / 'fresh-nonuart-r2.receipt.json'):
    assert not os.path.lexists(path), ('爸爸，R2独占目标已存在，禁止覆盖或重试', str(path))
for path, (sha, size) in FROZEN.items():
    row = ref(path)
    assert row['sha256'] == sha and row['bytes'] == size, row
spec = importlib.util.spec_from_file_location('base_zero_r2_frozen_d97', MAINTENANCE)
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
bindings = m.strict_json(BINDING.read_text())
assert len(bindings['refs']) == 55
m.protected_and_refs(bindings)
old = m.strict_json((PHASE / 'full-plan-only.stdout').read_text())
assert old['status'] == 'PLAN_ONLY' and old['plan_sha256'] == m.digest(old['plan'])
assert old['plan']['bindings'] == bindings
argv = [PYTHON, str(CANDIDATE), '--maintenance', str(MAINTENANCE), '--raw-helper', str(HELPER),
        '--bindings', str(BINDING), '--output', str(OUTPUT), '--artifact-root', str(BASE)]
full = m.strict_json(command('full-plan-only-r2', argv).decode())
plan = full['plan']
expected_plan = dict(old['plan'])
expected_plan['output'] = str(OUTPUT)
assert full['status'] == 'PLAN_ONLY' and full['actions_started'] == 0 and plan == expected_plan
assert full['plan_sha256'] == m.digest(plan) and full['plan_sha256'] != old['plan_sha256']
assert plan['run_id'] == 'base-zero-diagnostic-r1' and plan['position_targets'] == []
assert plan['firmware_contract_verified'] is False
assert plan['base_targets'] == [
    {'side': side, 'motor': motor, 'name': name, 'decision': 'DEFERRED',
     'reason_code': 'BASE_FIRMWARE_CONTRACT_UNVERIFIED'} for side, motor, name in m.BASE_TARGETS]
trial = plan['zero_diag']
assert trial['target'] == ['right', 9, 'base_right_wheel'] and trial['firmware_contract_verified'] is False
assert (trial['goal_velocity_address'], trial['goal_velocity_width'], trial['budget_ns'],
        trial['requested_pv_samples'], trial['minimum_in_budget_pv0_samples']) == (46, 2, 500000000, 3, 3)
assert trial['sample_offsets_ns'] == [0, 200000000, 400000000]
assert trial['write_sequence'] == [['Goal_Velocity', 0], ['Torque_Enable', 1], ['Torque_Enable', 0]]
fresh = m.strict_json(command('fresh-nonuart-r2', ['bash', str(NONUART)]).decode())
assert fresh['status'] == 'PASS_NON_UART_CURRENT_FACTS' and len(fresh['checks']) == 17
assert all(row['status'] == 'PASS' for row in fresh['checks'])
assert len(fresh['commands']) == 23 and all(row['reaped'] is True for row in fresh['commands'])
assert fresh['errors'] == fresh['received_signals'] == [] and fresh['uart_open_or_read'] is False
assert fresh['hardware_writes'] == 0 and fresh['identity_before'] == fresh['identity_after']
assert fresh['elf_executions'] == 0 and fresh['formal_head'] == 'ca14f0e6cf6df8ecc6baaaa8dff2dddb64abbd0a'
assert fresh['formal_status'] == ''
assert fresh['files_before'] == fresh['files_after'] and fresh['services']['valid'] is True
for key in ('hostname', 'uid', 'gid', 'boot_id'):
    assert fresh['identity_before'][key] == bindings['host'][key], key
for side, record in bindings['usb'].items():
    actual = fresh['devices'][side]
    for key in ('device', 'resolved', 'rdev', 'device_inode', 'link_inode', 'mode', 'uid', 'gid', 'link_target'):
        assert actual[key] == record[key], (side, key)
    fields = dict(line.split('=', 1) for line in actual['udev_command']['stdout'].splitlines() if '=' in line)
    assert fields.get('ID_SERIAL_SHORT') == record['serial'] and fields.get('DEVPATH') == record['devpath']
m.protected_and_refs(bindings)
for path, (sha, size) in FROZEN.items():
    row = ref(path)
    assert row['sha256'] == sha and row['bytes'] == size, row
assert all(not os.path.lexists(path) for path in (OUTPUT, LAUNCH, GO, FIELD))
report = {
    'status': 'PASS_BASE_ZERO_R2_NATIVE_SOFTWARE_PREPARATION_ONLY', 'round': 2,
    'profile_run_id': 'base-zero-diagnostic-r1', 'output': str(OUTPUT), 'launch': str(LAUNCH),
    'new_go_path': str(GO), 'new_field_path': str(FIELD), 'plan_sha256': full['plan_sha256'],
    'plan': ref(PLAN), 'candidate': ref(CANDIDATE), 'helper': ref(HELPER), 'bindings': ref(BINDING),
    'maintenance': ref(MAINTENANCE), 'frozen_refs': [ref(path) for path in FROZEN],
    'bound_refs': 55, 'bound_refs_verified_before_after': True,
    'plan_changed_keys_from_r1': ['output'], 'direct_children': rows,
    'nonuart_checks': 17, 'nonuart_commands': 23, 'uart_operations': 0, 'motor_actions': 0,
    'parameter_writes': 0, 'go_created': False, 'field_confirmation_created': False,
    'output_created': False, 'launch_record_created': False,
    'reused_software_gates': '同SHA R1十方法/默认零动作/缺GO拒绝及-p纯软件fixture，本次未重跑',
}
save(RESULT, report)
buffer = io.BytesIO()
with tarfile.open(fileobj=buffer, mode='w:gz') as archive:
    for path in owned_files:
        archive.add(path, arcname=path.name, recursive=False)
raw = buffer.getvalue()
print(json.dumps({'status': 'BASE_ZERO_R2_NATIVE_SOFTWARE_ORIGINALS_READY', 'round': 2,
                  'archive_sha256': hashlib.sha256(raw).hexdigest(), 'archive_bytes': len(raw),
                  'archive_files': len(owned_files), 'result': report,
                  'archive_base64': base64.b64encode(raw).decode()}, ensure_ascii=False), flush=True)

PYREMOTE
