#!/bin/bash
set -eu
quality_base=/home/loosqk/robot-checks/h2-right-r2-commit-quality-20261010-r1
tmux display-message -p -t h2-right-r2-quality-20261010-r1:nightly-quality-once '#{pane_id}|#{pane_pid}|#{pane_dead}|#{pane_dead_status}'
tail -n 12 "$quality_base/bootstrap/quality-owner.log"
if test -e "$quality_base/quality-r1/nightly_update/stderr.log"; then tail -n 12 "$quality_base/quality-r1/nightly_update/stderr.log"; fi
if test -e "$quality_base/quality-r1/result.json"; then cat "$quality_base/quality-r1/result.json"; fi
