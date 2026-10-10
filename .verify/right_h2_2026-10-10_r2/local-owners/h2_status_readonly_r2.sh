#!/bin/bash
# 本次持续只读状态观察；不发命令、不重启、不修改运行结果。
set -eu
exec /home/loosqk/.local/share/uv/python/cpython-3.12-linux-aarch64-gnu/bin/python3.12 - <<'PYMON'
import json, os, time
from pathlib import Path
os.nice(10)
os.sched_setaffinity(0, {0, 1, 2, 3})
p = Path('/home/loosqk/robot-checks/h2-exit-observation-20261009-r1/right-h2-run-02/run.json')
while True:
    run = json.loads(p.read_text())
    assert run['schema'] == 'P7_H2_RUN_V1' and run['side'] == 'right'
    assert run['expected_head'] == '894084738d95a7239611c2b8fbecfe081d3a7fc9'
    row = {key: run.get(key) for key in ('status', 'supervisor_pid', 'tool_pid', 'authorization_attempts', 'launch_attempts', 'tool_exit', 'tool_naturally_reaped', 'finished_ns')}
    row.update(scope='只读状态，不证明硬件通过', observed_ns=time.time_ns(), failure_count=len(run['failures']), last_failure=run['failures'][-1] if run['failures'] else None)
    print(json.dumps(row, ensure_ascii=False), flush=True)
    if type(run.get('finished_ns')) is int:
        break
    time.sleep(15)
PYMON
