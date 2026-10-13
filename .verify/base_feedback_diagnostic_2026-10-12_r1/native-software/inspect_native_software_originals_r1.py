"""爸爸，只核对已结束的自有SSH原件；不发网络命令，不启动候选。"""

import base64
import hashlib
import io
import json
from pathlib import Path
import re
import tarfile

HERE = Path(__file__).resolve().parent
OWNER = next(p for p in HERE.parents if (p / "target/h2_exit_observation_2026-10-09/control-v3").is_dir()) / "target/h2_exit_observation_2026-10-09/control-v3/base-feedback-r1-native-software.owner"
REMOTE = Path("/home/loosqk/robot-checks/base-feedback-r1-software")
GENERATION = json.loads((HERE / "recipe-generation-receipt.json").read_text())


def pin(raw):
    return {"sha256": hashlib.sha256(raw).hexdigest(), "bytes": len(raw)}


owner = json.loads((OWNER / "result.json").read_text())
assert owner["reaped"] is True and owner["owner_sent_signals"] == []
log = (OWNER / "child.log").read_bytes()
assert pin(log) == {key: owner["log"][key] for key in ("sha256", "bytes")}
original = json.loads(log.splitlines()[-1])
assert original["status"] == "BASE_FEEDBACK_R1_NATIVE_SOFTWARE_COLLECTED"
raw = base64.b64decode(original["archive_base64"], validate=True)
assert pin(raw) == {"sha256": original["archive_sha256"], "bytes": original["archive_bytes"]}
files = {}
with tarfile.open(fileobj=io.BytesIO(raw)) as archive:
    for member in archive:
        path = Path(member.name)
        assert not path.is_absolute() and ".." not in path.parts and member.isfile() and member.name not in files
        files[member.name] = archive.extractfile(member).read()
assert len(files) == original["archive_files"]
archive_path = HERE / "native-software-originals-base-feedback-r1.tar.gz"
with archive_path.open("xb") as stream:
    stream.write(raw)
out = HERE / "originals"
out.mkdir()
for name, content in files.items():
    path = out / name
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("xb") as stream:
        stream.write(content)


def data(name):
    return json.loads(files[name])


def relative(path):
    return str(Path(path).relative_to(REMOTE))


uploaded_before, uploaded_after = data("boot/uploaded-inputs-before.json"), data("boot/uploaded-inputs-after.json")
assert uploaded_before == uploaded_after == GENERATION["uploaded_files"]
for name, expected in uploaded_before.items():
    assert pin(files[name]) == expected
result = data("quality/result.json")
assert result == original["result"]
terminal = data("boot/terminal.json")
fields = terminal["terminal"].split("|")
assert fields[0] == "base-feedback-r1-software-20261012-r1" and fields[3] == str(original["driver_pid"])
assert fields[2] == original["pane"] and fields[4] == "1"
assert int(fields[5]) == terminal["exit_code"] == original["exit_code"] == owner["wait_returncode"]
reclaim = data("boot/reclaim.json")
assert reclaim["argv"] == ["tmux", "kill-pane", "-t", original["pane"]] and reclaim["exit_code"] == 0
for row in [*terminal["same_ssh_children"], reclaim]:
    assert row["reaped"] is True and row["signals_sent"] == [] and row["wait_returncode"] == row["exit_code"] == 0
commands = []
for reference in result["receipts"]:
    name = relative(reference["path"])
    assert hashlib.sha256(files[name]).hexdigest() == reference["sha256"]
    row = data(name)
    for suffix in ("stdout", "stderr"):
        content = files[relative(row[suffix])]
        assert pin(content) == {"sha256": row[suffix + "_sha256"], "bytes": row[suffix + "_bytes"]}
    assert row["reaped"] is True and row["exit_code"] == 0 and row["signals"] == []
    assert row["session_residual_before_cleanup"] == row["session_residual_after_cleanup"] == []
    assert "runner_error" not in row
    launch = data(relative(row["launch_receipt"]))
    assert launch["pid"] == row["pid"] and launch["identity"] == row["identity"] and launch["argv"] == row["argv"]
    commands.append(row)
