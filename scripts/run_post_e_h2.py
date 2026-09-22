#!/usr/bin/env -S uv run --no-project python
"""双臂 H2 监督驱动；默认只输出计划，真实执行须本次明确事实与完整清单。"""
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

from post_e_h2_report import arm_profile, require, strict_json, verify_report

UNITS = tuple(f"xlerobot-{role}-{side}{'-arm' if role == 's0' else ''}.service"
              for role in ("gateway", "s1", "s0") for side in ("left", "right"))
# 旧只读 helper 的左侧默认别名；真实运行只用显式 profile。
DEVICE = arm_profile().device
TRUST = arm_profile().trust
CONFIG = arm_profile().config


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


def tool_command(bin_dir, output, side="left"):
    profile = arm_profile(side)
    return ["taskset", "-c", "0-3", str(bin_dir / "comm_fault_e2e"), "--phase", "orin-sla",
            "--bin-dir", str(bin_dir), "--evidence-dir", str(output), "--config", str(profile.config),
            "--hardware-id", profile.hardware_id, "--ack-supervised-hardware"]


# 同一份固定只读代码供普通进程读取和受限 sudo 探针使用；默认导入仅定义函数。
# stdin 只接受已采集的 PID 身份，不能选择路径、命令或写操作。
PROC_PROBE_CODE = r'''
import json
import os
import sys

def _read_at(directory, name, limit):
    descriptor = os.open(name, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW, dir_fd=directory)
    try:
        data = bytearray()
        while len(data) <= limit:
            part = os.read(descriptor, min(8192, limit + 1 - len(data)))
            if not part:
                return bytes(data)
            data.extend(part)
        raise ValueError("proc字段超过读取上限")
    finally:
        os.close(descriptor)

def _stat_at(directory, pid):
    raw = _read_at(directory, "stat", 8192).decode()
    opening, closing = raw.find("("), raw.rfind(")")
    if opening < 1 or closing <= opening or int(raw[:opening].strip()) != pid:
        raise ValueError("proc stat PID格式错误")
    fields = raw[closing + 2:].split()
    start = int(fields[19])
    if start < 0:
        raise ValueError("proc start_ticks不能为负")
    return {"pid": pid, "start_ticks": start, "pgid": int(fields[2]), "state": fields[0]}

def _uids_at(directory):
    rows = [line.split()[1:] for line in _read_at(directory, "status", 65536).decode().splitlines()
            if line.startswith("Uid:")]
    if len(rows) != 1 or len(rows[0]) != 4:
        raise ValueError("proc Uid字段不完整")
    values = [int(value) for value in rows[0]]
    if any(value < 0 for value in values):
        raise ValueError("proc Uid无效")
    return values

def _boot(root):
    with open(root + "/sys/kernel/random/boot_id", encoding="ascii") as stream:
        value = stream.read(128).strip()
    if len(value) != 36 or any(c not in "0123456789abcdef-" for c in value):
        raise ValueError("proc boot_id格式错误")
    return value

def _stable(left, right):
    return all(left[key] == right[key] for key in ("pid", "start_ticks", "boot_id", "pgid", "uids"))

def _proc_probe(pid, expected=None, details=True, root="/proc"):
    result = {"pid": pid}
    directory = None
    try:
        directory = os.open(root + "/" + str(pid), os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC | os.O_NOFOLLOW)
        before = _stat_at(directory, pid)
        before.update(boot_id=_boot(root), uids=_uids_at(directory))
        result["before"] = before
        if expected is not None and not _stable(before, expected):
            return dict(result, status="replaced")
        identity = dict(before)
        if details:
            identity["argv"] = _read_at(directory, "cmdline", 65536).rstrip(b"\0").decode().split("\0")
            identity["exe"] = None if before["state"] == "Z" else os.readlink("exe", dir_fd=directory)
            identity["zombie"] = before["state"] == "Z"
        after = _stat_at(directory, pid)
        after.update(boot_id=_boot(root), uids=_uids_at(directory))
        result["after"] = after
        if not _stable(before, after):
            return dict(result, status="replaced")
        return dict(result, status="observed", identity=identity)
    except FileNotFoundError as error:
        # exe缺失不等于PID退出；只在同一dirfd的stat也消失时报告vanished。
        if directory is not None:
            try:
                result["remaining_stat"] = _stat_at(directory, pid)
            except FileNotFoundError:
                return dict(result, status="vanished", error=repr(error))
            except Exception as recheck_error:
                return dict(result, status="unreadable", error=repr(recheck_error))
            return dict(result, status="unreadable", error=repr(error))
        return dict(result, status="vanished", error=repr(error))
    except PermissionError as error:
        return dict(result, status="permission", error=repr(error))
    except (OSError, ValueError, KeyError, IndexError, UnicodeError) as error:
        return dict(result, status="unreadable", error=repr(error))
    finally:
        if directory is not None:
            os.close(directory)

def _probe_main():
    text = sys.stdin.read(131073)
    if len(text) > 131072:
        raise ValueError("PID批次输入过大")
    request = json.loads(text)
    if set(request) != {"schema", "candidates"} or request["schema"] != "P7_PROC_PROBE_V1":
        raise ValueError("PID探针请求格式错误")
    candidates = request["candidates"]
    if not isinstance(candidates, list) or not 1 <= len(candidates) <= 256:
        raise ValueError("PID批次超限")
    pids = set()
    for expected in candidates:
        if set(expected) != {"pid", "start_ticks", "boot_id", "pgid", "state", "uids"}:
            raise ValueError("PID身份字段错误")
        pid = expected["pid"]
        if type(pid) is not int or pid <= 0 or pid in pids:
            raise ValueError("PID无效或重复")
        if type(expected["start_ticks"]) is not int or expected["start_ticks"] < 0:
            raise ValueError("start_ticks无效")
        if type(expected["pgid"]) is not int or expected["pgid"] < 0:
            raise ValueError("PGID无效")
        if not isinstance(expected["uids"], list) or len(expected["uids"]) != 4 or any(type(u) is not int or u < 0 for u in expected["uids"]):
            raise ValueError("Uid无效")
        if not isinstance(expected["state"], str) or len(expected["state"]) != 1 or not isinstance(expected["boot_id"], str):
            raise ValueError("PID状态/boot错误")
        pids.add(pid)
    print(json.dumps({"schema": "P7_PROC_PROBE_V1", "reader_euid": os.geteuid(), "boot_id": _boot("/proc"),
                      "rows": [_proc_probe(row["pid"], row) for row in candidates]}, ensure_ascii=False))

if __name__ == "__main__":
    _probe_main()
'''
_PROC_READER = {"__name__": "p7_fixed_proc_reader"}
exec(PROC_PROBE_CODE, _PROC_READER)


