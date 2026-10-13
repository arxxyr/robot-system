"""保全本机唯一例子软件验证的原流和自有子进程回执。"""

import hashlib
import json
import subprocess
import time
from pathlib import Path

ROOT = Path("/Users/loosqk/repo/rust/robot-system")
STAGE = ROOT / ".verify/xlerobot_sync_zero_2026-10-13_software"
LOCAL = STAGE / "local"
SOURCE = ROOT / "crates/robot-adapters/feetech/examples/feetech_poke.rs"


def identity(path):
    data = path.read_bytes()
    return {"path": str(path), "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)}


def run(label, argv):
    stdout_path = LOCAL / f"{label}.stdout"
    stderr_path = LOCAL / f"{label}.stderr"
    receipt_path = LOCAL / f"{label}.receipt.json"
    before = identity(SOURCE)
    started = time.time_ns()
    with stdout_path.open("xb") as stdout, stderr_path.open("xb") as stderr:
        child = subprocess.Popen(argv, cwd=ROOT, stdout=stdout, stderr=stderr)
        returncode = child.wait()
    ended = time.time_ns()
    receipt = {
        "label": label,
        "argv": argv,
        "cwd": str(ROOT),
        "child_pid": child.pid,
        "start_ns": started,
        "end_ns": ended,
        "wait_returncode": returncode,
        "poll_returncode": child.poll(),
        "reaped": True,
        "signals_sent": [],
        "stdout": identity(stdout_path),
        "stderr": identity(stderr_path),
        "source_before": before,
        "source_after": identity(SOURCE),
    }
    receipt_path.write_text(json.dumps(receipt, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"label": label, "child_pid": child.pid, "exit_code": returncode, "reaped": True}), flush=True)
    return receipt


def main():
    commands = [
        ("rustc-identity", ["rustc", "-vV"]),
        ("cargo-identity", ["cargo", "-V"]),
        ("rustup-active", ["rustup", "show", "active-toolchain"]),
        ("format", ["cargo", "fmt", "--all"]),
        (
            "example-tests",
            ["cargo", "test", "-p", "robot-adapter-feetech", "--features", "real-hw", "--example", "feetech_poke"],
        ),
        (
            "example-clippy",
            ["cargo", "clippy", "-p", "robot-adapter-feetech", "--features", "real-hw", "--example", "feetech_poke", "--", "-D", "warnings"],
        ),
    ]
    receipts = []
    for label, argv in commands:
        receipt = run(label, argv)
        receipts.append(receipt)
        if receipt["wait_returncode"] != 0:
            break
    summary = {
        "status": "PASS_LOCAL_SOFTWARE" if len(receipts) == len(commands) and all(item["wait_returncode"] == 0 for item in receipts) else "FAIL_PRESERVED",
        "actual_children": len(receipts),
        "receipts": [str(LOCAL / f'{item["label"]}.receipt.json') for item in receipts],
        "not_run": [label for label, _ in commands[len(receipts):]] + ["full-workspace-tests", "root-clippy", "release-build", "real-cli", "SSH", "UART", "hardware"],
        "source_after": identity(SOURCE),
    }
    (LOCAL / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(summary, ensure_ascii=False), flush=True)


if __name__ == "__main__":
    main()
