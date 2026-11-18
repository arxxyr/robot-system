#!/usr/bin/env python3
"""冻结候选的三个 S0 unit 改名安装与六路径回滚；从不启动服务或操作硬件。"""

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
import tomllib

# 固定解释器以 -I -S 启动时，仅显式加入随脚本冻结的同目录工具。
sys.path.insert(0, str(Path(__file__).resolve().parent))
from artifact_checks import elf_identity, installed_file_identity, require, sha256, strict_json

UNIT_NAMES = ("robot-s0-preflight.service", "robot-s0-bus1.service", "robot-s0-bus2.service")
OLD_UNIT_NAMES = ("xlerobot-handoff.service", "xlerobot-s0-bus1.service", "xlerobot-s0-bus2.service")
MANAGED_UNIT_NAMES = (*UNIT_NAMES, *OLD_UNIT_NAMES)
PREFLIGHT_UNITS = (UNIT_NAMES[0], OLD_UNIT_NAMES[0])
BACKUP_SCHEMA = "S0_UNIT_RENAME_BACKUP_V1"
BUSINESS_UNITS = tuple(f"{prefix}-{role}-{side}{'-arm' if role == 's0' else ''}.service"
                       for prefix in ("xlerobot", "robot")
                       for side in ("left", "right") for role in ("s0", "s1", "gateway")) + (
    "xlerobot-coordinator.service", "xlerobot-web.service", "robot-coordinator.service", "robot-web.service",
    *UNIT_NAMES[1:], *OLD_UNIT_NAMES[1:],
)
UNIT_STATE_PROPERTIES = ("ActiveState", "SubState")
INSTALL_PROPERTIES = ("Id", "LoadState", *UNIT_STATE_PROPERTIES, "UnitFileState", "FragmentPath", "DropInPaths")
EXIT_PROPERTIES = ("Result", "ExecMainCode", "ExecMainStatus")
SYSTEMD_PROPERTIES = (*INSTALL_PROPERTIES, "Job", *EXIT_PROPERTIES)
PROTECTION_NAMES = ("rx_trust_marker", "rx_trust_marker.bak", "estop_latch", "estop_latch.bak",
                    "rx_trust.lock", "estop_latch.lock")
CANDIDATE_FILES = {"camera_capture.py", "configs/xlerobot-bus1.toml", "configs/xlerobot-bus2.toml",
                   *("systemd/" + name for name in UNIT_NAMES)}
CONTENT_KEYS = ("bytes", "sha256", "uid", "gid", "mode")


def same_service_state(before, after, *, autostart_changed=()):
    # 服务退出后的诊断字段可能由 systemd 回收；安装只比较当前状态与来源。
    return before.keys() == after.keys() and all(
        row[key] == after[name][key] for name, row in before.items() for key in INSTALL_PROPERTIES
        if key != "UnitFileState" or name not in autostart_changed)


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


def protection_lock_identity(path):
    # 锁可原位改写持有者；同步域由单链接普通文件的实体与权限确定。
    path = absolute(path)
    info = path.lstat()
    require(stat.S_ISREG(info.st_mode) and info.st_nlink == 1, "保护锁必须为单链接普通文件：" + str(path))
    return dict(path=str(path), device=info.st_dev, inode=info.st_ino, uid=info.st_uid, gid=info.st_gid,
                mode=stat.S_IMODE(info.st_mode), nlink=info.st_nlink)


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
    # 自启只允许固定三份 unit 的整组操作，不提供启动、停止或任意命令入口。
    autostart = any(argv == ["systemctl", action, *UNIT_NAMES] for action in ("enable", "disable"))
    require(argv == ["systemctl", "daemon-reload"] or argv[:2] == ["systemctl", "show"]
            or argv[:1] == ["fuser"] or argv[:3] == ["sudo", "-n", "fuser"] or autostart, "禁止的命令")
    return subprocess.run(argv, capture_output=True, text=True, timeout=30, check=False,
                          env={"PATH": "/usr/sbin:/usr/bin:/sbin:/bin", "LC_ALL": "C"})


