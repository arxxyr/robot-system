#!/usr/bin/env -S uv run --no-project python
"""本次已完成 H2 的原帧停止读回：由 root 审核真实绑定后另行启动。"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import stat
import struct
import subprocess
import sys
import time
from types import SimpleNamespace

sys.dont_write_bytecode = True
from prelaunch_go import canonical, pinned, read_regular, reference, require, tracked_driver

BASE = Path("/home/loosqk/robot-checks/h2-exit-observation-20261009-r1")
ROOT = BASE / "source"
RUN = BASE / "right-h2-run-02/run.json"
CLEANUP = BASE / "right-h2-cleanup-r2/result.json"
SSH_WAIT = BASE / "bootstrap/s52-right-h2-ssh-wait-r2.json"
INPUTS = BASE / "bootstrap/s52-right-inputs-r2.json"
GO = BASE / "bootstrap/s52-right-go-r2.json"
PLAN = Path("/home/loosqk/robot-checks/h2-right-ready-20261009-r2/bootstrap/s52-readback-plan-r1.json")
OUT = BASE / "register-readback-r2"
ELF = Path("/home/loosqk/robot-checks/h2-right-ready-20261009-r1/readback-tool-r1/feetech_poke")
HEAD = "894084738d95a7239611c2b8fbecfe081d3a7fc9"
BUILD_HEAD = "c800f86f81c3a9290e51a1a516a033a69efbf800"
SOURCE_SHA = "5970c9bed0ab02f99b47d88d47eb3718c6a748155ca4fafd5c18050a68ec6cf7"
PLAN_SHA = '9d1281e99786281c402e2d5690225c24fbdac4acfa480cb9610f84fdf6b686c0'
ELF_SHA = '13c97115b89d9f8780ea8b9cac80a091ddcc2b73549d34b2d5cbad83c718ffe1'
INPUTS_SHA = "764ff19d88e57473b7f633d7a016ec3d30e519f67a2843ff0a41d209347bb79c"
GO_SHA = "6febf5a229d3bb6549632895b1edc2bc358bec74352a86ed2f406ee6f77db41b"
GO_HELPER_SHA = "8121c8e8454d5a06dfa1f828ca85417917a0cdcd756d222717af89adf1024b77"
SSH_OWNER_SHA = "96d1c8ed14fa94e62cbfcb0ea28ff60f628bdcf0d76cf3796d968602ce14938a"
LAUNCH_INPUT_SHA = "6607d148409e708dfcd4802ea2ccbf1d26d5e01877efc679a36aee7e8cb582b8"
# 草稿绑定：实际 PID、时刻和启动输入 SHA 由 root 从本次真实回收原件填写。
DRAFT_ONLY = False
ELF_BYTES = 1510912
ACTUAL_EXEC_CHECKED_NS = 1791606539799200002
ACTUAL_DRIVER_PID = 605045
ACTUAL_TOOL_PID = 605107


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def elf_identity():
    raw = read_regular(ELF)
    require(digest(raw) == ELF_SHA and len(raw) == ELF_BYTES, "冻结读回 ELF 身份改变")
    require(raw[:6] == b"\x7fELF\x02\x01" and struct.unpack_from("<H", raw, 18)[0] == 183,
            "读回工具不是已验 Linux ARM64 ELF")
    require(os.access(ELF, os.X_OK), "读回工具不可执行")
    return {"path": str(ELF), "sha256": ELF_SHA, "bytes": len(raw), "elf_machine": 183}


def validate_plan(plan, driver):
    require(plan["status"] == "PLAN_ONLY" and plan["authorization"] == "NOT_AUTHORIZED"
            and plan["binary"] == str(ELF) and plan["binary_sha256"] == ELF_SHA
            and plan["build_head"] == BUILD_HEAD and plan["source_manifest_sha256"] == SOURCE_SHA,
            "已审读回计划身份不符")
    expected = []
    for side, count in (("left", 8), ("right", 9)):
        for motor in range(1, count + 1):
            for register in ("Torque_Enable", "Present_Velocity"):
                subsystem = "机械臂含夹爪" if motor <= 6 else ("头部" if side == "left" else "底盘")
                expected.append({"side": side, "motor_id": motor, "register": register,
                                 "argv": [str(ELF), driver.arm_profile(side).device,
                                          str(motor), "read-raw", register],
                                 "expected_integer": 0,
                                 "expected_exit": 0, "subsystem": subsystem})
    require(plan["steps"] == expected and len(expected) == 34, "只允许已审 34 条精确 read-raw 参数")
    build = reference(plan["binary_build_result"])
    receipt = reference(plan["binary_build_receipt"])
    require(build["status"] == "PASS" and build["build_head"] == BUILD_HEAD
            and build["source_manifest_sha256"] == SOURCE_SHA
            and build["artifact"]["path"] == str(ELF) and build["artifact"]["sha256"] == ELF_SHA
            and build["build_receipt"] == plan["binary_build_receipt"], "读回工具构建绑定错误")
    require(receipt["reaped"] is True and receipt["exit_code"] == 0 and receipt["signals"] == []
            and receipt["argv"] == ["cargo", "build", "--locked", "--release", "-p",
                                    "robot-adapter-feetech", "--features", "real-hw", "--example",
                                    "feetech_poke", "--message-format=json"], "读回工具缺真实构建回收")


def validate_evidence(options, driver, inputs, go, run, cleanup, ssh_wait):
    require(inputs["repository"] == str(ROOT) and inputs["source_manifest_sha256"] == SOURCE_SHA,
            "本次输入源码不符")
    driver.validate_facts(inputs, reference(inputs["alignment"]), reference(inputs["physical"]),
                          SimpleNamespace(side="right", expected_head=HEAD, output=RUN.parent), time.time_ns())
    require(go["schema"] == "P7_H2_RIGHT_ROOT_GO_V1" and go["approved"] is True
            and go["issued_by"] == "root" and go["side"] == "right" and go["expected_head"] == HEAD
            and go["output"] == str(RUN.parent) and go["inputs_sha256"] == INPUTS_SHA
            and go["physical_sha256"] == inputs["physical"]["sha256"]
            and go["alignment_sha256"] == inputs["alignment"]["sha256"], "已消费 GO 的绑定错误")
    require(type(go["signed_at_ns"]) is int and type(go["expires_at_ns"]) is int
            and 0 < go["signed_at_ns"] <= ACTUAL_EXEC_CHECKED_NS < go["expires_at_ns"],
            "原实际启动不在 GO 有效窗内")
    require(run["schema"] == "P7_H2_RUN_V1" and run["side"] == "right"
            and run["expected_head"] == HEAD and run["inputs_sha256"] == INPUTS_SHA
            and run["status"] in ("PASS", "FAIL") and type(run["started_ns"]) is int
            and type(run["finished_ns"]) is int
            and ACTUAL_EXEC_CHECKED_NS <= run["started_ns"] <= run["finished_ns"] <= time.time_ns()
            and run["supervisor_pid"] == ACTUAL_DRIVER_PID and run["tool_pid"] == ACTUAL_TOOL_PID
            and run["authorization_attempts"] == run["launch_attempts"] == 1
            and run["tool_naturally_reaped"] is True and type(run["tool_exit"]) is int,
            "H2 工具/driver 缺本次完整执行与真实回收事实")
    if run["status"] == "PASS":
        require(run["tool_exit"] == 0 and run["failures"] == [], "原 H2 PASS 不完整")
    require(ssh_wait["schema"] == "P7_H2_LOCAL_SSH_OWNER_V1" and ssh_wait["phase"] == "REAPED"
            and ssh_wait["reaped"] is True and type(ssh_wait["pid"]) is int and ssh_wait["pid"] > 0
            and ssh_wait["child_end_class"] == "EXIT_STATUS"
            and type(ssh_wait["wait_returncode"]) is int
            and ssh_wait["wait_returncode"] == (0 if run["status"] == "PASS" else 1)
            and ssh_wait["owner_sent_signals"] == [] and ssh_wait["errors"] == []
            and ssh_wait["helper_sha256"] == SSH_OWNER_SHA
            and ssh_wait["input"]["sha256"] == LAUNCH_INPUT_SHA
            and ssh_wait["command"] == ["/usr/bin/ssh", "-o", "BatchMode=yes", "-o",
                                         "StrictHostKeyChecking=yes", "-o", "ConnectTimeout=15",
                                         "robot", "bash", "-s"]
            and isinstance(ssh_wait["finished_utc"], str), "H2 SSH 缺本次真实自然 wait/reap 原件")
    outcome = {key: run[key] for key in ("status", "authorization_attempts", "launch_attempts", "tool_pid",
                                         "tool_exit", "tool_naturally_reaped", "supervisor_pid", "finished_ns", "failures")}
    require(cleanup["schema"] == "P7_H2_READONLY_CLEANUP_V1" and cleanup["read_only"] is True
            and cleanup["readonly_cleanup_status"] == cleanup["driver_evidence_status"] == "PASS"
            and cleanup["driver_status"] == run["status"] and cleanup["driver_result"] == outcome
            and cleanup["driver_run_sha256"] == options.run_sha256
            and cleanup["status"] == run["status"] and cleanup["register_readback"] == "NOT_PERFORMED"
            and type(cleanup["started_ns"]) is int and type(cleanup["finished_ns"]) is int
            and run["finished_ns"] <= cleanup["started_ns"] <= cleanup["finished_ns"] <= time.time_ns(),
            "独立清场尚未 PASS 或未绑定原 H2 回执")
    required_checks = {"driver_result", "source", "artifacts", "formal", "services", "processes", "device",
                       "holders", "state_files", "driver_result_unchanged", "inputs_unchanged"}
    checks = cleanup["checks"]
    require(len(checks) == len(required_checks) and {row["name"] for row in checks} == required_checks
            and all(row["status"] == "PASS" for row in checks), "独立清场检查不完整")
    require(next(row for row in checks if row["name"] == "processes")["value"]["clean"] is True,
            "清场仍有控制进程")


class Owner:
    """观察间隔不是运行超时；任何异常或退出信号都不释放仍存活的 child。"""

    def __init__(self, result):
        self.result = result
        self.stop = False
        self.previous_handlers = {}

    def error(self, kind, error):
        self.stop = True
        self.result["status"] = "FAIL"
        self.result["errors"].append({"kind": kind, "error": repr(error), "at_ns": time.time_ns()})

    def save(self):
        temporary = OUT / "result.json.tmp"
        with temporary.open("w") as stream:
            json.dump(self.result, stream, ensure_ascii=False, indent=2, allow_nan=False)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        temporary.replace(OUT / "result.json")

    def persist(self):
        try:
            self.save()
        except BaseException as error:
            self.error("evidence_io", error)

    def receive(self, signum, frame):
        self.stop = True
        self.result["received_signals"].append({"name": signal.Signals(signum).name, "at_ns": time.time_ns()})

    def install(self):
        for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT):
            self.previous_handlers[signum] = signal.signal(signum, self.receive)

    def restore(self):
        for signum, handler in self.previous_handlers.items():
            signal.signal(signum, handler)

    def wait(self, child, row):
        while not row["reaped"]:
            try:
                code = child.wait(timeout=1)
                row.update(exit_code=code, reaped=True, finished_ns=time.time_ns(),
                           child_end_class="EXIT_STATUS" if code >= 0 else "SIGNAL")
            except subprocess.TimeoutExpired:
                pass  # 仅观测；无总 deadline，无 terminate/kill。
            except BaseException as error:
                self.error("wait", error)
                try:
                    time.sleep(1)
                except BaseException as sleep_error:
                    self.error("wait_backoff", sleep_error)
            self.persist()

    def command(self, name, argv):
        require(not self.stop, "已收到信号或异常，停止后续命令")
        directory = OUT / name
        directory.mkdir()
        row = {"name": name, "argv": argv, "started_ns": time.time_ns(), "pid": None,
               "reaped": False, "exit_code": None, "signals_sent": [], "status": "RUNNING"}
        self.result["commands"].append(row)
        self.save()  # 没有可落盘回执就不启动。
        child = None
        with (directory / "stdout.log").open("xb", buffering=0) as stdout, \
                (directory / "stderr.log").open("xb", buffering=0) as stderr:
            try:
                require(not self.stop, "启动前中止")
                child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr,
                                         start_new_session=True)
                row.update(pid=child.pid, start_new_session=True)
                self.persist()
                self.wait(child, row)
            except BaseException as error:
                row["error"] = repr(error)
                self.error("command:" + name, error)
            finally:
                if child is not None and not row["reaped"]:
                    self.wait(child, row)
                os.fsync(stdout.fileno())
                os.fsync(stderr.fileno())
        raw_out = read_regular(directory / "stdout.log")
        raw_err = read_regular(directory / "stderr.log")
        row.update(stdout_sha256=digest(raw_out), stderr_sha256=digest(raw_err),
                   stdout_bytes=len(raw_out), stderr_bytes=len(raw_err),
                   stdout=raw_out.decode(errors="replace"), stderr=raw_err.decode(errors="replace"),
                   status="REAPED" if row["reaped"] else "FAIL")
        self.save()
        require(row["reaped"] and "error" not in row, "命令启动/监督失败")
        return row, raw_out, raw_err


def bus_guard(side, owner, driver, binaries):
    require(not owner.stop, "停止后续总线")
    profile = driver.arm_profile(side)
    snapshot = driver.process_snapshot(-1, RUN.parent, binaries)  # 实现自身按 os.getpid() 跳过本进程。
    owner.result["bus_checks"].append({"side": side, "at_ns": time.time_ns(), "processes": snapshot})
    owner.save()
    require(snapshot["clean"] is True, "控制进程残留或身份不可读")
    row, raw, error = owner.command(side + "-udev", ["udevadm", "info", "--query=property", "--name", profile.device])
    require(row["exit_code"] == 0 and error == b"", "udev 查询失败或 stderr 非空")
    lines = raw.decode("utf-8", errors="strict").splitlines()
    for key, value in {"ID_VENDOR_ID": "1a86", "ID_MODEL_ID": "55d3",
                       "ID_SERIAL_SHORT": profile.usb_serial, "ID_USB_DRIVER": "cdc_acm"}.items():
        require([line for line in lines if line.startswith(key + "=")] == [key + "=" + value], "USB 身份不唯一/不符")
    metadata = Path(profile.device).stat()
    require(stat.S_ISCHR(metadata.st_mode) and os.access(profile.device, os.R_OK | os.W_OK), "设备类型/权限错误")
    for elevated in (False, True):
        argv = (["sudo", "-n"] if elevated else []) + ["fuser", profile.device]
        row, raw, error = owner.command(side + ("-fuser-sudo" if elevated else "-fuser"), argv)
        require(row["exit_code"] == 1 and raw == error == b"", "串口持有者非空或 fuser 查询错误")


def validate_raw_readback(step, row, stdout, stderr):
    """复用已实际读取 40 次的原帧校验；只增加原整数零门的结果派生。"""
    require(row["exit_code"] == step["expected_exit"] and stderr == b"", "读回退出或 stderr 不符")
    require(len(stdout.splitlines()) == 1 and stdout.endswith(b"\n"), "结果行数或换行错误")
    device, motor, register = step["argv"][1], step["motor_id"], step["register"]
    report = json.loads(stdout)
    address, width = {"Torque_Enable": (40, 1), "Present_Velocity": (58, 2)}[register]
    keys = {"operation", "serial_device", "motor_id", "register", "requested_register_address",
            "width", "raw_unsigned", "tx", "rx"}
    require(set(report) == keys and
            (report["operation"], report["serial_device"], report["motor_id"], report["register"],
             report["requested_register_address"], report["width"]) ==
            ("sync_read", device, motor, register, address, width), "JSON 身份或参数不一致")
    expected = [255, 255, 254, 5, 130, address, width, motor]
    expected.append((~sum(expected[2:])) & 255)
    rx, raw = report["rx"], report["raw_unsigned"]
    require(report["tx"] == expected and len(rx) == 6 + width and
            rx[:5] == [255, 255, motor, width + 2, 0] and (sum(rx[2:]) & 255) == 255 and
            int.from_bytes(bytes(rx[5:5 + width]), "little") == raw, "原始帧校验不一致")
    require(type(raw) is int, "原字非整数")
    integer = raw if register == "Torque_Enable" else (-(raw & 32767) if raw & 32768 else raw & 32767)
    require(integer == step["expected_integer"], "寄存器读回非零，请人工断电")
    return report, integer


def main(argv=None):
    require(not DRAFT_ONLY, "不可执行草稿：未来 ELF、授权和实际回收原件尚未由 root 绑定并审核")
    parser = argparse.ArgumentParser(description=__doc__, epilog="只用 uv python find 选解释器后 shell exec；三个 SHA 来自本次实际完成原件。")
    for name in ("run-sha256", "cleanup-sha256", "ssh-wait-sha256"):
        parser.add_argument("--" + name, required=True)
    options = parser.parse_args(argv)
    # 固定路径内置，防止 H2 output 进入 wrapper argv 污染相关进程门。
    require(digest(read_regular(Path(__file__).resolve().with_name("prelaunch_go.py"))) == GO_HELPER_SHA,
            "引用 GO helper 身份改变")
    for path in (BASE, ROOT, OUT):
        canonical(path)
    require(BASE.is_dir() and not OUT.exists() and not OUT.is_symlink(), "读回输出必须全新")
    driver, driver_hashes = tracked_driver(ROOT / "scripts/run_post_e_h2.py", ROOT, HEAD)
    inputs, go, plan = pinned(INPUTS, INPUTS_SHA), pinned(GO, GO_SHA), pinned(PLAN, PLAN_SHA)
    run = pinned(RUN, options.run_sha256)
    cleanup = pinned(CLEANUP, options.cleanup_sha256)
    ssh_wait = pinned(SSH_WAIT, options.ssh_wait_sha256)
    validate_plan(plan, driver)
    validate_evidence(options, driver, inputs, go, run, cleanup, ssh_wait)
    binary_identity = elf_identity()
    manifests = {kind: reference(inputs[kind]) for kind in ("production", "measurement", "installed")}
    binaries = [Path(row["path"]) for kind in ("production", "measurement")
                for row in manifests[kind]["artifacts"].values()]
    binaries += [Path(row["path"]) for name, row in manifests["installed"]["files"].items()
                 if name.startswith("installed:")]
    binaries.append(ELF)
    result = {"schema": "P7_H2_SERIAL_REGISTER_READBACK_V1", "status": "RUNNING", "read_only": True,
              "started_ns": time.time_ns(), "inputs_sha256": INPUTS_SHA, "go_sha256": GO_SHA,
              "plan_sha256": PLAN_SHA, "driver_run_sha256": options.run_sha256,
              "cleanup_sha256": options.cleanup_sha256, "ssh_wait_sha256": options.ssh_wait_sha256,
              "original_h2_status": run["status"], "original_h2_unchanged": True,
              "binary": binary_identity, "driver_helpers": driver_hashes,
              "deployable": False, "D15": "OPEN/DEFERRED", "physical_torque_state": "UNKNOWN",
              "limits": plan["limits"], "received_signals": [], "errors": [], "commands": [], "bus_checks": [],
              "steps": [{**step, "index": index, "status": "NOT_RUN"}
                        for index, step in enumerate(plan["steps"], 1)]}
    OUT.mkdir()
    owner = Owner(result)
    owner.install()
    current_side = None
    try:
        owner.save()
        for step in result["steps"]:
            if owner.stop:
                break
            if step["side"] != current_side:
                bus_guard(step["side"], owner, driver, binaries)
                current_side = step["side"]
            require(not owner.stop, "已停止后续读回")
            # 每条启动前重新校验固定 ELF/原始证据；不产生新的授权或现场事实。
            elf_identity()
            require(pinned(RUN, options.run_sha256) == run
                    and pinned(CLEANUP, options.cleanup_sha256) == cleanup
                    and pinned(SSH_WAIT, options.ssh_wait_sha256) == ssh_wait
                    and pinned(INPUTS, INPUTS_SHA) == inputs and pinned(GO, GO_SHA) == go,
                    "读回前 pinned 原件改变")
            step["status"] = "RUNNING"
            row, stdout, stderr = owner.command(f"read-{step['index']:02d}", step["argv"])
            step.update(pid=row["pid"], exit_code=row["exit_code"], reaped=row["reaped"],
                        stdout_sha256=row["stdout_sha256"], stderr_sha256=row["stderr_sha256"],
                        started_ns=row["started_ns"], finished_ns=row["finished_ns"])
            # 原 stdout/stderr 文件保持；原帧校验后才按原整数零门判定。
            report, integer = validate_raw_readback(step, row, stdout, stderr)
            step.update(raw_readback=report, decoded_integer=integer)
            step["status"] = "PASS"
            owner.save()
    except BaseException as error:
        owner.error("readback", error)
        for step in result["steps"]:
            if step["status"] == "RUNNING":
                step.update(status="FAIL", error=repr(error))
    finally:
        try:
            require(pinned(RUN, options.run_sha256) == run
                    and pinned(CLEANUP, options.cleanup_sha256) == cleanup, "读回后原 H2/清场证据改变")
        except BaseException as error:
            result["original_h2_unchanged"] = False
            owner.error("final_evidence", error)
        result.update(status="PASS" if all(step["status"] == "PASS" for step in result["steps"])
                      and not result["errors"] and not result["received_signals"] else "FAIL",
                      finished_ns=time.time_ns(), completed=sum(step["status"] == "PASS" for step in result["steps"]))
        owner.persist()
        owner.restore()
    print(json.dumps(result, ensure_ascii=False, allow_nan=False), flush=True)
    return 0 if result["status"] == "PASS" else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        print(json.dumps({"status": "FAIL", "mode": "PRE_READBACK_REFUSED", "read_only": True,
                          "error": repr(error)}, ensure_ascii=False), file=sys.stderr)
        raise SystemExit(2)
