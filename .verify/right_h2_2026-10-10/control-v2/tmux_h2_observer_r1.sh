#!/bin/bash
# 仅在本次现场回执和 GO 就绪后使用；SSH 等待同一 tmux pane 的真实结束。
set -eu
umask 077
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
base=/home/loosqk/robot-checks/h2-exit-observation-20261009-r1
launch="$base/bootstrap/control-v2/h2_launch_r1.sh"
record="$base/bootstrap/tmux-hardware-r1"
# 断线后显式选 1，只观察已有 pane；旧 SSH 255 原件仍保留。
case "${H2_OBSERVE_ONLY:-0}" in
  0)
    test -f "$launch"
    test ! -e "$base/right-h2-run-01"
    test ! -e "$base/bootstrap/h2-driver-r1.log"
    mkdir "$record"
    tmux new-window -d -t robot-h2-20261009-proxy-r1 -n right-h2-a96-once -P \
      -F '#{pane_id} #{pane_pid} #{window_id}' "exec /bin/bash $launch" > "$record/pane.txt"
    ;;
  1) test -s "$record/pane.txt" ;;
  *) exit 2 ;;
esac
read -r pane driver_pid window < "$record/pane.txt"
case "$pane" in %[0-9]*) ;; *) exit 2;; esac
case "$driver_pid" in ''|*[!0-9]*) exit 2;; esac
while true; do
  state=$(tmux display-message -p -t "$pane" '#{pane_id} #{pane_pid} #{pane_dead} #{pane_dead_status}')
  printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$state" >> "$record/observations.log"
  read -r observed_pane observed_pid dead status <<< "$state"
  test "$observed_pane" = "$pane"
  test "$observed_pid" = "$driver_pid"
  if [ "$dead" = 1 ]; then
    printf '%s\n' "$state" > "$record/terminal.txt"
    case "$status" in 0|1) ;; *) exit 2;; esac
    h2_python=$(uv python find 3.12)
    "$h2_python" - "$base/right-h2-run-01/run.json" "$driver_pid" "$status" <<'PY'
import json
import sys
from pathlib import Path

# 只读真实 driver 最终回执；不生成或修复运行结果。
run = json.loads(Path(sys.argv[1]).read_text())
assert run["schema"] == "P7_H2_RUN_V1" and run["side"] == "right"
assert run["expected_head"] == "a96c34ae26b56fc9e8d2deef17c3b829c3d87d0d"
assert run["supervisor_pid"] == int(sys.argv[2])
assert run["status"] in ("PASS", "FAIL") and type(run["finished_ns"]) is int
assert int(sys.argv[3]) == (0 if run["status"] == "PASS" else 1)
if run.get("tool_pid") is not None:
    assert run["tool_naturally_reaped"] is True and type(run["tool_exit"]) is int
print(json.dumps({"driver_status": run["status"], "driver_pid": run["supervisor_pid"],
                  "tool_pid": run.get("tool_pid"), "tool_naturally_reaped": run.get("tool_naturally_reaped"),
                  "finished_ns": run["finished_ns"], "tmux_exit_status": int(sys.argv[3])}))
PY
    exit "$status"
  fi
  test "$dead" = 0
  sleep 2
done
