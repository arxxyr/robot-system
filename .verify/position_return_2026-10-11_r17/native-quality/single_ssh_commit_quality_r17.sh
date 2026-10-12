#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64, hashlib, io, json, os, stat, subprocess, sys, tarfile, time
from pathlib import Path
phase = Path('/home/loosqk/robot-checks/position-return-20261011-r17/head2-reverse-commit-quality')
payload = {'boot/commit_quality_once_r17.py': {'sha256': 'ef63e3f039efa7590fee34adf660d48e6bda903246e6f8606fce106ba27f8f35', 'bytes': 5236, 'base64': 'IiIi54i454i477yMUjE3IOWktDLlj43lkJHor5XpqozmlLblsL7lkI7nmoTljp/nlJ/otKjph4/mo4Dmn6XvvJvkuI3mjqXop6YgVUFSVOOAgUdP44CB5pyN5Yqh5oiW5q2j5byP5a6J6KOF44CCIiIiCgppbXBvcnQgaGFzaGxpYgppbXBvcnQgaW1wb3J0bGliLnV0aWwKaW1wb3J0IGpzb24KaW1wb3J0IG9zCmZyb20gcGF0aGxpYiBpbXBvcnQgUGF0aAppbXBvcnQgcmUKaW1wb3J0IHN1YnByb2Nlc3MKaW1wb3J0IHRhcmZpbGUKaW1wb3J0IHRpbWUKZnJvbSB0eXBlcyBpbXBvcnQgU2ltcGxlTmFtZXNwYWNlCgpCQVNFID0gUGF0aCgnL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTEtcjE3L2hlYWQyLXJldmVyc2UtY29tbWl0LXF1YWxpdHknKQpTT1VSQ0UgPSBQYXRoKCcvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9wb3NpdGlvbi1yZXR1cm4tcjcvc291cmNlJykKVEFSR0VUID0gUGF0aCgnL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy94bGVyb2JvdC12ZWxvY2l0eS1mb3VuZGF0aW9uLTIwMjYxMDA4LXI3L2J1aWxkJykKT1VUID0gQkFTRSAvICdxdWFsaXR5JwpIRUFEID0gJzg5NDA4NDczOGQ5NWE3MjM5NjExYzJiOGZiZWNmZTA4MWQzYTdmYzknCk9MRCA9ICczYTRjYzEyZTVlMjhmOWM5MTY0YjhlN2NkZjllMjg2MWI3MTI3ZDc1MzZhYjI5MWU3ZGI4Y2NlZDZkN2FhNGU5JwpORVcgPSAnMDY4Mjc3ZDg0M2IxOTFiYzgyODNiYWIzOGU2NjdiMGY2YTRkMzBlMjk2ZThjNWE5OWE5N2NmZTE5NjU0ZmNmOCcKQ09NUElMRVIgPSAnMDgwY2MyNzE0NjM4OWRiYjVjODc3MmM0NzZiMTkzN2JlOWM2MTEwZicKRklWRSA9IFsnY3JhdGVzL3JvYm90LWFkYXB0ZXJzL2ZlZXRlY2gvZXhhbXBsZXMvZmVldGVjaF9wb2tlLnJzJywKICAgICAgICAnY3JhdGVzL3JvYm90LWFkYXB0ZXJzL2ZlZXRlY2gvc3JjL2J1cy5ycycsCiAgICAgICAgJ2NyYXRlcy9yb2JvdC1hZGFwdGVycy9mZWV0ZWNoL3NyYy9idXNfcmF3X3JlYWRfdGVzdHMucnMnLAogICAgICAgICdjcmF0ZXMvcm9ib3QtYWRhcHRlcnMvZmVldGVjaC9zcmMvZGlhZ25vc3RpYy5ycycsCiAgICAgICAgJ2NyYXRlcy9yb2JvdC1hZGFwdGVycy9mZWV0ZWNoL3NyYy9yZWdpc3RlcnMucnMnXQoKCmRlZiBtb2R1bGUobmFtZSwgcGF0aCk6CiAgICBzcGVjID0gaW1wb3J0bGliLnV0aWwuc3BlY19mcm9tX2ZpbGVfbG9jYXRpb24obmFtZSwgcGF0aCkKICAgIHZhbHVlID0gaW1wb3J0bGliLnV0aWwubW9kdWxlX2Zyb21fc3BlYyhzcGVjKQogICAgc3BlYy5sb2FkZXIuZXhlY19tb2R1bGUodmFsdWUpCiAgICByZXR1cm4gdmFsdWUKCgphID0gbW9kdWxlKCdyMTJfZXhpc3RpbmdfYXJ0aWZhY3RzJywgUGF0aCgnL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9oMi1leGl0LW9ic2VydmF0aW9uLTIwMjYxMDA5LXIxL3NvdXJjZS9zY3JpcHRzL3Bvc3RfZV9oMl9hcnRpZmFjdHMucHknKSkKciA9IG1vZHVsZSgncjEyX2V4aXN0aW5nX3J1bm5lcicsIFBhdGgoJy9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvaDItcmlnaHQtcmVhZHktMjAyNjEwMDktcjEvYm9vdHN0cmFwL3J1bl9yZWxlYXNlX3YyLnB5JykpCmFzc2VydCBhLnNoYTI1NihQYXRoKHIuX19maWxlX18pKSA9PSAnZDZiMjBkYTI4ZGU4ZDVhZTA3N2E5M2NiMmZiNWJhN2YyN2E2NmI4MTQ1ZGY5ZTdlOTE5NTFjYjE0YWMwYjEyYicKYXNzZXJ0IGEuc2hhMjU2KFBhdGgoYS5fX2ZpbGVfXykpID09ICdiZDJiM2Y3YmI4NTI3Y2Q5ZWNlYzczODk0MTcxNjM4ZTljZjM3ODY1NmI4OWUyODRhNTg1YzNiYWEzN2I4NzMxJwpleHBlY3RlZCA9IGEuc3RyaWN0X2pzb24oUGF0aCgnL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTEtcjEyL25hdGl2ZS1xdWFsaXR5L2Jvb3QvaG9zdF9zb3VyY2VfZmluYWxfcjEyLmpzb24nKS5yZWFkX3RleHQoKSkKYXNzZXJ0IGV4cGVjdGVkWydzb3VyY2VfbWFuaWZlc3Rfc2hhMjU2J10gPT0gTkVXIGFuZCBsZW4oZXhwZWN0ZWRbJ2ZpbGVzJ10pID09IDU5NwpPVVQubWtkaXIoKQpydW5uZXIgPSByLlJ1bm5lcihTaW1wbGVOYW1lc3BhY2Uoc291cmNlPVNPVVJDRSwgdGFyZ2V0PVRBUkdFVCwgb3V0PU9VVCwgYnVpbGRfaGVhZD1IRUFEKSwgYSkKcnVubmVyLmVudlsnQ0FSR09fSE9NRSddID0gJy9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3Mvdml6LW9yaW4tYTY5NTdjYy0yMDI2MDkzMC1yMS9jYXJnby1ob21lJwpydW5uZXIuc2hvd25bJ0NBUkdPX0hPTUUnXSA9IHJ1bm5lci5lbnZbJ0NBUkdPX0hPTUUnXQpydW5uZXIuZW52WydQQVRIJ10gPSAnL2hvbWUvbG9vc3FrLy5jYXJnby9iaW46L2hvbWUvbG9vc3FrLy5sb2NhbC9iaW46JyArIHJ1bm5lci5lbnZbJ1BBVEgnXQpydW5uZXIuc2hvd25bJ1BBVEgnXSA9IHJ1bm5lci5lbnZbJ1BBVEgnXQpmb3Iga2V5IGluICgnQUxMX1BST1hZJywgJ2FsbF9wcm94eScsICdodHRwX3Byb3h5JywgJ2h0dHBzX3Byb3h5JywgJ0hUVFBfUFJPWFknLCAnSFRUUFNfUFJPWFknLCAnTERfUFJFTE9BRCcpOgogICAgcnVubmVyLmVudi5wb3Aoa2V5LCBOb25lKQpydW5uZXIuZW52LnVwZGF0ZShIVFRQX1BST1hZPSdodHRwOi8vMTAuMC4xLjgwOjEwODA4JywgSFRUUFNfUFJPWFk9J2h0dHA6Ly8xMC4wLjEuODA6MTA4MDgnLCBQWVRIT05ET05UV1JJVEVCWVRFQ09ERT0nMScpCnJ1bm5lci5zaG93bi51cGRhdGUoSFRUUF9QUk9YWT1ydW5uZXIuZW52WydIVFRQX1BST1hZJ10sIEhUVFBTX1BST1hZPXJ1bm5lci5lbnZbJ0hUVFBTX1BST1hZJ10pCnJhd19yZWNlaXB0cyA9IFtdCnJlc3VsdCA9IHsnc2NoZW1hJzogJ1IxN19IRUFEMl9SRVZFUlNFX0NPTU1JVF9RVUFMSVRZX1YxJywgJ3N0YXR1cyc6ICdGQUlMJywgJ3N0YXJ0ZWRfYXQnOiByLnV0YygpLAogICAgICAgICAgJ3Njb3BlJzogJ+S7o+eQhuabtOaWsG5pZ2h0bHnlkI7ku4Xlrp7pmYVmbXTihpLmoLlDbGlwcHnkuIDmrKHvvJvkuI3mtYvor5UvcmVsZWFzZS9VQVJUL0dPL+acjeWKoScsCiAgICAgICAgICAnbmF0aXZlX2dpdF9oZWFkJzogSEVBRCwgJ25hdGl2ZV93b3JrdHJlZSc6ICdmaXZlIGNoYW5nZWQgc291cmNlczsgbm90IGNsZWFuJywKICAgICAgICAgICdzb3VyY2VfYmVmb3JlX2luc3RhbGxfZXhwZWN0ZWQnOiBPTEQsICdzb3VyY2VfZXhwZWN0ZWQnOiBORVcsICdjb21waWxlcl9leHBlY3RlZCc6IENPTVBJTEVSfQoKCmRlZiB0ZXh0KG5hbWUsIGFyZ3YsIHRpbWVvdXQ9NzIwMCk6CiAgICBfLCByb3cgPSBydW5uZXIuY29tbWFuZChuYW1lLCBhcmd2LCB0aW1lb3V0KQogICAgcmV0dXJuIFBhdGgocm93WydzdGRvdXQnXSkucmVhZF90ZXh0KCkKCgpkZWYgc25hcHNob3QobGFiZWwsIGRpZ2VzdCk6CiAgICBoZWFkID0gdGV4dChsYWJlbCArICdfaGVhZCcsIFsnZ2l0JywgJ3Jldi1wYXJzZScsICdIRUFEJ10pCiAgICBzdGF0dXMgPSB0ZXh0KGxhYmVsICsgJ19zdGF0dXMnLCBbJ2dpdCcsICdzdGF0dXMnLCAnLS1wb3JjZWxhaW49djEnLCAnLS11bnRyYWNrZWQtZmlsZXM9YWxsJ10pCiAgICBuYW1lcyA9IHRleHQobGFiZWwgKyAnX2ZpbGVzJywgWydnaXQnLCAnbHMtZmlsZXMnLCAnLXonLCAnLS1jYWNoZWQnLCAnLS1vdGhlcnMnLCAnLS1leGNsdWRlLXN0YW5kYXJkJ10pCiAgICBhc3NlcnQgaGVhZC5zdHJpcCgpID09IEhFQUQgYW5kIHN0YXR1cyA9PSAnJy5qb2luKCcgTSAnICsgbmFtZSArICdcbicgZm9yIG5hbWUgaW4gc29ydGVkKEZJVkUpKQogICAgdmFsdWUgPSBhLnNuYXBzaG90X3NvdXJjZShTT1VSQ0UsIG5hbWVzLnJzdHJpcCgnXDAnKS5zcGxpdCgnXDAnKSkKICAgIGFzc2VydCBsZW4odmFsdWVbJ2ZpbGVzJ10pID09IDU5NyBhbmQgdmFsdWVbJ3NvdXJjZV9tYW5pZmVzdF9zaGEyNTYnXSA9PSBkaWdlc3QKICAgIHIuc2F2ZShPVVQgLyAobGFiZWwgKyAnLmpzb24nKSwgdmFsdWUpCiAgICByZXR1cm4gdmFsdWUKCgp0cnk6CiAgICBydW5uZXIuY29tbWFuZCgnbmlnaHRseV91cGRhdGUnLCBbJ3J1c3R1cCcsICd1cGRhdGUnLCAnbmlnaHRseScsICctLW5vLXNlbGYtdXBkYXRlJ10sIDcyMDApCiAgICBiZWZvcmUgPSBzbmFwc2hvdCgnc291cmNlX2JlZm9yZScsIE5FVykKICAgIGNvbXBpbGVyID0gYS5wYXJzZV9ydXN0Y19pZGVudGl0eSh0ZXh0KCdydXN0Y19iZWZvcmUnLCBbJ3J1c3RjJywgJy12ViddKSkKICAgIGFzc2VydCBjb21waWxlclsnY29tbWl0X2hhc2gnXSA9PSBDT01QSUxFUiBhbmQgY29tcGlsZXJbJ2hvc3QnXSA9PSAnYWFyY2g2NC11bmtub3duLWxpbnV4LWdudScKICAgIHJ1bm5lci5jb21tYW5kKCdmbXQnLCBbJ2NhcmdvJywgJ2ZtdCcsICctLWFsbCddLCAzMDApCiAgICBmbXRfYWZ0ZXIgPSBhLnNuYXBzaG90X3NvdXJjZShTT1VSQ0UsIGxpc3QoZXhwZWN0ZWRbJ2ZpbGVzJ10pKQogICAgci5zYXZlKE9VVCAvICdmbXRfYWZ0ZXIuanNvbicsIGZtdF9hZnRlcikKICAgIGFzc2VydCBmbXRfYWZ0ZXIgPT0gYmVmb3JlCiAgICBydW5uZXIuY29tbWFuZCgnY2xpcHB5JywgWydjYXJnbycsICdjbGlwcHknLCAnLS1hbGwnLCAnLS1hbGwtdGFyZ2V0cycsICctLScsICctRCcsICd3YXJuaW5ncyddLCA3MjAwKQogICAgYWZ0ZXIgPSBzbmFwc2hvdCgnc291cmNlX2FmdGVyJywgTkVXKQogICAgY29tcGlsZXJfYWZ0ZXIgPSBhLnBhcnNlX3J1c3RjX2lkZW50aXR5KHRleHQoJ3J1c3RjX2FmdGVyJywgWydydXN0YycsICctdlYnXSkpCiAgICBhc3NlcnQgYWZ0ZXIgPT0gYmVmb3JlIGFuZCBjb21waWxlcl9hZnRlciA9PSBjb21waWxlcgogICAgcmVzdWx0LnVwZGF0ZShzdGF0dXM9J1BBU1MnLCBjb21waWxlcj1jb21waWxlciwgc291cmNlX2JlZm9yZV9hZnRlcj0nUEFTUycsIGNvbXBpbGVyX2JlZm9yZV9hZnRlcj0nUEFTUycsIHRlc3RzX3J1bj0wLCByZWxlYXNlX2J1aWxkcz0wLCBVQVJUX29wZXJhdGlvbnM9MCkKZXhjZXB0IEJhc2VFeGNlcHRpb24gYXMgZXJyb3I6CiAgICByZXN1bHRbJ2Vycm9yJ10gPSByZXByKGVycm9yKQpmaW5hbGx5OgogICAgcmVzdWx0LnVwZGF0ZShmaW5pc2hlZF9hdD1yLnV0YygpLCByZWNlaXB0cz1ydW5uZXIucmVjZWlwdHMpCiAgICByLnNhdmUoT1VUIC8gJ3Jlc3VsdC5qc29uJywgcmVzdWx0KQogICAgcHJpbnQoanNvbi5kdW1wcyhyZXN1bHQsIGVuc3VyZV9hc2NpaT1GYWxzZSksIGZsdXNoPVRydWUpCnJhaXNlIFN5c3RlbUV4aXQoMCBpZiByZXN1bHRbJ3N0YXR1cyddID09ICdQQVNTJyBlbHNlIDEpCg=='}}
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
launcher = boot / 'launch_commit_quality_r17.sh'
launcher.write_text('#!/bin/bash\nset -eu\nexport PYTHONDONTWRITEBYTECODE=1\ntmux set-option -p remain-on-exit on\nexec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 '+str(boot / 'commit_quality_once_r17.py')+' > '+str(boot / 'driver.log')+' 2>&1\n')
receipts = []
def run(name, argv):
    start = time.time_ns()
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    stdout, stderr = child.communicate(); code = child.wait()
    row = {'name': name, 'argv': argv, 'pid': child.pid, 'started_ns': start, 'finished_ns': time.time_ns(), 'exit_code': code, 'reaped': True, 'signals_sent': [], 'stdout': stdout.decode(), 'stderr': stderr.decode()}
    receipts.append(row)
    return row
start = run('start-software-only', ['tmux', 'new-session', '-d', '-s', 'head2-reverse-commit-quality-20261011-r17', '-P', '-F', '#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}', 'bash '+str(launcher)])
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
print(json.dumps({'status': 'R17_COMMIT_QUALITY_COLLECTED', 'driver_pid': pid, 'pane': pane, 'exit_code': exit_code, 'archive_sha256': hashlib.sha256(raw).hexdigest(), 'archive_bytes': len(raw), 'archive_files': count, 'result': result, 'archive_base64': base64.b64encode(raw).decode()}, ensure_ascii=False), flush=True)
sys.exit(exit_code)

PYREMOTE
