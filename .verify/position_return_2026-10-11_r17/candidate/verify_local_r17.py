"""爸爸，保存R17本机原输出及直属child自然回收；禁止网络/UART/硬件。"""
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
OUTPUT = HERE / 'software-verification-r17-r1'
OUTPUT.mkdir(exist_ok=False)
CANDIDATE = HERE / 'head2_p32_reverse_trial_r17.py'
TEST = HERE / 'test_head2_p32_reverse_trial_r17.py'
BASE = ROOT / 'target/position_return_2026-10-11_r14'
SOURCES = [Path(__file__).resolve(), CANDIDATE, TEST, ROOT / 'scripts/run_hardware_maintenance.py',
           BASE / 'helpers/parameter_stop_head2_r14.py', BASE / 'helpers/head2_outward_observation_r14.py',
           BASE / 'head-p24-preparation-r6/test_p24_head1_wire_r6.py',
           BASE / 'head-p24-preparation-r6/p24_trial_head1_wire_r6.py',
           BASE / 'head-settle-preparation-r8/test_head1_outward_observation_r8.py',
           BASE / 'head-settle-preparation-r8/head1_outward_observation_r8.py',
           BASE / 'head-settle-preparation-r8/native-preparation/maintenance_bindings_head_settle_r8.json',
           BASE / 'head-p24-preparation-r6/head1-p24-wire-r6.go.json',
           BASE / 'head-settle-preparation-r8/head1-outward-observation-r8.go.json',
           BASE / 'head-load-preparation-r10/head1-load-observation-r10.go.json',
           BASE / 'head-d0-preparation-r11/head1-d0-trial-r11.go.json',
           BASE / 'head-p32-preparation-r13/head1-p32-trial-r13.go.json',
           ROOT / 'target/position_return_2026-10-10_r1/head-p24-preparation-r6/p24_trial_head1_wire_r6.py']


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
             '--bindings', '/MEMORY_ONLY/bindings', '--output', '/MEMORY_ONLY/output', '--run-id', 'head2-p32-reverse-r17',
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
methods = re.findall(r'^(test_\w+) \(__main__\.P32Cases\.(test_\w+)\) \.\.\. ok$', tests, re.M)
default = json.loads((OUTPUT / 'default_plan.stdout').read_text())
passed = (before == after and len(methods) == 10 and all(left == right for left, right in methods)
          and len({left for left, _ in methods}) == 10 and '\nOK\n' in tests
          and all(row['exit_code'] == row['expected_exit_code'] and row['reaped'] for row in receipts)
          and all(default[key] == 0 for key in ('actions_started', 'parameter_writes', 'motion_commands', 'load_raw_reads'))
          and default['delta_raw'] == -24 and default['run_id'] == 'head2-p32-reverse-r17'
          and '新P32反向功能试验GO' in (OUTPUT / 'missing_go.stderr').read_text())
result = {'status': 'PASS' if passed else 'FAIL', 'scope': 'LOCAL_MEMORY_SOFTWARE_ONLY_NO_SSH_UART_MOTION',
          'distinct_memory_methods': len(methods), 'expected_memory_methods': 10, 'methods': [left for left, _ in methods],
          'source_before': before, 'source_after': after, 'source_unchanged': before == after,
          'commands': receipts, 'default_plan': default, 'hardware_actions': 0, 'uart_reads': 0, 'parameter_writes': 0,
          'limitations': ['全部参数/动作输出为内存夹具，未执行远端工具',
                          'P21属于厂家EEPROM配置区；计划P32/16写，Lock1且不发送保存/Lock写；掉电持久性未验证',
                          '不同起点/方向只验原精度功能，不能证明P因果或PID即时采用或长期参数闭环',
                          '沿既有command预算，不提供硬实时绝对READ截止']}
(OUTPUT / 'result.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
print(json.dumps({'status': result['status'], 'distinct_memory_methods': len(methods), 'direct_children': len(receipts),
                  'natural_wait_reap': all(row['reaped'] for row in receipts), 'result': str(OUTPUT / 'result.json')},
                 ensure_ascii=False))
sys.exit(0 if passed else 1)
