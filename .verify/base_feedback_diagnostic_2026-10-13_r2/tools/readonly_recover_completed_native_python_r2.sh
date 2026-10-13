#!/bin/bash
# 爸爸，只读取已完R2原件；不测试、不构建、不改远端文件或任何硬件。
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64
import hashlib
import io
import json
import os
from pathlib import Path
import re
import stat
import tarfile
import time

phase = Path('/home/loosqk/robot-checks/base-feedback-r2-software-20261013')
assert phase == phase.resolve() and phase.is_dir()
expected_names = {
 'candidate/base_feedback_diagnostic_r2.py', 'candidate/test_base_feedback_diagnostic_r2.py',
 'candidate/fixtures/raw_right9_from_r14_r2.py', 'candidate/fixtures/run_hardware_maintenance.py',
 'candidate/fixtures/maintenance_bindings_base_zero_diagnostic_r4.json',
 'boot/verify_pure_base_feedback_r2.py', 'boot/root-input-freeze-r2.json',
 'boot/uploaded-inputs-before.json', 'boot/uploaded-inputs-after.json',
 'boot/launch_native_software_base_feedback_r2.sh', 'boot/base-feedback-r2-software-driver.log',
 'boot/start.json', 'boot/observations.jsonl', 'boot/terminal.json', 'boot/reclaim.json',
 'pure-software/memory_tests.stdout', 'pure-software/memory_tests.stderr',
 'pure-software/memory_tests.receipt.json', 'pure-software/result.json',
}
expected_pins = {
 'candidate/base_feedback_diagnostic_r2.py': ('cf651bb200d0ea56228282dd51b311bb090650945b631f51e8d7ff2f1e431b78', 18648),
 'candidate/test_base_feedback_diagnostic_r2.py': ('815ec4995e1bcd0c92e0446ad49d876da518185072f985f94f573f4eb7445417', 37227),
 'candidate/fixtures/raw_right9_from_r14_r2.py': ('70d777bd5c2a19e2ce3f57e68f9abd40eaed2b8a9aa8c592e7fe53a940f1e38c', 12220),
 'candidate/fixtures/run_hardware_maintenance.py': ('d97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41', 50168),
 'candidate/fixtures/maintenance_bindings_base_zero_diagnostic_r4.json': ('347a65dbfe0b3d553f7df06b413507b1491c073ced8a283f498bf7a81f51be86', 30051),
 'boot/verify_pure_base_feedback_r2.py': ('2000c7d98be463d6467f3ede11214135960c763fdd67e53d11393b00bc953561', 6985),
 'boot/root-input-freeze-r2.json': ('cfabf0cb11b4182255e0a297d3731105befd4b50fc72894b1ce28ed47965492c', 4106),
 'pure-software/memory_tests.stdout': ('aa7b9d72b906759c02761df7769e42b3a78eed6b03d7035f949051f6504f046d', 190),
 'pure-software/memory_tests.stderr': ('7a583f996d714d4209012e46024bede02a621e1a2eb659c47bc593274e42e9f8', 4675),
}


