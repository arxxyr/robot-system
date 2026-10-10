#!/usr/bin/env -S uv run --no-project python
"""右 H2 独立只读清场；不修复状态，不发控制命令，不证明寄存器已断扭。"""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import pwd
import sys
import time
from types import SimpleNamespace

sys.dont_write_bytecode = True
from prelaunch_go import canonical, pinned, read_regular, reference, require, tracked_driver, validate_output_boundary


def combine_status(driver_status, cleanup_status):
    return "PASS" if driver_status == cleanup_status == "PASS" else "FAIL"


def driver_outcome(run, inputs, inputs_sha):
    require(run["schema"] == "P7_H2_RUN_V1" and run["side"] == "right" and
            run["expected_head"] == inputs["expected_head"] and run["inputs_sha256"] == inputs_sha,
            "driver 回执未绑定本次输入")
    require(run["status"] in ("PASS", "FAIL") and type(run.get("finished_ns")) is int,
            "driver 尚未形成最终结果")
    if run.get("tool_pid") is not None:
        require(type(run["tool_pid"]) is int and run["tool_pid"] > 0 and
                run.get("tool_naturally_reaped") is True and type(run.get("tool_exit")) is int,
                "已启动工具缺真实回收事实")
    if run["status"] == "PASS":
        require(run["authorization_attempts"] == run["launch_attempts"] == 1 and
                run.get("tool_pid", 0) > 0 and run.get("tool_exit") == 0 and
                run.get("tool_naturally_reaped") is True and not run.get("failures"),
                "PASS driver 的执行/回收事实不完整")
    return {"status": run["status"], "authorization_attempts": run.get("authorization_attempts"),
            "launch_attempts": run.get("launch_attempts"), "tool_pid": run.get("tool_pid"),
            "tool_exit": run.get("tool_exit"), "tool_naturally_reaped": run.get("tool_naturally_reaped"),
            "supervisor_pid": run.get("supervisor_pid"), "finished_ns": run["finished_ns"],
            "failures": run.get("failures", [])}


