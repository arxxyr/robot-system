#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import base64, hashlib, io, json, os, stat, subprocess, sys, tarfile, time
from pathlib import Path
phase = Path('/home/loosqk/robot-checks/base-zero-r4-run-quality')
payload = {'boot/commit_quality_once_base_zero_r4.py': {'sha256': 'deff2d57ed9982b5184bd70fbb5e6eea8c652c5d71a5b68cb040580efdb59ac3', 'bytes': 5204, 'base64': 'IiIi54i454i477yM5bqV55uY6Zu255uu5qCHUjTlh4blpIfnmoTni6znq4vljp/nlJ/ova/ku7botKjph4/mo4Dmn6XvvJvkuI3mjqXop6YgVUFSVOOAgUdP44CB5pyN5Yqh5oiW5q2j5byP5a6J6KOF44CCIiIiCgppbXBvcnQgaGFzaGxpYgppbXBvcnQgaW1wb3J0bGliLnV0aWwKaW1wb3J0IGpzb24KaW1wb3J0IG9zCmZyb20gcGF0aGxpYiBpbXBvcnQgUGF0aAppbXBvcnQgcmUKaW1wb3J0IHN1YnByb2Nlc3MKaW1wb3J0IHRhcmZpbGUKaW1wb3J0IHRpbWUKZnJvbSB0eXBlcyBpbXBvcnQgU2ltcGxlTmFtZXNwYWNlCgpCQVNFID0gUGF0aCgnL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9iYXNlLXplcm8tcjQtcnVuLXF1YWxpdHknKQpTT1VSQ0UgPSBQYXRoKCcvaG9tZS9sb29zcWsvcm9ib3QtY2hlY2tzL3Bvc2l0aW9uLXJldHVybi0yMDI2MTAxMC1yMS9wb3NpdGlvbi1yZXR1cm4tcjcvc291cmNlJykKVEFSR0VUID0gUGF0aCgnL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy94bGVyb2JvdC12ZWxvY2l0eS1mb3VuZGF0aW9uLTIwMjYxMDA4LXI3L2J1aWxkJykKT1VUID0gQkFTRSAvICdxdWFsaXR5JwpIRUFEID0gJzg5NDA4NDczOGQ5NWE3MjM5NjExYzJiOGZiZWNmZTA4MWQzYTdmYzknCk9MRCA9ICczYTRjYzEyZTVlMjhmOWM5MTY0YjhlN2NkZjllMjg2MWI3MTI3ZDc1MzZhYjI5MWU3ZGI4Y2NlZDZkN2FhNGU5JwpORVcgPSAnMDY4Mjc3ZDg0M2IxOTFiYzgyODNiYWIzOGU2NjdiMGY2YTRkMzBlMjk2ZThjNWE5OWE5N2NmZTE5NjU0ZmNmOCcKQ09NUElMRVIgPSAnMDgwY2MyNzE0NjM4OWRiYjVjODc3MmM0NzZiMTkzN2JlOWM2MTEwZicKRklWRSA9IFsnY3JhdGVzL3JvYm90LWFkYXB0ZXJzL2ZlZXRlY2gvZXhhbXBsZXMvZmVldGVjaF9wb2tlLnJzJywKICAgICAgICAnY3JhdGVzL3JvYm90LWFkYXB0ZXJzL2ZlZXRlY2gvc3JjL2J1cy5ycycsCiAgICAgICAgJ2NyYXRlcy9yb2JvdC1hZGFwdGVycy9mZWV0ZWNoL3NyYy9idXNfcmF3X3JlYWRfdGVzdHMucnMnLAogICAgICAgICdjcmF0ZXMvcm9ib3QtYWRhcHRlcnMvZmVldGVjaC9zcmMvZGlhZ25vc3RpYy5ycycsCiAgICAgICAgJ2NyYXRlcy9yb2JvdC1hZGFwdGVycy9mZWV0ZWNoL3NyYy9yZWdpc3RlcnMucnMnXQoKCmRlZiBtb2R1bGUobmFtZSwgcGF0aCk6CiAgICBzcGVjID0gaW1wb3J0bGliLnV0aWwuc3BlY19mcm9tX2ZpbGVfbG9jYXRpb24obmFtZSwgcGF0aCkKICAgIHZhbHVlID0gaW1wb3J0bGliLnV0aWwubW9kdWxlX2Zyb21fc3BlYyhzcGVjKQogICAgc3BlYy5sb2FkZXIuZXhlY19tb2R1bGUodmFsdWUpCiAgICByZXR1cm4gdmFsdWUKCgphID0gbW9kdWxlKCdyMTJfZXhpc3RpbmdfYXJ0aWZhY3RzJywgUGF0aCgnL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9oMi1leGl0LW9ic2VydmF0aW9uLTIwMjYxMDA5LXIxL3NvdXJjZS9zY3JpcHRzL3Bvc3RfZV9oMl9hcnRpZmFjdHMucHknKSkKciA9IG1vZHVsZSgncjEyX2V4aXN0aW5nX3J1bm5lcicsIFBhdGgoJy9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3MvaDItcmlnaHQtcmVhZHktMjAyNjEwMDktcjEvYm9vdHN0cmFwL3J1bl9yZWxlYXNlX3YyLnB5JykpCmFzc2VydCBhLnNoYTI1NihQYXRoKHIuX19maWxlX18pKSA9PSAnZDZiMjBkYTI4ZGU4ZDVhZTA3N2E5M2NiMmZiNWJhN2YyN2E2NmI4MTQ1ZGY5ZTdlOTE5NTFjYjE0YWMwYjEyYicKYXNzZXJ0IGEuc2hhMjU2KFBhdGgoYS5fX2ZpbGVfXykpID09ICdiZDJiM2Y3YmI4NTI3Y2Q5ZWNlYzczODk0MTcxNjM4ZTljZjM3ODY1NmI4OWUyODRhNTg1YzNiYWEzN2I4NzMxJwpleHBlY3RlZCA9IGEuc3RyaWN0X2pzb24oUGF0aCgnL2hvbWUvbG9vc3FrL3JvYm90LWNoZWNrcy9wb3NpdGlvbi1yZXR1cm4tMjAyNjEwMTEtcjEyL25hdGl2ZS1xdWFsaXR5L2Jvb3QvaG9zdF9zb3VyY2VfZmluYWxfcjEyLmpzb24nKS5yZWFkX3RleHQoKSkKYXNzZXJ0IGV4cGVjdGVkWydzb3VyY2VfbWFuaWZlc3Rfc2hhMjU2J10gPT0gTkVXIGFuZCBsZW4oZXhwZWN0ZWRbJ2ZpbGVzJ10pID09IDU5NwpPVVQubWtkaXIoKQpydW5uZXIgPSByLlJ1bm5lcihTaW1wbGVOYW1lc3BhY2Uoc291cmNlPVNPVVJDRSwgdGFyZ2V0PVRBUkdFVCwgb3V0PU9VVCwgYnVpbGRfaGVhZD1IRUFEKSwgYSkKcnVubmVyLmVudlsnQ0FSR09fSE9NRSddID0gJy9ob21lL2xvb3Nxay9yb2JvdC1jaGVja3Mvdml6LW9yaW4tYTY5NTdjYy0yMDI2MDkzMC1yMS9jYXJnby1ob21lJwpydW5uZXIuc2hvd25bJ0NBUkdPX0hPTUUnXSA9IHJ1bm5lci5lbnZbJ0NBUkdPX0hPTUUnXQpydW5uZXIuZW52WydQQVRIJ10gPSAnL2hvbWUvbG9vc3FrLy5jYXJnby9iaW46L2hvbWUvbG9vc3FrLy5sb2NhbC9iaW46JyArIHJ1bm5lci5lbnZbJ1BBVEgnXQpydW5uZXIuc2hvd25bJ1BBVEgnXSA9IHJ1bm5lci5lbnZbJ1BBVEgnXQpmb3Iga2V5IGluICgnQUxMX1BST1hZJywgJ2FsbF9wcm94eScsICdodHRwX3Byb3h5JywgJ2h0dHBzX3Byb3h5JywgJ0hUVFBfUFJPWFknLCAnSFRUUFNfUFJPWFknLCAnTERfUFJFTE9BRCcpOgogICAgcnVubmVyLmVudi5wb3Aoa2V5LCBOb25lKQpydW5uZXIuZW52LnVwZGF0ZShIVFRQX1BST1hZPSdodHRwOi8vMTAuMC4xLjgwOjEwODA4JywgSFRUUFNfUFJPWFk9J2h0dHA6Ly8xMC4wLjEuODA6MTA4MDgnLCBQWVRIT05ET05UV1JJVEVCWVRFQ09ERT0nMScpCnJ1bm5lci5zaG93bi51cGRhdGUoSFRUUF9QUk9YWT1ydW5uZXIuZW52WydIVFRQX1BST1hZJ10sIEhUVFBTX1BST1hZPXJ1bm5lci5lbnZbJ0hUVFBTX1BST1hZJ10pCnJhd19yZWNlaXB0cyA9IFtdCnJlc3VsdCA9IHsnc2NoZW1hJzogJ0JBU0VfWkVST19SNF9DT01NSVRfUVVBTElUWV9WMScsICdzdGF0dXMnOiAnRkFJTCcsICdzdGFydGVkX2F0Jzogci51dGMoKSwKICAgICAgICAgICdzY29wZSc6ICfku6PnkIbmm7TmlrBuaWdodGx55ZCO5LuF5a6e6ZmFZm104oaS5qC5Q2xpcHB55LiA5qyh77yb5LiN5rWL6K+VL3JlbGVhc2UvVUFSVC9HTy/mnI3liqEnLAogICAgICAgICAgJ25hdGl2ZV9naXRfaGVhZCc6IEhFQUQsICduYXRpdmVfd29ya3RyZWUnOiAnZml2ZSBjaGFuZ2VkIHNvdXJjZXM7IG5vdCBjbGVhbicsCiAgICAgICAgICAnc291cmNlX2JlZm9yZV9pbnN0YWxsX2V4cGVjdGVkJzogT0xELCAnc291cmNlX2V4cGVjdGVkJzogTkVXLCAnY29tcGlsZXJfZXhwZWN0ZWQnOiBDT01QSUxFUn0KCgpkZWYgdGV4dChuYW1lLCBhcmd2LCB0aW1lb3V0PTcyMDApOgogICAgXywgcm93ID0gcnVubmVyLmNvbW1hbmQobmFtZSwgYXJndiwgdGltZW91dCkKICAgIHJldHVybiBQYXRoKHJvd1snc3Rkb3V0J10pLnJlYWRfdGV4dCgpCgoKZGVmIHNuYXBzaG90KGxhYmVsLCBkaWdlc3QpOgogICAgaGVhZCA9IHRleHQobGFiZWwgKyAnX2hlYWQnLCBbJ2dpdCcsICdyZXYtcGFyc2UnLCAnSEVBRCddKQogICAgc3RhdHVzID0gdGV4dChsYWJlbCArICdfc3RhdHVzJywgWydnaXQnLCAnc3RhdHVzJywgJy0tcG9yY2VsYWluPXYxJywgJy0tdW50cmFja2VkLWZpbGVzPWFsbCddKQogICAgbmFtZXMgPSB0ZXh0KGxhYmVsICsgJ19maWxlcycsIFsnZ2l0JywgJ2xzLWZpbGVzJywgJy16JywgJy0tY2FjaGVkJywgJy0tb3RoZXJzJywgJy0tZXhjbHVkZS1zdGFuZGFyZCddKQogICAgYXNzZXJ0IGhlYWQuc3RyaXAoKSA9PSBIRUFEIGFuZCBzdGF0dXMgPT0gJycuam9pbignIE0gJyArIG5hbWUgKyAnXG4nIGZvciBuYW1lIGluIHNvcnRlZChGSVZFKSkKICAgIHZhbHVlID0gYS5zbmFwc2hvdF9zb3VyY2UoU09VUkNFLCBuYW1lcy5yc3RyaXAoJ1wwJykuc3BsaXQoJ1wwJykpCiAgICBhc3NlcnQgbGVuKHZhbHVlWydmaWxlcyddKSA9PSA1OTcgYW5kIHZhbHVlWydzb3VyY2VfbWFuaWZlc3Rfc2hhMjU2J10gPT0gZGlnZXN0CiAgICByLnNhdmUoT1VUIC8gKGxhYmVsICsgJy5qc29uJyksIHZhbHVlKQogICAgcmV0dXJuIHZhbHVlCgoKdHJ5OgogICAgcnVubmVyLmNvbW1hbmQoJ25pZ2h0bHlfdXBkYXRlJywgWydydXN0dXAnLCAndXBkYXRlJywgJ25pZ2h0bHknLCAnLS1uby1zZWxmLXVwZGF0ZSddLCA3MjAwKQogICAgYmVmb3JlID0gc25hcHNob3QoJ3NvdXJjZV9iZWZvcmUnLCBORVcpCiAgICBjb21waWxlciA9IGEucGFyc2VfcnVzdGNfaWRlbnRpdHkodGV4dCgncnVzdGNfYmVmb3JlJywgWydydXN0YycsICctdlYnXSkpCiAgICBhc3NlcnQgY29tcGlsZXJbJ2NvbW1pdF9oYXNoJ10gPT0gQ09NUElMRVIgYW5kIGNvbXBpbGVyWydob3N0J10gPT0gJ2FhcmNoNjQtdW5rbm93bi1saW51eC1nbnUnCiAgICBydW5uZXIuY29tbWFuZCgnZm10JywgWydjYXJnbycsICdmbXQnLCAnLS1hbGwnXSwgMzAwKQogICAgZm10X2FmdGVyID0gYS5zbmFwc2hvdF9zb3VyY2UoU09VUkNFLCBsaXN0KGV4cGVjdGVkWydmaWxlcyddKSkKICAgIHIuc2F2ZShPVVQgLyAnZm10X2FmdGVyLmpzb24nLCBmbXRfYWZ0ZXIpCiAgICBhc3NlcnQgZm10X2FmdGVyID09IGJlZm9yZQogICAgcnVubmVyLmNvbW1hbmQoJ2NsaXBweScsIFsnY2FyZ28nLCAnY2xpcHB5JywgJy0tYWxsJywgJy0tYWxsLXRhcmdldHMnLCAnLS0nLCAnLUQnLCAnd2FybmluZ3MnXSwgNzIwMCkKICAgIGFmdGVyID0gc25hcHNob3QoJ3NvdXJjZV9hZnRlcicsIE5FVykKICAgIGNvbXBpbGVyX2FmdGVyID0gYS5wYXJzZV9ydXN0Y19pZGVudGl0eSh0ZXh0KCdydXN0Y19hZnRlcicsIFsncnVzdGMnLCAnLXZWJ10pKQogICAgYXNzZXJ0IGFmdGVyID09IGJlZm9yZSBhbmQgY29tcGlsZXJfYWZ0ZXIgPT0gY29tcGlsZXIKICAgIHJlc3VsdC51cGRhdGUoc3RhdHVzPSdQQVNTJywgY29tcGlsZXI9Y29tcGlsZXIsIHNvdXJjZV9iZWZvcmVfYWZ0ZXI9J1BBU1MnLCBjb21waWxlcl9iZWZvcmVfYWZ0ZXI9J1BBU1MnLCB0ZXN0c19ydW49MCwgcmVsZWFzZV9idWlsZHM9MCwgVUFSVF9vcGVyYXRpb25zPTApCmV4Y2VwdCBCYXNlRXhjZXB0aW9uIGFzIGVycm9yOgogICAgcmVzdWx0WydlcnJvciddID0gcmVwcihlcnJvcikKZmluYWxseToKICAgIHJlc3VsdC51cGRhdGUoZmluaXNoZWRfYXQ9ci51dGMoKSwgcmVjZWlwdHM9cnVubmVyLnJlY2VpcHRzKQogICAgci5zYXZlKE9VVCAvICdyZXN1bHQuanNvbicsIHJlc3VsdCkKICAgIHByaW50KGpzb24uZHVtcHMocmVzdWx0LCBlbnN1cmVfYXNjaWk9RmFsc2UpLCBmbHVzaD1UcnVlKQpyYWlzZSBTeXN0ZW1FeGl0KDAgaWYgcmVzdWx0WydzdGF0dXMnXSA9PSAnUEFTUycgZWxzZSAxKQo='}}
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
launcher.write_text('#!/bin/bash\nset -eu\nexport PYTHONDONTWRITEBYTECODE=1\ntmux set-option -p remain-on-exit on\nexec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 '+str(boot / 'commit_quality_once_base_zero_r4.py')+' > '+str(boot / 'base-zero-r4-run-quality-driver.log')+' 2>&1\n')
receipts = []
def run(name, argv):
    start = time.time_ns()
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    stdout, stderr = child.communicate(); code = child.wait()
    row = {'name': name, 'argv': argv, 'pid': child.pid, 'started_ns': start, 'finished_ns': time.time_ns(), 'exit_code': code, 'reaped': True, 'signals_sent': [], 'stdout': stdout.decode(), 'stderr': stderr.decode()}
    receipts.append(row)
    return row
start = run('start-software-only', ['tmux', 'new-session', '-d', '-s', 'base-zero-r4-run-quality-20261012-r1', '-P', '-F', '#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}', 'bash '+str(launcher)])
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
