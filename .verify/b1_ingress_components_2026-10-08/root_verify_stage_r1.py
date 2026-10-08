"""独立核本阶段两个只读原件包，不执行或解包源码。"""
import hashlib
import json
import stat
import tarfile
from pathlib import Path, PurePosixPath

ROOT = Path('/Users/loosqk/repo/rust/robot-system')
BASE = ROOT / '.verify/b1_ingress_components_2026-10-08'
receipts = []
for filename, count in [('original-evidence.index.json', 1337), ('main-binding-review.index.json', 6)]:
    index_path = BASE / filename
    p = json.loads(index_path.read_text())
    rows = {r['name']: r for r in p['files']}
    assert len(rows) == len(p['files']) == count
    path = Path(p['archive']['path'])
    assert path.stat().st_size == p['archive']['bytes']
    assert hashlib.sha256(path.read_bytes()).hexdigest() == p['archive']['sha256']
    with tarfile.open(path, 'r:gz') as archive:
        members = archive.getmembers()
        assert len(members) == count and {m.name for m in members} == set(rows)
        for m in members:
            name = PurePosixPath(m.name)
            assert not name.is_absolute() and '..' not in name.parts and name.as_posix() == m.name
            row = rows[m.name]
            assert m.isfile() and m.size == row['bytes'] and m.mode == row['mode']
            assert hashlib.sha256(archive.extractfile(m).read()).hexdigest() == row['sha256']
            original = Path(row['original_path'])
            assert stat.S_ISREG(original.lstat().st_mode) and not original.is_symlink()
            assert original.stat().st_size == row['bytes'] and stat.S_IMODE(original.stat().st_mode) == row['mode']
            assert hashlib.sha256(original.read_bytes()).hexdigest() == row['sha256']
    receipts.append({'index': filename, 'members': count, 'archive_sha256': p['archive']['sha256']})
summary = json.loads((BASE / 'summary.json').read_text())
assert summary['status'] == 'MAIN_INTEGRATED_SOFTWARE_PASS' and summary['deployable'] is False
assert summary['actual_validation']['different_Rust_method_passes'] == 313
receipt = {'status': 'PASS_1343_STAGE_ORIGINAL_MEMBERS_BYTES_SHA_MODE', 'packages': receipts,
           'summary_sha256': hashlib.sha256((BASE / 'summary.json').read_bytes()).hexdigest()}
(ROOT / 'target/b1_freshness_integration_2026-10-08/root_stage_archive_review_r1.json').write_text(json.dumps(receipt, indent=2) + '\n')
print(json.dumps(receipt))