def read_process(pid, expected=None, details=True, proc=Path("/proc")):
    return _PROC_READER["_proc_probe"](pid, expected, details, str(proc))


def process_identity(pid, proc=Path("/proc")):
    row = read_process(pid, proc=proc)
    require(row["status"] == "observed", f"进程身份不稳定或不可读：{row}")
    return row["identity"]


def privileged_process_probe(candidates):
    """最多一次有界只读PID批次；不导入项目代码，不继承Python加载环境。"""
    argv = ["sudo", "-n", "/usr/bin/python3", "-I", "-S", "-c", PROC_PROBE_CODE]
    request = {"schema": "P7_PROC_PROBE_V1", "candidates": candidates}
    row = {"argv_prefix": argv[:6], "code_sha256": hashlib.sha256(PROC_PROBE_CODE.encode()).hexdigest(),
           "request": request, "started_ns": time.time_ns()}
    try:
        completed = subprocess.run(argv, input=json.dumps(request), capture_output=True, text=True,
                                   timeout=10, check=False, env={"PATH": "/usr/sbin:/usr/bin:/sbin:/bin", "LC_ALL": "C"})
        row.update(exit_code=completed.returncode, stdout=completed.stdout, stderr=completed.stderr)
        require(completed.returncode == 0 and not completed.stderr.strip(), "只读PID探针未成功")
        row["response"] = strict_json(completed.stdout)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        row["error"] = repr(error)
    row["finished_ns"] = time.time_ns()
    return row


