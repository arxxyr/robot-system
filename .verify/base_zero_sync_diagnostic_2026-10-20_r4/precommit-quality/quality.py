"""仅在独立导出源执行 nightly 更新、格式化和根 Clippy。"""
import datetime
import hashlib
import json
import os
from pathlib import Path
import subprocess

base = Path('/home/loosqk/robot-checks/base-zero-quality-20261020-r4')
source = Path('/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/source')
target = Path('/home/loosqk/robot-checks/xlerobot-velocity-foundation-20261008-r7/build')
expected = json.loads(Path('/home/loosqk/robot-checks/evidence-dedup-20261019/expected-source.json').read_text())
result = {'status': 'FAIL', 'steps': [], 'uart': 0, 'services': 0, 'tests': 0, 'release_builds': 0}

def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()

def snapshot(name):
    files = {}
    for name_in_source in sorted(expected['files']):
        path = source / name_in_source
        assert path.is_file() and not path.is_symlink(), str(path)
        data = path.read_bytes()
        files[name_in_source] = {'sha256': hashlib.sha256(data).hexdigest(), 'bytes': len(data), 'executable': bool(path.stat().st_mode & 0o100)}
    value = {'domains': expected['domains'], 'files': files, 'source_manifest_sha256': hashlib.sha256(json.dumps(files, sort_keys=True, separators=(',', ':'), ensure_ascii=False, allow_nan=False).encode()).hexdigest()}
    (base / (name + '.json')).write_text(json.dumps(value, indent=2) + '\n')
    assert value == expected, '源文件和本机预期不一致'
    return value

env = dict(os.environ)
for key in list(env):
    if key in {'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'RUSTDOCFLAGS', 'RUSTC', 'RUSTDOC', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER', 'RUST_TEST_THREADS', 'CLIPPY_ARGS', 'CARGO_INCREMENTAL', 'NOTIFY_SOCKET', 'INVOCATION_ID', 'WATCHDOG_USEC', 'WATCHDOG_PID', 'ALL_PROXY', 'all_proxy', 'http_proxy', 'https_proxy', 'LD_PRELOAD'} or key.startswith(('CARGO_PROFILE_', 'CARGO_TARGET_', 'CARGO_BUILD_', 'ROBOT_TEST_')):
        del env[key]
env.update(PATH='/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:' + env['PATH'], RUSTUP_TOOLCHAIN='nightly', RUSTC_WRAPPER='', CARGO_TERM_COLOR='never', CARGO_TARGET_DIR=str(target), CARGO_BUILD_JOBS='2', CARGO_HOME='/home/loosqk/robot-checks/viz-orin-a6957cc-20260930-r1/cargo-home', HTTP_PROXY='http://10.0.1.80:10808', HTTPS_PROXY='http://10.0.1.80:10808')

def run(name, argv):
    row = {'name': name, 'argv': argv, 'started_at': now()}
    with (base / (name + '.log')).open('xb') as log:
        process = subprocess.Popen(argv, cwd=source, env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
        row['pid'] = process.pid
        row['exit_code'] = process.wait()
    row.update(finished_at=now(), reaped=True)
    result['steps'].append(row)
    (base / 'progress.json').write_text(json.dumps(result, indent=2) + '\n')
    assert row['exit_code'] == 0, name + ' 失败'

result['started_at'] = now()
result['source_count'] = len(expected['files'])
result['source_manifest_sha256'] = expected['source_manifest_sha256']
result['boot_id'] = Path('/proc/sys/kernel/random/boot_id').read_text().strip()
try:
    assert source.is_dir() and target.is_dir()
    active = []
    for proc in Path('/proc').iterdir():
        if not proc.name.isdigit():
            continue
        try:
            comm = (proc / 'comm').read_text().strip()
            if comm in {'cargo', 'rustc', 'rustup', 'clippy-driver', 'cargo-clippy', 'cargo-fmt', 'rustfmt'}:
                active.append({'pid': int(proc.name), 'comm': comm})
        except (FileNotFoundError, PermissionError):
            pass
    result['existing_compilers'] = active
    assert not active, '发现外部编译进程，不争用已有缓存'
    snapshot('source-before')
    run('rustc-before', ['rustc', '-vV'])
    assert 'commit-hash: 07e526db9df34143c99ec8064acc8ad543f69b30' in (base / 'rustc-before.log').read_text(), 'nightly身份变化'
    run('fmt', ['cargo', 'fmt', '--all'])
    snapshot('source-after-fmt')
    run('clippy', ['cargo', 'clippy', '--all', '--all-targets', '--', '-D', 'warnings'])
    run('rustc-after', ['rustc', '-vV'])
    assert (base / 'rustc-before.log').read_bytes() == (base / 'rustc-after.log').read_bytes(), '编译器身份变化'
    result['status'] = 'PASS'
except BaseException as error:
    result['error'] = repr(error)
finally:
    try:
        snapshot('source-after')
        result['source_unchanged'] = True
    except BaseException as error:
        result.update(status='FAIL', source_unchanged=False, source_error=repr(error))
    result['finished_at'] = now()
    (base / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result), flush=True)
raise SystemExit(0 if result['status'] == 'PASS' else 1)
