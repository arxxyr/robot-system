#!/bin/bash
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12-linux-aarch64-gnu/bin/python3.12 /home/loosqk/robot-checks/h2-exit-observation-20261009-r1/bootstrap/control-v3/readback_serial_once_raw_r2.py --run-sha256 0087b8945543d6199c214e386f8fbf5be5485bcd4bdb47bc3d512924711d09cb --cleanup-sha256 3fc76faaa121bedf761ee5589d37ff3eebf3e91d66d7907127cba92b00c86fd2 --ssh-wait-sha256 defeed688d2222d29ca9ebbfd088471315f7ec161e382249b95df672f7ac18bb