def process_snapshot(pgid, output, binaries, proc=Path("/proc"), *, probe_runner=None, reader=None):
    """普通身份先读；只为已绑定身份的EACCES候选提权核实，不忽略未知。"""
    related, unreadable, vanished, denied, candidates = [], [], [], [], []
    read = reader or read_process
    binary_names = {str(path) for path in binaries}

    def classify(row):
        identity = row["identity"]
        arguments = identity["argv"]
        if (identity["pgid"] == pgid or str(output) in arguments or identity["exe"] in binary_names
                or any(argument in binary_names for argument in arguments)):
            related.append(identity)

    for path in proc.iterdir():
        if not path.name.isdigit() or int(path.name) == os.getpid():
            continue
        pid = int(path.name)
        basic = read(pid, details=False, proc=proc)
        if basic["status"] == "vanished":
            vanished.append(basic)
            continue
        if basic["status"] != "observed":
            unreadable.append(basic)
            continue
        identity = basic["identity"]
        if identity["pgid"] != pgid and os.getuid() not in identity["uids"]:
            continue
        row = read(pid, expected=identity, proc=proc)
        if row["status"] == "observed":
            classify(row)
        elif row["status"] == "permission":
            denied.append(row)
            candidates.append(identity)
        elif row["status"] == "vanished":
            vanished.append(row)
        else:
            unreadable.append(row)
    probe = None
    if candidates:
        try:
            require(len(candidates) <= 256, "拒读PID数量超过有限批次")
            # 临时proc夹具不会意外访问宿主机sudo；测试必须显式提供后端。
            require(proc == Path("/proc") or probe_runner is not None, "非真实proc需要显式只读探针夹具")
            probe = (probe_runner or privileged_process_probe)(candidates)
            require(isinstance(probe, dict) and not probe.get("error") and probe.get("exit_code") == 0, "只读PID探针失败")
            response = probe["response"]
            require(isinstance(response, dict) and response["schema"] == "P7_PROC_PROBE_V1" and response["reader_euid"] == 0
                    and all(response["boot_id"] == row["boot_id"] for row in candidates), "探针格式/权限/boot错误")
            rows = response["rows"]
            require(isinstance(rows, list) and all(isinstance(r, dict) for r in rows)
                    and len(rows) == len(candidates) and [r["pid"] for r in rows] == [r["pid"] for r in candidates], "探针PID批次不对应")
            for expected, row in zip(candidates, rows):
                status = row["status"]
                if status == "observed":
                    require(isinstance(row.get("identity"), dict) and isinstance(row.get("before"), dict)
                            and isinstance(row.get("after"), dict), "探针缺完整身份记录")
                    require(_PROC_READER["_stable"](expected, row["before"])
                            and _PROC_READER["_stable"](expected, row["after"])
                            and _PROC_READER["_stable"](expected, row["identity"]), "探针PID/start/boot身份不对应")
                    require(isinstance(row["identity"]["argv"], list)
                            and all(isinstance(a, str) for a in row["identity"]["argv"])
                            and (isinstance(row["identity"]["exe"], str) and row["identity"]["exe"].startswith("/")
                                 or row["identity"]["exe"] is None and row["identity"].get("zombie") is True and row["identity"]["state"] == "Z"), "探针exe/argv不完整")
                    classify(row)
                elif status == "vanished":
                    require("before" not in row or _PROC_READER["_stable"](expected, row["before"]), "消失回执身份不符")
                    vanished.append({"expected": expected, "probe": row})
                else:
                    unreadable.append({"expected": expected, "probe": row})
        except (OSError, ValueError, KeyError, TypeError, IndexError, subprocess.SubprocessError) as error:
            unreadable.extend({"pid": row["pid"], "expected": row, "error": repr(error)} for row in candidates)
    return {"related": related, "unreadable": unreadable, "vanished": vanished,
            "ordinary_denied": denied, "privileged_probe": probe, "clean": not related and not unreadable}


