"""爸爸，R4只读本轮新非UART原件并生成binding/PLAN；未绑定先拒绝，不创建GO。"""
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
NONUART_STDOUT_PATH = None
NONUART_STDOUT_EXPECTED = None
NONUART_RECEIPT_PATH = None
NONUART_RECEIPT_EXPECTED = None
assert all(type(value) is str and value and Path(value).is_absolute()
           for value in (NONUART_STDOUT_PATH, NONUART_RECEIPT_PATH)) and all(
    type(value) is tuple and len(value) == 2 and type(value[0]) is str and len(value[0]) == 64
    and type(value[1]) is int and value[1] > 0
    for value in (NONUART_STDOUT_EXPECTED, NONUART_RECEIPT_EXPECTED)), '爸爸，本轮新非UART原件未绑定，零目标I/O和child。'
BASE = Path('/home/loosqk/robot-checks/position-return-20261010-r1')
PHASE = BASE / 'base-zero-diagnostic-r1'
PYTHON = '/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3'
CANDIDATE = PHASE / 'base_zero_diagnostic_r1.py'
HELPER = PHASE / 'helpers/raw_right9_from_r14_r1.py'
OLD_BINDING = PHASE / 'maintenance_bindings_base_zero_diagnostic_r3.json'
BINDING = PHASE / 'maintenance_bindings_base_zero_diagnostic_r4.json'
MAINTENANCE = BASE / 'software-facts-r1/scripts/run_hardware_maintenance.py'
OUTPUT = PHASE / 'base-zero-diagnostic-r4'
LAUNCH = PHASE / 'base-zero-diagnostic-launch-r4'
GO = PHASE / 'base-zero-diagnostic-r4.go.json'
FIELD = PHASE / 'field_confirmation_base_zero_r4.json'
PLAN = PHASE / 'full-plan-only-r4.stdout'
RESULT = PHASE / 'prepare-result-base-zero-r4.json'
FRESH = Path(NONUART_STDOUT_PATH)
FRESH_RECEIPT = Path(NONUART_RECEIPT_PATH)
assert FRESH == PHASE / 'usb-refresh-nonuart-r4.stdout'
assert FRESH_RECEIPT == PHASE / 'usb-refresh-nonuart-r4.receipt.json'
assert NONUART_STDOUT_EXPECTED[0] != 'f497a4b2b49d94428096419cc24d034ee86913f8e6462408acd768ea28ada06e'
assert NONUART_RECEIPT_EXPECTED[0] != '6533a306d71958a3cfb9ed39c721b77de6c0ec67873deacdaed1de63663c1709'
FROZEN = {
    CANDIDATE: ('f59455a6b25f832efd5b725224d8f16229b421f28c1556cbe4416ed24ac6f314', 19556),
    HELPER: ('406295f0bf97ca4a482241a565500d2bae1e226c6a8959bcebbd3264c50b9dfa', 12216),
    OLD_BINDING: ('347a65dbfe0b3d553f7df06b413507b1491c073ced8a283f498bf7a81f51be86', 30051),
    MAINTENANCE: ('d97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41', 50168),
    PHASE / 'full-plan-only.stdout': ('1d049b491e69d0756e074cee388beaf814edb8e95013fdfce1ecc9d8d4ee0555', 38231),
    FRESH: NONUART_STDOUT_EXPECTED,
    FRESH_RECEIPT: NONUART_RECEIPT_EXPECTED,
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
             PHASE / 'full-plan-only-r4.stderr', PHASE / 'full-plan-only-r4.receipt.json',
             PHASE / 'usb-stat-before-r4.json', PHASE / 'usb-stat-after-r4.json'):
    assert not os.path.lexists(path), ('爸爸，R4独占目标已存在，禁止覆盖或重试', str(path))
verify_frozen()
spec = importlib.util.spec_from_file_location('base_zero_r4_frozen_d97', MAINTENANCE)
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
old = m.strict_json(OLD_BINDING.read_text())
assert len(old['refs']) == 55
m.protected_and_refs(old)
assert Path('/proc/sys/kernel/random/boot_id').read_text().strip() == old['host']['boot_id']
fresh = m.strict_json(FRESH.read_text())
receipt = m.strict_json(FRESH_RECEIPT.read_text())
assert type(receipt['pid']) is int and receipt['pid'] > 0
assert receipt['exit_code'] == receipt['wait_returncode'] == 0
assert receipt['reaped'] is True and receipt['signals_sent'] == []
assert receipt[FRESH.name] == {'sha256': NONUART_STDOUT_EXPECTED[0], 'bytes': NONUART_STDOUT_EXPECTED[1]}
assert fresh['status'] == 'PASS_NON_UART_CURRENT_FACTS' and fresh['read_only'] is True
assert len(fresh['checks']) == 17 and all(row['status'] == 'PASS' for row in fresh['checks'])
assert len(fresh['commands']) == 23 and all(row['reaped'] is True for row in fresh['commands'])
assert fresh['errors'] == fresh['received_signals'] == [] and fresh['uart_open_or_read'] is False
assert fresh['hardware_writes'] == fresh['elf_executions'] == 0
assert fresh['identity_before'] == fresh['identity_after'] and fresh['files_before'] == fresh['files_after']
assert fresh['services']['valid'] is True
assert fresh['formal_head'] == 'ca14f0e6cf6df8ecc6baaaa8dff2dddb64abbd0a' and fresh['formal_status'] == ''
for key, value in old['host'].items():
    assert fresh['identity_before'][key] == value, key
