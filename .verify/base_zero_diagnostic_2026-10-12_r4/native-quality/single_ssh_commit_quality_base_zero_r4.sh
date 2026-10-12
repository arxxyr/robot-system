#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64, hashlib, io, json, os, stat, subprocess, sys, tarfile, time
from pathlib import Path
phase = Path('/home/loosqk/robot-checks/base-zero-r4-quality')
payload = {'boot/commit_quality_once_base_zero_r4.py': {'sha256': 'a830238586ce5dcf27eca215cf4bce7d042e0c7ae667e49419dbccf6b2912874', 'bytes': 5200, 'base64': 'IiIi54i454i477yM5bqV55uY6Zu255uu5qCHUjTlh4blpIfnmoTni6znq4vljp/nlJ/ova/ku7botKjph4/mo4Dmn6XvvJvkuI3mjqXop6YgVUFSVOOAgUdP44CB5pyN5Yqh5oiW5q2j5byP5a6J6KOF44CCIiIiCgppbXBvcnQgaGFzaGxpYgppbXBvcnQgaW1wb3J0bGliLnV0aWwKaW1wb3J0IGpzb24KaW1wb3J0IG9zCmZyb20gcGF0aGxpYiBpbXBvcnQgUGF0aAppbXBvcnQgcmUKaW1wb3J0IHN1YnByb2Nlc3MKaW1wb3J0IHRhcmZpbGUKaW1wb3J0IHRpbWUKZnJvbSB0eXBlcyBpbXBvcnQgU2ltcGxlTmFtZXNwYWNlCgpCQVNFID0gUGF0aCgnL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9iYXNlLXplcm8tcjQtcXVhbGl0eScpClNPVVJDRSA9IFBhdGgoJy9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvcG9zaXRpb24tcmV0dXJuLTIwMjYxMDEwLXIxL3Bvc2l0aW9uLXJldHVybi1yNy9zb3VyY2UnKQpUQVJHRVQgPSBQYXRoKCcvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3hsZXJvYm90LXZlbG9jaXR5LWZvdW5kYXRpb24tMjAyNjEwMDgtcjcvYnVpbGQnKQpPVVQgPSBCQVNFIC8gJ3F1YWxpdHknCkhFQUQgPSAnODk0MDg0NzM4ZDk1YTcyMzk2MTFjMmI4ZmJlY2ZlMDgxZDNhN2ZjOScKT0xEID0gJzNhNGNjMTJlNWUyOGY5YzkxNjRiOGU3Y2RmOWUyODYxYjcxMjdkNzUzNmFiMjkxZTdkYjhjY2VkNmQ3YWE0ZTknCk5FVyA9ICcwNjgyNzdkODQzYjE5MWJjODI4M2JhYjM4ZTY2N2IwZjZhNGQzMGUyOTZlOGM1YTk5YTk3Y2ZlMTk2NTRmY2Y4JwpDT01QSUxFUiA9ICcwODBjYzI3MTQ2Mzg5ZGJiNWM4NzcyYzQ3NmIxOTM3YmU5YzYxMTBmJwpGSVZFID0gWydjcmF0ZXMvcm9ib3QtYWRhcHRlcnMvZmVldGVjaC9leGFtcGxlcy9mZWV0ZWNoX3Bva2UucnMnLAogICAgICAgICdjcmF0ZXMvcm9ib3QtYWRhcHRlcnMvZmVldGVjaC9zcmMvYnVzLnJzJywKICAgICAgICAnY3JhdGVzL3JvYm90LWFkYXB0ZXJzL2ZlZXRlY2gvc3JjL2J1c19yYXdfcmVhZF90ZXN0cy5ycycsCiAgICAgICAgJ2NyYXRlcy9yb2JvdC1hZGFwdGVycy9mZWV0ZWNoL3NyYy9kaWFnbm9zdGljLnJzJywKICAgICAgICAnY3JhdGVzL3JvYm90LWFkYXB0ZXJzL2ZlZXRlY2gvc3JjL3JlZ2lzdGVycy5ycyddCgoKZGVmIG1vZHVsZShuYW1lLCBwYXRoKToKICAgIHNwZWMgPSBpbXBvcnRsaWIudXRpbC5zcGVjX2Zyb21fZmlsZV9sb2NhdGlvbihuYW1lLCBwYXRoKQogICAgdmFsdWUgPSBpbXBvcnRsaWIudXRpbC5tb2R1bGVfZnJvbV9zcGVjKHNwZWMpCiAgICBzcGVjLmxvYWRlci5leGVjX21vZHVsZSh2YWx1ZSkKICAgIHJldHVybiB2YWx1ZQoKCmEgPSBtb2R1bGUoJ3IxMl9leGlzdGluZ19hcnRpZmFjdHMnLCBQYXRoKCcvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL2gyLWV4aXQtb2JzZXJ2YXRpb24tMjAyNjEwMDktcjEvc291cmNlL3NjcmlwdHMvcG9zdF9lX2gyX2FydGlmYWN0cy5weScpKQpyID0gbW9kdWxlKCdyMTJfZXhpc3RpbmdfcnVubmVyJywgUGF0aCgnL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9oMi1yaWdodC1yZWFkeS0yMDI2MTAwOS1yMS9ib290c3RyYXAvcnVuX3JlbGVhc2VfdjIucHknKSkKYXNzZXJ0IGEuc2hhMjU2KFBhdGgoci5fX2ZpbGVfXykpID09ICdkNmIyMGRhMjhkZThkNWFlMDc3YTkzY2IyZmI1YmE3ZjI3YTY2YjgxNDVkZjllN2U5MTk1MWNiMTRhYzBiMTJiJwphc3NlcnQgYS5zaGEyNTYoUGF0aChhLl9fZmlsZV9fKSkgPT0gJ2JkMmIzZjdiYjg1MjdjZDllY2VjNzM4OTQxNzE2MzhlOWNmMzc4NjU2Yjg5ZTI4NGE1ODVjM2JhYTM3Yjg3MzEnCmV4cGVjdGVkID0gYS5zdHJpY3RfanNvbihQYXRoKCcvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMS1yMTIvbmF0aXZlLXF1YWxpdHkvYm9vdC9ob3N0X3NvdXJjZV9maW5hbF9yMTIuanNvbicpLnJlYWRfdGV4dCgpKQphc3NlcnQgZXhwZWN0ZWRbJ3NvdXJjZV9tYW5pZmVzdF9zaGEyNTYnXSA9PSBORVcgYW5kIGxlbihleHBlY3RlZFsnZmlsZXMnXSkgPT0gNTk3Ck9VVC5ta2RpcigpCnJ1bm5lciA9IHIuUnVubmVyKFNpbXBsZU5hbWVzcGFjZShzb3VyY2U9U09VUkNFLCB0YXJnZXQ9VEFSR0VULCBvdXQ9T1VULCBidWlsZF9oZWFkPUhFQUQpLCBhKQpydW5uZXIuZW52WydDQVJHT19IT01FJ10gPSAnL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy92aXotb3Jpbi1hNjk1N2NjLTIwMjYwOTMwLXIxL2NhcmdvLWhvbWUnCnJ1bm5lci5zaG93blsnQ0FSR09fSE9NRSddID0gcnVubmVyLmVudlsnQ0FSR09fSE9NRSddCnJ1bm5lci5lbnZbJ1BBVEgnXSA9ICcvaG9tZS9sb29zcWsvLmNhcmdvL2JpbjovaG9tZS9sb29zcWsvLmxvY2FsL2JpbjonICsgcnVubmVyLmVudlsnUEFUSCddCnJ1bm5lci5zaG93blsnUEFUSCddID0gcnVubmVyLmVudlsnUEFUSCddCmZvciBrZXkgaW4gKCdBTExfUFJPWFknLCAnYWxsX3Byb3h5JywgJ2h0dHBfcHJveHknLCAnaHR0cHNfcHJveHknLCAnSFRUUF9QUk9YWScsICdIVFRQU19QUk9YWScsICdMRF9QUkVMT0FEJyk6CiAgICBydW5uZXIuZW52LnBvcChrZXksIE5vbmUpCnJ1bm5lci5lbnYudXBkYXRlKEhUVFBfUFJPWFk9J2h0dHA6Ly8xMC4wLjEuODA6MTA4MDgnLCBIVFRQU19QUk9YWT0naHR0cDovLzEwLjAuMS44MDoxMDgwOCcsIFBZVEhPTkRPTlRXUklURUJZVEVDT0RFPScxJykKcnVubmVyLnNob3duLnVwZGF0ZShIVFRQX1BST1hZPXJ1bm5lci5lbnZbJ0hUVFBfUFJPWFknXSwgSFRUUFNfUFJPWFk9cnVubmVyLmVudlsnSFRUUFNfUFJPWFknXSkKcmF3X3JlY2VpcHRzID0gW10KcmVzdWx0ID0geydzY2hlbWEnOiAnQkFTRV9aRVJPX1I0X0NPTU1JVF9RVUFMSVRZX1YxJywgJ3N0YXR1cyc6ICdGQUlMJywgJ3N0YXJ0ZWRfYXQnOiByLnV0YygpLAogICAgICAgICAgJ3Njb3BlJzogJ+S7o+eQhuabtOaWsG5pZ2h0bHnlkI7ku4Xlrp7pmYVmbXTihpLmoLlDbGlwcHnkuIDmrKHvvJvkuI3mtYvor5UvcmVsZWFzZS9VQVJUL0dPL+acjeWKoScsCiAgICAgICAgICAnbmF0aXZlX2dpdF9oZWFkJzogSEVBRCwgJ25hdGl2ZV93b3JrdHJlZSc6ICdmaXZlIGNoYW5nZWQgc291cmNlczsgbm90IGNsZWFuJywKICAgICAgICAgICdzb3VyY2VfYmVmb3JlX2luc3RhbGxfZXhwZWN0ZWQnOiBPTEQsICdzb3VyY2VfZXhwZWN0ZWQnOiBORVcsICdjb21waWxlcl9leHBlY3RlZCc6IENPTVBJTEVSfQoKCmRlZiB0ZXh0KG5hbWUsIGFyZ3YsIHRpbWVvdXQ9NzIwMCk6CiAgICBfLCByb3cgPSBydW5uZXIuY29tbWFuZChuYW1lLCBhcmd2LCB0aW1lb3V0KQogICAgcmV0dXJuIFBhdGgocm93WydzdGRvdXQnXSkucmVhZF90ZXh0KCkKCgpkZWYgc25hcHNob3QobGFiZWwsIGRpZ2VzdCk6CiAgICBoZWFkID0gdGV4dChsYWJlbCArICdfaGVhZCcsIFsnZ2l0JywgJ3Jldi1wYXJzZScsICdIRUFEJ10pCiAgICBzdGF0dXMgPSB0ZXh0KGxhYmVsICsgJ19zdGF0dXMnLCBbJ2dpdCcsICdzdGF0dXMnLCAnLS1wb3JjZWxhaW49djEnLCAnLS11bnRyYWNrZWQtZmlsZXM9YWxsJ10pCiAgICBuYW1lcyA9IHRleHQobGFiZWwgKyAnX2ZpbGVzJywgWydnaXQnLCAnbHMtZmlsZXMnLCAnLXonLCAnLS1jYWNoZWQnLCAnLS1vdGhlcnMnLCAnLS1leGNsdWRlLXN0YW5kYXJkJ10pCiAgICBhc3NlcnQgaGVhZC5zdHJpcCgpID09IEhFQUQgYW5kIHN0YXR1cyA9PSAnJy5qb2luKCcgTSAnICsgbmFtZSArICdcbicgZm9yIG5hbWUgaW4gc29ydGVkKEZJVkUpKQogICAgdmFsdWUgPSBhLnNuYXBzaG90X3NvdXJjZShTT1VSQ0UsIG5hbWVzLnJzdHJpcCgnXDAnKS5zcGxpdCgnXDAnKSkKICAgIGFzc2VydCBsZW4odmFsdWVbJ2ZpbGVzJ10pID09IDU5NyBhbmQgdmFsdWVbJ3NvdXJjZV9tYW5pZmVzdF9zaGEyNTYnXSA9PSBkaWdlc3QKICAgIHIuc2F2ZShPVVQgLyAobGFiZWwgKyAnLmpzb24nKSwgdmFsdWUpCiAgICByZXR1cm4gdmFsdWUKCgp0cnk6CiAgICBydW5uZXIuY29tbWFuZCgnbmlnaHRseV91cGRhdGUnLCBbJ3J1c3R1cCcsICd1cGRhdGUnLCAnbmlnaHRseScsICctLW5vLXNlbGYtdXBkYXRlJ10sIDcyMDApCiAgICBiZWZvcmUgPSBzbmFwc2hvdCgnc291cmNlX2JlZm9yZScsIE5FVykKICAgIGNvbXBpbGVyID0gYS5wYXJzZV9ydXN0Y19pZGVudGl0eSh0ZXh0KCdydXN0Y19iZWZvcmUnLCBbJ3J1c3RjJywgJy12ViddKSkKICAgIGFzc2VydCBjb21waWxlclsnY29tbWl0X2hhc2gnXSA9PSBDT01QSUxFUiBhbmQgY29tcGlsZXJbJ2hvc3QnXSA9PSAnYWFyY2g2NC11bmtub3duLWxpbnV4LWdudScKICAgIHJ1bm5lci5jb21tYW5kKCdmbXQnLCBbJ2NhcmdvJywgJ2ZtdCcsICctLWFsbCddLCAzMDApCiAgICBmbXRfYWZ0ZXIgPSBhLnNuYXBzaG90X3NvdXJjZShTT1VSQ0UsIGxpc3QoZXhwZWN0ZWRbJ2ZpbGVzJ10pKQogICAgci5zYXZlKE9VVCAvICdmbXRfYWZ0ZXIuanNvbicsIGZtdF9hZnRlcikKICAgIGFzc2VydCBmbXRfYWZ0ZXIgPT0gYmVmb3JlCiAgICBydW5uZXIuY29tbWFuZCgnY2xpcHB5JywgWydjYXJnbycsICdjbGlwcHknLCAnLS1hbGwnLCAnLS1hbGwtdGFyZ2V0cycsICctLScsICctRCcsICd3YXJuaW5ncyddLCA3MjAwKQogICAgYWZ0ZXIgPSBzbmFwc2hvdCgnc291cmNlX2FmdGVyJywgTkVXKQogICAgY29tcGlsZXJfYWZ0ZXIgPSBhLnBhcnNlX3J1c3RjX2lkZW50aXR5KHRleHQoJ3J1c3RjX2FmdGVyJywgWydydXN0YycsICctdlYnXSkpCiAgICBhc3NlcnQgYWZ0ZXIgPT0gYmVmb3JlIGFuZCBjb21waWxlcl9hZnRlciA9PSBjb21waWxlcgogICAgcmVzdWx0LnVwZGF0ZShzdGF0dXM9J1BBU1MnLCBjb21waWxlcj1jb21waWxlciwgc291cmNlX2JlZm9yZV9hZnRlcj0nUEFTUycsIGNvbXBpbGVyX2JlZm9yZV9hZnRlcj0nUEFTUycsIHRlc3RzX3J1bj0wLCByZWxlYXNlX2J1aWxkcz0wLCBVQVJUX29wZXJhdGlvbnM9MCkKZXhjZXB0IEJhc2VFeGNlcHRpb24gYXMgZXJyb3I6CiAgICByZXN1bHRbJ2Vycm9yJ10gPSByZXByKGVycm9yKQpmaW5hbGx5OgogICAgcmVzdWx0LnVwZGF0ZShmaW5pc2hlZF9hdD1yLnV0YygpLCByZWNlaXB0cz1ydW5uZXIucmVjZWlwdHMpCiAgICByLnNhdmUoT1VUIC8gJ3Jlc3VsdC5qc29uJywgcmVzdWx0KQogICAgcHJpbnQoanNvbi5kdW1wcyhyZXN1bHQsIGVuc3VyZV9hc2NpaT1GYWxzZSksIGZsdXNoPVRydWUpCnJhaXNlIFN5c3RlbUV4aXQoMCBpZiByZXN1bHRbJ3N0YXR1cyddID09ICdQQVNTJyBlbHNlIDEpCg=='}}
phase.parent.mkdir(mode=0o700, exist_ok=True)
phase.mkdir(mode=0o700, exist_ok=False)
for name, row in payload.items():
    relative = Path(name)
    assert not relative.is_absolute() and '..' not in relative.parts
    raw = base64.b64decode(row['base64'], validate=True)
    assert len(raw) == row['bytes'] and hashlib.sha256(raw).hexdigest() == row['sha256']
    path = phase / relative
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    with path.open('xb') as f: f.write(raw); f.flush(); os.fsync(f.fileno())
boot = phase / 'boot'
launcher = boot / 'launch_commit_quality_base_zero_r4.sh'
launcher.write_text('#!/bin/bash\nset -eu\nexport PYTHONDONTWRITEBYTECODE=1\ntmux set-option -p remain-on-exit on\nexec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 '+str(boot / 'commit_quality_once_base_zero_r4.py')+' > '+str(boot / 'base-zero-r4-quality-driver.log')+' 2>&1\n')
receipts = []
def run(name, argv):
    start = time.time_ns()
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    stdout, stderr = child.communicate(); code = child.wait()
    row = {'name': name, 'argv': argv, 'pid': child.pid, 'started_ns': start, 'finished_ns': time.time_ns(), 'exit_code': code, 'reaped': True, 'signals_sent': [], 'stdout': stdout.decode(), 'stderr': stderr.decode()}
    receipts.append(row)
    return row
