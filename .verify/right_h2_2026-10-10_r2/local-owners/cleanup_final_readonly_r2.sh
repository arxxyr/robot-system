#!/bin/bash
set -eu
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export PYTHONDONTWRITEBYTECODE=1
b=/home/loosqk/robot-checks/h2-exit-observation-20261009-r1
p=$(uv python find 3.12)
exec "$p" "$b/bootstrap/control-v3/post_h2_cleanup.py" \
 --inputs "$b/bootstrap/s52-right-inputs-r2.json" \
 --inputs-sha256 764ff19d88e57473b7f633d7a016ec3d30e519f67a2843ff0a41d209347bb79c \
 --preparation "$b/right-readonly-r2/run.json" \
 --preparation-sha256 19b2b6729ad8bcd8b1a38e42e0ce424fc6ba005cd2d8707e646832441e173e3c \
 --driver "$b/source/scripts/run_post_e_h2.py" \
 --output "$b/right-h2-run-02" \
 --out "$b/right-h2-final-readonly-r2"
