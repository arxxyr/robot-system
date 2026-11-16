#!/usr/bin/env python3
"""准备、备份与回滚状态接入或左侧常驻控制 unit；不启动服务、不访问硬件。"""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import fcntl
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import pwd
import re
import secrets
import subprocess
import sys
import tempfile
import tomllib

sys.path.insert(0, str(Path(__file__).resolve().parent))
from artifact_checks import elf_identity, require, sha256, strict_json
from install_s0_units import absolute, file_identity, same_content, sync_directory, write_new

UNITS = ("robot-gateway-bus1.service", "robot-gateway-bus2.service", "robot-status.service")
BINARIES = ("robot-gateway", "robot-status")
FILES = tuple("bin/" + name for name in BINARIES) + (
    "configs/xlerobot-status.toml", "configs/rpc-roles-status.toml",
) + tuple("systemd/" + name for name in UNITS)
LEFT_UNITS = ("robot-s1-bus1.service", "robot-gateway-bus1.service", "robot-status.service")
LEFT_BINARIES = (*BINARIES, "robot-s1")
LEFT_FILES = tuple("bin/" + name for name in LEFT_BINARIES) + (
    "configs/xlerobot-status.toml", "configs/rpc-roles-web-control.toml", "configs/web-control.env",
    "configs/xlerobot-bus1-motion.toml", "configs/xlerobot-bus1-s1.toml",
) + tuple("systemd/" + name for name in LEFT_UNITS)
CONTROL_CAPABILITIES = ("read_state", "set_enabled", "clear_estop", "estop", "invoke_task")
PROPERTIES = ("Id", "LoadState", "ActiveState", "SubState", "MainPID", "Job",
              "UnitFileState", "FragmentPath", "DropInPaths")
SCHEMA = "ROBOT_STATUS_DEPLOY_V1"
BACKUP_SCHEMA = "ROBOT_STATUS_BACKUP_V1"


def deployment_files(record):
    left_control = record.get("left_control", False)
    require(type(left_control) is bool, "左侧控制标记必须为布尔值")
    return (LEFT_UNITS, LEFT_BINARIES, LEFT_FILES) if left_control else (UNITS, BINARIES, FILES)


def safe_directory(path):
    path = absolute(path)
    require(path.parent.resolve(strict=True) == path.parent, "目录父路径不能经符号链接")
    require(not any(path.is_relative_to(f"/var/lib/robot/xlerobot_{side}") for side in ("left", "right")),
            "状态部署不得进入机器人保护目录")
    return path


def unit_path(path):
    path = absolute(path)
    require(re.fullmatch(r"/[A-Za-z0-9_./+-]+", str(path)), "unit 路径含空白或特殊展开字符")
    return str(path)


def replace_line(text, key, value):
    result, count = re.subn(r"(?m)^" + re.escape(key) + r"\s*=.*$", lambda _: key + "=" + value, text)
    require(count == 1, "配置键须唯一：" + key)
    return result


