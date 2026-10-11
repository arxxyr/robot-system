#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYREMOTE'
import hashlib,json,os,stat
from pathlib import Path
expected={'path': '/home/loosqk/robot-checks/position-return-20261011-r12/readback-tool-r12/feetech_poke', 'sha256': '15ab06cab40f0fca7f9cba3b9c1831f2e408905c27a99e94db54c4751d9ec8d4', 'bytes': 1511200, 'mode': 448, 'device': 66305, 'inode': 17173944, 'mtime_ns': 1791689302850460700, 'ctime_ns': 1791689302878461787, 'nlink': 1, 'symlink': False}
p=Path(expected['path']);before=p.lstat();raw=p.read_bytes();after=p.lstat()
assert p==p.resolve() and stat.S_ISREG(before.st_mode) and before==after
assert raw[:4]==b'\x7fELF' and raw[4]==2 and raw[5]==1
actual={'path':str(p),'sha256':hashlib.sha256(raw).hexdigest(),'bytes':len(raw),'mode':stat.S_IMODE(after.st_mode),'device':after.st_dev,'inode':after.st_ino,'mtime_ns':after.st_mtime_ns,'ctime_ns':after.st_ctime_ns,'nlink':after.st_nlink,'symlink':p.is_symlink(),'uid':after.st_uid,'gid':after.st_gid,'elf_machine':int.from_bytes(raw[18:20],'little')}
assert all(actual[k]==v for k,v in expected.items())
assert actual['elf_machine']==183 and actual['nlink']==1 and not actual['symlink']
print(json.dumps({'status':'PASS_PURE_TOOL_STAT_ELF_IDENTITY','pid':os.getpid(),'ref':actual,'uart_operations':0,'services_actions':0,'usb_accesses':0},ensure_ascii=False),flush=True)
PYREMOTE
