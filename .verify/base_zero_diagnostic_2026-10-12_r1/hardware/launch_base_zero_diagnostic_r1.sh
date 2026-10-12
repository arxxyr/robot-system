#!/bin/bash
# 爸爸，独立launcher也必须由已核starter填全部本轮pins；纯草稿零I/O拒绝。
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - <<'PYLAUNCH'
import os,subprocess
FIELD_STAMP=1791779943356024000
FIELD_EXPECTED=('1f50e2702a78732bf18f887469a4ed9c1e5d86d57cd38097bbbb6e4ca91f5c97', 1541)
GO_EXPECTED=('c38354601430df6f3c7c546ea880a88de39f43fc2134c108f22b91a5bef84235', 1732)
BINDING_EXPECTED=('60c1ce479a89e198bc237cc3c29ccf3d9b9cd81434c47026b9cf2b5c1dd03e1c', 30051)
PLAN_EXPECTED=('1d049b491e69d0756e074cee388beaf814edb8e95013fdfce1ecc9d8d4ee0555', 38231)
PLAN_CANONICAL_SHA='37bb533e461a88cfafebdef966e601a1f21b8d67c04ea6b03e7852d6b21f9323'
NATIVE_PREPARATION_EXPECTED=('7cb595efc356e73626a558c6b83857427eee59c878a8e33c1d9a85875e451160', 9904)
assert type(FIELD_STAMP) is int and FIELD_STAMP>0 and all(v is not None for v in (FIELD_EXPECTED,GO_EXPECTED,BINDING_EXPECTED,PLAN_EXPECTED,NATIVE_PREPARATION_EXPECTED)) and type(PLAN_CANONICAL_SHA) is str and len(PLAN_CANONICAL_SHA)==64, '爸爸，零目标launcher未绑定本轮现场/GO/软件准备，零I/O拒绝。'
log=os.open('/home/loosqk/robot-checks/position-return-20261010-r1/base-zero-diagnostic-r1/base-zero-diagnostic-launch-r1/driver.log',os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
os.dup2(log,1);os.dup2(log,2);os.close(log)
subprocess.run(['tmux','set-option','-w','-t',os.environ['TMUX_PANE'],'remain-on-exit','on'],check=True)
os.execv('/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3',['/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3','/home/loosqk/robot-checks/position-return-20261010-r1/base-zero-diagnostic-r1/base_zero_diagnostic_r1.py','--raw-helper','/home/loosqk/robot-checks/position-return-20261010-r1/base-zero-diagnostic-r1/helpers/raw_right9_from_r14_r1.py','--maintenance','/home/loosqk/robot-checks/position-return-20261010-r1/software-facts-r1/scripts/run_hardware_maintenance.py','--bindings','/home/loosqk/robot-checks/position-return-20261010-r1/base-zero-diagnostic-r1/maintenance_bindings_base_zero_diagnostic_r1.json','--output','/home/loosqk/robot-checks/position-return-20261010-r1/base-zero-diagnostic-r1/base-zero-diagnostic-r1','--artifact-root','/home/loosqk/robot-checks/position-return-20261010-r1','--execute','--approval','/home/loosqk/robot-checks/position-return-20261010-r1/base-zero-diagnostic-r1/base-zero-diagnostic-r1.go.json'])
PYLAUNCH
