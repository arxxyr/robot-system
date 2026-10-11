#!/bin/bash
# 本轮R12单次nightly更新观察；无UART/GO/服务动作。
# SSH 等待同一 pane 的退出通知，通知后仍只读核验真实终态。
set -euC
umask 077
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
base=/home/loosqk/robot-checks/position-return-20261011-r12/readonly12-commit-quality
record="$base/observer-event"
mkdir "$record"
printf '%s\n' '__ACTUAL_PANE_PID_WINDOW__' > "$record/pane.txt"
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
    /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - "$base/quality/result.json" "$driver_pid" "$status" <<'PYRESULT'
import hashlib,json,sys
from pathlib import Path
p=Path(sys.argv[1]);raw=p.read_bytes();j=json.loads(raw)
assert j['schema']=='R12_HARDWARE_DOC_COMMIT_QUALITY_V1'
assert int(sys.argv[3])==(0 if j['status']=='PASS' else 1)
print(json.dumps({'status':'R12_MINIMAL_QUALITY_REAL_TERMINAL','pane_pid':int(sys.argv[2]),'pane_exit':int(sys.argv[3]),'result_status':j['status'],'result_sha256':hashlib.sha256(raw).hexdigest(),'result_bytes':len(raw),'source_before_after':j.get('source_before_after'),'compiler':j.get('compiler'),'unique_tests':j.get('unique_tests'),'cli_receipts':len(j.get('cli_receipts',[])),'receipts':len(j['receipts']),'error':j.get('error')},ensure_ascii=False),flush=True)
PYRESULT
exit "$status"