def load_latch_reader(expected_sha):
    # 只复用已审准备 helper 的只读文件读取函数，不调用 prepare/main。
    path = Path(__file__).resolve().with_name("prepare_right_readonly.py")
    digest = hashlib.sha256(read_regular(path)).hexdigest()
    require(digest == expected_sha, "只读 helper 与本次准备回执身份不符")
    spec = importlib.util.spec_from_file_location("right_h2_readonly_contract", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.latch_snapshot, path, digest


def collect(options, inputs, preparation, driver, helper_hashes, latch_reader, result):
    options.out.mkdir()
    backend_options = SimpleNamespace(side="right", expected_head=inputs["expected_head"],
                                      output=options.output, inputs_sha256=options.inputs_sha256)
    journal = driver.Journal(options.out, backend_options)
    journal.state.update(schema="P7_H2_READONLY_CLEANUP_JOURNAL_V1", mode="READONLY", checks=[],
                         authorization="NOT_AUTHORIZED", planned_output=str(options.output))
    journal.save()
    backend = driver.RealBackend(backend_options, journal)
    backend.inputs = inputs
    backend.alignment = reference(inputs["alignment"])
    result.update(schema="P7_H2_READONLY_CLEANUP_V1", hardware_result="NOT_REINTERPRETED",
                  power_state="UNKNOWN", deployable=False, started_ns=time.time_ns(),
                  driver_helpers=helper_hashes, checks=[])
    baseline = {row["name"]: row for row in preparation["checks"]}
    values = {}

    def check(name, action, validate=None):
        row = {"name": name, "status": "FAIL", "started_ns": time.time_ns()}
        result["checks"].append(row)
        try:
            row["value"] = action()
            if validate is not None:
                validate(row["value"])
            values[name] = row["value"]
            row["status"] = "PASS"
        except Exception as error:
            row["error"] = repr(error)
        row["finished_ns"] = time.time_ns()
        journal.state["checks"] = result["checks"]
        journal.save()

    run_path = options.output / "run.json"
    run_raw = None
    def read_driver():
        nonlocal run_raw
        run_raw = read_regular(run_path)
        run = driver.strict_json(run_raw.decode())
        result["driver_status"] = run.get("status", "UNKNOWN")
        result["driver_run_sha256"] = hashlib.sha256(run_raw).hexdigest()
        return driver_outcome(run, inputs, options.inputs_sha256)

    check("driver_result", read_driver)
    if "driver_result" in values:
        result["driver_status"] = values["driver_result"]["status"]
        result["driver_result"] = values["driver_result"]
        result["driver_run_sha256"] = hashlib.sha256(run_raw).hexdigest()

    check("source", backend.source_check)
    check("artifacts", backend.artifact_check)
    check("formal", backend.formal_check)
    check("services", lambda: driver.validate_services(backend.command(
        ["systemctl", "show", *driver.UNITS, "--property=Id,LoadState,ActiveState,SubState,MainPID"])))
    binaries = [Path(item["path"]) for kind in ("production", "measurement")
                for item in reference(inputs[kind])["artifacts"].values()]
    binaries += [Path(row["path"]) for name, row in reference(inputs["installed"])["files"].items()
                 if name.startswith("installed:")]
    check("processes", lambda: driver.process_snapshot(-1, options.output, binaries),
          lambda value: require(value["clean"], "相关进程残留或身份不可读"))
    check("device", backend.device_check)

    def holders():
        records = []
        profile = driver.arm_profile("right")
        for path in (Path(profile.device), profile.trust / "rx_trust.lock", profile.trust / "estop_latch.lock"):
            require(path.exists(), "持有者检查路径缺失：" + str(path))
            for elevated in (False, True):
                argv = (["sudo", "-n"] if elevated else []) + ["fuser", str(path)]
                stdout = backend.command(argv, allowed=(1,))
                row = journal.state["steps"][-1]
                require(not stdout.strip() and not row["stderr"].strip(), "串口/锁被占用或查询失败")
                records.append({"path": str(path), "sudo": elevated, "exit_code": row["exit_code"]})
        return records
    check("holders", holders)

    def state_files():
        account = pwd.getpwnam("loosqk")
        states = {}
        for side in ("left", "right"):
            trust = driver.arm_profile(side).trust
            states[side] = {"markers": driver.marker_snapshot(trust, None, account.pw_uid, account.pw_gid),
                            "latches": latch_reader(trust)}
        return states

    def validate_states(states):
        require(states["left"] == baseline["state_files_after"]["value"]["left"], "左臂保护文件变化")
        require(all(row.get("hex") == "310a" for row in states["right"]["markers"].values()), "右 marker 未回 Dirty")
        require(all(row.get("state") == "LATCHED" and row.get("hex_prefix") == "310a"
                    for row in states["right"]["latches"].values()), "右双份 EStop latch 未置位")
    check("state_files", state_files, validate_states)
    check("driver_result_unchanged", lambda: read_regular(run_path) == run_raw,
          lambda unchanged: require(unchanged and run_raw is not None, "清场期间 driver 原回执变化"))
    check("inputs_unchanged", lambda: pinned(options.inputs, options.inputs_sha256) == inputs,
          lambda unchanged: require(unchanged, "清场期间输入变化"))
    result["driver_evidence_status"] = "PASS" if "driver_result" in values else "FAIL"
    result["readonly_cleanup_status"] = "PASS" if all(row["status"] == "PASS" for row in result["checks"]
                                                       if row["name"] != "driver_result") else "FAIL"
    result["status"] = combine_status(result["driver_status"], result["readonly_cleanup_status"])
    if result["driver_evidence_status"] != "PASS":
        result["status"] = "FAIL"
    result["finished_ns"] = time.time_ns()
    journal.state.update(status=result["readonly_cleanup_status"], finished_ns=result["finished_ns"])
    journal.save()
    require(not journal.state.get("journal_io_failure"), "只读清场证据写入失败")
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, epilog="用 uv python find 3.12 选解释器后 shell exec；勿保留带 output 参数的 uv run 父进程，不屏蔽进程门。")
    for name in ("inputs", "preparation", "driver", "output", "out"):
        parser.add_argument("--" + name, type=Path, required=True)
    for name in ("inputs-sha256", "preparation-sha256"):
        parser.add_argument("--" + name, required=True)
    options = parser.parse_args(argv)
    for name in ("inputs", "preparation", "driver", "output", "out"):
        canonical(getattr(options, name))
    require(options.output.is_dir() and not options.out.exists() and options.out.parent.is_dir(), "run 必须存在，清场目录必须全新")
    inputs = pinned(options.inputs, options.inputs_sha256)
    require(inputs.get("schema") == "P7_H2_INPUTS_V1" and inputs.get("side") == "right"
            and inputs["output"] == str(options.output), "清场输入未绑定右臂实际目录")
    preparation = pinned(options.preparation, options.preparation_sha256)
    require(preparation["schema"] == "P7_H2_READONLY_PREPARATION_JOURNAL_V1"
            and preparation["authorization"] == "NOT_AUTHORIZED" and preparation["status"] == "PASS"
            and preparation["side"] == "right" and preparation["expected_head"] == inputs["expected_head"]
            and preparation["planned_output"] == str(options.output), "准备基线不属于本次运行")
    require(len({row["name"] for row in preparation["checks"]}) == len(preparation["checks"])
            and all(row["status"] == "PASS" for row in preparation["checks"]), "准备基线检查不完整")
    root = Path(inputs["repository"])
    validate_output_boundary(options.out, root, {kind: reference(inputs[kind]) for kind in ("production", "measurement")})
    for protected in (root, options.output, Path("/etc"), Path("/var/lib/robot"), Path("/dev")):
        require(not options.out.is_relative_to(protected) and not protected.is_relative_to(options.out), "清场输出与受保护路径重合")
    driver, hashes = tracked_driver(options.driver, root, inputs["expected_head"])
    latch_reader, helper_path, helper_sha = load_latch_reader(preparation["helper_sha256"])
    result = {"status": "FAIL", "readonly_cleanup_status": "FAIL", "driver_status": "UNKNOWN",
              "read_only": True, "register_readback": "NOT_PERFORMED", "physical_torque_state": "UNKNOWN"}
    try:
        result = collect(options, inputs, preparation, driver, hashes, latch_reader, result)
        require(hashlib.sha256(read_regular(helper_path)).hexdigest() == helper_sha, "只读 helper 身份变化")
    except BaseException as error:
        result.update(status="FAIL", readonly_cleanup_status="FAIL", error=repr(error))
    result["readonly_helper_sha256"] = helper_sha
    if options.out.is_dir():
        with (options.out / "result.json").open("x") as stream:
            json.dump(result, stream, ensure_ascii=False, indent=2, allow_nan=False)
            stream.write("\n")
    print(json.dumps(result, ensure_ascii=False), flush=True)
    return 0 if result["status"] == "PASS" else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(json.dumps({"status": "FAIL", "read_only": True, "error": repr(error)}, ensure_ascii=False), file=sys.stderr)
        raise SystemExit(2)
