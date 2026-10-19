#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYFIELD'
import base64,hashlib,os
from pathlib import Path
p=Path('/home/loosqk/robot-checks/base-zero-sync-diagnostic-r2')
assert p.is_dir() and p==p.resolve()
files={'field_confirmation_base_zero_sync_r1.json': {'base64': 'ewogICJzY2hlbWEiOiAiQkFTRV9aRVJPX1NZTkNfUjJfRElSRUNUX0ZJRUxEX0NPTkZJUk1BVElPTiIsCiAgInJlY29yZGVkX2F0X25zIjogMTc5MTg4NDM2NzAwMDAwMDAwMCwKICAicmVjb3JkZWRfYXRfdXRjIjogIjIwMjYtMTAtMTNUMDk6Mzk6MjcrMDA6MDAiLAogICJ0aW1lc3RhbXBfYmFzaXMiOiAi5pys6L2ucm9vdOmmluasoeinguWvn+ebtOaOpeeUqOaIt+WbnuWkjeeahOW3peWFt+aXtuWIu++8m+S4jeWboOWHhuWkh+W7tuWQjuWIt+aWsCIsCiAgInF1ZXN0aW9uX2lkIjogImNhbGxfNGZmMDU4MDMzOWRiNDg3NTkwZTQ1ZmU3NGYwMDUyM2MiLAogICJodW1hbl9hbnN3ZXIiOiAi5bey5oGi5aSN5L6b55S177yM5LiJ6L2u56iz5a6a5p6256m677yM6KeC5a+f5ZKM55yL5oqk5bCx57uq77yM5Y+v56uL5Y2z5pat55S1IiwKICAicG93ZXIiOiAiT05fVVNFUl9SRVBPUlRFRCIsCiAgImluZGVwZW5kZW50X3Bvd2VyX3RlbGVtZXRyeSI6ICJVTktOT1dOIiwKICAic2NvcGUiOiAi5LuF5Y+zOS9iYXNlX3JpZ2h0X3doZWVs5LiA5qyh5Y2VSUQ5IFNZTkNfV1JJVEUgR29hbF9WZWxvY2l0eTDvvIjml6BBQ0vvvInihpLlj5HpgIHlrozmiJDlkI7oh7PlsJE1MG1z4oaSRW5hYmxl4oaSNTAwbXPljp9QVuinguWvn+KGkkRpc2FibGXlj4rljp9TVE9QNTHvvJvlj6/og73ovazliqjvvIzkuInova7pobvlt7LnprvlnLDkuJTlupXnm5jnqLPlrprmlK/mkpEiLAogICJwaHlzaWNhbCI6IHsKICAgICJsZWZ0IjogewogICAgICAic3VwcG9ydGVkIjogdHJ1ZSwKICAgICAgInVubG9hZGVkIjogdHJ1ZSwKICAgICAgImFyZWFfY2xlYXIiOiB0cnVlLAogICAgICAiY29udGludW91c19wcmVzZW5jZV91bnRpbF9jbGVhbnVwIjogdHJ1ZSwKICAgICAgImltbWVkaWF0ZV9wb3dlcl9jdXQiOiB0cnVlLAogICAgICAiY29uZmlybWVkX2F0X25zIjogMTc5MTg4NDM2NzAwMDAwMDAwMAogICAgfSwKICAgICJyaWdodCI6IHsKICAgICAgInN1cHBvcnRlZCI6IHRydWUsCiAgICAgICJ1bmxvYWRlZCI6IHRydWUsCiAgICAgICJhcmVhX2NsZWFyIjogdHJ1ZSwKICAgICAgImNvbnRpbnVvdXNfcHJlc2VuY2VfdW50aWxfY2xlYW51cCI6IHRydWUsCiAgICAgICJpbW1lZGlhdGVfcG93ZXJfY3V0IjogdHJ1ZSwKICAgICAgImNvbmZpcm1lZF9hdF9ucyI6IDE3OTE4ODQzNjcwMDAwMDAwMDAsCiAgICAgICJhbGxfdGhyZWVfd2hlZWxzX29mZl9ncm91bmQiOiB0cnVlLAogICAgICAiYmFzZV9zdGFibHlfc3VwcG9ydGVkIjogdHJ1ZSwKICAgICAgImJhc2VfY29uZmlybWF0aW9uX2tpbmQiOiAiQ1VSUkVOVF9TRVRVUF9SRUFEWSIsCiAgICAgICJiYXNlX2NvbmZpcm1hdGlvbl9hbnN3ZXIiOiAi5bey5oGi5aSN5L6b55S177yM5LiJ6L2u56iz5a6a5p6256m677yM6KeC5a+f5ZKM55yL5oqk5bCx57uq77yM5Y+v56uL5Y2z5pat55S1IiwKICAgICAgImJhc2VfY29uZmlybWF0aW9uX3Njb3BlIjogIuS7heWPszkvYmFzZV9yaWdodF93aGVlbOS4gOasoeWNlUlEOSBTWU5DX1dSSVRFIEdvYWxfVmVsb2NpdHkw77yI5pegQUNL77yJ4oaS5Y+R6YCB5a6M5oiQ5ZCO6Iez5bCRNTBtc+KGkkVuYWJsZeKGkjUwMG1z5Y6fUFbop4Llr5/ihpJEaXNhYmxl5Y+K5Y6fU1RPUDUx77yb5Y+v6IO96L2s5Yqo77yM5LiJ6L2u6aG75bey56a75Zyw5LiU5bqV55uY56iz5a6a5pSv5pKRIgogICAgfQogIH0sCiAgInVzYl9yZXBsdWdfcmVxdWlyZWQiOiBmYWxzZSwKICAicG93ZXJfcmVzZXRfcmVxdWlyZWQiOiBmYWxzZSwKICAidmlzdWFsX29ic2VydmVyX3JlYWR5IjogdHJ1ZSwKICAicHJpb3JfdmlzdWFsX29ic2VydmF0aW9uIjogIuacqueci+a4he+8m+eUqOaIt+imgeaxguWGjea1i+S4gOasoe+8jOS4jeS9nOaXp+i9rueJqeeQhue7k+iuuiIKfQo=', 'sha256': 'ffa941a0857f28478cd62dcb016ad94a1669326e904d6c14f8beb09920ed32fb', 'bytes': 1880}, 'base-zero-sync-diagnostic-r1.go.json': {'base64': 'ewogICJzY2hlbWEiOiAiTUFJTlRFTkFOQ0VfR09fVjIiLAogICJpc3N1ZWRfYnkiOiAicm9vdCIsCiAgInBsYW5fc2hhMjU2IjogIjU3NzE1NjhiOTFjNDNkN2MwODZhOWNhMjA0YmMyMjYzZjcxYWJjMjc5ZjVhNDE3YzNjNTI4M2U3NzcwODA2YzQiLAogICJhcnRpZmFjdF9yb290IjogIi9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvYmFzZS16ZXJvLXN5bmMtZGlhZ25vc3RpYy1yMiIsCiAgImlzc3VlZF9hdF9ucyI6IDE3OTE4ODQ5NDI5NDU5NzMwMDAsCiAgImV4cGlyZXNfYXRfbnMiOiAxNzkxODg0OTY3MDAwMDAwMDAwLAogICJob3N0IjogewogICAgImhvc3RuYW1lIjogInVidW50dSIsCiAgICAidWlkIjogMTAwMCwKICAgICJnaWQiOiAxMDAwLAogICAgImJvb3RfaWQiOiAiNmNmN2VlM2UtYTNjNS00YzkxLTgwYmMtNGNhOTI0ODE4YjUzIgogIH0sCiAgInNvdXJjZSI6ICLlvZPliY3nlKjmiLfnm7TmjqXnoa7orqRjYWxsXzRmZjA1ODAzMzlkYjQ4NzU5MGU0NWZlNzRmMDA1MjNj77yb5pys6L2uZnJlc2gxNy8yM+OAgTU4cmVmc+WPiuWUr+S4gFBMQU7nu49yb2905ZKM56qE54us5a6h6YCa6L+H77yb5LuF5LiA5qyh5Y6fcmlnaHQ5IFNZTkPpm7blgLzihpLoh7PlsJE1MG1z4oaSRW5hYmxl4oaS5Y6f5LiJUFYvNTAwbXPihpJEaXNhYmxl5ZKMU1RPUDUx77yb5Y6fNjAw56eS546w5Zy65pyf6ZmQ5LiN57utIiwKICAicGh5c2ljYWwiOiB7CiAgICAibGVmdCI6IHsKICAgICAgInN1cHBvcnRlZCI6IHRydWUsCiAgICAgICJ1bmxvYWRlZCI6IHRydWUsCiAgICAgICJhcmVhX2NsZWFyIjogdHJ1ZSwKICAgICAgImNvbnRpbnVvdXNfcHJlc2VuY2VfdW50aWxfY2xlYW51cCI6IHRydWUsCiAgICAgICJpbW1lZGlhdGVfcG93ZXJfY3V0IjogdHJ1ZSwKICAgICAgImNvbmZpcm1lZF9hdF9ucyI6IDE3OTE4ODQzNjcwMDAwMDAwMDAKICAgIH0sCiAgICAicmlnaHQiOiB7CiAgICAgICJzdXBwb3J0ZWQiOiB0cnVlLAogICAgICAidW5sb2FkZWQiOiB0cnVlLAogICAgICAiYXJlYV9jbGVhciI6IHRydWUsCiAgICAgICJjb250aW51b3VzX3ByZXNlbmNlX3VudGlsX2NsZWFudXAiOiB0cnVlLAogICAgICAiaW1tZWRpYXRlX3Bvd2VyX2N1dCI6IHRydWUsCiAgICAgICJjb25maXJtZWRfYXRfbnMiOiAxNzkxODg0MzY3MDAwMDAwMDAwLAogICAgICAiYWxsX3RocmVlX3doZWVsc19vZmZfZ3JvdW5kIjogdHJ1ZSwKICAgICAgImJhc2Vfc3RhYmx5X3N1cHBvcnRlZCI6IHRydWUsCiAgICAgICJiYXNlX2NvbmZpcm1hdGlvbl9raW5kIjogIkNVUlJFTlRfU0VUVVBfUkVBRFkiLAogICAgICAiYmFzZV9jb25maXJtYXRpb25fYW5zd2VyIjogIuW3suaBouWkjeS+m+eUte+8jOS4iei9rueos+Wumuaetuepuu+8jOinguWvn+WSjOeci+aKpOWwsee7qu+8jOWPr+eri+WNs+aWreeUtSIsCiAgICAgICJiYXNlX2NvbmZpcm1hdGlvbl9zY29wZSI6ICLku4Xlj7M5L2Jhc2VfcmlnaHRfd2hlZWzkuIDmrKHljZVJRDkgU1lOQ19XUklURSBHb2FsX1ZlbG9jaXR5MO+8iOaXoEFDS++8ieKGkuWPkemAgeWujOaIkOWQjuiHs+WwkTUwbXPihpJFbmFibGXihpI1MDBtc+WOn1BW6KeC5a+f4oaSRGlzYWJsZeWPiuWOn1NUT1A1Me+8m+WPr+iDvei9rOWKqO+8jOS4iei9rumhu+W3suemu+WcsOS4lOW6leebmOeos+WumuaUr+aSkSIKICAgIH0KICB9LAogICJzY29wZSI6ICLku4Xlj7M5L2Jhc2VfcmlnaHRfd2hlZWzkuIDmrKHljZVJRDkgU1lOQ19XUklURSBHb2FsX1ZlbG9jaXR5MO+8iOaXoEFDS++8ieKGkuWPkemAgeWujOaIkOWQjuiHs+WwkTUwbXPihpJFbmFibGXihpI1MDBtc+WOn1BW6KeC5a+f4oaSRGlzYWJsZeWPiuWOn1NUT1A1Me+8m+WPr+iDvei9rOWKqO+8jOS4iei9rumhu+W3suemu+WcsOS4lOW6leebmOeos+WumuaUr+aSkSIKfQo=', 'sha256': 'dd44110a23a1cc64389d04b1f61e2de15aa8c281a5195e0eda0bf1ba84e1ff72', 'bytes': 1886}}
for name,r in files.items():
 q=p/name;assert not os.path.lexists(q)
 raw=base64.b64decode(r['base64'],validate=True);assert hashlib.sha256(raw).hexdigest()==r['sha256'] and len(raw)==r['bytes']
 with q.open('xb') as stream:stream.write(raw);stream.flush();os.fsync(stream.fileno())
 os.chmod(q,0o600)
 assert hashlib.sha256(q.read_bytes()).hexdigest()==r['sha256']