bindings = copy.deepcopy(old)
allowed = {'device_inode', 'link_inode', 'usec_initialized'}
changes = {}
for side, record in bindings['usb'].items():
    actual = fresh['devices'][side]
    fields = dict(line.split('=', 1) for line in actual['udev_command']['stdout'].splitlines() if '=' in line)
    assert fields.get('ID_VENDOR_ID') == '1a86' and fields.get('ID_MODEL_ID') == '55d3'
    assert fields.get('ID_USB_DRIVER') == 'cdc_acm'
    assert fields.get('ID_SERIAL_SHORT') == record['serial'] and fields.get('DEVPATH') == record['devpath']
    for key in ('device', 'resolved', 'rdev', 'mode', 'uid', 'gid', 'link_target'):
        assert actual[key] == record[key], (side, key)
    for key in ('device_inode', 'link_inode'):
        assert type(actual[key]) is int and actual[key] > 0
        record[key] = actual[key]
    usec = fields.get('USEC_INITIALIZED')
    assert type(usec) is str and usec.isascii() and usec.isdecimal() and int(usec) > 0
    record['usec_initialized'] = usec
    changed = {key for key in record if record[key] != old['usb'][side][key]}
    assert changed <= allowed and all(record[key] == old['usb'][side][key] for key in record if key not in allowed)
    changes[side] = {key: {'before': old['usb'][side][key], 'after': record[key]} for key in sorted(changed)}
assert all(bindings[key] == old[key] for key in old if key != 'usb')
before = usb_stat(bindings)
save(PHASE / 'usb-stat-before-r4.json', before)
save(BINDING, bindings)
argv = [PYTHON, str(CANDIDATE), '--maintenance', str(MAINTENANCE), '--raw-helper', str(HELPER),
        '--bindings', str(BINDING), '--output', str(OUTPUT), '--artifact-root', str(BASE)]
started = time.time_ns()
child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
stdout, stderr = child.communicate()
code = child.wait()
row = {'name': 'full-plan-only-r4', 'argv': argv, 'pid': child.pid, 'started_ns': started,
       'finished_ns': time.time_ns(), 'exit_code': code, 'wait_returncode': code,
       'reaped': True, 'signals_sent': []}
for suffix, raw in [('stdout', stdout), ('stderr', stderr)]:
    path = PHASE / ('full-plan-only-r4.' + suffix)
    new(path, raw)
    row[suffix] = ref(path)
save(PHASE / 'full-plan-only-r4.receipt.json', row)
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
save(PHASE / 'usb-stat-after-r4.json', after)
assert all(not os.path.lexists(path) for path in (OUTPUT, LAUNCH, GO, FIELD))
report = {'status': 'PASS_BASE_ZERO_R4_NATIVE_SOFTWARE_PREPARATION_ONLY', 'round': 4,
          'profile_run_id': 'base-zero-diagnostic-r1', 'output': str(OUTPUT), 'launch': str(LAUNCH),
          'new_go_path': str(GO), 'new_field_path': str(FIELD), 'plan_sha256': full['plan_sha256'],
          'plan': ref(PLAN), 'candidate': ref(CANDIDATE), 'helper': ref(HELPER), 'bindings': ref(BINDING),
          'maintenance': ref(MAINTENANCE), 'baseline_bindings': ref(OLD_BINDING),
          'current_nonuart_stdout': ref(FRESH), 'current_nonuart_receipt': ref(FRESH_RECEIPT),
          'usb_changed_fields': changes, 'bound_refs': 55, 'usb_stat_before_after_equal': True,
          'bound_refs_protection_verified_before_after': True, 'direct_children': [row],
          'nonuart_checks': 17, 'nonuart_commands': 23, 'new_nonuart_runs': 0,
          'uart_operations': 0, 'motor_actions': 0, 'parameter_writes': 0, 'go_created': False,
          'field_confirmation_created': False, 'output_created': False, 'launch_record_created': False,
          'hardware_started': False, 'physical_confirmation': '本recipe不读取/沿用R3现场或GO，不创建/续期R4现场或GO'}
save(RESULT, report)
paths = [*owned, FRESH, FRESH_RECEIPT]
buffer = io.BytesIO()
with tarfile.open(fileobj=buffer, mode='w:gz') as archive:
    for path in paths:
        archive.add(path, arcname=path.name, recursive=False)
raw = buffer.getvalue()
print(json.dumps({'status': 'BASE_ZERO_R4_NATIVE_SOFTWARE_ORIGINALS_READY', 'round': 4,
                  'archive_sha256': hashlib.sha256(raw).hexdigest(), 'archive_bytes': len(raw),
                  'archive_files': len(paths), 'result': report,
                  'archive_base64': base64.b64encode(raw).decode()}, ensure_ascii=False), flush=True)
