"""爸爸，隔离导出源上的最新nightly诊断例子软件门；不动旧runtime源码或硬件。"""

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import sys
from types import SimpleNamespace

BASE = Path('/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013')
SOURCE = BASE / 'source'
TARGET = Path('/home/loosqk/robot-checks/xlerobot-velocity-foundation-20261008-r7/build')
OUT = BASE / 'quality'
SOURCE_SHA = 'ae00cf4f50d50e96de18a41ac362fc363851f2087afee04b01744bea11b68ee5'
ROOT_EXPORT_HEAD = '67d8b2ad43c47956c05b60bd428679dc91b1dd12'
SHARED_SOURCE = Path('/home/loosqk/robot-checks/position-return-20261010-r1/position-return-r7/source')
EXPECTED_TESTS = [
    'tests::existing_write_preflight_keeps_supported_arguments',
    'tests::firmware_version_preflight_rejects_write_and_preserves_read_before_open',
    'tests::existing_missing_invalid_and_unknown_arguments_remain_for_original_dispatch',
    'tests::diagnostic_write_preflight_denies_even_missing_or_invalid_values',
    'tests::original_do_write_still_rejects_bad_arguments_without_mock_io',
    'tests::read_and_ping_preflight_preserve_existing_dispatch',
    'tests::read_raw_missing_or_partial_reply_has_exit1_and_no_json',
    'tests::read_raw_bad_checksum_has_exit1_and_no_json',
    'tests::original_read_still_sends_single_read_data',
    'tests::read_raw_preflight_rejects_bad_count_register_and_id_with_zero_mock_io',
    'tests::read_raw_wrong_response_id_has_exit1_and_no_json',
    'tests::sync_zero_rejects_extra_arguments_and_invalid_ids_before_mock_io',
    'tests::sync_zero_sends_exact_single_id_packet_without_ack_or_other_register_writes',
    'tests::read_raw_keeps_bit15_words_unsigned_without_signed_or_si_fields',
    'tests::read_raw_success_reports_requested_metadata_and_actual_sync_read_bytes',
]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


a = module('sync_zero_existing_artifacts', Path('/home/loosqk/robot-checks/h2-exit-observation-20261009-r1/source/scripts/post_e_h2_artifacts.py'))
r = module('sync_zero_existing_runner', Path('/home/loosqk/robot-checks/h2-right-ready-20261009-r1/bootstrap/run_release_v2.py'))
assert a.sha256(Path(a.__file__)) == 'bd2b3f7bb8527cd9ecec73894171638e9cf378656b89e284a585c3baa37b8731'
assert a.sha256(Path(r.__file__)) == 'd6b20da28de8d5ae077a93cb2fb5ba7f27a66b8145df9e7e91951cb14ac0b12b'
expected = a.strict_json((BASE / 'boot/host-source-root597.json').read_text())
origin = a.strict_json((BASE / 'boot/root-export-origin.json').read_text())
assert expected['source_manifest_sha256'] == SOURCE_SHA and len(expected['files']) == 597
assert origin['root_export_git_head'] == ROOT_EXPORT_HEAD and origin['native_git_head'] is None
assert not (SOURCE / '.git').exists()
OUT.mkdir()
runner = r.Runner(SimpleNamespace(source=SOURCE, target=TARGET, out=OUT, build_head=ROOT_EXPORT_HEAD), a)
runner.env['CARGO_HOME'] = '/home/loosqk/robot-checks/viz-orin-a6957cc-20260930-r1/cargo-home'
runner.shown['CARGO_HOME'] = runner.env['CARGO_HOME']
runner.env['PATH'] = '/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:' + runner.env['PATH']
runner.shown['PATH'] = runner.env['PATH']
for key in ('ALL_PROXY', 'all_proxy', 'http_proxy', 'https_proxy', 'HTTP_PROXY', 'HTTPS_PROXY', 'LD_PRELOAD'):
    runner.env.pop(key, None)
runner.env.update(HTTP_PROXY='http://10.0.1.80:10808', HTTPS_PROXY='http://10.0.1.80:10808', PYTHONDONTWRITEBYTECODE='1')
runner.shown.update(HTTP_PROXY=runner.env['HTTP_PROXY'], HTTPS_PROXY=runner.env['HTTPS_PROXY'])
result = {'schema': 'XLEROBOT_SYNC_ZERO_NATIVE_SOFTWARE_V1', 'status': 'FAIL', 'started_at': r.utc(),
          'native_git_head': None, 'root_export_git_head': ROOT_EXPORT_HEAD,
          'root_export_dirty_compile_files': ['crates/robot-adapters/feetech/examples/feetech_poke.rs'],
          'source_expected': SOURCE_SHA,
          'scope': '隔离597源：latestnightly→fmt→默认/real-hw根Clippy→例子15→仅例子release→三参数拒绝；不UART/GO/服务/安装'}


def text(name, argv, timeout=7200):
    _, row = runner.command(name, argv, timeout)
    return Path(row['stdout']).read_text()


def source_snapshot(name):
    value = a.snapshot_source(SOURCE, list(expected['files']))
    r.save(OUT / (name + '.json'), value)
    assert value == expected, '隔离导出源发生变化，本轮停止，不回退或修改旧SOURCE。'
    return value


