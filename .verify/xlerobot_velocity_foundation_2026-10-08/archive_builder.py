"""只归档本阶段固定原件；不运行 Cargo、SSH 或包内代码，不回填旧结果。"""
import datetime
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import stat
import tarfile

ROOT = Path('/Users/loosqk/repo/rust/robot-system')
WORK = ROOT / 'target/xlerobot_velocity_foundation'
OUT = ROOT / '.verify/xlerobot_velocity_foundation_2026-10-08'
DOCS = ('CLAUDE.md', 'docs/CHANGELOG.md', 'docs/10_verification.md',
        'team-swe/specs/post_e_comm_fault_hardening.HANDOFF.md')
SOURCE_FILES = ('crates/robot-core/src/executor_ctrl.rs',
                'crates/robot-core/src/serial_services.rs',
                'crates/robot-logging/src/lib.rs',
                'crates/robot-rpc/src/liveness.rs',
                'crates/robot-s0/src/ipc_thread.rs',
                'crates/robot-s0/src/serial_tick.rs')
AGGREGATE = '43b9d67550d885549f18cc43e5afd748a587f3d57a8800a61ab6f216dcff46c3'


def require(ok, message):
    if not ok:
        raise ValueError(message)


def encoded(value):
    return (json.dumps(value, ensure_ascii=False, indent=2, allow_nan=False) + '\n').encode()


def write(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open('xb') as stream:
        stream.write(data)


def write_json(path, value):
    write(path, encoded(value))


def identity(info):
    return (info.st_dev, info.st_ino, info.st_mode, info.st_nlink, info.st_size,
            info.st_mtime_ns, info.st_ctime_ns, info.st_uid, info.st_gid)


def snapshot(path, capture=False):
    require(path.is_absolute() and path == path.resolve() and path.is_relative_to(ROOT),
            '拒绝链接或越界路径：' + str(path))
    cursor = ROOT
    for part in path.relative_to(ROOT).parts[:-1]:
        cursor = cursor / part
        require(stat.S_ISDIR(cursor.lstat().st_mode), '拒绝链接父目录：' + str(cursor))
    before = path.lstat()
    require(stat.S_ISREG(before.st_mode), '原件不是普通文件：' + str(path))
    digest = hashlib.sha256()
    chunks = []
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW), 'rb') as stream:
        require(identity(os.fstat(stream.fileno())) == identity(before), '打开时身份漂移')
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
            if capture:
                chunks.append(chunk)
        require(identity(os.fstat(stream.fileno())) == identity(before), '读取时身份漂移')
    require(identity(path.lstat()) == identity(before), '读取后身份漂移')
    ref = {'sha256': digest.hexdigest(), 'bytes': before.st_size,
           'mode': stat.S_IMODE(before.st_mode), 'uid': before.st_uid, 'gid': before.st_gid}
    return ref, identity(before), b''.join(chunks) if capture else None


def reference(path):
    ref, _, _ = snapshot(path)
    return {'path': path.relative_to(OUT).as_posix(), **ref}


def load(path):
    _, _, raw = snapshot(path, True)
    return json.loads(raw)


native_inputs = {}
for number in range(1, 8):
    stage = WORK / f'native-r{number}'
    for src_name, dst_name in (
        ('native-evidence.tar.gz', 'evidence.tar.gz'),
        ('native-evidence.index.json', 'evidence.index.json'),
        ('native-evidence.metadata.json', 'evidence.metadata.json'),
        ('native_archive_verification.json', 'archive-verification.json'),
    ):
        native_inputs[f'native/r{number}/{dst_name}'] = stage / src_name

local_inputs = {}
excluded = []
for path in sorted(WORK.rglob('*')):
    rel = path.relative_to(WORK)
    if any(part in ('native-extracted', '__pycache__') for part in rel.parts):
        continue
    if path.is_dir():
        continue
    require(not path.is_symlink(), '本机原件出现链接：' + str(path))
    if path.name == 'source-inputs.tar.gz' or path.name.startswith('native-evidence.') or path.name.endswith('.pyc'):
        excluded.append(rel.as_posix())
        continue
    local_inputs['stage/' + rel.as_posix()] = path
for name in DOCS:
    local_inputs['project-docs-before-stage/' + name] = ROOT / name
