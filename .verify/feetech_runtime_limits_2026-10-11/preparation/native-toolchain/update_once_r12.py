"""仅更新 floating nightly；每个 child 自然 communicate/wait，不发信号。"""

import datetime
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import time

SOURCE = Path('/home/loosqk/robot-checks/position-return-20261010-r1/position-return-r7/source')
OUT = Path('/home/loosqk/robot-checks/position-return-20261011-r12/native-toolchain/update')
HELPER = Path('/home/loosqk/robot-checks/h2-exit-observation-20261009-r1/source/scripts/post_e_h2_artifacts.py')
EXPECTED_HEAD = '894084738d95a7239611c2b8fbecfe081d3a7fc9'
EXPECTED_SOURCE = '3a4cc12e5e28f9c9164b8e7cdf9e2861b7127d7536ab291e7db8cced6d7aa4e9'


def utc():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def save(path, value):
    with path.open('x') as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2, allow_nan=False)
        stream.write('\n')
        stream.flush()
        os.fsync(stream.fileno())


OUT.mkdir()
assert hashlib.sha256(HELPER.read_bytes()).hexdigest() == 'bd2b3f7bb8527cd9ecec73894171638e9cf378656b89e284a585c3baa37b8731'
spec = importlib.util.spec_from_file_location('existing_source_helper', HELPER)
a = importlib.util.module_from_spec(spec)
spec.loader.exec_module(a)
env = dict(os.environ)
removed = []
for key in ('ALL_PROXY', 'all_proxy', 'http_proxy', 'https_proxy', 'HTTP_PROXY', 'HTTPS_PROXY', 'LD_PRELOAD', 'RUSTUP_DIST_SERVER', 'RUSTUP_UPDATE_ROOT'):
    if key in env:
        removed.append(key)
        del env[key]
env.update(PATH='/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:' + env['PATH'],
           HTTP_PROXY='http://10.0.1.80:10808', HTTPS_PROXY='http://10.0.1.80:10808',
           RUSTUP_TOOLCHAIN='nightly', PYTHONDONTWRITEBYTECODE='1')
shown = {key: env[key] for key in ('PATH', 'HTTP_PROXY', 'HTTPS_PROXY', 'RUSTUP_TOOLCHAIN', 'PYTHONDONTWRITEBYTECODE')}
receipts = []
result = {'schema': 'R12_NIGHTLY_UPDATE_ONCE_V1', 'status': 'FAIL', 'started_utc': utc(),
          'scope': '仅rustup nightly一次更新和只读身份；无Cargo检查/构建、UART、GO、控制服务或源码写入',
          'signals_sent': [], 'update_attempts': 0, 'source_head_expected': EXPECTED_HEAD,
          'proxy': shown, 'removed_environment_keys': removed}


def command(name, argv):
    directory = OUT / name
    directory.mkdir()
    row = {'name': name, 'argv': argv, 'cwd': str(SOURCE), 'env': shown,
           'started_utc': utc(), 'started_monotonic_ns': time.monotonic_ns(),
           'reaped': False, 'wait_returncode': None, 'signals_sent': [], 'timeout': None}
    child = subprocess.Popen(argv, cwd=SOURCE, env=env, stdin=subprocess.DEVNULL,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    row['pid'] = child.pid
    save(directory / 'launch.json', row)
    stdout, stderr = child.communicate()
    code = child.wait()
    row.update(reaped=True, wait_returncode=code, exit_code=code, finished_utc=utc(),
               ended_monotonic_ns=time.monotonic_ns())
    for label, raw in (('stdout', stdout), ('stderr', stderr)):
        path = directory / (label + '.log')
        with path.open('xb') as stream:
            stream.write(raw)
            stream.flush()
            os.fsync(stream.fileno())
        row[label] = {'path': str(path), 'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}
    save(directory / 'completion.json', row)
    receipts.append(row)
    print(json.dumps({'name': name, 'pid': child.pid, 'exit_code': code, 'reaped': True}, ensure_ascii=False), flush=True)
    assert code == 0, row
    return stdout.decode()


def source_snapshot(label):
    head = command(label + '_head', ['git', 'rev-parse', 'HEAD']).strip()
    status = command(label + '_status', ['git', 'status', '--porcelain=v1', '--untracked-files=all'])
    files = command(label + '_files', ['git', 'ls-files', '-z', '--cached', '--others', '--exclude-standard'])
    snapshot = a.snapshot_source(SOURCE, files.rstrip('\0').split('\0'))
    save(OUT / (label + '.json'), snapshot)
    assert head == EXPECTED_HEAD and len(snapshot['files']) == 597 and snapshot['source_manifest_sha256'] == EXPECTED_SOURCE
    return {'head': head, 'status': status, 'manifest': snapshot}


try:
    before = source_snapshot('source_before')
    result['source_before'] = {'head': before['head'], 'status': before['status'], 'file_count': 597,
                               'manifest_sha256': before['manifest']['source_manifest_sha256']}
    result['rustup_active_before'] = command('rustup_active_before', ['rustup', 'show', 'active-toolchain']).strip()
    result['compiler_before'] = a.parse_rustc_identity(command('rustc_before', ['rustc', '+nightly', '-vV']))
    result['update_attempts'] = 1
    command('rustup_update_nightly', ['rustup', 'update', 'nightly', '--no-self-update'])
    result['compiler_after'] = a.parse_rustc_identity(command('rustc_after', ['rustc', '+nightly', '-vV']))
    result['rustup_active_after'] = command('rustup_active_after', ['rustup', 'show', 'active-toolchain']).strip()
    result['components_after'] = command('components_after', ['rustup', 'component', 'list', '--installed', '--toolchain', 'nightly']).splitlines()
    after = source_snapshot('source_after')
    assert after == before
    assert result['compiler_after']['host'] == 'aarch64-unknown-linux-gnu' and 'nightly' in result['compiler_after']['release']
    result.update(status='PASS', source_before_after='PASS', source_after=result['source_before'],
                  compiler_changed=result['compiler_before'] != result['compiler_after'])
except BaseException as error:
    result['error'] = repr(error)
finally:
    result.update(finished_utc=utc(), receipts=receipts)
    save(OUT / 'result.json', result)
    print(json.dumps(result, ensure_ascii=False), flush=True)
raise SystemExit(0 if result['status'] == 'PASS' else 1)
