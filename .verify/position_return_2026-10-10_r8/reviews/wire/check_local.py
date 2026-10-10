"""只核 R8 已采集的四份普通 READ 原件，不连接硬件。"""

from pathlib import Path
import hashlib
import json
import re


HERE = Path(__file__).resolve().parent
PREP = HERE.parent
ORIGINAL = PREP / "native-preparation/head1-outward-observation-r8-originals/head1-outward-observation-r8"
SEQUENCES = (136, 142, 337, 343)
BY_ID = "/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C122340-if00"
TTY = "/dev/ttyACM1<char 166:1>"
ELF = "/home/loosqk/robot-checks/h2-serial-recovery-20260930-r1/readback-tool-r1/feetech_poke"


def identity(path):
    data = path.read_bytes()
    return {"path": str(path), "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}


def escaped_bytes(text):
    assert re.fullmatch(r"(?:\\x[0-9a-f]{2})+", text)
    return bytes.fromhex(text.replace("\\x", ""))


def descriptor(text):
    return re.sub(
        r"(?:\\x[0-9a-f]{2})+",
        lambda m: escaped_bytes(m.group()).decode("ascii"),
        text,
    )


run_path = ORIGINAL / "run.json"
run_before = identity(run_path)
assert run_before["bytes"] == 544340
assert run_before["sha256"] == "08a3a5cd487daba50b82cedd64655c761f48148f3e502d9311f9b696301b5210"
run = json.loads(run_path.read_bytes())
commands = {row["sequence"]: row for row in run["commands"]}
rows = []
input_before = [run_before]
for sequence in SEQUENCES:
    command = commands[sequence]
    trace = ORIGINAL / Path(command["ordinary_read_trace"]["path"]).name
    trace_identity = identity(trace)
    input_before.append(trace_identity)
    assert trace_identity["bytes"] == command["ordinary_read_trace"]["bytes"] == 10036
    assert trace_identity["sha256"] == command["ordinary_read_trace"]["sha256"]
    original_argv = command["ordinary_read_trace"]["original_argv"]
    motor = int(original_argv[2])
    assert original_argv == [ELF, BY_ID, str(motor), "read", "Present_Velocity"]
    assert command["argv"][-len(original_argv):] == original_argv
    assert command["argv"][:10] == ["/usr/bin/strace", "-ttt", "-xx", "-s", "256", "-yy", "-e", "trace=openat,close,read,write,ioctl", "-o", command["ordinary_read_trace"]["path"]]
    assert command["reaped"] is True and command["exit_code"] == 0
    assert command["signals_sent"] == [] and command["owner_errors"] == []
    assert command["over_budget"] is False
    lines = trace.read_text().splitlines()
    assert len(lines) == 32
    opened = []
    resolved_self_pids = []
    serial = []
    stdout_writes = []
    serial_close = []
    for line_no, line in enumerate(lines, 1):
        if '"\\x2f\\x70\\x72\\x6f\\x63\\x2f\\x73\\x65\\x6c\\x66\\x2f\\x6d\\x61\\x70\\x73"' in line:
            decoded = descriptor(line)
            match = re.search(r" = (\d+)</proc/(\d+)/maps>$", decoded)
            assert match
            resolved_self_pids.append(int(match.group(2)))
        if 'O_RDWR|O_NOCTTY|O_NONBLOCK|O_CLOEXEC' in line:
            decoded = descriptor(line)
            assert f'"{BY_ID}"' in decoded
            match = re.search(r" = (\d+)<(/dev/ttyACM1<char 166:1>)>$", decoded)
            assert match
            opened.append({"line": line_no, "fd": int(match.group(1)), "device": match.group(2), "timestamp": line.split()[0]})
        match = re.fullmatch(r'(\d+\.\d+) (read|write)\((\d+)<(.+)>, "((?:\\x[0-9a-f]{2})+)", (\d+)\) = (\d+)', line)
        if match:
            timestamp, operation, fd, target, payload, requested, returned = match.groups()
            fd = int(fd)
            data = escaped_bytes(payload)
            target = descriptor(target)
            if target == TTY:
                assert int(requested) == int(returned) == len(data)
                assert data[:2] == b"\xff\xff" and data[2] == motor
                assert len(data) == data[3] + 4
                assert sum(data[2:]) & 255 == 255
                serial.append({"line": line_no, "timestamp": timestamp, "operation": operation, "fd": fd, "requested": int(requested), "returned": int(returned), "frame_hex": data.hex(" "), "id": data[2], "length_byte": data[3], "opcode_or_error": data[4], "payload_hex": data[5:-1].hex(" "), "checksum": data[-1], "length_valid": True, "checksum_valid": True})
            elif fd == 1:
                assert int(requested) == int(returned) == len(data)
                stdout_writes.append(data)
        if "close(" in line and descriptor(line).endswith(f"<{TTY}>) = 0"):
            serial_close.append(line_no)
    assert len(opened) == len(resolved_self_pids) == len(serial_close) == 1
    assert opened[0]["fd"] == 3
    assert [row["operation"] for row in serial] == ["write", "read"] * 3
    assert all(row["fd"] == 3 for row in serial)
    frames = [bytes.fromhex(row["frame_hex"]) for row in serial]
    assert frames[0][3:5] == bytes([2, 1]) and frames[0][5:-1] == b""
    assert frames[1][3:5] == bytes([2, 0]) and frames[1][5:-1] == b""
    assert frames[2][3:7] == bytes([4, 2, 3, 2])
    assert frames[3][3:7] == bytes([4, 0, 9, 3])
    assert frames[4][3:7] == bytes([4, 2, 58, 2])
    assert frames[5][3:7] == bytes([4, 0, 0, 0])
    expected_stdout = f"read Present_Velocity@{motor} = 0\n".encode()
    assert stdout_writes == [expected_stdout]
    outputs = []
    for stream in ("stdout", "stderr"):
        path = ORIGINAL / Path(command[f"{stream}_path"]).name
        record = identity(path)
        assert record["bytes"] == command[f"{stream}_bytes"]
        assert record["sha256"] == command[f"{stream}_sha256"]
        assert path.read_bytes() == (expected_stdout if stream == "stdout" else b"")
        outputs.append({"stream": stream, **record})
        input_before.append(record)
    assert lines[-1].endswith("+++ exited with 0 +++")
    rows.append({"sequence": sequence, "motor": motor, "cleanup": command["cleanup"], "trace": trace_identity, "outer_strace_child": {"pid": command["pid"], "reaped_in_run_receipt": True, "exit_code_in_run_receipt": 0, "signals_sent": [], "owner_errors": []}, "trace_process_pid_from_resolved_proc_self_maps": resolved_self_pids[0], "nested_wait_reap_direct_evidence": "NOT_CAPTURED", "opened_serial": opened[0], "serial_frames": serial, "ordinary_read": {"opcode": 2, "address": 58, "width": 2, "response_error": 0, "raw_payload_hex": "00 00", "present_velocity_raw": 0}, "stdout": expected_stdout.decode(), "outputs": outputs, "serial_close_line": serial_close[0], "traced_process_exit_code": 0, "status": "PASS"})
input_after = [identity(Path(row["path"])) for row in input_before]
assert input_before == input_after
review = {"schema": "r8-stop-wire-independent-review-v1", "status": "PASS", "scope": "仅本地核四个已采集普通 READ；不重跑，不 SSH/UART，不修改原件或主源", "run": run_before, "trace_count": 4, "serial_tx_count": 12, "serial_rx_count": 12, "present_velocity_response_count": 4, "input_hashes_unchanged": True, "rows": rows, "limitations": ["仅四个右8/右9速度 READ 响应原帧；不等同整机51停止字段原帧。", "每条普通read还含PING和地址3的2B读取；串口TX写入协议READ请求，不含电机参数写指令或Enable。", "PID由被追踪进程的/proc/self/maps解析获得；trace不含exec、wait或-f，不把nested PID计入driver直属child，不宣称其wait/reap直接证据。", "外层strace直属child的exit0/reaped/signals[]为原run回执记录，父任务另核完整363child；这里不核全部owner路径。", "四次PV原帧为零，不能解释或覆盖旧±50停止失败，不能据此判断历史轮子实际运动。", "地址3响应为09 03即raw777；此处只核字节，不外推现场固件版本或速度单位。", "不改变原run中的PENDING_ROOT_REVIEW_OF_COMPLETE_STRACE字段或其整体结果。"]}
for name, value in (("review.json", review),):
    with (HERE / name).open("x") as file:
        json.dump(value, file, ensure_ascii=False, indent=2)
        file.write("\n")
table = ["|序号|电机|外层 strace PID|trace /proc/self PID|TX (PV READ)|RX|", "|---|---|---:|---:|---|---|"]
for row in rows:
    table.append(f"|{row['sequence']}|右{row['motor']}|{row['outer_strace_child']['pid']}|{row['trace_process_pid_from_resolved_proc_self_maps']}|`{row['serial_frames'][4]['frame_hex']}`|`{row['serial_frames'][5]['frame_hex']}`|")
text = "爸爸，四份 R8 普通 READ 原件独立本地核验通过。每条 trace 均32行/10036B，实际串口 fd 3 为 /dev/ttyACM1 (166:1)，四个 PV 响应 error=0、payload=00 00、普通 stdout=0。\n\n" + "\n".join(table) + "\n\n每条均有完整 PING、地址3/2B READ、地址58/2B READ，所有12 TX及12 RX的长度、ID、字节数和校验和匹配；串口成功关闭，trace进程exit0。三对TX/RX及完整SHA见review.json，原run/trace/stdout/stderr核验前后SHA一致。\n\n" + "\n\n".join(review["limitations"]) + "\n"
with (HERE / "review.md").open("x") as file:
    file.write(text)
print(json.dumps({"status": "PASS", "outputs": [identity(HERE / name) for name in ("check_local.py", "review.json", "review.md")], "rows": [{"sequence": r["sequence"], "trace_sha256": r["trace"]["sha256"], "outer_pid": r["outer_strace_child"]["pid"], "trace_process_pid": r["trace_process_pid_from_resolved_proc_self_maps"]} for r in rows]}, ensure_ascii=False))