for name in SOURCE_FILES:
    local_inputs['main-final-source/' + name] = ROOT / name

all_inputs = {**native_inputs, **local_inputs}
require(len(all_inputs) == len(native_inputs) + len(local_inputs), '输入名称重复')
before = {}
for name, path in sorted(all_inputs.items()):
    ref, state, _ = snapshot(path)
    before[name] = {'source_path': str(path), **ref, 'identity': state}

expected = load(WORK / 'expected_source_r7.json')
require(len(expected['files']) == 590 and expected['source_manifest_sha256'] == AGGREGATE,
        '冻结编译域不符')
for name in SOURCE_FILES:
    ref = before['main-final-source/' + name]
    require(ref['sha256'] == expected['files'][name]['sha256'] and
            ref['bytes'] == expected['files'][name]['bytes'], '主树六源与冻结不符：' + name)

original = load(WORK / 'native-r7/summary_resume_r2.json')
qa = load(WORK / 'native-r7/native-extracted/qa-r7/result.json')
require(qa['status'] == 'PASS' and qa['aggregate'] == AGGREGATE and
        qa['source_files'] == 590 and qa['native_git_head'] is None, 'r7原 QA 身份不同')
require(qa['source_before_after'] == qa['compiler_window'] == 'PASS', '源或编译器窗口不符')
require(qa['compiler']['commit_hash'] == '1d81eb4ad9cd207e3e638bd32b17ec4fce8412a6', '实际 nightly 身份不符')
require(len(qa['receipts']) == 13 and len(qa['tests']['distinct_library_names']) == 734 and
        qa['tests']['distinct_all_with_serial'] == 739 and qa['tests']['new_library_subset'] == 24,
        'r7原回执或测试名单不同')
require(original['lost_local_owner_count'] == 1 and
        original['local_original_qa_owner_reap_verified'] is False, '不能改写本机未知回收')

native_refs = {}
for number in range(1, 8):
    stage = WORK / f'native-r{number}'
    meta = load(stage / 'native-evidence.metadata.json')
    require(meta['status'] == 'PASS', '原生原件收集未通过')
    require(before[f'native/r{number}/evidence.tar.gz']['sha256'] == meta['archive_sha256'] and
            before[f'native/r{number}/evidence.tar.gz']['bytes'] == meta['archive_bytes'] and
            before[f'native/r{number}/evidence.index.json']['sha256'] == meta['index_sha256'],
            '原生包与原 metadata 绑定不符')
    for name, path in sorted(native_inputs.items()):
        if not name.startswith(f'native/r{number}/'):
            continue
        ref, state, raw = snapshot(path, True)
        require(ref == {key: before[name][key] for key in ref} and state == before[name]['identity'],
                '原生包复制前漂移')
        write(OUT / name, raw)
        (OUT / name).chmod(ref['mode'])
        require(snapshot(OUT / name)[0] == ref, '原生包逐字复制读回不符')
        native_refs[name] = reference(OUT / name)

index = {'schema': 'XLEROBOT_VELOCITY_FOUNDATION_LOCAL_INDEX_V1',
         'scope': '固定本机原件；七轮完整原生包另存，排除重复解包源码和 source-inputs.tar.gz。',
         'files': {name: {key: before[name][key] for key in ('sha256', 'bytes', 'mode', 'uid', 'gid')}
                   for name in sorted(local_inputs)}}
index_path = OUT / 'local-evidence.index.json'
write_json(index_path, index)
index_raw = index_path.read_bytes()
archive_path = OUT / 'local-evidence.tar.gz'
with archive_path.open('xb') as output:
    with tarfile.open(fileobj=output, mode='w:gz', format=tarfile.PAX_FORMAT) as archive:
        for name, path in sorted(local_inputs.items()):
            ref, state, raw = snapshot(path, True)
            require(ref == index['files'][name] and state == before[name]['identity'], '封包前原件漂移：' + name)
            item = tarfile.TarInfo(name)
            item.size, item.mode, item.uid, item.gid = ref['bytes'], ref['mode'], ref['uid'], ref['gid']
            archive.addfile(item, io.BytesIO(raw))
        item = tarfile.TarInfo('local-evidence.index.json')
        item.size, item.mode = len(index_raw), 0o644
        archive.addfile(item, io.BytesIO(index_raw))

