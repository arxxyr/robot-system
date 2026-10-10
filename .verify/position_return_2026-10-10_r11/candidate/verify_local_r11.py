"""爸爸，保存R11本机原输出及直属child自然回收；禁止网络/UART/硬件。"""
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
OUTPUT = HERE / 'software-verification-r11-r2'
OUTPUT.mkdir(exist_ok=False)
CANDIDATE = HERE / 'head1_d0_trial_r11.py'
TEST = HERE / 'test_head1_d0_trial_r11.py'
R6 = HERE.parent / 'head-p24-preparation-r6'
R8 = HERE.parent / 'head-settle-preparation-r8'
R10 = HERE.parent / 'head-load-preparation-r10'
SOURCES = [Path(__file__).resolve(), CANDIDATE, TEST, ROOT / 'scripts/run_hardware_maintenance.py',
           R6 / 'p24_trial_head1_wire_r6.py', R8 / 'head1_outward_observation_r8.py',
           R6 / 'test_p24_head1_wire_r6.py', R8 / 'test_head1_outward_observation_r8.py',
           R8 / 'native-preparation/maintenance_bindings_head_settle_r8.json',
           R6 / 'head1-p24-wire-r6.go.json', R8 / 'head1-outward-observation-r8.go.json',
           R10 / 'head1-load-observation-r10.go.json']


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
             '--helper', '/MEMORY_ONLY/helper', '--observation-helper', '/MEMORY_ONLY/observation',
             '--bindings', '/MEMORY_ONLY/bindings', '--output', '/MEMORY_ONLY/output', '--run-id', 'memory-r11',
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
    receipts.append({'label': label, 'argv': argv, 'pid': child.pid, 'owner_pid': os.getpid(),
                     'started_ns': started, 'finished_ns': finished, 'exit_code': child.returncode,
                     'expected_exit_code': expected, 'reaped': reaped, 'signals_sent': [],
                     'stdout': ref(out), 'stderr': ref(err)})
after = [ref(path) for path in SOURCES]
tests = (OUTPUT / 'memory_tests.stderr').read_text()
methods = re.findall(r'^(test_\w+) \(__main__\.D0Cases\.(test_\w+)\) \.\.\. ok$', tests, re.M)
default = json.loads((OUTPUT / 'default_plan.stdout').read_text())
passed = (before == after and len(methods) == 10 and all(left == right for left, right in methods)
          and len({left for left, _ in methods}) == 10 and '\nOK\n' in tests
          and all(row['exit_code'] == row['expected_exit_code'] and row['reaped'] for row in receipts)
          and all(default[key] == 0 for key in ('actions_started', 'parameter_writes', 'motion_commands', 'load_raw_reads'))
          and '新D0功能试验GO' in (OUTPUT / 'missing_go.stderr').read_text())
result = {'status': 'PASS' if passed else 'FAIL', 'scope': 'LOCAL_MEMORY_SOFTWARE_ONLY_NO_SSH_UART_MOTION',
          'distinct_memory_methods': len(methods), 'expected_memory_methods': 10, 'methods': [left for left, _ in methods],
          'source_before': before, 'source_after': after, 'source_unchanged': before == after,
          'commands': receipts, 'default_plan': default, 'hardware_actions': 0, 'uart_reads': 0, 'parameter_writes': 0,
          'limitations': ['全部参数/动作输出为内存夹具，未执行远端工具',
                          'D22属于厂家EEPROM配置区；计划D0/43写，Lock1且不发送保存/Lock写；掉电持久性未验证',
                          '不同起点/方向只验原精度功能，不能证明D因果或长期参数闭环',
                          '沿既有command预算，不提供硬实时绝对READ截止']}
(OUTPUT / 'result.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
print(json.dumps({'status': result['status'], 'distinct_memory_methods': len(methods), 'direct_children': len(receipts),
                  'natural_wait_reap': all(row['reaped'] for row in receipts), 'result': str(OUTPUT / 'result.json')},
                 ensure_ascii=False))
sys.exit(0 if passed else 1)
