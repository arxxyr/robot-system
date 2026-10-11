set -eu
export PATH=/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:$PATH
printf 'TIME '; date -u +%FT%TZ
printf 'BOOT '; cat /proc/sys/kernel/random/boot_id
rustc -vV
cd /home/loosqk/robot-checks/position-return-20261010-r1/position-return-r7/source
git rev-parse HEAD
git status --short
ls -l /dev/serial/by-id/
for unit in robot-s0 robot-s1 robot-rpc robot-policy-host robot-web robot-viz; do systemctl is-active "$unit" || test "$?" = 3; done
pgrep -af '(robot-s0|robot-s1|robot-rpc|feetech_poke|run_hardware_maintenance)' || test "$?" = 1
