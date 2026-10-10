"""复用既有 Runner：独立源码快进、16项只读准备和一次源码身份 probe。"""
import importlib.util
import json
import os
from pathlib import Path
import signal
import sys
from types import SimpleNamespace

sys.dont_write_bytecode = True
base = Path('/home/loosqk/robot-checks/h2-exit-observation-20261009-r1')
source, release, out = base / 'source', base / 'release-32d-r2', base / 'source-sync-r2'
control, readonly = base / 'bootstrap/control-v3', base / 'right-readonly-r2'
planned = base / 'right-h2-run-02'
target = Path('/home/loosqk/robot-checks/xlerobot-velocity-foundation-20261008-r7/build')
formal = Path('/home/loosqk/repo/robot/robot-system')
build = 'a96c34ae26b56fc9e8d2deef17c3b829c3d87d0d'
runtime = '894084738d95a7239611c2b8fbecfe081d3a7fc9'
branch = 'feat/post-e-comm-fault-hardening'
modules = []
for name, path in [('artifacts', source / 'scripts/post_e_h2_artifacts.py'),
                   ('release_runner', Path('/home/loosqk/robot-checks/h2-right-ready-20261009-r1/bootstrap/run_release_v2.py'))]:
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    modules.append(module)
a, r = modules
a.require(a.sha256(Path(r.__file__)) == 'd6b20da28de8d5ae077a93cb2fb5ba7f27a66b8145df9e7e91951cb14ac0b12b', '既有 Runner 身份不同')
a.require(a.sha256(Path(a.__file__)) == 'bd2b3f7bb8527cd9ecec73894171638e9cf378656b89e284a585c3baa37b8731', '共享校验模块不同')
sys.path.insert(0, str(source / 'scripts'))
import run_post_e_h2 as driver
def interrupted(signum, _frame):
    r.INTERRUPTED = signum
for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
    signal.signal(sig, interrupted)
a.require(not out.exists() and not readonly.exists() and not planned.exists(), '单次输出已存在')
out.mkdir()
r.save(out / 'owner.json', r.proc_entry(os.getpid()))
runner = r.Runner(SimpleNamespace(source=source, target=target, out=out, build_head=build), a)
runner.env['GIT_SSH_COMMAND'] = 'ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o ConnectTimeout=15'
runner.env['H2_EXPECTED_HEAD'] = runtime
runner.shown.update({key: runner.env[key] for key in ('GIT_SSH_COMMAND', 'H2_EXPECTED_HEAD')})
result = {'status': 'FAIL', 'started_at': r.utc(), 'build_head': build, 'runtime_head': runtime,
          'scope': '仅独立 source fetch/ff-only、既有16门和一次 alignment probe；无Cargo/UART/现场receipt/GO/安装/服务操作',
          'deployable': False, 'script_sha256': a.sha256(Path(__file__))}
manifests = {kind: a.strict_json((release / (kind + '.json')).read_text()) for kind in ('production', 'measurement')}
frozen_source = a.strict_json((release / 'source_before.json').read_text())
helper_names = ('run_post_e_h2.py', 'post_e_h2_report.py', 'post_e_h2_artifacts.py')
def text_command(label, argv, timeout=60):
    _, row = runner.command(label, argv, timeout)
    return Path(row['stdout']).read_text()
def identities():
    return {'frozen': {kind + '/' + name: a.elf_identity(item['path'])
                       for kind, manifest in manifests.items() for name, item in manifest['artifacts'].items()},
            'formal': {name: a.installed_file_identity(formal / 'target/release' / name) for name in a.INSTALLED_NAMES},
            'manifests': {kind: a.sha256(release / (kind + '.json')) for kind in manifests},
            'helpers': {name: a.sha256(source / 'scripts' / name) for name in helper_names}}
def formal_state(label):
    head = text_command(label + '_formal_head', ['git', '-C', str(formal), 'rev-parse', 'HEAD']).strip()
    status = text_command(label + '_formal_status', ['git', '-C', str(formal), 'status', '--porcelain=v1', '--untracked-files=all'])
    output = text_command(label + '_services', ['systemctl', 'show', *driver.UNITS, '-p', 'Id', '-p', 'ActiveState', '-p', 'SubState', '-p', 'MainPID'])
    rows = [dict(line.split('=', 1) for line in block.splitlines()) for block in output.strip().split('\n\n')]
    a.require(head.startswith('ca14f0e') and status == '' and len(rows) == 6 and {row['Id'] for row in rows} == set(driver.UNITS)
              and all(row['ActiveState'] == 'inactive' and row['SubState'] == 'dead' and row['MainPID'] == '0' for row in rows), '正式 ca14/六服务不符')
    return {'head': head, 'status_porcelain': status, 'services': rows}
