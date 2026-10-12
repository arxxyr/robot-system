"""爸爸，底盘零目标R2准备的独立原生软件质量检查；不接触 UART、GO、服务或正式安装。"""

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import time
from types import SimpleNamespace

BASE = Path('/home/loosqk/robot-checks/base-zero-r2-quality')
SOURCE = Path('/home/loosqk/robot-checks/position-return-20261010-r1/position-return-r7/source')
TARGET = Path('/home/loosqk/robot-checks/xlerobot-velocity-foundation-20261008-r7/build')
OUT = BASE / 'quality'
HEAD = '894084738d95a7239611c2b8fbecfe081d3a7fc9'
OLD = '3a4cc12e5e28f9c9164b8e7cdf9e2861b7127d7536ab291e7db8cced6d7aa4e9'
NEW = '068277d843b191bc8283bab38e667b0f6a4d30e296e8c5a99a97cfe19654fcf8'
COMPILER = '080cc27146389dbb5c8772c476b1937be9c6110f'
FIVE = ['crates/robot-adapters/feetech/examples/feetech_poke.rs',
        'crates/robot-adapters/feetech/src/bus.rs',
        'crates/robot-adapters/feetech/src/bus_raw_read_tests.rs',
        'crates/robot-adapters/feetech/src/diagnostic.rs',
        'crates/robot-adapters/feetech/src/registers.rs']


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


a = module('r12_existing_artifacts', Path('/home/loosqk/robot-checks/h2-exit-observation-20261009-r1/source/scripts/post_e_h2_artifacts.py'))
r = module('r12_existing_runner', Path('/home/loosqk/robot-checks/h2-right-ready-20261009-r1/bootstrap/run_release_v2.py'))
assert a.sha256(Path(r.__file__)) == 'd6b20da28de8d5ae077a93cb2fb5ba7f27a66b8145df9e7e91951cb14ac0b12b'
assert a.sha256(Path(a.__file__)) == 'bd2b3f7bb8527cd9ecec73894171638e9cf378656b89e284a585c3baa37b8731'
expected = a.strict_json(Path('/home/loosqk/robot-checks/position-return-20261011-r12/native-quality/boot/host_source_final_r12.json').read_text())
assert expected['source_manifest_sha256'] == NEW and len(expected['files']) == 597
OUT.mkdir()
runner = r.Runner(SimpleNamespace(source=SOURCE, target=TARGET, out=OUT, build_head=HEAD), a)
runner.env['CARGO_HOME'] = '/home/loosqk/robot-checks/viz-orin-a6957cc-20260930-r1/cargo-home'
runner.shown['CARGO_HOME'] = runner.env['CARGO_HOME']
runner.env['PATH'] = '/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:' + runner.env['PATH']
runner.shown['PATH'] = runner.env['PATH']
for key in ('ALL_PROXY', 'all_proxy', 'http_proxy', 'https_proxy', 'HTTP_PROXY', 'HTTPS_PROXY', 'LD_PRELOAD'):
    runner.env.pop(key, None)
runner.env.update(HTTP_PROXY='http://10.0.1.80:10808', HTTPS_PROXY='http://10.0.1.80:10808', PYTHONDONTWRITEBYTECODE='1')
runner.shown.update(HTTP_PROXY=runner.env['HTTP_PROXY'], HTTPS_PROXY=runner.env['HTTPS_PROXY'])
raw_receipts = []
result = {'schema': 'BASE_ZERO_R2_COMMIT_QUALITY_V1', 'status': 'FAIL', 'started_at': r.utc(),
          'scope': '代理更新nightly后仅实际fmt→根Clippy一次；不测试/release/UART/GO/服务',
          'native_git_head': HEAD, 'native_worktree': 'five changed sources; not clean',
          'source_before_install_expected': OLD, 'source_expected': NEW, 'compiler_expected': COMPILER}


def text(name, argv, timeout=7200):
    _, row = runner.command(name, argv, timeout)
    return Path(row['stdout']).read_text()


def snapshot(label, digest):
    head = text(label + '_head', ['git', 'rev-parse', 'HEAD'])
    status = text(label + '_status', ['git', 'status', '--porcelain=v1', '--untracked-files=all'])
    names = text(label + '_files', ['git', 'ls-files', '-z', '--cached', '--others', '--exclude-standard'])
    assert head.strip() == HEAD and status == ''.join(' M ' + name + '\n' for name in sorted(FIVE))
    value = a.snapshot_source(SOURCE, names.rstrip('\0').split('\0'))
    assert len(value['files']) == 597 and value['source_manifest_sha256'] == digest
    r.save(OUT / (label + '.json'), value)
    return value


try:
    runner.command('nightly_update', ['rustup', 'update', 'nightly', '--no-self-update'], 7200)
    before = snapshot('source_before', NEW)
    compiler = a.parse_rustc_identity(text('rustc_before', ['rustc', '-vV']))
    assert compiler['commit_hash'] == COMPILER and compiler['host'] == 'aarch64-unknown-linux-gnu'
    runner.command('fmt', ['cargo', 'fmt', '--all'], 300)
    fmt_after = a.snapshot_source(SOURCE, list(expected['files']))
    r.save(OUT / 'fmt_after.json', fmt_after)
    assert fmt_after == before
    runner.command('clippy', ['cargo', 'clippy', '--all', '--all-targets', '--', '-D', 'warnings'], 7200)
    after = snapshot('source_after', NEW)
    compiler_after = a.parse_rustc_identity(text('rustc_after', ['rustc', '-vV']))
    assert after == before and compiler_after == compiler
    result.update(status='PASS', compiler=compiler, source_before_after='PASS', compiler_before_after='PASS', tests_run=0, release_builds=0, UART_operations=0)
except BaseException as error:
    result['error'] = repr(error)
finally:
    result.update(finished_at=r.utc(), receipts=runner.receipts)
    r.save(OUT / 'result.json', result)
    print(json.dumps(result, ensure_ascii=False), flush=True)
raise SystemExit(0 if result['status'] == 'PASS' else 1)
