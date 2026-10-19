#!/bin/bash
# 左1单次握手命令留档；复用历史工具，仅 PING 和型号 READ。
set -euo pipefail
umask 077
field_seconds=${1:-}
[[ "$field_seconds" =~ ^[0-9]{10}$ ]] || exit 2
check_field() {
  local now
  now=$(date +%s)
  (( now >= field_seconds && now - field_seconds < 600 ))
}
check_field
[[ $(cat /proc/sys/kernel/random/boot_id) == 6cf7ee3e-a3c5-4c91-80bc-4ca924818b53 ]]
tool=/home/loosqk/robot-checks/h2-serial-recovery-20260930-r1/readback-tool-r1/feetech_poke
device=/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C121427-if00
out=/home/loosqk/robot-checks/left1-communication-20261015/ping
[[ $(sha256sum "$tool") == "f2014ee4008311e7a8725235a39f0d1b3913391f17cbd977b7441f120539114b  $tool" ]]
[[ $(sha256sum /usr/bin/strace) == '5392f54d889a87341d49f970d91e21ab8c1f70a835ac9ead6ee4d17e532ae7f7  /usr/bin/strace' ]]
mkdir "$out"
uart_attempted=0
finish() {
  local result=$?
  printf 'shell_exit=%s\nuart_attempted=%s\nfinished_utc=%s\n' "$result" "$uart_attempted" "$(date -u +%FT%TZ)" > "$out/terminal.txt"
}
trap finish EXIT
printf 'field_seconds=%s\nstarted_utc=%s\nshell_pid=%s\n' "$field_seconds" "$(date -u +%FT%TZ)" "$$" > "$out/start.txt"
for side in left right; do
  for name in rx_trust.lock estop_latch.lock; do
    path=/var/lib/robot/xlerobot_${side}/$name
    [[ -f "$path" && ! -L "$path" ]]
  done
done
exec 9</var/lib/robot/xlerobot_left/rx_trust.lock
exec 8</var/lib/robot/xlerobot_left/estop_latch.lock
exec 7</var/lib/robot/xlerobot_right/rx_trust.lock
exec 6</var/lib/robot/xlerobot_right/estop_latch.lock
for fd in 9 8 7 6; do flock -n "$fd"; done
units=(xlerobot-s0-left-arm.service xlerobot-s1-left.service xlerobot-gateway-left.service
       xlerobot-s0-right-arm.service xlerobot-s1-right.service xlerobot-gateway-right.service
       xlerobot-coordinator.service xlerobot-web.service xlerobot-s0-bus1.service xlerobot-s0-bus2.service)
systemctl show "${units[@]}" --property=ActiveState,SubState,MainPID > "$out/services.txt"
[[ $(grep -c '^ActiveState=inactive$' "$out/services.txt") == 10 ]]
[[ $(grep -c '^SubState=dead$' "$out/services.txt") == 10 ]]
[[ $(grep -c '^MainPID=0$' "$out/services.txt") == 10 ]]
[[ -c "$device" ]]
udevadm info --query=property --name="$device" > "$out/usb.txt"
grep -qx 'ID_SERIAL_SHORT=5A7C121427' "$out/usb.txt"
grep -qx 'ID_VENDOR_ID=1a86' "$out/usb.txt"
grep -qx 'ID_MODEL_ID=55d3' "$out/usb.txt"
set +e
fuser "$device" > "$out/fuser.stdout" 2> "$out/fuser.stderr"
users_exit=$?
set -e
[[ "$users_exit" == 1 && ! -s "$out/fuser.stdout" && ! -s "$out/fuser.stderr" ]]
check_field
uart_attempted=1
set +e
timeout --kill-after=1s 3s /usr/bin/strace -ttt -T -xx -s 256 -yy \
  -e trace=openat,ioctl,read,write,poll,ppoll,close -o "$out/uart.strace" \
  "$tool" "$device" 1 ping > "$out/ping.stdout" 2> "$out/ping.stderr"
result=$?
set -e
printf '%s\n' "$result" > "$out/ping.exit"
exit "$result"