def validate_listen(value):
    host, separator, port = value.rpartition(":")
    require(separator and port.isdigit() and 1 <= int(port) <= 65535, "监听地址须含固定端口")
    address = ipaddress.IPv4Address(host)
    require(address.is_loopback or any(address in ipaddress.ip_network(network)
            for network in ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16")),
            "状态页只允许 loopback 或 RFC1918 私网地址")
    return value


def prepare(candidate, bin_directory, bus_configs, runtime_commit, listen, source_root, *, left_control=False):
    candidate = safe_directory(candidate)
    require(re.fullmatch(r"[0-9a-f]{40}", runtime_commit), "运行产物提交须为完整 Git SHA")
    listen = validate_listen(listen)
    require(len(bus_configs) == 2 and len(set(map(str, bus_configs))) == 2, "须明确两份不同的总线配置")
    units, binaries, files = deployment_files({"left_control": left_control})
    external, robot_ids, hardware_ids = {}, [], []
    for bus, path in enumerate(bus_configs, 1):
        path = absolute(path)
        require(path.resolve(strict=True) == path, "总线配置不能经符号链接")
        external[f"bus{bus}"] = file_identity(path)
        config = tomllib.loads(path.read_text())
        robot_id = config["robot"]["id"]
        require(isinstance(robot_id, str) and re.fullmatch(r"[A-Za-z0-9_-]+", robot_id), "机器人身份无效")
        require(len(config["hardware"]) == 1, "每个网关须对应唯一 hardware")
        require(re.fullmatch(r"[A-Za-z0-9_-]+", config["hardware"][0]["id"]), "硬件身份无效")
        robot_ids.append(robot_id)
        hardware_ids.append(config["hardware"][0]["id"])
    require(len(set(robot_ids)) == 2, "两总线机器人身份重复")
    source_root, bin_directory = absolute(source_root), absolute(bin_directory)
    payloads = {}
    for name in binaries:
        path = bin_directory / name
        # Cargo 的 bin/deps 可以共享硬链接；只读源核内容，冻结副本仍须单链接。
        before = elf_identity(path, 183)
        data = path.read_bytes()
        require(elf_identity(path, 183) == before and len(data) == before["bytes"]
                and hashlib.sha256(data).hexdigest() == before["sha256"], "复制期间运行产物变化")
        payloads["bin/" + name] = data
    status = (source_root / "configs/xlerobot-status.toml").read_text()
    status = replace_line(status, "listen", json.dumps(listen))
    for bus, path in enumerate(bus_configs, 1):
        status, count = re.subn(r'(?m)^config\s*=\s*"xlerobot-bus' + str(bus) + r'.toml"\s*$',
                               lambda _: "config = " + json.dumps(str(path)), status)
        require(count == 1, "状态配置未唯一引用原总线")
    sources = tomllib.loads(status)["sources"]
    require(len(sources) == 2 and all(row["id"] == f"bus{index + 1}"
            and row["endpoint"] == f"http://127.0.0.1:{50061 + index}"
            and row["config"] == str(bus_configs[index]) for index, row in enumerate(sources)),
            "状态源与两份网关地址或配置不一致")
    if left_control:
        motion_path = source_root / "configs/xlerobot-bus1-motion.toml"
        motion = motion_path.read_text()
        motion_config = tomllib.loads(motion)
        require(set(motion_config) == {"s1"} and set(motion_config["s1"]) == {"single_joint"},
                "运动片段只能包含原单关节约束")
        original = tomllib.loads(Path(bus_configs[0]).read_text())
        require("single_joint" not in original.get("s1", {}), "原总线已有运动约束，不能重复合成")
        names = [row["name"] for row in motion_config["s1"]["single_joint"]["joints"]]
        require(names == original["hardware"][0]["joints"], "左侧运动片段与原总线关节顺序不一致")
        composed = Path(bus_configs[0]).read_text().rstrip() + "\n\n" + motion
        expected = {**original, "s1": {**original.get("s1", {}), **motion_config["s1"]}}
        require(tomllib.loads(composed) == expected, "合成配置改变了原总线内容")
        payloads["configs/xlerobot-bus1-s1.toml"] = composed.encode()
        payloads["configs/xlerobot-bus1-motion.toml"] = motion.encode()
        status, count = re.subn(r'(?m)^(\[control\]\s*\n)enabled\s*=\s*false\s*$',
                               r'\1enabled = true', status)
        require(count == 1, "控制开关须唯一且默认关闭")
        status, count = re.subn(r'(?m)^# (control = \{ motion_config = "xlerobot-bus1-motion.toml"[^\n]+)$',
                               r'\1', status)
        require(count == 1, "缺少唯一的左侧控制配置示例")
        parsed = tomllib.loads(status)
        require(parsed["control"] == {"enabled": True} and "control" in parsed["sources"][0]
                and "control" not in parsed["sources"][1], "只能开放左侧控制")
        token = secrets.token_hex(32)
        roles_name = "rpc-roles-web-control.toml"
        payloads["configs/" + roles_name] = ("[tokens]\n" + json.dumps(token) + ' = "maintainer"\n'
            '[roles.maintainer]\ncapabilities = ' + json.dumps(CONTROL_CAPABILITIES) + "\n").encode()
        payloads["configs/web-control.env"] = ("ROBOT_TOKEN=" + token + "\n").encode()
    else:
        roles_name = "rpc-roles-status.toml"
        roles = (source_root / "configs" / roles_name).read_bytes()
        require(tomllib.loads(roles.decode()) == {"tokens": {}, "roles": {"viewer": {"capabilities": ["read_state"]}}},
                "只读候选不得含控制凭据或权限")
        payloads["configs/" + roles_name] = roles
    payloads["configs/xlerobot-status.toml"] = status.encode()
    for name in units:
        text = (source_root / "scripts/systemd" / name).read_text()
        text = re.sub(r"\\\n\s*", " ", text)
        if left_control:
            users = re.findall(r"(?m)^User=([A-Za-z0-9_-]+)$", text)
            require(len(users) == 1 and pwd.getpwnam(users[0]).pw_uid == os.geteuid(),
                    "左侧候选须以unit的服务用户执行prepare，确保0600凭据可读；安装仍使用root")
        # S1沿现有同侧S0依赖；Web和网关仍不会拉起硬件服务。
        dependencies = re.findall(r"(?m)^\s*(Requires|Wants|BindsTo|Upholds|OnFailure|OnSuccess)\s*=([^\n]*)", text)
        require((dependencies == [("Requires", "robot-s0-bus1.service")] if name == "robot-s1-bus1.service" else not dependencies)
                and all(key == "ExecStart" for key in re.findall(r"(?m)^\s*(Exec[A-Za-z]+)\s*=", text)),
                "状态模板不得自动操作其他服务或执行额外命令")
        text = replace_line(text, "WorkingDirectory", unit_path(candidate))
        if name.startswith("robot-gateway-"):
            index = 0 if name == "robot-gateway-bus1.service" else 1
            argv = [unit_path(candidate / "bin/robot-gateway"), "--robot-id", robot_ids[index],
                    "--rpc-roles-toml", unit_path(candidate / "configs" / roles_name),
                    "--config", unit_path(bus_configs[index]), "--listen-addr", f"127.0.0.1:{50061 + index}"]
        elif name == "robot-s1-bus1.service":
            argv = [unit_path(candidate / "bin/robot-s1"), "--robot-id", robot_ids[0],
                    "--hardware-id", hardware_ids[0], "--config", unit_path(candidate / "configs/xlerobot-bus1-s1.toml")]
        else:
            argv = [unit_path(candidate / "bin/robot-status"), "--config", unit_path(candidate / "configs/xlerobot-status.toml")]
            if left_control:
                require(not re.search(r"(?m)^EnvironmentFile=", text), "Web模板不得预置其他凭据文件")
                text = text.replace("[Service]\n", "[Service]\nEnvironmentFile=" + unit_path(candidate / "configs/web-control.env") + "\n", 1)
                text = replace_line(text, "Description", "Robot status and left joint control")
        payloads["systemd/" + name] = replace_line(text, "ExecStart", " ".join(argv)).encode()
    candidate.mkdir(mode=0o755)
    for directory in ("bin", "configs", "systemd"):
        (candidate / directory).mkdir(mode=0o755)
    for name, data in payloads.items():
        secret = name in ("configs/rpc-roles-web-control.toml", "configs/web-control.env")
        write_new(candidate / name, data, 0o600 if secret else 0o555 if name.startswith("bin/") else 0o444)
    require(all(file_identity(ref["path"]) == ref for ref in external.values()), "准备期间总线配置变化")
    manifest = {"schema": SCHEMA, "candidate": str(candidate), "runtime_commit": runtime_commit,
                "files": {name: file_identity(candidate / name) for name in files}, "external_configs": external}
    if left_control:
        manifest["left_control"] = True
    write_new(candidate / "manifest.json", (json.dumps(manifest, ensure_ascii=False, indent=2) + "\n").encode(), 0o444)
    sync_directory(candidate.parent)
    return {"result": "PREPARE_PASS", "candidate": str(candidate), "manifest_sha256": sha256(candidate / "manifest.json")}


def run_command(argv):
    require(argv == ["systemctl", "daemon-reload"] or any(
        argv == ["systemctl", "show", name, "--all", "--property=" + ",".join(PROPERTIES)] for name in (*UNITS, *LEFT_UNITS)),
        "安装入口仅允许已知接入服务属性查询及 daemon-reload")
    return subprocess.run(argv, capture_output=True, text=True, timeout=30, check=False,
                          env={"PATH": "/usr/sbin:/usr/bin:/sbin:/bin", "LC_ALL": "C"})


def atomic_replace(path, data, expected):
    descriptor, temporary = tempfile.mkstemp(prefix="." + path.name + ".status-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(data)
            os.fchown(stream.fileno(), expected["uid"], expected["gid"])
            os.fchmod(stream.fileno(), expected["mode"])
            stream.flush()
            os.fsync(stream.fileno())
        require(same_content(file_identity(temporary), expected), "临时文件内容或权限错误")
        os.replace(temporary, path)
        sync_directory(path.parent)
    finally:
        Path(temporary).unlink(missing_ok=True)


def validate_candidate(candidate, manifest_sha256, *, verify_files=True):
    """仅核候选及引用配置，允许运行验收复用；不查询 systemd。"""
    candidate = safe_directory(candidate)
    require(candidate.resolve(strict=True) == candidate, "候选目录不能经符号链接")
    require(re.fullmatch(r"[0-9a-f]{64}", manifest_sha256), "清单 SHA 格式错误")
    path = candidate / "manifest.json"
    require(file_identity(path)["sha256"] == manifest_sha256, "候选清单变化")
    record = strict_json(path.read_text())
    _, binaries, files = deployment_files(record)
    require(record["schema"] == SCHEMA and record["candidate"] == str(candidate)
            and re.fullmatch(r"[0-9a-f]{40}", record["runtime_commit"])
            and set(record["files"]) == set(files) and set(record["external_configs"]) == {"bus1", "bus2"},
            "候选字段或文件集合错误")
    for name, ref in record["files"].items():
        require(ref["path"] == str(candidate / name), "候选文件路径越界")
    if verify_files:
        for ref in (*record["files"].values(), *record["external_configs"].values()):
            require(file_identity(ref["path"]) == ref, "候选或总线配置身份变化：" + ref["path"])
        for name in binaries:
            elf_identity(candidate / "bin" / name, 183)
    return record


class StatusUnitInstaller:
    def __init__(self, candidate, manifest_sha256, *, unit_directory=Path("/etc/systemd/system"), runner=run_command):
        self.candidate, self.unit_directory = safe_directory(candidate), safe_directory(unit_directory)
        require(re.fullmatch(r"[0-9a-f]{64}", manifest_sha256), "清单 SHA 格式错误")
        self.manifest_sha256, self.runner = manifest_sha256, runner
        self.units = deployment_files(self.manifest(verify_files=False))[0]

    @contextmanager
    def locked(self):
        require(self.candidate.resolve(strict=True) == self.candidate
                and self.unit_directory.resolve(strict=True) == self.unit_directory, "目录不能经符号链接")
        descriptor = os.open(self.unit_directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            yield
        finally:
            os.close(descriptor)

    def manifest(self, verify_files=True):
        return validate_candidate(self.candidate, self.manifest_sha256, verify_files=verify_files)

    def command(self, argv):
        result = self.runner(argv)
        require(result.returncode == 0 and not result.stderr.strip(), "命令失败：" + repr(argv))
        return result.stdout

    def target(self, name):
        require(name in self.units, "仅管理本候选的三个接入 unit")
        path = self.unit_directory / name
        require(not os.path.lexists(str(path) + ".d"), "拒绝目标 drop-in")
        return {"path": str(path), "exists": True, **file_identity(path)} if os.path.lexists(path) else {"path": str(path), "exists": False}

    def state(self, verify_files=True):
        manifest = self.manifest(verify_files)
        services, targets = {}, {}
        for name in self.units:
            raw = self.command(["systemctl", "show", name, "--all", "--property=" + ",".join(PROPERTIES)])
            rows = [line.split("=", 1) for line in raw.splitlines()]
            require(all(len(row) == 2 for row in rows) and len(rows) == len(PROPERTIES), "服务属性缺失或重复")
            fields = dict(rows)
            require(set(fields) == set(PROPERTIES) and fields["Id"] == name and fields["MainPID"] == "0"
                    and fields["Job"] in ("", "0", "0 /") and not fields["DropInPaths"]
                    and fields["LoadState"] in ("loaded", "not-found")
                    and (fields["ActiveState"], fields["SubState"]) in (("inactive", "dead"), ("failed", "failed"))
                    and fields["UnitFileState"] in ("", "disabled", "static")
                    and fields["FragmentPath"] in ("", str(self.unit_directory / name)), "服务仍运行、存在 Job 或来源不符：" + name)
            services[name], targets[name] = fields, self.target(name)
        return {"manifest": manifest, "services": services, "targets": targets}

    def check(self):
        with self.locked():
            state = self.state()
            return {"result": "CHECK_PASS", "runtime_commit": state["manifest"]["runtime_commit"], "services": state["services"]}

    def backup_path(self, path):
        path = safe_directory(path)
        require(not path.is_relative_to(self.candidate) and not path.is_relative_to(self.unit_directory), "备份目录与安装范围重叠")
        return path

    def backup(self, directory):
        with self.locked():
            state, path = self.state(), self.backup_path(directory)
            path.mkdir(mode=0o700)
            sync_directory(path.parent)
            (path / "units").mkdir(mode=0o700)
            saved = {}
            for name, ref in state["targets"].items():
                if ref["exists"]:
                    write_new(path / "units" / name, Path(ref["path"]).read_bytes())
                    saved[name] = file_identity(path / "units" / name)
                    require(saved[name]["sha256"] == ref["sha256"], "备份内容错误")
            require(self.state() == state, "备份期间候选或安装状态变化")
            owner = self.unit_directory.stat()
            record = {"schema": BACKUP_SCHEMA, "candidate": str(self.candidate), "manifest_sha256": self.manifest_sha256,
                      "unit_directory": str(self.unit_directory), "originals": state["targets"], "saved": saved,
                      "new_units": {name: {**state["manifest"]["files"]["systemd/" + name], "uid": owner.st_uid,
                                            "gid": owner.st_gid, "mode": 0o644} for name in self.units}}
            data = (json.dumps(record, ensure_ascii=False, indent=2) + "\n").encode()
            write_new(path / "backup.json", data)
            write_new(path / "backup.sha256", (hashlib.sha256(data).hexdigest() + "\n").encode())
            return {"result": "BACKUP_PASS", "backup_directory": str(path), "files": len(saved)}

    def read_backup(self, directory):
        path = self.backup_path(directory)
        require(path.resolve(strict=True) == path and (path / "units").resolve(strict=True) == path / "units", "备份目录不能经符号链接")
        digest = file_identity(path / "backup.json")["sha256"]
        file_identity(path / "backup.sha256")
        require((path / "backup.sha256").read_text() == digest + "\n", "备份清单损坏")
        record = strict_json((path / "backup.json").read_text())
        require(record["schema"] == BACKUP_SCHEMA and record["candidate"] == str(self.candidate)
                and record["manifest_sha256"] == self.manifest_sha256 and record["unit_directory"] == str(self.unit_directory)
                and set(record["originals"]) == set(record["new_units"]) == set(self.units)
                and set(record["saved"]) == {name for name, ref in record["originals"].items() if ref["exists"]}, "备份不属于本候选")
        for name, ref in record["originals"].items():
            require(ref["path"] == str(self.unit_directory / name), "备份目标路径错误")
            if ref["exists"]:
                saved = record["saved"][name]
                require(saved["path"] == str(path / "units" / name)
                        and file_identity(saved["path"]) == saved and saved["sha256"] == ref["sha256"], "备份文件损坏")
        return path, record

    def apply(self, directory, rollback=False):
        with self.locked():
            path, record = self.read_backup(directory)
            state = self.state(verify_files=not rollback)
            owner = self.unit_directory.stat()
            for name in self.units:
                current, old, new = state["targets"][name], record["originals"][name], record["new_units"][name]
                source = state["manifest"]["files"]["systemd/" + name]
                require(all(new[key] == source[key] for key in ("path", "sha256", "bytes"))
                        and (new["uid"], new["gid"], new["mode"]) == (owner.st_uid, owner.st_gid, 0o644), "备份候选身份错误")
                original = current == old or rollback and old["exists"] and current["exists"] and same_content(current, old)
                installed = rollback and current["exists"] and same_content(current, new)
                require(original or installed, "安装目标出现未知漂移：" + name)
            receipt = {"operation": "rollback" if rollback else "install", "status": "IN_PROGRESS", "completed": [],
                       "manifest_sha256": self.manifest_sha256}
            if rollback:
                descriptor, receipt_name = tempfile.mkstemp(prefix="rollback-", suffix=".json", dir=path)
                os.close(descriptor)
                receipt_path = Path(receipt_name)
            else:
                receipt_path = path / "install-result.json"
                write_new(receipt_path, b"", 0o600)
            def save_receipt():
                data = (json.dumps(receipt, ensure_ascii=False, indent=2) + "\n").encode()
                atomic_replace(receipt_path, data, {"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest(),
                               "uid": os.geteuid(), "gid": os.getegid(), "mode": 0o600})
            save_receipt()
            try:
                for name in self.units:
                    require(self.target(name) == state["targets"][name], "换入前目标再次漂移")
                    target = self.unit_directory / name
                    expected = record["originals"][name] if rollback else record["new_units"][name]
                    if rollback and not expected["exists"]:
                        target.unlink(missing_ok=True)
                        sync_directory(target.parent)
                    else:
                        source = path / "units" / name if rollback else self.candidate / "systemd" / name
                        atomic_replace(target, source.read_bytes(), expected)
                    receipt["completed"].append(name)
                    save_receipt()
                self.command(["systemctl", "daemon-reload"])
                after = self.state(verify_files=not rollback)
                for name, current in after["targets"].items():
                    expected = record["originals"][name] if rollback else record["new_units"][name]
                    require((not current["exists"] if rollback and not expected["exists"] else
                             current["exists"] and same_content(current, expected)), "换入后文件校验失败")
                receipt["status"] = "PASS"
            except BaseException as error:
                receipt.update(status="FAIL", error=repr(error))
                save_receipt()
                raise
            save_receipt()
            return {**receipt, "receipt_path": str(receipt_path)}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("prepare", "check", "backup", "install", "rollback"))
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--manifest-sha256")
    parser.add_argument("--backup-dir", type=Path)
    parser.add_argument("--bin-dir", type=Path)
    parser.add_argument("--bus1-config", type=Path)
    parser.add_argument("--bus2-config", type=Path)
    parser.add_argument("--runtime-commit")
    parser.add_argument("--listen", default="127.0.0.1:8080")
    parser.add_argument("--left-control", action="store_true", help="显式准备常驻左侧S1、左网关和Web；右侧仅保留状态源")
    args = parser.parse_args(argv)
    try:
        if args.operation == "prepare":
            require(all((args.bin_dir, args.bus1_config, args.bus2_config, args.runtime_commit)), "prepare 缺少产物或总线配置")
            result = prepare(args.candidate, args.bin_dir, (args.bus1_config, args.bus2_config),
                             args.runtime_commit, args.listen, Path(__file__).resolve().parents[1], left_control=args.left_control)
        else:
            require(args.manifest_sha256 is not None, "须指定候选清单 SHA")
            installer = StatusUnitInstaller(args.candidate, args.manifest_sha256)
            if args.operation == "check":
                result = installer.check()
            else:
                require(os.geteuid() == 0 and args.backup_dir is not None, "写操作须 root 和明确备份目录")
                result = installer.backup(args.backup_dir) if args.operation == "backup" else installer.apply(args.backup_dir, args.operation == "rollback")
        print(json.dumps(result, ensure_ascii=False))
        return 0
    except (ValueError, OSError, KeyError, TypeError, subprocess.SubprocessError) as error:
        print(json.dumps({"result": "FAIL", "error": str(error)}, ensure_ascii=False), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
