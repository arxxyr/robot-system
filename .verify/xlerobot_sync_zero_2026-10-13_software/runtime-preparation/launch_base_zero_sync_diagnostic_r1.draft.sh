#!/bin/bash
# 爸爸，保持stdin连接tmux的PTY，只有本轮全部pins绑定后才执行；正文/只读候选入口保持。
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 -c 'import os,subprocess
FIELD_STAMP=None
FIELD_EXPECTED=None
GO_EXPECTED=None
BINDING_EXPECTED=None
PLAN_EXPECTED=None
PLAN_CANONICAL_SHA=None
NATIVE_PREPARATION_EXPECTED=None
assert type(FIELD_STAMP) is int and FIELD_STAMP>0 and all(v is not None for v in (FIELD_EXPECTED,GO_EXPECTED,BINDING_EXPECTED,PLAN_EXPECTED,NATIVE_PREPARATION_EXPECTED)) and type(PLAN_CANONICAL_SHA) is str and len(PLAN_CANONICAL_SHA)==64, '"'"'爸爸，SYNC零值launcher未绑定本轮现场/GO/软件准备，零I/O拒绝。'"'"'
log=os.open('"'"'/home/loosqk/robot-checks/base-zero-sync-diagnostic-r1/base-zero-sync-diagnostic-launch-r1/driver.log'"'"',os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
os.dup2(log,1);os.dup2(log,2);os.close(log)
subprocess.run(['"'"'tmux'"'"','"'"'set-option'"'"','"'"'-p'"'"','"'"'-t'"'"',os.environ['"'"'TMUX_PANE'"'"'],'"'"'remain-on-exit'"'"','"'"'on'"'"'],check=True)
os.execv('"'"'/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3'"'"',['"'"'/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3'"'"','"'"'/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/diagnostic-candidate/base_zero_sync_diagnostic_r1.py'"'"','"'"'--raw-helper'"'"','"'"'/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/diagnostic-candidate/fixtures/raw_right9_from_r14_r2.py'"'"','"'"'--maintenance'"'"','"'"'/home/loosqk/robot-checks/position-return-20261010-r1/software-facts-r1/scripts/run_hardware_maintenance.py'"'"','"'"'--bindings'"'"','"'"'/home/loosqk/robot-checks/base-zero-sync-diagnostic-r1/maintenance_bindings_base_zero_sync_diagnostic_r1.json'"'"','"'"'--output'"'"','"'"'/home/loosqk/robot-checks/base-zero-sync-diagnostic-r1/base-zero-sync-diagnostic-r1'"'"','"'"'--artifact-root'"'"','"'"'/home/loosqk/robot-checks/base-zero-sync-diagnostic-r1'"'"','"'"'--execute'"'"','"'"'--approval'"'"','"'"'/home/loosqk/robot-checks/base-zero-sync-diagnostic-r1/base-zero-sync-diagnostic-r1.go.json'"'"'])'
