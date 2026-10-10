#!/bin/bash
# root已收到本轮现场就绪事实并签署本次一次GO；exec保留进程身份。
set -euC
umask 077
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export PYTHONDONTWRITEBYTECODE=1
base=/home/loosqk/robot-checks/h2-exit-observation-20261009-r1
tmux set-option -w -t "$TMUX_PANE" remain-on-exit on
exec > "$base/bootstrap/h2-driver-r2.log" 2>&1
h2_python=$(uv python find 3.12)
exec taskset -c 0-3 "$h2_python" "$base/bootstrap/control-v3/prelaunch_go.py" \
  --execute --inputs "$base/bootstrap/s52-right-inputs-r2.json" \
  --inputs-sha256 764ff19d88e57473b7f633d7a016ec3d30e519f67a2843ff0a41d209347bb79c --go "$base/bootstrap/s52-right-go-r2.json" \
  --go-sha256 6febf5a229d3bb6549632895b1edc2bc358bec74352a86ed2f406ee6f77db41b --driver "$base/source/scripts/run_post_e_h2.py" \
  --output "$base/right-h2-run-02" \
  --expected-head 894084738d95a7239611c2b8fbecfe081d3a7fc9