PYFIELD
#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64,hashlib,importlib.util,json,os,shlex,subprocess,time
from pathlib import Path
FIELD_STAMP=1791884367000000000  # 仅root本次新现场事实可替换；草稿不能执行。
FIELD_EXPECTED=('ffa941a0857f28478cd62dcb016ad94a1669326e904d6c14f8beb09920ed32fb', 1880)  # root固定新现场回执(sha256,bytes)。
GO_EXPECTED=('dd44110a23a1cc64389d04b1f61e2de15aa8c281a5195e0eda0bf1ba84e1ff72', 1886)  # root固定本次新GO(sha256,bytes)，不能复用任何旧GO。
BINDING_EXPECTED=('c8a380a42af7ae3a6a44ccadea7e791a5e90b2bf459272714acbfa4f646baf5c', 30882)  # native准备后由root固定本轮binding(sha256,bytes)。
PLAN_EXPECTED=('c2c5e65746dd3cdae0dcee2ea90e328a45bdf1684475a7b2ab2e46f84d8192f8', 39856)  # native准备后由root固定完整PLAN stdout(sha256,bytes)。
NATIVE_PREPARATION_EXPECTED=('17db7ae4e9ad9e63cb6e70b19031795865fb3e8059485a065e64315cb937178a', 5116)  # root固定本轮原生软件prepare-result(sha256,bytes)。
PLAN_CANONICAL_SHA='5771568b91c43d7c086a9ca204bc2263f71abc279f5a417c3c5283e7770806c4'  # native准备后由root固定本轮canonical PLAN。
assert type(FIELD_STAMP) is int and FIELD_STAMP>0 and FIELD_EXPECTED is not None and GO_EXPECTED is not None, '爸爸，BASE_ZERO_SYNC_R1启动草稿未获root本次新GO，零启动。' 
assert BINDING_EXPECTED is not None and PLAN_EXPECTED is not None and NATIVE_PREPARATION_EXPECTED is not None and type(PLAN_CANONICAL_SHA) is str and len(PLAN_CANONICAL_SHA)==64, '爸爸，BASE_ZERO_SYNC_R1原生binding/PLAN未固定，零I/O拒绝。'
legacy=Path('/home/loosqk/robot-checks/position-return-20261010-r1');base=Path('/home/loosqk/robot-checks/base-zero-sync-diagnostic-r2');phase=base
expected={'maintenance_bindings_base_zero_sync_diagnostic_r1.json': BINDING_EXPECTED, 'full-plan-only-r1.stdout': PLAN_EXPECTED, 'prepare-result-base-zero-sync-r1.json': NATIVE_PREPARATION_EXPECTED, 'field_confirmation_base_zero_sync_r1.json': FIELD_EXPECTED, 'base-zero-sync-diagnostic-r1.go.json': GO_EXPECTED}
for name,(sha,size) in expected.items():
 path=phase/name;raw=path.read_bytes();assert path.is_file() and not path.is_symlink() and hashlib.sha256(raw).hexdigest()==sha and len(raw)==size