try:
    shared_before = a.snapshot_source(SHARED_SOURCE, list(expected['files']))
    r.save(OUT / 'shared-old-source-before.json', shared_before)
    assert shared_before['source_manifest_sha256'] == '068277d843b191bc8283bab38e667b0f6a4d30e296e8c5a99a97cfe19654fcf8'
    runner.command('nightly_update', ['rustup', 'update', 'nightly', '--no-self-update'], 7200)
    before = source_snapshot('source_before')
    compiler = a.parse_rustc_identity(text('rustc_before', ['rustc', '-vV']))
    assert compiler['host'] == 'aarch64-unknown-linux-gnu'
    result['compiler'] = compiler
    runner.command('fmt', ['cargo', 'fmt', '--all'], 300)
    source_snapshot('fmt_after')
    runner.command('clippy_default', ['cargo', 'clippy', '--all', '--all-targets', '--', '-D', 'warnings'], 7200)
    runner.command('clippy_real_hw', ['cargo', 'clippy', '--all', '--all-targets', '--features', 'real-hw', '--', '-D', 'warnings'], 7200)
    tests = text('example_tests', ['cargo', 'test', '--locked', '-p', 'robot-adapter-feetech', '--example', 'feetech_poke', '--features', 'real-hw'], 7200)
    methods = re.findall(r'^test (tests::\w+) \.\.\. ok$', tests, re.M)
    assert sorted(methods) == sorted(EXPECTED_TESTS) and len(methods) == len(set(methods)) == 15
    assert '15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out' in tests
    build_ref, build_row = runner.command('diagnostic_example_release', ['cargo', 'build', '--locked', '--release', '-p', 'robot-adapter-feetech', '--features', 'real-hw', '--example', 'feetech_poke', '--message-format=json'], 7200)
    records = [a.strict_json(line) for line in Path(build_row['stdout']).read_text().splitlines() if line.strip()]
    finished = [row for row in records if row.get('reason') == 'build-finished']
    assert len(finished) == 1 and finished[0]['success'] is True
    candidates = [row for row in records if row.get('reason') == 'compiler-artifact'
                  and row.get('target', {}).get('kind') == ['example']
                  and row['target'].get('name') == 'feetech_poke' and row.get('executable')]
    assert len(candidates) == 1
    artifact = candidates[0]
    assert 'real-hw' in artifact['features'] and artifact['profile']['opt_level'] == '3' and artifact['profile']['test'] is False
    cargo_elf = Path(artifact['executable'])
    assert cargo_elf == TARGET / 'release/examples/feetech_poke'
    frozen = OUT / 'frozen/feetech_poke'
    frozen.parent.mkdir()
    cargo_identity = a.elf_identity(cargo_elf)
    with cargo_elf.open('rb') as incoming, frozen.open('xb') as outgoing:
        shutil.copyfileobj(incoming, outgoing)
    frozen.chmod(cargo_identity['mode'])
    frozen_identity = a.elf_identity(frozen)
    assert frozen_identity == cargo_identity
    r.save(frozen.parent / 'artifact.json', {'scope': 'ONLY_DIAGNOSTIC_EXAMPLE_NOT_PRODUCTION_CONTROL',
           'source_manifest_sha256': SOURCE_SHA, 'compiler': compiler, 'build_ref': build_ref,
           'compiler_artifact': artifact, 'frozen_path': str(frozen), 'frozen_identity': frozen_identity,
           'cargo_path': str(cargo_elf), 'cargo_identity': cargo_identity,
           'no_ACK_semantics': 'SYNC_WRITE不消费ACK；软件写成功不证明电机接收或实际停止'})
    runner.command('parameter_rejection_gates', [sys.executable, str(BASE / 'boot/verify_parameter_rejections_r1.py')], 300)
    parameters = a.strict_json((BASE / 'parameter-rejections/result.json').read_text())
    assert parameters['status'] == 'PASS' and len(parameters['commands']) == 3
    assert a.elf_identity(frozen) == frozen_identity and a.elf_identity(cargo_elf) == cargo_identity
    after = source_snapshot('source_after')
    compiler_after = a.parse_rustc_identity(text('rustc_after', ['rustc', '-vV']))
    assert after == before and compiler_after == compiler
    shared_after = a.snapshot_source(SHARED_SOURCE, list(expected['files']))
    r.save(OUT / 'shared-old-source-after.json', shared_after)
    assert shared_after == shared_before
    result.update(status='PASS', source_before_after='PASS', compiler_before_after='PASS',
                  shared_old_source_before_after='UNCHANGED_NO_WRITES', example_distinct_methods=15, methods=methods,
                  diagnostic_release_builds=1, production_control_builds=0, full_workspace_tests=0,
                  lib_tests=0, RT_tests=0, H2_runs=0, UART_operations=0, GO_created=False,
                  services_touched=0, formal_installations=0, frozen_ELF=frozen_identity,
                  parameter_rejections=3, no_ACK_semantics='软件证明固定2B帧且不消费ACK；无硬件接收/停止证明')
except BaseException as error:
    result['error'] = repr(error)
finally:
    result.update(finished_at=r.utc(), receipts=runner.receipts)
    r.save(OUT / 'result.json', result)
    print(json.dumps(result, ensure_ascii=False), flush=True)
raise SystemExit(0 if result['status'] == 'PASS' else 1)
