"""爸爸，仅三次/dev/never-open参数门；不发送合法命令或打开UART。"""

import errno
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time

BASE = Path('/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013')
OUT = BASE / 'parameter-rejections'
ELF = BASE / 'quality/frozen/feetech_poke'
artifact = json.loads((BASE / 'quality/frozen/artifact.json').read_text())


def identity():
    raw = ELF.read_bytes()
    result = {'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}
    assert result == {key: artifact['frozen_identity'][key] for key in ('sha256', 'bytes')}
    return result


before = identity()
assert not os.path.lexists('/dev/never-open'), '爸爸，参数门固定路径必须不存在。'
OUT.mkdir()
groups = [
    ('extra0', [str(ELF), '/dev/never-open', '9', 'sync-zero', '0'], 'error: sync-zero不接受寄存器或数值参数\n'),
    ('GoalVelocity0', [str(ELF), '/dev/never-open', '9', 'sync-zero', 'Goal_Velocity', '0'], 'error: sync-zero不接受寄存器或数值参数\n'),
    ('invalidID0', [str(ELF), '/dev/never-open', '0', 'sync-zero'], 'error: motor_id 0: motor id out of range [1, 253]: got 0\n'),
]
receipts = []
result = {'status': 'FAIL', 'scope': 'THREE_PARAMETER_REJECTIONS_ONLY_NO_VALID_SYNC_ZERO', 'ELF_before': before}
try:
    for name, argv, expected_stderr in groups:
        assert not os.path.lexists('/dev/never-open')
        started = time.time_ns()
        child = subprocess.Popen(argv, cwd=BASE, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        stdout, stderr = child.communicate()
        code = child.wait()
        reaped = False
        try:
            os.waitpid(child.pid, os.WNOHANG)
        except ChildProcessError as error:
            reaped = error.errno == errno.ECHILD
        row = {'name': name, 'argv': argv, 'pid': child.pid, 'owner_pid': os.getpid(), 'started_ns': started,
               'finished_ns': time.time_ns(), 'exit_code': code, 'wait_returncode': code,
               'expected_exit_code': 2, 'reaped': reaped, 'signals_sent': []}
        for suffix, raw in (('stdout', stdout), ('stderr', stderr)):
            path = OUT / (name + '.' + suffix)
            with path.open('xb') as stream:
                stream.write(raw)
            row[suffix] = {'path': str(path), 'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}
        receipts.append(row)
        with (OUT / (name + '.receipt.json')).open('x') as stream:
            json.dump(row, stream, ensure_ascii=False, indent=2)
            stream.write('\n')
        assert code == 2 and reaped and stdout == b'' and stderr == expected_stderr.encode(), row
        assert b'open serial' not in stderr and b'bus.connect' not in stderr
    after = identity()
    assert after == before
    result.update(status='PASS', ELF_after=after, ELF_before_after_equal=True,
                  exact_parameter_rejections=3, UART_operations=0, valid_sync_zero_runs=0,
                  hardware_actions=0, GO_created=False)
except BaseException as error:
    result['error'] = repr(error)
finally:
    result['commands'] = receipts
    with (OUT / 'result.json').open('x') as stream:
        json.dump(result, stream, ensure_ascii=False, indent=2)
        stream.write('\n')
    print(json.dumps({'status': result['status'], 'direct_children': len(receipts)}, ensure_ascii=False), flush=True)
raise SystemExit(0 if result['status'] == 'PASS' else 1)