class S0UnitInstaller:
    def __init__(self, candidate, manifest_sha256, *, unit_directory=Path("/etc/systemd/system"),
                 runner=run_command, protection_root=Path("/var/lib/robot"),
                 previous_candidate=None, previous_manifest_sha256=None):
        self.candidate = absolute(candidate)
        self.unit_directory = absolute(unit_directory)
        self.protection_root = absolute(protection_root)
        require(re.fullmatch(r"[0-9a-f]{64}", manifest_sha256), "清单 SHA 格式错误")
        self.manifest_sha256, self.runner = manifest_sha256, runner
        require((previous_candidate is None) == (previous_manifest_sha256 is None), "上一版候选路径与清单 SHA 必须成对指定")
        self.previous_candidate = absolute(previous_candidate) if previous_candidate is not None else None
        self.previous_manifest_sha256 = previous_manifest_sha256
        if self.previous_candidate is not None:
            require(self.previous_candidate != self.candidate, "上一版候选不能与新候选相同")
            require(re.fullmatch(r"[0-9a-f]{64}", previous_manifest_sha256), "上一版清单 SHA 格式错误")

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
        autostart = any(argv == ["systemctl", action, *UNIT_NAMES] for action in ("enable", "disable"))
        messages = row.stderr.strip().splitlines()
        symlink_messages = autostart and all(re.fullmatch(r"(?:Created symlink|Removed) .+", line) for line in messages)
        require(row.returncode in allowed and (not messages or symlink_messages),
                "命令失败或出现未知诊断：" + repr(dict(argv=argv, exit_code=row.returncode, stderr=row.stderr)))
        return row.stdout

    def _services(self, *, allow_failed_units=False, upgrade=False):
        result = {}
        for name in (*BUSINESS_UNITS, *PREFLIGHT_UNITS):
            raw = self._command(["systemctl", "show", name, "--all", "--property=" + ",".join(SYSTEMD_PROPERTIES)])
            fields = {}
            for line in raw.splitlines():
                require("=" in line, "systemctl 属性行不完整")
                key, value = line.split("=", 1)
                require(key not in fields, "systemctl 属性重复")
                fields[key] = value
            require(set(fields) == set(SYSTEMD_PROPERTIES) and fields["Id"] == name, "systemctl 属性缺失或身份错误")
            require(fields["Job"] in ("", "0", "0 /"), "服务存在待执行 Job：" + name)
            # 仅撤销自启可接受本批三 unit 已退出的失败终态；不清除 Result。
            failed_current = (allow_failed_units and name in UNIT_NAMES and fields["LoadState"] == "loaded"
                              and (fields["ActiveState"], fields["SubState"]) == ("failed", "failed"))
            # 同名候选升级仅容纳已停稳的 Dirty 退出，不清除 systemd 保存的失败记录。
            failed_upgrade = (upgrade and name in UNIT_NAMES[1:] and fields["LoadState"] == "loaded"
                              and (fields["ActiveState"], fields["SubState"]) == ("failed", "failed")
                              and tuple(fields[key] for key in EXIT_PROPERTIES) == ("exit-code", "1", "1"))
            if failed_current or failed_upgrade:
                pass
            elif name in PREFLIGHT_UNITS:
                require(fields["LoadState"] in ("loaded", "not-found") and (fields["ActiveState"], fields["SubState"])
                        in (("active", "exited"), ("inactive", "dead")), "preflight 既有完成态变化：" + name)
            else:
                stopped = (fields["LoadState"] in ("loaded", "not-found")
                           and (fields["ActiveState"], fields["SubState"]) == ("inactive", "dead"))
                require(stopped, "业务服务未停稳：" + name)
            result[name] = fields
        return result

    def _target(self, name):
        path = self.unit_directory / name
        require(not os.path.lexists(str(path) + ".d"), "拒绝目标 unit 的 drop-in 目录：" + name)
        if not os.path.lexists(path):
            return dict(path=str(path), exists=False)
        file_identity(path)
        return installed_file_identity(path)

    def _targets(self, services, *, rollback_record=None, autostart=False):
        result = {name: self._target(name) for name in MANAGED_UNIT_NAMES}
        for name, row in result.items():
            service = services[name]
            allowed = ("enabled", "disabled") if autostart and name in UNIT_NAMES else ("", "disabled", "static")
            require(not service["DropInPaths"] and service["UnitFileState"] in allowed,
                    "拒绝 drop-in、mask、alias 或启用状态：" + name)
            # 文件事务失败后，缓存可以仍指向本次六路径的旧状态；内容允许集在 _apply 再核对。
            stale = (rollback_record is not None
                     and ((service["LoadState"] == "not-found" and service["FragmentPath"] == "")
                          or (service["LoadState"] == "loaded" and service["FragmentPath"] == row["path"])))
            exact = (service["LoadState"] == ("loaded" if row["exists"] else "not-found")
                     and service["FragmentPath"] == (row["path"] if row["exists"] else ""))
            require(stale or exact,
                    "unit 实际 FragmentPath 不符：" + name)
            if name == UNIT_NAMES[0] and not row["exists"]:
                require(service["ActiveState"] == "inactive", "缺失的新 preflight 不得带有完成态")
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
                and set(document["binaries"]) == {"robot-deploy", "robot-s0"}
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
                identity = protection_lock_identity if name.endswith(".lock") else file_identity
                result[ref["path"]] = identity(ref["path"])
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

    def _previous(self, document):
        if self.previous_candidate is None:
            return None
        require(self.previous_candidate.resolve(strict=True) == self.previous_candidate, "上一版候选不能经符号链接")
        path = self.previous_candidate / "manifest.json"
        manifest = file_identity(path)
        previous = S0UnitInstaller(self.previous_candidate, self.previous_manifest_sha256,
                                   unit_directory=self.unit_directory, protection_root=self.protection_root)
        old = previous._manifest(verify_runtime=False)
        require(old["protection_domains"] == document["protection_domains"]
                and set(old["protection"]) == {"left", "right"}, "上一版保护域与新候选不一致")
        for side in ("left", "right"):
            require(set(old["protection"][side]) == set(PROTECTION_NAMES)
                    and all(old["protection"][side][name]["path"] == document["protection"][side][name]["path"]
                            for name in PROTECTION_NAMES), "上一版保护文件路径与新候选不一致")
        units = {}
        for name in UNIT_NAMES:
            ref = old["candidate_files"]["systemd/" + name]
            source = self.previous_candidate / "systemd" / name
            require(ref["path"] == str(source) and source.resolve(strict=True) == source,
                    "上一版 unit 源路径不符：" + name)
            units[name] = file_identity(source)
            require(same_content(units[name], ref), "上一版 unit 源内容或权限变化：" + name)
        require(file_identity(path) == manifest, "核对期间上一版清单身份变化")
        return dict(candidate=str(self.previous_candidate), manifest_sha256=self.previous_manifest_sha256,
                    manifest=manifest, units=units)

    def _check(self, *, rollback_record=None, autostart=False, verify_runtime=True, allow_failed_units=False,
               allow_upgrade_state=True):
        # 回滚及撤销自启仍固定清单；撤销自启不要求运行产物与设备就绪。
        document = self._manifest(verify_runtime=rollback_record is None and verify_runtime)
        protection = self._protection(document)
        if rollback_record is None:
            previous = self._previous(document)
            upgrade = previous is not None
        else:
            previous, upgrade = self._backup_previous(rollback_record)
        upgrade = upgrade and allow_upgrade_state
        services = self._services(allow_failed_units=allow_failed_units, upgrade=upgrade)
        targets = self._targets(services, rollback_record=rollback_record, autostart=autostart or upgrade)
        if upgrade:
            require(all(not targets[name]["exists"] for name in OLD_UNIT_NAMES), "同名升级要求旧三 unit 路径全部不存在")
        if rollback_record is None:
            unit_owner = self.unit_directory.stat()
            for name in UNIT_NAMES:
                source = previous["units"][name] if previous is not None else document["candidate_files"]["systemd/" + name]
                expected = {**source,
                            "mode": 0o644, "uid": unit_owner.st_uid, "gid": unit_owner.st_gid}
                if previous is not None:
                    require(targets[name]["exists"] and same_content(targets[name], expected),
                            "当前三 unit 必须完整匹配上一版候选：" + name)
                else:
                    require(not targets[name]["exists"] or same_content(targets[name], expected),
                            "新 unit 路径已被非候选文件占用：" + name)
        if verify_runtime:
            self._devices(document)
        require(self._protection(document) == protection, "检查期间保护状态变化")
        return dict(document=document, protection=protection, services=services, targets=targets, previous_candidate=previous)

    def _autostart_check(self, *, require_estop=True, verify_runtime=True, allow_failed_units=False):
        require(self.previous_candidate is None, "自启操作不接受上一版候选参数")
        state = self._check(autostart=True, verify_runtime=verify_runtime, allow_failed_units=allow_failed_units)
        require(all(state["targets"][name]["exists"] for name in UNIT_NAMES)
                and all(not state["targets"][name]["exists"] for name in OLD_UNIT_NAMES), "自启要求本批新三 unit 已完整安装且旧三路径已撤除")
        if verify_runtime:
            for bus in (1, 2):
                hardware = tomllib.loads((self.candidate / "configs" / f"xlerobot-bus{bus}.toml").read_text()).get("hardware")
                require(isinstance(hardware, list) and len(hardware) == 1 and isinstance(hardware[0], dict)
                        and hardware[0].get("auto_enable") is False, "自启配置必须明确 auto_enable=false")
        for name in UNIT_NAMES:
            section, directives = "", []
            for line in (self.unit_directory / name).read_text().splitlines():
                line = line.strip()
                if not line or line.startswith(("#", ";")):
                    continue
                if line.startswith("["):
                    section = line
                elif section == "[Install]":
                    directives.append(line)
            require(directives == ["WantedBy=multi-user.target"], "自启 unit 必须仅由 multi-user.target 启用：" + name)
        values = {}
        for side, files in state["document"]["protection"].items():
            values[side] = {name: Path(files[name]["path"]).read_bytes().decode("ascii", errors="backslashreplace") for name in
                            ("rx_trust_marker", "rx_trust_marker.bak", "estop_latch", "estop_latch.bak")}
            if require_estop:
                require(all(values[side][name] == "1\n" for name in ("estop_latch", "estop_latch.bak")), "开启自启要求双侧双 EStop 保持1")
        require(self._protection(state["document"]) == state["protection"], "自启检查期间保护状态变化")
        state["protection_values"] = values
        return state

    def autostart_check(self):
        with self._locked():
            state = self._autostart_check()
            return dict(result="AUTOSTART_CHECK_PASS", manifest_sha256=self.manifest_sha256,
                        autostart={name: state["services"][name]["UnitFileState"] for name in UNIT_NAMES},
                        services=state["services"], targets=state["targets"], protection=state["protection"],
                        protection_values=state["protection_values"], rx_authorize_executed=False,
                        changed_units=0, hardware_started=False)

    def _autostart(self, backup_directory, enable):
        require(self.previous_candidate is None, "自启操作不接受上一版候选参数")
        with self._locked():
            path, record = self._read_backup(backup_directory)
            operation = "enable-autostart" if enable else "disable-autostart"
            descriptor, receipt_name = tempfile.mkstemp(prefix=operation + "-", suffix="-result.json", dir=path)
            os.close(descriptor)
            receipt_path = Path(receipt_name)
            receipt = dict(operation=operation, status="IN_PROGRESS", manifest_sha256=self.manifest_sha256,
                           receipt_path=str(receipt_path), units=list(UNIT_NAMES), hardware_started=False, rx_authorize_executed=False)
            self._receipt(receipt_path, receipt)
            state = None
            try:
                require(all(record["originals"][name]["exists"] and record["services"][name]["UnitFileState"] == "disabled"
                            for name in UNIT_NAMES), "自启备份必须在新三 unit 已安装且全部 disabled 时生成")
                state = self._autostart_check(require_estop=enable, verify_runtime=enable, allow_failed_units=not enable)
                for name, expected in record["new_units"].items():
                    source = state["document"]["candidate_files"]["systemd/" + name]
                    require(all(expected[key] == source[key] for key in ("path", "bytes", "sha256"))
                            and same_content(state["targets"][name], expected)
                            and state["targets"][name] == record["originals"][name], "自启目标不属于本批已备份候选：" + name)
                receipt["before"] = {name: state["services"][name]["UnitFileState"] for name in UNIT_NAMES}
                receipt["services_before"] = state["services"]
                receipt["protection_before"] = state["protection"]
                if enable:
                    require(all(value == "disabled" for value in receipt["before"].values()), "开启前新三 unit 必须全部 disabled")
                self._command(["systemctl", "enable" if enable else "disable", *UNIT_NAMES])
                after = self._autostart_check(require_estop=enable, verify_runtime=enable, allow_failed_units=not enable)
                receipt["after"] = {name: after["services"][name]["UnitFileState"] for name in UNIT_NAMES}
                receipt["services_after"] = after["services"]
                receipt["protection_after"] = after["protection"]
                require(all(value == ("enabled" if enable else "disabled") for value in receipt["after"].values()), "自启状态未达到目标")
                require(after["targets"] == state["targets"] and after["protection"] == state["protection"], "自启操作期间 unit 或保护文件变化")
                require(same_service_state(state["services"], after["services"], autostart_changed=UNIT_NAMES),
                        "自启操作期间服务状态变化")
                receipt["status"] = "PASS"
            except BaseException as error:
                receipt.update(status="FAIL", error=repr(error))
                if state is not None:
                    try:
                        receipt["services_after_failure"] = self._services(allow_failed_units=not enable)
                        receipt["protection_unchanged"] = self._protection(state["document"]) == state["protection"]
                    except BaseException as observation_error:
                        receipt["observation_error"] = repr(observation_error)
                self._receipt(receipt_path, receipt)
                raise
            self._receipt(receipt_path, receipt)
            return receipt

    def enable_autostart(self, backup_directory):
        return self._autostart(backup_directory, True)

    def disable_autostart(self, backup_directory):
        return self._autostart(backup_directory, False)

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
            for name in UNIT_NAMES:
                ref = state["document"]["candidate_files"]["systemd/" + name]
                new_units[name] = {**ref, "mode": 0o644, "uid": os.geteuid(), "gid": os.getegid()}
            require({name: self._target(name) for name in MANAGED_UNIT_NAMES} == state["targets"], "备份期间旧目标变化")
            require(self._protection(state["document"]) == state["protection"], "备份期间保护状态变化")
            require(self._previous(state["document"]) == state["previous_candidate"], "备份期间上一版源文件身份变化")
            if state["previous_candidate"] is not None:
                require(same_service_state(state["services"], self._services(upgrade=True)), "备份期间服务状态变化")
            record = dict(schema=BACKUP_SCHEMA, renames=dict(zip(OLD_UNIT_NAMES, UNIT_NAMES)),
                          candidate=str(self.candidate), manifest_sha256=self.manifest_sha256,
                          unit_directory=str(self.unit_directory), originals=state["targets"], saved=saved,
                          new_units=new_units, services=state["services"])
            if state["previous_candidate"] is not None:
                record["previous_candidate"] = state["previous_candidate"]
            raw = (json.dumps(record, ensure_ascii=False, indent=2) + "\n").encode()
            write_new(path / "backup.json", raw)
            write_new(path / "backup.sha256", (hashlib.sha256(raw).hexdigest() + "\n").encode())
            return dict(result="BACKUP_PASS", files=len(saved), backup_directory=str(path), changed_units=0)

    def _backup_previous(self, record):
        if "previous_candidate" not in record:
            return None, False
        previous = record["previous_candidate"]
        require(isinstance(previous, dict) and set(previous) == {"candidate", "manifest_sha256", "manifest", "units"},
                "备份上一版候选字段错误")
        candidate = absolute(previous["candidate"])
        require(candidate != self.candidate and isinstance(previous["manifest_sha256"], str)
                and re.fullmatch(r"[0-9a-f]{64}", previous["manifest_sha256"])
                and isinstance(previous["units"], dict) and set(previous["units"]) == set(UNIT_NAMES),
                "备份上一版候选身份错误")
        identity_keys = {"path", "bytes", "sha256", "uid", "gid", "mode", "device", "inode", "mtime_ns", "ctime_ns"}
        for name, ref in [("manifest.json", previous["manifest"]),
                          *(("systemd/" + name, previous["units"][name]) for name in UNIT_NAMES)]:
            require(isinstance(ref, dict) and set(ref) == identity_keys
                    and ref["path"] == str(candidate / name)
                    and isinstance(ref["sha256"], str) and re.fullmatch(r"[0-9a-f]{64}", ref["sha256"])
                    and all(type(ref[key]) is int and ref[key] >= 0 for key in identity_keys - {"path", "sha256"})
                    and ref["mode"] <= 0o7777, "备份上一版文件身份错误：" + name)
        require(previous["manifest"]["sha256"] == previous["manifest_sha256"], "备份上一版清单 SHA 不一致")
        unit_owner = self.unit_directory.stat()
        require(all(record["originals"][name]["exists"] is False for name in OLD_UNIT_NAMES),
                "同名升级备份不得包含旧三 unit")
        for name in UNIT_NAMES:
            old = record["originals"][name]
            expected = {**previous["units"][name], "mode": 0o644, "uid": unit_owner.st_uid, "gid": unit_owner.st_gid}
            require(old["exists"] is True and same_content(old, expected), "同名升级备份原件不匹配上一版：" + name)
        require(isinstance(record.get("services"), dict)
                and set(record["services"]) == set((*BUSINESS_UNITS, *PREFLIGHT_UNITS)), "同名升级备份服务集合错误")
        # 旧备份额外保存 MainPID，不用于判断当前服务是否已退出。
        fields = [set(service) - {"MainPID"} for service in record["services"].values() if isinstance(service, dict)]
        require(len(fields) == len(record["services"]), "同名升级备份服务属性不完整")
        complete = all(keys == set(SYSTEMD_PROPERTIES) for keys in fields)
        legacy_properties = set(SYSTEMD_PROPERTIES) - {"ExecMainCode", "ExecMainStatus"}
        legacy = all(keys == legacy_properties for keys in fields)
        require(complete or legacy, "同名升级备份服务属性不完整")
        for name in UNIT_NAMES:
            service = record["services"][name]
            require(service["Id"] == name and service["UnitFileState"] in (("enabled", "disabled") if complete else ("disabled",)),
                    "同名升级备份自启状态错误：" + name)
        # 历史备份没有退出码属性，仅沿用原 strict 回滚，不补造成功终态。
        return previous, complete

    def _read_backup(self, backup_directory):
        path = self._backup_path(backup_directory)
        require(path.is_dir() and not path.is_symlink(), "备份目录不是普通目录")
        require((path / "units").is_dir() and not (path / "units").is_symlink(), "备份 units 不能经符号链接")
        digest = file_identity(path / "backup.json")["sha256"]
        file_identity(path / "backup.sha256")
        require((path / "backup.sha256").read_text() == digest + "\n", "备份清单损坏")
        record = strict_json((path / "backup.json").read_text())
        require(record.get("schema") == BACKUP_SCHEMA
                and record.get("renames") == dict(zip(OLD_UNIT_NAMES, UNIT_NAMES)), "备份不是本次六路径改名格式")
        require(record["candidate"] == str(self.candidate) and record["manifest_sha256"] == self.manifest_sha256
                and record["unit_directory"] == str(self.unit_directory)
                and set(record["originals"]) == set(MANAGED_UNIT_NAMES)
                and set(record["new_units"]) == set(UNIT_NAMES), "备份不属于本候选六路径")
        require(set(record["saved"]) == {name for name, ref in record["originals"].items() if ref["exists"]}, "备份文件集合错误")
        for name, old in record["originals"].items():
            require(old["path"] == str(self.unit_directory / name), "备份原路径越界")
            if old["exists"]:
                ref = record["saved"][name]
                require(ref["path"] == str(path / "units" / name)
                        and same_content(file_identity(ref["path"]), ref) and ref["sha256"] == old["sha256"], "备份文件损坏")
        self._backup_previous(record)
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
            previous, upgrade = self._backup_previous(record)
            if not rollback:
                require((previous is None and self.previous_candidate is None)
                        or (isinstance(previous, dict) and previous.get("candidate") == str(self.previous_candidate)
                            and previous.get("manifest_sha256") == self.previous_manifest_sha256),
                        "安装的上一版选择与备份不一致")
            # 回滚仅恢复已校验的服务文件；断电后的设备缺失不妨碍恢复。
            state = self._check(rollback_record=record if rollback else None, allow_upgrade_state=upgrade,
                                verify_runtime=not rollback)
            if upgrade:
                require(all(state["services"][name]["UnitFileState"] == record["services"][name]["UnitFileState"]
                            for name in UNIT_NAMES), "换入前自启状态与备份不一致")
            if not rollback:
                require(state["previous_candidate"] == previous, "上一版源文件身份与备份不一致")
            for name, new in record["new_units"].items():
                source = state["document"]["candidate_files"]["systemd/" + name]
                require(all(new[key] == source[key] for key in ("path", "bytes", "sha256"))
                        and new["mode"] == 0o644 and new["uid"] == os.geteuid()
                        and new["gid"] == os.getegid(), "备份候选不一致")
            for name, current in state["targets"].items():
                old = record["originals"][name]
                new = record["new_units"].get(name)
                if rollback:
                    original = current == old or old["exists"] and current["exists"] and same_content(current, old)
                    installed = current["exists"] and new is not None and same_content(current, new)
                    retired = not current["exists"] and name in OLD_UNIT_NAMES
                    require(original or installed or retired, "回滚目标超出原件/候选允许集：" + name)
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
                # 安装先写新三份再撤旧三份；回滚先恢复旧文件，随后归还新路径原状。
                order = (*OLD_UNIT_NAMES, *UNIT_NAMES) if rollback else MANAGED_UNIT_NAMES
                for name in order:
                    target = self.unit_directory / name
                    require(self._target(name) == state["targets"][name], "换入前目标再次漂移：" + name)
                    expected = record["originals"][name] if rollback else record["new_units"].get(name)
                    if expected is None or rollback and not expected["exists"]:
                        target.unlink(missing_ok=True)
                        sync_directory(target.parent)
                    else:
                        source = path / "units" / name if rollback else self.candidate / "systemd" / name
                        self._atomic_replace(target, source.read_bytes(), expected)
                    receipt["completed"].append(name)
                    self._receipt(receipt_path, receipt)
                self._command(["systemctl", "daemon-reload"])
                after = self._services(upgrade=upgrade)
                actual = self._targets(after, autostart=upgrade)
                if upgrade:
                    require(all(after[name]["UnitFileState"] == record["services"][name]["UnitFileState"]
                                for name in UNIT_NAMES), "换入后自启状态与备份不一致")
                    require(all(all(state["services"][name][key] == after[name][key] for key in UNIT_STATE_PROPERTIES)
                                for name in UNIT_NAMES), "同名升级期间服务终态变化")
                for name, row in actual.items():
                    expected = record["originals"][name] if rollback else record["new_units"].get(name)
                    absent = expected is None or rollback and not expected["exists"]
                    require((not row["exists"] if absent else row["exists"] and same_content(row, expected)),
                            "换入后文件身份不符：" + name)
                for name in PREFLIGHT_UNITS:
                    before_handoff, after_handoff = state["services"][name], after[name]
                    require(all(before_handoff[key] == after_handoff[key] for key in UNIT_STATE_PROPERTIES),
                            "preflight 完成态变化：" + name)
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
        require(self.previous_candidate is None, "回滚不接受上一版候选参数")
        return self._apply(backup_directory, True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", nargs="?", choices=("check", "backup", "install", "rollback", "autostart-check", "enable-autostart", "disable-autostart"), default="check")
    parser.add_argument("--candidate", type=Path)
    parser.add_argument("--manifest-sha256")
    parser.add_argument("--backup-dir", type=Path)
    parser.add_argument("--previous-candidate", type=Path)
    parser.add_argument("--previous-manifest-sha256")
    args = parser.parse_args(argv)
    if (args.candidate is None and args.manifest_sha256 is None and args.operation == "check" and args.backup_dir is None
            and args.previous_candidate is None and args.previous_manifest_sha256 is None):
        parser.print_help()
        return 0
    try:
        require(args.candidate is not None and args.manifest_sha256 is not None, "必须指定候选目录与清单 SHA")
        require((args.previous_candidate is None and args.previous_manifest_sha256 is None)
                or args.operation in ("check", "backup", "install"), "仅 check/backup/install 接受上一版候选参数")
        installer = S0UnitInstaller(args.candidate, args.manifest_sha256,
                                    previous_candidate=args.previous_candidate,
                                    previous_manifest_sha256=args.previous_manifest_sha256)
        if args.operation in ("check", "autostart-check"):
            result = getattr(installer, args.operation.replace("-", "_"))()
        else:
            require(args.backup_dir is not None and os.geteuid() == 0, "写操作须用 sudo 并指定 --backup-dir")
            result = getattr(installer, args.operation.replace("-", "_"))(args.backup_dir)
        print(json.dumps(result, ensure_ascii=False, indent=2))
        return 0
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        print("S0 unit 操作拒绝或失败：" + str(error), file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
