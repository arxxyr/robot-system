#!/bin/bash
# 爸爸，保持stdin连接tmux的PTY，只有本轮全部pins绑定后才执行；正文/只读候选入口保持。
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 -c 'import os,subprocess
FIELD_STAMP=1791885054000000000
FIELD_EXPECTED=('"'"'34b5e697c8b6f5ef50e178a8275eb281a6ab7ce637f366c77cba08e252298f16'"'"', 1836)
GO_EXPECTED=('"'"'08881ee2e558f08e14a0f1da5f20a307cb7463222eead4bd026da89aa20abe70'"'"', 1808)
BINDING_EXPECTED=('"'"'c8a380a42af7ae3a6a44ccadea7e791a5e90b2bf459272714acbfa4f646baf5c'"'"', 30882)
PLAN_EXPECTED=('"'"'c2c5e65746dd3cdae0dcee2ea90e328a45bdf1684475a7b2ab2e46f84d8192f8'"'"', 39856)
PLAN_CANONICAL_SHA='"'"'5771568b91c43d7c086a9ca204bc2263f71abc279f5a417c3c5283e7770806c4'"'"'
NATIVE_PREPARATION_EXPECTED=('"'"'17db7ae4e9ad9e63cb6e70b19031795865fb3e8059485a065e64315cb937178a'"'"', 5116)
assert type(FIELD_STAMP) is int and FIELD_STAMP>0 and all(v is not None for v in (FIELD_EXPECTED,GO_EXPECTED,BINDING_EXPECTED,PLAN_EXPECTED,NATIVE_PREPARATION_EXPECTED)) and type(PLAN_CANONICAL_SHA) is str and len(PLAN_CANONICAL_SHA)==64, '"'"'爸爸，SYNC零值launcher未绑定本轮现场/GO/软件准备，零I/O拒绝。'"'"'
log=os.open('"'"'/home/loosqk/robot-checks/base-zero-sync-diagnostic-r2/base-zero-sync-diagnostic-launch-r2/driver.log'"'"',os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
os.dup2(log,1);os.dup2(log,2);os.close(log)
subprocess.run(['"'"'tmux'"'"','"'"'set-option'"'"','"'"'-p'"'"','"'"'-t'"'"',os.environ['"'"'TMUX_PANE'"'"'],'"'"'remain-on-exit'"'"','"'"'on'"'"'],check=True)
os.execv('"'"'/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3'"'"',['"'"'/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3'"'"','"'"'/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/diagnostic-candidate/base_zero_sync_diagnostic_r1.py'"'"','"'"'--raw-helper'"'"','"'"'/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/diagnostic-candidate/fixtures/raw_right9_from_r14_r2.py'"'"','"'"'--maintenance'"'"','"'"'/home/loosqk/robot-checks/position-return-20261010-r1/software-facts-r1/scripts/run_hardware_maintenance.py'"'"','"'"'--bindings'"'"','"'"'/home/loosqk/robot-checks/base-zero-sync-diagnostic-r2/maintenance_bindings_base_zero_sync_diagnostic_r1.json'"'"','"'"'--output'"'"','"'"'/home/loosqk/robot-checks/base-zero-sync-diagnostic-r2/base-zero-sync-diagnostic-r1'"'"','"'"'--artifact-root'"'"','"'"'/home/loosqk/robot-checks/base-zero-sync-diagnostic-r2'"'"','"'"'--execute'"'"','"'"'--approval'"'"','"'"'/home/loosqk/robot-checks/base-zero-sync-diagnostic-r2/base-zero-sync-diagnostic-r1.go.json'"'"'])'
