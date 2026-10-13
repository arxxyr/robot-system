#!/bin/bash
# 爸爸，当前输入未绑定即先拒绝；仅准备只读PLAN，不UART/GO。
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
"""爸爸，SYNC R1只读本轮新非UART原件并生成binding/PLAN；未绑定先拒绝，不创建GO。"""
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
FIELD_STAMP = None
FIELD_EXPECTED = None
assert type(FIELD_STAMP) is int and FIELD_STAMP>0 and isinstance(FIELD_EXPECTED,tuple) and len(FIELD_EXPECTED)==2, '爸爸，本轮恢复供电及新现场确认未固定，零I/O和child。'
NONUART_STDOUT_PATH = None
NONUART_STDOUT_EXPECTED = None
NONUART_RECEIPT_PATH = None
NONUART_RECEIPT_EXPECTED = None
assert all(type(value) is str and value and Path(value).is_absolute()
           for value in (NONUART_STDOUT_PATH, NONUART_RECEIPT_PATH)) and all(
    type(value) is tuple and len(value) == 2 and type(value[0]) is str and len(value[0]) == 64
    and type(value[1]) is int and value[1] > 0
    for value in (NONUART_STDOUT_EXPECTED, NONUART_RECEIPT_EXPECTED)), '爸爸，本轮新非UART原件未绑定，零目标I/O和child。'
