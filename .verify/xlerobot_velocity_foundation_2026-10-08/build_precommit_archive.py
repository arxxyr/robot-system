"""只读校验/归档提交前门；不重跑命令，不改原 QA 或本机回收 UNKNOWN。"""
import datetime
import gzip
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path, PurePosixPath
import stat
import tarfile

ROOT = Path('/Users/loosqk/repo/rust/robot-system')
WORK = ROOT / 'target/xlerobot_velocity_foundation/precommit_20261008_r1'
OUT = ROOT / '.verify/xlerobot_velocity_foundation_2026-10-08'
REMOTE = Path('/home/loosqk/robot-checks/xlerobot-velocity-foundation-20261008-r7/precommit_20261008_r1')


def require(ok, message):
    if not ok:
        raise ValueError(message)


def record(path, capture=False):
    require(path.is_absolute() and path == path.resolve(), '拒绝链接输入')
    first = path.lstat()
    require(stat.S_ISREG(first.st_mode), '只读普通原件')
    identity = lambda v: (v.st_dev, v.st_ino, v.st_mode, v.st_size, v.st_nlink, v.st_mtime_ns, v.st_ctime_ns)
    digest = hashlib.sha256()
    chunks = []
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW), 'rb') as stream:
        require(identity(os.fstat(stream.fileno())) == identity(first), '打开身份不同')
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
            if capture:
                chunks.append(chunk)
        require(identity(os.fstat(stream.fileno())) == identity(first), '读期间身份不同')
    require(identity(path.lstat()) == identity(first), '读后身份不同')
    return {'sha256': digest.hexdigest(), 'bytes': first.st_size, 'mode': stat.S_IMODE(first.st_mode)}, identity(first), b''.join(chunks) if capture else None


def sha(path):
    return record(path)[0]['sha256']


def load(path):
    return json.loads(record(path, True)[2])


def save(path, value):
    with path.open('x') as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2)
        stream.write('\n')


spec = importlib.util.spec_from_file_location('artifacts', ROOT / 'target/xlerobot_velocity_foundation/native-r7/artifacts.py')
artifacts = importlib.util.module_from_spec(spec)
spec.loader.exec_module(artifacts)
require(sha(ROOT / 'target/xlerobot_velocity_foundation/native-r7/artifacts.py') == 'bd2b3f7bb8527cd9ecec73894171638e9cf378656b89e284a585c3baa37b8731', 'helper不同')
expected = load(ROOT / 'target/xlerobot_velocity_foundation/expected_source_r7.json')
final_inventory = (WORK / 'main_final_inventory.owner/child.log').read_text().rstrip('\0').split('\0')
final_source = artifacts.snapshot_source(ROOT, final_inventory)
require(final_source == expected and len(final_source['files']) == 590, '最终主树输入非r7同源590')

main = load(WORK / 'main_result.json')
native = WORK / 'orin-originals'
orin = load(native / 'result.json')
require(main['status'] == orin['status'] == 'PASS' and main['source_before_after'] == orin['source_before_after'] == 'PASS', '本机/原生质量门未通过')
require(load(WORK / 'main_source_before.json') == load(WORK / 'main_source_after.json') ==
        load(native / 'source_before.json') == load(native / 'source_after.json') == final_source,
        '两端590输入首/末/提交前不同')
require(main['compiler_window'] == orin['compiler_window'] == 'PASS' and
        main['compiler_before']['commit-hash'] == orin['compiler_before']['commit_hash'] ==
        '1d81eb4ad9cd207e3e638bd32b17ec4fce8412a6', '实际编译器窗口不同')
require(main['cargo_before'] == main['cargo_after'] == orin['cargo_before'] == orin['cargo_after'], '实际cargo不同')

owners = []
for directory in sorted(WORK.glob('*.owner')):
    row = load(directory / 'result.json')
    require(row['wait_returncode'] == 0 and row['reaped'] is True and not row['errors'] and not row['owner_sent_signals'] and not row['received_signals'], '本次owner没有自然回收')
    require(sha(directory / 'child.log') == row['log']['sha256'] and (directory / 'child.log').stat().st_size == row['log']['bytes'], 'owner原输出不同')
    owners.append({'path': directory.relative_to(WORK).as_posix(), **row})
require(len(owners) == 11, '不是实际8本机/3SSH或SCP的11个owner')
fmt = load(WORK / 'main_fmt.owner/result.json')
ssh = load(WORK / 'orin_precommit.owner/result.json')
require(fmt['finished_utc'] < ssh['started_utc'] and fmt['elapsed_ns'] > 0, 'fmt与原生执行顺序不同')

commands = []
require(len(orin['receipts']) == 5, '本次不是5个原生命令')
for ref in orin['receipts']:
    path = native / Path(ref['path']).relative_to(REMOTE)
    require(sha(path) == ref['sha256'], '回执SHA不同')
    row, launch = load(path), load(path.parent / 'launch.json')
    require(row['exit_code'] == 0 and row['reaped'] is True and not row['signals'] and
            not row['budget_exceeded'] and not row['session_residual_before_cleanup'] and
            not row['session_residual_after_cleanup'] and 'runner_error' not in row, '真实自然退出缺失')
    for key in ('name', 'argv', 'cwd', 'env', 'started_at', 'started_monotonic_ns',
                'timeout_seconds', 'deadline_monotonic_ns', 'pid', 'identity', 'stdout', 'stderr',
                'bootstrap_argv', 'bootstrap_sha256', 'identity_phase'):
        require(row[key] == launch[key], '启动/完成绑定不同：' + key)
    for key in ('stdout', 'stderr'):
        log = native / Path(row[key]).relative_to(REMOTE)
        require(sha(log) == row[key + '_sha256'] and log.stat().st_size == row[key + '_bytes'], '命令原日志不同')
    commands.append(row)
