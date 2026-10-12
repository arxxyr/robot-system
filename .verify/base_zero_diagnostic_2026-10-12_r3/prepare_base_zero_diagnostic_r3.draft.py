"""爸爸，R3复用一次已核非UART原件，仅更新左USB三字段并生成PLAN；不创建GO。"""
import base64
import copy
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
CANDIDATE = PHASE / 'base_zero_diagnostic_r1.py'
HELPER = PHASE / 'helpers/raw_right9_from_r14_r1.py'
OLD_BINDING = PHASE / 'maintenance_bindings_base_zero_diagnostic_r1.json'
BINDING = PHASE / 'maintenance_bindings_base_zero_diagnostic_r3.json'
MAINTENANCE = BASE / 'software-facts-r1/scripts/run_hardware_maintenance.py'
OUTPUT = PHASE / 'base-zero-diagnostic-r3'
LAUNCH = PHASE / 'base-zero-diagnostic-launch-r3'
GO = PHASE / 'base-zero-diagnostic-r3.go.json'
FIELD = PHASE / 'field_confirmation_base_zero_r3.json'
PLAN = PHASE / 'full-plan-only-r3.stdout'
RESULT = PHASE / 'prepare-result-base-zero-r3.json'
FRESH = PHASE / 'usb-refresh-nonuart-r3.stdout'
FRESH_RECEIPT = PHASE / 'usb-refresh-nonuart-r3.receipt.json'
FROZEN = {
    CANDIDATE: ('f59455a6b25f832efd5b725224d8f16229b421f28c1556cbe4416ed24ac6f314', 19556),
    HELPER: ('406295f0bf97ca4a482241a565500d2bae1e226c6a8959bcebbd3264c50b9dfa', 12216),
    OLD_BINDING: ('60c1ce479a89e198bc237cc3c29ccf3d9b9cd81434c47026b9cf2b5c1dd03e1c', 30051),
    MAINTENANCE: ('d97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41', 50168),
    PHASE / 'full-plan-only.stdout': ('1d049b491e69d0756e074cee388beaf814edb8e95013fdfce1ecc9d8d4ee0555', 38231),
    FRESH: ('f497a4b2b49d94428096419cc24d034ee86913f8e6462408acd768ea28ada06e', 176291),
    FRESH_RECEIPT: ('6533a306d71958a3cfb9ed39c721b77de6c0ec67873deacdaed1de63663c1709', 584),
}
owned = []


