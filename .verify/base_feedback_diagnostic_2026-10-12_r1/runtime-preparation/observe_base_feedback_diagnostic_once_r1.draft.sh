#!/bin/bash
# 爸爸，本轮BASE_FEEDBACK_R1右9只读反馈终态观察副本；目标session为base-feedback-diagnostic-trial-20261012-r1，实际pane/PID/window仍待绑定。
# 爸爸，SSH 等待同一 pane 的退出通知，通知后仍只读核验真实终态。
set -euC
umask 077
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
base=/home/loosqk/robot-checks/base-feedback-diagnostic-r1
record="$base/base-feedback-diagnostic-launch-r1/observer-event-base-feedback-r1"
pane_identity='None'  # root只能在实际单次启动回执后绑定pane/PID/window；首门拒绝草稿。
read -r pane driver_pid window <<< "$pane_identity"
case "$pane" in %[0-9]*) ;; *) exit 2;; esac
case "$driver_pid" in ''|*[!0-9]*) exit 2;; esac
mkdir "$record"
printf '%s\n' "$pane_identity" > "$record/pane.txt"
# 爸爸，同一服务器的 pane_id 不复用；同一记录目录的 observer PID 文件不覆盖。
# 爸爸，每个观察者有自己的 indexed hook/channel，续接不覆盖旧 waiter。
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
# 爸爸，挂钩后查询：先退出的 pane 直接核验；查询到存活后的退出由 channel 留存通知。
observe
if [ "$dead" = 0 ]; then
  tmux wait-for "$channel"
  observe
fi
# 爸爸，提前或错误通知不能变成 PASS，也不再次等待/启动。
if [ "$dead" != 1 ]; then exit 2; fi
printf '%s\n' "$state" > "$record/observer-$observer.terminal"
    case "$status" in 0|1) ;; *) exit 2;; esac
    /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 - "$base/base-feedback-diagnostic-r1/run.json" "$driver_pid" "$status" <<'PY'
import hashlib,json,sys
from pathlib import Path
p=Path(sys.argv[1]);raw=p.read_bytes();j=json.loads(raw)
assert j['schema']=='MAINTENANCE_RUN_V2' and j['plan']['run_id']=='base-feedback-diagnostic-r1'
assert int(sys.argv[3])==(0 if j['result']=='PASS' else 1)
print(json.dumps({'status':'BASE_FEEDBACK_DIAGNOSTIC_R1_REAL_TERMINAL','pane_pid':int(sys.argv[2]),'pane_exit':int(sys.argv[3]),'result_status':j['result'],'result_sha256':hashlib.sha256(raw).hexdigest(),'result_bytes':len(raw),'stop_verification':j['stop_verification'],'position_verification':j['position_verification'],'identity_status':j['identity_verification']['status'],'probes':j['probes'],'feedback_diag':j.get('feedback_diag'),'error':j['error'],'before_errors':j['before_errors'],'after_errors':j['after_errors'],'evidence_errors':j['evidence_errors'],'manual_power_cut_required':j['manual_power_cut_required'],'child_count':len(j['commands']),'unreaped':[v['sequence'] for v in j['commands'] if v['reaped'] is not True],'child_active':j['child_active']},ensure_ascii=False),flush=True)
if j['manual_power_cut_required']:print('爸爸，停止/身份未确认，请现场立即切断两个电机电源并保持支撑；禁止后续UART。',flush=True)
PY
exit "$status"
