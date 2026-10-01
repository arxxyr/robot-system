from pathlib import Path
import ast
import hashlib
import json
import re
import traceback

ROOT = Path('/Users/loosqk/.codex/worktrees/xlerobot-base-kinematics/robot-system/target')
MAIN = Path('/Users/loosqk/repo/rust/robot-system')
OUT = ROOT / 'b0-diagnostic-archive-review'
BOOT = '6cf7ee3e-a3c5-4c91-80bc-4ca924818b53'
COMMIT = '5c543b0b8c73c7b72bc8284ced4fb22ead15734d'
checks = 0

def need(value, message):
    global checks
    checks += 1
    if not value:
        raise ValueError(message)

def sha(data):
    return hashlib.sha256(data).hexdigest()

def load(path):
    return json.loads(path.read_bytes())

def identity(path):
    data = path.read_bytes()
    return {'sha256': sha(data), 'bytes': len(data)}

def pinned(spec):
    actual = identity(Path(spec['path']))
    need(actual['sha256'] == spec['sha256'], '引用SHA：' + spec['path'])
    if 'bytes' in spec:
        need(actual['bytes'] == spec['bytes'], '引用bytes：' + spec['path'])

def canonical(files):
    return sha(json.dumps(files, sort_keys=True, separators=(',', ':')).encode())

def manifest(value):
    need(len(value['files']) in (589, 590), '清单计数')
    need(canonical(value['files']) == value['source_manifest_sha256'], '清单聚合')

def completion(path, extracted, remote, failure=False):
    row = load(path)
    local = lambda name: extracted / Path(name).relative_to(remote)
    launch = load(local(row['launch_receipt']))
    need(local(row['completion_receipt']) == path, 'completion原路径')
    for key in ('name', 'argv', 'cwd', 'env', 'removed_env_keys', 'started_at', 'started_monotonic_ns',
                'timeout_seconds', 'stdout', 'stderr', 'launch_receipt', 'completion_receipt',
                'pid', 'identity', 'bootstrap_argv', 'expected_exit'):
        need(row.get(key) == launch.get(key), 'launch绑定：' + key)
    need(launch['reaped'] is False and launch['exit_code'] is None and launch['signals'] == [], '原启动状态')
    need('runner_error' not in launch, '启动不带错误')
    pid = row['pid']
    ident = row['identity']
    need(type(pid) is int and pid > 0 and ident['pid'] == ident['sid'] == ident['pgid'] == pid, '真实PID/session')
    need(type(ident['start_ticks']) is int and ident['start_ticks'] > 0 and ident['boot_id'] == BOOT, '启动ticks/boot')
    need(Path(ident['exe']).is_absolute(), '启动exe')
    need(row['reaped'] is True and row['signals'] == [], '自然wait/reap')
    need(row['session_residual_before_cleanup'] == row['session_residual_after_cleanup'] == [], '会话清空')
    need(row['exec_gate_released'] is True, '启动门')
    need(row['elapsed_ns'] == row['ended_monotonic_ns'] - row['started_monotonic_ns'] >= 0, 'elapsed')
    need(row['deadline_monotonic_ns'] == row['started_monotonic_ns'] + row['timeout_seconds'] * 10**9, '原deadline')
    need(not row['budget_exceeded'] and row['ended_monotonic_ns'] <= row['deadline_monotonic_ns'], '原预算')
    need(row['bootstrap_argv'][1] == '-c' and row['bootstrap_argv'][3].isdigit()
         and sha(row['bootstrap_argv'][2].encode()) == row['bootstrap_sha256']
         and row['bootstrap_argv'][4:] == row['argv'], 'bootstrap目标argv')
    need(set(row['env']) == {'RUSTUP_TOOLCHAIN','RUSTC_WRAPPER','CARGO_BUILD_JOBS','CARGO_TERM_COLOR','CARGO_TARGET_DIR','PATH'}, '固定环境')
    need(row['env']['RUSTUP_TOOLCHAIN'] == 'nightly' and row['env']['RUSTC_WRAPPER'] == '', '仅nightly')
    need(row['cwd'] == str(remote / 'source'), '真实cwd')
    if failure:
        need(row['name'] == 'clippy_default' and row['exit_code'] == 101 and row['expected_exit'] == 0,
             '仅保留Clippy101')
        need(row['runner_error'] == "ValueError('命令原始退出码非预期')", '保留真实非零错误')
    else:
        need('runner_error' not in row and row['exit_code'] == row.get('expected_exit', 0), '成功预期退出')
    for stream in ('stdout', 'stderr'):
        data = local(row[stream]).read_bytes()
        need(sha(data) == row[stream + '_sha256'] and len(data) == row[stream + '_bytes'], '真实日志SHA/bytes')
    return row