seen = set()
with tarfile.open(archive_path, 'r:gz') as archive:
    for item in archive:
        path = PurePosixPath(item.name)
        require(not path.is_absolute() and '..' not in path.parts and str(path) == item.name and
                item.name not in seen and item.isfile() and not item.issym() and not item.islnk(),
                '本机包成员不安全或重复')
        seen.add(item.name)
        raw = archive.extractfile(item).read(item.size + 1)
        if item.name == 'local-evidence.index.json':
            require(raw == index_raw and item.mode == 0o644, '内外索引不符')
        else:
            require(item.name in index['files'], '额外成员')
            ref = index['files'][item.name]
            require(len(raw) == item.size == ref['bytes'] and
                    hashlib.sha256(raw).hexdigest() == ref['sha256'] and
                    (item.mode, item.uid, item.gid) == (ref['mode'], ref['uid'], ref['gid']), '成员读回不符')
require(seen == set(local_inputs) | {'local-evidence.index.json'}, '本机包存在漏项')
with gzip.open(archive_path, 'rb') as stream:
    while stream.read(1024 * 1024):
        pass

after = {}
for name, path in sorted(all_inputs.items()):
    ref, state, _ = snapshot(path)
    after[name] = {'source_path': str(path), **ref, 'identity': state}
require(before == after, '全部固定输入在封包期间身份或内容漂移')
write_json(OUT / 'input-identities.json', {'before': before, 'after': after, 'all_fixed_inputs_unchanged': True,
           'fixed_input_count': len(all_inputs), 'excluded_duplicates': excluded,
           'not_included': ['七轮 native-extracted 全部解包树；已有原始包完整包含', 'Python 字节码缓存']})
write(OUT / 'orin-original-summary.json', (WORK / 'native-r7/summary_resume_r2.json').read_bytes())
require(snapshot(OUT / 'orin-original-summary.json')[0]['sha256'] ==
        before['stage/native-r7/summary_resume_r2.json']['sha256'], '原摘要复制不符')
write_json(OUT / 'root-source-readback.json', {'status': 'PASS_SIX_CHANGED_SOURCE_READBACK',
           'source_manifest': '冻结清单590项按原生七轮包/本机包保存；此处只读回当前六处变更。',
           'aggregate': AGGREGATE, 'changed_files': {name: expected['files'][name] for name in SOURCE_FILES},
           'all_six_match_frozen_manifest': True})
write_json(OUT / 'archive-verification.json', {'status': 'PASS_ARCHIVE_INTEGRITY_ONLY',
           'native_original_package_count': 7, 'native_copies': native_refs,
           'local_indexed_files': len(local_inputs), 'local_regular_members': len(seen),
           'local_archive': reference(archive_path), 'local_index': reference(index_path),
           'fixed_input_count': len(all_inputs), 'all_input_identities_before_after': 'PASS',
           'scope': '复制/封包/逐项读回，不是新测试、硬件或本机未知owner回收证明。'})
