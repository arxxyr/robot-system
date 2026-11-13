#!/usr/bin/env python3
"""准备、备份与回滚三个只读状态接入 unit；不启动服务、不访问硬件。"""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import fcntl
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import re
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
PROPERTIES = ("Id", "LoadState", "ActiveState", "SubState", "MainPID", "Job",
              "UnitFileState", "FragmentPath", "DropInPaths")
SCHEMA = "ROBOT_STATUS_DEPLOY_V1"
BACKUP_SCHEMA = "ROBOT_STATUS_BACKUP_V1"


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


def prepare(candidate, bin_directory, bus_configs, runtime_commit, listen, source_root):
    candidate = safe_directory(candidate)
    require(re.fullmatch(r"[0-9a-f]{40}", runtime_commit), "运行产物提交须为完整 Git SHA")
    listen = validate_listen(listen)
    require(len(bus_configs) == 2 and len(set(map(str, bus_configs))) == 2, "须明确两份不同的总线配置")
    external, robot_ids = {}, []
    for bus, path in enumerate(bus_configs, 1):
        path = absolute(path)
        require(path.resolve(strict=True) == path, "总线配置不能经符号链接")
        external[f"bus{bus}"] = file_identity(path)
        config = tomllib.loads(path.read_text())
        robot_id = config["robot"]["id"]
        require(isinstance(robot_id, str) and re.fullmatch(r"[A-Za-z0-9_-]+", robot_id), "机器人身份无效")
        require(len(config["hardware"]) == 1, "每个网关须对应唯一 hardware")
        robot_ids.append(robot_id)
    require(len(set(robot_ids)) == 2, "两总线机器人身份重复")
    source_root, bin_directory = absolute(source_root), absolute(bin_directory)
    payloads = {}
    for name in BINARIES:
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
    payloads["configs/xlerobot-status.toml"] = status.encode()
    roles = (source_root / "configs/rpc-roles-status.toml").read_bytes()
    require(tomllib.loads(roles.decode()) == {"tokens": {}, "roles": {"viewer": {"capabilities": ["read_state"]}}},
            "只读候选不得含控制凭据或权限")
    payloads["configs/rpc-roles-status.toml"] = roles
    for index, name in enumerate(UNITS):
        text = (source_root / "scripts/systemd" / name).read_text()
        text = re.sub(r"\\\n\s*", " ", text)
        require(not re.search(r"(?m)^\s*(?:Requires|Wants|BindsTo|Upholds|OnFailure|OnSuccess)\s*=", text)
                and all(key == "ExecStart" for key in re.findall(r"(?m)^\s*(Exec[A-Za-z]+)\s*=", text)),
                "状态模板不得自动操作其他服务或执行额外命令")
        text = replace_line(text, "WorkingDirectory", unit_path(candidate))
        if index < 2:
            argv = [unit_path(candidate / "bin/robot-gateway"), "--robot-id", robot_ids[index],
                    "--rpc-roles-toml", unit_path(candidate / "configs/rpc-roles-status.toml"),
                    "--config", unit_path(bus_configs[index]), "--listen-addr", f"127.0.0.1:{50061 + index}"]
        else:
            argv = [unit_path(candidate / "bin/robot-status"), "--config", unit_path(candidate / "configs/xlerobot-status.toml")]
        payloads["systemd/" + name] = replace_line(text, "ExecStart", " ".join(argv)).encode()
    candidate.mkdir(mode=0o755)
    for directory in ("bin", "configs", "systemd"):
        (candidate / directory).mkdir(mode=0o755)
    for name, data in payloads.items():
        write_new(candidate / name, data, 0o555 if name.startswith("bin/") else 0o444)
    require(all(file_identity(ref["path"]) == ref for ref in external.values()), "准备期间总线配置变化")
    manifest = {"schema": SCHEMA, "candidate": str(candidate), "runtime_commit": runtime_commit,
                "files": {name: file_identity(candidate / name) for name in FILES}, "external_configs": external}
    write_new(candidate / "manifest.json", (json.dumps(manifest, ensure_ascii=False, indent=2) + "\n").encode(), 0o444)
    sync_directory(candidate.parent)
    return {"result": "PREPARE_PASS", "candidate": str(candidate), "manifest_sha256": sha256(candidate / "manifest.json")}


def run_command(argv):
    require(argv == ["systemctl", "daemon-reload"] or any(
        argv == ["systemctl", "show", name, "--all", "--property=" + ",".join(PROPERTIES)] for name in UNITS),
        "安装入口仅允许三服务属性查询及 daemon-reload")
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
    require(record["schema"] == SCHEMA and record["candidate"] == str(candidate)
            and re.fullmatch(r"[0-9a-f]{40}", record["runtime_commit"])
            and set(record["files"]) == set(FILES) and set(record["external_configs"]) == {"bus1", "bus2"},
            "候选字段或文件集合错误")
    for name, ref in record["files"].items():
        require(ref["path"] == str(candidate / name), "候选文件路径越界")
    if verify_files:
        for ref in (*record["files"].values(), *record["external_configs"].values()):
            require(file_identity(ref["path"]) == ref, "候选或总线配置身份变化：" + ref["path"])
        for name in BINARIES:
            elf_identity(candidate / "bin" / name, 183)
    return record


class StatusUnitInstaller:
    def __init__(self, candidate, manifest_sha256, *, unit_directory=Path("/etc/systemd/system"), runner=run_command):
        self.candidate, self.unit_directory = safe_directory(candidate), safe_directory(unit_directory)
        require(re.fullmatch(r"[0-9a-f]{64}", manifest_sha256), "清单 SHA 格式错误")
        self.manifest_sha256, self.runner = manifest_sha256, runner

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
        require(name in UNITS, "仅管理固定三个状态接入 unit")
        path = self.unit_directory / name
        require(not os.path.lexists(str(path) + ".d"), "拒绝目标 drop-in")
        return {"path": str(path), "exists": True, **file_identity(path)} if os.path.lexists(path) else {"path": str(path), "exists": False}

    def state(self, verify_files=True):
        manifest = self.manifest(verify_files)
        services, targets = {}, {}
        for name in UNITS:
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
                                            "gid": owner.st_gid, "mode": 0o644} for name in UNITS}}
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
                and set(record["originals"]) == set(record["new_units"]) == set(UNITS)
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
            for name in UNITS:
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
                for name in UNITS:
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
    args = parser.parse_args(argv)
    try:
        if args.operation == "prepare":
            require(all((args.bin_dir, args.bus1_config, args.bus2_config, args.runtime_commit)), "prepare 缺少产物或总线配置")
            result = prepare(args.candidate, args.bin_dir, (args.bus1_config, args.bus2_config),
                             args.runtime_commit, args.listen, Path(__file__).resolve().parents[1])
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
