"""爸爸，仅原生软件准备模板；新ELF及最终payload未绑定时，零child/目录创建拒绝。"""
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import time

sys.dont_write_bytecode = True
BASE = Path('/home/loosqk/robot-checks/position-return-20261010-r1')
PHASE = BASE / 'runtime-limits-r12'
PYTHON = '/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3'
# root看到本轮实际ELF身份后，才把这些None替换为已核原件；模板不得原样远端执行。
APPROVED_NEW_TOOL_REF = None
CANDIDATE_PAYLOAD = None
TEST_PAYLOAD = None
CONFIG_TEMPLATE_PAYLOAD = None
OLD_R6_GO_PAYLOAD = None
OLD_BINDING_SHA = '42e8a811c2a9eac7f89ff41a1f72aaa90e6a96ce6cd2ab5f8311bf1b179e7789'


def require(value, reason):
    if not value:
        raise ValueError(reason)


def unpack(payload):
    require(isinstance(payload, dict), 'root尚未绑定最终payload；零启动拒绝')
    raw = base64.b64decode(payload['base64'], validate=True)
    require(len(raw) == payload['bytes'] and hashlib.sha256(raw).hexdigest() == payload['sha256'],
            'payload字节或SHA变化')
    return raw


def reference(path):
    info = path.lstat()
    require(path == path.resolve() and stat.S_ISREG(info.st_mode), '原件不能是符号链接')
    raw = path.read_bytes()
    return {'path': str(path), 'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}


def new_bytes(path, raw):
    with path.open('xb') as stream:
        stream.write(raw)
        stream.flush()
        os.fsync(stream.fileno())


def save(path, value):
    new_bytes(path, (json.dumps(value, ensure_ascii=False, indent=2) + '\n').encode())


def command(name, argv, expected=0):
    started = time.time_ns()
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    stdout, stderr = child.communicate()
    code = child.wait()
    row = {'name': name, 'argv': argv, 'pid': child.pid, 'started_ns': started,
           'finished_ns': time.time_ns(), 'exit_code': code, 'wait_returncode': code,
           'reaped': True, 'signals_sent': []}
    for suffix, raw in [('stdout', stdout), ('stderr', stderr)]:
        path = PHASE / (name + '.' + suffix)
        new_bytes(path, raw)
        row[suffix] = reference(path)
    save(PHASE / (name + '.receipt.json'), row)
    require(code == expected, '原生软件命令返回错误：' + repr(row))
    return stdout, stderr, row


def main():
    require(isinstance(APPROVED_NEW_TOOL_REF, dict), '新ELF身份未绑定；零child/目录创建拒绝')
    require(APPROVED_NEW_TOOL_REF['sha256'] not in (
        '5621358b3e5e18be01b8fb94ff7fa85affda60812e27d46bf3c1d1b8978a339f',
        'f2014ee4008311e7a8725235a39f0d1b3913391f17cbd977b7441f120539114b',
        '13c97115b89d9f8780ea8b9cac80a091ddcc2b73549d34b2d5cbad83c718ffe1'), '旧工具禁止代用')
    source_raw = unpack(CANDIDATE_PAYLOAD)
    test_raw = unpack(TEST_PAYLOAD)
    template_raw = unpack(CONFIG_TEMPLATE_PAYLOAD)
    old_go_raw = unpack(OLD_R6_GO_PAYLOAD)
    # payload绑定齐全后才创建一次性软件准备目录；不覆盖既有目录，不触原生git树。
    PHASE.mkdir(mode=0o700)
    candidate = PHASE / 'readonly12_r12.py'
    test = PHASE / 'test_readonly12_r12.py'
    new_bytes(candidate, source_raw)
    new_bytes(test, test_raw)
    new_bytes(PHASE / 'config-template-r12.json', template_raw)
    config = json.loads(template_raw)
    require(config['schema'] == 'READONLY12_CONFIG_R12'
            and config['artifact_root'] == str(BASE)
            and config['output'] == str(PHASE / 'readonly12-evidence-r12')
            and config['run_id'] == 'readonly-runtime-limits-r12'
            and config['field_confirmation'] is None and config['approval'] is None, '准备配置漂移')
    script = Path(config['maintenance']['path'])
    support = Path(config['support']['path'])
    require(reference(script) == config['maintenance'] and reference(support) == config['support'],
            'd97或R4辅助原件变化')
    require(reference(script)['sha256'] == 'd97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41', '只能原d97')
    spec = importlib.util.spec_from_file_location('readonly12_prepare_original_maintenance', script)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    old_path = BASE / 'normal-read-r4/normal_read_execution_bindings_r4b.json'
    old_ref = reference(old_path)
    require(old_ref['sha256'] == OLD_BINDING_SHA and old_ref['bytes'] == 23460, '完整旧R4绑定原件变化')
    old = m.strict_json(old_path.read_text())
    for row in old['refs']:
        m.verify_ref(row)
    for side, files in old['protection'].items():
        for name, row in files.items():
            m.verify_protection_file(side, name, row)
    fresh_raw, fresh_stderr, fresh_row = command('fresh-nonuart', ['bash', str(BASE / 'normal-read-r4/nonuart_probe_r4.sh')])
    require(not fresh_stderr, '非UART快照stderr不空')
    fresh = m.strict_json(fresh_raw.decode())
    require(fresh['status'] == 'PASS_NON_UART_CURRENT_FACTS' and not fresh['errors'], '非UART前置未通过')
    mismatches = []
    for key in ('hostname', 'uid', 'gid', 'boot_id'):
        if fresh['identity_before'][key] != old['host'][key]:
            mismatches.append({'kind': 'host', 'key': key, 'old': old['host'][key], 'actual': fresh['identity_before'][key]})
    for side, row in old['usb'].items():
        actual = fresh['devices'][side]
        for key in ('device', 'resolved', 'rdev', 'device_inode', 'link_inode', 'mode', 'uid', 'gid', 'link_target'):
            if actual[key] != row[key]:
                mismatches.append({'kind': 'usb', 'side': side, 'key': key, 'old': row[key], 'actual': actual[key]})
        fields = dict(line.split('=', 1) for line in actual['udev_command']['stdout'].splitlines() if '=' in line)
        for key, wanted in [('ID_SERIAL_SHORT', row['serial']), ('DEVPATH', row['devpath'])]:
            if fields.get(key) != wanted:
                mismatches.append({'kind': 'udev', 'side': side, 'key': key, 'old': wanted, 'actual': fields.get(key)})
    if mismatches:
        report = {'status': 'IDENTITY_MISMATCH_NO_UART', 'mismatches': mismatches,
                  'requested_usb_replug': False, 'protection_mutations': 0, 'go_created': False}
        save(PHASE / 'identity-mismatch-r12.json', report)
        print(json.dumps(report, ensure_ascii=False), flush=True)
        raise ValueError('历史身份与当次快照不匹配；只报告，不自动改绑定或保护')
    for row in old['refs']:
        m.verify_ref(row)
    for side, files in old['protection'].items():
        for name, row in files.items():
            m.verify_protection_file(side, name, row)
    spec = importlib.util.spec_from_file_location('readonly12_final_candidate', candidate)
    c = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(c)
    require(c.APPROVED_TOOL == APPROVED_NEW_TOOL_REF, '最终候选没有完整绑定新ELF')
    c.tool_identity(m, APPROVED_NEW_TOOL_REF)
    bindings = {**old, 'refs': [*old['refs'], APPROVED_NEW_TOOL_REF, reference(candidate)],
                'binary_paths': [*old['binary_paths'], APPROVED_NEW_TOOL_REF['path']]}
    require(all(bindings[key] == old[key] for key in old if key not in ('refs', 'binary_paths'))
            and bindings['refs'][:-2] == old['refs'] and bindings['binary_paths'][:-1] == old['binary_paths'], '原完整身份绑定被改写')
    binding_path = PHASE / 'readonly12_bindings_r12.json'
    save(binding_path, bindings)
    save(PHASE / 'diagnostic_tool_identity_r12.json', APPROVED_NEW_TOOL_REF)
    config.update(bindings=reference(binding_path), diagnostic_tool=APPROVED_NEW_TOOL_REF)
    config_path = PHASE / 'config.readonly12.r12.json'
    save(config_path, config)
    # isolated测试root只含byteequal d97与原R6 GO反例；候选/测试/config也逐字节复制。
    fixture = PHASE / 'software-test-fixtures-r12'
    fixture.mkdir(mode=0o700)
    fixture_items = [
        ('scripts/run_hardware_maintenance.py', script.read_bytes()),
        ('.verify/position_return_2026-10-10_r6/preparation/head1-p24-wire-r6.go.json', old_go_raw),
        ('candidate/readonly12_r12.py', source_raw), ('candidate/test_readonly12_r12.py', test_raw),
        ('candidate/config-template-r12.json', template_raw)]
    fixture_refs = []
    for name, raw in fixture_items:
        path = fixture / name
        path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        new_bytes(path, raw)
        require(path.read_bytes() == raw, 'isolated fixture不是byteequal')
        fixture_refs.append(reference(path))
    memory, memory_stderr, memory_row = command('native-memory9', [PYTHON, str(fixture / 'candidate/test_readonly12_r12.py')])
    require(b'Ran 9 tests' in memory_stderr and b'\nOK\n' in memory_stderr, '不是9项内存PASS')
    default, default_stderr, default_row = command('default-plan-only', [PYTHON, str(candidate)])
    require(not default_stderr, '默认PLAN stderr不空')
    missing, missing_stderr, missing_row = command('missing-go', [PYTHON, str(candidate), '--config', str(config_path), '--execute'], expected=2)
    require(not missing and '缺当次现场/GO/绑定引用'.encode() in missing_stderr, '缺GO未在loader前拒绝')
    full, full_stderr, full_row = command('config-plan-only', [PYTHON, str(candidate), '--config', str(config_path)])
    require(not full_stderr, '完整PLAN stderr不空')
    default_plan, full_plan = m.strict_json(default.decode()), m.strict_json(full.decode())
    require(default_plan['status'] == full_plan['status'] == 'PLAN_ONLY'
            and default_plan['actions_started'] == full_plan['actions_started'] == 0
            and not default_plan['output_created'] and not full_plan['output_created'], '软件准备启动了动作或输出')
    require(len(default_plan['fixed_reads']) == len(full_plan['plan']['fixed_reads']) == 12
            and full_plan['plan_sha256'] == m.digest(full_plan['plan'])
            and full_plan['plan']['field_confirmation'] is None
            and full_plan['plan']['register_writes'] == full_plan['plan']['motion_commands'] == 0, '完整12项PLAN不符')
    require(not Path(config['output']).exists() and not (PHASE / 'readonly12-launch-record-r12').exists(), '软件准备创建了执行目录')
    for row in fixture_refs:
        m.verify_ref(row)
    c.tool_identity(m, APPROVED_NEW_TOOL_REF)
    save(PHASE / 'prepare-result-r12.json', {'status': 'PASS_READONLY12_PREPARATION_ONLY',
         'plan_sha256': full_plan['plan_sha256'], 'tool': APPROVED_NEW_TOOL_REF,
         'bindings': reference(binding_path), 'config': reference(config_path), 'candidate': reference(candidate),
         'direct_children': [fresh_row, memory_row, default_row, missing_row, full_row],
         'fixture_refs': fixture_refs, 'nonuart_commands': len(fresh['commands']), 'uart_operations': 0,
         'hardware_started': False, 'output_created': False, 'go_created': False, 'field_created': False})
    print((PHASE / 'prepare-result-r12.json').read_text(), end='', flush=True)


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print('爸爸，12项原生软件准备拒绝：' + repr(error), file=sys.stderr)
        sys.exit(2)
