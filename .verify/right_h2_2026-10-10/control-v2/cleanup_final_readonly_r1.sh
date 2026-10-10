#!/bin/bash
set -eu
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
export PYTHONDONTWRITEBYTECODE=1
b=/home/loosqk/robot-checks/h2-exit-observation-20261009-r1
p=$(uv python find 3.12)
exec "$p" "$b/bootstrap/control-v2/post_h2_cleanup.py" \
 --inputs "$b/bootstrap/s52-right-inputs-r1.json" \
 --inputs-sha256 46dde8fae6fa9a25bd187ad61863ae4243d6bd81e21ed7754961a0ca9f8a3a24 \
 --preparation "$b/right-readonly-r1/run.json" \
 --preparation-sha256 bdb32269c263f45cb1b4102c447cda2aa72e5f138bab3412ed218fe7624dfa87 \
 --driver "$b/source/scripts/run_post_e_h2.py" \
 --output "$b/right-h2-run-01" \
 --out "$b/right-h2-final-readonly-r1"
