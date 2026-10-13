"""爸爸，四个纯软件入口及原始child回执；未冻结时先拒绝。"""

import copy
import errno
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

sys.dont_write_bytecode = True
FROZEN_INPUTS = None
EXPECTED_METHODS = None
EXPECTED_DEFAULT = None
MISSING_GO_PHRASE = None
assert isinstance(FROZEN_INPUTS, dict) and FROZEN_INPUTS
assert isinstance(EXPECTED_METHODS, list) and EXPECTED_METHODS and len(EXPECTED_METHODS) == len(set(EXPECTED_METHODS))
assert isinstance(EXPECTED_DEFAULT, dict) and EXPECTED_DEFAULT
assert isinstance(MISSING_GO_PHRASE, str) and MISSING_GO_PHRASE
BASE = Path("/home/loosqk/robot-checks/base-feedback-r1-software")
OUT = BASE / "pure-software"
CANDIDATE = BASE / "candidate/base_feedback_diagnostic_r1.py"
TEST = BASE / "candidate/test_base_feedback_diagnostic_r1.py"
MAINTENANCE = BASE / "candidate/fixtures/run_hardware_maintenance.py"
HELPER = BASE / "candidate/fixtures/raw_right9_from_r14_r1.py"
BINDINGS = BASE / "candidate/fixtures/maintenance_bindings_base_zero_diagnostic_r4.json"


def ref(path):
    raw = path.read_bytes()
    return {"path": str(path), "sha256": hashlib.sha256(raw).hexdigest(), "bytes": len(raw)}


def check_inputs():
    actual = {}
    for name, expected in FROZEN_INPUTS.items():
        path = BASE / name
        assert path == path.resolve() and not path.is_symlink() and path.is_file()
        row = ref(path)
        actual[name] = {key: row[key] for key in ("sha256", "bytes")}
        assert actual[name] == expected
    return actual


def save(path, value):
    with path.open("x", encoding="utf-8") as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2)
        stream.write("\n")


before = check_inputs()
OUT.mkdir()
# 仅复制旧JSON并添加本次候选引用；不读取USB/proc/服务，不生成现场binding/GO。
bindings = json.loads(BINDINGS.read_text())
bindings = copy.deepcopy(bindings)
for source in (CANDIDATE, HELPER):
    value = ref(source)
    if not any(all(row.get(key) == item for key, item in value.items()) for row in bindings["refs"]):
        bindings["refs"].append(value)
static_binding = OUT / "static-binding-plan-only.json"
save(static_binding, bindings)
argv_common = ["--maintenance", str(MAINTENANCE), "--raw-helper", str(HELPER),
               "--bindings", str(static_binding), "--output", str(BASE / "UNEXECUTED-HARDWARE-OUTPUT"),
               "--artifact-root", str(BASE)]
recipes = [
    ("memory_tests", [sys.executable, str(TEST)], 0),
    ("default_plan", [sys.executable, str(CANDIDATE)], 0),
    ("missing_go", [sys.executable, str(CANDIDATE), "--execute", "--maintenance", "/MEMORY_ONLY/source",
                    "--raw-helper", "/MEMORY_ONLY/helper", "--bindings", "/MEMORY_ONLY/bindings",
                    "--output", "/MEMORY_ONLY/output", "--artifact-root", "/MEMORY_ONLY"], 2),
    ("full_static_plan", [sys.executable, str(CANDIDATE), *argv_common], 0),
]
receipts = []
result = {"status": "FAIL", "scope": "NATIVE_PURE_MEMORY_AND_PLAN_ONLY_NO_DEVICE_INSPECTION", "source_before": before,
          "baseline_usb_current_state": "UNKNOWN，旧binding仅静态fixture；不构成当前binding/ready",
          "hardware_actions": 0, "uart_reads": 0, "parameter_writes": 0, "new_GO": False,
          "Rust_tests_run": 0, "release_builds": 0}
try:
    for label, argv, expected_code in recipes:
        started = time.time_ns()
        child = subprocess.Popen(argv, cwd=BASE, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        stdout, stderr = child.communicate()
        code = child.wait()
        reaped = False
        try:
            os.waitpid(child.pid, os.WNOHANG)
        except ChildProcessError as error:
            reaped = error.errno == errno.ECHILD
        out, err = OUT / (label + ".stdout"), OUT / (label + ".stderr")
        with out.open("xb") as stream:
            stream.write(stdout)
        with err.open("xb") as stream:
            stream.write(stderr)
        receipt = {"label": label, "argv": argv, "owner_pid": os.getpid(), "pid": child.pid,
                   "started_ns": started, "finished_ns": time.time_ns(), "exit_code": code,
                   "wait_returncode": code, "expected_exit_code": expected_code, "reaped": reaped,
                   "signals_sent": [], "stdout": ref(out), "stderr": ref(err)}
        receipts.append(receipt)
        save(OUT / (label + ".receipt.json"), receipt)
        assert code == expected_code and reaped, receipt
    tests = (OUT / "memory_tests.stderr").read_text()
    methods = re.findall(r"^(test_\w+) \(__main__\.\w+\.(test_\w+)\) \.\.\. ok$", tests, re.M)
    assert all(left == right for left, right in methods)
    names = [left for left, _ in methods]
    assert sorted(names) == sorted(EXPECTED_METHODS) and len(names) == len(set(names)) and "\nOK\n" in tests
    default = json.loads((OUT / "default_plan.stdout").read_text())
    assert default == EXPECTED_DEFAULT
    assert MISSING_GO_PHRASE in (OUT / "missing_go.stderr").read_text()
    assert (OUT / "missing_go.stdout").read_bytes() == b""
    full = json.loads((OUT / "full_static_plan.stdout").read_text())
    assert full["status"] == "PLAN_ONLY" and full["actions_started"] == 0
    assert full["plan"]["bindings"] == bindings and full["plan"]["position_targets"] == []
    assert full["plan"]["firmware_contract_verified"] is False
    assert len(full["plan"]["base_targets"]) == 3 and all(row["decision"] == "DEFERRED" for row in full["plan"]["base_targets"])
    assert not (BASE / "UNEXECUTED-HARDWARE-OUTPUT").exists()
    after = check_inputs()
    assert after == before
    result.update(status="PASS", source_after=after, frozen_inputs_before_after="PASS", methods=names,
                  distinct_memory_methods=len(names), expected_memory_methods=len(EXPECTED_METHODS),
                  default_plan=default, full_static_plan_sha256=full["plan_sha256"],
                  full_static_plan=ref(OUT / "full_static_plan.stdout"))
except BaseException as error:
    result["error"] = repr(error)
finally:
    result["commands"] = receipts
    save(OUT / "result.json", result)
    print(json.dumps({"status": result["status"], "result": str(OUT / "result.json"),
                      "direct_children": len(receipts), "distinct_memory_methods": result.get("distinct_memory_methods")}, ensure_ascii=False), flush=True)
raise SystemExit(0 if result["status"] == "PASS" else 1)
