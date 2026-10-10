#!/bin/bash
# 候选观察副本；本文件未取得新现场回执或 GO，不得用于真实 H2。
# SSH 等待同一 pane 的退出通知，通知后仍只读核验真实终态。
set -euC
umask 077
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
base=/home/loosqk/robot-checks/h2-exit-observation-20261009-r1
launch="$base/bootstrap/control-v2/h2_launch_r1.sh"
record="$base/bootstrap/tmux-hardware-event-r1"
# 断线后显式选 1，只观察已有 pane；旧 SSH 255 原件仍保留。
case "${H2_OBSERVE_ONLY:-0}" in
  0)
    test -f "$launch"
    test ! -e "$base/right-h2-run-01"
    test ! -e "$base/bootstrap/h2-driver-r1.log"
    mkdir "$record"
    tmux new-window -d -t robot-h2-20261009-proxy-r1 -n right-h2-a96-event-once -P \
      -F '#{pane_id} #{pane_pid} #{window_id}' "exec /bin/bash $launch" > "$record/pane.txt"
    ;;
  1) test -s "$record/pane.txt" ;;
  *) exit 2 ;;
esac
read -r pane driver_pid window < "$record/pane.txt"
case "$pane" in %[0-9]*) ;; *) exit 2;; esac
case "$driver_pid" in ''|*[!0-9]*) exit 2;; esac
# 同一服务器的 pane_id 不复用；同一记录目录的 observer PID 文件不覆盖。
# 每个观察者有自己的 indexed hook/channel，续接不覆盖旧 waiter。
observer=$$
channel="h2.pane.${pane#%}.driver.$driver_pid.observer.$observer"
hook="pane-died[$observer]"
printf '%s\n' "$channel" > "$record/observer-$observer.channel"
before_hooks=$(tmux show-hooks -p -t "$pane")
printf '%s\n' "$before_hooks" > "$record/observer-$observer.hooks-before"
while read -r hook_name hook_command; do
  if [ "$hook_name" = "$hook" ]; then exit 2; fi
done <<< "$before_hooks"
cleanup_hook() {
  tmux set-hook -pu -t "$pane" "$hook"
}
trap cleanup_hook EXIT
tmux set-hook -p -t "$pane" "$hook" "wait-for -S $channel"
tmux show-hooks -p -t "$pane" > "$record/observer-$observer.hooks-after"
observe() {
  state=$(tmux display-message -p -t "$pane" '#{pane_id} #{pane_pid} #{pane_dead} #{pane_dead_status}')
  printf '%s\n' "$state" >> "$record/observer-$observer.log"
  read -r observed_pane observed_pid dead status <<< "$state"
  test "$observed_pane" = "$pane"
  test "$observed_pid" = "$driver_pid"
}
# 挂钩后查询：先退出的 pane 直接核验；查询到存活后的退出由 channel 留存通知。
observe
if [ "$dead" = 0 ]; then
  tmux wait-for "$channel"
  observe
fi
# 提前或错误通知不能变成 PASS，也不再次等待/启动。
if [ "$dead" != 1 ]; then exit 2; fi
printf '%s\n' "$state" > "$record/observer-$observer.terminal"
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
