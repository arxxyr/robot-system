import hashlib
import io
import json
from pathlib import Path
import shutil
import tarfile

ROOT = Path('/Users/loosqk/repo/rust/robot-system')
WORK = Path('/Users/loosqk/.codex/worktrees/xlerobot-base-kinematics/robot-system')
OUT = ROOT / '.verify/xlerobot_base_kinematics_2026-09-30'
OUT.mkdir(exist_ok=False)


def record(path):
    data = path.read_bytes()
    return {'sha256': hashlib.sha256(data).hexdigest(), 'bytes': len(data)}


def write_json(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')


def copy(src, name, expected=None):
    before = record(src)
    if expected is not None:
        assert before['sha256'] == expected, src
    dst = OUT / name
    shutil.copyfile(src, dst)
    assert before == record(dst) == record(src)
    return {'path': name, **before}


refs = {}
for src, name, sha in [
    (WORK / 'target/xlerobot-base-si-orin-evidence.tar.gz', 'orin-evidence.tar.gz', '075b1802d6e3fe554945bc03f548c3cae872c079fad380d72005e078a431e977'),
    (WORK / 'target/xlerobot-base-si-orin-evidence.index.json', 'orin-evidence.index.json', '3cc17eecc6420ede7451f6ea36de254bfd761b93ce9962365573e079ae5b3618'),
    (WORK / 'target/xlerobot-base-orin-r2/summary.json', 'orin-original-summary.json', 'ffe3e31765b64af9bf24ce463c54464a16ad08b7ed1cef25abdf66bed4d24927'),
    (WORK / 'target/xlerobot-base-orin-r2/outer_archive_verification.json', 'orin-outer-verification.json', None),
]:
    refs[name] = copy(src, name, sha)

local = {}
for directory, prefix in [
    (WORK / 'target/xlerobot-base-stage-a', 'math-local'),
    (ROOT / 'target/xlerobot_base_stage_a_root', 'root-integration'),
    (ROOT / 'target/hardware_maintenance_classification/main_math_fmt.owner', 'main-fmt'),
]:
    for path in sorted(directory.rglob('*')):
        if path.is_file() and not path.is_symlink() and '__pycache__' not in path.parts:
            local[prefix + '/' + str(path.relative_to(directory))] = (path, record(path))
for rel in ['target/hardware_position_diagnosis_20260930/xlerobot_base_stage_a_plan.md',
            'target/xlerobot_base_runtime_next_plan_review.json',
            'target/xlerobot_base_runtime_next_plan_review.enumeration_correction.json',
            'target/hardware_position_diagnosis_20260930/b0_diagnostic_metadata_design_review.json']:
    path = ROOT / rel
    local['next-plans/' + path.name] = (path, record(path))
for rel, sha in [
    ('crates/robot-kinematics-xlerobot/src/base.rs', '8c49aded71bc718ee03487e5ae5fc1a59b6cf1358e099ed9d899f802234e4488'),
    ('crates/robot-kinematics-xlerobot/src/lib.rs', 'a919d5f6dc30d51b9a674a28b541025d0503381cde746e6ff52feed58112ebd1'),
]:
    path = ROOT / rel
    assert record(path)['sha256'] == sha
    local['source/' + rel] = (path, record(path))
index = {'schema': 'XLEROBOT_BASE_MATH_LOCAL_INDEX_V1',
         'files': {name: ref for name, (_, ref) in sorted(local.items())}}
write_json(OUT / 'local-evidence.index.json', index)
with tarfile.open(OUT / 'local-evidence.tar.gz', 'w:gz') as archive:
    for name, (src, ref) in sorted(local.items()):
        assert record(src) == ref
        data = src.read_bytes()
        info = tarfile.TarInfo(name)
        info.size = len(data)
        info.mode = 0o644
        archive.addfile(info, io.BytesIO(data))
    data = (OUT / 'local-evidence.index.json').read_bytes()
    info = tarfile.TarInfo('local-evidence.index.json')
    info.size = len(data)
    info.mode = 0o644
    archive.addfile(info, io.BytesIO(data))
with tarfile.open(OUT / 'local-evidence.tar.gz', 'r:gz') as archive:
    members = archive.getmembers()
    assert len(members) == len(local) + 1
    assert all(member.isfile() for member in members)
    assert len({member.name for member in members}) == len(members)
    for name, (_, ref) in local.items():
        data = archive.extractfile(name).read()
        assert {'sha256': hashlib.sha256(data).hexdigest(), 'bytes': len(data)} == ref
    assert archive.extractfile('local-evidence.index.json').read() == (OUT / 'local-evidence.index.json').read_bytes()

source = json.loads((ROOT / 'target/xlerobot_base_stage_a_root/current_source_readback.json').read_text())
assert source['status'] == 'PASS' and source['source_files'] == 590
assert source['aggregate'] == '0e580bd954d29014534c6d04707873c8e779e75ffb7b93add2a5095d136d4eaa'
shutil.copyfile(source_path := ROOT / 'target/xlerobot_base_stage_a_root/current_source_readback.json', OUT / 'root-source-readback.json')
assert record(source_path) == record(OUT / 'root-source-readback.json')
shutil.copyfile(Path(__file__), OUT / 'archive_builder.py')
verification = {'status': 'PASS', 'copies': refs, 'local_indexed_files': len(local),
                'local_members': len(local) + 1, 'source_files': 590, 'aggregate': source['aggregate'],
                'hardware_access': False, 'scope': '归档复制与逐项读回；不是新测试/硬件动作。'}
write_json(OUT / 'root-archive-verification.json', verification)
summary = {
    'schema': 'XLEROBOT_BASE_KINEMATICS_SOFTWARE_SUMMARY_V1',
    'status': 'SOFTWARE_GATES_PASS_PENDING_FINAL_BINDING_REVIEW',
    'scope': 'XLeRobot三轮纯SI运动学，供后续S1使用；未接入S0/S1执行链。',
    'motor_order': {'front': 8, 'left': 7, 'right': 9},
    'implemented': ['具名机身米/秒、弧度/秒与三轮角速度', '显式几何/最大轮角速度，无默认实测值',
                    '正逆变换及独立golden', '三轮统一比例限速/一ULP保守舍入修正',
                    '非法几何/轮序、非有限输入与中间值、比例下溢0明确拒绝'],
    'source': {'build_HEAD': '1c5758f97b7c502a38c850e8352ce970216bea53', 'files': 590,
               'aggregate': source['aggregate'], 'main_same_files': True,
               'base_sha256': '8c49aded71bc718ee03487e5ae5fc1a59b6cf1358e099ed9d899f802234e4488',
               'lib_sha256': 'a919d5f6dc30d51b9a674a28b541025d0503381cde746e6ff52feed58112ebd1',
               'old_arm_algorithm_and_API': 'UNCHANGED'},
    'software': {'actual_latest_nightly_update': True, 'nightly_commit': '5c543b0b8c73c7b72bc8284ced4fb22ead15734d',
                 'Orin_fmt': 'PASS', 'Orin_root_Clippy_all_all_targets_D_warnings': 'PASS',
                 'crate_lib_total': 30, 'new_base_subset': 21, 'existing_arm_subset': 9,
                 'failed': 0, 'ignored': 0, 'test_threads_override': None,
                 'precommit': '独立实际fmt→根Clippy/590前后同源；30不重跑',
                 'main_fmt': '实际PID81243 wait0/reaped，原件见local包'},
    'failures_preserved': ['首Orin Clippy101 similar_names，30NOT_RUN',
                           '仅cfg(test)helper局部名修复，无新allow/公共API或生产变化',
                           'postfmt首次本机审查normalize误拒，原件保留'],
    'originals': refs,
    'local_archive': {'path': 'local-evidence.tar.gz', **record(OUT / 'local-evidence.tar.gz'),
                      'indexed_files': len(local), 'regular_members': len(local) + 1},
    'local_index': {'path': 'local-evidence.index.json', **record(OUT / 'local-evidence.index.json')},
    'boundary': {'full_workspace_tests': 'NOT_RUN', 'H2_six_recipes': 'NOT_RUN',
                 'raw_conversion': 'NOT_IMPLEMENTED', 'Phase_contract': 'UNVERIFIED',
                 'odometry_and_generic_BaseKinematics_trait': 'NOT_IMPLEMENTED',
                 'base_active_hardware_motion': 'NOT_RUN', 'physical_power_cut': 'PENDING_UNKNOWN',
                 'right_H2': 'OVERALL_FAIL_UNCHANGED', 'left_wrist_return': 'FAIL_UNCHANGED',
                 'D15': 'OPEN_DEFERRED', 'deployable': False, 'UART': False, 'new_GO': False,
                 'formal_installation_changed': False, 'push': False,
                 'old_H2_seven_frozen_files': 'UNCHANGED;新HEAD用于H2须另行原生冻结'},
    'next': 'B0诊断元数据草稿独立准备，非已实现；B1/base-only唯一writer和B2混合总线仅方案，非SpecApproved。',
    'sidecars': '独审在封包后附加；Orin原summary/两轮原件及外包不回填。',
}
write_json(OUT / 'summary.json', summary)
print(json.dumps({'path': str(OUT), 'summary': record(OUT / 'summary.json'), 'local_files': len(local)}, ensure_ascii=False))