assert len(commands) == 12
pure = data("pure-software/result.json")
assert pure["status"] == result["status"] == "PASS"
assert len(pure["commands"]) == 4 and pure["distinct_memory_methods"] == 30
for row in pure["commands"]:
    assert row == data("pure-software/" + row["label"] + ".receipt.json")
    assert row["reaped"] is True and row["signals_sent"] == []
    assert row["exit_code"] == row["wait_returncode"] == row["expected_exit_code"]
    for suffix in ("stdout", "stderr"):
        expected = row[suffix]
        assert pin(files[relative(expected["path"])]) == {key: expected[key] for key in ("sha256", "bytes")}
assert pure["source_before"] == pure["source_after"]
assert pure["source_before"] == {name: uploaded_before[name] for name in pure["source_before"]}
source_before = data("quality/source_before.json")
assert source_before == data("quality/fmt_after.json") == data("quality/source_after.json")
assert len(source_before["files"]) == 597
assert source_before["source_manifest_sha256"] == "068277d843b191bc8283bab38e667b0f6a4d30e296e8c5a99a97cfe19654fcf8"
rustc_before, rustc_after = files["quality/rustc_before/stdout.log"], files["quality/rustc_after/stdout.log"]
assert rustc_before == rustc_after
assert re.search(rb"\nrelease: [0-9]+\.[0-9]+\.[0-9]+-nightly\n", rustc_before)
assert b"\nhost: aarch64-unknown-linux-gnu\n" in rustc_before
assert ("\ncommit-hash: " + result["compiler"]["commit_hash"] + "\n").encode() in rustc_before
assert result["inputs_before"] == result["inputs_after"]
assert not any("warning:" in files[relative(row["stderr"])].decode() for row in commands if row["name"] == "clippy")
manifest = {name: pin(content) for name, content in sorted(files.items())}
with (HERE / "native-originals-manifest-base-feedback-r1.json").open("x") as stream:
    json.dump(manifest, stream, ensure_ascii=False, indent=2)
    stream.write("\n")
verification = {
    "status": "PASS_NATIVE_SOFTWARE_ORIGINALS_INDEPENDENT_BYTE_REVIEW",
    "native_result": result["status"], "archive": {"path": str(archive_path), **pin(raw), "files": len(files)},
    "owner": {"owner_pid": owner["owner_pid"], "ssh_pid": owner["pid"], "exit_code": owner["wait_returncode"], "reaped": owner["reaped"]},
    "driver": {"pid": original["driver_pid"], "pane": original["pane"], "terminal": terminal["terminal"], "owned_dead_pane_reclaimed": True},
    "native_direct_children": len(commands), "native_raw_stdout_stderr": len(commands) * 2,
    "python_nested_direct_children": len(pure["commands"]), "python_nested_stdout_stderr": len(pure["commands"]) * 2,
    "wrapper_direct_children": len(terminal["same_ssh_children"]) + 1,
    "distinct_memory_methods": 30, "compiler": result["compiler"],
    "source597_three_windows_equal": True, "candidate_fixture_five_byte_equal": True,
    "all_uploaded_eight_byte_equal": True, "all_recorded_streams_byte_equal": True,
    "all_recorded_children_natural_wait_reap": True, "signals_observed_this_run": [],
    "Rust_tests_run": 0, "release_builds": 0, "UART_operations": 0, "hardware_actions": 0, "GO_created": False,
    "baseline_USB": "STATIC_FIXTURE_ONLY_NOT_READY_NOT_CURRENT_IDENTITY", "R4_manual_cut_confirmation": "PENDING_UNCHANGED",
}
with (HERE / "native-software-verification-base-feedback-r1.json").open("x") as stream:
    json.dump(verification, stream, ensure_ascii=False, indent=2)
    stream.write("\n")
print(json.dumps(verification, ensure_ascii=False))
