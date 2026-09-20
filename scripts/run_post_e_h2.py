#!/usr/bin/env -S uv run --no-project python
"""左臂 H2 监督驱动；默认只输出计划，真实执行须本次明确事实与完整清单。"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import pwd
import re
import resource
import shutil
import signal
import stat
import subprocess
import sys
import time
import tomllib
import traceback

from post_e_h2_report import require, strict_json, verify_report

UNITS = tuple(f"xlerobot-{role}-{side}{'-arm' if role == 's0' else ''}.service"
              for role in ("gateway", "s1", "s0") for side in ("left", "right"))
DEVICE = "/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C121427-if00"
TRUST = Path("/var/lib/robot/xlerobot_left")
CONFIG = Path("/etc/robot/xlerobot_left_arm.toml")


def file_hash(path):
    path = Path(path)
    metadata = path.lstat()
    require(stat.S_ISREG(metadata.st_mode) and metadata.st_nlink == 1, f"非独立普通文件：{path}")
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def pinned_json(reference):
    path = Path(reference["path"])
    require(path.is_absolute() and path == path.resolve(), "清单路径必须绝对且无符号链接")
    require(file_hash(path) == reference["sha256"], f"清单 SHA 改变：{path}")
    return strict_json(path.read_text())


def clean_environment(original):
    blocked = {"NOTIFY_SOCKET", "INVOCATION_ID", "WATCHDOG_USEC", "WATCHDOG_PID",
               "ROBOT_ID", "ROBOT_GATEWAY_ADDR", "ROBOT_TOKEN"}
    result = {key: value for key, value in original.items()
              if not key.startswith("ROBOT_TEST_") and key not in blocked}
    result["RUST_LOG"] = "info"
    return result


def tool_command(bin_dir, output):
    return ["taskset", "-c", "0-3", str(bin_dir / "comm_fault_e2e"), "--phase", "orin-sla",
            "--bin-dir", str(bin_dir), "--evidence-dir", str(output), "--config", str(CONFIG),
            "--hardware-id", "left_arm", "--ack-supervised-hardware"]


def process_identity(pid, proc=Path("/proc")):
    """PID 与 boot/start_ticks 同时绑定；缺失或读错不能猜成正常退出。"""
    base = proc / str(pid)
    raw = (base / "stat").read_text()
    fields = raw[raw.rfind(")") + 2:].split()
    return {"pid": pid, "start_ticks": int(fields[19]), "pgid": int(fields[2]),
            "state": fields[0], "boot_id": (proc / "sys/kernel/random/boot_id").read_text().strip(),
            "exe": os.readlink(base / "exe"),
            "argv": (base / "cmdline").read_bytes().rstrip(b"\0").decode().split("\0")}


def process_snapshot(pgid, output, binaries, proc=Path("/proc")):
    """只读查本轮组、路径和三件可执行文件；不向进程发信号。"""
    related, unreadable = [], []
    binary_names = {str(path) for path in binaries}
    for path in proc.iterdir():
        if not path.name.isdigit() or int(path.name) == os.getpid():
            continue
        try:
            fields = (path / "stat").read_text().split(") ", 1)[1].split()
            group = int(fields[2])
            same_uid = path.stat().st_uid == os.getuid()
            if group != pgid and not same_uid:
                continue
            arguments = (path / "cmdline").read_bytes().rstrip(b"\0").decode().split("\0")
            same_run = str(output) in arguments
            # 成功 exec 后的 ELF 路径才是身份；argv 仅用于补查 exec 前窗口。
            executable = os.readlink(path / "exe") if fields[0] != "Z" else None
            if group == pgid or same_run or executable in binary_names or any(a in binary_names for a in arguments):
                if executable is None:
                    related.append({"pid": int(path.name), "pgid": group, "state": fields[0], "zombie": True})
                else:
                    related.append(process_identity(int(path.name), proc))
        except FileNotFoundError:
            continue
        except (OSError, ValueError, IndexError, UnicodeError) as error:
            unreadable.append({"pid": int(path.name), "error": repr(error)})
    return {"related": related, "unreadable": unreadable, "clean": not related and not unreadable}


def marker_snapshot(directory, expected=None, uid=None, gid=None):
    """只读双份严格字节与所有权；绝不直接写 marker。"""
    result = {}
    for name in ("rx_trust_marker", "rx_trust_marker.bak"):
        path = directory / name
        try:
            metadata = path.lstat()
        except FileNotFoundError:
            require(expected is None, f"缺少 marker：{name}")
            result[name] = {"missing": True}
            continue
        require(stat.S_ISREG(metadata.st_mode) and metadata.st_nlink == 1, f"marker 对象错误：{name}")
        with path.open("rb") as source:
            data = source.read(3)
        require(metadata.st_size <= 3, "marker 大小异常")
        result[name] = {"hex": data.hex(), "uid": metadata.st_uid, "gid": metadata.st_gid,
                        "mode": stat.S_IMODE(metadata.st_mode), "sha256": hashlib.sha256(data).hexdigest()}
        require(metadata.st_uid == uid and metadata.st_gid == gid and stat.S_IMODE(metadata.st_mode) == 0o600,
                f"marker 权限/属主错误：{name}")
        if expected is not None:
            require(data == expected, f"marker 未处于预期阶段：{name}")
    return result


def validate_services(output):
    found = {}
    for block in output.strip().split("\n\n"):
        fields = {}
        for line in block.splitlines():
            key, value = line.split("=", 1)
            require(key not in fields, "服务字段重复")
            fields[key] = value
        name = fields["Id"]
        require(name not in found, "服务 Id 重复")
        found[name] = fields
    require(set(found) == set(UNITS), "六服务清单缺失/多项")
    for name, row in found.items():
        require(row["LoadState"] == "loaded" and row["ActiveState"] == "inactive"
                and row["SubState"] == "dead" and row["MainPID"] == "0", f"服务未停稳：{name}")
    return found


def validate_facts(inputs, alignment, physical, options, now_ns):
    """回执由 root 收到本次事实后生成；校验绑定，不替代现场确认。"""
    require(inputs["schema"] == "P7_H2_INPUTS_V1" and inputs["expected_head"] == options.expected_head
            and inputs["output"] == str(options.output), "运行输入没有绑定本次目录/HEAD")
    require(alignment["schema"] == "P7_H2_ALIGNMENT_V1" and alignment["expected_head"] == options.expected_head,
            "两端对齐回执不符")
    hosts = alignment["hosts"]
    require(len(hosts) == 2 and {row["role"] for row in hosts} == {"local", "orin"}, "两端身份不完整")
    for row in hosts:
        require(row["head"] == options.expected_head and row["status_porcelain"] == ""
                and row["source_manifest_sha256"] == inputs["source_manifest_sha256"], "两端未同HEAD/clean/源")
        require(row["host"] and row["boot_id"], "对齐回执缺主机身份")
    require(0 < alignment["recorded_at_ns"] <= now_ns, "对齐时刻错误")
    require(physical["schema"] == "P7_H2_PHYSICAL_CONFIRMATION_V1" and physical["side"] == "left"
            and physical["issued_by"] == "root" and physical["expected_head"] == options.expected_head
            and physical["output"] == str(options.output), "现场回执没有绑定本次左臂运行")
    require(physical["measurement_manifest_sha256"] == inputs["measurement"]["sha256"]
            and physical["alignment_sha256"] == inputs["alignment"]["sha256"], "现场回执绑定清单错误")
    require(alignment["recorded_at_ns"] <= physical["confirmed_at_ns"] <= now_ns
            and isinstance(physical["source"], str) and physical["source"].strip(), "缺本次现场时间/事实来源")
    for key in ("supported", "unloaded", "area_clear", "motor_power_cycle", "usb_replug",
                "continuous_presence_until_cleanup", "immediate_power_cut"):
        require(physical[key] is True, f"缺少现场事实：{key}")


class RealBackend:
    """仅显式 execute 使用；构造不运行命令，子进程所有权先于句柄交接。"""
    def __init__(self, options, journal):
        self.options, self.journal = options, journal
        self.process = None
        self.launching = False
        self.log = None
        self.inputs = None
        self.environment = clean_environment(os.environ)
        self.identities = {}

    def command(self, argv, allowed=(0,)):
        row = {"argv": [str(a) for a in argv], "started_ns": time.time_ns()}
        self.journal.state["steps"].append(row)
        result = subprocess.run(row["argv"], capture_output=True, text=True, timeout=30,
                                env=self.environment, check=False)
        row.update(exit_code=result.returncode, stdout=result.stdout, stderr=result.stderr,
                   finished_ns=time.time_ns())
        self.journal.save()
        require(result.returncode in allowed, f"命令失败：{row['argv']} exit={result.returncode}")
        return result.stdout

    def source_check(self):
        import post_e_h2_artifacts as artifacts
        root = Path(self.inputs["repository"])
        require(root.is_absolute() and root == root.resolve(), "源码根路径不规范")
        head = self.command(["git", "-C", root, "rev-parse", "HEAD"]).strip()
        status = self.command(["git", "-C", root, "status", "--porcelain=v1", "--untracked-files=all"])
        require(head == self.options.expected_head and status == "", "实际运行端 HEAD/clean 不符")
        names = self.command(["git", "-C", root, "ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        source = artifacts.snapshot_source(root, names.rstrip("\0").split("\0"))
        require(source == pinned_json(self.inputs["source"])
                and source["source_manifest_sha256"] == self.inputs["source_manifest_sha256"], "当前编译源发生变化")
        orin = next(r for r in self.alignment["hosts"] if r["role"] == "orin")
        require(orin["host"] == os.uname().nodename and orin["boot_id"] ==
                Path("/proc/sys/kernel/random/boot_id").read_text().strip(), "运行端不是对齐回执中的 Orin")
        self.source = source
        return {"head": head, "status_porcelain": status, "source_manifest_sha256": source["source_manifest_sha256"]}

    def artifact_check(self):
        import post_e_h2_artifacts as artifacts
        result = {}
        for kind in ("production", "measurement"):
            manifest = pinned_json(self.inputs[kind])
            ancestor = False
            if manifest["build_head"] != self.options.expected_head:
                self.command(["git", "-C", self.inputs["repository"], "merge-base", "--is-ancestor",
                              manifest["build_head"], self.options.expected_head])
                for name in ("run_post_e_h2.py", "post_e_h2_report.py", "post_e_h2_artifacts.py"):
                    frozen_text = self.command(["git", "-C", self.inputs["repository"], "show",
                                                manifest["build_head"] + ":scripts/" + name])
                    require(frozen_text.encode() == (Path(self.inputs["repository"]) / "scripts" / name).read_bytes(),
                            "继承构建时驱动/报告/共享校验输入改变")
                ancestor = True
            result[kind] = artifacts.validate_artifact_set(manifest, source_manifest=self.source,
                expected_kind=kind, expected_head=self.options.expected_head, build_ancestor_verified=ancestor)
            setattr(self, kind, manifest)
        self.bin_dir = Path(self.measurement["artifact_directory"])
        require(self.bin_dir != Path(self.production["artifact_directory"]), "生产/测量冻结目录复用")
        return result

    def formal_check(self):
        import post_e_h2_artifacts as artifacts
        expected = pinned_json(self.inputs["installed"])
        actual = artifacts.snapshot_installed(expected)
        artifacts.validate_installed_snapshot(actual, expected)
        frozen = [Path(row["path"]) for manifest in (self.production, self.measurement)
                  for row in manifest["artifacts"].values()]
        for name in artifacts.INSTALLED_NAMES:
            installed = Path(actual["files"]["installed:" + name]["path"])
            require(all(not installed.samefile(path) for path in frozen), "冻结件与正式安装件重合")
        return actual

    def config_check(self):
        require(file_hash(CONFIG) == self.inputs["config_sha256"] ==
                file_hash(Path(self.inputs["repository"]) / "configs/xlerobot_left_arm.toml"), "D2配置身份不符")
        config = tomllib.loads(CONFIG.read_text())
        require(config["robot"]["id"] == "xlerobot_left" and len(config["hardware"]) == 1, "非左臂独占配置")
        hw = config["hardware"][0]
        require(hw["id"] == "left_arm" and hw["address"] == DEVICE
                and hw["adapter"] == "feetech_sts3215" and hw["motor_ids"] == list(range(1, 7))
                and hw["auto_enable"] is False and len(hw["calibration_paths"]) == 1, "D2硬件字段改变")
        calibration = Path(hw["calibration_paths"][0])
        require(calibration.is_absolute() and calibration.name == "left_arm.json"
                and str(calibration) == self.inputs["calibration"]["path"]
                and file_hash(calibration) == self.inputs["calibration"]["sha256"], "Python标定身份改变")
        strict_json(calibration.read_text())
        return {"config_sha256": file_hash(CONFIG), "calibration_sha256": file_hash(calibration)}

    def device_check(self):
        lines = self.command(["udevadm", "info", "--query=property", "--name", DEVICE]).splitlines()
        for key, value in {"ID_VENDOR_ID": "1a86", "ID_MODEL_ID": "55d3",
                           "ID_SERIAL_SHORT": "5A7C121427", "ID_USB_DRIVER": "cdc_acm"}.items():
            require([line for line in lines if line.startswith(key + "=")] == [key + "=" + value], "USB身份不唯一/不符")
        info = Path(DEVICE).stat()
        require(stat.S_ISCHR(info.st_mode) and os.access(DEVICE, os.R_OK | os.W_OK), "串口类型/权限不符")
        output = self.command(["fuser", DEVICE], allowed=(1,))
        last = self.journal.state["steps"][-1]
        require(not output.strip() and not last["stderr"].strip(), "fuser错误不能当成空闲")
        return {"path": DEVICE, "resolved": str(Path(DEVICE).resolve()), "rdev": info.st_rdev,
                "uid": info.st_uid, "gid": info.st_gid, "mode": stat.S_IMODE(info.st_mode)}

    def preflight(self):
        self.inputs = pinned_json({"path": str(self.options.inputs), "sha256": self.options.inputs_sha256})
        self.alignment = pinned_json(self.inputs["alignment"])
        physical = pinned_json(self.inputs["physical"])
        validate_facts(self.inputs, self.alignment, physical, self.options, time.time_ns())
        account = pwd.getpwnam("loosqk")
        self.uid, self.gid = account.pw_uid, account.pw_gid
        require(os.getuid() == os.geteuid() == self.uid and os.getgid() == self.gid, "必须以 loosqk 实际身份运行")
        require(os.uname().machine == "aarch64", "真实H2只允许原生Orin")
        for name in ("taskset", "git", "systemctl", "udevadm", "fuser", "sha256sum"):
            require(shutil.which(name), f"缺少必要工具：{name}")
        limits = {"rtprio": resource.getrlimit(resource.RLIMIT_RTPRIO),
                  "memlock": resource.getrlimit(resource.RLIMIT_MEMLOCK)}
        require(limits["rtprio"][0] == resource.RLIM_INFINITY or limits["rtprio"][0] >= 70, "实际身份没有FIFO70权限")
        require(limits["memlock"][0] == resource.RLIM_INFINITY, "实际身份没有无限MEMLOCK预算")
        self.journal.state.update(physical=physical, alignment=self.alignment, limits=limits,
                                  actual_uid=self.uid, actual_gid=self.gid, environment_keys=sorted(self.environment))
        require(TRUST.is_dir() and not TRUST.is_symlink() and TRUST.stat().st_uid == self.uid
                and stat.S_IMODE(TRUST.stat().st_mode) == 0o750, "trust目录权限不符")

    def audit(self):
        require(self.inputs is not None, "未通过输入预检")
        require(pinned_json({"path": str(self.options.inputs), "sha256": self.options.inputs_sha256}) == self.inputs, "输入变化")
        require(pinned_json(self.inputs["alignment"]) == self.alignment, "两端对齐证据变化")
        require(pinned_json(self.inputs["physical"]) == self.journal.state["physical"], "物理事实回执变化")
        source = self.source_check()
        artifacts = self.artifact_check()
        formal = self.formal_check()
        config = self.config_check()
        device = self.device_check()
        if self.process is None:
            self.cleanup()
        self.journal.state["markers_observed"] = self.markers(None)
        return {"source": source, "artifacts": artifacts, "formal": formal, "config": config, "device": device}

    def trust(self, action):
        require(action in ("status", "revoke", "authorize"), "未知维护动作")
        return self.command([self.production["artifacts"]["s0_daemon"]["path"], "rx-trust", action,
                             "--robot-id", "xlerobot_left", "--rx-trust-dir", str(TRUST)])

    def markers(self, expected):
        return marker_snapshot(TRUST, expected, self.uid, self.gid)

    def launch(self):
        require(self.process is None, "禁止第二次启动")
        self.log = (self.options.output / "tool.log").open("xb")
        argv = tool_command(self.bin_dir, self.options.output)
        self.journal.state.update(tool_argv=argv, tool_launch_ns=time.time_ns())
        self.journal.save()
        require(not self.journal.state["failures"], "启动记录失败，拒绝Popen")
        # 信号处理器在此窗口只记失败、不抛异常；子进程继承正常信号掩码。
        self.launching = True
        try:
            self.process = subprocess.Popen(argv, stdout=self.log, stderr=subprocess.STDOUT,
                env=self.environment, start_new_session=True)
        finally:
            self.launching = False
        return self.process

    def identity(self, pid):
        return process_identity(pid)

    def observe(self, process, journal):
        if process.poll() is not None:
            return
        snapshot = process_snapshot(process.pid, self.options.output,
                                    [self.bin_dir / name for name in self.measurement["artifacts"]])
        require(not snapshot["unreadable"], "本轮进程身份不可读")
        for row in snapshot["related"]:
            key = f"{row['pid']}:{row.get('start_ticks')}"
            if key not in self.identities:
                self.identities[key] = row
                journal.state["observed_processes"] = list(self.identities.values())
                journal.save()

    def report(self, pid):
        expected = {"bin_dir": str(self.bin_dir), "config": str(CONFIG),
                    "config_sha256": self.inputs["config_sha256"],
                    "calibration_sha256": self.inputs["calibration"]["sha256"], "tool_pid": pid,
                    "artifacts": {name: {"sha256": row["sha256"], "build": pinned_json(row["sidecar"])}
                                  for name, row in self.measurement["artifacts"].items()}}
        return verify_report(self.options.output, expected)

    def cleanup(self):
        require(hasattr(self, "measurement"), "未建立本轮产物身份，无法完成清场核验")
        value = process_snapshot(self.process.pid if self.process else -1, self.options.output,
                                 [self.bin_dir / name for name in self.measurement["artifacts"]])
        require(value["clean"], f"清场存在残留或不可读身份：{value}")
        return value

    def close(self):
        if self.log is not None:
            self.log.close()


class Journal:
    def __init__(self, output, options):
        self.output = output
        self.state = {"schema": "P7_H2_RUN_V1", "status": "RUNNING", "started_ns": time.time_ns(),
                      "expected_head": options.expected_head, "inputs_sha256": options.inputs_sha256,
                      "supervisor_pid": os.getpid(), "steps": [], "failures": [],
                      "authorization_attempts": 0, "launch_attempts": 0, "deployable": False, "D15": "OPEN"}

    def save(self):
        try:
            self._save()
        except OSError as error:
            self.state["status"] = "FAIL"
            self.state["journal_io_failure"] = repr(error)
            if not self.state["failures"] or self.state["failures"][-1].get("error") != repr(error):
                self.state["failures"].append({"at_ns": time.time_ns(), "error": repr(error)})

    def _save(self):
        temp = self.output / "run.json.tmp"
        with temp.open("w") as stream:
            json.dump(self.state, stream, ensure_ascii=False, indent=2)
            stream.write("\n"); stream.flush(); os.fsync(stream.fileno())
        temp.replace(self.output / "run.json")

    def fail(self, error):
        self.state["status"] = "FAIL"
        self.state["failures"].append({"at_ns": time.time_ns(), "error": str(error)})
        self.save()


def supervise(process, backend, journal):
    """一旦启动，异常也保留监督并真实 wait；从不重启或 kill 整组。"""
    failures = set()
    while True:
        try:
            backend.observe(process, journal)
        except Exception as error:
            message = f"进程观察失败：{error!r}"
            if message not in failures:
                failures.add(message); journal.fail(message)
        try:
            result = process.wait(timeout=1)
            journal.state["tool_exit"] = result
            journal.state["tool_naturally_reaped"] = True
            journal.state["tool_ended_ns"] = time.time_ns()
            journal.save()
            return result
        except subprocess.TimeoutExpired:
            continue
        except (KeyboardInterrupt, InterruptedError) as error:
            journal.fail(f"监督收到中断，继续等待工具实际收尾：{error!r}")


def execute(options, backend_factory):
    # 独占目录即消费记录，任何失败也不复用。校验必需参数在此之前完成。
    options.output.mkdir(exist_ok=False)
    journal = Journal(options.output, options)
    backend = backend_factory(options, journal)
    process = None
    prior_handlers = {}

    def interrupted(signum, _frame):
        journal.fail(f"收到信号 {signum}；不转发、不重授权，已启动工具继续受监督")
        if process is None and backend.process is None and not getattr(backend, "launching", False):
            raise InterruptedError(f"启动前收到信号 {signum}")

    try:
        journal.save()
        for sig in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
            prior_handlers[sig] = signal.signal(sig, interrupted)
        backend.preflight()
        journal.state["before"] = backend.audit()
        backend.trust("status")
        backend.trust("revoke")
        journal.state["markers_revoked"] = backend.markers(b"1\n")
        backend.trust("status")
        journal.state["authorization_attempts"] += 1
        journal.save()
        require(not journal.state["failures"], "授权前已有失败")
        backend.trust("authorize")
        journal.state["markers_authorized"] = backend.markers(b"0\n")
        backend.trust("status")
        journal.state["launch_attempts"] += 1
        journal.save()
        require(not journal.state["failures"], "启动前已有失败")
        process = backend.launch()
        journal.state["tool_pid"] = process.pid
        # 身份采集/落证失败仍保 process，必须进入 supervise 完成真实 wait。
        try:
            journal.state["launch_identity"] = backend.identity(process.pid)
            journal.save()
        except Exception as error:
            journal.fail(f"已启动但身份记录失败：{error!r}")
        exit_code = supervise(process, backend, journal)
        require(exit_code == 0, f"工具原退出码 {exit_code}，保留原始失败")
        journal.state["report"] = backend.report(process.pid)
        require(not journal.state["failures"], "本轮已记录失败，不能由后续报告覆盖")
        journal.state["status"] = "PASS"
    except BaseException as error:
        process = process if process is not None else backend.process
        if process is not None:
            journal.state["tool_pid"] = process.pid
            if "launch_identity" not in journal.state:
                try:
                    journal.state["launch_identity"] = backend.identity(process.pid)
                except Exception as identity_error:
                    journal.fail(f"异常启动身份记录失败：{identity_error!r}")
        journal.fail(f"{error!r}\n{traceback.format_exc()}")
        if process is not None and process.poll() is None:
            journal.state["needs_owner_supervision"] = True
            supervise(process, backend, journal)
        elif process is not None and not journal.state.get("tool_naturally_reaped"):
            journal.state["tool_exit"] = process.wait()
            journal.state["tool_naturally_reaped"] = True
        if journal.state["authorization_attempts"] and process is None:
            try:
                backend.trust("revoke")
                journal.state["unused_authorization_revoked"] = backend.markers(b"1\n")
            except BaseException as revoke_error:
                journal.fail(f"未消费授权撤销失败，须人工处置：{revoke_error!r}")
    finally:
        for name, check in (("after", backend.audit), ("cleanup", backend.cleanup),
                            ("markers_after", lambda: backend.markers(b"1\n"))):
            try:
                journal.state[name] = check()
            except BaseException as error:
                journal.fail(f"最终核验 {name} 失败：{error!r}")
        try:
            backend.close()
        except Exception as close_error:
            journal.fail(f"关闭日志失败：{close_error!r}")
        journal.state["finished_ns"] = time.time_ns()
        journal.save()
        for sig, previous in prior_handlers.items():
            signal.signal(sig, previous)
    return 0 if journal.state["status"] == "PASS" else 1


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--execute", action="store_true")
    parser.add_argument("--physical-reset-confirmed", action="store_true")
    parser.add_argument("--expected-head")
    parser.add_argument("--inputs", type=Path)
    parser.add_argument("--inputs-sha256")
    parser.add_argument("--output", type=Path)
    return parser.parse_args(argv)


def validate_options(options):
    require(options.physical_reset_confirmed, "缺少本次现场确认参数")
    require(re.fullmatch(r"[0-9a-f]{40}", options.expected_head or ""), "expected-head 必须为完整提交 SHA")
    require(re.fullmatch(r"[0-9a-f]{64}", options.inputs_sha256 or ""), "缺少完整 inputs SHA")
    for name in ("inputs", "output"):
        value = getattr(options, name)
        require(value is not None and value.is_absolute() and ".." not in value.parts and value == value.resolve(),
                f"{name} 必须是明确绝对路径")
    require(sys.version_info >= (3, 11), "驱动需要 uv 选择 Python >=3.11")


def main(argv=None, backend_factory=None):
    options = parse_args(argv)
    if not options.execute:
        print(json.dumps({"mode": "PLAN_ONLY", "side": "left", "deployable": False,
                          "steps": ["新run/HEAD/两端clean/源/两套产物/正式安装身份/六服务核验",
                                    "本次现场事实、D2/标定/USB/权限/无持有者",
                                    "status→revoke→status→一次authorize→status→一次工具启动",
                                    "原四组与真实final_stop/自然wait/前后身份及清场核验"],
                          "required": ["--execute", "--physical-reset-confirmed", "--expected-head",
                                       "--inputs", "--inputs-sha256", "--output"]}, ensure_ascii=False))
        return 0
    try:
        validate_options(options)
        return execute(options, backend_factory or RealBackend)
    except (OSError, ValueError, KeyError) as error:
        print(f"执行前拒绝：{error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
