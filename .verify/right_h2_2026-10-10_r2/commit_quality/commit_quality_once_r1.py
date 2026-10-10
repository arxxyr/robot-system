"""本次右H2终态收尾完成后仅执行提交质量门，不触硬件。"""

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
OUT = Path('/home/loosqk/robot-checks/h2-right-r2-commit-quality-20261010-r1/quality-r1')
HEAD = '894084738d95a7239611c2b8fbecfe081d3a7fc9'
SOURCE_SHA = '5970c9bed0ab02f99b47d88d47eb3718c6a748155ca4fafd5c18050a68ec6cf7'
TARGET = Path('/home/loosqk/robot-checks/xlerobot-velocity-foundation-20261008-r7/build')
CACHE = '/home/loosqk/robot-checks/viz-orin-a6957cc-20260930-r1/cargo-home'
RUNNER_PATH = Path('/home/loosqk/robot-checks/h2-right-ready-20261009-r1/bootstrap/run_release_v2.py')
RUNNER_SHA = 'd6b20da28de8d5ae077a93cb2fb5ba7f27a66b8145df9e7e91951cb14ac0b12b'
ARTIFACTS_SHA = 'bd2b3f7bb8527cd9ecec73894171638e9cf378656b89e284a585c3baa37b8731'

modules = []
for name, path in [('artifacts', SOURCE / 'scripts/post_e_h2_artifacts.py'),
                   ('release_runner', RUNNER_PATH)]:
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    modules.append(module)
a, r = modules
a.require(a.sha256(RUNNER_PATH) == RUNNER_SHA, '既有Runner身份改变')
a.require(a.sha256(Path(a.__file__)) == ARTIFACTS_SHA, '共享校验模块身份改变')
OUT.mkdir()
r.save(OUT / 'owner.json', r.proc_entry(os.getpid()))
runner = r.Runner(SimpleNamespace(source=SOURCE, target=TARGET, out=OUT, build_head=HEAD), a)
proxy = {name: 'http://10.0.1.80:10808'
         for name in ('http_proxy', 'https_proxy', 'HTTP_PROXY', 'HTTPS_PROXY')}
runner.env.update(proxy)
runner.shown.update(proxy)


def cargo_home(path):
    # 实际child环境和逐命令回执同时记录当前阶段的CARGO_HOME。
    runner.env['CARGO_HOME'] = path
    runner.shown['CARGO_HOME'] = path


def command_text(label, argv, timeout=60):
    ref, row = runner.command(label, argv, timeout)
    return ref, Path(row['stdout']).read_text()


def interrupted(signum, frame):
    r.INTERRUPTED = signum


for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
    signal.signal(sig, interrupted)

result = {
    'schema': 'H2_COMMIT_QUALITY_ONCE_V1',
    'status': 'FAIL',
    'started_at': r.utc(),
    'scope': '本次右H2终态收尾完成：实际更新floating nightly、身份、fmt→根Clippy及首尾绑定；无测试/release/硬件/服务操作。',
    'expected_head': HEAD,
    'expected_rust_inputs': 597,
    'expected_source_manifest_sha256': SOURCE_SHA,
    'runner_sha256': RUNNER_SHA,
    'artifacts_module_sha256': ARTIFACTS_SHA,
    'script_sha256': a.sha256(Path(__file__)),
    'target': str(TARGET),
    'proxy_runtime_overrides': proxy,
    'stage_cargo_home': {'nightly_update': '/home/loosqk/.cargo', 'cargo': CACHE},
}
before = compiler = cargo_before = None
try:
    cargo_home('/home/loosqk/.cargo')
    before = runner.source('source_before')
    a.require(len(before['files']) == 597 and before['source_manifest_sha256'] == SOURCE_SHA,
              '597项Rust源码身份不同')
    runner.command('nightly_update', ['rustup', 'update', 'nightly'], 7200)
    cargo_home(CACHE)
    _, rustc_text = command_text('rustc_before', ['rustc', '-vV'])
    compiler = a.parse_rustc_identity(rustc_text)
    result['rustc_identity'] = compiler
    _, cargo_before = command_text('cargo_before', ['cargo', '-V'])
    result['cargo_identity'] = cargo_before.strip()
    runner.command('fmt', ['cargo', 'fmt', '--all'], 300)
    runner.command('clippy', ['cargo', 'clippy', '--all', '--all-targets', '--', '-D', 'warnings'], 7200)
    result['status'] = 'PASS'
except BaseException as error:
    result['error'] = repr(error)
finally:
    try:
        a.require(before is not None and runner.source('source_after') == before, '源码首尾不同或前身份未采')
        result['source_after'] = 'PASS'
    except BaseException as error:
        result.update(status='FAIL', source_after=repr(error))
    try:
        cargo_home(CACHE)
        _, rustc_text = command_text('rustc_after', ['rustc', '-vV'])
        a.require(compiler is not None and a.parse_rustc_identity(rustc_text) == compiler,
                  '更新后编译器首尾不同或前身份未采')
        result['compiler_after'] = 'PASS'
        _, cargo_after = command_text('cargo_after', ['cargo', '-V'])
        a.require(cargo_before is not None and cargo_after == cargo_before, 'Cargo首尾身份不同或前身份未采')
        result['cargo_after'] = 'PASS'
    except BaseException as error:
        result.update(status='FAIL', compiler_after=repr(error))
    result.update(finished_at=r.utc(), receipts=runner.receipts, interrupted_signal=r.INTERRUPTED)
    r.save(OUT / 'result.json', result)
    print(json.dumps(result, ensure_ascii=False), flush=True)
raise SystemExit(0 if result['status'] == 'PASS' else 1)