script=legacy/'software-facts-r1/scripts/run_hardware_maintenance.py'
assert hashlib.sha256(script.read_bytes()).hexdigest()=='d97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41'
spec=importlib.util.spec_from_file_location('base_zero_start_frozen_d97',script);m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
bindings=m.strict_json((phase/'maintenance_bindings_base_zero_sync_diagnostic_r1.json').read_text())
assert len(bindings['refs'])==58
SYNC_REF={'path': '/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/quality/frozen/feetech_poke', 'sha256': 'b2bfedb1bc420c9957a276cd69f36f5fd47c66d0b7aaa519cf3375e257eabd94', 'bytes': 1507224}
assert any(ref==SYNC_REF for ref in bindings['refs']) and SYNC_REF['path'] in bindings['binary_paths']
m.verify_ref(SYNC_REF,executable=True)
for ref in bindings['refs']:m.verify_ref(ref)
for side,files in bindings['protection'].items():
 for name,ref in files.items():m.verify_protection_file(side,name,ref)
assert Path('/proc/sys/kernel/random/boot_id').read_text().strip()==bindings['host']['boot_id']
for side,ref in bindings['usb'].items():
 device=Path(ref['device']);assert str(device.resolve())==ref['resolved'] and device.stat().st_rdev==ref['rdev'] and device.stat().st_ino==ref['device_inode'] and device.lstat().st_ino==ref['link_inode']
