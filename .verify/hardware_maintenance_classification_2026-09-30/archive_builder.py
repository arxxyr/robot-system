import hashlib
import io
import json
import pathlib
import shutil
import stat
import tarfile

ROOT = pathlib.Path('/Users/loosqk/repo/rust/robot-system')
NATIVE = pathlib.Path('/Users/loosqk/.codex/worktrees/serial-periodic-recovery/robot-system/target')
OUT = ROOT / '.verify/hardware_maintenance_classification_2026-09-30'
OUT.mkdir(exist_ok=False)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def record(path):
    data = path.read_bytes()
    return {'sha256': digest(data), 'bytes': len(data)}


def write_json(path, data):
    path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + '\n')


def checked_copy(src, dest, expected=None):
    before = record(src)
    if expected is not None:
        assert before['sha256'] == expected, (src, before)
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(src, dest)
    assert record(dest) == before == record(src), src
    return {'path': str(dest.relative_to(OUT)), **before}


# 两份原包和原始摘要不可变；独审是封包后新增的单独附件。
copies = {}
for name, expected in [
    ('h2-trace-v2-orin-r1-evidence.tar.gz', 'db264b37ea0d00bcb23bb0eccf57a5fca6dd0befadf279d33fd2209ec3f1b83f'),
    ('h2-maintenance-orin-precommit-r1-evidence.tar.gz', '004859d28cdd5108f911a9c4de59f0b65d9fd6e791ed4c5c77bffbdd4520549d'),
]:
    copies[name] = checked_copy(NATIVE / name, OUT / name, expected)
    index_name = name.replace('.tar.gz', '.index.json')
    copies[index_name] = checked_copy(NATIVE / index_name, OUT / index_name)

for dirname, summary_sha, review_sha in [
    ('h2-trace-v2-orin-r1', '3a15ebc256b1c5d32a428336a6b703cdc1628cd9ba1c409c9c0ad7c59d3e5a09', 'b393bdc9459aa86e98b2a8a3c3454cb25194688c83820fb3b879e55733731af5'),
    ('h2-maintenance-orin-precommit-r1', '25d11a19a880932308efc3198b4de8913c7c915e5f4b005000211fe246f2e53f', '074f1e3a4bf9d80a7df20504dcd66d402b9352bbf562dd2e0e1f3773d347e5a2'),
]:
    for basename, expected in [('summary.json', summary_sha), ('independent_native_review.json', review_sha)]:
        rel = dirname + '/' + basename
        copies[rel] = checked_copy(NATIVE / rel, OUT / rel, expected)
copies['independent_outer_archive_review.json'] = checked_copy(
    NATIVE / 'independent_outer_archive_review.json', OUT / 'independent_outer_archive_review.json')
assert json.loads((OUT / 'independent_outer_archive_review.json').read_text())['status'] == 'PASS'

# 主树当前编译域再次逐项和 Orin 原件比较；维护脚本与文档不属于该域。
source_manifest = json.loads((NATIVE / 'h2-trace-v2-orin-r1/native-extracted/source-inputs-r1/source_before.json').read_text())
assert len(source_manifest['files']) == 589
source_checked = {}
for rel, expected in source_manifest['files'].items():
    path = ROOT / rel
    got = {**record(path), 'executable': bool(path.stat().st_mode & 0o111)}
    assert got == expected, (rel, got, expected)
    source_checked[rel] = got
write_json(OUT / 'root_source_readback.json', {
    'status': 'PASS', 'source_files': 589, 'source_manifest_sha256': source_manifest['source_manifest_sha256'],
    'files': source_checked, 'boundary': '只核本轮589编译域；不包括维护3源及4文档，不外推硬件。',
})

files = {}
local = ROOT / 'target/hardware_maintenance_classification'
for path in sorted(local.rglob('*')):
    if path.is_file() and not path.is_symlink() and '__pycache__' not in path.parts:
        rel = 'classification/' + str(path.relative_to(local))
        files[rel] = (path, record(path))
for rel, expected in [
    ('scripts/run_hardware_maintenance.py', '76825d205f3cda5a0bbb52897948ee961bc874855505cc91bb577d537f530bb0'),
    ('scripts/tests/test_hardware_maintenance.py', 'bcb7387b71eb2950d1a1cb9c806fde618f2026b9da5e7523d9ade7a62c7820ba'),
    ('scripts/tests/fixtures/hardware_maintenance_run02_return_error.json', 'a7bfa3fbfc3f3039be9db68973348ba4db156b74f807e57a01eecda8dfd27948'),
]:
    path = ROOT / rel
    metadata = record(path)
    assert metadata['sha256'] == expected
    files[rel] = (path, metadata)

index = {'schema': 'HARDWARE_MAINTENANCE_CLASSIFICATION_LOCAL_INDEX_V1',
         'files': {rel: metadata for rel, (_, metadata) in sorted(files.items())}}
