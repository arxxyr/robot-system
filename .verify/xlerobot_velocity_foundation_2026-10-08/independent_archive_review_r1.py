import difflib
import hashlib
import json
from pathlib import Path, PurePosixPath
import stat
import tarfile

ROOT = Path('/Users/loosqk/repo/rust/robot-system')
BASE = Path(__file__).resolve().parent
REPORT = {'schema': 'INDEPENDENT_FINAL_ARCHIVE_REVIEW_R1', 'scope': '只读逐包成员与引用闭合独核；文档为当前待root最终验收版本；无Cargo/SSH/UART/源码或原件改写', 'checks': [], 'closed_typed_references': [], 'identity_only_unarchived_runtime_files': []}
def ref(p):
    raw = Path(p).read_bytes()
    return {'path': str(p), 'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}
def load(p): return json.loads(Path(p).read_text())
def check(label, value, details=None): REPORT['checks'].append({'name': label, 'status': 'PASS' if value else 'FAIL', 'details': details})
def must(value, label):
    if not value: raise ValueError(label)
def safe(name):
    p = PurePosixPath(name)
    return bool(name) and not p.is_absolute() and '..' not in p.parts and str(p) == name

summary = load(BASE / 'summary.json')
original = load(BASE / 'orin-original-summary.json')
verify = load(BASE / 'archive-verification.json')
inputs = load(BASE / 'input-identities.json')
index_raw = (BASE / 'local-evidence.index.json').read_bytes()
index = json.loads(index_raw)
local = {}
seen = set()
errors = []
try:
    with tarfile.open(BASE / 'local-evidence.tar.gz', 'r:gz') as archive:
        for m in archive:
            must(safe(m.name) and m.name not in seen and m.isfile() and not m.issym() and not m.islnk(), '本机成员越界/重复/非普通')
            seen.add(m.name)
            raw = archive.extractfile(m).read(m.size + 1)
            if m.name == 'local-evidence.index.json':
                must(raw == index_raw and m.mode == 0o644, '本机包内外自索引不同')
            else:
                must(m.name in index['files'], '本机包未索引成员')
                v = index['files'][m.name]
                must(len(raw) == m.size == v['bytes'] and hashlib.sha256(raw).hexdigest() == v['sha256'] and (m.mode, m.uid, m.gid) == (v['mode'], v['uid'], v['gid']), '本机成员SHA/bytes/mode/数字属主不同')
                local[m.name] = raw
    must(seen == set(index['files']) | {'local-evidence.index.json'}, '本机成员集合不闭合')
except (ValueError, KeyError, OSError) as error:
    errors.append(repr(error))
check('620本机原件/621普通成员/自索引与安全闭合集合', not errors and len(index['files']) == len(local) == 620 and len(seen) == 621 and 'local-evidence.index.json' not in index['files'], {'errors': errors, 'indexed': len(index['files']), 'members': len(seen)})
check('648封包输入前后身份真实个数', inputs['all_fixed_inputs_unchanged'] is True and inputs['before'] == inputs['after'] and len(inputs['before']) == inputs['fixed_input_count'] == verify['fixed_input_count'] == 648 and set(inputs['before']) == set(index['files']) | set(summary['originals']['native_packages']) and len(summary['originals']['native_packages']) == 28)
check('自索引及之后附件不制造循环引用', not any(n.endswith('local-evidence.index.json') for n in index['files']) and not any(n.startswith('independent_') for n in index['files']) and 'summary.json' not in inputs['before'] and 'input-identities.json' not in inputs['before'] and 'archive-verification.json' not in inputs['before'])
for name, v in index['files'].items():
    check('local固定输入内容/' + name, all(inputs['before'][name][k] == v[k] for k in ('sha256', 'bytes', 'mode', 'uid', 'gid')))

native_index = {}
native_content = {}
for n in range(1, 8):
    folder = BASE / f'native/r{n}'
    raw_index = (folder / 'evidence.index.json').read_bytes()
    ix = json.loads(raw_index)
    metadata = load(folder / 'evidence.metadata.json')
    files = {v['path']: v for v in ix['files']}
    dirs = {v['path']: v for v in ix['directories']}
    names = set()
    problems = []
    content = {}
    try:
        must(len(files) == len(ix['files']) and len(dirs) == len(ix['directories']) and not (set(files) & set(dirs)), 'native index集合冲突')
        must(ref(folder / 'evidence.tar.gz')['sha256'] == metadata['archive_sha256'] and (folder / 'evidence.tar.gz').stat().st_size == metadata['archive_bytes'] and hashlib.sha256(raw_index).hexdigest() == metadata['index_sha256'], 'native包/index/原metadata身份不同')
        with tarfile.open(folder / 'evidence.tar.gz', 'r:gz') as archive:
            for m in archive:
                must(safe(m.name) and m.name not in names, 'native成员越界/重复')
                names.add(m.name)
                if m.name in dirs:
                    must(m.isdir() and m.mode == dirs[m.name]['mode'], 'native目录类型/mode不同')
                    continue
                must(m.isfile() and not m.issym() and not m.islnk(), 'native非普通文件')
                raw = archive.extractfile(m).read(m.size + 1)
                if m.name == 'archive-index.json':
                    must(raw == raw_index and m.mode == 0o644, 'native自索引不同')
                else:
                    must(m.name in files, 'native未索引文件')
                    v = files[m.name]
                    must(len(raw) == m.size == v['bytes'] and hashlib.sha256(raw).hexdigest() == v['sha256'] and (m.mode, m.uid, m.gid) == (v['mode'], v['uid'], v['gid']), 'native成员身份不同')
                    if not m.name.startswith('source/'):
                        content[m.name] = raw
        must(names == set(files) | set(dirs) | {'archive-index.json'}, 'native成员集合未闭合')
    except (ValueError, KeyError, OSError) as error:
        problems.append(repr(error))
    native_index[n] = files
    native_content[n] = content
    check('归档复制native逐成员安全及原身份/' + str(n), not problems, {'files': len(files), 'members': len(names), 'errors': problems})
    for target, prior in [('evidence.tar.gz', 'native-evidence.tar.gz'), ('evidence.index.json', 'native-evidence.index.json'), ('evidence.metadata.json', 'native-evidence.metadata.json'), ('archive-verification.json', 'native_archive_verification.json')]:
        current = ref(folder / target)
        previous = ref(ROOT / 'target/xlerobot_velocity_foundation' / f'native-r{n}' / prior)
        bound = summary['originals']['native_packages'][f'native/r{n}/{target}']
        check('native原包逐字复制/' + str(n) + '/' + target, all(current[k] == previous[k] == bound[k] for k in ('sha256', 'bytes')))

source_mapping = {v['source_path']: k for k, v in inputs['before'].items()}
def resolve(path):
    if path in source_mapping:
        member = source_mapping[path]
        if member in local:
            return 'local-evidence.tar.gz -> ' + member, local[member]
        file = BASE / member
        if file.is_file():
            return member, file.read_bytes()
    for n in range(1, 8):
        local_marker = f'/target/xlerobot_velocity_foundation/native-r{n}/native-extracted/'
        remote_marker = f"/home/loosqk/robot-checks/xlerobot-velocity-foundation-{'20261008' if n == 7 else '20261007' if n >= 3 else '20261001'}-r{n}/"
        part = path.split(local_marker, 1)[1] if local_marker in path else path.removeprefix(remote_marker) if path.startswith(remote_marker) else None
        if part is not None:
            if part in native_index[n]:
                return f'native/r{n}/evidence.tar.gz -> ' + part, native_content[n].get(part)
            if n == 2 and part.startswith('diagnostic-old-rr-r1/'):
                member = 'stage/diagnostic-old-rr-r1/remote-originals/' + part
                if member in local:
                    return 'local-evidence.tar.gz -> ' + member, local[member]
    return None, None

typed = []
def walk(value, label):
    if isinstance(value, dict):
        if 'path' in value and 'sha256' in value and isinstance(value['path'], str):
            typed.append((label, value))
        for key, item in value.items(): walk(item, label + '/' + key)
    elif isinstance(value, list):
        for number, item in enumerate(value): walk(item, label + '/' + str(number))
walk(original, 'orin-original-summary')
for label, item in typed:
    location, raw = resolve(item['path'])
    if label == 'orin-original-summary/separate_diagnostic/elf_before':
        must(location is None and raw is None, '旧ELF未封存边界发生变化，必须新记录')
        REPORT['identity_only_unarchived_runtime_files'].append({'label': label, 'observed_identity': item, 'status': 'IDENTITY_ONLY_ELF_BYTES_NOT_ARCHIVED', 'reason': '原ELF只作为一次历史执行身份描述，构建缓存未封存；不宣称其88MB字节归档或由一次观察关闭旧超时根因'})
        continue
    good = location is not None and raw is not None and hashlib.sha256(raw).hexdigest() == item['sha256'] and ('bytes' not in item or len(raw) == item['bytes'])
    check('原摘要typed引用包内SHA/bytes闭合/' + label, good, {'source_path': item['path'], 'archive_member': location})
    if good:
        REPORT['closed_typed_references'].append({'label': label, 'source_path': item['path'], 'archive_member': location, 'sha256': item['sha256'], 'bytes': len(raw)})
check('367typed引用实际366闭合加1明确未封存ELF', len(typed) == 367 and len(REPORT['closed_typed_references']) == 366 and len(REPORT['identity_only_unarchived_runtime_files']) == 1)
diag_prefix = 'stage/diagnostic-old-rr-r1/remote-originals/diagnostic-old-rr-r1/'
diagnostic = json.loads(local[diag_prefix + 'result.json'])
completion = json.loads(local[diag_prefix + 'exact_old_rr_once/completion.json'])
launch = json.loads(local[diag_prefix + 'exact_old_rr_once/launch.json'])
old_stdout = local[diag_prefix + 'exact_old_rr_once/stdout.log']
elf = diagnostic['elf_before']
check('未封存ELF身份来源及一次实际执行原件可解出', diagnostic['status'] == 'PASS_SINGLE_OBSERVATION_NOT_FULL_QA' and diagnostic['original_cause'] == 'UNCONFIRMED' and diagnostic['actual_test_count'] == 1 and diagnostic['source_and_elf_before_after'] == 'PASS' and elf == original['separate_diagnostic']['elf_before'] and elf['sha256'] == '564f37a7b0c19b7b1a9354235d52c579aef0afc3cdec74d876bf593313a6d259' and elf['bytes'] == 88772656 and completion['argv'][0] == elf['path'] and completion['argv'][1:] == [diagnostic['test'], '--exact', '--test-threads=1'] and completion['reaped'] is True and completion['exit_code'] == 0 and not completion['signals'] and completion['exec_gate_released'] is True and all(completion[k] == launch[k] for k in ('argv', 'pid', 'identity', 'started_monotonic_ns', 'bootstrap_sha256')) and b'1 passed; 0 failed; 0 ignored' in old_stdout, {'archived_result': 'local-evidence.tar.gz -> ' + diag_prefix + 'result.json', 'archived_completion': 'local-evidence.tar.gz -> ' + diag_prefix + 'exact_old_rr_once/completion.json', 'ELF_file_archived': False})
unknown = json.loads(local['stage/native-r7/host/lost_local_owner_observation_r1.json'])
stale = json.loads(local['stage/native-r7/host/checks.owner/status.json'])
check('包内失去原owner仍UNKNOWN且没有伪result', 'stage/native-r7/host/checks.owner/result.json' not in local and unknown['status'] == 'LOCAL_OWNER_REAP_UNKNOWN' and unknown['original_wait_returncode'] is None and unknown['original_reaped'] is None and stale['status'] == 'RUNNING' and stale['wait_returncode'] is None and stale['reaped'] is False and summary['software']['original_local_QA_owner_wait'] == summary['software']['original_local_QA_owner_reap'] == 'UNKNOWN')
check('软件证据没有混入之后硬件READ', summary['boundary']['UART'] is False and summary['boundary']['new_hardware_GO'] is False and summary['boundary']['this_software_stage_hardware_motion'] == 'NOT_RUN' and summary['boundary']['deployable'] is False and summary['boundary']['full_B1'] == 'NOT_IMPLEMENTED' and not any('hardware_read' in n or 'supervised_read' in n for n in inputs['before']))

doc_verify = load(BASE / 'documentation_update_verification.json')
docs = []
for name, value in doc_verify['docs'].items():
    old = local['project-docs-before-stage/' + name]
    current = (ROOT / name).read_bytes()
    matcher = difflib.SequenceMatcher(a=old.splitlines(keepends=True), b=current.splitlines(keepends=True), autojunk=False)
    opcodes = matcher.get_opcodes()
    inserted = b''.join(b''.join(current.splitlines(keepends=True)[j:j2]) for tag, i, i2, j, j2 in opcodes if tag == 'insert').decode()
    check('文档旧历史逐字保留/仅插入/' + name, all(tag in ('equal', 'insert') for tag, *_ in opcodes) and hashlib.sha256(old).hexdigest() == value['old_sha256'] and hashlib.sha256(current).hexdigest() == value['new_sha256'] and len(current) == value['new_bytes'], {'current': ref(ROOT / name), 'inserted_lines': sum(j2 - j for tag, i, i2, j, j2 in opcodes if tag == 'insert')})
    check('文档当前事实与限界/' + name, all(t in inserted for t in ['43b9d675', '641', '734', '739', '24', 'UNKNOWN', 'B1', 'P1A', 'FAIL', 'D15', 'deployable=false']) and ('最终质量绑定待完成' in inserted or '独核与最终质量绑定待完成' in inserted or '归档独核与最终质量绑定待完成' in inserted), {'review_state': '这是本次独核前生成的待root验收文档，最终质量通过后root应新更新状态，原准备期文档回执保持'} )
    docs.append({'path': name, 'identity': ref(ROOT / name), 'current_status': 'PENDING_ROOT_FINAL_QUALITY_AND_ACCEPTANCE', 'software_hardware_boundaries_preserved': True})
check('4文档含verification上部记录及历史表审计行', len(docs) == 4 and (ROOT / 'docs/10_verification.md').read_text().count('2026-10-08') >= 2)
readback = load(BASE / 'root-source-readback.json')
for name, value in readback['changed_files'].items():
    current = ref(ROOT / name)
    archived = local['main-final-source/' + name]
    check('当前六源/本机原件/frozenSHA一致/' + name, current['sha256'] == value['sha256'] == hashlib.sha256(archived).hexdigest() and current['bytes'] == value['bytes'] == len(archived) and bool((ROOT / name).stat().st_mode & stat.S_IXUSR) == value['executable'])
for name in ('local_archive', 'local_index', 'source_identity_check', 'orin_original_summary'):
    value = summary['originals'][name]
    current = ref(BASE / value['path'])
    check('最终入口显式归档引用/' + name, all(current[k] == value[k] for k in ('sha256', 'bytes')))
REPORT['summary_at_review'] = ref(BASE / 'summary.json')
REPORT['native_dynamic_review_basis'] = ref(BASE / 'independent_native_dynamic_review_r1.json')
REPORT['documents_at_review'] = docs
REPORT['counts'] = {'native_packages': 7, 'local_indexed_originals': 620, 'local_regular_tar_members': 621, 'fixed_inputs_before_after': 648, 'original_summary_typed_references': 367, 'typed_references_actual_archived': 366, 'identity_only_unarchived_ELF': 1}
REPORT['root_followups'] = ['在summary中明确旧ELF为identity_only_unarchived_runtime_files；367引用中仅366原件实际归档可解出，不宣称旧ELF字节封存', 'root最终质量绑定与验收完成后新增状态附件并更新4文档待完成口径，原七包、local包、原QA及文档准备期回执不重写']
REPORT['status'] = 'FINAL_ARCHIVE_SOURCE_AND_RAW_RECEIPTS_ACCEPTED_WITH_IDENTITY_ONLY_ELF_LIMIT' if all(c['status'] == 'PASS' for c in REPORT['checks']) else 'REVIEW_FINDINGS_REQUIRE_RESOLUTION'
out = BASE / 'independent_archive_review_r1.json'
with out.open('x') as f:
    json.dump(REPORT, f, ensure_ascii=False, indent=2)
    f.write('\n')
print(json.dumps({'status': REPORT['status'], 'report': ref(out), 'checks': len(REPORT['checks']), 'failures': [c for c in REPORT['checks'] if c['status'] != 'PASS'], 'counts': REPORT['counts']}, ensure_ascii=False))