candidate=Path('/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/diagnostic-candidate/base_zero_sync_diagnostic_r1.py');helper=Path('/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/diagnostic-candidate/fixtures/raw_right9_from_r14_r2.py')
for path,pin in ((candidate,('3b68165786d7a603956f46843d8052a74316cb1fc061ba203dbb9737446001f7', 27496)),(helper,('70d777bd5c2a19e2ce3f57e68f9abd40eaed2b8a9aa8c592e7fe53a940f1e38c',12220)),(legacy/'normal-read-r4/nonuart_probe_r4.sh',('e3962ec7841a9d5ed783ca8279ed718c8f29ed718701de528c226f0588b91527',20020))):
 raw=path.read_bytes();assert not path.is_symlink() and hashlib.sha256(raw).hexdigest()==pin[0] and len(raw)==pin[1]
for name,path in (('base_zero_start_fixed_candidate',candidate),('base_zero_start_fixed_raw_helper',helper)):
 spec=importlib.util.spec_from_file_location(name,path);value=importlib.util.module_from_spec(spec);spec.loader.exec_module(value)
 if path==candidate:c=value
 else:h=value
preparation=m.strict_json((phase/'prepare-result-base-zero-sync-r1.json').read_text())
assert preparation['bound_refs']==58 and preparation['sync_tool']==SYNC_REF
assert preparation['round']==1 and preparation['profile_run_id']=='base-zero-sync-diagnostic-r1' and preparation['status']=='PASS_BASE_ZERO_SYNC_R1_NATIVE_RUNTIME_PREPARATION_ONLY' and preparation['uart_operations']==preparation['motor_actions']==0 and preparation['go_created'] is False and preparation['field_confirmation_created'] is False
plan=m.strict_json((phase/'full-plan-only-r1.stdout').read_text());assert plan['plan_sha256']==PLAN_CANONICAL_SHA and m.digest(plan['plan'])==plan['plan_sha256']
go_path=phase/'base-zero-sync-diagnostic-r1.go.json';go_raw=go_path.read_bytes();assert go_path.is_file() and not go_path.is_symlink()
go=m.strict_json(go_raw.decode());c.install(m,h,go);now=time.time_ns();m.validate_go(go,plan['plan'],now)
assert 0<go['expires_at_ns']-go['issued_at_ns']<=600_000_000_000 and 0<=now-FIELD_STAMP<600_000_000_000
assert all(go['physical'][side]['confirmed_at_ns']==FIELD_STAMP for side in ('left','right'))
output=phase/'base-zero-sync-diagnostic-r1';record=phase/'base-zero-sync-diagnostic-launch-r2';assert not output.exists() and not record.exists()