LEGACY = Path('/home/loosqk/robot-checks/position-return-20261010-r1')
BASE = Path('/home/loosqk/robot-checks/base-zero-sync-diagnostic-r1')
PHASE = BASE
PYTHON = '/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3'
CANDIDATE = Path('/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/diagnostic-candidate/base_zero_sync_diagnostic_r1.py')
HELPER = Path('/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/diagnostic-candidate/fixtures/raw_right9_from_r14_r2.py')
SYNC_ELF = Path('/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/quality/frozen/feetech_poke')
SYNC_REF = {'path': '/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/quality/frozen/feetech_poke', 'sha256': 'b2bfedb1bc420c9957a276cd69f36f5fd47c66d0b7aaa519cf3375e257eabd94', 'bytes': 1507224}
OLD_BINDING = LEGACY / 'base-zero-diagnostic-r1/maintenance_bindings_base_zero_diagnostic_r4.json'
BINDING = PHASE / 'maintenance_bindings_base_zero_sync_diagnostic_r1.json'
MAINTENANCE = LEGACY / 'software-facts-r1/scripts/run_hardware_maintenance.py'
OUTPUT = PHASE / 'base-zero-sync-diagnostic-r1'
LAUNCH = PHASE / 'base-zero-sync-diagnostic-launch-r1'
GO = PHASE / 'base-zero-sync-diagnostic-r1.go.json'
FIELD = PHASE / 'field_confirmation_base_zero_sync_r1.json'
PLAN = PHASE / 'full-plan-only-r1.stdout'
RESULT = PHASE / 'prepare-result-base-zero-sync-r1.json'
FRESH = Path(NONUART_STDOUT_PATH)
FRESH_RECEIPT = Path(NONUART_RECEIPT_PATH)
assert FRESH == PHASE / 'usb-refresh-nonuart-r1.stdout'
assert FRESH_RECEIPT == PHASE / 'usb-refresh-nonuart-r1.receipt.json'
assert NONUART_STDOUT_EXPECTED[0] not in ('f497a4b2b49d94428096419cc24d034ee86913f8e6462408acd768ea28ada06e','928e5e978179d95a6a34436ccc0abd1ff545c65529290ea2a404e68e623fad88')
assert NONUART_RECEIPT_EXPECTED[0] not in ('6533a306d71958a3cfb9ed39c721b77de6c0ec67873deacdaed1de63663c1709','7c117798c001bda3c6fd54bd698425e3277ce19bd70d3b2316293427031010ec')
FROZEN = {
    CANDIDATE: ('3b68165786d7a603956f46843d8052a74316cb1fc061ba203dbb9737446001f7', 27496),
    SYNC_ELF: ('b2bfedb1bc420c9957a276cd69f36f5fd47c66d0b7aaa519cf3375e257eabd94', 1507224),
    HELPER: ('70d777bd5c2a19e2ce3f57e68f9abd40eaed2b8a9aa8c592e7fe53a940f1e38c', 12220),
    OLD_BINDING: ('347a65dbfe0b3d553f7df06b413507b1491c073ced8a283f498bf7a81f51be86', 30051),
    MAINTENANCE: ('d97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41', 50168),
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
             PHASE / 'full-plan-only-r1.stderr', PHASE / 'full-plan-only-r1.receipt.json',
             PHASE / 'usb-stat-before-r1.json', PHASE / 'usb-stat-after-r1.json'):
    assert not os.path.lexists(path), ('爸爸，SYNC R1独占目标已存在，禁止覆盖或重试', str(path))
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
assert receipt['field_stamp']==FIELD_STAMP and tuple(receipt['field_expected'])==FIELD_EXPECTED
assert receipt['field_scope']=='CURRENT_POWER_ON_AND_SUSPENDED_WHEELS_SYNC_ZERO_READY'
assert FIELD_STAMP<=receipt['started_ns']<receipt['finished_ns']<=time.time_ns()<FIELD_STAMP+600_000_000_000
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
helper_ref=ref(HELPER)
candidate_ref=ref(CANDIDATE)
assert helper_ref=={'path':str(HELPER),'sha256':'70d777bd5c2a19e2ce3f57e68f9abd40eaed2b8a9aa8c592e7fe53a940f1e38c','bytes':12220}
assert candidate_ref=={'path':str(CANDIDATE),'sha256':'3b68165786d7a603956f46843d8052a74316cb1fc061ba203dbb9737446001f7','bytes':27496}
assert all(not any(row['path']==item['path'] for row in bindings['refs']) for item in (helper_ref,candidate_ref))
sync_ref=ref(SYNC_ELF)
assert sync_ref==SYNC_REF
m.verify_ref(sync_ref, executable=True)
assert not any(row['path']==sync_ref['path'] for row in bindings['refs']) and sync_ref['path'] not in bindings['binary_paths']
bindings['refs'].extend([helper_ref,candidate_ref,sync_ref])
bindings['binary_paths'].append(sync_ref['path'])
assert bindings['binary_paths'][:-1]==old['binary_paths']
assert bindings['refs'][:-3]==old['refs'] and bindings['refs'][-3:]==[helper_ref,candidate_ref,sync_ref] and len(bindings['refs'])==58
before = usb_stat(bindings)
save(PHASE / 'usb-stat-before-r1.json', before)
save(BINDING, bindings)
argv = [PYTHON, str(CANDIDATE), '--maintenance', str(MAINTENANCE), '--raw-helper', str(HELPER),
        '--bindings', str(BINDING), '--output', str(OUTPUT), '--artifact-root', str(BASE)]
started = time.time_ns()
child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
stdout, stderr = child.communicate()
code = child.wait()
row = {'name': 'full-plan-only-r1', 'argv': argv, 'pid': child.pid, 'started_ns': started,
       'finished_ns': time.time_ns(), 'exit_code': code, 'wait_returncode': code,
       'reaped': True, 'signals_sent': []}
for suffix, raw in [('stdout', stdout), ('stderr', stderr)]:
    path = PHASE / ('full-plan-only-r1.' + suffix)
    new(path, raw)
    row[suffix] = ref(path)
