"""爸爸，仅核本轮已完成原件，补齐root验收记录，无SSH/UART/Cargo。"""
import hashlib
import json
from pathlib import Path
import re
import tarfile
import time

base = Path(__file__).resolve().parent
sha = lambda b: hashlib.sha256(b).hexdigest()
quality = base / 'live/commit-quality'
receipt = json.loads((quality / 'quality-archive-receipt-r14.json').read_text())
qtar = quality / 'commit-quality-originals-r14.tar.gz'
assert sha(qtar.read_bytes()) == receipt['archive_metadata']['sha256']
assert qtar.stat().st_size == receipt['archive_metadata']['bytes']
with tarfile.open(qtar) as tar:
    members = {m.name: tar.extractfile(m).read() for m in tar if m.isfile()}
assert len(members) == 61
assert receipt['result']['status'] == 'PASS'
assert len(receipt['receipts']) == 10
assert len({r['pid'] for r in receipt['receipts']}) == 10
for row in receipt['receipts']:
    assert row['exit_code'] == 0 and row['reaped'] and not row['signals']
    assert not row['session_residual_after_cleanup']
    for stream in ('stdout', 'stderr'):
        data = members['quality/' + row['name'] + '/' + stream + '.log']
        assert len(data) == row[stream + '_bytes']
        assert sha(data) == row[stream + '_sha256']
fmt, clippy = [next(r for r in receipt['receipts'] if r['name'] == n) for n in ('fmt', 'clippy')]
assert fmt['argv'] == ['cargo', 'fmt', '--all']
assert clippy['argv'] == ['cargo', 'clippy', '--all', '--all-targets', '--', '-D', 'warnings']
assert fmt['ended_monotonic_ns'] <= clippy['started_monotonic_ns']
windows = [json.loads((quality / name).read_text()) for name in ('source_before.json', 'fmt_after.json', 'source_after.json')]
assert windows[0] == windows[1] == windows[2]
files = windows[0]['files']
assert len(files) == 597
assert windows[0]['source_manifest_sha256'] == '068277d843b191bc8283bab38e667b0f6a4d30e296e8c5a99a97cfe19654fcf8'
root = Path.cwd()
for name, row in files.items():
    p = root / name
    raw = p.read_bytes()
    assert len(raw) == row['bytes'] and sha(raw) == row['sha256']
    assert bool(p.stat().st_mode & 0o111) == row['executable']
for row in receipt['same_ssh']:
    assert row['reaped'] and row['exit_code'] == 0 and not row['signals_sent']
owner = receipt['owner']
assert owner['reaped'] and owner['wait_returncode'] == 0 and not owner['owner_sent_signals'] and not owner['errors']
quality_review = {
    'status': 'ROOT_ACCEPTED_R14_ACTUAL_FMT_THEN_ROOT_CLIPPY',
    'reviewed_ns': time.time_ns(), 'tar_sha256': sha(qtar.read_bytes()),
    'tar_regular_files': 61, 'native_direct_children': 10, 'streams_verified': 20,
    'native_three_source_windows_equal': True, 'root_597_files_verified_now': True,
    'source_sha256': windows[0]['source_manifest_sha256'],
    'actual_fmt_pid': fmt['pid'], 'actual_clippy_pid': clippy['pid'],
    'all_naturally_reaped': True, 'root_fmt': '本轮前序实际cargo fmt --all exit0；无Rust文件变化',
    'compiler': receipt['result']['compiler'], 'native_git_head': receipt['result']['native_git_head'],
    'native_git_state': '五项dirty，不称Git对齐', 'UART': 0,
}
(base / 'root-actual-quality-review-r14.json').write_text(json.dumps(quality_review, ensure_ascii=False, indent=2) + '\n')

with tarfile.open(base / 'live/hardware-originals-r14.tar.gz') as tar:
    hw = {m.name: tar.extractfile(m).read() for m in tar if m.isfile()}
run = json.loads(hw['head2-p32-trial-r14/run.json'])
raw_commands = [r for r in run['commands'] if 'read-raw' in r['argv']]
assert len(raw_commands) == 30
for row in raw_commands:
    packet = json.loads(hw['head2-p32-trial-r14/' + Path(row['stdout_path']).name])
    tx, rx = packet['tx'], packet['rx']
    assert packet['motor_id'] == 8 and packet['register'] == row['argv'][-1]
    assert packet['operation'] == 'sync_read'
    assert tx[:5] == [255, 255, 254, 5, 130]
    assert tx[5:8] == [packet['requested_register_address'], packet['width'], 8]
    assert rx[:3] == [255, 255, 8] and rx[4] == 0
    assert len(tx) == 9 and len(rx) == packet['width'] + 6
    assert rx[3] == packet['width'] + 2
    assert sum(tx[2:]) % 256 == sum(rx[2:]) % 256 == 255
    assert int.from_bytes(bytes(rx[5:-1]), 'little') == packet['raw_unsigned']
independent = json.loads((base / 'independent-actual-hardware-review-r14.json').read_text())
for capture in independent['ordinary_right8_right9_read_trace']['captures']:
    data = hw[capture['member']]
    assert sha(data) == capture['sha256'] and len(data) == capture['bytes']
    rows = []
    for line in data.decode().splitlines():
        match = re.search(r'(write|read)\(3<.*<char 166:1>>, "((?:\\x[0-9a-f]{2})+)", (\d+)\) = (\d+)$', line)
        if match:
            payload = bytes.fromhex(match[2].replace('\\x', ''))
            assert len(payload) == int(match[3]) == int(match[4])
            rows.append((match[1], payload))
    assert len(rows) == 6
    ident = capture['motor_id']
    assert [op for op, b in rows] == ['write', 'read'] * 3
    for op, b in rows:
        assert b[:3] == bytes([255, 255, ident])
        assert len(b) == b[3] + 4 and sum(b[2:]) % 256 == 255
    assert rows[0][1][3:5] == bytes([2, 1])
    assert rows[1][1][4] == 0
    assert rows[2][1][4:7] == bytes([2, 3, 2])
    assert rows[3][1][4:7] == bytes([0, 9, 3])
    assert rows[4][1][4:7] == bytes([2, 58, 2])
    assert rows[5][1][4:7] == bytes([0, 0, 0])
hardware_review_path = base / 'root-actual-hardware-review-r14.json'
review = json.loads(hardware_review_path.read_text())
review.update(full_raw_and_strace_review='ROOT_VERIFIED_30_RAW_COMMANDS_AND_FOUR_ORDINARY_READ_TRACES',
              saved_raw_stage_fields=29, actual_read_raw_commands=30,
              binding_full_stat_refs=33, binding_path_sha_size_only_refs=17,
              reviewer_assumption_errors_preserved=['首次展示把p_trial误当run顶层：KeyError', '首strace正则混入/proc/maps读：AssertionError，改为已有ttyACM1字符设备字段；无原件修改/新采集'],
              final_reviewed_ns=time.time_ns())
hardware_review_path.write_text(json.dumps(review, ensure_ascii=False, indent=2) + '\n')
print(json.dumps({'quality': quality_review['status'], 'raw_commands': 30, 'ordinary_READ_traces': 4, 'source_files': 597}, ensure_ascii=False))
