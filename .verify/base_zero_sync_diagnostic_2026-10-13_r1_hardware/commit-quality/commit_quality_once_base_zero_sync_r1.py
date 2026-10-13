"""爸爸，单轮SYNC零值R1本轮证据提交的独立原生软件质量检查；不接触 UART、GO、服务或正式安装。"""

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

BASE = Path('/home/loosqk/robot-checks/base-zero-sync-r1-commit-quality-20261013')
SOURCE = Path('/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/source')
TARGET = Path('/home/loosqk/robot-checks/xlerobot-velocity-foundation-20261008-r7/build')
OUT = BASE / 'quality'
HEAD = '5ee8a9c427c2010fe760c2090aac78acc0c64623'
NEW = 'ae00cf4f50d50e96de18a41ac362fc363851f2087afee04b01744bea11b68ee5'


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


a = module('r12_existing_artifacts', Path('/home/loosqk/robot-checks/h2-exit-observation-20261009-r1/source/scripts/post_e_h2_artifacts.py'))
r = module('r12_existing_runner', Path('/home/loosqk/robot-checks/h2-right-ready-20261009-r1/bootstrap/run_release_v2.py'))
assert a.sha256(Path(r.__file__)) == 'd6b20da28de8d5ae077a93cb2fb5ba7f27a66b8145df9e7e91951cb14ac0b12b'
assert a.sha256(Path(a.__file__)) == 'bd2b3f7bb8527cd9ecec73894171638e9cf378656b89e284a585c3baa37b8731'
expected = a.strict_json((BASE / 'boot/host-source-root597.json').read_text())
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
result = {'schema': 'BASE_ZERO_SYNC_R1_COMMIT_QUALITY_V1', 'status': 'FAIL', 'started_at': r.utc(), 'scope': 'latestnightly→实际fmt→仅默认根Clippy；597三窗/编译器前后；不测试/release/ELF/UART/USB/GO/服务', 'native_git_head': None, 'root_git_head_provenance_only': HEAD, 'source_expected': NEW, 'hardware_run_result': 'NEEDS_MANUAL_POWER_CUT_UNCHANGED', 'physical_cut_confirmation_at_dispatch': 'PENDING', 'hardware_stop_or_identity_proof': False}


def text(name, argv, timeout=7200):
    _, row = runner.command(name, argv, timeout)
    return Path(row['stdout']).read_text()


def snapshot(label, digest):
    value = a.snapshot_source(SOURCE, list(expected['files']))
    assert len(value['files']) == 597 and value['source_manifest_sha256'] == digest and value == expected
    r.save(OUT / (label + '.json'), value)
    return value


try:
    runner.command('nightly_update', ['rustup', 'update', 'nightly', '--no-self-update'], 7200)
    before = snapshot('source_before', NEW)
    compiler = a.parse_rustc_identity(text('rustc_before', ['rustc', '-vV']))
    assert compiler['host'] == 'aarch64-unknown-linux-gnu'
    runner.command('fmt', ['cargo', 'fmt', '--all'], 300)
    fmt_after = a.snapshot_source(SOURCE, list(expected['files']))
    r.save(OUT / 'fmt_after.json', fmt_after)
    assert fmt_after == before
    runner.command('clippy', ['cargo', 'clippy', '--all', '--all-targets', '--', '-D', 'warnings'], 7200)
    after = snapshot('source_after', NEW)
    compiler_after = a.parse_rustc_identity(text('rustc_after', ['rustc', '-vV']))
    assert after == before and compiler_after == compiler
    result.update(status='PASS', compiler=compiler, source_before_after='PASS', compiler_before_after='PASS', tests_run=0, release_builds=0, ELF_executions=0, UART_operations=0, USB_checks=0, GO_created=False, services_touched=0)
except BaseException as error:
    result['error'] = repr(error)
finally:
    result.update(finished_at=r.utc(), receipts=runner.receipts)
    r.save(OUT / 'result.json', result)
    print(json.dumps(result, ensure_ascii=False), flush=True)
raise SystemExit(0 if result['status'] == 'PASS' else 1)
