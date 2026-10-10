"""只读核对 R8 本机已有原件；不连接硬件、不启动候选。"""

from collections import Counter
from pathlib import Path, PurePosixPath
import hashlib
import json
import re
import tarfile


ROOT = Path(__file__).resolve().parents[4]
PREP = Path(__file__).resolve().parents[1]
NATIVE = PREP / "native-preparation"
ORIGINALS = NATIVE / "head1-outward-observation-r8-originals"
REMOTE = PurePosixPath("/home/loosqk/robot-checks/position-return-20261010-r1/head-settle-r8")
RUN_PATH = ORIGINALS / "head1-outward-observation-r8/run.json"
REPORT = Path(__file__).with_name("review.json")


def sha(data):
    return hashlib.sha256(data).hexdigest()


def reference(path):
    data = path.read_bytes()
    return {"path": str(path), "sha256": sha(data), "bytes": len(data)}


def read_json(path):
    return json.loads(path.read_bytes())


def local(remote):
    return ORIGINALS / str(PurePosixPath(remote).relative_to(REMOTE))


def verify_archive(path, expanded, expected_sha):
    ref = reference(path)
    assert ref["sha256"] == expected_sha
    rows = []
    with tarfile.open(path) as archive:
        members = [row for row in archive.getmembers() if row.isfile()]
        assert len({row.name for row in members}) == len(members)
        for row in members:
            assert not PurePosixPath(row.name).is_absolute()
            assert ".." not in PurePosixPath(row.name).parts
            data = archive.extractfile(row).read()
            assert len(data) == row.size
            if expanded is not None:
                file = expanded / row.name
                assert file.is_file() and not file.is_symlink()
                assert data == file.read_bytes(), row.name
            rows.append({"path": row.name, "sha256": sha(data), "bytes": len(data)})
    return {"archive": ref, "regular_files": len(rows), "regular_bytes": sum(row["bytes"] for row in rows), "members": rows}


def f201_argv(row):
    argv = row["argv"]
    if argv[0] == "/usr/bin/strace":
        argv = argv[-5:]
    if argv[0] == TOOL and len(argv) == 5:
        return argv
    return None


def check_packet(raw):
    tx, rx = raw["tx"], raw["rx"]
    width, motor = raw["width"], raw["motor_id"]
    assert tx[:2] == rx[:2] == [255, 255]
    assert tx[2:5] == [254, 5, 130]
    assert tx[5:8] == [raw["requested_register_address"], width, motor]
    assert len(tx) == tx[3] + 4 and len(rx) == rx[3] + 4
    assert sum(tx[2:]) & 255 == 255 and sum(rx[2:]) & 255 == 255
    assert rx[2] == motor and rx[4] == 0 and rx[3] == width + 2
    assert int.from_bytes(bytes(rx[5:-1]), "little") == raw["raw_unsigned"]


run_before = reference(RUN_PATH)
run = read_json(RUN_PATH)
TOOL = run["plan"]["bindings"]["tool"]["path"]
assert run_before["sha256"] == "08a3a5cd487daba50b82cedd64655c761f48148f3e502d9311f9b696301b5210"
assert run_before["bytes"] == 544340
commands = run["commands"]
assert len(commands) == 363
assert [row["sequence"] for row in commands] == list(range(1, 364))
assert len({row["pid"] for row in commands}) == 363
streams = []
values = {}
for row in commands:
    assert row["reaped"] is True and row["signals_sent"] == []
    assert row["owner_errors"] == [] and row["over_budget"] is False
    assert row["finished_ns"] >= row["started_ns"]
    for kind in ["stdout", "stderr"]:
        ref = reference(local(row[kind + "_path"]))
        assert ref["sha256"] == row[kind + "_sha256"]
        assert ref["bytes"] == row[kind + "_bytes"]
        streams.append(ref)
    if row["exit_code"] == 1:
        assert "fuser" in row["argv"]
        assert row["stdout_bytes"] == row["stderr_bytes"] == 0
    else:
        assert row["exit_code"] == 0
    argv = f201_argv(row)
    if argv and argv[3] == "read":
        output = local(row["stdout_path"]).read_text().strip()
        match = re.fullmatch(r"read (\w+)@(\d+) = (-?\d+)", output)
        assert match and match.group(1) == argv[4] and int(match.group(2)) == int(argv[2])
        values[row["sequence"]] = int(match.group(3))
