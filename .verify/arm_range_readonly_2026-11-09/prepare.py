"""准备已有维护入口的右臂范围只读；不打开串口、不创建GO。"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shlex
import subprocess

ROOT = Path('/home/loosqk/robot-checks/arm-range-readonly-20261110')
SCRIPT = ROOT / 'run_hardware_maintenance.py'
CONFIG = ROOT / 'xlerobot_bus2_full.toml'
BINDINGS = Path('/home/loosqk/robot-checks/front-wheel-name-20261105/mixed-bindings.json')
PYTHON = '/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3'


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


m = load('arm_range_prepare', SCRIPT)
m.configure_bus_mapping(CONFIG)
m.validate_artifact_root(ROOT, filesystem=True)
bindings = m.strict_json(BINDINGS.read_text())
host = bindings['host']
m.require(os.uname().nodename == host['hostname']
          and Path('/proc/sys/kernel/random/boot_id').read_text().strip() == host['boot_id']
          and os.getuid() == host['uid'] and os.getgid() == host['gid'], '主机身份变化')
m.protected_and_refs(bindings)
m.verify_ref(bindings['proc_helper'])
helper = load('maintenance_proc_identity', bindings['proc_helper']['path'])
preflight = ROOT / 'preparation'
preflight.mkdir()
io = m.RealExecutor(bindings['tool'], preflight)
io.process_snapshots = []
try:
    m.idle(io, bindings, helper)
    for side in m.SERIALS:
        m.usb_identity(io, side, bindings['usb'][side])
        for name in ('rx_trust.lock', 'estop_latch.lock'):
            for prefix in ([], ['sudo', '-n']):
                m.require(io.command(prefix + ['fuser', bindings['protection'][side][name]['path']],
                                     allowed=(1,)) == '', '保护锁被占用')
finally:
    io.journal.close()

argv = [PYTHON, '-I', '-S', '-B', str(SCRIPT), '--bus-config', str(CONFIG),
        '--bindings', str(BINDINGS), '--output', str(ROOT / 'capture'),
        '--run-id', 'bus2-arm-range-readonly', '--artifact-root', str(ROOT),
        '--bus2-arm-range-only']
result = subprocess.run(argv, capture_output=True, text=True)
(ROOT / 'plan.stdout').write_text(result.stdout)
(ROOT / 'plan.stderr').write_text(result.stderr)
m.require(result.returncode == 0, 'PLAN_ONLY失败：' + result.stderr.strip())
document = m.strict_json(result.stdout)
m.require(document['result'] == 'PLAN_ONLY' and document['actions_started'] == 0
          and 'bus2_arm_range_only' in document['plan'], '必须为右臂只读计划')
m.write_new(ROOT / 'plan.json', document)
launch = ROOT / 'launch'
launch.mkdir()
command = shlex.join(argv + ['--execute', '--approval', str(ROOT / 'go.json')])
launcher = ROOT / 'launch.sh'
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
m.write_new(ROOT / 'preparation.json', {
    'result': 'PREPARED_NOT_EXECUTED', 'host': host,
    'plan_sha256': document['plan_sha256'], 'checks': len(io.commands),
    'script_sha256': hashlib.sha256(SCRIPT.read_bytes()).hexdigest(),
    'bus_config_sha256': hashlib.sha256(CONFIG.read_bytes()).hexdigest(),
    'interpreter': PYTHON, 'identity': 'PASS', 'launch_syntax': 'PASS',
    'uart_access': False, 'go_created': False, 'register_writes': 0,
    'next': '取得本轮供电、双臂支撑和持续看护回执后，仅执行右臂六电机18项只读；不自动串联混合动作。'
})
print(json.dumps({'result': 'PREPARED_NOT_EXECUTED', 'checks': len(io.commands),
                  'plan_sha256': document['plan_sha256'], 'uart_access': False}))
