from pathlib import Path
import hashlib
import json
import shutil
import stat
import tarfile
import re
from datetime import datetime, timezone

ROOT = Path('/Users/loosqk/repo/rust/robot-system')
WT = Path('/Users/loosqk/.codex/worktrees/feetech-wheel-firmware-contract/robot-system')
INTEGRATION = ROOT / 'target/feetech_firmware_integration_2026-10-08'
STAGE = ROOT / '.verify/feetech_firmware_contract_2026-10-08'
RESEARCH = ROOT / 'target/feetech_firmware_contract_2026-10-08'
PLAN = ROOT / 'target/xlerobot_b1_firmware_contract_20261008'
RECEIPT = ROOT / 'target/hardware_readonly_2026-10-08/selected_motion_preparation_r1/root_manual_power_cut_receipt_r1.json'


def identity(path):
    s = path.stat()
    return {'bytes': s.st_size, 'sha256': hashlib.sha256(path.read_bytes()).hexdigest(), 'mode': stat.S_IMODE(s.st_mode)}


def dump(path, data):
    with path.open('x') as f:
        json.dump(data, f, ensure_ascii=False, indent=2)
        f.write('\n')


STAGE.mkdir(exist_ok=False)
support = {}


def add_file(path, name):
    assert path.is_file() and not path.is_symlink(), path
    assert name not in support, name
    support[name] = path


def add_tree(path, prefix):
    for f in sorted(path.rglob('*')):
        if f.is_file():
            add_file(f, prefix + '/' + f.relative_to(path).as_posix())


# Mac 原件仅取直接文件；明确排除 Cargo target。
for round_name in ['firmware_contract_qa_r1', 'firmware_contract_qa_r2']:
    d = WT / 'target' / round_name
    for f in sorted(d.iterdir()):
        if f.is_file():
            add_file(f, 'mac/' + round_name + '/' + f.name)
add_tree(WT / 'target/firmware_contract_delivery_r2', 'mac/delivery_r2')
add_tree(RESEARCH, 'manufacturer')
review = json.loads((RESEARCH / 'review.json').read_text())
# 把已验证的十五个只读上游文件一起保存，不将它们当厂家单位认证。
for row in review['local_upstream']['read_scope_files']:
    f = Path(row['path'])
    observed = identity(f)
    assert observed['sha256'] == row['sha256'] and observed['bytes'] == row['bytes'], f
    add_file(f, 'upstream_local/' + f.relative_to(Path(review['local_upstream']['root'])).as_posix())
add_tree(PLAN, 'pure_design')
add_file(INTEGRATION / 'root_integration_r1.json', 'root/root_integration_r1.json')
add_file(INTEGRATION / 'root_native_archive_review_r1.json', 'root/root_native_archive_review_r1.json')
add_tree(INTEGRATION / 'independent_review_r1', 'independent_review_r1')
add_tree(INTEGRATION / 'root_precommit_fmt_r1', 'root/root_precommit_fmt_r1')
add_tree(INTEGRATION / 'root_precommit_fmt_r2', 'root/root_precommit_fmt_r2')
assert identity(RECEIPT)['sha256'] == 'c509819ccf03acffd14f6b23019f574fcafe1be147bfa0d6f6ab4d696e015fd3'
add_file(RECEIPT, 'human_receipt/root_manual_power_cut_receipt_r1.json')
# 原来要求断电的 receipt 单独保留，绝不由后来人工答复回填。
old_stop = RECEIPT.parent / 'root_hardware_session_stop_r1.json'
assert identity(old_stop)['sha256'] == json.loads(RECEIPT.read_text())['related_original_stop']['sha256']
add_file(old_stop, 'human_receipt/original_root_hardware_session_stop_r1.json')

copy_files = {
    'native-r1-original-evidence.tar.gz': INTEGRATION / 'native-r1-original-evidence.tar.gz',
    'native-r1-original-evidence.index.json': INTEGRATION / 'native-r1-original-evidence.index.json',
    'native-r1-archive-verification.json': INTEGRATION / 'native-r1-archive-verification.json',
    'native-r1-summary.json': INTEGRATION / 'native-r1/summary.json',
    'native-r1-test-roster-and-overlap.json': INTEGRATION / 'native-r1/test_roster_and_overlap.json',
    'root-integration-r1.json': INTEGRATION / 'root_integration_r1.json',
    'manufacturer-review.json': RESEARCH / 'review.json',
    'candidate-manifest-r2.json': WT / 'target/firmware_contract_delivery_r2/candidate_manifest.json',
    'manual-power-cut-receipt-r1.json': RECEIPT,
}
before = {str(p): identity(p) for p in set(support.values()) | set(copy_files.values())}
copy_rows = []
for name, source in copy_files.items():
    shutil.copy2(source, STAGE / name)
    observed = identity(STAGE / name)
    assert observed == before[str(source)]
    copy_rows.append({'name': name, 'original_path': str(source), **observed})