assert len(streams) == 726
assert Counter(row["exit_code"] for row in commands) == {0: 335, 1: 28}
jsonl = [json.loads(line) for line in (RUN_PATH.parent / "commands.jsonl").read_text().splitlines()]
assert jsonl == commands

writes = [row for row in commands if "write" in row["argv"]]
expected = [(204, "Goal_Position", "2661"), (207, "Torque_Enable", "1"), (209, "Goal_Position", "2685"), (225, "Goal_Position", "2661"), (241, "Torque_Enable", "0")]
left = run["plan"]["bindings"]["usb"]["left"]["device"]
assert len(writes) == 5
for row, (sequence, register, value) in zip(writes, expected):
    assert row["sequence"] == sequence
    assert row["argv"] == [TOOL, left, "7", "write", register, value]
    assert row["exit_code"] == 0
assert writes[-1]["cleanup"] is True

registers = ["Operating_Mode", "Torque_Enable", "Present_Position", "Present_Velocity", "Goal_Position", "Goal_Velocity"]
read_groups = {}
for row in commands:
    argv = f201_argv(row)
    if argv and argv[3] == "read":
        side = next(side for side in ["left", "right"] if argv[1] == run["plan"]["bindings"]["usb"][side]["device"])
        read_groups.setdefault((f"{side}:{argv[2]}", argv[4]), []).append(row)
assert set(run["before"]) == set(run["after"]) and len(run["after"]) == 17
for motor in run["before"]:
    for register in registers:
        rows = read_groups[(motor, register)]
        assert values[rows[0]["sequence"]] == run["before"][motor][register]
        assert values[rows[-1]["sequence"]] == run["after"][motor][register]
        if motor != "left:7":
            assert run["before"][motor][register] == run["after"][motor][register]
    assert run["before"][motor]["Operating_Mode"] == run["after"][motor]["Operating_Mode"]
stop = run["stop_verification"]
assert stop["status"] == "PASS" and stop["issues"] == []
assert stop["motors_expected"] == 17 and len(stop["observed"]) == 17
assert stop["registers"] == ["Torque_Enable", "Present_Velocity", "Goal_Velocity"]
for motor, row in stop["observed"].items():
    assert set(row) == set(stop["registers"])
    for register, value in row.items():
        assert type(value) is int and value == 0
        assert run["after"][motor][register] == value

assert len(run["probes"]) == 1
probe = run["probes"][0]
assert [probe[k] for k in ["side", "motor", "name", "status"]] == ["left", 7, "head_motor_1", "FAIL"]
assert probe["anchor_raw"] == 2661 and probe["goal_raw"] == 2685
assert probe["cleanup_errors"] == []
stages = probe["position_accuracy"]["stages"]
for stage, witness in stages.items():
    assert witness["tolerance_raw"] == 4 and witness["budget_ms"] == 800
    assert witness["deadline_ns"] - witness["started_ns"] == 800000000
    assert witness["finished_ns"] - witness["started_ns"] == witness["elapsed_ns"]
    reads = [row for row in read_groups[("left:7", "Present_Position")] if witness["started_ns"] <= row["started_ns"] < witness["finished_ns"]]
    assert len(reads) == len(witness["samples"]) == 14
    for row, sample in zip(reads, witness["samples"]):
        assert values[row["sequence"]] == sample["position_raw"]
        assert sample["signed_error_raw"] == sample["position_raw"] - witness["target_raw"]
        assert row["finished_ns"] <= sample["stamp_ns"] <= witness["finished_ns"]
