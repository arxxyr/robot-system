#!/bin/bash
set -eu
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export PYTHONDONTWRITEBYTECODE=1
quality_base=/home/loosqk/robot-checks/h2-right-r2-commit-quality-20261010-r1
tmux display-message -p -t h2-right-r2-quality-20261010-r1:nightly-quality-once '#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}|#{pane_dead_status}' > "$quality_base/bootstrap/tmux-terminal-identity.txt"
quality_python=$(uv python find 3.12)
exec "$quality_python" - <<'PYREMOTE'
from pathlib import Path
import hashlib,json,shutil,subprocess
base=Path('/home/loosqk/robot-checks/h2-right-r2-commit-quality-20261010-r1')
terminal=(base/'bootstrap/tmux-terminal-identity.txt').read_text().strip().split('|')
if terminal!=['h2-right-r2-quality-20261010-r1','@40','%40','614935','1','0']:raise ValueError('自有pane原身份或自然终态不符')
result=json.loads((base/'quality-r1/result.json').read_bytes())
if result['status']!='PASS' or (base/'bootstrap/quality-owner.exit').read_text()!='0\n':raise ValueError('质量终态不通过')
owner=json.loads((base/'quality-r1/owner.json').read_bytes())
if (Path('/proc')/str(owner['pid'])).exists():raise ValueError('质量owner PID仍存在，拒绝当作回收')
basis=base/'basis';basis.mkdir()
for path in [Path('/home/loosqk/robot-checks/h2-right-ready-20261009-r1/bootstrap/run_release_v2.py'),Path('/home/loosqk/robot-checks/h2-exit-observation-20261009-r1/source/scripts/post_e_h2_artifacts.py')]:shutil.copyfile(path,basis/path.name)
metadata={'terminal':terminal,'owner':owner,'owner_pid_absent':True,'receipts':len(result['receipts'])}
(base/'bootstrap/terminal-verification.json').write_text(json.dumps(metadata,ensure_ascii=False,indent=2)+'\n')
archive=base/'native-quality-originals-r1.tar.gz'
if archive.exists():raise ValueError('归包不得覆盖')
subprocess.run(['tar','-czf',str(archive),'-C',str(base),'quality-r1','bootstrap','basis'],check=True)
raw=archive.read_bytes()
print(json.dumps({'archive':str(archive),'sha256':hashlib.sha256(raw).hexdigest(),'bytes':len(raw),**metadata},ensure_ascii=False))
PYREMOTE
