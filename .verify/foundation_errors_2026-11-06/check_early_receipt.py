"""以过期 GO 离线验证真实 Rust 入口到既有 Python 解析器，不打开串口。"""

import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys

root = Path.cwd()
logs = Path(sys.argv[1])
spec = importlib.util.spec_from_file_location(
    "maintenance_tests", root / "scripts/tests/test_hardware_maintenance.py"
)
tests = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tests)
case = tests.MixedRawTests()
case.setUp()
try:
    records = [json.loads(line) for line in (logs / "example-build.jsonl").read_text().splitlines()]
    binaries = [r["executable"] for r in records if r.get("reason") == "compiler-artifact"
                and r.get("target", {}).get("name") == "feetech_poke" and r.get("executable")]
    assert len(binaries) == 1, binaries
    binary = Path(binaries[0])
    calibration = case.bindings["bus2_mixed_raw_hold"]["arm_calibration"]["path"]
    device = tests.m.DEVICES["right"]
    command = [str(binary), device, "9", "mixed-hold-right9", "--arm-calibration",
               calibration, "--go-expires-at-ns", "0"]
    capture = subprocess.run(
        ["strace", "-e", "trace=openat", "-o", str(logs / "early-open.trace"), *command],
        capture_output=True, text=True, timeout=15, check=False
    )
    (logs / "early-receipt.json").write_text(capture.stdout)
    (logs / "early-stderr.log").write_text(capture.stderr)
    receipt = json.loads(capture.stdout)
    assert capture.returncode == 1, capture.returncode
    assert receipt["result"] == "FAIL" and receipt["error"] == "GO已过期", receipt
    assert receipt["cycle_count"] == 0 and receipt["cycles"] == []
    assert all(v is None for v in receipt["timing"].values())
    assert not any(receipt["cleanup"].values())
    trace = (logs / "early-open.trace").read_text()
    assert device not in trace and "/dev/serial/" not in trace and "/dev/ttyUSB" not in trace
    try:
        tests.m.parse_mixed_raw_result(capture.stdout, case.plan, 0, case.calibration)
    except ValueError as error:
        assert str(error) == "混合child失败: GO已过期", str(error)
    else:
        raise AssertionError("过期失败回执不能变为PASS")
    summary = {"result": "PASS", "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
               "child_exit_code": capture.returncode, "child_error": receipt["error"],
               "parser_error": "混合child失败: GO已过期", "cycle_count": 0,
               "serial_open_calls": 0, "new_go": False, "new_motor_commands": 0}
    (logs / "early-parser-summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(summary, ensure_ascii=False))
finally:
    case.doCleanups()