# 原生 tar 只流式验证，不重复解包 591 份编译输入。
native_index = json.loads((STAGE / 'native-r1-original-evidence.index.json').read_text())
native_expected = {r['name']: r for r in native_index['files']}
for row in native_index['files']:
    actual = identity(Path(row['original_path']))
    assert actual == {k: row[k] for k in ['bytes', 'sha256', 'mode']}
    before[row['original_path']] = actual
native_seen = set()
with tarfile.open(STAGE / 'native-r1-original-evidence.tar.gz', 'r:gz') as archive:
    for member in archive.getmembers():
        assert member.isfile() and member.name not in native_seen
        native_seen.add(member.name)
        expected = native_expected[member.name]
        content = archive.extractfile(member).read()
        assert member.size == expected['bytes'] and member.mode == expected['mode']
        assert hashlib.sha256(content).hexdigest() == expected['sha256']
        assert identity(Path(expected['original_path'])) == {k: expected[k] for k in ['bytes', 'sha256', 'mode']}
assert native_seen == set(native_expected)

support_rows = [{'name': name, 'original_path': str(source), **before[str(source)]} for name, source in sorted(support.items())]
support_tar = STAGE / 'supporting-original-evidence.tar.gz'
with tarfile.open(support_tar, 'x:gz') as archive:
    for row in support_rows:
        archive.add(row['original_path'], arcname=row['name'], recursive=False)
support_seen = set()
expected_support = {r['name']: r for r in support_rows}
with tarfile.open(support_tar, 'r:gz') as archive:
    for member in archive.getmembers():
        assert member.isfile() and member.name not in support_seen
        support_seen.add(member.name)
        row = expected_support[member.name]
        content = archive.extractfile(member).read()
        assert member.size == row['bytes'] and member.mode == row['mode']
        assert hashlib.sha256(content).hexdigest() == row['sha256']
assert support_seen == set(expected_support)
for path, original in before.items():
    assert identity(Path(path)) == original, path
dump(STAGE / 'supporting-original-evidence.index.json', {
    'schema': 'FEETECH_FIRMWARE_SUPPORTING_ORIGINALS_V1',
    'archive': {'name': support_tar.name, **identity(support_tar)},
    'files': support_rows,
    'source_origin_map': {str(source): {'archive': support_tar.name, 'member': name} for name, source in sorted(support.items())},
    'scope': 'Mac 两轮原件与明确后置恢复、厂家 raw 链及十五本地上游参考、纯设计历史、root 整合/fmt、独立人工断电回执；无 Cargo target、无重复展开 591 源码',
})

native = json.loads((INTEGRATION / 'native-r1/summary.json').read_text())
integration = json.loads((INTEGRATION / 'root_integration_r1.json').read_text())
source_expected = json.loads((INTEGRATION / 'native-r1/expected_source.json').read_text())
native_map = {r['original_path']: {'archive': 'native-r1-original-evidence.tar.gz', 'member': r['name']} for r in native_index['files']}
support_map = {str(source): {'archive': support_tar.name, 'member': name} for name, source in support.items()}


def typed_refs(value):
    if isinstance(value, dict):
        if isinstance(value.get('path'), str) and re.fullmatch(r'[0-9a-f]{64}', str(value.get('sha256'))):
            yield value
        for child in value.values():
            yield from typed_refs(child)
    elif isinstance(value, list):
        for child in value:
            yield from typed_refs(child)