outward, returned = stages["outward"], stages["return"]
assert outward["status"] == "PASS" and outward["first_qualified_elapsed_ns"] == 251205164
assert outward["first_qualified_sample"] == outward["samples"][4]
assert outward["final_signed_error_raw"] == -4 and outward["elapsed_ns"] == 800063244
assert all(sample["position_raw"] == 2681 for sample in outward["samples"][4:])
assert all(sample["stamp_ns"] <= outward["deadline_ns"] for sample in outward["samples"])
assert returned["status"] == "FAIL" and returned["final_signed_error_raw"] == 7
assert returned["elapsed_ns"] == 801153067 and returned["failure_code"] == "POSITION_ACCURACY_TIMEOUT"
in_deadline = [s for s in returned["samples"] if s["stamp_ns"] <= returned["deadline_ns"] and s["signed_error_raw"] == 7]
late = [s for s in returned["samples"] if s["stamp_ns"] > returned["deadline_ns"]]
assert len(in_deadline) == 10 and len(late) == 1 and late[0]["signed_error_raw"] == 7
assert probe["outward"] == outward["samples"]

entry = probe["parameter_entry"]
assert entry["status"] == "PASS" and entry["fixed_p"] == 16 and entry["fixed_i"] == 0 and entry["parameter_writes"] == 0
assert len(entry["raw"]) == 19
raw_rows = [row for row in commands if "read-raw" in row["argv"]]
assert len(raw_rows) == 19
raw_parameters = {}
for row in raw_rows:
    raw = json.loads(local(row["stdout_path"]).read_bytes())
    assert raw == entry["raw"][raw["register"]]
    check_packet(raw)
    raw_parameters[raw["register"]] = raw["raw_unsigned"]
assert {k: raw_parameters[k] for k in ["P_Coefficient", "I_Coefficient", "D_Coefficient", "CW_Dead_Zone", "CCW_Dead_Zone", "Lock"]} == {"P_Coefficient": 16, "I_Coefficient": 0, "D_Coefficient": 43, "CW_Dead_Zone": 1, "CCW_Dead_Zone": 1, "Lock": 1}
assert raw_parameters["Present_Position"] == 2661 and raw_parameters["Goal_Position"] == 2655
assert run["identity_verification"]["status"] == "PASS"
checks = run["identity_verification"]["checks"]
assert len(checks) == 72 and all(row["status"] == "PASS" for row in checks)
for row in checks:
    if row["name"].startswith("ref:"):
        matching = [ref for ref in run["plan"]["bindings"]["refs"] if ref["path"] == row["name"][4:]]
        assert matching and any(all(ref[k] == row["evidence"][k] for k in ["sha256", "bytes"]) for ref in matching)
    if row["name"].startswith("usb:"):
        binding = run["plan"]["bindings"]["usb"][row["name"].split(":")[1]]
        assert all(binding[k] == value for k, value in row["evidence"].items())
assert len(run["process_snapshots"]) == 5
assert all(s["clean"] is True and not s["related"] and not s["unreadable"] and not s["vanished"] for s in run["process_snapshots"])
assert run["before_errors"] == run["after_errors"] == run["evidence_errors"] == []
assert run["result"] == "FAIL" and run["manual_power_cut_required"] is False
assert run["child_active"] is False and run["received_signal"] is None

software = read_json(PREP / "software-verification-r8/result.json")
assert all(software["checks"].values()) and len(software["commands"]) == 3
for row in software["commands"]:
    assert row["returncode"] == row["expected_returncode"] == row["wait_returncode"]
    assert row["natural_communicate_wait"] and row["echild_after_wait"] and row["signals_sent"] == []
    for kind in ["stdout", "stderr"]:
        actual = reference(Path(row[kind]["path"]))
        assert all(actual[key] == row[kind][key] for key in ["sha256", "bytes"])

owners = []
for name in ["position_head_settle_prepare_r8.owner", "position_head_settle_live_r8.owner", "position_register_commit_quality_r7_r8.owner"]:
    path = ROOT / "target/h2_exit_observation_2026-10-09/control-v3" / name
    owner = read_json(path / "result.json")
    assert owner["status"] == "PASS" and owner["wait_returncode"] == 0 and owner["reaped"] is True
    assert owner["owner_sent_signals"] == [] and owner["received_signals"] == {} and owner["errors"] == []
    for key in ["input", "log"]:
        actual = reference(Path(owner[key]["path"]))
        assert all(actual[k] == owner[key][k] for k in ["sha256", "bytes"])
    owners.append({"name": name, "owner_pid": owner["owner_pid"], "ssh_pid": owner["pid"], "wait_returncode": 0, "reaped": True, "result": reference(path / "result.json")})

