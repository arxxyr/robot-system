#!/bin/bash
# 本轮软件质量观察副本；无UART/GO/服务动作。
# SSH 等待同一 pane 的退出通知，通知后仍只读核验真实终态。
set -euC
umask 077
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
base=/home/loosqk/robot-checks/position-return-20261010-r1
record="$base/runtime-limits-r12/readonly12-launch-record-r12/observer-event-r12"
mkdir "$record"
printf '%s\n' '%71 640784 @71' > "$record/pane.txt"
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
    case "$status" in 0|1|2) ;; *) exit 2;; esac
    /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - "$base/runtime-limits-r12/readonly12-evidence-r12/run.json" "$driver_pid" "$status" <<'PY'
import hashlib,json,sys
from pathlib import Path
p=Path(sys.argv[1]);pid=int(sys.argv[2]);status=int(sys.argv[3]);raw=p.read_bytes() if p.exists() else None;j=json.loads(raw) if raw is not None else None
if j is not None:
 assert j['pid']==pid and j['plan']['run_id']=='readonly-runtime-limits-r12'
 assert status==(0 if j['status']=='PASS_READONLY12_CAPTURE' else 1)
print(json.dumps({'status':'READONLY12_REAL_TERMINAL','pane_pid':pid,'pane_exit':status,'run_exists':j is not None,'result_status':None if j is None else j['status'],'result_sha256':None if raw is None else hashlib.sha256(raw).hexdigest(),'result_bytes':None if raw is None else len(raw),'readouts':[] if j is None else j['readouts'],'errors':[] if j is None else j['errors'],'evidence_errors':[] if j is None else j['evidence_errors'],'manual_power_cut_requested':None if j is None else j['manual_power_cut_requested'],'child_count':None if j is None else len(j['commands']),'unreaped':[] if j is None else [v['sequence'] for v in j['commands'] if v['reaped'] is not True],'child_active':None if j is None else j['child_active'],'nonuart_post':None if j is None else j['nonuart_post']},ensure_ascii=False),flush=True)
if j is not None and j['manual_power_cut_requested']:print('爸爸，只读出现异常，请现场立即切断两个电机电源并保持支撑；禁止后续UART。',flush=True)
PY
exit "$status"
