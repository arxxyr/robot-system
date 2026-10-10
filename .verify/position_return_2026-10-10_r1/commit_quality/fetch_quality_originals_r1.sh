#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'END_PY'
# 只封本批软件原件，不带ELF/源码/共享Runner或缓存。
import base64,hashlib,json,tarfile
from pathlib import Path
base=Path('/home/loosqk/robot-checks/h2-exit-observation-20261009-r1')
archive=base/'bootstrap/position-quality-r1-originals.tar.gz'
files=sorted(p for p in (base/'position-return-commit-quality-r1').rglob('*') if p.is_file())
files+=sorted(p for p in (base/'bootstrap/position-quality-event-r1').rglob('*') if p.is_file())
files += [base/'bootstrap'/name for name in ('position-quality-r1.py','position-quality-r1-host-source.json','position-quality-r1-launch.sh','position-quality-r1-owner.log','position-quality-r1-owner.exit')]
assert len(files)==len(set(files)) and not archive.exists()
assert json.loads((base/'position-return-commit-quality-r1/result.json').read_bytes())['status']=='PASS'
for p in files:assert not p.is_symlink()
with archive.open('xb') as raw:
 with tarfile.open(fileobj=raw,mode='w:gz') as t:
  for p in files:t.add(p,arcname=str(p.relative_to(base)),recursive=False)
raw=archive.read_bytes()
print(json.dumps({'archive_path':str(archive),'sha256':hashlib.sha256(raw).hexdigest(),'bytes':len(raw),'regular_files':len(files),'original_bytes':sum(p.stat().st_size for p in files),'base64':base64.b64encode(raw).decode()},ensure_ascii=False),flush=True)
END_PY