record.mkdir(mode=0o700)
def write_once(name,raw):
 with (record/name).open('xb') as stream:stream.write(raw);stream.flush();os.fsync(stream.fileno())
def start_tmux(name,session,launcher):
 path=record/name;write_once(name,launcher)
 argv=['tmux','new-session','-d','-s',session,'-P','-F','#{session_name} #{window_id} #{pane_id} #{pane_pid}','bash '+str(path)]
 started=time.time_ns();child=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE);a,b=child.communicate();code=child.wait()
 row={'argv':argv,'pid':child.pid,'started_ns':started,'finished_ns':time.time_ns(),'exit_code':code,'wait_returncode':code,'reaped':True,'stdout':a.decode(),'stderr':b.decode(),'signals_sent':[]}
 write_once(name+'.tmux-receipt.json',(json.dumps(row,ensure_ascii=False,indent=2)+'\n').encode());assert code==0 and b==b''
 return a.decode().strip(),row
probe_argv=['bash',str(legacy/'normal-read-r4/nonuart_probe_r4.sh')]
started=time.time_ns();child=subprocess.Popen(probe_argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE);probe_out,probe_err=child.communicate();code=child.wait()
write_once('nonuart-before-motor.stdout',probe_out);write_once('nonuart-before-motor.stderr',probe_err)
probe_receipt={'argv':probe_argv,'pid':child.pid,'started_ns':started,'finished_ns':time.time_ns(),'exit_code':code,'wait_returncode':code,'reaped':True,'signals_sent':[]}
write_once('nonuart-before-motor.receipt.json',(json.dumps(probe_receipt,ensure_ascii=False,indent=2)+'\n').encode())
probe=m.strict_json(probe_out.decode());assert code==0 and probe_err==b'' and probe['status']=='PASS_NON_UART_CURRENT_FACTS'
assert len(probe['checks'])==17 and all(v['status']=='PASS' for v in probe['checks'])
# 前置非UART门消耗的时间不能续现场/GO期限；再次真实校验后才允许唯一tmux。
assert len(bindings['refs'])==58
SYNC_REF={'path': '/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/quality/frozen/feetech_poke', 'sha256': 'b2bfedb1bc420c9957a276cd69f36f5fd47c66d0b7aaa519cf3375e257eabd94', 'bytes': 1507224}
assert any(ref==SYNC_REF for ref in bindings['refs']) and SYNC_REF['path'] in bindings['binary_paths']
m.verify_ref(SYNC_REF,executable=True)
for ref in bindings['refs']:m.verify_ref(ref)
for side,files in bindings['protection'].items():
 for name,ref in files.items():m.verify_protection_file(side,name,ref)
