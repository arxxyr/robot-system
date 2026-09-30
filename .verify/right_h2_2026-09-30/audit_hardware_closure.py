import sys
sys.dont_write_bytecode = True
import csv
import hashlib
import importlib.util
import io
import json
from pathlib import Path, PurePosixPath
import subprocess
import tarfile

ROOT = Path('/Users/loosqk/repo/rust/robot-system')
BASE = ROOT / '.verify/right_h2_2026-09-30'
REPORTS = ROOT / 'target/post_e_h2_rt/s52/reviews'

def digest(data):
    return hashlib.sha256(data).hexdigest()

summary_data = (BASE / 'summary.json').read_bytes()
summary = json.loads(summary_data)
blobs, archive_proofs = {}, {}
for label, manifest_name, expected_count in [('native', 'manifest.json', 123), ('host', 'host_manifest.json', 408)]:
    manifest_data = (BASE / manifest_name).read_bytes()
    manifest = json.loads(manifest_data)
    indexed = {row['path']: row for row in manifest['files']}
    assert len(indexed) == len(manifest['files']) == expected_count
    archive_data = (BASE / f'{label}.tar.gz').read_bytes()
    assert digest(archive_data) == manifest['archive']['sha256'] == summary[f'{label}_archive']['sha256']
    assert len(archive_data) == manifest['archive']['bytes'] == summary[f'{label}_archive']['bytes']
    total, seen = 0, set()
    blobs[label] = {}
    with tarfile.open(BASE / f'{label}.tar.gz', 'r:gz') as archive:
        members = archive.getmembers()
        assert len(members) == expected_count
        for member in members:
            path = PurePosixPath(member.name)
            assert not path.is_absolute() and '..' not in path.parts and str(path) == member.name
            assert not member.name.startswith('./') and member.name not in seen
            assert member.isfile() and not member.islnk() and not member.issym() and not member.sparse
            seen.add(member.name)
            data = archive.extractfile(member).read()
            entry = indexed[member.name]
            assert len(data) == member.size == entry['bytes'] and digest(data) == entry['sha256'], member.name
            total += len(data)
            blobs[label][member.name] = data
    assert seen == set(indexed) and total == manifest['input_bytes'] == summary[f'{label}_archive']['input_bytes']
    archive_proofs[label] = {'sha256': digest(archive_data), 'bytes': len(archive_data), 'manifest_sha256': digest(manifest_data), 'files': expected_count, 'input_bytes': total, 'safe_unique_regular_members': True, 'all_hashes_and_bytes_verified': True}

references = []
def walk(value):
    if isinstance(value, dict):
        if all(key in value for key in ['archive', 'path', 'sha256']):
            label = value['archive'].removesuffix('.tar.gz')
            data = blobs[label][value['path']]
            assert digest(data) == value['sha256']
            if 'bytes' in value:
                assert len(data) == value['bytes']
            references.append({'archive': value['archive'], 'path': value['path'], 'sha256': value['sha256'], 'bytes': len(data)})
        for child in value.values():
            walk(child)
    elif isinstance(value, list):
        for child in value:
            walk(child)
walk(summary)
assert digest((ROOT / '.verify/h2_serial_recovery_2026-09-30/summary.json').read_bytes()) == summary['software_preparation']['sha256']

def native(name):
    return json.loads(blobs['native'][name])
def host(name):
    return json.loads(blobs['host'][name])

run = native('right-h2-run-01/run.json')
tool_report = native('right-h2-run-01/report.json')
tool_evidence = tool_report['cases'][0]['evidence']
assert run['status'] == 'FAIL' and run['authorization_attempts'] == run['launch_attempts'] == 1
assert run['supervisor_pid'] == 423458 and run['tool_pid'] == 423513 and run['tool_exit'] == 0 and run['tool_naturally_reaped'] is True
assert run['failures'] == summary['hardware_result']['failures'] and run['deployable'] is False
assert run['expected_head'] == summary['runtime_head'] == '4a8e171f27bf18625973aa4f3a6097e9b9fbed18'
assert run['cleanup']['clean'] and run['cleanup']['related'] == run['cleanup']['unreadable'] == []
assert tool_evidence['measurement']['hardware_only'] is True and tool_report['status'] == 'PASS'
physical = native('bootstrap/s52-right-physical-r1.json')
go = native('bootstrap/s52-right-go-r1.json')
assert run['physical'] == physical
assert run['inputs_sha256'] == digest(blobs['native']['bootstrap/s52-right-inputs-r1.json'])
assert all(physical[key] is True for key in ['supported', 'unloaded', 'area_clear', 'motor_power_cycle', 'usb_replug', 'continuous_presence_until_cleanup', 'immediate_power_cut'])
assert go['expected_head'] == run['expected_head'] and go['output'] == physical['output']
assert go['signed_at_ns'] <= 1790791167218613516 <= go['expires_at_ns']
assert go['inputs_sha256'] == run['inputs_sha256'] and go['physical_sha256'] == digest(blobs['native']['bootstrap/s52-right-physical-r1.json'])

