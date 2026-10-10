"""爸爸，保存R10本机软件验证原输出与直属child真实回收；禁止实机与网络。"""
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
OUTPUT = HERE / 'software-verification-r10-r3'
OUTPUT.mkdir(exist_ok=False)
CANDIDATE = HERE / 'head1_load_observation_r10.py'
TEST = HERE / 'test_head1_load_observation_r10.py'
R9 = HERE.parent / 'head-reverse-preparation-r9'
SOURCES = [Path(__file__).resolve(), CANDIDATE, TEST, ROOT / 'scripts/run_hardware_maintenance.py',
           HERE.parent / 'head-p24-preparation-r6/p24_trial_head1_wire_r6.py',
           R9 / 'head1_reverse_observation_r9.py',
           R9 / 'native-preparation/maintenance_bindings_head_reverse_r9.json',
           R9 / 'head1-reverse-observation-r9.go.json']


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
             '--helper', '/MEMORY_ONLY/helper', '--reverse-helper', '/MEMORY_ONLY/reverse',
             '--bindings', '/MEMORY_ONLY/bindings', '--output', '/MEMORY_ONLY/output', '--run-id', 'memory-r10',
             '--artifact-root', '/MEMORY_ONLY'], 2)]
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
    receipt = {'label': label, 'argv': argv, 'pid': child.pid, 'owner_pid': os.getpid(),
               'started_ns': started, 'finished_ns': finished, 'exit_code': child.returncode,
               'expected_exit_code': expected, 'reaped': reaped, 'signals_sent': [],
               'stdout': ref(out), 'stderr': ref(err)}
    receipts.append(receipt)
after = [ref(path) for path in SOURCES]
tests = (OUTPUT / 'memory_tests.stderr').read_text()
methods = re.findall(r'^(test_\w+) \(__main__\.LoadCases\.(test_\w+)\) \.\.\. ok$', tests, re.M)
default = json.loads((OUTPUT / 'default_plan.stdout').read_text())
passed = (before == after and len(methods) == 14 and all(left == right for left, right in methods)
          and len({left for left, _ in methods}) == 14 and '\nOK\n' in tests
          and all(row['exit_code'] == row['expected_exit_code'] and row['reaped'] for row in receipts)
          and all(default[key] == 0 for key in ('actions_started', 'parameter_writes', 'motion_commands', 'load_raw_reads'))
          and '新Load对照GO' in (OUTPUT / 'missing_go.stderr').read_text())
result = {'status': 'PASS' if passed else 'FAIL', 'scope': 'LOCAL_MEMORY_SOFTWARE_ONLY_NO_SSH_UART_MOTION',
          'distinct_memory_methods': 14, 'methods': [left for left, _ in methods],
          'source_before': before, 'source_after': after, 'source_unchanged': before == after,
          'commands': receipts, 'default_plan': default, 'native_load_tool_frozen': False,
          'hardware_actions': 0, 'uart_reads': 0, 'parameter_writes': 0,
          'limitations': ['Load原帧仅内存夹具，实际新ELF引用由root提供，不是本机原生执行',
                          '既有command预算不证明硬实时绝对READ截止', '本机软件通过不证明位置精度或机械原因']}
(OUTPUT / 'result.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
print(json.dumps({'status': result['status'], 'distinct_memory_methods': 14, 'direct_children': len(receipts),
                  'natural_wait_reap': all(row['reaped'] for row in receipts), 'result': str(OUTPUT / 'result.json')},
                 ensure_ascii=False))
sys.exit(0 if passed else 1)