require([row['name'] for row in commands] == ['rustc_before', 'cargo_before', 'clippy_default', 'rustc_after', 'cargo_after'], '本次命令顺序不同')
require(commands[2]['argv'] == ['cargo', 'clippy', '--locked', '--all', '--all-targets', '--', '-D', 'warnings'], '根Clippy命令不同')
require(all(left['ended_monotonic_ns'] <= right['started_monotonic_ns'] for left, right in zip(commands, commands[1:])), '远端命令重叠')

inputs = {}
for path in sorted(WORK.rglob('*')):
    if '__pycache__' in path.parts:
        continue
    if path.is_dir():
        continue
    ref, identity, _ = record(path)
    inputs[path.relative_to(WORK).as_posix()] = {'path': path, 'ref': ref, 'identity': identity}
index = {'schema': 'XLEROBOT_PRECOMMIT_QUALITY_INDEX_V1', 'files': {name: item['ref'] for name, item in inputs.items()}}
save(OUT / 'precommit-quality.index.json', index)
index_raw = (OUT / 'precommit-quality.index.json').read_bytes()
archive_path = OUT / 'precommit-quality.tar.gz'
with archive_path.open('xb') as output:
    with tarfile.open(fileobj=output, mode='w:gz') as archive:
        for name, item in inputs.items():
            ref, identity, raw = record(item['path'], True)
            require(ref == item['ref'] and identity == item['identity'], '封包前身份漂移')
            info = tarfile.TarInfo(name)
            info.size, info.mode = len(raw), ref['mode']
            archive.addfile(info, io.BytesIO(raw))
        info = tarfile.TarInfo('precommit-quality.index.json')
        info.size, info.mode = len(index_raw), 0o644
        archive.addfile(info, io.BytesIO(index_raw))
seen = set()
with tarfile.open(archive_path, 'r:gz') as archive:
    for member in archive:
        path = PurePosixPath(member.name)
        require(not path.is_absolute() and '..' not in path.parts and str(path) == member.name and
                member.name not in seen and member.isfile(), '包路径/重复/类型不同')
        seen.add(member.name)
        raw = archive.extractfile(member).read(member.size + 1)
        if member.name == 'precommit-quality.index.json':
            require(raw == index_raw, '内外索引不同')
        else:
            require(member.name in inputs and {'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw), 'mode': member.mode} == inputs[member.name]['ref'], '原件读回不同')
require(seen == set(inputs) | {'precommit-quality.index.json'}, '包成员漏项')
with gzip.open(archive_path, 'rb') as stream:
    while stream.read(1024 * 1024):
        pass
for item in inputs.values():
    ref, identity, _ = record(item['path'])
    require(ref == item['ref'] and identity == item['identity'], '全部原件封包后身份不同')
binding = {'schema': 'XLEROBOT_PRECOMMIT_QUALITY_BINDING_V1', 'status': 'PASS_FMT_THEN_SAME_SOURCE_ROOT_CLIPPY',
           'finished_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
           'source_files': 590, 'aggregate': final_source['source_manifest_sha256'],
           'main_before_after_final_equals_orin_before_after': True,
           'compiler_commit': '1d81eb4ad9cd207e3e638bd32b17ec4fce8412a6',
           'main_compiler': main['compiler_before'], 'orin_compiler': orin['compiler_before'],
           'compiler_and_cargo_windows': 'PASS', 'cargo_version': main['cargo_before'],
           'main_fmt': fmt, 'strict_SSH_owner': ssh, 'local_owner_count': len(owners),
           'local_owners': owners, 'remote_command_count': len(commands), 'remote_commands': commands,
           'sequential_fmt_before_remote_Clippy': True,
           'hardware_READ_Orin_resource_released_after_all_SSH_SCP_reaped': True,
           'archive': {'path': archive_path.name, **record(archive_path)[0], 'indexed_files': len(inputs), 'regular_members': len(seen)},
           'index': {'path': 'precommit-quality.index.json', **record(OUT / 'precommit-quality.index.json')[0]},
           'fixed_input_identities_before_after': 'PASS',
           'old_r7_local_owner_wait_reap': 'UNKNOWN_UNCHANGED;不能用本次11个owner回收覆盖旧UNKNOWN。',
           'not_run': ['rustup update', 'feature Clippy', 'test', 'release', 'UART', '硬件GO', '安装', '服务激活', 'push'],
           'boundary': '提交前软件门追加原件；739测试引用已验收r7原件而不重跑/累计，非完整B1或硬件通过。',
           'deployable': False}
save(OUT / 'precommit_quality_binding.json', binding)
print(json.dumps({'status': binding['status'], 'local_owner_count': len(owners), 'remote_command_count': len(commands),
                  'indexed_files': len(inputs), 'source_files': 590, 'aggregate': binding['aggregate'],
                  'binding_sha256': sha(OUT / 'precommit_quality_binding.json'), 'archive': binding['archive']}, ensure_ascii=False))