attempts = [json.loads(line) for line in blobs['native']['right-h2-run-01/orin_sla_right/attempts.jsonl'].decode().splitlines()]
assert len(attempts) == 4401 and sum(row['matched'] is True for row in attempts) == 4400
assert [row for row in attempts if row['matched'] is False] == summary['measurement']['mismatched_attempt']
for group, identity in zip(summary['measurement']['groups'], [(False, False), (False, True), (True, False), (True, True)]):
    value = native(group['evidence']['path'])
    assert value == group['result'] and value['passed'] is True
    rows = [row for row in attempts if (row['external'], row['requested_in_flight']) == identity]
    accepted = [row for row in rows if row['matched']]
    assert len(accepted) == 1100 and all(row['warmup'] for row in accepted[:100]) and not any(row['warmup'] for row in accepted[100:])
    for key, budget in [('sla_a_ns', 2_000_000), ('sla_b_ns', 75_000_000 if identity[1] else 20_000_000)]:
        values = sorted((row['t1_ns'] - row['t0_ns']) if key == 'sla_a_ns' else (row['all_off_ns'] - row['t0_ns']) for row in accepted[100:])
        assert value[key]['count'] == 1000 and value[key]['p99'] == values[989] and value[key]['max'] == values[-1] and values[989] <= budget
    assert value['class_mismatch_retries'] == len(rows) - 1100

spec = importlib.util.spec_from_file_location('hardware_report_readonly', ROOT / 'scripts/post_e_h2_report.py')
mod = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = mod
spec.loader.exec_module(mod)
boundaries = [json.loads(line) for line in blobs['native']['right-h2-run-01/orin_sla_right/boundary.jsonl'].decode().splitlines()]
final_stop = native('right-h2-run-01/orin_sla_right/final_stop.json')
final_trace = native('right-h2-run-01/orin_sla_right/final_trace.json')
mod.verify_final_stop(final_stop, boundaries)
mod.verify_final_trace(final_trace, boundaries)
assert summary['final_stop']['value'] == final_stop and summary['final_stop']['boundary_trace'] == final_trace

cleanup = native('right-h2-cleanup-r1/result.json')
postread = native('right-h2-postread-r1/result.json')
for value in [cleanup, postread]:
    assert value['status'] == value['driver_status'] == 'FAIL' and value['readonly_cleanup_status'] == 'PASS'
    assert value['read_only'] is True and value['driver_evidence_status'] == 'PASS'
    assert len(value['checks']) == 11 and all(check['status'] == 'PASS' for check in value['checks'])
    assert value['driver_run_sha256'] == digest(blobs['native']['right-h2-run-01/run.json'])

reader = native('register-readback-r1/result.json')
plan = native('bootstrap/s52-readback-plan-r1.json')
assert reader['status'] == 'PASS' and reader['completed'] == 34
assert reader['original_h2_status'] == 'FAIL' and reader['original_h2_unchanged'] is True
assert reader['received_signals'] == [] and reader['errors'] == [] and len(reader['steps']) == len(plan['steps']) == 34 and len(reader['commands']) == 40
assert reader['plan_sha256'] == digest(blobs['native']['bootstrap/s52-readback-plan-r1.json'])
assert reader['binary']['sha256'] == plan['binary_sha256'] == 'f2014ee4008311e7a8725235a39f0d1b3913391f17cbd977b7441f120539114b'
for number, (step, planned) in enumerate(zip(reader['steps'], plan['steps']), 1):
    for key in ['side', 'motor_id', 'register', 'argv', 'expected_stdout', 'expected_exit', 'subsystem']:
        assert step[key] == planned[key], (number, key)
    assert step['index'] == number and step['status'] == 'PASS' and step['exit_code'] == 0 and step['reaped'] is True
    assert step['argv'][-2] == 'read' and step['expected_stdout'].endswith(' = 0')
    command = next(row for row in reader['commands'] if row['name'] == f'read-{number:02}')
    assert command['pid'] == step['pid'] and command['argv'] == step['argv'] and command['exit_code'] == 0
    stdout = blobs['native'][f'register-readback-r1/read-{number:02}/stdout.log']
    stderr = blobs['native'][f'register-readback-r1/read-{number:02}/stderr.log']
    assert stdout.decode().strip() == step['expected_stdout'] and stderr == b''
    assert digest(stdout) == step['stdout_sha256'] == command['stdout_sha256']
    assert digest(stderr) == step['stderr_sha256'] == command['stderr_sha256']
for number, command in enumerate(reader['commands']):
    assert command['reaped'] is True and command['status'] == 'REAPED' and not command['signals_sent'] and command['child_end_class'] == 'EXIT_STATUS'
    if number:
        assert reader['commands'][number-1]['finished_ns'] <= command['started_ns']
    if 'fuser' in command['name']:
        assert command['exit_code'] == 1 and command['stdout'] == command['stderr'] == ''
    else:
        assert command['exit_code'] == 0
assert reader['driver_run_sha256'] == digest(blobs['native']['right-h2-run-01/run.json'])
assert reader['cleanup_sha256'] == digest(blobs['native']['right-h2-cleanup-r1/result.json'])
assert reader['ssh_wait_sha256'] == digest(blobs['host']['host/right_h2_execute_once.owner/result.json'])

