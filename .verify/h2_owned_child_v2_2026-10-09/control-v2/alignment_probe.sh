set -eu
export PATH=/home/loosqk/.local/bin:$PATH
H2_EXPECTED_HEAD="${H2_EXPECTED_HEAD:?root 必须显式传入最终运行 HEAD}"
case "$H2_EXPECTED_HEAD" in *[!0-9a-f]*|'') exit 2;; esac
[ "${#H2_EXPECTED_HEAD}" -eq 40 ]
export H2_EXPECTED_HEAD
H2_PYTHON="$(uv python find 3.12)"
exec "$H2_PYTHON" - <<'PY'
import hashlib, importlib.util, json, os, platform, subprocess, sys, time
from pathlib import Path

base = Path('/home/loosqk/robot-checks/h2-exit-observation-20261009-r1')
root = base / 'source'
expected_head = os.environ['H2_EXPECTED_HEAD']
planned = base / 'right-h2-run-01'
out = base / 'bootstrap/s52-alignment-identity-r1.json'
source_out = base / 'bootstrap/s52-alignment-source-r1.json'
assert platform.system() == 'Linux' and platform.machine() == 'aarch64'
assert root == root.resolve(strict=True)
assert not out.exists() and not out.is_symlink()
assert not source_out.exists() and not source_out.is_symlink()
assert not planned.exists() and not planned.is_symlink()
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('artifacts', root / 'scripts/post_e_h2_artifacts.py')
artifacts = importlib.util.module_from_spec(spec)
spec.loader.exec_module(artifacts)
commands = []

def git(*args):
    argv = ['git', '-C', str(root), *args]
    result = subprocess.run(argv, capture_output=True, text=True, timeout=30,
                            env={**os.environ, 'GIT_OPTIONAL_LOCKS': '0'}, check=False)
    commands.append({'argv': argv, 'exit_code': result.returncode,
                     'stdout': result.stdout, 'stderr': result.stderr})
    assert result.returncode == 0
    return result.stdout

started_ns = time.time_ns()
head = git('rev-parse', 'HEAD').strip()
status = git('status', '--porcelain=v1', '--untracked-files=all')
assert head == expected_head and status == ''
inventory = git('ls-files', '-z', '--cached', '--others', '--exclude-standard').rstrip('\0').split('\0')
source = artifacts.snapshot_source(root, inventory)
frozen_path = Path('/home/loosqk/robot-checks/h2-exit-observation-20261009-r1/release-r1/source_before.json')
frozen = artifacts.strict_json(frozen_path.read_text())
artifacts.validate_source_manifest(frozen)
assert source == frozen and len(source['files']) == 597
assert source['source_manifest_sha256'] == '5970c9bed0ab02f99b47d88d47eb3718c6a748155ca4fafd5c18050a68ec6cf7'
assert git('rev-parse', 'HEAD').strip() == head
assert git('status', '--porcelain=v1', '--untracked-files=all') == status
assert not planned.exists() and not planned.is_symlink()
with source_out.open('x') as stream:
    json.dump(source, stream, ensure_ascii=False, indent=2)
    stream.write('\n')
record = {'schema': 'S52_ALIGNMENT_ORIN_IDENTITY_V1', 'status': 'PASS',
          'scope': '只读源码身份与冻结输入比较；不是现场确认或 GO',
          'started_at_ns': started_ns, 'recorded_at_ns': time.time_ns(),
          'identity': {'role': 'orin', 'host': os.uname().nodename,
                       'boot_id': Path('/proc/sys/kernel/random/boot_id').read_text().strip(),
                       'head': head, 'status_porcelain': status,
                       'source_manifest_sha256': source['source_manifest_sha256']},
          'source': {'path': str(source_out), 'sha256': artifacts.sha256(source_out)},
          'frozen_source': {'path': str(frozen_path), 'sha256': artifacts.sha256(frozen_path)},
          'source_files': len(source['files']), 'commands': commands,
          'planned_output_created': False}
with out.open('x') as stream:
    json.dump(record, stream, ensure_ascii=False, indent=2)
    stream.write('\n')
print(json.dumps({'status': 'PASS', 'identity': record['identity'],
                  'out': str(out), 'sha256': artifacts.sha256(out),
                  'recorded_at_ns': record['recorded_at_ns'],
                  'source': record['source'], 'frozen_source': record['frozen_source'],
                  'planned_output_created': False}, ensure_ascii=False))
PY
