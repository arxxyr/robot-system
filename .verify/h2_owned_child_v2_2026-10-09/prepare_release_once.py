"""本次 a96 软件冻结：复用既有构建器，不接触 UART 或激活服务。"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import sys
from types import SimpleNamespace

sys.dont_write_bytecode = True
BASE = Path('/home/loosqk/robot-checks/h2-exit-observation-20261009-r1')
SOURCE = BASE / 'source'
OUT = BASE / 'release-r1'
HEAD = 'a96c34ae26b56fc9e8d2deef17c3b829c3d87d0d'
SOURCE_SHA = '5970c9bed0ab02f99b47d88d47eb3718c6a748155ca4fafd5c18050a68ec6cf7'
FORMAL = Path('/home/loosqk/repo/robot/robot-system')
TARGET = Path('/home/loosqk/robot-checks/xlerobot-velocity-foundation-20261008-r7/build')
CACHE = '/home/loosqk/robot-checks/viz-orin-a6957cc-20260930-r1/cargo-home'
OVERLAYS = {'scripts/run_post_e_h2.py': '3b69527eb66115a3770b96ab55aa06d99716270399bb5cf2b0d84c1c9c51b9ae',
            'scripts/tests/test_run_post_e_h2.py': '28c9beee78f800b0d5c8b667144162875ff1924c76b81bf3a6893513de8dd119'}
modules = []
for name, path in [('artifacts', SOURCE / 'scripts/post_e_h2_artifacts.py'),
                   ('release_runner', Path('/home/loosqk/robot-checks/h2-right-ready-20261009-r1/bootstrap/run_release_v2.py'))]:
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    modules.append(module)
a, r = modules
a.require(a.sha256(Path(r.__file__)) == 'd6b20da28de8d5ae077a93cb2fb5ba7f27a66b8145df9e7e91951cb14ac0b12b', '原构建器改变')
a.require(a.sha256(Path(a.__file__)) == 'bd2b3f7bb8527cd9ecec73894171638e9cf378656b89e284a585c3baa37b8731', '共享校验改变')
OUT.mkdir()
r.save(OUT / 'owner.json', r.proc_entry(os.getpid()))
options = SimpleNamespace(source=SOURCE, target=TARGET, out=OUT, formal_root=FORMAL, build_head=HEAD)
runner = r.Runner(options, a)
proxy = {key: 'http://10.0.1.80:10808' for key in ('http_proxy', 'https_proxy', 'HTTP_PROXY', 'HTTPS_PROXY')}
runner.env.update(proxy)
runner.env['GIT_SSH_COMMAND'] = 'ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o ConnectTimeout=15'
runner.env['CARGO_HOME'] = CACHE
result = {'schema': 'P7_NATIVE_PREPARATION_V2', 'status': 'FAIL', 'deployable': False,
          'started_at': r.utc(), 'scope': '仅源码同步、新V2六配方七件及默认计划/help；无UART/安装/服务激活',
          'runner_sha256': a.sha256(Path(r.__file__)), 'preparation_sha256': a.sha256(Path(__file__)),
          'artifacts_module_sha256': a.sha256(Path(a.__file__)), 'proxy_runtime_overrides': proxy,
          'stage_cargo_home': {'nightly_update': '/home/loosqk/.cargo', 'cargo': CACHE}}
before = isolation = None
def text_command(label, argv, timeout=60):
    ref, row = runner.command(label, argv, timeout)
    return ref, Path(row['stdout']).read_text()
def formal(label):
    sys.path.insert(0, str(SOURCE / 'scripts'))
    import run_post_e_h2 as d
    _, output = text_command(label + '_services', ['systemctl', 'show', *d.UNITS, '-p', 'Id', '-p', 'ActiveState', '-p', 'SubState', '-p', 'MainPID'])
    rows = [dict(line.split('=', 1) for line in block.splitlines()) for block in output.strip().split('\n\n')]
    a.require(len(rows) == 6 and {row['Id'] for row in rows} == set(d.UNITS) and
              all(row['ActiveState'] == 'inactive' and row['SubState'] == 'dead' and row['MainPID'] == '0' for row in rows), '正式服务未停止')
    snapshot = d.process_snapshot(-1, OUT, [FORMAL / 'target/release' / name for name in a.INSTALLED_NAMES])
    r.save(OUT / (label + '_processes.json'), snapshot)
    a.require(snapshot['clean'], '正式控制进程或未知PID仍存在')
    return rows
def interrupted(signum, frame):
    r.INTERRUPTED = signum
for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
    signal.signal(sig, interrupted)
try:
    _, dirty = text_command('sync_status', ['git', 'status', '--porcelain=v1', '--untracked-files=all'])
    a.require(set(dirty.splitlines()) == {' M ' + name for name in OVERLAYS}, '同步前存在其他改动')
    a.require(all(a.sha256(SOURCE / name) == sha for name, sha in OVERLAYS.items()), '自有overlay改变')
    runner.command('fetch', ['git', 'fetch', 'origin', 'feat/post-e-comm-fault-hardening'], 300)
    _, remote = text_command('remote_head', ['git', 'rev-parse', 'origin/feat/post-e-comm-fault-hardening'])
    a.require(remote.strip() == HEAD, 'origin不是已核a96')
    for index, (name, sha) in enumerate(OVERLAYS.items()):
        _, content = text_command('remote_overlay_' + str(index), ['git', 'show', HEAD + ':' + name])
        a.require(hashlib.sha256(content.encode()).hexdigest() == sha, '远端overlay与现有字节不同')
    runner.command('restore_owned_overlays', ['git', 'restore', '--', *OVERLAYS])
    runner.command('ff', ['git', 'merge', '--ff-only', 'origin/feat/post-e-comm-fault-hardening'], 300)
    before = runner.source('source_before')
    a.require(len(before['files']) == 597 and before['source_manifest_sha256'] == SOURCE_SHA, 'Rust输入不同')
    result['helpers_before'] = {name: a.sha256(SOURCE / 'scripts' / name) for name in ('run_post_e_h2.py', 'post_e_h2_report.py', 'post_e_h2_artifacts.py')}
    installed = {name: FORMAL / 'target/release' / name for name in a.INSTALLED_NAMES}
    isolation = a.validate_target_isolation(TARGET, installed)
    r.save(OUT / 'isolation_before.json', isolation)
    result['formal_before'] = formal('formal_before')
    runner.env['CARGO_HOME'] = '/home/loosqk/.cargo'
    runner.command('nightly_update', ['rustup', 'update', 'nightly'], 300)
    runner.env['CARGO_HOME'] = CACHE
    rustc_before, compiler = runner.command('rustc_before', ['rustc', '-vV'])
    identity = a.parse_rustc_identity(Path(compiler['stdout']).read_text())
    runner.command('cargo_version', ['cargo', '-V'])
    old = a.strict_json((BASE / 'qa-r2/result.json').read_text())
    a.require(identity == old['rustc_identity'], '编译器改变，停止复用质量门')
    for label, path, sha in [('fmt', BASE / 'qa-r2/fmt/completion.json', 'b554435a73d48c0345845d08200d21c1f0e9bcff540150a31ded613718a23e4f'),
                             ('clippy', BASE / 'qa-r2/clippy/completion.json', '297408f3efb7b3e32431915351342340383ca6ffc375806551b956eb4f1f9de3'),
                             ('python124', BASE / 'qa-r4/python_contracts/completion.json', '31d60cb92c75ff34ded6b7ca8b0f451228291f07a26fc3ce2f3bf085ac7c7e3a')]:
        ref = {'path': str(path), 'sha256': sha}
        a.validate_completion(ref)
        result.setdefault('reused_quality', {})[label] = ref
    a.require(a.strict_json((BASE / 'qa-r2/source_after.json').read_text())['compiled_source'] == before, '已验Rust源不同')
    _, plan = text_command('default_plan', ['uv', 'run', '--no-project', 'python', 'scripts/run_post_e_h2.py'])
    a.require(a.strict_json(plan)['mode'] == 'PLAN_ONLY', '默认入口不是PLAN_ONLY')
    source_ref = {'path': str(OUT / 'source_before.json'), 'sha256': a.sha256(OUT / 'source_before.json')}
    sets = {kind: runner.freeze(kind, before, source_ref) for kind in ('production', 'measurement')}
    rustc_after, _ = runner.command('rustc_after', ['rustc', '-vV'])
    result['artifacts'] = {}
    for kind, manifest in sets.items():
        manifest.update(rustc_before=rustc_before, rustc_after=rustc_after, rustc_identity=identity)
        verified = a.validate_artifact_set(manifest, source_manifest=before, expected_kind=kind, expected_head=HEAD)
        path = OUT / (kind + '.json')
        r.save(path, manifest)
        result['artifacts'][kind] = {'path': str(path), 'sha256': a.sha256(path), 'validation': verified}
        for name, item in manifest['artifacts'].items():
            runner.command('help_' + kind + '_' + name, [item['path'], '--help'])
    a.require(runner.source('source_after') == before, '源码前后改变')
    result['source_after'] = 'PASS'
    result['helpers_after'] = {name: a.sha256(SOURCE / 'scripts' / name) for name in result['helpers_before']}
    a.require(result['helpers_after'] == result['helpers_before'], 'H2 helper改变')
    after = a.validate_target_isolation(TARGET, installed)
    r.save(OUT / 'isolation_after.json', after)
    a.require(after['installed'] == isolation['installed'], '正式四件改变')
    result['isolation_after'] = 'PASS'
    result['formal_after'] = formal('formal_after')
    a.require(result['formal_after'] == result['formal_before'], '正式服务前后改变')
    result['status'] = 'PASS'
except BaseException as error:
    result['error'] = repr(error)
finally:
    result.update(finished_at=r.utc(), receipts=runner.receipts, interrupted_signal=r.INTERRUPTED)
    r.save(OUT / 'result.json', result)
    print(json.dumps(result, ensure_ascii=False), flush=True)
raise SystemExit(0 if result['status'] == 'PASS' else 1)