remote = '/home/loosqk/robot-checks/feetech-firmware-contract-quality-20261008-r1/'
closed_refs = []
for origin in [INTEGRATION / 'native-r1/summary.json', INTEGRATION / 'native-r1/artifact_index.json', INTEGRATION / 'native-r1/source_export.json', INTEGRATION / 'native-r1/orin-originals/result.json', INTEGRATION / 'native-r1/orin-originals/frozen/feetech_poke.build.json', WT / 'target/firmware_contract_delivery_r2/candidate_manifest.json']:
    for item in typed_refs(json.loads(origin.read_text())):
        original = item['path']
        if original.startswith(remote + 'qa/'):
            original = str(INTEGRATION / 'native-r1/orin-originals' / original.removeprefix(remote + 'qa/'))
        elif original.startswith(remote + 'bootstrap/'):
            original = str(INTEGRATION / 'native-r1' / original.removeprefix(remote + 'bootstrap/'))
        observed = identity(Path(original))
        assert observed['sha256'] == item['sha256']
        assert 'bytes' not in item or observed['bytes'] == item['bytes']
        location = native_map.get(original) or support_map.get(original)
        if location is None and Path(original).is_relative_to(WT / 'crates'):
            relative = Path(original).relative_to(WT).as_posix()
            source_row = source_expected['files'][relative]
            assert source_row['sha256'] == observed['sha256'] and source_row['bytes'] == observed['bytes']
            location = {'archive': 'native-r1-original-evidence.tar.gz', 'member': 'source-inputs.tar.gz', 'nested_member': relative, 'nested_archive_sha256': identity(INTEGRATION / 'native-r1/source-inputs.tar.gz')['sha256']}
        assert location is not None, ('未归档引用', original)
        closed_refs.append({'origin': str(origin), 'reference': item, 'resolved_original': original, 'archived_location': location, **observed})
dump(STAGE / 'typed-reference-archive-closure.json', {
    'schema': 'FEETECH_FIRMWARE_TYPED_REFERENCE_ARCHIVE_CLOSURE_V1',
    'scope': '与独审相同六份主入口 JSON 中的 typed path/SHA 引用；不是对每份 JSON 任意字符串路径的全局闭合宣称',
    'occurrences': len(closed_refs), 'unique_resolved_files': len({r['resolved_original'] for r in closed_refs}),
    'all_declared_sha_and_bytes_match_archived_location': True, 'references': closed_refs,
})
old_hardware = ROOT / '.verify/hardware_selected_targets_2026-10-08/summary.json'
sources = [{'path': r['path'], 'bytes': r['bytes'], 'sha256': r['after_sha256']} for r in integration['changes']]
for row in sources:
    actual = identity(ROOT / row['path'])
    assert actual['bytes'] == row['bytes'] and actual['sha256'] == row['sha256']
