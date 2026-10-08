import datetime
import hashlib
import json
import stat
import tarfile
from pathlib import Path, PurePosixPath

root=Path('/Users/loosqk/repo/rust/robot-system')
stage=root/'.verify/b1_ingress_components_2026-10-08'
out=root/'target/b1_freshness_integration_2026-10-08/independent_stage_scope_r1'

def ref(path):
    data=path.read_bytes()
    return {'path':str(path),'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest(),'mode':stat.S_IMODE(path.stat().st_mode)}

def load(path):
    def pairs(rows):
        result={}
        for key,value in rows:
            assert key not in result
            result[key]=value
        return result
    return json.loads(path.read_text(),object_pairs_hook=pairs)

def pinned(value):
    actual=ref(Path(value['path']))
    assert all(actual[k]==value[k] for k in ('bytes','sha256','mode'))
    return actual

summary_path=stage/'summary.json'
summary_pin=ref(summary_path)
assert summary_pin['sha256']=='b1600fde28e4d5b851e45d9b7b96fa86faf301e49baa0af57c8a9b5438271934' and summary_pin['bytes']==10642
summary=load(summary_path)
assert summary['status']=='MAIN_INTEGRATED_SOFTWARE_PASS' and summary['commit_status']=='PENDING_ROOT_COMMIT' and summary['deployable'] is False
assert summary['source']['main_integrated_count']==596 and summary['source']['aggregate']=='b919cd320dd81693ae49612dcd0778b98e6ee97191d2344bdf87cefd69c2eed0'
all_members={}
archive_rows=[]
for archive_name,index_name,count in [('original-evidence.tar.gz','original-evidence.index.json',1337),('main-binding-review.tar.gz','main-binding-review.index.json',6)]:
    index=load(stage/index_name)
    archive_pin=pinned(index['archive'])
    entries={x['name']:x for x in index['files']}
    assert len(entries)==len(index['files'])==index['member_count']==count
    with tarfile.open(stage/archive_name,'r:gz') as tar:
        members=tar.getmembers()
        assert len(members)==len({m.name for m in members})==count and {m.name for m in members}==set(entries)
        for member in members:
            path=PurePosixPath(member.name)
            assert member.isfile() and not path.is_absolute() and '..' not in path.parts and path.as_posix()==member.name and '\\' not in member.name
            item=entries[member.name];data=tar.extractfile(member).read()
            assert len(data)==item['bytes']==member.size and hashlib.sha256(data).hexdigest()==item['sha256'] and member.mode==item['mode']
            original=ref(Path(item['original_path']))
            assert all(original[k]==item[k] for k in ('bytes','sha256','mode'))
            if 'identity_after_archive' in item:
                assert item['identity_before']==item['identity_after_read']==item['identity_after_archive']
            else:
                assert item['identity_before']==item['identity_after']
            all_members[(archive_name,member.name)]=(item,data)
    if archive_name=='original-evidence.tar.gz':
        canonical=json.dumps([{k:x[k] for k in ('name','sha256','bytes','mode')} for x in index['files']],sort_keys=True,separators=(',',':')).encode()
        assert hashlib.sha256(canonical).hexdigest()==index['member_manifest_sha256']==summary['archive']['member_manifest_sha256']
    archive_rows.append({'archive':archive_pin,'index':ref(stage/index_name),'members':count,'all_safe_unique_regular_and_original_member_bytes_sha_mode_equal':True})

member_checks=[]
def archive_refs(value):
    if isinstance(value,dict):
        if 'archive_member' in value:
            archive_name=value.get('archive','original-evidence.tar.gz')
            key=(archive_name,value['archive_member'])
            assert key in all_members
            item,data=all_members[key]
            for field in ('bytes','sha256'):
                if field in value: assert value[field]==item[field]
            member_checks.append({'archive':archive_name,'member':value['archive_member'],'sha256':item['sha256'],'bytes':item['bytes']})
            for field in ('index_member','verification_member','summary_member'):
                if field in value: assert (archive_name,value[field]) in all_members
        for child in value.values(): archive_refs(child)
    elif isinstance(value,list):
        for child in value: archive_refs(child)
archive_refs(summary)
for key in ('original_evidence','index','verification'):
    pinned(summary['archive'][key])
for key in ('archive','index','verification'):
    pinned(summary['archive']['main_binding_support'][key])
assert summary['archive']['all_outer_member_count_including_support']==1337+6==1343

native=json.loads(all_members[('original-evidence.tar.gz','native-summary/native-r5-summary.json')][1])
prior=json.loads(all_members[('original-evidence.tar.gz','reviews-and-quality/independent_native_r5/review.json')][1])
main=json.loads(all_members[('main-binding-review.tar.gz','independent_main_binding_r1/review.json')][1])
assert native['status']=='PASS_SCOPED_SOFTWARE' and prior['status']=='PASS_SCOPED_SOFTWARE_NO_BLOCKER_PENDING_MAIN_INTEGRATION_BINDING'
assert main['status']=='PASS_MAIN_596_EXACT_AND_NATIVE_PRECOMMIT_QUALITY_BOUND'
assert native['source_aggregate']==summary['source']['aggregate']==main['source']['aggregate']
assert native['count_policy']['different_Rust_method_pass_total']==summary['actual_validation']['different_Rust_method_passes']==313
counts=summary['actual_validation']
assert (counts['core'],counts['transport'],counts['p1c'])==(147,129,37)
assert counts['original_Rust_ignored']==counts['original_doc_ignored']==2 and counts['compile_fail_doctest_passes_separate']==1
assert counts['native_owned_children']==18 and summary['precommit_quality']['main_fmt_owned_children']==7
assert summary['precommit_quality']['independent_status']==main['status']
assert summary['next_piece']['status']=='FUNCTION_PLAN_ROOT_APPROVED_NOT_IMPLEMENTED_NOT_TESTED'
assert '25拟议oracle仅计划，不计本轮313' in summary['next_piece']['scope']
assert '不含生产IPC/adapter/串口执行接入' in summary['implemented']
assert any('无UART/硬件GO/部署/正式安装/push' in x for x in summary['limits'])
assert any('现场FW未读' in x and 'D15 OPEN_DEFERRED/deployable=false不变' in x for x in summary['limits'])
assert any('仅identity-only未收字节' in x for x in summary['limits'])
history=[]
for version in ('r2','r3','r4'):
    package=summary['history']['immutable_native_packages']['native_'+version]
    old=json.loads(all_members[('original-evidence.tar.gz',package['summary_member'])][1])
    assert package['status']=='FAIL_ORIGINAL_UNCHANGED' and old['status'].startswith('FAIL')
    assert old.get('Rust_test_method_executions',old.get('Rust_method_executions'))==0
    nested_index=json.loads(all_members[('original-evidence.tar.gz',package['index_member'])][1])
    assert len(nested_index['files'])==package['original_member_count']
    history.append({'round':version,'status':old['status'],'original_native_members':package['original_member_count'],'Rust_method_executions':0,'nested_container_sha_checked_not_reexecuted':True})
assert 'Linux NOT_RUN，不伪记FAIL' in summary['history']['p1c_r1']
assert ref(summary_path)==summary_pin

review={
    'schema':'B1_INGRESS_COMPONENTS_STAGE_INDEPENDENT_SCOPE_REVIEW_R1',
    'status':'PASS_STAGE_SCOPE_AND_ARCHIVE_NO_BLOCKER_PENDING_ROOT_COMMIT',
    'created_at':datetime.datetime.now(datetime.timezone.utc).isoformat(),
    'stage_summary':summary_pin,
    'archives':archive_rows,
    'total_outer_members':1343,
    'typed_member_reference_checks':len(member_checks),
    'member_references':member_checks,
    'accepted_scope':'主树已整合P1A/P1B/P1C十一源纯软件组件，596/b919同r5原生及root质量绑定；313不同Rust方法、doc compile_fail1另计，库原2/doc原2 ignored保留。',
    'non_additive':'mock61在默认transport129内；P1C默认36在guard37内，guard内部36调用不加；Mac143不加Linux147，trybuild8子例不加成8方法。',
    'precommit_binding':'现存root main596/mainfmt7child及r5实际fmt→根Clippy同源闭合；native旧future REQUIRED记录不改，新旁包main PASS提供后续事实。',
    'historical_failures':history,
    'scope_limits':['next absolute deadline写执行25拟议oracle仅FUNCTION_PLAN_APPROVED，未实现/未测试，不计313。','非完整workspace/release/H2/完整B1运行接入/真实RT WCET或硬件验收；未读取现场FW/改Phase。','没有新UART/GO/部署/安装/push，硬件FAIL与D15 OPEN/DEFERRED、deployable=false保持；断电收尾不回填旧PENDING。','嵌套native档以已核container SHA封存，不重复动态审；工具链及外部工具binary仅身份记录，不宣称byte封存。'],
    'blockers':[],
    'remaining':['root最终commit仍PENDING；本报告不改stage summary/旧reviews或原FAIL。'],
    'operations':{'Cargo':0,'SSH':0,'UART':0,'source_edits':0,'stage_original_edits':0,'old_review_edits':0}
}
out.mkdir(parents=True,exist_ok=True)
(out/'review.json').write_text(json.dumps(review,ensure_ascii=False,indent=2)+'\n')
(out/'review.md').write_text('爸爸，阶段汇总与封包范围独审通过，无当前阻断。大包1337成员及main-binding旁包6成员，共1343个成员，全部安全相对路径、普通文件、唯一名称，bytes/SHA/mode与索引及本机原件相同；汇总typed成员引用闭合。\n\n准确口径为P1A/P1B/P1C十一源纯软件组件已整合，596/b919与r5原生及root提交前质量绑定相同。313不同Rust方法；doc compile_fail1另列，库原2/doc原2 ignored保留。mock61/默认36/guard内部36/Mac143/trybuild8子例不累计；r1 NOT_RUN与r2/r3/r4原FAIL/0方法执行保持。\n\n下一片绝对写期限仅函数计划批准，25拟议oracle未实现/测试。没有完整B1运行接入、硬件、部署或生产替换宣称，现场FW未读；旧FAIL、D15 OPEN/DEFERRED、deployable=false保持。旧native future REQUIRED记录保持，新main-binding旁证闭合。root最终提交仍PENDING。\n\n此次仅核新外层封存和表述，未重跑native/源码审或Cargo/SSH/UART；没有改原包、summary或旧review。\n')
(out/'index.json').write_text(json.dumps({'schema':'B1_STAGE_SCOPE_REVIEW_INDEX','files':[ref(out/'review.json'),ref(out/'review.md'),ref(out/'verify.py')],'stage_summary':summary_pin},ensure_ascii=False,indent=2)+'\n')
print(json.dumps({'status':review['status'],'files':[ref(out/'review.json'),ref(out/'review.md'),ref(out/'index.json')],'members':1343,'typed_member_checks':len(member_checks)},ensure_ascii=False,indent=2))