save(PHASE / 'full-plan-only-r1.receipt.json', row)
assert code == 0 and stderr == b'', row
full = m.strict_json(stdout.decode())
spec=importlib.util.spec_from_file_location('zero_sync_r1_native_plan_candidate',CANDIDATE)
c=importlib.util.module_from_spec(spec);spec.loader.exec_module(c)
spec=importlib.util.spec_from_file_location('zero_sync_r1_native_plan_helper',HELPER)
h=importlib.util.module_from_spec(spec);spec.loader.exec_module(h)
expected=c.make_plan(m,h,bindings,OUTPUT,BASE,ref(CANDIDATE),ref(HELPER))
assert full['status']=='PLAN_ONLY' and full['actions_started']==0 and full['plan']==expected
assert full['plan_sha256']==m.digest(expected) and expected['run_id']=='base-zero-sync-diagnostic-r1'
assert expected['position_targets']==[] and expected['firmware_contract_verified'] is False
assert len(expected['base_targets'])==3 and all(row['decision']=='DEFERRED' for row in expected['base_targets'])
trial=expected['zero_diag']
assert trial['sample_offsets_ns']==[0,200000000,400000000] and trial['budget_ns']==500000000
assert trial['expected_raw']==c.EXPECTED and trial['write_sequence']==[['Goal_Velocity',0],['Torque_Enable',1],['Torque_Enable',0]]
assert trial['sync_write']==c.sync_contract() and trial['sync_write']['tool']==SYNC_REF
assert trial['sync_write']['instruction']==0x83 and trial['sync_write']['register_address']==46 and trial['sync_write']['width']==2 and trial['sync_write']['motor_ids']==[9]
assert trial['sync_write']['ack']=='NONE' and trial['sync_write']['result']=='sent_no_ACK' and trial['sync_write']['minimum_settle_ns']==50000000
m.protected_and_refs(bindings)
verify_frozen()
after = usb_stat(bindings)
assert before == after
save(PHASE / 'usb-stat-after-r1.json', after)
assert all(not os.path.lexists(path) for path in (OUTPUT, LAUNCH, GO, FIELD))
report = {'status': 'PASS_BASE_ZERO_SYNC_R1_NATIVE_RUNTIME_PREPARATION_ONLY', 'round': 1,
          'profile_run_id': 'base-zero-sync-diagnostic-r1', 'output': str(OUTPUT), 'launch': str(LAUNCH),
          'new_go_path': str(GO), 'new_field_path': str(FIELD), 'plan_sha256': full['plan_sha256'],
          'plan': ref(PLAN), 'candidate': ref(CANDIDATE), 'helper': ref(HELPER), 'sync_tool': ref(SYNC_ELF), 'bindings': ref(BINDING),
          'maintenance': ref(MAINTENANCE), 'baseline_bindings': ref(OLD_BINDING),
          'current_nonuart_stdout': ref(FRESH), 'current_nonuart_receipt': ref(FRESH_RECEIPT),
          'usb_changed_fields': changes, 'bound_refs': 58, 'usb_stat_before_after_equal': True,
          'bound_refs_protection_verified_before_after': True, 'direct_children': [row],
          'nonuart_checks': 17, 'nonuart_commands': 23, 'new_nonuart_runs': 0,
          'uart_operations': 0, 'motor_actions': 0, 'parameter_writes': 0, 'go_created': False,
          'field_confirmation_created': False, 'output_created': False, 'launch_record_created': False,
          'hardware_started': False, 'physical_confirmation': '本recipe仅接受本次恢复供电后的新非UART原件及新现场固定pin；不生成/续期GO或现场文件'}
save(RESULT, report)
paths = [*owned, FRESH, FRESH_RECEIPT]
buffer = io.BytesIO()
with tarfile.open(fileobj=buffer, mode='w:gz') as archive:
    for path in paths:
        archive.add(path, arcname=path.name, recursive=False)
raw = buffer.getvalue()
print(json.dumps({'status': 'BASE_ZERO_SYNC_R1_NATIVE_SOFTWARE_ORIGINALS_READY', 'round': 1,
                  'archive_sha256': hashlib.sha256(raw).hexdigest(), 'archive_bytes': len(raw),
                  'archive_files': len(paths), 'result': report,
                  'archive_base64': base64.b64encode(raw).decode()}, ensure_ascii=False), flush=True)

PYREMOTE