summary = {
    'schema': 'FEETECH_FIRMWARE_CONTRACT_STAGE_ARCHIVE_V1',
    'date': '2026-10-08',
    'created_at_utc': datetime.now(timezone.utc).isoformat(),
    'archive_status': 'SOFTWARE_STAGE_ARCHIVED_PENDING_INDEPENDENT_ROOT_ACCEPTANCE',
    'deployable': False,
    'source_binding': {**native['source_binding'], 'head': integration['head_before'], 'affected_sources': sources, 'native_originals_archive': 'native-r1-original-evidence.tar.gz', 'native_members': len(native_seen), 'source_input_representation': '591 files retained once as the original source-inputs.tar.gz inside native archive; not repeatedly expanded'},
    'implemented': [
        'Firmware_Major_Version 地址 0 和 Firmware_Minor_Version 地址 1 的单字节只读名称；四个具名写 API 和 CLI 在取得租约/发包/开串口前返回具体只读错误，原 B0 Phase/CW/CCW 拒写门保持。',
        '固定三轮 role/ID、Model777/Mode1/精确预期 firmware 和 Phase 的纯 typed profile；Phase bit3=0 才有 Enable/Zero 构造正路径，Phase12 明确拒绝零速合同。预期 firmware 匹配不等于厂家适用性认证。',
        'Phase bit2 为两档 nominal RPM 单位 0.732/0.0146，速度 46/58 使用 bit15 signed-magnitude 和小端；非有限值/算术溢出/表示越界/raw_limit 超限明确拒绝，无单轮静默 clamp。',
    ],
    'unchanged_integration_boundary': '纯 profile 未接入 adapter/S0/S1/runtime；现有 adapter Position 与 raw 速度读回未变，ArmIK 保持。没有 Phase/Lock/EEPROM 写副作用。',
    'manufacturer_contract': {
        'review': 'manufacturer-review.json', 'review_status': review['status'],
        'raw_archive_prefix': 'manufacturer/', 'raw_download_count': 29,
        'public_chain': 'doc.feetech.cn HTML → app JS/manifest/chunk11 → public GET getBysrcType FT-SMS-STS-emanual-229f4476422d4059abfb1cb0 → response.data.version=181 → exact memory-table MD',
        'version181_meaning': '公开平台文档 version 字段；不是现场固件版本。官表没有给出现场 firmware 适用白名单。',
        'table_lines': {'firmware_major_minor': [18, 19], 'phase18_range': [38], 'mode1': [51], 'goal_velocity46': [66], 'lock55': [69], 'present_velocity58': [75], 'phase_unit_bit2': [102], 'phase_zero_bit3': [103]},
        'lock55_contract': 'Lock0 下写 EPROM 掉电保存，Lock1 下写 EPROM 不保存；临时维护方案要求原 Lock1 且保持1，原 Lock0 默认拒绝，未实施。',
        'firmware_field_read_on_real_device': 'NOT_RUN', 'actual_phase12_changed': False,
        'dimos_unit_6_rad_s_at_1023': 'EMPIRICAL_REFERENCE_NOT_MANUFACTURER_CALIBRATION',
    },
    'software_validation': {
        'mac_first_r1': {'status': 'FAIL', 'fmt_actual_exit': 0, 'crate_clippy_actual_exit': 101, 'reason': '新 profile 两处 match_bool lint；随后 QA 未执行，不把缺失编译器后验当漂移', 'archive_prefix': 'mac/firmware_contract_qa_r1/'},
        'mac_r2': {'status': 'PASS_SCOPED_CRATE_SOFTWARE', 'fmt': 0, 'crate_real_hw_all_target_clippy': 0, 'lib': 171, 'poke': 6, 'failed': 0, 'ignored': 0, 'archive_prefix': 'mac/firmware_contract_qa_r2/'},
        'compiler_mac': {'release': '1.101.0-nightly', 'commit_hash': native['compiler']['commit_hash'], 'host': 'aarch64-apple-darwin', 'cargo': native['cargo']},
        'native_r1': {'status': native['status'], 'fmt_and_default_and_real_hw_root_clippy': 'PASS', 'lib': 171, 'poke': 6, 'failed': 0, 'ignored': 0, 'quality_outer_children': 13, 'expected_exit2_inner_children': 3, 'distinct_native_children': 16, 'real_wait_reap': True, 'compiler': native['compiler'], 'cargo': native['cargo'], 'compiler_before_after_same': True},
        'count_policy': {'distinct_rust_methods': 177, 'new_methods_subset': 12, 'sample_count_in_one_method': 131072, 'mac_rerun_not_added_to_distinct': True, 'python_model_not_rust_tests': True},
        'release_artifact': native['release_artifact'],
        'actual_probe_argv_and_wait': native['expected_exit2_children'],
        'probe_actual_motor_id': 8,
        'help_actual_exit': 2,
        'two_firmware_write_probe_actual_exit': [2, 2],
        'zero_uart_boundary': '同源早退控制流、实际具体只读错误/exit2、不存在 dev 路径前后、mock 零 TX/lease 与 CLI preflight 测试；Orin 无 strace，本轮没有 syscall trace，不声称系统调用层的零 IO 观测。',
        'root_fmt_r1_preparation': {'status': 'FAIL_BEFORE_CARGO', 'actual_outer_exit': 1, 'cargo_children_launched': 0, 'archive_member': 'root/root_precommit_fmt_r1/preparation_failure.json'},
        'root_fmt_r2': {'status': 'PASS', 'actual_children': 5, 'real_wait_reap': True, 'source_before_after_same': True, 'archive_prefix': 'root/root_precommit_fmt_r2/'},
        'new_linux_precommit_gate': 'SEPARATE_NEW_RECEIPT_PENDING_OUTSIDE_THIS_ARCHIVE',
        'old_r7_local_owner_wait': 'UNKNOWN_PRESERVED_NOT_BACKFILLED_FROM_NATIVE_PASS',
    },
    'human_power_cut_receipt': {'file': 'manual-power-cut-receipt-r1.json', **identity(RECEIPT), 'status': 'HUMAN_REPORTED_BOTH_MOTOR_SUPPLIES_CUT_ARMS_SUPPORTED', 'received_at_utc': '2026-10-08 06:35:40 UTC', 'physical_cut_timestamp': None, 'independent_power_telemetry': None, 'new_go': False, 'watch_scope': 'ENDED', 'original_NEEDS_MANUAL_POWER_CUT_and_FAIL': 'PRESERVED'},
    'historical_hardware_boundary': {'old_package_summary': {'path': str(old_hardware), **identity(old_hardware)}, 'right_h2': 'OVERALL_FAIL', 'left_wrist': 'FAIL', 'head1': 'RETURN_FAIL_PLUS8_AT_800MS', 'head2': 'NOT_RUN', 'old_stop': 'FAIL_NEEDS_MANUAL_POWER_CUT', 'd15': 'OPEN/DEFERRED', 'deployable': False},
    'evidence_limits': [
        'Mac r1 profile 原始文件未当时另存；后来由 r2 逆转已知修复，7 个文件严格匹配原 r1 manifest 的 bytes/SHA。明确标注事后精确恢复，不冒充同期抓取。',
        'Mac r1 owner 工具 exit1 后置观察保留；未同期保存完整工具传输返回。Mac r2/native 原实际工具 wait 返回均另有原件。',
        'root fmt r1 准备失败的完整 stderr 当时只由工具返回，未完整同期落盘；本包保留准备失败事实 receipt，不重构伪原件。',
        'native run_orin.py 未执行草稿的 ELF 字段预检纠正保留；实际 run_orin_verified.py 执行，不将预检修正称运行失败。',
        '原件 JSON 中绝对路径与远端路径保留；本包 index 给出 original_path 到 tar member 映射，远端路径不是本轮归档后重新探测结果。',
    ],
    'typed_reference_archive_closure': {'report': 'typed-reference-archive-closure.json', 'entry_jsons': 6, 'occurrences': len(closed_refs), 'unique_resolved_files': len({r['resolved_original'] for r in closed_refs}), 'scope': '只六份独审入口的结构化 path/SHA 引用，7 候选源按 SHA 映射到 native archive 内 source-inputs.tar.gz 的 nested member；其他字符串路径保持历史语义，不概称全局闭合'},
    'not_done': ['真实 firmware 0/1 READ', '现场 Phase12 配置修改', 'UART或真实电机动作', 'adapter/runtime/S0/S1接入', 'H2六配方/真实RPC/RT或硬件门', '完整workspace测试', '正式安装/服务启动/部署/push', 'D15独立断电硬件实现或验收'],
    'archive': {'native_originals': 'native-r1-original-evidence.tar.gz', 'native_index': 'native-r1-original-evidence.index.json', 'native_verification': 'native-r1-archive-verification.json', 'supporting_originals': support_tar.name, 'supporting_index': 'supporting-original-evidence.index.json', 'supporting_members': len(support_rows), 'top_index': 'artifact_index.json'},
}
dump(STAGE / 'summary.json', summary)
shutil.copy2(Path(__file__), STAGE / 'build_archive_r1.py')
files = [{'name': p.name, **identity(p)} for p in sorted(STAGE.iterdir()) if p.is_file()]
dump(STAGE / 'artifact_index.json', {
    'schema': 'FEETECH_FIRMWARE_STAGE_ARTIFACTS_V1',
    'files': files, 'copied_originals': copy_rows,
    'native_original_member_origin_map': {r['original_path']: {'archive': 'native-r1-original-evidence.tar.gz', 'member': r['name']} for r in native_index['files']},
    'supporting_originals_index': 'supporting-original-evidence.index.json',
    'index_self_excluded': True,
})
for row in files:
    assert identity(STAGE / row['name']) == {k: row[k] for k in ['bytes', 'sha256', 'mode']}