def main():
    summary_path = ROOT / 'b0-diagnostic-orin-r3/summary.json'
    summary = load(summary_path)
    need(identity(summary_path) == {'sha256':'2085292296e00a2ac872fcc547829bbeca1ca0b445dbd1d9679b4d3b3300c6fe','bytes':174662}, '固定summary')
    need(summary['status'] == 'PASS_AFTER_TWO_PRESERVED_FAILURES', '总状态')
    rounds = []
    sources = []
    total_direct = total_host = 0
    for item in summary['iterations']:
        n = item['native_iteration']
        directory = ROOT / f'b0-diagnostic-orin-r{n}'
        extracted = directory / 'native-extracted'
        remote = Path(f'/home/loosqk/robot-checks/feetech-diagnostic-b0-20260930-r{n}')
        qa = extracted / 'qa-r1'
        result = load(qa / 'result.json')
        pinned(item['qa_result'])
        need(result['status'] == ('PASS' if n == 3 else 'FAIL'), '原QA状态')
        need(result['native_git_head'] == '1c5758f97b7c502a38c850e8352ce970216bea53', 'native原HEAD')
        before = load(qa / 'source_before.json')
        manifest(before)
        sources.append(before)
        need(before == load(qa / 'source_after.json') == load(qa / 'source_after_fmt.json'), '原源首尾/fmt同源')
        need(before['source_manifest_sha256'] == item['aggregate'] and len(before['files']) == 590, '每轮590')
        for path in qa.glob('*_source_*.json'):
            need(load(path) == before, '逐gate源：' + path.name)
        need(load(qa / 'before_originals.json') == load(qa / 'after_originals.json'), '旧H2/数学/包前后不变')
        preserved = load(qa / 'before_originals.json')['source']
        manifest(preserved)
        need(preserved['source_manifest_sha256'] == '83048e983bb7a13c2f75ae23e593ec301f129b10b412db8f4f80176ca434f62f', '旧H2源不复绑')
        snapshot = extracted / 'source-inputs-r1'
        sr = load(snapshot / 'result.json')
        need(sr['status'] == 'PASS' and sr['files'] == 590 and sr['aggregate'] == item['aggregate'], '副本结果')
        need(load(snapshot / 'source_before.json') == load(snapshot / 'source_after.json') == before, '副本首尾')
        for name, spec in before['files'].items():
            copied = snapshot / 'files' / name
            need(identity(copied) == {k:spec[k] for k in ('sha256','bytes')}, '590逐件副本')
            need(bool(copied.stat().st_mode & 0o111) == spec['executable'], '副本exec位')
        rows = []
        for phase in ('qa-r1','source-inputs-r1','diagnostic-r1'):
            for path in sorted((extracted / phase).glob('*/completion.json')):
                rows.append(completion(path, extracted, remote, n < 3 and path.parent.name == 'clippy_default'))
        need(len(rows) == item['direct_count'] == (41 if n == 3 else 28), '自然child计数')
        indexed = {r['pid']:r for r in rows}
        need(len(indexed) == len(rows), 'PID不重复')
        for entry in item['direct_children']:
            actual = indexed[entry['pid']]
            for key, value in entry.items():
                need(actual.get(key, 0 if key == 'expected_exit' else None) == value, 'summary/direct：' + key)
        for phase in ('qa-r1','source-inputs-r1','diagnostic-r1'):
            path = extracted / phase / 'result.json'
            if path.exists():
                for reference in load(path).get('receipts', []):
                    local = extracted / Path(reference['path']).relative_to(remote)
                    need(identity(local)['sha256'] == reference['sha256'], '原receipt固定SHA')
        actual_by_name = {r['name']:r for r in rows if r['completion_receipt'].startswith(str(remote / 'qa-r1'))}
        need(actual_by_name['latest_nightly_update']['argv'] == ['rustup','update','nightly','--no-self-update'], '实际update')
        for name in ('rustc_before_update','rustc_after_update'):
            raw = (qa / name / 'stdout.log').read_text()
            need('release: 1.101.0-nightly' in raw and 'commit-hash: ' + COMMIT in raw and 'host: aarch64-unknown-linux-gnu' in raw, '真实编译器')
        need((qa/'cargo_before_update/stdout.log').read_bytes() == (qa/'cargo_after_update/stdout.log').read_bytes(), 'Cargo更新前后')
        need(result['compiler']['commit_hash'] == result['compiler_before_update']['commit_hash'] == COMMIT, 'compiler绑定')
        if n < 3:
            need(set(actual_by_name).isdisjoint({'adapter_lib','example_tests','release_example','clippy_real_hw'}), '失败后未测试')
            stderr = (qa / 'clippy_default/stderr.log').read_text()
            need(('E0004' in stderr and 'DiagnosticRegisterWriteForbidden' in stderr) if n == 1 else
                 ('needless_pass_by_value' in stderr and 'assert_is_empty' in stderr), '原失败类别')
            need(load(extracted/'diagnostic-r1/result.json')['status'] == 'PASS', '失败后仅编译器取证')
        else:
            need(actual_by_name['fmt']['argv'] == ['cargo','fmt','--all'], 'fmt完整命令')
            for name, args in [('clippy_default', []),('clippy_real_hw',['--features','robot-adapter-feetech/real-hw'])]:
                need(actual_by_name[name]['argv'] == ['cargo','clippy','--locked','--all','--all-targets',*args,'--','-D','warnings'], '两根Clippy完整命令')
            need(actual_by_name['fmt']['finished_at'] < actual_by_name['clippy_default']['started_at'] < actual_by_name['clippy_real_hw']['started_at'], '质量顺序')
            expected_tests = load(extracted/'bootstrap/expected_tests.json')
            for name, count, expected in [('adapter_lib',160,expected_tests['library_new_subset']),('example_tests',5,expected_tests['example_all'])]:
                text = (qa/name/'stdout.log').read_text()
                names = re.findall(r'^test ([^ ]+) \.\.\. ok$', text, re.M)
                need(len(names) == len(set(names)) == count and f'test result: ok. {count} passed; 0 failed; 0 ignored;' in text, '真实测试结果')
                need(set(expected) <= set(names), '18子集名字')
                need('--test-threads' not in ' '.join(actual_by_name[name]['argv']), '默认并行')
            need(len(expected_tests['library_new_subset']) + len(expected_tests['example_all']) == 18, '18不同方法')
            need('adapter::diagnostic::tests::typed_classes_keep_literal_diagnostic_codes' in result['tests']['adapter_lib']['names'], '既有方法三literal实际测试')
            cargo = [json.loads(line) for line in (qa/'release_example/stdout.log').read_text().splitlines() if line.strip()]
            need(sum(v.get('reason') == 'build-finished' and v['success'] is True for v in cargo) == 1, '唯一build-finished')
            artifacts = [v for v in cargo if v.get('reason') == 'compiler-artifact' and v.get('executable')]
            need(len(artifacts) == 1 and artifacts[0] == result['read_tool']['compiler_artifact'], '唯一CargoJSON executable')
            a = artifacts[0]
            need(a['target']['name'] == 'feetech_poke' and a['target']['kind'] == ['example'] and a['features'] == ['default','real-hw'], '正确target/features')
            need(a['profile']['opt_level'] == '3' and not a['profile']['test'] and not a['profile']['debug_assertions'], '优化构建')
            elf = extracted / 'read-tool-r1/feetech_poke'
            binary = elf.read_bytes()
            need(identity(elf) == {'sha256':'2daabe75878989abfa759920ec07773d3c97b017042a42b3906483c3da4f5a71','bytes':1470368}, '冻结ELF')
            need(binary[:4] == b'\x7fELF' and int.from_bytes(binary[18:20],'little') == 183, 'ELF183')
            need(load(extracted/'read-tool-r1/feetech_poke.build.json') == result['read_tool'], 'build sidecar原文')
            smoke = load(extracted/'bootstrap/smoke_allowlist.json')
            need(len(smoke['commands']) == 9 and smoke['expected_exit'] == 2, '固定9allowlist')
            for name, argv in smoke['commands'].items():
                row = actual_by_name[name]
                need(row['argv'] == argv and row['expected_exit'] == row['exit_code'] == 2, '实际9拒写2')
                need(argv[:4] == [str(remote/'read-tool-r1/feetech_poke'),str(remote/'qa-r1/never-open-device'),'1','write'], '固定NX/ELF/ID1/write')
                need(argv[4] in ('Phase','CW_Dead_Zone','CCW_Dead_Zone'), '只有三诊断项')
                need((qa/name/'stdout.log').read_bytes() == b'' and (qa/name/'stderr.log').read_text() == 'error: 本项目诊断专用寄存器禁止写入：'+argv[4]+'\n', '精确typed stderr')
            need(not (qa/'never-open-device').exists(), 'NX不存在')
            need((qa/'rustc_after_update/stdout.log').read_bytes() == (qa/'rustc_after/stdout.log').read_bytes(), '编译器门后同源')
            need((qa/'cargo_after_update/stdout.log').read_bytes() == (qa/'cargo_after/stdout.log').read_bytes(), 'Cargo门后')
            need(result['tests']['distinct_total'] == 165 and result['smoke_cases'] == 9, '计数不累计')
        for owner_spec in item['host_owners']:
            pinned(owner_spec)
            owner = load(Path(owner_spec['path']))
            need(owner['pid'] == owner_spec['pid'] and owner['command'] == owner_spec['command'], '本机owned PID/argv')
            need(owner['phase'] == 'REAPED' and owner['reaped'] and not owner['owner_sent_signals'] and not owner['errors'], '本机wait/reap')
            expected_exit = 1 if n < 3 and Path(owner_spec['path']).parent.name == 'checks.owner' else 0
            need(owner['wait_returncode'] == owner_spec['wait_returncode'] == expected_exit, '本机原wait退出')
            pinned(owner['log'])
            if 'input' in owner:
                pinned(owner['input'])
            total_host += 1
        total_direct += len(rows)
        rounds.append({'round':n,'status':result['status'],'aggregate':item['aggregate'],'direct_children':len(rows),'host_owners':item['host_owner_count']})
    need(total_direct == 97 and total_host == 26, '97+26真实回收')
    need({k for k in sources[0]['files'] if sources[0]['files'][k] != sources[1]['files'][k]} == {'crates/robot-adapters/feetech/src/diagnostic.rs'}, 'r2仅分类补丁')
    changed = {'crates/robot-adapters/feetech/src/bus.rs','crates/robot-adapters/feetech/examples/feetech_poke.rs'}
    need({k for k in sources[1]['files'] if sources[1]['files'][k] != sources[2]['files'][k]} == changed, 'r4仅两test源')
    for name in changed:
        a = ROOT/'b0-diagnostic-orin-r2/native-extracted/source-inputs-r1/files'/name
        b = ROOT/'b0-diagnostic-orin-r3/native-extracted/source-inputs-r1/files'/name
        need(a.read_bytes().split(b'#[cfg(test)]',1)[0] == b.read_bytes().split(b'#[cfg(test)]',1)[0], '生产prefix逐字保持')
    for name, spec in sources[2]['files'].items():
        current = MAIN / name
        need(identity(current) == {k:spec[k] for k in ('sha256','bytes')} and bool(current.stat().st_mode & 0o111) == spec['executable'], 'main590绑定')
    for spec in summary['previous_H2_local_archives_unchanged'] + summary['previous_math_local_archives']:
        pinned(spec)
    return {'schema':'B0_INDEPENDENT_DYNAMIC_REVIEW_V1','status':'PASS','checks':checks,'summary':identity(summary_path),'source_files':590,'final_aggregate':summary['final_aggregate'],'rounds':rounds,'naturally_reaped':{'direct':97,'host':26,'preserved_direct_exit101':2,'compiled_expected_exit2':9},'tests':'165不同方法=160库+5example；18新方法为子集，既有literal方法补三案例，不计新方法。','release_ELF':{'sha256':'2daabe75878989abfa759920ec07773d3c97b017042a42b3906483c3da4f5a71','bytes':1470368,'machine':183},'boundary':'纯软件；不代表H2、全workspace、Phase现场合同、头/底盘动作或S0/S1速度接入；人工断电PENDING，旧硬件FAIL/D15 OPEN_DEFERRED/deployable=false不变。','current_formal_site_claim':'本轮未执行新正式状态探针；仅软件检查范围未改正式安装/无UART，不能声明现场最近已确认。','actions_this_review':{'Cargo':False,'SSH':False,'UART':False,'hardware':False,'source_modified':False}}

try:
    report = main()
except BaseException as error:
    report = {'schema':'B0_INDEPENDENT_DYNAMIC_REVIEW_V1','status':'FAIL_REVIEWER_OR_EVIDENCE_MISMATCH','checks':checks,'error':repr(error),'traceback':traceback.format_exc(),'source_and_originals_modified':False}
path = OUT/'independent_dynamic_review.json'
if path.exists():
    path = OUT/'independent_dynamic_review.next.json'
path.write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n')
print(json.dumps({'path':str(path),'status':report['status'],**identity(path)},ensure_ascii=False))
if report['status'] != 'PASS':
    print(report['traceback'])
    raise SystemExit(1)