guard = host('host/right_h2_final_bus_guard_once.owner/child.log')
assert guard['status'] == 'PASS' and guard['read_only'] is True and len(guard['checks']) == 16 and all(check['status'] == 'PASS' for check in guard['checks'])
assert guard['finished_ns'] == summary['final_both_bus_guard']['finished_ns'] and guard['started_ns'] >= postread['finished_ns']
for check in guard['checks']:
    if '_holder_' in check['name']:
        assert check['value']['exit_code'] == 1 and check['value']['stdout'] == check['value']['stderr'] == ''
    if check['name'] == 'processes':
        assert check['value']['clean'] and check['value']['related'] == check['value']['unreadable'] == []

owner_proofs = []
for name, reference in summary['process_ownership'].items():
    owner = host(reference['path'])
    assert owner['phase'] == 'REAPED' and owner['reaped'] is True and owner['wait_returncode'] == reference['wait_returncode']
    assert owner['owner_sent_signals'] == [] and owner['received_signals'] == {} and owner['errors'] == [] and owner['child_end_class'] == 'EXIT_STATUS'
    data = blobs['host'][reference['path'].rsplit('/', 1)[0] + '/child.log']
    assert len(data) == owner['log']['bytes'] and digest(data) == owner['log']['sha256']
    owner_proofs.append({'name': name, 'pid': owner['pid'], 'wait_returncode': owner['wait_returncode'], 'reaped': True, 'finished_utc': owner['finished_utc']})
assert summary['D15'] == 'OPEN/DEFERRED' and summary['deployable'] is False and summary['watch_released'] is True and summary['physical_state_after_watch_release'] == 'UNKNOWN'
assert summary['head_base_active_motion'] == 'NOT_TESTED' and summary['historical_34_unknown_root_cause'] == 'OPEN_UNCONFIRMED' and summary['push_performed'] is False
assert digest(blobs['host']['reviews/h2_trace_attempt_diagnosis.json']) == summary['diagnosis']['sha256']

docs = {}
for name in ['CLAUDE.md', 'docs/10_verification.md', 'docs/CHANGELOG.md', 'team-swe/specs/post_e_comm_fault_hardening.HANDOFF.md']:
    data = (ROOT / name).read_bytes()
    docs[name] = {'sha256': digest(data), 'bytes': len(data)}
assert subprocess.run(['git', 'diff', '--check'], cwd=ROOT, capture_output=True).returncode == 0
assert set(subprocess.check_output(['git', 'diff', '--name-only'], cwd=ROOT, text=True).splitlines()) == set(docs)

result = {'schema': 'P7_H2_RIGHT_INDEPENDENT_CLOSURE_REVIEW_V1', 'status': 'PASS', 'accepted_scope': '真实 H2 FAIL 完整归档及受监督运行后的独立只读收尾；不是 Hardware PASS', 'overall_h2_status': 'FAIL', 'blocking_findings': [], 'summary_sha256_at_review': digest(summary_data), 'runtime_head': summary['runtime_head'], 'fixed_build_head': summary['build_head'], 'source_aggregate': summary['source_manifest_sha256'], 'archives': archive_proofs, 'summary_artifact_references_checked': len(references), 'owner_proofs': owner_proofs, 'measurement': {'four_groups': 4, 'valid_warmup_each': 100, 'valid_official_each': 1000, 'attempts': 4401, 'classification_mismatch': 1, 'original_p99_budgets_unchanged': True, 'original_trace_gate_rejections': 3}, 'closure': {'final_disable_and_disarm_contract': 'PASS', 'independent_cleanup_checks': 11, 'register_steps': 34, 'register_commands_naturally_reaped': 40, 'postread_cleanup_checks': 11, 'final_both_bus_checks': 16, 'head_base_active_motion': 'NOT_TESTED'}, 'docs': docs, 'tracked_diff': '仅四份阶段文档新增38行，git diff --check通过，无生产源码改动', 'diagnosis_sha256': summary['diagnosis']['sha256'], 'limits': ['仅本机读取原件与已冻结前置证据，未SSH/UART/硬件重跑。', '清场原整体 status/SSH exit1保留 driver FAIL；readonly_cleanup_status和各11checks为PASS。', '17电机零使能/零速度仅支持34次读取时刻，不证明连续静止或硬件fail-safe。', '三条标签合同错误不回填原trace或整体PASS；具体抢占点未证实。', '值守解除后物理状态UNKNOWN；D15 OPEN/DEFERRED、deployable=false，不安装/部署/push。', 'summary的review/root_acceptance当前PENDING，root可依据本报告更新验收字段，其余绑定保持。']}
out = REPORTS / 'hardware_archive_and_closure_review.json'
assert not out.exists()
data = (json.dumps(result, ensure_ascii=False, indent=2) + '\n').encode()
out.write_bytes(data)
print(json.dumps({'status': 'PASS', 'report': str(out), 'sha256': digest(data), 'bytes': len(data), 'archive_files': {key: value['files'] for key, value in archive_proofs.items()}, 'owner_proofs': len(owner_proofs), 'summary_refs': len(references)}, ensure_ascii=False))