try:
    before = runner.source('source_before')
    a.require(before == frozen_source and len(before['files']) == 597, '原 a96/597 冻结输入不符')
    initial = identities()
    r.save(out / 'identities_before.json', initial)
    result['formal_before'] = formal_state('before')
    compiler = a.parse_rustc_identity(text_command('rustc_before', ['rustc', '-vV']))
    a.require(compiler == manifests['production']['rustc_identity'] == manifests['measurement']['rustc_identity']
              and compiler['commit_hash'] == '32dba69d69c5b10ea89a4042de8d0619f7756203', '当前编译器不是实际32d冻结身份')
    text_command('fetch', ['git', 'fetch', 'origin', branch], 1800)
    a.require(text_command('fetch_head', ['git', 'rev-parse', 'FETCH_HEAD']).strip() == runtime, 'origin 分支不是已核894')
    text_command('ancestor', ['git', 'merge-base', '--is-ancestor', build, runtime])
    changed = text_command('changed_paths', ['git', 'diff', '--name-only', build, runtime]).splitlines()
    a.require(changed and all(name.startswith('.verify/') or name == 'team-swe/specs/post_e_comm_fault_hardening.HANDOFF.md' for name in changed), '祖先差异超出证据与HANDOFF')
    text_command('ff_only', ['git', 'merge', '--ff-only', runtime])
    runner.options.build_head = runtime
    current = runner.source('source_synced')
    a.require(current == before and identities() == initial, 'ff 后597/helper/七冻结件/正式四件发生变化')
    result.update(source_manifest_sha256=current['source_manifest_sha256'], changed_paths=changed,
                  build_ancestor_verified=True, rustc_identity=compiler)
    result['readonly_receipt'], _ = runner.command('readonly16', ['bash', str(control / 'prepare_readonly.sh')], 300)
    journal = a.strict_json((readonly / 'run.json').read_text())
    draft = a.strict_json((readonly / 'preparation_inputs_draft.json').read_text())
    a.require(journal['status'] == 'PASS' and len(journal['checks']) == 16 and all(row['status'] == 'PASS' for row in journal['checks'])
              and journal['authorization_attempts'] == journal['launch_attempts'] == 0
              and draft['physical'] is None and draft['alignment'] is None and not planned.exists(), '16门或无授权边界不符')
    result['readonly'] = {'path': str(readonly / 'run.json'), 'sha256': a.sha256(readonly / 'run.json'), 'checks': 16, 'authorization_attempts': 0, 'launch_attempts': 0}
    result['alignment_probe_receipt'], _ = runner.command('alignment_probe', ['bash', str(control / 'alignment_probe_r2.sh')], 120)
    result['alignment'] = {name: {'path': str(base / 'bootstrap' / name), 'sha256': a.sha256(base / 'bootstrap' / name)}
                           for name in ('s52-alignment-identity-r2.json', 's52-alignment-source-r2.json')}
    after = runner.source('source_after')
    a.require(after == before, '末尾597输入变化')
    final = identities()
    r.save(out / 'identities_after.json', final)
    a.require(final == initial, '末尾 helper/七件/正式四件变化')
    result['formal_after'] = formal_state('after')
    a.require(result['formal_after'] == result['formal_before'], '正式 ca14/六服务前后变化')
    a.require(a.parse_rustc_identity(text_command('rustc_after', ['rustc', '-vV'])) == compiler, '编译器前后变化')
    a.require(not planned.exists() and a.sha256(Path(__file__)) == result['script_sha256'], '计划输出出现或单次脚本变化')
    result.update(status='PASS', source_after='PASS', identities_after='PASS', planned_output_created=False)
except BaseException as error:
    result['error'] = repr(error)
result.update(finished_at=r.utc(), receipts=runner.receipts)
r.save(out / 'result.json', result)
print(json.dumps(result, ensure_ascii=False), flush=True)
raise SystemExit(0 if result['status'] == 'PASS' else 1)