def inspect_runtime_preconditions(trust=TRUST):
    """当前普通运行身份的只读前置；不建目录、不碰marker、不调用任何子进程。"""
    account = pwd.getpwnam("loosqk")
    info = trust.lstat()
    return {"schema": "P7_RUNTIME_PRECONDITIONS_V1", "uid": os.getuid(), "euid": os.geteuid(),
            "gid": os.getgid(), "egid": os.getegid(), "account_uid": account.pw_uid, "account_gid": account.pw_gid,
            "machine": os.uname().machine, "python_version": list(sys.version_info[:3]),
            "rtprio": list(resource.getrlimit(resource.RLIMIT_RTPRIO)),
            "memlock": list(resource.getrlimit(resource.RLIMIT_MEMLOCK)),
            "tools": {name: shutil.which(name) for name in ("taskset", "git", "systemctl", "udevadm", "fuser", "sha256sum", "sudo")},
            "trust": {"path": str(trust), "mode": stat.S_IMODE(info.st_mode), "uid": info.st_uid,
                      "gid": info.st_gid, "device": info.st_dev, "inode": info.st_ino,
                      "is_directory": stat.S_ISDIR(info.st_mode), "is_symlink": stat.S_ISLNK(info.st_mode)}}


def validate_runtime_preconditions(value, trust=TRUST):
    require(value["schema"] == "P7_RUNTIME_PRECONDITIONS_V1", "运行前置schema不符")
    require(value["uid"] == value["euid"] == value["account_uid"]
            and value["gid"] == value["egid"] == value["account_gid"], "必须以loosqk实际身份运行")
    require(value["machine"] == "aarch64" and value["python_version"] >= [3, 11, 0], "运行架构/Python版本不符")
    require(set(value["tools"]) == {"taskset", "git", "systemctl", "udevadm", "fuser", "sha256sum", "sudo"}
            and all(value["tools"].values()), "缺少必要运行工具")
    require(value["rtprio"][0] == resource.RLIM_INFINITY or value["rtprio"][0] >= 70, "实际身份没有FIFO70权限")
    require(value["memlock"][0] == resource.RLIM_INFINITY, "实际身份没有无限MEMLOCK预算")
    require(value["trust"]["path"] == str(trust), "运行前置TRUST路径与所选侧不符")
    trust = value["trust"]
    require(trust["is_directory"] and not trust["is_symlink"] and trust["uid"] == value["account_uid"]
            and trust["gid"] == value["account_gid"] and trust["mode"] == 0o750, "trust目录权限不符")
    return value


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
    profile = arm_profile(getattr(options, "side", "left"))
    require(arm_profile(inputs.get("side", "left")) == profile, "运行输入臂侧与CLI不符")
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
    require(physical["schema"] == "P7_H2_PHYSICAL_CONFIRMATION_V1" and physical["side"] == profile.side
            and physical["issued_by"] == "root" and physical["expected_head"] == options.expected_head
            and physical["output"] == str(options.output), "现场回执没有绑定本次所选臂运行")
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
        self.profile = arm_profile(getattr(options, "side", "left"))
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
        require(file_hash(self.profile.config) == self.inputs["config_sha256"] ==
                file_hash(Path(self.inputs["repository"]) / self.profile.repository_config), "D2配置身份不符")
        config = tomllib.loads(self.profile.config.read_text())
        require(config["robot"]["id"] == self.profile.robot_id and len(config["hardware"]) == 1, "非所选臂独占配置")
        hw = config["hardware"][0]
        require(hw["id"] == self.profile.hardware_id and hw["address"] == self.profile.device
                and hw["adapter"] == "feetech_sts3215" and hw["motor_ids"] == list(range(1, 7))
                and hw["auto_enable"] is False and len(hw["calibration_paths"]) == 1, "D2硬件字段改变")
        calibration = Path(hw["calibration_paths"][0])
        require(calibration.is_absolute() and calibration.name == self.profile.calibration_name
                and str(calibration) == self.inputs["calibration"]["path"]
                and file_hash(calibration) == self.inputs["calibration"]["sha256"], "Python标定身份改变")
        strict_json(calibration.read_text())
        return {"config_sha256": file_hash(self.profile.config), "calibration_sha256": file_hash(calibration)}

    def device_check(self):
        lines = self.command(["udevadm", "info", "--query=property", "--name", self.profile.device]).splitlines()
        for key, value in {"ID_VENDOR_ID": "1a86", "ID_MODEL_ID": "55d3",
                           "ID_SERIAL_SHORT": self.profile.usb_serial, "ID_USB_DRIVER": "cdc_acm"}.items():
            require([line for line in lines if line.startswith(key + "=")] == [key + "=" + value], "USB身份不唯一/不符")
        info = Path(self.profile.device).stat()
        require(stat.S_ISCHR(info.st_mode) and os.access(self.profile.device, os.R_OK | os.W_OK), "串口类型/权限不符")
        output = self.command(["fuser", self.profile.device], allowed=(1,))
        last = self.journal.state["steps"][-1]
        require(not output.strip() and not last["stderr"].strip(), "fuser错误不能当成空闲")
        return {"path": self.profile.device, "resolved": str(Path(self.profile.device).resolve()), "rdev": info.st_rdev,
                "uid": info.st_uid, "gid": info.st_gid, "mode": stat.S_IMODE(info.st_mode)}

    def preflight(self):
        self.inputs = pinned_json({"path": str(self.options.inputs), "sha256": self.options.inputs_sha256})
        self.alignment = pinned_json(self.inputs["alignment"])
        physical = pinned_json(self.inputs["physical"])
        validate_facts(self.inputs, self.alignment, physical, self.options, time.time_ns())
        runtime = inspect_runtime_preconditions(self.profile.trust)
        self.uid, self.gid = runtime["account_uid"], runtime["account_gid"]
        self.journal.state.update(physical=physical, alignment=self.alignment, runtime_preconditions=runtime,
                                  limits={key: runtime[key] for key in ("rtprio", "memlock")},
                                  actual_uid=runtime["uid"], actual_gid=runtime["gid"],
                                  environment_keys=sorted(self.environment))
        self.journal.save()
        validate_runtime_preconditions(runtime, self.profile.trust)

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
                             "--robot-id", self.profile.robot_id, "--rx-trust-dir", str(self.profile.trust)])

    def markers(self, expected):
        return marker_snapshot(self.profile.trust, expected, self.uid, self.gid)

    def launch(self):
        require(self.process is None, "禁止第二次启动")
        self.log = (self.options.output / "tool.log").open("xb")
        argv = tool_command(self.bin_dir, self.options.output, self.profile.side)
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
        if snapshot["privileged_probe"] or snapshot["unreadable"]:
            journal.state["last_process_snapshot"] = snapshot
            journal.save()
        require(not snapshot["unreadable"], f"本轮进程身份不可读：{snapshot}")
        for row in snapshot["related"]:
            key = f"{row['pid']}:{row.get('start_ticks')}"
            if key not in self.identities:
                self.identities[key] = row
                journal.state["observed_processes"] = list(self.identities.values())
                journal.save()

    def report(self, pid):
        expected = {"side": self.profile.side, "bin_dir": str(self.bin_dir), "config": str(self.profile.config),
                    "config_sha256": self.inputs["config_sha256"],
                    "calibration_sha256": self.inputs["calibration"]["sha256"], "tool_pid": pid,
                    "artifacts": {name: {"sha256": row["sha256"], "build": pinned_json(row["sidecar"])}
                                  for name, row in self.measurement["artifacts"].items()}}
        return verify_report(self.options.output, expected)

    def cleanup(self):
        require(hasattr(self, "measurement"), "未建立本轮产物身份，无法完成清场核验")
        value = process_snapshot(self.process.pid if self.process else -1, self.options.output,
                                 [self.bin_dir / name for name in self.measurement["artifacts"]])
        self.journal.state["last_process_snapshot"] = value
        self.journal.save()
        require(value["clean"], f"清场存在残留或不可读身份：{value}")
        return value

    def close(self):
        if self.log is not None:
            self.log.close()


class Journal:
    def __init__(self, output, options):
        self.output = output
        self.state = {"schema": "P7_H2_RUN_V1", "status": "RUNNING", "started_ns": time.time_ns(),
                      "side": arm_profile(getattr(options, "side", "left")).side,
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
    parser.add_argument("--side", choices=("left", "right"), default="left")
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
        print(json.dumps({"mode": "PLAN_ONLY", "side": options.side, "deployable": False,
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