for path, original in before.items():
    assert identity(Path(path)) == original, path
dump(STAGE / 'archive-verification.json', {
    'schema': 'FEETECH_FIRMWARE_STAGE_CLOSURE_V1', 'status': 'PASS_LOCAL_ARCHIVE_BYTES_SHA_MODE',
    'native_member_count': len(native_seen), 'supporting_member_count': len(support_seen),
    'no_duplicate_members': True, 'all_native_and_supporting_members_bytes_sha_mode_same': True,
    'all_selected_originals_before_after_same': True, 'selected_originals_count': len(before),
    'copies_bytes_sha_mode_same': True, 'all_indexed_top_artifacts_same': True,
    'artifact_index': {'name': 'artifact_index.json', **identity(STAGE / 'artifact_index.json')},
    'summary': {'name': 'summary.json', **identity(STAGE / 'summary.json')},
    'closure_document_self_excluded': True,
    'scope': '仅本机打包/流式校验；未展开591源码、未执行SSH/Cargo/UART、未修改旧硬件包或原PENDING receipt；本阶段尚待独审/root验收',
})
print(json.dumps({'stage': str(STAGE), 'summary': identity(STAGE / 'summary.json'), 'native_tar': identity(STAGE / 'native-r1-original-evidence.tar.gz'), 'supporting_tar': identity(support_tar), 'native_members': len(native_seen), 'supporting_members': len(support_seen)}, ensure_ascii=False, indent=2))
