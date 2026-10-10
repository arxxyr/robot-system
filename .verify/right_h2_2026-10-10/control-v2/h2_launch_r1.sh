#!/bin/bash
# 草案：只有 root 收到本次现场事实后，才填入两个实际 SHA 并生成 h2_launch.sh。
set -euC
umask 077
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export PYTHONDONTWRITEBYTECODE=1
base=/home/loosqk/robot-checks/h2-exit-observation-20261009-r1
tmux set-option -w -t "$TMUX_PANE" remain-on-exit on
exec > "$base/bootstrap/h2-driver-r1.log" 2>&1
h2_python=$(uv python find 3.12)
exec taskset -c 0-3 "$h2_python" "$base/bootstrap/control-v2/prelaunch_go.py" \
  --execute --inputs "$base/bootstrap/s52-right-inputs-r1.json" \
  --inputs-sha256 46dde8fae6fa9a25bd187ad61863ae4243d6bd81e21ed7754961a0ca9f8a3a24 --go "$base/bootstrap/s52-right-go-r1.json" \
  --go-sha256 5d621ac975ce61f507fd49be9ccb05cb99c38aef126f20fcfde3fe9b81402588 --driver "$base/source/scripts/run_post_e_h2.py" \
  --output "$base/right-h2-run-01" \
  --expected-head a96c34ae26b56fc9e8d2deef17c3b829c3d87d0d