summary = {'schema': 'XLEROBOT_VELOCITY_FOUNDATION_SOFTWARE_SUMMARY_V1',
           'status': 'SOFTWARE_GATES_PASS_PENDING_ROOT_ARCHIVE_REVIEW',
           'date': '2026-10-08', 'scope': 'Velocity入站与SerialTick归零基础；非完整底盘B1执行链。',
           'implemented': ['Velocity独立解码，严格数量/有限值/f32范围/时间窗/Zero策略，拒绝不更新latest-slot',
                           'SerialTick分开Forward/Skip/VelocityZero；独立有限值/期限检查和同数量栈上归零',
                           'Revoked后保原安全复检和独立PeriodicRead；不改变RT租约及急停门',
                           '原RR测试夹具等真实tick/心跳/pending就绪，原3秒预算与生产实现不改',
                           '最新nightly实际lint最窄修复：两处文档标识反引号及两个原test模块逐字移EOF'],
           'source': {'origin_main_head': qa['origin']['main_head'], 'origin_main_dirty_paths': qa['origin']['dirty_compilation_paths'],
                      'kind': qa['source_kind'], 'native_git_head': None, 'input_files': 590,
                      'aggregate': AGGREGATE, 'source_before_after': 'PASS',
                      'compiler': qa['compiler'], 'compiler_window': 'PASS',
                      'final_manifest_in_native_package': 'native/r7/evidence.tar.gz -> qa-r7/source_before.json'},
           'software': {'r7_original_QA_status': qa['status'], 'gates': qa['gates'],
                        'actual_latest_nightly_update': True, 'remote_command_completions': 13,
                        'remote_command_children': '原launch/completion/原始stdout/stderr已逐项核验；13次自然exit0/真实reap/无信号/无残留。',
                        'tests': qa['tests'], 'new_subset_not_added_again': True,
                        'formal_QA_direct_children_across_seven_rounds': original['direct_child_count'],
                        'local_reaped_host_owners': original['host_owner_count'],
                        'lost_local_owner_count': original['lost_local_owner_count'],
                        'lost_local_owners': original['lost_local_owners'],
                        'original_local_QA_owner_wait': 'UNKNOWN', 'original_local_QA_owner_reap': 'UNKNOWN',
                        'remote_PASS_does_not_fill_local_owner_reap': True},
           'prior_rounds': [{'number': item['number'], 'qa_status': item['qa_status'],
                            'aggregate': item['aggregate'], 'compiler': item['compiler'],
                            'compiler_window': item['compiler_window'],
                            'compiler_window_error': item['compiler_window_error']}
                           for item in original['iterations']],
           'failures_preserved': [original['prior_failure'],
                                  '原本机r7 owner/SSH丢失：旧RUNNING状态/缺result/新ps缺失旁证保留，wait/reap UNKNOWN。',
                                  'resume_r1审计KeyError runner_error原FAIL保留；resume_r2仅成功回执可缺错误字段，旧非零及超预算仍严格匹配。'],
           'separate_diagnostics': {'old_ELF_single_observation': '1PASS仅一次观察，不计739；旧RR超时具体根因仍UNCONFIRMED。',
                                    'r6_keep_going': 'exit101仅诊断采集，不是QA通过；5个原completion另存。'},
           'originals': {'native_packages': native_refs, 'local_archive': reference(archive_path),
                         'local_index': reference(index_path), 'source_identity_check': reference(OUT / 'input-identities.json'),
                         'orin_original_summary': reference(OUT / 'orin-original-summary.json')},
           'boundary': {'full_workspace_tests': 'NOT_RUN', 'release_build': 'NOT_RUN',
                        'H2_six_recipes_or_new_V2_freeze': 'NOT_RUN', 'full_B1': 'NOT_IMPLEMENTED',
                        'real_wire_deadline_gate': 'NOT_IMPLEMENTED;当前probe只检查存活，不声称真实写入前/途中期限拒绝。',
                        'base_firmware_Phase_units_zero_contract': 'UNVERIFIED',
                        'P1A_P1B': '孤立Mac软件候选，未整合；不作为本阶段已验收实现。',
                        'right_H2': 'OVERALL_FAIL_UNCHANGED', 'left_wrist_return': 'FAIL_UNCHANGED',
                        'D15': 'OPEN_DEFERRED', 'deployable': False,
                        'this_software_stage_hardware_motion': 'NOT_RUN', 'UART': False,
                        'new_hardware_GO': False, 'formal_installation_or_service_activation': False,
                        'push': False, 'formal_runtime_identity': '本软件阶段未做新的正式安装件/服务健康后验，不引用历史快照冒称当前状态。'},
           'next': '独核归档与提交前质量绑定；继续真实发布者身份/显式轮速配置和完整B1合同，真机门另行绑定新原生冻结与当次现场事实。',
           'sidecars': '独审/root验收附件后加，不回填七轮原包或原QA结果。',
           'built_at': datetime.datetime.now(datetime.timezone.utc).isoformat()}
write_json(OUT / 'summary.json', summary)
print(json.dumps({'status': summary['status'], 'summary': reference(OUT / 'summary.json'),
                  'native_packages': 7, 'local_indexed_files': len(local_inputs),
                  'fixed_inputs': len(all_inputs), 'local_archive': reference(archive_path)}, ensure_ascii=False))