def pin(raw):
 return {'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}


def snapshot():
 result = {}
 for name in sorted(expected_names):
  path = phase / name
  info = path.lstat()
  assert stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and path == path.resolve()
  raw = path.read_bytes()
  result[name] = {**pin(raw), 'device': info.st_dev, 'inode': info.st_ino, 'mtime_ns': info.st_mtime_ns, 'ctime_ns': info.st_ctime_ns}
 return result


started = time.time_ns()
before = snapshot()
for name, (sha, size) in expected_pins.items():
 assert before[name]['sha256'] == sha and before[name]['bytes'] == size
result = json.loads((phase / 'pure-software/result.json').read_text())
assert result['status'] == 'PASS' and result['native_test_batches'] == 1 and result['distinct_memory_methods'] == result['expected_memory_methods'] == 31
assert len(result['commands']) == 1
child = result['commands'][0]
assert child['pid'] == 664208 and child['owner_pid'] == 664204 and child['exit_code'] == child['wait_returncode'] == 0 and child['reaped'] is True and child['signals_sent'] == []
assert child == json.loads((phase / 'pure-software/memory_tests.receipt.json').read_text())
stderr = (phase / 'pure-software/memory_tests.stderr').read_bytes()
methods = re.findall(rb'^(test_\w+) \(__main__\.\w+\.(test_\w+)\) \.\.\. ok$', stderr, re.M)
assert len(methods) == 31 and all(left == right for left, right in methods) and len({left for left, right in methods}) == 31 and b'\nOK\n' in stderr
assert [left.decode() for left, right in methods] == result['methods']
assert result['source_before'] == result['source_after']
upload_before = json.loads((phase / 'boot/uploaded-inputs-before.json').read_text())
upload_after = json.loads((phase / 'boot/uploaded-inputs-after.json').read_text())
assert upload_before == upload_after and len(upload_before) == 7
for name, expected in upload_before.items():
 assert {key: before[name][key] for key in ('sha256', 'bytes')} == expected
terminal = json.loads((phase / 'boot/terminal.json').read_text())
assert terminal['terminal'] == 'base-feedback-r2-software-20261013-r1|@105|%105|664204|1|0' and terminal['driver_pid'] == 664204 and terminal['exit_code'] == 0
reclaim = json.loads((phase / 'boot/reclaim.json').read_text())
assert reclaim['argv'] == ['tmux', 'kill-pane', '-t', '%105'] and reclaim['exit_code'] == reclaim['wait_returncode'] == 0 and reclaim['reaped'] is True
for row in [*terminal['same_ssh_children'], reclaim]:
 assert row['exit_code'] == row['wait_returncode'] == 0 and row['reaped'] is True and row['signals_sent'] == []
old_sha, old_bytes = '0af7e60fafd5b5a806e67de036876b8fefa61d1abc4682ad359af5a0787df0bc', 46557
tar_candidates = []
old_raw = None
for path in sorted(phase.rglob('*.tar.gz')):
 assert stat.S_ISREG(path.lstat().st_mode) and path == path.resolve()
 raw = path.read_bytes()
 row = {'path': str(path), **pin(raw)}
 tar_candidates.append(row)
 if row['sha256'] == old_sha and row['bytes'] == old_bytes:
  assert old_raw is None
  old_raw = raw
if old_raw is not None:
 raw = old_raw
 archive_kind = 'EXISTING_ORIGINAL_COMPRESSED_BYTES_RECOVERED'
else:
 # 爸爸，这是本次只读封包，不猜原gzip时间，不将新SHA冒充旧0af7。
 buffer = io.BytesIO()
 with tarfile.open(fileobj=buffer, mode='w:gz') as archive:
  for name in sorted(expected_names):
   archive.add(phase / name, arcname=name, recursive=False)
 raw = buffer.getvalue()
 archive_kind = 'NEW_READONLY_RECOVERY_PACKAGE_OF_EXISTING_19_ORIGINALS'
after = snapshot()
assert before == after
print(json.dumps({'status': 'PASS_READONLY_COMPLETED_R2_EVIDENCE_RECOVERY', 'collector_pid': os.getpid(),
 'started_ns': started, 'finished_ns': time.time_ns(), 'source_phase': str(phase),
 'archive_kind': archive_kind, 'old_compressed_bytes': 'RECOVERED' if old_raw is not None else 'LOST_NOT_PERSISTED_IN_PHASE',
 'old_archive_historical_sha256': old_sha, 'old_archive_historical_bytes': old_bytes,
 'existing_tar_candidates': tar_candidates, 'archive_sha256': hashlib.sha256(raw).hexdigest(), 'archive_bytes': len(raw),
 'archive_files': 19, 'original_manifest_before': before, 'original_manifest_after': after,
 'original_result_status': result['status'], 'original_unittest_child': child,
 'original_driver_terminal': terminal['terminal'], 'original_driver_dead_pane_reclaim': reclaim,
 'distinct_original_methods': 31, 'test_reruns': 0, 'native_batch_restarts': 0,
 'UART_operations': 0, 'GO_created': False, 'services_touched': 0, 'remote_files_written': 0,
 'old_local_owner_93330_ssh_93331_original_receipts': 'LOST_LOCAL_TARGET_DELETION_NOT_REPLACED_BY_THIS_COLLECTION',
 'archive_base64': base64.b64encode(raw).decode()}, ensure_ascii=False), flush=True)
PYREMOTE
