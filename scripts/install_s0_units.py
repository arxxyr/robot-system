#!/usr/bin/env python3
"""冻结候选的三个 S0 unit 检查、备份、安装与回滚；从不启动服务或操作硬件。"""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile

# 固定解释器以 -I -S 启动时，仅显式加入随脚本冻结的同目录工具。
sys.path.insert(0, str(Path(__file__).resolve().parent))
from post_e_h2_artifacts import elf_identity, installed_file_identity, require, sha256, strict_json

UNIT_NAMES = ("xlerobot-handoff.service", "xlerobot-s0-bus1.service", "xlerobot-s0-bus2.service")
BUSINESS_UNITS = tuple(f"xlerobot-{role}-{side}{'-arm' if role == 's0' else ''}.service"
                       for side in ("left", "right") for role in ("s0", "s1", "gateway")) + (
    "xlerobot-coordinator.service", "xlerobot-web.service", *UNIT_NAMES[1:],
)
SYSTEMD_PROPERTIES = ("Id", "LoadState", "ActiveState", "SubState", "MainPID", "UnitFileState",
                      "FragmentPath", "DropInPaths", "Job")
PROTECTION_NAMES = ("rx_trust_marker", "rx_trust_marker.bak", "estop_latch", "estop_latch.bak",
                    "rx_trust.lock", "estop_latch.lock")
CANDIDATE_FILES = {"camera_capture.py", "configs/xlerobot_bus1_full.toml", "configs/xlerobot_bus2_full.toml",
                   *("systemd/" + name for name in UNIT_NAMES)}
CONTENT_KEYS = ("bytes", "sha256", "uid", "gid", "mode")


def absolute(path):
    path = Path(path)
    require(path.is_absolute() and str(path) == os.path.normpath(path), "要求规范绝对路径：" + str(path))
    return path


def file_identity(path):
    path = absolute(path)
    info = path.lstat()
    require(stat.S_ISREG(info.st_mode) and info.st_nlink == 1, "要求单链接普通文件：" + str(path))
    row = dict(path=str(path), bytes=info.st_size, uid=info.st_uid, gid=info.st_gid,
               mode=stat.S_IMODE(info.st_mode), device=info.st_dev, inode=info.st_ino,
               mtime_ns=info.st_mtime_ns, ctime_ns=info.st_ctime_ns)
    row["sha256"] = sha256(path)
    after = path.lstat()
    require((info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns)
            == (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns),
            "读取期间文件变化：" + str(path))
    return row


def path_identity(path):
    path = absolute(path)
    info = path.lstat()
    return dict(path=str(path), resolved=str(path.resolve(strict=True)), device=info.st_dev, inode=info.st_ino,
                mode=stat.S_IMODE(info.st_mode), uid=info.st_uid, gid=info.st_gid,
                mtime_ns=info.st_mtime_ns, ctime_ns=info.st_ctime_ns,
                symlink_target=os.readlink(path) if stat.S_ISLNK(info.st_mode) else None)


def same_content(actual, expected):
    return all(actual.get(key) == expected.get(key) for key in CONTENT_KEYS)


