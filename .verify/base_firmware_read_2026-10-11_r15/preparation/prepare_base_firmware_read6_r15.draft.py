"""爸爸，仅R15原生软件准备草稿；复用冻结13c，无UART、现场回执或GO。"""
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
PHASE = BASE / 'base-firmware-read-r15'
PYTHON = '/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3'
APPROVED_TOOL_REF = {'path': '/home/loosqk/robot-checks/h2-right-ready-20261009-r1/readback-tool-r1/feetech_poke', 'sha256': '13c97115b89d9f8780ea8b9cac80a091ddcc2b73549d34b2d5cbad83c718ffe1', 'bytes': 1510912, 'uid': 1000, 'gid': 1000, 'mode': 509, 'device': 66305, 'inode': 16908610, 'mtime_ns': 1791532094529877671, 'ctime_ns': 1791532094529877671, 'elf_machine': 183}
INPUT_REFS = {'candidate/base_firmware_read6_r15.py': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/base-firmware-read-r15/candidate/base_firmware_read6_r15.py', 'sha256': '39506725bc4008f018e9aa8af615e8f64a6bd3a7974f3433f585599df475b88b', 'bytes': 16721}, 'candidate/test_base_firmware_read6_r15.py': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/base-firmware-read-r15/candidate/test_base_firmware_read6_r15.py', 'sha256': 'd6389ccffbab724cdb1540ce21e96ad494d3157be4820a5fc0f3f4acb11a7eeb', 'bytes': 12565}, 'candidate/config-template-r15.json': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/base-firmware-read-r15/candidate/config-template-r15.json', 'sha256': 'e5f8954fb6e49279008c0d9d9e07f04b5439074d70c35bb703afed9b5a604a4a', 'bytes': 1246}, '.verify/position_return_2026-10-10_r6/preparation/head1-p24-wire-r6.go.json': {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/base-firmware-read-r15/.verify/position_return_2026-10-10_r6/preparation/head1-p24-wire-r6.go.json', 'sha256': 'f523c7bf976bf9ae78d18c902ca333f8156e2d1c374e2a9fb61a1b688bd344a6', 'bytes': 1211}}
OLD_BINDING_REF = {'path': '/home/loosqk/robot-checks/position-return-20261010-r1/head2-p32-trial-r14/maintenance_bindings_head2_p32_r14.json', 'sha256': 'fb94a118feaf074ca495fe7311b710807668ee0eff0beba3c660b12e9183f660', 'bytes': 28598}