hardware_archive = verify_archive(NATIVE / "head1-outward-observation-r8-originals.tar.gz", ORIGINALS, "a4bd2f7b6b13e0d76209ce4c990533ec5a33d88d2f7548ad06d6773eb8352575")
quality_path = PREP / "commit-quality-r8/commit-quality-r7-r8-originals.tar.gz"
quality_archive = verify_archive(quality_path, PREP / "commit-quality-r8/commit-quality-r7-r8-originals", "1ea20010ae71d908c504b4d59d0e70da421018aae33dfd989e8c4648f02f4573")
with tarfile.open(quality_path) as archive:
    quality = json.load(archive.extractfile("commit-quality-r8/result.json"))
    assert quality["status"] == "PASS" and len(quality["receipts"]) == 10
    quality_rows = []
    for ref in quality["receipts"]:
        member = "commit-quality-r8/" + ref["path"].split("/commit-quality-r8/")[1]
        data = archive.extractfile(member).read()
        assert sha(data) == ref["sha256"]
        row = json.loads(data)
        assert row["exit_code"] == 0 and row["reaped"] and row["signals"] == []
        for kind in ["stdout", "stderr"]:
            output = "commit-quality-r8/" + row[kind].split("/commit-quality-r8/")[1]
            data = archive.extractfile(output).read()
            assert sha(data) == row[kind + "_sha256"] and len(data) == row[kind + "_bytes"]
        quality_rows.append(row)
    fmt = next(row for row in quality_rows if row["name"] == "fmt")
    clippy = next(row for row in quality_rows if row["name"] == "clippy")
    assert fmt["argv"] == ["cargo", "fmt", "--all"]
    assert clippy["argv"] == ["cargo", "clippy", "--all", "--all-targets", "--", "-D", "warnings"]
    assert fmt["ended_monotonic_ns"] <= clippy["started_monotonic_ns"]
    assert quality["source_file_count"] == 597
    assert quality["source_manifest_sha256"] == "913f4353a925acf0e4762e01b54f6264b682ae4635da43783405a8cf1ac038c8"
    assert quality["compiler"]["commit_hash"] == "32dba69d69c5b10ea89a4042de8d0619f7756203"

wire = read_json(PREP / "stop-wire-independent-review-r8/review.json")
assert reference(RUN_PATH) == run_before
report = {"status": "INDEPENDENT_LOCAL_ORIGINALS_REVIEW_PASS", "hardware_result_unchanged": "FAIL", "run": run_before, "direct_children": 363, "streams_checked": 726, "expected_fuser_exit1": 28, "write_count": 5, "writes": [{"sequence": row["sequence"], "argv": row["argv"], "pid": row["pid"], "cleanup": row["cleanup"]} for row in writes], "parameter_writes": 0, "base_writes": 0, "raw_entry_parameters": raw_parameters, "outward": outward, "return": returned, "return_plus7_within_deadline": 10, "return_plus7_after_deadline": 1, "stop_integer_zero_fields": 51, "identity_checks": 72, "clean_process_snapshots": 5, "manual_power_cut_required": False, "software_direct_children": 3, "software_methods": 6, "quality_direct_children": 10, "quality_streams": 20, "quality": quality, "owners": owners, "wire_independent_review": reference(PREP / "stop-wire-independent-review-r8/review.json"), "archives": [{k: value for k, value in row.items() if k != "members"} for row in [hardware_archive, quality_archive]], "limits": ["四普通READ原帧不是全部51停止字段原帧", "nested strace ELF PID未计入363直属child，trace未采wait/exec", "旧±50根因仍OPEN，实际历史轮转动UNKNOWN", "本轮回程失败；无底盘动作、H2/D15关闭或正式安装", "没有SSH、UART、GO、重跑或源代码修改"]}
REPORT.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
Path(__file__).with_name("archive_members.json").write_text(json.dumps([hardware_archive, quality_archive], ensure_ascii=False, indent=2) + "\n")
print(json.dumps({"status": report["status"], "review": reference(REPORT), "archives": report["archives"]}, ensure_ascii=False))