def ref(path):
    assert path == path.resolve() and stat.S_ISREG(path.lstat().st_mode)
    raw = path.read_bytes()
    return {'path': str(path), 'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}


def new(path, raw):
    with path.open('xb') as stream:
        stream.write(raw)
        stream.flush()
        os.fsync(stream.fileno())
    owned.append(path)


def save(path, value):
    new(path, (json.dumps(value, ensure_ascii=False, indent=2) + '\n').encode())


def verify_frozen():
    for path, (sha, size) in FROZEN.items():
        row = ref(path)
        assert row['sha256'] == sha and row['bytes'] == size, row


def usb_stat(bindings):
    result = {}
    for side, record in bindings['usb'].items():
        device = Path(record['device'])
        info, link = device.stat(), device.lstat()
        assert stat.S_ISCHR(info.st_mode)
        actual = {'device': str(device), 'resolved': str(device.resolve()), 'rdev': info.st_rdev,
                  'mode': stat.S_IMODE(info.st_mode), 'uid': info.st_uid, 'gid': info.st_gid,
                  'device_inode': info.st_ino, 'link_inode': link.st_ino, 'link_target': os.readlink(device)}
        assert all(actual[key] == record[key] for key in actual), (side, actual)
        result[side] = actual
    return result


assert PHASE.is_dir() and PHASE == PHASE.resolve()
for path in (BINDING, OUTPUT, LAUNCH, GO, FIELD, PLAN, RESULT,
             PHASE / 'full-plan-only-r3.stderr', PHASE / 'full-plan-only-r3.receipt.json',
             PHASE / 'usb-stat-before-r3.json', PHASE / 'usb-stat-after-r3.json'):
    assert not os.path.lexists(path), ('爸爸，R3独占目标已存在，禁止覆盖或重试', str(path))
verify_frozen()
spec = importlib.util.spec_from_file_location('base_zero_r3_frozen_d97', MAINTENANCE)
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
old = m.strict_json(OLD_BINDING.read_text())
assert len(old['refs']) == 55
m.protected_and_refs(old)
assert Path('/proc/sys/kernel/random/boot_id').read_text().strip() == old['host']['boot_id']
fresh = m.strict_json(FRESH.read_text())
receipt = m.strict_json(FRESH_RECEIPT.read_text())
assert receipt['pid'] == 654450 and receipt['exit_code'] == receipt['wait_returncode'] == 0
assert receipt['reaped'] is True and receipt['signals_sent'] == []
assert receipt[FRESH.name] == {'sha256': FROZEN[FRESH][0], 'bytes': FROZEN[FRESH][1]}
assert fresh['status'] == 'PASS_NON_UART_CURRENT_FACTS' and len(fresh['checks']) == 17
assert all(row['status'] == 'PASS' for row in fresh['checks'])
assert len(fresh['commands']) == 23 and all(row['reaped'] is True for row in fresh['commands'])
assert fresh['errors'] == fresh['received_signals'] == [] and fresh['uart_open_or_read'] is False
assert fresh['hardware_writes'] == fresh['elf_executions'] == 0
assert fresh['identity_before'] == fresh['identity_after'] and fresh['files_before'] == fresh['files_after']
assert fresh['services']['valid'] is True
assert fresh['formal_head'] == 'ca14f0e6cf6df8ecc6baaaa8dff2dddb64abbd0a' and fresh['formal_status'] == ''
for key in ('hostname', 'uid', 'gid', 'boot_id'):
    assert fresh['identity_before'][key] == old['host'][key], key
bindings = copy.deepcopy(old)
changes = {'device_inode': 1552, 'link_inode': 1555, 'usec_initialized': '3536208877616'}
assert {key: old['usb']['left'][key] for key in changes} == {
    'device_inode': 1544, 'link_inode': 1547, 'usec_initialized': '3368981464912'}
bindings['usb']['left'].update(changes)
for side, record in bindings['usb'].items():
    actual = fresh['devices'][side]
    fields = dict(line.split('=', 1) for line in actual['udev_command']['stdout'].splitlines() if '=' in line)
    assert fields.get('ID_VENDOR_ID') == '1a86' and fields.get('ID_MODEL_ID') == '55d3'
    assert fields.get('ID_USB_DRIVER') == 'cdc_acm'
    for key in ('device', 'resolved', 'rdev', 'mode', 'uid', 'gid', 'device_inode', 'link_inode', 'link_target'):
        assert actual[key] == record[key], (side, key)
    for key, property_name in [('serial', 'ID_SERIAL_SHORT'), ('devpath', 'DEVPATH'), ('usec_initialized', 'USEC_INITIALIZED')]:
        assert fields.get(property_name) == record[key], (side, key)
assert {key for key in old['usb']['left'] if old['usb']['left'][key] != bindings['usb']['left'][key]} == set(changes)
assert bindings['usb']['right'] == old['usb']['right']
assert all(bindings[key] == old[key] for key in old if key != 'usb')
before = usb_stat(bindings)
save(PHASE / 'usb-stat-before-r3.json', before)
save(BINDING, bindings)
argv = [PYTHON, str(CANDIDATE), '--maintenance', str(MAINTENANCE), '--raw-helper', str(HELPER),
        '--bindings', str(BINDING), '--output', str(OUTPUT), '--artifact-root', str(BASE)]
started = time.time_ns()
child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
stdout, stderr = child.communicate()
code = child.wait()
row = {'name': 'full-plan-only-r3', 'argv': argv, 'pid': child.pid, 'started_ns': started,
       'finished_ns': time.time_ns(), 'exit_code': code, 'wait_returncode': code,
       'reaped': True, 'signals_sent': []}
for suffix, raw in [('stdout', stdout), ('stderr', stderr)]:
    path = PHASE / ('full-plan-only-r3.' + suffix)
    new(path, raw)
    row[suffix] = ref(path)
save(PHASE / 'full-plan-only-r3.receipt.json', row)
assert code == 0 and stderr == b'', row
full = m.strict_json(stdout.decode())
old_plan = m.strict_json((PHASE / 'full-plan-only.stdout').read_text())['plan']
expected = copy.deepcopy(old_plan)
expected.update(output=str(OUTPUT), bindings=bindings)
assert full['status'] == 'PLAN_ONLY' and full['actions_started'] == 0 and full['plan'] == expected
assert full['plan_sha256'] == m.digest(expected) and expected['run_id'] == 'base-zero-diagnostic-r1'
assert expected['position_targets'] == [] and expected['firmware_contract_verified'] is False
assert all(row['decision'] == 'DEFERRED' for row in expected['base_targets']) and len(expected['base_targets']) == 3
assert expected['zero_diag']['sample_offsets_ns'] == [0, 200000000, 400000000]
assert expected['zero_diag']['budget_ns'] == 500000000
assert expected['zero_diag']['write_sequence'] == [['Goal_Velocity', 0], ['Torque_Enable', 1], ['Torque_Enable', 0]]
m.protected_and_refs(bindings)
verify_frozen()
after = usb_stat(bindings)
assert before == after
save(PHASE / 'usb-stat-after-r3.json', after)
assert all(not os.path.lexists(path) for path in (OUTPUT, LAUNCH, GO, FIELD))
report = {'status': 'PASS_BASE_ZERO_R3_NATIVE_SOFTWARE_PREPARATION_ONLY', 'round': 3,
          'profile_run_id': 'base-zero-diagnostic-r1', 'output': str(OUTPUT), 'launch': str(LAUNCH),
          'new_go_path': str(GO), 'new_field_path': str(FIELD), 'plan_sha256': full['plan_sha256'],
          'plan': ref(PLAN), 'candidate': ref(CANDIDATE), 'helper': ref(HELPER), 'bindings': ref(BINDING),
          'maintenance': ref(MAINTENANCE), 'old_bindings': ref(OLD_BINDING),
          'reused_nonuart_stdout': ref(FRESH), 'reused_nonuart_receipt': ref(FRESH_RECEIPT),
          'usb_changed_fields': {'left': changes, 'right': {}}, 'bound_refs': 55,
          'usb_stat_before_after_equal': True, 'bound_refs_protection_verified_before_after': True,
          'direct_children': [row], 'nonuart_checks': 17, 'nonuart_commands': 23,
          'new_nonuart_runs': 0, 'uart_operations': 0, 'motor_actions': 0, 'parameter_writes': 0,
          'go_created': False, 'field_confirmation_created': False, 'output_created': False,
          'launch_record_created': False, 'hardware_started': False,
          'field_ttl': '仅软件准备，不新建/续期05:29:52UTC的现场确认或GO'}
save(RESULT, report)
paths = [*owned, FRESH, FRESH_RECEIPT]
buffer = io.BytesIO()
with tarfile.open(fileobj=buffer, mode='w:gz') as archive:
    for path in paths:
        archive.add(path, arcname=path.name, recursive=False)
raw = buffer.getvalue()
print(json.dumps({'status': 'BASE_ZERO_R3_NATIVE_SOFTWARE_ORIGINALS_READY', 'round': 3,
                  'archive_sha256': hashlib.sha256(raw).hexdigest(), 'archive_bytes': len(raw),
                  'archive_files': len(paths), 'result': report,
                  'archive_base64': base64.b64encode(raw).decode()}, ensure_ascii=False), flush=True)
