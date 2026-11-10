"""用 sleep ELF 验证原维护启动及身份观察，不访问 UART。"""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, sys.argv[1])
import run_hardware_maintenance as maintenance
import run_post_e_h2 as identity

target = Path('/usr/bin/sleep')
raw = target.read_bytes()
reference = {'path': str(target), 'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}
results = []
with tempfile.TemporaryDirectory(prefix='name-native-') as temporary:
    root = Path(temporary)
    for traced in (False, True):
        output = root / str(traced)
        output.mkdir()
        executor = maintenance.RealExecutor(reference, output)
        original_reap = executor.reap
        observed = []

        def reap(row, failures, **kwargs):
            # 仅此无硬件 sleep 夹具使用较长观察预算，避开并行编译调度影响。
            row['budget_ns'] = 10_000_000_000
            pid = executor.child.pid
            if traced:
                # 本机内核不提供 task/children，复用原进程观察器定位同组叶子。
                deadline = time.monotonic() + 1
                leaves = []
                while not leaves and time.monotonic() < deadline:
                    snapshot = identity.process_snapshot(executor.child.pid, output, [target])
                    leaves = [item for item in snapshot['related']
                              if item['pgid'] == executor.child.pid and item['exe'] == str(target.resolve())]
                    time.sleep(0.005)
                assert len(leaves) == 1, leaves
                pid = leaves[0]['pid']
            proc = Path('/proc') / str(pid)
            deadline = time.monotonic() + 1
            while proc.joinpath('comm').read_text().strip() != 'robot-feetech' and time.monotonic() < deadline:
                time.sleep(0.005)
            comm = proc.joinpath('comm').read_text().strip()
            exe = os.readlink(proc / 'exe')
            argv = proc.joinpath('cmdline').read_bytes().decode().split('\0')[:-1]
            snapshot = identity.process_snapshot(executor.child.pid, output, [target])
            assert comm == 'robot-feetech', comm
            assert exe == str(target.resolve()), exe
            assert Path(argv[0]).name == 'robot-feetech', argv
            assert any(item['pid'] == pid for item in snapshot['related']), snapshot
            observed.append({'pid': pid, 'comm': comm, 'exe': exe, 'argv': argv,
                             'identity_observed': True})
            return original_reap(row, failures, **kwargs)

        executor.reap = reap
        argv = [str(target), '3']
        if traced:
            argv = [maintenance.RIGHT_PULSE_TRACE['path'], '-ttt', '-xx', '-s', '256', '-yy',
                    '-e', 'trace=openat,close,read,write,ioctl', '-o', str(output / 'trace'), *argv]
        try:
            executor.command(argv)
            row = executor.commands[-1]
            alias = next(Path(arg) for arg in row['argv'] if Path(arg).name == 'robot-feetech')
            assert not alias.parent.exists()
            assert row['reaped'] and row['exit_code'] == 0
            assert len(observed) == 1
            assert hashlib.sha256(target.read_bytes()).hexdigest() == reference['sha256']
            results.append({'traced': traced, 'observed': observed, 'alias_cleaned': True,
                            'original_sha_unchanged': True})
        finally:
            executor.journal.close()
print(json.dumps({'results': results, 'hardware_access': False}, ensure_ascii=False, indent=2))
