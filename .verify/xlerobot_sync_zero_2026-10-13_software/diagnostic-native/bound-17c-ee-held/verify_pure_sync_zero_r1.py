"""爸爸，仅最终6个纯内存unittest、默认入口及缺GO拒绝；复用既有纯软件child回收。"""

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
FROZEN_INPUTS = {'diagnostic-candidate/base_zero_sync_diagnostic_r1.py': {'sha256': '17c098ec1952f35c21c007e5bef05dc2089bba6387be23857bc1a7de5e7a1699', 'bytes': 27500}, 'diagnostic-candidate/test_base_zero_sync_diagnostic_r1.py': {'sha256': 'ee197854a2e9daefa84b4b4ccb82759b92ebddea4677357e8e005bd6f1481aa0', 'bytes': 20952}, 'diagnostic-candidate/fixtures/run_hardware_maintenance.py': {'sha256': 'd97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41', 'bytes': 50168}, 'diagnostic-candidate/fixtures/raw_right9_from_r14_r2.py': {'sha256': '70d777bd5c2a19e2ce3f57e68f9abd40eaed2b8a9aa8c592e7fe53a940f1e38c', 'bytes': 12220}, 'diagnostic-candidate/fixtures/maintenance_bindings_base_zero_diagnostic_r4.json': {'sha256': '347a65dbfe0b3d553f7df06b413507b1491c073ced8a283f498bf7a81f51be86', 'bytes': 30051}}
EXPECTED_METHODS = ['test_complete_sequence_fifty_ms_no_ack_three_writes_and_trace', 'test_constructor_initial_left7_stop_failure_sticky_without_later_uart', 'test_lower_bound_expiry_and_original_deferred_stop_unknown', 'test_no_old_gv_write_unknown_tool_or_other_target', 'test_unbound_default_missing_ref_and_old_go_before_execution', 'test_valid_nonzero_pv_keeps_te0_cleanup_unknown_frame_or_ack_latches']
assert isinstance(FROZEN_INPUTS, dict) and FROZEN_INPUTS
assert isinstance(EXPECTED_METHODS, list) and EXPECTED_METHODS and len(EXPECTED_METHODS) == len(set(EXPECTED_METHODS))
BASE = Path('/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013')
OUT = BASE / 'diagnostic-native-r1/pure-software'
CANDIDATE = BASE / 'diagnostic-candidate/base_zero_sync_diagnostic_r1.py'
TEST = BASE / 'diagnostic-candidate/test_base_zero_sync_diagnostic_r1.py'
MAINTENANCE = BASE / 'diagnostic-candidate/fixtures/run_hardware_maintenance.py'
HELPER = BASE / 'diagnostic-candidate/fixtures/raw_right9_from_r14_r2.py'
BINDINGS = BASE / 'diagnostic-candidate/fixtures/maintenance_bindings_base_zero_diagnostic_r4.json'


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
recipes = [('memory_tests', [sys.executable, str(TEST)], 0), ('default_plan', [sys.executable, str(CANDIDATE)], 0), ('missing_go', [sys.executable, str(CANDIDATE), '--execute', '--maintenance', '/MEMORY_ONLY/never-open/maintenance.py', '--raw-helper', '/MEMORY_ONLY/never-open/helper.py', '--bindings', '/MEMORY_ONLY/never-open/bindings.json', '--output', '/MEMORY_ONLY/never-open/must-not-create', '--artifact-root', '/MEMORY_ONLY/never-open'], 2)]
receipts = []
result = {"status": "FAIL", "scope": "NATIVE_SINGLE_6_MEMORY_TESTS_DEFAULT_PLAN_MISSING_GO_NO_DEVICE_INSPECTION", "source_before": before,
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
    default = json.loads((OUT / 'default_plan.stdout').read_text())
    assert default['status'] == 'PLAN_ONLY' and default['actions_started'] == default['register_writes'] == default['motion_commands'] == 0 and default['new_GO'] is False
    assert (OUT / 'default_plan.stderr').read_bytes() == b''
    assert (OUT / 'missing_go.stdout').read_bytes() == b''
    assert (OUT / 'missing_go.stderr').read_text() == "爸爸，底盘零目标诊断前置拒绝：ValueError('必须root本次新架空就绪GO；能力答复和旧GO不适用')\n"
    assert not Path('/MEMORY_ONLY/never-open').exists()
    after = check_inputs()
    assert after == before
    result.update(status="PASS", source_after=after, frozen_inputs_before_after="PASS", methods=names,
                  distinct_memory_methods=len(names), expected_memory_methods=len(EXPECTED_METHODS),
                  native_test_batches=1, canonical_candidate_path=str(CANDIDATE),
                  sync_tool_ref={"path": "/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/quality/frozen/feetech_poke", "sha256": "b2bfedb1bc420c9957a276cd69f36f5fd47c66d0b7aaa519cf3375e257eabd94", "bytes": 1507224},
                  default_PLAN_ONLY_exit=0, missing_GO_original_exit=2, input_loading_before_missing_GO=0)
except BaseException as error:
    result["error"] = repr(error)
finally:
    result["commands"] = receipts
    save(OUT / "result.json", result)
    print(json.dumps({"status": result["status"], "result": str(OUT / "result.json"),
                      "direct_children": len(receipts), "distinct_memory_methods": result.get("distinct_memory_methods")}, ensure_ascii=False), flush=True)
raise SystemExit(0 if result["status"] == "PASS" else 1)