start = run('start-software-only', ['tmux', 'new-session', '-d', '-s', 'base-zero-r4-quality-20261012-r1', '-P', '-F', '#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}', 'bash '+str(launcher)])
assert start['exit_code'] == 0 and not start['stderr']
session, window, pane, pid = start['stdout'].strip().split('|'); pid = int(pid)
deadline = time.monotonic() + 14400
terminal = None
while time.monotonic() < deadline:
    row = run('observe-owned-pane', ['tmux', 'display-message', '-p', '-t', pane, '#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}|#{pane_dead_status}'])
    assert row['exit_code'] == 0 and not row['stderr']
    fields = row['stdout'].strip().split('|')
    assert fields[:4] == [session, window, pane, str(pid)]
    if fields[4] == '1': terminal = row['stdout'].strip(); break
    time.sleep(10)
assert terminal is not None, '软件进程未自然结束，保留pane，不发信号或重试'
exit_code = int(terminal.split('|')[-1])
(boot / 'terminal.json').write_text(json.dumps({'terminal': terminal, 'driver_pid': pid, 'exit_code': exit_code, 'same_ssh_children': receipts}, ensure_ascii=False, indent=2)+'\n')
reclaim = run('reclaim-only-natural-dead-owned-pane', ['tmux', 'kill-pane', '-t', pane])
assert reclaim['exit_code'] == 0
(boot / 'reclaim.json').write_text(json.dumps(reclaim, ensure_ascii=False, indent=2)+'\n')
buffer = io.BytesIO()
with tarfile.open(fileobj=buffer, mode='w:gz') as tar:
    for path in sorted(phase.rglob('*')):
        assert not path.is_symlink()
        if path.is_file(): tar.add(path, arcname=str(path.relative_to(phase)), recursive=False)
raw = buffer.getvalue()
with tarfile.open(fileobj=io.BytesIO(raw)) as tar: count = sum(m.isfile() for m in tar)
result_path = phase / 'quality/result.json'
result = json.loads(result_path.read_text()) if result_path.exists() else None
print(json.dumps({'status': 'BASE_ZERO_R4_COMMIT_QUALITY_COLLECTED', 'driver_pid': pid, 'pane': pane, 'exit_code': exit_code, 'archive_sha256': hashlib.sha256(raw).hexdigest(), 'archive_bytes': len(raw), 'archive_files': count, 'result': result, 'archive_base64': base64.b64encode(raw).decode()}, ensure_ascii=False), flush=True)
sys.exit(exit_code)

PYREMOTE
