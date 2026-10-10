#!/bin/bash
set -eu
quality_identity=$(tmux display-message -p -t h2-right-r2-quality-20261010-r1:nightly-quality-once '#{session_name}|#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}|#{pane_dead_status}')
test "$quality_identity" = 'h2-right-r2-quality-20261010-r1|@40|%40|614935|1|0'
tmux kill-pane -t %40
printf '%s\n' "$quality_identity"
