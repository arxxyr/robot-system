"""爸爸，保存底盘零目标R1本机原输出及直属child自然回收；禁止网络/UART/硬件。"""
import errno
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
ROOT = next(path for path in HERE.parents if (path / 'scripts/run_hardware_maintenance.py').is_file())
OUTPUT = HERE / 'software-verification-r4'
OUTPUT.mkdir(exist_ok=False)
CANDIDATE = HERE / 'base_zero_diagnostic_r1.py'
TEST = HERE / 'test_base_zero_diagnostic_r1.py'
SOURCES = [Path(__file__).resolve(), CANDIDATE, TEST, HERE / 'raw_right9_from_r14_r1.py',
           ROOT / 'scripts/run_hardware_maintenance.py',
           ROOT / '.verify/position_return_2026-10-11_r14/helpers/parameter_stop_head2_r14.py',
           ROOT / 'target/position_return_2026-10-11_r17/native-preparation/maintenance_bindings_head2_p32_reverse_r17.json']


def ref(path):
    raw = path.read_bytes()
    return {'path': str(path), 'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}


before = [ref(path) for path in SOURCES]
uv = shutil.which('uv')
if uv is None:
    raise RuntimeError('缺少uv，不回退系统Python')
prefix = [uv, 'run', '--no-project', 'python']
recipes = [('memory_tests', prefix + [str(TEST)], 0), ('default_plan', prefix + [str(CANDIDATE)], 0),
           ('missing_go', prefix + [str(CANDIDATE), '--execute', '--maintenance', '/MEMORY_ONLY/source',
             '--raw-helper', '/MEMORY_ONLY/helper', '--bindings', '/MEMORY_ONLY/bindings',
             '--output', '/MEMORY_ONLY/output', '--artifact-root', '/MEMORY_ONLY'], 2)]
receipts = []
for label, argv, expected in recipes:
    started = time.time_ns()
    child = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    stdout, stderr = child.communicate()
    finished = time.time_ns()
    reaped = False
    try:
        os.waitpid(child.pid, os.WNOHANG)
    except ChildProcessError as error:
        reaped = error.errno == errno.ECHILD
    out = OUTPUT / (label + '.stdout')
    err = OUTPUT / (label + '.stderr')
    out.write_bytes(stdout)
    err.write_bytes(stderr)
    receipts.append({'label': label, 'argv': argv, 'pid': child.pid, 'owner_pid': os.getpid(),
                     'started_ns': started, 'finished_ns': finished, 'exit_code': child.returncode,
                     'expected_exit_code': expected, 'reaped': reaped, 'signals_sent': [],
                     'stdout': ref(out), 'stderr': ref(err)})
after = [ref(path) for path in SOURCES]
tests = (OUTPUT / 'memory_tests.stderr').read_text()
methods = re.findall(r'^(test_\w+) \(__main__\.ZeroCases\.(test_\w+)\) \.\.\. ok$', tests, re.M)
default = json.loads((OUTPUT / 'default_plan.stdout').read_text())
passed = (before == after and len(methods) == 10 and all(left == right for left, right in methods)
          and len({left for left, _ in methods}) == 10 and '\nOK\n' in tests
          and all(row['exit_code'] == row['expected_exit_code'] and row['reaped'] for row in receipts)
          and all(default[key] == 0 for key in ('actions_started', 'register_writes', 'motion_commands'))
          and default['position_targets'] == [] and default['run_id'] == 'base-zero-diagnostic-r1'
          and '新架空就绪GO' in (OUTPUT / 'missing_go.stderr').read_text())
result = {'status': 'PASS' if passed else 'FAIL', 'scope': 'LOCAL_MEMORY_SOFTWARE_ONLY_NO_SSH_UART_MOTION',
          'distinct_memory_methods': len(methods), 'expected_memory_methods': 10, 'methods': [left for left, _ in methods],
          'source_before': before, 'source_after': after, 'source_unchanged': before == after,
          'commands': receipts, 'default_plan': default, 'hardware_actions': 0, 'uart_reads': 0, 'parameter_writes': 0,
          'limitations': ['纯内存夹具未执行远端工具，现场架空/供电/GO均未知',
                          '0/200/400ms三个分散原PV0不能证明连续500ms物理静止',
                          '裸46/2B路径，不增加41..47写API，旧三轮DEFERRED/生产合同拒绝不改']}
(OUTPUT / 'result.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
print(json.dumps({'status': result['status'], 'distinct_memory_methods': len(methods), 'direct_children': len(receipts),
                  'natural_wait_reap': all(row['reaped'] for row in receipts), 'result': str(OUTPUT / 'result.json')},
                 ensure_ascii=False))
sys.exit(0 if passed else 1)
