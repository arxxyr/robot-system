#!/bin/bash
set -eu
quality_base=/home/loosqk/robot-checks/h2-right-r2-commit-quality-20261010-r1
test ! -e "$quality_base/quality-r1"
tmux new-session -d -s h2-right-r2-quality-20261010-r1 -n nightly-quality-once "bash $quality_base/bootstrap/commit_quality_run_r1.sh"
tmux display-message -p -t h2-right-r2-quality-20261010-r1:nightly-quality-once '#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}|#{pane_dead_status}' > "$quality_base/bootstrap/tmux-launch-identity.txt"
cat "$quality_base/bootstrap/tmux-launch-identity.txt"
