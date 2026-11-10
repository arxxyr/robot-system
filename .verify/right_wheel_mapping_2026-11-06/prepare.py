"""复用维护入口准备右9只读对照或ID7单轮测试；不打开UART、不创建GO。"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shlex
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--artifact-root', type=Path,
                    default=Path('/home/loosqk/robot-checks/right-wheel-mapping-20261106'),
                    help='本轮独立产物目录；已有准备文件拒绝覆盖')
parser.add_argument('--wheel-pulse', choices=('left',),
                    help='复用旧名义left入口，仅准备ID7一次+500/20秒及零速停止检查')
options = parser.parse_args()
ROOT = options.artifact_root
SCRIPT = Path('/home/loosqk/robot-checks/s0-bus2-raw-maintenance-20261023-r1/software/scripts/run_hardware_maintenance.py')
BINDINGS = Path('/home/loosqk/robot-checks/front-wheel-name-20261105/mixed-bindings.json')
PYTHON = '/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3'


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


module = load('maintenance_prepare', SCRIPT)
module.validate_artifact_root(ROOT, filesystem=True)
bindings = module.strict_json(BINDINGS.read_text())
host = bindings['host']
module.require(os.uname().nodename == host['hostname']
               and Path('/proc/sys/kernel/random/boot_id').read_text().strip() == host['boot_id']
               and os.getuid() == host['uid'] and os.getgid() == host['gid'], '主机身份变化')
module.protected_and_refs(bindings)
module.verify_ref(bindings['proc_helper'])
helper = load('maintenance_proc_identity', bindings['proc_helper']['path'])
preflight = ROOT / 'preparation'
preflight.mkdir()
io = module.RealExecutor(bindings['tool'], preflight)
io.process_snapshots = []
try:
    module.idle(io, bindings, helper)
    for side in module.SERIALS:
        module.usb_identity(io, side, bindings['usb'][side])
        for name in ('rx_trust.lock', 'estop_latch.lock'):
            for prefix in ([], ['sudo', '-n']):
                module.require(io.command(prefix + ['fuser', bindings['protection'][side][name]['path']],
                                          allowed=(1,)) == '', '保护锁被占用')
finally:
    io.journal.close()

plans = {}
reference_checks = 0
for phase in (('pulse',) if options.wheel_pulse else ('before', 'after')):
    run_id = 'wheel-id7-mapping-pulse' if options.wheel_pulse else f'right9-mapping-{phase}'
    argv = [PYTHON, '-I', '-S', str(SCRIPT), '--bindings', str(BINDINGS),
            '--output', str(ROOT / f'{phase}-capture'), '--run-id', run_id,
            '--artifact-root', str(ROOT)]
    argv += (['--wheel-pulse', options.wheel_pulse, '--zero-stop-check']
             if options.wheel_pulse else ['--right-wheel-feedback-only'])
    result = subprocess.run(argv, check=True, capture_output=True, text=True)
    document = module.strict_json(result.stdout)
    module.require(document['result'] == 'PLAN_ONLY' and document['actions_started'] == 0,
                   '只允许PLAN_ONLY准备')
    if options.wheel_pulse:
        contract = document['plan']['right_wheel_pulse']
        module.require(contract['target'][1] == 7 and contract['raw_velocity'] == 500
                       and contract['observation_ns'] == 20_000_000_000,
                       '本轮仅准备ID7固定+500/20秒')
        references = contract['sdk']['refs'] + [contract[key] for key in
                    ('raw_tool', 'raw_helper', 'strace', 'write_tool', 'sync_tool')]
        for reference in references:
            module.verify_ref(reference)
        reference_checks = len(references)
    module.write_new(ROOT / f'{phase}-plan.json', document)
    plans[phase] = document['plan_sha256']
    launch = ROOT / f'{phase}-launch'
    launch.mkdir()
    command = shlex.join(argv + ['--execute', '--approval', str(ROOT / f'{phase}-go.json')])
    launcher = ROOT / f'{phase}-launch.sh'
    launcher.write_text('#!/usr/bin/env bash\nset -u\n'
                        f'printf "%s\\n" "$$" > {shlex.quote(str(launch / "pid"))}\n'
                        f'date -u +%Y-%m-%dT%H:%M:%S.%NZ > {shlex.quote(str(launch / "started-at"))}\n'
                        'export PYTHONDONTWRITEBYTECODE=1\n'
                        f'{command} > {shlex.quote(str(launch / "stdout.log"))} '
                        f'2> {shlex.quote(str(launch / "stderr.log"))}\n'
                        'result=$?\n'
                        f'printf "%s\\n" "$result" > {shlex.quote(str(launch / "exit-code"))}\n'
                        'exit "$result"\n')
    subprocess.run(['bash', '-n', str(launcher)], check=True)

module.write_new(ROOT / 'preparation.json', {
    'result': 'PREPARED_NOT_EXECUTED', 'host': host, 'plans': plans,
    'script_sha256': hashlib.sha256(SCRIPT.read_bytes()).hexdigest(),
    'interpreter': PYTHON, 'checks': len(io.commands), 'identity': 'PASS',
    'pulse_reference_checks': reference_checks,
    'launch_syntax': 'PASS', 'uart_access': False, 'go_created': False,
    'register_writes': 0,
    'next': ('新的现场动作确认后仅执行一次ID7 +500/20秒，零速、断扭及原停止检查；'
             '观察机器人自身哪只轮盘转动及最终是否全部停止，不预设实物轮位。'
             if options.wheel_pulse else
             '新的现场确认后先before；14读完整、TE/GV0且正常收尾后才通知轻转整个右轮盘；'
             '明确确认放手后仍保持在新角度，再独立after。手转回执不续期现场事实；不自动连接两次执行。')})
print(json.dumps({'result': 'PREPARED_NOT_EXECUTED', 'plans': plans, 'uart_access': False}))