def require(value, reason):
    if not value:
        raise ValueError(reason)


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
    # root须先上传四个逐字节原件；本稿不封包、不联网、不重编工具。
    for expected in INPUT_REFS.values():
        require(reference(Path(expected['path'])) == expected, 'R15输入原件变化')
    candidate = PHASE / 'candidate/base_firmware_read6_r15.py'
    test = PHASE / 'candidate/test_base_firmware_read6_r15.py'
    source_raw, test_raw = candidate.read_bytes(), test.read_bytes()
    template_raw = (PHASE / 'candidate/config-template-r15.json').read_bytes()
    old_go_raw = (PHASE / '.verify/position_return_2026-10-10_r6/preparation/head1-p24-wire-r6.go.json').read_bytes()
    config = json.loads(template_raw)
    require(config['schema'] == 'BASE_FIRMWARE_READ6_CONFIG_R15'
            and config['artifact_root'] == str(BASE)
            and config['output'] == str(PHASE / 'read6-evidence-r15')
            and config['run_id'] == 'base-firmware-read6-r15'
            and config['field_confirmation'] is None and config['approval'] is None, '准备配置漂移')
    script = Path(config['maintenance']['path'])
    support = Path(config['support']['path'])
    require(reference(script) == config['maintenance'] and reference(support) == config['support'],
            'd97或R4辅助原件变化')
    require(reference(script)['sha256'] == 'd97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41', '只能原d97')
    spec = importlib.util.spec_from_file_location('base_firmware_read6_prepare_original_maintenance', script)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    old_path = Path(OLD_BINDING_REF['path'])
    old_ref = reference(old_path)
    require(old_ref == OLD_BINDING_REF, '完整R14绑定原件变化')
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
        save(PHASE / 'identity-mismatch-r15.json', report)
        print(json.dumps(report, ensure_ascii=False), flush=True)
        raise ValueError('历史身份与当次快照不匹配；只报告，不自动改绑定或保护')
    for row in old['refs']:
        m.verify_ref(row)
    for side, files in old['protection'].items():
        for name, row in files.items():
            m.verify_protection_file(side, name, row)
    spec = importlib.util.spec_from_file_location('base_firmware_read6_final_candidate', candidate)
    c = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(c)
    require(c.APPROVED_TOOL == APPROVED_TOOL_REF, '最终候选没有完整绑定冻结13cELF')
    c.tool_identity(m, APPROVED_TOOL_REF)
    require(any(all(row.get(k) == v for k, v in APPROVED_TOOL_REF.items()) for row in old['refs'])
            and APPROVED_TOOL_REF['path'] in old['binary_paths'], 'R14缺完整13c引用')
    bindings = {**old, 'refs': [*old['refs'], reference(candidate)],
                'binary_paths': [*old['binary_paths'], str(candidate)]}
    require(all(bindings[key] == old[key] for key in old if key not in ('refs', 'binary_paths'))
            and bindings['refs'][:-1] == old['refs'] and bindings['binary_paths'][:-1] == old['binary_paths'], '原完整身份绑定被改写')
    binding_path = PHASE / 'base_firmware_read6_bindings_r15.json'
    save(binding_path, bindings)
    save(PHASE / 'diagnostic_tool_identity_r15.json', APPROVED_TOOL_REF)
    config.update(bindings=reference(binding_path), diagnostic_tool=APPROVED_TOOL_REF)
    config_path = PHASE / 'config.base_firmware_read6.r15.json'
    save(config_path, config)
    # isolated测试root只含byteequal d97与原R6 GO反例；候选/测试/config也逐字节复制。
    fixture = PHASE / 'software-test-fixtures-r15'
    fixture.mkdir(mode=0o700)
    fixture_items = [
        ('scripts/run_hardware_maintenance.py', script.read_bytes()),
        ('.verify/position_return_2026-10-10_r6/preparation/head1-p24-wire-r6.go.json', old_go_raw),
        ('candidate/base_firmware_read6_r15.py', source_raw), ('candidate/test_base_firmware_read6_r15.py', test_raw),
        ('candidate/config-template-r15.json', template_raw)]
    fixture_refs = []
    for name, raw in fixture_items:
        path = fixture / name
        path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        new_bytes(path, raw)
        require(path.read_bytes() == raw, 'isolated fixture不是byteequal')
        fixture_refs.append(reference(path))
    memory, memory_stderr, memory_row = command('native-memory8', [PYTHON, str(fixture / 'candidate/test_base_firmware_read6_r15.py')])
    require(b'Ran 8 tests' in memory_stderr and b'\nOK\n' in memory_stderr, '不是8项内存PASS')
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
    require(len(default_plan['fixed_reads']) == len(full_plan['plan']['fixed_reads']) == 6
            and full_plan['plan_sha256'] == m.digest(full_plan['plan'])
            and full_plan['plan']['field_confirmation'] is None
            and full_plan['plan']['register_writes'] == full_plan['plan']['motion_commands'] == 0, '完整六项PLAN不符')
    require(not Path(config['output']).exists() and not (PHASE / 'read6-launch-record-r15').exists(), '软件准备创建了执行目录')
    for row in fixture_refs:
        m.verify_ref(row)
    c.tool_identity(m, APPROVED_TOOL_REF)
    save(PHASE / 'prepare-result-r15.json', {'status': 'PASS_BASE_FIRMWARE_READ6_PREPARATION_ONLY',
         'plan_sha256': full_plan['plan_sha256'], 'tool': APPROVED_TOOL_REF,
         'bindings': reference(binding_path), 'config': reference(config_path), 'candidate': reference(candidate),
         'direct_children': [fresh_row, memory_row, default_row, missing_row, full_row],
         'fixture_refs': fixture_refs, 'nonuart_commands': len(fresh['commands']), 'uart_operations': 0,
         'hardware_started': False, 'output_created': False, 'go_created': False, 'field_created': False})
    print((PHASE / 'prepare-result-r15.json').read_text(), end='', flush=True)


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print('爸爸，六项原生软件准备拒绝：' + repr(error), file=sys.stderr)
        sys.exit(2)