write_json(OUT / 'local-evidence.index.json', index)
with tarfile.open(OUT / 'local-evidence.tar.gz', 'w:gz') as archive:
    for rel, (path, metadata) in sorted(files.items()):
        assert record(path) == metadata
        data = path.read_bytes()
        info = tarfile.TarInfo(rel)
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
    assert len(members) == len(files) + 1
    assert all(member.isfile() for member in members)
    assert len({member.name for member in members}) == len(members)
    for rel, (_, expected) in files.items():
        data = archive.extractfile(rel).read()
        assert {'sha256': digest(data), 'bytes': len(data)} == expected
    assert archive.extractfile('local-evidence.index.json').read() == (OUT / 'local-evidence.index.json').read_bytes()

checks = {'schema': 'HARDWARE_MAINTENANCE_ROOT_ARCHIVE_VERIFICATION_V1', 'status': 'PASS',
          'local_indexed_files': len(files), 'local_regular_members': len(files) + 1,
          'local_archive': record(OUT / 'local-evidence.tar.gz'),
          'source_files': 589, 'source_manifest_sha256': source_manifest['source_manifest_sha256'],
          'copies': copies, 'no_SSH_UART_or_hardware': True}
write_json(OUT / 'root_archive_verification.json', checks)
summary = {
    'schema': 'HARDWARE_MAINTENANCE_CLASSIFICATION_SOFTWARE_SUMMARY_V1',
    'status': 'SOFTWARE_ACCEPTED_PENDING_FINAL_BINDING_REVIEW',
    'build_HEAD': '1c5758f97b7c502a38c850e8352ce970216bea53',
    'source': {'files': 589, 'aggregate': source_manifest['source_manifest_sha256'],
               'maintenance_files_separate': 3, 'root_readback': 'root_source_readback.json'},
    'maintenance': {'distinct_methods': 35, 'local': 'PASS', 'Orin': 'PASS', 'root_same35': 'PASS_NOT_ADDITIVE',
                    'default_PLAN_ONLY': 'PASS_ZERO_ACTIONS',
                    'classification': '分别判定精度、17×3停止及每项身份后验；收尾失败保持粘性。',
                    'position_gate': '+24raw/±4raw/800ms不放宽；过期合格读值仍拒绝。',
                    'base_live': 'DEFERRED_BASE_FIRMWARE_CONTRACT_UNVERIFIED',
                    'complete_fake_execute': {'snapshot_READ': 204, 'base_WRITE': 0, 'base_enable': 0, 'overall': 'FAIL'},
                    'r4_failure_retained': 'NOT_RUN/DEFERRED三轮误入已运动目标，导致非目标漂移漏项；r5 +4接受/+5拒绝。'},
    'H2_native_V2': {'recipes': 6, 'frozen_ELF': 7, 'existing_Python': 71,
                      'owner_tests': 12, 'owner_tests_boundary': '历史194子集，非本轮194或完整workspace。',
                      'help': 7, 'mock_groups': 4, 'mock_samples_each': 2, 'tonic_tests': 6, 'tonic_scenes': 9,
                      'original_summary': 'h2-trace-v2-orin-r1/summary.json'},
    'precommit': {'actual_latest_nightly_update': True, 'nightly_commit': '5c543b0b8c73c7b72bc8284ced4fb22ead15734d',
                  'fmt': 'PASS', 'root_Clippy_all_all_targets_D_warnings': 'PASS',
                  'same_source_589': True, 'original_summary': 'h2-maintenance-orin-precommit-r1/summary.json'},
    'reviews': {'local_r5': 'PASS_ARCHIVED_IN_LOCAL_TAR',
                'native_V2': 'h2-trace-v2-orin-r1/independent_native_review.json',
                'native_additional': 'h2-maintenance-orin-precommit-r1/independent_native_review.json',
                'outer_archives': 'independent_outer_archive_review.json',
                'sidecars_boundary': '独审于外包封装后产生，作为单独附件；原包和原summary未回填。'},
    'preserved_failures': ['开发r1/r2、r4审查失败原件见local包',
                          'V2第一次SFTP参数失败原件保留在V2外包',
                          '旧run02 NEEDS_MANUAL_POWER_CUT/左腕回程+6 FAIL不改',
                          '旧右H2整体FAIL不改'],
    'hardware_boundary': {'physical_power_cut': 'PENDING_UNKNOWN', 'new_hardware_GO': False,
                          'UART': False, 'right_H2': 'OVERALL_FAIL_UNCHANGED', 'D15': 'OPEN_DEFERRED',
                          'deployable': False, 'formal_installation_changed': False, 'push': False,
                          'base_S0_S1_integration': 'NOT_IMPLEMENTED', 'isolated_math': 'SEPARATE_NEXT_STAGE'},
    'artifacts': copies,
    'local_archive': {'path': 'local-evidence.tar.gz', **record(OUT / 'local-evidence.tar.gz'),
                      'indexed_files': len(files), 'regular_members': len(files) + 1},
    'local_index': {'path': 'local-evidence.index.json', **record(OUT / 'local-evidence.index.json')},
    'root_verification': {'path': 'root_archive_verification.json', **record(OUT / 'root_archive_verification.json')},
}
write_json(OUT / 'summary.json', summary)
print(json.dumps({'path': str(OUT), 'summary': record(OUT / 'summary.json'),
                  'local_files': len(files), 'copies': len(copies)}, ensure_ascii=False))
