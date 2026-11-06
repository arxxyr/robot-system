#!/usr/bin/env bash
set -u
task_dir=/home/loosqk/robot-checks/front-wheel-name-20261105/handoff-hardware-r1
printf '%s\n' "$$" > "$task_dir/pid"
date -u +%Y-%m-%dT%H:%M:%S.%NZ > "$task_dir/started-at"
sudo -n systemctl start xlerobot-handoff.service > "$task_dir/start.stdout" 2> "$task_dir/start.stderr"
result=$?
systemctl show xlerobot-handoff.service --property=Id,LoadState,ActiveState,SubState,MainPID,Result,ExecMainCode,ExecMainStatus > "$task_dir/service-after.txt"
journalctl --no-pager -b -u xlerobot-handoff.service -o short-precise > "$task_dir/journal.log" 2> "$task_dir/journal.stderr"
printf '%s\n' "$result" > "$task_dir/exit-code"
exit "$result"
