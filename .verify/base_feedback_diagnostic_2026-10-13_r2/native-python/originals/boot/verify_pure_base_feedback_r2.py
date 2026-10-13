"""爸爸，仅31个纯内存unittest及原始child回执；未冻结时先拒绝。"""

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
FROZEN_INPUTS = {'candidate/base_feedback_diagnostic_r2.py': {'sha256': 'cf651bb200d0ea56228282dd51b311bb090650945b631f51e8d7ff2f1e431b78', 'bytes': 18648}, 'candidate/test_base_feedback_diagnostic_r2.py': {'sha256': '815ec4995e1bcd0c92e0446ad49d876da518185072f985f94f573f4eb7445417', 'bytes': 37227}, 'candidate/fixtures/raw_right9_from_r14_r2.py': {'sha256': '70d777bd5c2a19e2ce3f57e68f9abd40eaed2b8a9aa8c592e7fe53a940f1e38c', 'bytes': 12220}, 'candidate/fixtures/run_hardware_maintenance.py': {'sha256': 'd97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41', 'bytes': 50168}, 'candidate/fixtures/maintenance_bindings_base_zero_diagnostic_r4.json': {'sha256': '347a65dbfe0b3d553f7df06b413507b1491c073ced8a283f498bf7a81f51be86', 'bytes': 30051}}
EXPECTED_METHODS = ['test_all_fixed_metadata_mismatches_block', 'test_bad_checksum_or_id_blocks_finally_snapshot_uart', 'test_bad_raw_frame_field_blocks_at_the_original_read', 'test_boolean_metadata_or_tx_byte_does_not_pass_integer_one', 'test_completion_after_500ms_is_outside_original_window', 'test_completion_at_500ms_remains_inside_original_window', 'test_constructor_installs_write_guard_before_first_probe', 'test_default_plan_has_zero_actions_before_loading', 'test_execute_without_complete_inputs_refuses_before_loading', 'test_execute_without_go_refuses_before_loading', 'test_first_group_nonzero_te_is_sticky_before_pp_observation', 'test_guard_rejects_every_write_enable_and_non_target_raw', 'test_initial_ordinary_stdout_identity_unknown_is_sticky_and_final_snapshot_has_zero_uart', 'test_initial_ordinary_stop_nonzero_is_sticky_and_final_snapshot_has_zero_uart', 'test_missed_second_slot_is_not_retimed_or_retried', 'test_observation_nonzero_te_stops_before_same_group_pp_or_pv', 'test_old_go_or_different_scope_is_rejected', 'test_original_other_motor_stop_reads_remain_read_only', 'test_oversleep_cannot_shift_original_sample_schedule', 'test_plan_keeps_three_wheels_deferred_and_firmware_unverified', 'test_pp_keeps_unsigned_two_byte_word_without_physical_inference', 'test_pv_0032_immediately_blocks_and_preserves_same_group_pp', 'test_pv_8032_immediately_blocks_and_preserves_same_group_pp', 'test_raw_bool_value_does_not_pass_integer_zero', 'test_raw_group_retains_complete_frames_and_request_completion_times', 'test_read_exception_latches_without_retry_or_fallback', 'test_right7_ordinary_pv_capture_only_matches_read_and_raw_id9_is_unchanged', 'test_stop51_unknown_cannot_be_reported_pass_or_firmware_verified', 'test_success_has_three_te_pp_pv_groups_at_original_offsets', 'test_three_deferred_rows_are_independent_of_diagnostic_pass', 'test_unknown_original_stop_read_is_sticky']
assert isinstance(FROZEN_INPUTS, dict) and FROZEN_INPUTS
assert isinstance(EXPECTED_METHODS, list) and EXPECTED_METHODS and len(EXPECTED_METHODS) == len(set(EXPECTED_METHODS))
BASE = Path("/home/loosqk/robot-checks/base-feedback-r2-software-20261013")
OUT = BASE / "pure-software"
CANDIDATE = BASE / "candidate/base_feedback_diagnostic_r2.py"
TEST = BASE / "candidate/test_base_feedback_diagnostic_r2.py"
MAINTENANCE = BASE / "candidate/fixtures/run_hardware_maintenance.py"
HELPER = BASE / "candidate/fixtures/raw_right9_from_r14_r2.py"
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
recipes = [('memory_tests', [sys.executable, str(TEST)], 0)]
receipts = []
result = {"status": "FAIL", "scope": "NATIVE_SINGLE_31_MEMORY_UNITTEST_NO_CARGO_PLAN_OR_DEVICE_INSPECTION", "source_before": before,
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
    after = check_inputs()
    assert after == before
    result.update(status="PASS", source_after=after, frozen_inputs_before_after="PASS", methods=names,
                  distinct_memory_methods=len(names), expected_memory_methods=len(EXPECTED_METHODS),
                  native_test_batches=1)
except BaseException as error:
    result["error"] = repr(error)
finally:
    result["commands"] = receipts
    save(OUT / "result.json", result)
    print(json.dumps({"status": result["status"], "result": str(OUT / "result.json"),
                      "direct_children": len(receipts), "distinct_memory_methods": result.get("distinct_memory_methods")}, ensure_ascii=False), flush=True)
raise SystemExit(0 if result["status"] == "PASS" else 1)
