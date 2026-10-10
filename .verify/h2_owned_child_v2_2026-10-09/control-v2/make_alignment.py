"""阶段提交后读取两端新证据，独占写入对齐回执；不生成现场事实或 GO。"""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--expected-head', required=True)
    parser.add_argument('--orin-identity', required=True, type=Path)
    parser.add_argument('--orin-identity-sha256', required=True)
    parser.add_argument('--frozen-source', required=True, type=Path)
    parser.add_argument('--frozen-source-sha256', required=True)
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--local-evidence', required=True, type=Path)
    args = parser.parse_args()
    root = Path('/Users/loosqk/repo/rust/robot-system')
    assert platform.system() == 'Darwin'
    assert re.fullmatch('[0-9a-f]{40}', args.expected_head)
    for path in (args.out, args.local_evidence):
        assert path.is_absolute() and path.parent.is_dir()
        assert not path.exists() and not path.is_symlink()
    assert args.out != args.local_evidence
    sys.dont_write_bytecode = True
    spec = importlib.util.spec_from_file_location('artifacts', root / 'scripts/post_e_h2_artifacts.py')
    artifacts = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(artifacts)
    orin = artifacts.read_pinned_json({'path': str(args.orin_identity), 'sha256': args.orin_identity_sha256})
    frozen = artifacts.read_pinned_json({'path': str(args.frozen_source), 'sha256': args.frozen_source_sha256})
    artifacts.validate_source_manifest(frozen)
    assert orin['schema'] == 'S52_ALIGNMENT_ORIN_IDENTITY_V1' and orin['status'] == 'PASS'
    assert orin['source_files'] == len(frozen['files']) == 597
    assert orin['planned_output_created'] is False
    assert orin['frozen_source']['sha256'] == args.frozen_source_sha256
    assert orin['identity']['head'] == args.expected_head and orin['identity']['status_porcelain'] == ''
    commands = []

    def query(argv):
        result = subprocess.run(argv, capture_output=True, text=True, timeout=30,
                                env={**os.environ, 'GIT_OPTIONAL_LOCKS': '0'}, check=False)
        commands.append({'argv': argv, 'exit_code': result.returncode,
                         'stdout': result.stdout, 'stderr': result.stderr})
        assert result.returncode == 0
        return result.stdout

    started_ns = time.time_ns()
    head = query(['git', '-C', str(root), 'rev-parse', 'HEAD']).strip()
    status = query(['git', '-C', str(root), 'status', '--porcelain=v1', '--untracked-files=all'])
    boot_id = query(['sysctl', '-n', 'kern.bootsessionuuid']).strip()
    assert head == args.expected_head and status == '' and boot_id
    names = query(['git', '-C', str(root), 'ls-files', '-z', '--cached', '--others', '--exclude-standard'])
    source = artifacts.snapshot_source(root, names.rstrip('\0').split('\0'))
    assert source == frozen
    assert source['source_manifest_sha256'] == orin['identity']['source_manifest_sha256']
    assert query(['git', '-C', str(root), 'rev-parse', 'HEAD']).strip() == head
    assert query(['git', '-C', str(root), 'status', '--porcelain=v1', '--untracked-files=all']) == status
    recorded_ns = time.time_ns()
    assert 0 < orin['recorded_at_ns'] <= recorded_ns
    local = {'role': 'local', 'host': os.uname().nodename, 'boot_id': boot_id,
             'head': head, 'status_porcelain': status,
             'source_manifest_sha256': source['source_manifest_sha256']}
    alignment = {'schema': 'P7_H2_ALIGNMENT_V1', 'expected_head': head,
                 'recorded_at_ns': recorded_ns, 'hosts': [local, orin['identity']]}
    evidence = {'scope': '只读对齐，本回执不授权现场硬件', 'started_at_ns': started_ns,
                'finished_at_ns': recorded_ns, 'commands': commands, 'source': source,
                'orin_identity': {'path': str(args.orin_identity), 'sha256': args.orin_identity_sha256},
                'frozen_source': {'path': str(args.frozen_source), 'sha256': args.frozen_source_sha256}}
    for path, value in ((args.local_evidence, evidence), (args.out, alignment)):
        with path.open('x') as stream:
            json.dump(value, stream, ensure_ascii=False, indent=2)
            stream.write('\n')
    print(json.dumps({'status': 'PASS', 'alignment': str(args.out),
                      'alignment_sha256': hashlib.sha256(args.out.read_bytes()).hexdigest(),
                      'recorded_at_ns': recorded_ns, 'local_evidence': str(args.local_evidence),
                      'source_files': len(source['files'])}, ensure_ascii=False))


if __name__ == '__main__':
    main()
