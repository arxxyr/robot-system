import hashlib
import json
import pathlib
import tarfile

root = pathlib.Path('/Users/loosqk/repo/rust/robot-system')
phase = root / 'target/position_return_2026-10-11_r17'
receipt = json.loads((phase / 'native-quality/quality-archive-receipt-r17.json').read_text())
archive = phase / 'native-quality/commit-quality-originals-r17.tar.gz'
meta = receipt['archive_metadata']
assert archive.stat().st_size == meta['archive_bytes']
assert hashlib.sha256(archive.read_bytes()).hexdigest() == meta['archive_sha256']
with tarfile.open(archive) as tar:
    members = {m.name: tar.extractfile(m).read() for m in tar.getmembers() if m.isfile()}
assert len(members) == meta['archive_files'] == 53
receipts = receipt['receipts']
assert len(receipts) == len({r['pid'] for r in receipts}) == 11
for item in receipts:
    assert item['exit_code'] == 0 and item['reaped'] and not item['signals']
    assert not item['session_residual_before_cleanup'] and not item['session_residual_after_cleanup']
    for stream in ('stdout', 'stderr'):
        data = members[f"quality/{item['name']}/{stream}.log"]
        assert len(data) == item[f'{stream}_bytes']
        assert hashlib.sha256(data).hexdigest() == item[f'{stream}_sha256']
fmt = next(r for r in receipts if r['name'] == 'fmt')
clippy = next(r for r in receipts if r['name'] == 'clippy')
assert fmt['argv'] == ['cargo', 'fmt', '--all']
assert clippy['argv'] == ['cargo', 'clippy', '--all', '--all-targets', '--', '-D', 'warnings']
assert fmt['ended_monotonic_ns'] < clippy['started_monotonic_ns']
warning_lines = [line for line in members['quality/clippy/stderr.log'].decode().splitlines() if line.lstrip().startswith('warning:')]
assert warning_lines == []
windows = [json.loads(members[f'quality/{name}.json']) for name in ('source_before', 'fmt_after', 'source_after')]
assert windows[0] == windows[1] == windows[2]
source = windows[0]
assert len(source['files']) == 597
assert source['source_manifest_sha256'] == '068277d843b191bc8283bab38e667b0f6a4d30e296e8c5a99a97cfe19654fcf8'
for name, spec in source['files'].items():
    file = root / name
    assert file.stat().st_size == spec['bytes']
    assert hashlib.sha256(file.read_bytes()).hexdigest() == spec['sha256']
    assert bool(file.stat().st_mode & 0o111) == spec['executable']
compiler_before = members['quality/rustc_before/stdout.log']
assert compiler_before == members['quality/rustc_after/stdout.log']
assert b'080cc27146389dbb5c8772c476b1937be9c6110f' in compiler_before
terminal = json.loads(members['boot/terminal.json'])
reclaim = json.loads(members['boot/reclaim.json'])
for item in terminal['same_ssh_children'] + [reclaim]:
    assert item['exit_code'] == 0 and item['reaped'] and not item['signals_sent']
assert terminal['exit_code'] == 0 and terminal['driver_pid'] == 653366
owner = json.loads(pathlib.Path(receipt['owner_result']).read_text())
assert owner['wait_returncode'] == 0 and owner['reaped']
assert not owner['errors'] and not owner['owner_sent_signals'] and not owner['received_signals']
log = pathlib.Path(owner['log']['path']).read_bytes()
assert len(log) == owner['log']['bytes']
assert hashlib.sha256(log).hexdigest() == owner['log']['sha256']
out = {
    'status': 'PASS_ROOT_REVIEW_R17_NATIVE_QUALITY',
    'archive_sha256': meta['archive_sha256'], 'archive_bytes': meta['archive_bytes'],
    'archive_files': len(members), 'distinct_runner_children': len(receipts), 'streams': 22,
    'fmt_pid': fmt['pid'], 'clippy_pid': clippy['pid'], 'clippy_warning_lines': warning_lines,
    'actual_root_fmt': {'chunk_id': '728bef', 'exit_code': 0, 'wall_seconds': 0.938255833},
    'source_inputs': 597, 'source_aggregate': source['source_manifest_sha256'],
    'root_native_per_file_bytes_hash_mode_equal': True,
    'compiler_commit': '080cc27146389dbb5c8772c476b1937be9c6110f',
    'native_git_head': receipt['result']['native_git_head'], 'native_git_clean': False,
    'driver_pid': terminal['driver_pid'], 'SSH_owner': owner['owner_pid'], 'SSH_child': owner['pid'],
    'original_local_inspection_failures': [
        {'chunk_id': '419717', 'exit_code': 1, 'error': "KeyError: 'clippy_warning_lines'", 'scope': '只读检查错误地假设可选回执字段存在；改从原Clippy stderr核验，无Cargo重跑'},
        {'chunk_id': 'fa67a5', 'exit_code': 1, 'error': 'KeyError: 0', 'scope': '只读查看source.files误按list取首项；实际为dict，无原件修改'}
    ],
    'limits': '仅提交前fmt与根Clippy质量门；无Rust测试、release、UART或新控制ELF'
}
(phase / 'review/root-actual-quality-review-r17.json').write_text(json.dumps(out, ensure_ascii=False, indent=2) + '\n')
print(json.dumps(out, ensure_ascii=False))