def sync_directory(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def write_new(path, data, mode=0o400):
    with Path(path).open("xb") as stream:
        stream.write(data)
        os.fchmod(stream.fileno(), mode)
        stream.flush()
        os.fsync(stream.fileno())
    sync_directory(Path(path).parent)


def run_command(argv):
    # 即便未来误接调用方，也没有 start/stop/enable 或任意命令入口。
    require(argv == ["systemctl", "daemon-reload"] or argv[:2] == ["systemctl", "show"]
            or argv[:1] == ["fuser"] or argv[:3] == ["sudo", "-n", "fuser"], "禁止的命令")
    return subprocess.run(argv, capture_output=True, text=True, timeout=30, check=False,
                          env={"PATH": "/usr/sbin:/usr/bin:/sbin:/bin", "LC_ALL": "C"})


class S0UnitInstaller:
    def __init__(self, candidate, manifest_sha256, *, unit_directory=Path("/etc/systemd/system"),
                 runner=run_command, protection_root=Path("/var/lib/robot")):
        self.candidate = absolute(candidate)
        self.unit_directory = absolute(unit_directory)
        self.protection_root = absolute(protection_root)
        require(re.fullmatch(r"[0-9a-f]{64}", manifest_sha256), "清单 SHA 格式错误")
        self.manifest_sha256, self.runner = manifest_sha256, runner

    @contextmanager
    def _locked(self):
        require(self.candidate.resolve(strict=True) == self.candidate, "候选目录不能经符号链接")
        require(self.unit_directory.resolve(strict=True) == self.unit_directory, "unit 目录不能经符号链接")
        require(not self.unit_directory.is_relative_to(self.protection_root), "unit 目录不能位于保护域")
        fd = os.open(self.unit_directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            # 锁目录 inode，不创建部署锁文件，也不接触机器人稳定锁。
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            yield
        finally:
            os.close(fd)

    def _command(self, argv, allowed=(0,)):
        row = self.runner(argv)
        require(row.returncode in allowed and not row.stderr.strip(), "只读查询或 reload 失败：" + repr(argv))
        return row.stdout

    def _services(self):
        result = {}
        for name in (*BUSINESS_UNITS, UNIT_NAMES[0]):
            raw = self._command(["systemctl", "show", name, "--all", "--property=" + ",".join(SYSTEMD_PROPERTIES)])
            fields = {}
            for line in raw.splitlines():
                require("=" in line, "systemctl 属性行不完整")
                key, value = line.split("=", 1)
                require(key not in fields, "systemctl 属性重复")
                fields[key] = value
            require(set(fields) == set(SYSTEMD_PROPERTIES) and fields["Id"] == name, "systemctl 属性缺失或身份错误")
            require(fields["Job"] == "", "服务存在待执行 Job：" + name)
            require(fields["MainPID"] == "0", "服务仍有主进程：" + name)
            if name == UNIT_NAMES[0]:
                require(fields["LoadState"] == "loaded" and (fields["ActiveState"], fields["SubState"])
                        in (("active", "exited"), ("inactive", "dead")), "handoff 既有完成态变化")
            else:
                require(fields["LoadState"] in ("loaded", "not-found") and fields["ActiveState"] == "inactive"
                        and fields["SubState"] == "dead", "业务服务未停稳：" + name)
            result[name] = fields
        return result

    def _target(self, name):
        path = self.unit_directory / name
        require(not os.path.lexists(str(path) + ".d"), "拒绝目标 unit 的 drop-in 目录：" + name)
        if not os.path.lexists(path):
            return dict(path=str(path), exists=False)
        file_identity(path)
        return installed_file_identity(path)

    def _targets(self, services, *, rollback_record=None):
        result = {name: self._target(name) for name in UNIT_NAMES}
        for name, row in result.items():
            service = services[name]
            require(not service["DropInPaths"] and "masked" not in service["UnitFileState"], "拒绝 drop-in 或 mask：" + name)
            stale_new = (rollback_record is not None and name in UNIT_NAMES[1:] and row["exists"]
                         and service["LoadState"] == "not-found" and service["FragmentPath"] == ""
                         and same_content(row, rollback_record["new_units"][name]))
            stale_removed = (rollback_record is not None and name in UNIT_NAMES[1:] and not row["exists"]
                             and not rollback_record["originals"][name]["exists"]
                             and service["LoadState"] == "loaded" and service["FragmentPath"] == row["path"])
            exact = (service["LoadState"] == ("loaded" if row["exists"] else "not-found")
                     and service["FragmentPath"] == (row["path"] if row["exists"] else ""))
            require(stale_new or stale_removed or exact,
                    "unit 实际 FragmentPath 不符：" + name)
        return result

    def _manifest(self, *, verify_runtime=True):
        path = self.candidate / "manifest.json"
        require(file_identity(path)["sha256"] == self.manifest_sha256, "候选清单 SHA 变化")
        document = strict_json(path.read_text())
        require(document["schema"] == "S0_DEPLOY_PREPARE_20261111_V1"
                and document["root"] == str(self.candidate.parent)
                and document["deployable"] is False and document["installed"] is False
                and document["hardware_started"] is False and document["D15"] == "OPEN/DEFERRED", "候选合同错误")
        require(set(document["candidate_files"]) == CANDIDATE_FILES
                and set(document["binaries"]) == {"xlerobot_deploy", "s0_daemon"}
                and len(document["dependencies"]) == 6, "运行依赖集合不完整")
        refs = list(document["candidate_files"].values()) + list(document["binaries"].values()) + list(document["dependencies"].values())
        require(len({ref["path"] for ref in refs}) == 14, "运行依赖路径重复")
        for name, ref in document["candidate_files"].items():
            require(ref["path"] == str(self.candidate / name), "候选文件路径越界")
        if not verify_runtime:
            return document
        for ref in refs:
            require(Path(ref["path"]).resolve(strict=True) == Path(ref["path"])
                    and same_content(file_identity(ref["path"]), ref), "运行文件路径/SHA/权限变化：" + ref["path"])
        for ref in document["binaries"].values():
            elf_identity(ref["path"], 183)
        for side in ("left", "right"):
            expected_cameras = {"head_color", "left_wrist_color"} if side == "left" else {"right_wrist_color"}
            require(set(document["acceptance"][side]["python_paths"]) == expected_cameras, "固定 Python 相机集合错误")
            for rows in document["acceptance"][side]["python_paths"].values():
                require(rows and all(path_identity(row["path"]) == row for row in rows), "固定 Python 链接身份变化")
        return document

    def _protection(self, document):
        require(set(document["protection"]) == {"left", "right"}, "保护域集合错误")
        result = {}
        for side in ("left", "right"):
            directory = self.protection_root / f"xlerobot_{side}"
            require(directory.resolve(strict=True) == directory and stat.S_ISDIR(directory.lstat().st_mode),
                    "保护目录不能经符号链接迁移")
            domain = document["protection_domains"][side]
            require(domain["rx_trust_dir"] == str(directory) and domain["latch_path"] == str(directory / "estop_latch"),
                    "禁止迁移保护域")
            require(set(document["protection"][side]) == set(PROTECTION_NAMES), "保护文件集合错误")
            for name, ref in document["protection"][side].items():
                require(ref["path"] == str(directory / name), "保护文件路径错误")
                result[ref["path"]] = file_identity(ref["path"])
        return result

    def _devices(self, document):
        devices = document["devices_stat_only"]
        require(set(devices) == {"left", "right", "head_color", "left_wrist_color", "right_wrist_color"}, "设备集合错误")
        for ref in devices.values():
            path = absolute(ref["path"])
            info, identity = path.stat(), path_identity(path)
            require(path.is_symlink() and stat.S_ISCHR(info.st_mode), "设备不是稳定字符设备链接")
            require(identity == {key: ref[key] for key in identity}, "设备链接身份变化")
            target = dict(target_device=info.st_dev, target_inode=info.st_ino, rdev=info.st_rdev,
                          target_mode=stat.S_IMODE(info.st_mode), target_uid=info.st_uid, target_gid=info.st_gid)
            require(all(ref.get(key) == value for key, value in target.items()), "设备字符节点身份变化")
            prefix = [] if os.geteuid() == 0 else ["sudo", "-n"]
            require(not self._command(prefix + ["fuser", str(path)], allowed=(1,)).strip(), "设备存在持有者")

    def _check(self, *, rollback_record=None):
        # 损坏的新运行依赖不能阻止恢复完整旧 unit；回滚仍固定原候选清单。
        document = self._manifest(verify_runtime=rollback_record is None)
        protection = self._protection(document)
        services = self._services()
        targets = self._targets(services, rollback_record=rollback_record)
        self._devices(document)
        require(self._protection(document) == protection, "检查期间保护状态变化")
        return dict(document=document, protection=protection, services=services, targets=targets)

    def check(self):
        with self._locked():
            state = self._check()
            return dict(result="CHECK_PASS", runtime_files=14, protected_files=12, devices=5,
                        manifest_sha256=self.manifest_sha256, targets=state["targets"], protection=state["protection"],
                        services=state["services"], changed_units=0, hardware_started=False)

    def _backup_path(self, backup_directory):
        path = absolute(backup_directory)
        require(path.parent.resolve(strict=True) == path.parent and not path.is_relative_to(self.candidate)
                and not path.is_relative_to(self.unit_directory), "备份目录不能覆盖候选或 unit 目录")
        for side in ("left", "right"):
            require(not path.is_relative_to(self.protection_root / f"xlerobot_{side}"), "备份不得进入保护域")
        return path

    def backup(self, backup_directory):
        with self._locked():
            state = self._check()
            path = self._backup_path(backup_directory)
            path.mkdir(mode=0o700)
            sync_directory(path.parent)
            (path / "units").mkdir(mode=0o700)
            sync_directory(path)
            saved, new_units = {}, {}
            for name, old in state["targets"].items():
                if old["exists"]:
                    target = path / "units" / name
                    write_new(target, Path(old["path"]).read_bytes())
                    saved[name] = file_identity(target)
                    require(saved[name]["sha256"] == old["sha256"], "备份内容不符")
                ref = state["document"]["candidate_files"]["systemd/" + name]
                new_units[name] = {**ref, "mode": 0o644, "uid": os.geteuid(), "gid": os.getegid()}
            require({name: self._target(name) for name in UNIT_NAMES} == state["targets"], "备份期间旧目标变化")
            require(self._protection(state["document"]) == state["protection"], "备份期间保护状态变化")
            record = dict(candidate=str(self.candidate), manifest_sha256=self.manifest_sha256,
                          unit_directory=str(self.unit_directory), originals=state["targets"], saved=saved,
                          new_units=new_units, services=state["services"])
            raw = (json.dumps(record, ensure_ascii=False, indent=2) + "\n").encode()
            write_new(path / "backup.json", raw)
            write_new(path / "backup.sha256", (hashlib.sha256(raw).hexdigest() + "\n").encode())
            return dict(result="BACKUP_PASS", files=len(saved), backup_directory=str(path), changed_units=0)

    def _read_backup(self, backup_directory):
        path = self._backup_path(backup_directory)
        require(path.is_dir() and not path.is_symlink(), "备份目录不是普通目录")
        require((path / "units").is_dir() and not (path / "units").is_symlink(), "备份 units 不能经符号链接")
        digest = file_identity(path / "backup.json")["sha256"]
        file_identity(path / "backup.sha256")
        require((path / "backup.sha256").read_text() == digest + "\n", "备份清单损坏")
        record = strict_json((path / "backup.json").read_text())
        require(record["candidate"] == str(self.candidate) and record["manifest_sha256"] == self.manifest_sha256
                and record["unit_directory"] == str(self.unit_directory)
                and set(record["originals"]) == set(record["new_units"]) == set(UNIT_NAMES), "备份不属于本候选三 unit")
        require(set(record["saved"]) == {name for name, ref in record["originals"].items() if ref["exists"]}, "备份文件集合错误")
        for name, old in record["originals"].items():
            require(old["path"] == str(self.unit_directory / name), "备份原路径越界")
            if old["exists"]:
                ref = record["saved"][name]
                require(ref["path"] == str(path / "units" / name)
                        and same_content(file_identity(ref["path"]), ref) and ref["sha256"] == old["sha256"], "备份文件损坏")
        return path, record

    def _atomic_replace(self, target, data, expected):
        descriptor, name = tempfile.mkstemp(prefix="." + target.name + ".s0-", dir=target.parent)
        temporary = Path(name)
        try:
            with os.fdopen(descriptor, "wb") as stream:
                stream.write(data)
                os.fchown(stream.fileno(), expected["uid"], expected["gid"])
                os.fchmod(stream.fileno(), expected["mode"])
                stream.flush()
                os.fsync(stream.fileno())
            require(same_content(file_identity(temporary), expected), "临时文件 SHA/权限不符")
            os.replace(temporary, target)
            sync_directory(target.parent)
        finally:
            temporary.unlink(missing_ok=True)

    def _receipt(self, path, record, *, first=False):
        raw = (json.dumps(record, ensure_ascii=False, indent=2) + "\n").encode()
        if first:
            write_new(path, raw, 0o600)
        else:
            expected = dict(bytes=len(raw), sha256=hashlib.sha256(raw).hexdigest(),
                            uid=os.geteuid(), gid=os.getegid(), mode=0o600)
            self._atomic_replace(path, raw, expected)

    def _apply(self, backup_directory, rollback):
        with self._locked():
            path, record = self._read_backup(backup_directory)
            state = self._check(rollback_record=record if rollback else None)
            for name, current in state["targets"].items():
                old, new = record["originals"][name], record["new_units"][name]
                source = state["document"]["candidate_files"]["systemd/" + name]
                require(all(new[key] == source[key] for key in ("path", "bytes", "sha256"))
                        and new["mode"] == 0o644, "备份候选不一致")
                if rollback:
                    original = current == old or old["exists"] and current["exists"] and same_content(current, old)
                    require(original or current["exists"] and same_content(current, new), "回滚目标超出原件/候选允许集：" + name)
                else:
                    require(current == old, "安装前旧目标漂移：" + name)
            operation = "rollback" if rollback else "install"
            receipt = dict(operation=operation, status="IN_PROGRESS", completed=[], manifest_sha256=self.manifest_sha256)
            if rollback:
                descriptor, receipt_name = tempfile.mkstemp(prefix="rollback-", suffix="-result.json", dir=path)
                os.close(descriptor)
                receipt_path = Path(receipt_name)
            else:
                receipt_path = path / "install-result.json"
            receipt["receipt_path"] = str(receipt_path)
            self._receipt(receipt_path, receipt, first=not rollback)
            try:
                for name in UNIT_NAMES:
                    target = self.unit_directory / name
                    require(self._target(name) == state["targets"][name], "换入前目标再次漂移：" + name)
                    expected = record["originals"][name] if rollback else record["new_units"][name]
                    if rollback and not expected["exists"]:
                        target.unlink(missing_ok=True)
                        sync_directory(target.parent)
                    else:
                        source = path / "units" / name if rollback else self.candidate / "systemd" / name
                        self._atomic_replace(target, source.read_bytes(), expected)
                    receipt["completed"].append(name)
                    self._receipt(receipt_path, receipt)
                self._command(["systemctl", "daemon-reload"])
                after = self._services()
                actual = self._targets(after)
                for name, row in actual.items():
                    expected = record["originals"][name] if rollback else record["new_units"][name]
                    require((not row["exists"] if rollback and not expected["exists"] else row["exists"] and same_content(row, expected)),
                            "换入后文件身份不符：" + name)
                before_handoff, after_handoff = state["services"][UNIT_NAMES[0]], after[UNIT_NAMES[0]]
                require(all(before_handoff[key] == after_handoff[key] for key in ("ActiveState", "SubState", "MainPID")),
                        "handoff 完成态变化")
                require(self._protection(state["document"]) == state["protection"], "换入期间保护状态变化")
                receipt["status"] = "PASS"
            except BaseException as error:
                receipt.update(status="FAIL", error=repr(error))
                self._receipt(receipt_path, receipt)
                raise
            self._receipt(receipt_path, receipt)
            return receipt

    def install(self, backup_directory):
        return self._apply(backup_directory, False)

    def rollback(self, backup_directory):
        return self._apply(backup_directory, True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", nargs="?", choices=("check", "backup", "install", "rollback"), default="check")
    parser.add_argument("--candidate", type=Path)
    parser.add_argument("--manifest-sha256")
    parser.add_argument("--backup-dir", type=Path)
    args = parser.parse_args(argv)
    if args.candidate is None and args.manifest_sha256 is None and args.operation == "check" and args.backup_dir is None:
        parser.print_help()
        return 0
    try:
        require(args.candidate is not None and args.manifest_sha256 is not None, "必须指定候选目录与清单 SHA")
        installer = S0UnitInstaller(args.candidate, args.manifest_sha256)
        if args.operation == "check":
            result = installer.check()
        else:
            require(args.backup_dir is not None and os.geteuid() == 0, "写操作须用 sudo 并指定 --backup-dir")
            result = getattr(installer, args.operation)(args.backup_dir)
        print(json.dumps(result, ensure_ascii=False, indent=2))
        return 0
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        print("S0 unit 操作拒绝或失败：" + str(error), file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
