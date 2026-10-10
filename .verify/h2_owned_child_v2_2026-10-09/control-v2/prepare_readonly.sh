set -eu
tmux set-option -w -t "$TMUX_PANE" remain-on-exit on
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
H2_EXPECTED_HEAD="${H2_EXPECTED_HEAD:?root 必须显式传入阶段提交后的最终运行 HEAD}"
case "$H2_EXPECTED_HEAD" in *[!0-9a-f]*|'') exit 2;; esac
[ "${#H2_EXPECTED_HEAD}" -eq 40 ]
export H2_EXPECTED_HEAD
H2_PYTHON="$(uv python find 3.12)"
exec taskset -c 0-3 "$H2_PYTHON" /home/loosqk/robot-checks/h2-exit-observation-20261009-r1/bootstrap/control-v2/prepare_right_readonly.py \
 --source /home/loosqk/robot-checks/h2-exit-observation-20261009-r1/source \
 --expected-head "$H2_EXPECTED_HEAD" \
 --release-root /home/loosqk/robot-checks/h2-exit-observation-20261009-r1/release-r1 \
 --out /home/loosqk/robot-checks/h2-exit-observation-20261009-r1/right-readonly-r1 \
 --planned-output /home/loosqk/robot-checks/h2-exit-observation-20261009-r1/right-h2-run-01