assert Path('/proc/sys/kernel/random/boot_id').read_text().strip()==bindings['host']['boot_id']
for side,ref in bindings['usb'].items():
 device=Path(ref['device']);assert str(device.resolve())==ref['resolved'] and device.stat().st_rdev==ref['rdev'] and device.stat().st_ino==ref['device_inode'] and device.lstat().st_ino==ref['link_inode']
now=time.time_ns();m.validate_go(go,plan['plan'],now)
assert 0<FIELD_STAMP<=now and now-FIELD_STAMP<600_000_000_000
assert not output.exists()
launcher=base64.b64decode('IyEvYmluL2Jhc2gKIyDniLjniLjvvIzkv53mjIFzdGRpbui/nuaOpXRtdXjnmoRQVFnvvIzlj6rmnInmnKzova7lhajpg6hwaW5z57uR5a6a5ZCO5omN5omn6KGM77yb5q2j5paHL+WPquivu+WAmemAieWFpeWPo+S/neaMgeOAggpzZXQgLWV1CmV4cG9ydCBQWVRIT05ET05UV1JJVEVCWVRFQ09ERT0xCmV4ZWMgL2hvbWUvbG9vc3FrLy5sb2NhbC9zaGFyZS91di9weXRob24vY3B5dGhvbi0zLjEyLjEzLWxpbnV4LWFhcmNoNjQtZ251L2Jpbi9weXRob24zIC1jICdpbXBvcnQgb3Msc3VicHJvY2VzcwpGSUVMRF9TVEFNUD1Ob25lCkZJRUxEX0VYUEVDVEVEPU5vbmUKR09fRVhQRUNURUQ9Tm9uZQpCSU5ESU5HX0VYUEVDVEVEPU5vbmUKUExBTl9FWFBFQ1RFRD1Ob25lClBMQU5fQ0FOT05JQ0FMX1NIQT1Ob25lCk5BVElWRV9QUkVQQVJBVElPTl9FWFBFQ1RFRD1Ob25lCmFzc2VydCB0eXBlKEZJRUxEX1NUQU1QKSBpcyBpbnQgYW5kIEZJRUxEX1NUQU1QPjAgYW5kIGFsbCh2IGlzIG5vdCBOb25lIGZvciB2IGluIChGSUVMRF9FWFBFQ1RFRCxHT19FWFBFQ1RFRCxCSU5ESU5HX0VYUEVDVEVELFBMQU5fRVhQRUNURUQsTkFUSVZFX1BSRVBBUkFUSU9OX0VYUEVDVEVEKSkgYW5kIHR5cGUoUExBTl9DQU5PTklDQUxfU0hBKSBpcyBzdHIgYW5kIGxlbihQTEFOX0NBTk9OSUNBTF9TSEEpPT02NCwgJyInIifniLjniLjvvIxTWU5D6Zu25YC8bGF1bmNoZXLmnKrnu5HlrprmnKzova7njrDlnLovR08v6L2v5Lu25YeG5aSH77yM6Zu2SS9P5ouS57ud44CCJyInIicKbG9nPW9zLm9wZW4oJyInIicvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL2Jhc2UtemVyby1zeW5jLWRpYWdub3N0aWMtcjIvYmFzZS16ZXJvLXN5bmMtZGlhZ25vc3RpYy1sYXVuY2gtcjIvZHJpdmVyLmxvZyciJyInLG9zLk9fV1JPTkxZfG9zLk9fQ1JFQVR8b3MuT19FWENMLDBvNjAwKQpvcy5kdXAyKGxvZywxKTtvcy5kdXAyKGxvZywyKTtvcy5jbG9zZShsb2cpCnN1YnByb2Nlc3MucnVuKFsnIiciJ3RtdXgnIiciJywnIiciJ3NldC1vcHRpb24nIiciJywnIiciJy1wJyInIicsJyInIictdCciJyInLG9zLmVudmlyb25bJyInIidUTVVYX1BBTkUnIiciJ10sJyInIidyZW1haW4tb24tZXhpdCciJyInLCciJyInb24nIiciJ10sY2hlY2s9VHJ1ZSkKb3MuZXhlY3YoJyInIicvaG9tZS9sb29zcWsvLmxvY2FsL3NoYXJlL3V2L3B5dGhvbi9jcHl0aG9uLTMuMTIuMTMtbGludXgtYWFyY2g2NC1nbnUvYmluL3B5dGhvbjMnIiciJyxbJyInIicvaG9tZS9sb29zcWsvLmxvY2FsL3NoYXJlL3V2L3B5dGhvbi9jcHl0aG9uLTMuMTIuMTMtbGludXgtYWFyY2g2NC1nbnUvYmluL3B5dGhvbjMnIiciJywnIiciJy9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MveGxlcm9ib3Qtc3luYy16ZXJvLXNvZnR3YXJlLTIwMjYxMDEzL2RpYWdub3N0aWMtY2FuZGlkYXRlL2Jhc2VfemVyb19zeW5jX2RpYWdub3N0aWNfcjEucHknIiciJywnIiciJy0tcmF3LWhlbHBlciciJyInLCciJyInL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy94bGVyb2JvdC1zeW5jLXplcm8tc29mdHdhcmUtMjAyNjEwMTMvZGlhZ25vc3RpYy1jYW5kaWRhdGUvZml4dHVyZXMvcmF3X3JpZ2h0OV9mcm9tX3IxNF9yMi5weSciJyInLCciJyInLS1tYWludGVuYW5jZSciJyInLCciJyInL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTAtcjEvc29mdHdhcmUtZmFjdHMtcjEvc2NyaXB0cy9ydW5faGFyZHdhcmVfbWFpbnRlbmFuY2UucHknIiciJywnIiciJy0tYmluZGluZ3MnIiciJywnIiciJy9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvYmFzZS16ZXJvLXN5bmMtZGlhZ25vc3RpYy1yMi9tYWludGVuYW5jZV9iaW5kaW5nc19iYXNlX3plcm9fc3luY19kaWFnbm9zdGljX3IxLmpzb24nIiciJywnIiciJy0tb3V0cHV0JyInIicsJyInIicvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL2Jhc2UtemVyby1zeW5jLWRpYWdub3N0aWMtcjIvYmFzZS16ZXJvLXN5bmMtZGlhZ25vc3RpYy1yMSciJyInLCciJyInLS1hcnRpZmFjdC1yb290JyInIicsJyInIicvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL2Jhc2UtemVyby1zeW5jLWRpYWdub3N0aWMtcjInIiciJywnIiciJy0tZXhlY3V0ZSciJyInLCciJyInLS1hcHByb3ZhbCciJyInLCciJyInL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9iYXNlLXplcm8tc3luYy1kaWFnbm9zdGljLXIyL2Jhc2UtemVyby1zeW5jLWRpYWdub3N0aWMtcjEuZ28uanNvbiciJyInXSknCg==',validate=True)
# 爸爸，先在Python正文填本轮pins，再整体shell引用，避免repr内单引号破坏-c参数。
launcher_header,launcher_command=launcher.decode().split('exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 -c ',1)
launcher_args=shlex.split(launcher_command);assert len(launcher_args)==1
launcher_program=launcher_args[0]
for key,value in (('FIELD_STAMP',FIELD_STAMP),('FIELD_EXPECTED',FIELD_EXPECTED),('GO_EXPECTED',GO_EXPECTED),('BINDING_EXPECTED',BINDING_EXPECTED),('PLAN_EXPECTED',PLAN_EXPECTED),('PLAN_CANONICAL_SHA',PLAN_CANONICAL_SHA),('NATIVE_PREPARATION_EXPECTED',NATIVE_PREPARATION_EXPECTED)):
 marker=key+'=None';assert launcher_program.count(marker)==1
 launcher_program=launcher_program.replace(marker,key+'='+repr(value))
launcher=(launcher_header+'exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 -c '+shlex.quote(launcher_program)+'\n').encode()
motor_identity,motor_tmux=start_tmux('launch_base_zero_sync_diagnostic_r1.sh','base-zero-sync-diagnostic-trial-20261013-r2',launcher)
write_once('motor-pane.txt',(motor_identity+'\n').encode())
result={'status':'STARTED_SINGLE_BASE_ZERO_SYNC_DIAGNOSTIC_R1','tmux_identity':motor_identity,'motor_identity':motor_identity,'motor_tmux':motor_tmux,'go_sha256':hashlib.sha256(go_raw).hexdigest(),'go_bytes':len(go_raw),'plan_sha256':go['plan_sha256'],'motor_launcher_sha256':hashlib.sha256(launcher).hexdigest(),'camera_processes_started':0}
write_once('start_receipt_base_zero_sync_r1.json',(json.dumps(result,ensure_ascii=False,indent=2)+'\n').encode());print(json.dumps(result,ensure_ascii=False),flush=True)

PYREMOTE
