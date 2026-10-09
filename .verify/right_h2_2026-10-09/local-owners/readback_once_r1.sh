#!/bin/bash
# 当次已获 root 批准的 34 项原帧停止读回；失败停止且不重试。
set -eu
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export PYTHONDONTWRITEBYTECODE=1
b=/home/loosqk/robot-checks/h2-right-ready-20261009-r2
h2_python=$(uv python find 3.12)
exec "$h2_python" "$b/bootstrap/control-r2/readback_serial_once_raw.py" \
 --run-sha256 3f735d4658ceb9429c920c6a50531f806c9a258d81ef9cd5f21f91c2fc86a124 \
 --cleanup-sha256 a3d7b62467fc908c6722c466848e41921076889d2f93722fc59309771e4f252b \
 --ssh-wait-sha256 0cd22496831925daa9e8abb6fc400bf1cc4d1a23b2ab43603910315254d1c4ed
