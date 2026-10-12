"""爸爸，纯软件fixture的自有pane；仅验证-p同形调用，不导入机器人候选。"""
import errno
import json
import os
from pathlib import Path
import subprocess
import sys
import time

case = Path(sys.argv[1])
flag = '-p'
assert len(sys.argv) == 2
assert case.is_absolute() and case.is_dir()
fd = os.open(case / 'driver.log', os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
os.dup2(fd, 1); os.dup2(fd, 2); os.close(fd)
real_popen = subprocess.Popen
children = []
def recording_popen(*args, **kwargs):
    child = real_popen(*args, **kwargs)
    children.append((child, time.time_ns()))
    return child
subprocess.Popen = recording_popen
row = {'scope': 'SOFTWARE_ONLY_TMUX_OPTION_FIXTURE', 'driver_pid': os.getpid(),
       'pane': os.environ['TMUX_PANE'], 'flag': flag, 'started_ns': time.time_ns(),
       'after_option_reached': False, 'children': [], 'UART': 0, 'GO': False}
code = 1
try:
    before = subprocess.run(['tmux', 'show-options', '-p', '-v', '-t', os.environ['TMUX_PANE'],
                             'remain-on-exit'], check=True, capture_output=True, text=True)
    row['remain_on_exit_before'] = before.stdout
    # 关键调用与失败launcher同形态，唯一差别为-w改-p；只指向自身pane。
    subprocess.run(['tmux', 'set-option', '-p', '-t', os.environ['TMUX_PANE'],
                    'remain-on-exit', 'on'], check=True)
    row['after_option_reached'] = True
    after = subprocess.run(['tmux', 'show-options', '-p', '-v', '-t', os.environ['TMUX_PANE'],
                            'remain-on-exit'], check=True, capture_output=True, text=True)
    row['remain_on_exit_after'] = after.stdout
    code = 0
except BaseException as error:
    row['error'] = repr(error)
finally:
    subprocess.Popen = real_popen
    for child, started in children:
        returned = child.wait()
        reaped = False
        try:
            os.waitpid(child.pid, os.WNOHANG)
        except ChildProcessError as error:
            reaped = error.errno == errno.ECHILD
        row['children'].append({'argv': child.args, 'pid': child.pid, 'started_ns': started,
                                'exit_code': returned, 'real_wait_returncode': returned,
                                'reaped': reaped, 'signals_sent': []})
    row.update(finished_ns=time.time_ns(), option_wrapper_exit=code)
    with (case / 'driver-receipt.json').open('x') as stream:
        json.dump(row, stream, ensure_ascii=False, indent=2); stream.write('\n')
if code:
    sys.exit(code)
# 成功时仅exec纯软件marker，保持同driver PID；绝不执行硬件候选。
marker_value = 'SOFTWARE_ONLY_REACHED\n'
marker_code = "from pathlib import Path; Path(" + repr(str(case / 'software-marker.txt')) + ").write_text(" + repr(marker_value) + ")"
os.execv(sys.executable, [sys.executable, '-c', marker_code])
