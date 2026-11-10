#!/usr/bin/env bash
set -u
mapping_root=/home/loosqk/robot-checks/wheel-id7-mapping-20261109
/home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 -I -S -B "$mapping_root/prepare.py" --artifact-root "$mapping_root" --wheel-pulse left > "$mapping_root/preparation-launch/stdout.log" 2> "$mapping_root/preparation-launch/stderr.log"
result=$?
printf "%s\n" "$result" > "$mapping_root/preparation-launch/exit-code"
exit "$result"
