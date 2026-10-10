#!/bin/bash
# 本轮软件质量观察副本；无UART/GO/服务动作。
# SSH 等待同一 pane 的退出通知，通知后仍只读核验真实终态。
set -euC
umask 077
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
base=/home/loosqk/robot-checks/h2-exit-observation-20261009-r1
record="$base/bootstrap/position-quality-event-r1"
mkdir "$record"
printf '%s\n' '%43 616124 @43' > "$record/pane.txt"
read -r pane driver_pid window < "$record/pane.txt"
case "$pane" in %[0-9]*) ;; *) exit 2;; esac
case "$driver_pid" in ''|*[!0-9]*) exit 2;; esac
# 同一服务器的 pane_id 不复用；同一记录目录的 observer PID 文件不覆盖。
# 每个观察者有自己的 indexed hook/channel，续接不覆盖旧 waiter。
observer=$$
channel="quality.pane.${pane#%}.driver.$driver_pid.observer.$observer"
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
    /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - "$base/position-return-commit-quality-r1/result.json" "$driver_pid" "$status" <<'PY'
import hashlib,json,sys
from pathlib import Path
p=Path(sys.argv[1]);raw=p.read_bytes();j=json.loads(raw)
assert j['schema']=='H2_COMMIT_QUALITY_ONCE_V1' and j['expected_head']=='894084738d95a7239611c2b8fbecfe081d3a7fc9'
assert int(sys.argv[3])==(0 if j['status']=='PASS' else 1)
print(json.dumps({'status':'SOFTWARE_QUALITY_REAL_TERMINAL','pane_pid':int(sys.argv[2]),'pane_exit':int(sys.argv[3]),'result_status':j['status'],'result_sha256':hashlib.sha256(raw).hexdigest(),'result_bytes':len(raw),'rustc_identity':j.get('rustc_identity'),'source_after':j.get('source_after'),'fmt_source_before_after':j.get('fmt_source_before_after'),'receipts':len(j['receipts']),'error':j.get('error')},ensure_ascii=False),flush=True)
PY
exit "$status"
