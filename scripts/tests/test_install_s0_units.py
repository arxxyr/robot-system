"""S0 unit 安装器的离线文件事务测试，不访问 systemd 或真实设备。"""

import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import stat
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
SPEC = importlib.util.spec_from_file_location("install_s0_units", SCRIPTS / "install_s0_units.py")
installer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(installer)

UNIT_NAMES = ("robot-s0-preflight.service", "robot-s0-bus1.service", "robot-s0-bus2.service")
OLD_UNIT_NAMES = ("xlerobot-handoff.service", "xlerobot-s0-bus1.service", "xlerobot-s0-bus2.service")
MANAGED_UNIT_NAMES = (*UNIT_NAMES, *OLD_UNIT_NAMES)
PROTECTION_NAMES = (
    "rx_trust_marker",
    "rx_trust_marker.bak",
    "estop_latch",
    "estop_latch.bak",
    "rx_trust.lock",
    "estop_latch.lock",
)
REJECTED = (ValueError, OSError)


def reference(path):
    """与候选清单一致，冻结普通文件内容及 lstat 身份。"""
    info = path.lstat()
    return {
        "path": str(path),
        "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
        "bytes": info.st_size,
        "uid": info.st_uid,
        "gid": info.st_gid,
        "mode": stat.S_IMODE(info.st_mode),
        "nlink": info.st_nlink,
        "device": info.st_dev,
        "inode": info.st_ino,
        "mtime_ns": info.st_mtime_ns,
        "ctime_ns": info.st_ctime_ns,
        "symlink": stat.S_ISLNK(info.st_mode),
    }


def path_identity(path):
    info = path.lstat()
    return {
        "path": str(path),
        "resolved": str(path.resolve(strict=True)),
        "device": info.st_dev,
        "inode": info.st_ino,
        "mode": stat.S_IMODE(info.st_mode),
        "uid": info.st_uid,
        "gid": info.st_gid,
        "mtime_ns": info.st_mtime_ns,
        "ctime_ns": info.st_ctime_ns,
        "symlink_target": os.readlink(path) if path.is_symlink() else None,
    }


def device_identity(path):
    info = path.stat()
    return {
        **path_identity(path),
        "target_device": info.st_dev,
        "target_inode": info.st_ino,
        "rdev": info.st_rdev,
        "target_mode": stat.S_IMODE(info.st_mode),
        "target_uid": info.st_uid,
        "target_gid": info.st_gid,
    }


def elf_bytes(elf_type=2):
    """只构造可识别的 AArch64 ELF 头，测试从不执行它。"""
    return struct.pack(
        "<16sHHIQQQIHHHHHH",
        b"\x7fELF\x02\x01\x01" + b"\0" * 9,
        elf_type,
        183,
        1,
        0,
        0,
        0,
        0,
        64,
        0,
        0,
        0,
        0,
        0,
    )


class FakeCommands:
    """仅模拟 show、fuser 和 daemon-reload，其他子命令立即失败。"""

    def __init__(self, unit_directory):
        self.unit_directory = unit_directory
        self.calls = []
        self.omit_properties = set()
        self.overrides = {}
        self.busy_devices = False
        self.reload_returncode = 0
        self.cached_values = {}

    def __call__(self, argv):
        argv = [str(arg) for arg in argv]
        self.calls.append(argv)
        command = argv[2:] if argv[:2] == ["sudo", "-n"] else argv
        executable = Path(command[0]).name
        if executable == "fuser":
            return subprocess.CompletedProcess(argv, 0 if self.busy_devices else 1, "1234\n" if self.busy_devices else "", "")
        if executable != "systemctl":
            raise AssertionError("离线测试拒绝外部命令：" + repr(argv))
        if command[1:] == ["daemon-reload"]:
            if self.reload_returncode == 0:
                self.cached_values.clear()
            return subprocess.CompletedProcess(argv, self.reload_returncode, "", "")
        if len(command) < 3 or command[1] != "show":
            raise AssertionError("安装器不得启动或停止服务：" + repr(argv))
        units = [arg for arg in command[2:] if arg.endswith(".service")]
        if not units:
            raise AssertionError("show 缺少明确 unit：" + repr(argv))
        properties = []
        for arg in command[2:]:
            if arg.startswith("--property="):
                properties.extend(arg.split("=", 1)[1].split(","))
        if not properties:
            raise AssertionError("show 必须明确请求属性：" + repr(argv))
        blocks = []
        for unit in units:
            target = self.unit_directory / unit
            missing = not target.exists()
            values = {
                "Id": unit,
                "LoadState": "not-found" if missing else "loaded",
                "ActiveState": "active" if unit == OLD_UNIT_NAMES[0] else "inactive",
                "SubState": "exited" if unit == OLD_UNIT_NAMES[0] else "dead",
                "MainPID": "0",
                "Job": "",
                "ExecMainPID": "0",
                "ControlPID": "0",
                "UnitFileState": "" if missing else "disabled",
                "FragmentPath": "" if missing else str(target) if target.exists() else "/usr/lib/systemd/system/" + unit,
                "DropInPaths": "",
                "NeedDaemonReload": "no",
                "Type": "oneshot" if unit == OLD_UNIT_NAMES[0] else "simple",
                "RemainAfterExit": "yes" if unit == OLD_UNIT_NAMES[0] else "no",
            }
            # systemd 仅在 daemon-reload 后重新发现磁盘上的 unit 文件。
            values = self.cached_values.setdefault(unit, values).copy()
            values.update(self.overrides.get(unit, {}))
            blocks.append("\n".join(
                name + "=" + values.get(name, "")
                for name in properties if name not in self.omit_properties
            ))
        return subprocess.CompletedProcess(argv, 0, "\n\n".join(blocks) + "\n", "")


class DeploymentFixture:
    def __init__(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="s0-unit-installer-")
        self.root = Path(self.temporary.name).resolve()
        self.candidate = self.root / "candidate"
        self.unit_directory = self.root / "systemd"
        self.protection_root = self.root / "protection"
        self.backup_dir = self.root / "backup"
        self.candidate.mkdir()
        self.unit_directory.mkdir()
        self.commands = FakeCommands(self.unit_directory)
        self.old_handoff = b"[Service]\nType=oneshot\nExecStart=/fixture/old-handoff\n"
        self.write(self.unit_directory / OLD_UNIT_NAMES[0], self.old_handoff, 0o640)
        for name in OLD_UNIT_NAMES[1:]:
            self.write(self.unit_directory / name, ("old " + name + "\n").encode(), 0o640)
        self.originals = {name: reference(self.unit_directory / name) for name in OLD_UNIT_NAMES}
        self.old_handoff_identity = reference(self.unit_directory / OLD_UNIT_NAMES[0])
        self.write(self.root / "bin" / "robot-deploy", elf_bytes(), 0o755)
        self.write(self.root / "bin" / "robot-s0", elf_bytes(3), 0o755)
        self.python_real = self.root / "python" / "python3"
        self.write(self.python_real, elf_bytes(3), 0o755)
        self.python = self.root / "venv" / "bin" / "python"
        self.python.parent.mkdir(parents=True)
        self.python.symlink_to(self.python_real)
        self.write(self.root / "venv" / "pyvenv.cfg", b"home = /fixture/python\n")
        self.devices = {}
        for name in ("left", "right", "head_color", "left_wrist_color", "right_wrist_color"):
            device = self.root / "devices" / name
            device.parent.mkdir(exist_ok=True)
            device.symlink_to("/dev/null")
            self.devices[name] = device_identity(device)
        self.domains = {}
        self.protection = {}
        for side, hardware in (("left", "left_arm_full_with_head"), ("right", "right_arm_full_with_base")):
            domain = self.protection_root / ("xlerobot_" + side)
            self.domains[side] = {
                "robot_id": "xlerobot_bus1_full" if side == "left" else "xlerobot_bus2_full",
                "hardware_id": hardware,
                "rx_trust_dir": str(domain),
                "latch_path": str(domain / "estop_latch"),
            }
            self.protection[side] = {}
            for name in PROTECTION_NAMES:
                path = domain / name
                self.write(path, b"0\n", 0o600)
                self.protection[side][name] = reference(path)
        calibrations = []
        for name in ("left_arm", "head", "right_arm", "wheels"):
            path = self.root / "calibration" / (name + ".json")
            self.write(path, b"{}\n")
            calibrations.append(path)
        self.write(self.candidate / "camera_capture.py", "# 离线相机夹具\n".encode(), 0o444)
        for side, bus in (("left", 1), ("right", 2)):
            config = self.candidate / "configs" / (f"xlerobot-bus{bus}.toml")
            self.write(config, ("[robot]\nid = \"" + self.domains[side]["robot_id"] + "\"\n").encode(), 0o444)
        deploy = self.root / "bin" / "robot-deploy"
        s0 = self.root / "bin" / "robot-s0"
        handoff = (
            "[Service]\nType=oneshot\nRemainAfterExit=yes\n"
            + f"WorkingDirectory={self.candidate}\n"
            + f"ExecStart={deploy} preflight-handoff --left-config {self.config('left')} --right-config {self.config('right')}\n"
        )
        self.write(self.candidate / "systemd" / UNIT_NAMES[0], handoff.encode(), 0o444)
        for side, unit in zip(("left", "right"), UNIT_NAMES[1:]):
            domain = self.domains[side]
            content = (
                "[Service]\nType=notify\n"
                + f"WorkingDirectory={self.candidate}\n"
                + f"ExecStart={deploy} guard --arm {side} --config {self.config(side)} --s0-bin {s0} "
                + f"--robot-id {domain['robot_id']} -- --robot-id {domain['robot_id']} "
                + f"--hardware-id {domain['hardware_id']} --latch-path {domain['latch_path']} "
                + f"--rx-trust-dir {domain['rx_trust_dir']} --notify-mode required\n"
            )
            self.write(self.candidate / "systemd" / unit, content.encode(), 0o444)
        candidate_names = ["camera_capture.py"] + [
            "configs/xlerobot-bus1.toml", "configs/xlerobot-bus2.toml"
        ] + ["systemd/" + name for name in UNIT_NAMES]
        python_paths = [path_identity(self.python), path_identity(self.python_real)]
        self.manifest = {
            "schema": "S0_DEPLOY_PREPARE_20261111_V1",
            "result": "PREPARED_NOT_INSTALLED",
            "root": str(self.root),
            "D15": "OPEN/DEFERRED",
            "deployable": False,
            "installed": False,
            "hardware_started": False,
            "uart_opened": False,
            "camera_opened": False,
            "service_commands_executed": False,
            "guard_preflight_provision_executed": False,
            "protection_state_copied": False,
            "candidate_files": {name: reference(self.candidate / name) for name in candidate_names},
            "binaries": {path.name: {**reference(path), "elf_machine": 183} for path in (deploy, s0)},
            "dependencies": {str(path): reference(path) for path in calibrations + [self.python_real, self.root / "venv" / "pyvenv.cfg"]},
            "protection": self.protection,
            "protection_domains": self.domains,
            "devices_stat_only": self.devices,
            "acceptance": {
                "left": {"python_paths": {name: python_paths for name in ("head_color", "left_wrist_color")}},
                "right": {"python_paths": {"right_wrist_color": python_paths}},
            },
            "config_same_path_checks": {
                side: {"guard": str(self.config(side)), "s0_effective": str(self.config(side)), "handoff": str(self.config(side)), "same_path": True}
                for side in ("left", "right")
            },
        }
        self.freeze_manifest()

    @staticmethod
    def write(path, content, mode=0o644):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
        path.chmod(mode)

    def config(self, side):
        return self.candidate / "configs" / ("xlerobot-bus1.toml" if side == "left" else "xlerobot-bus2.toml")

    def freeze_manifest(self):
        path = self.candidate / "manifest.json"
        path.write_text(json.dumps(self.manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        path.chmod(0o444)
        self.manifest_sha256 = hashlib.sha256(path.read_bytes()).hexdigest()

    def make_installer(self):
        return installer.S0UnitInstaller(
            self.candidate,
            self.manifest_sha256,
            unit_directory=self.unit_directory,
            runner=self.commands,
            protection_root=self.protection_root,
        )

    def cleanup(self):
        self.temporary.cleanup()


class UnitInstallerTests(unittest.TestCase):
    def setUp(self):
        self.fixture = DeploymentFixture()
        self.addCleanup(self.fixture.cleanup)
        self.subject = self.fixture.make_installer()

    def assert_original_targets(self):
        for name, expected in self.fixture.originals.items():
            self.assertTrue(installer.same_content(reference(self.fixture.unit_directory / name), expected))
        for name in UNIT_NAMES:
            self.assertFalse((self.fixture.unit_directory / name).exists())

    def assert_no_reload(self):
        self.assertFalse(any(call[-1] == "daemon-reload" for call in self.fixture.commands.calls))

    @staticmethod
    def damage_runtime_file(path):
        mode = stat.S_IMODE(path.stat().st_mode)
        path.chmod(0o644)
        path.write_bytes(b"damaged runtime fixture\n")
        path.chmod(mode)

    def assert_runtime_damage_prevents_install(self, path):
        self.subject.backup(self.fixture.backup_dir)
        self.damage_runtime_file(path)
        with self.assertRaises(REJECTED):
            self.subject.check()
        with self.assertRaises(REJECTED):
            self.subject.install(self.fixture.backup_dir)
        self.assert_original_targets()
        self.assert_no_reload()

    def assert_runtime_damage_allows_rollback(self, path):
        self.subject.backup(self.fixture.backup_dir)
        self.subject.install(self.fixture.backup_dir)
        self.damage_runtime_file(path)
        with self.assertRaises(REJECTED):
            self.subject.check()
        self.subject.rollback(self.fixture.backup_dir)
        self.assert_original_targets()

    def test_check_is_read_only_and_accepts_zero_protection_contents(self):
        before = {str(path): reference(path) for path in self.fixture.protection_root.rglob("*") if path.is_file()}
        result = self.subject.check()
        self.assertIsInstance(result, dict)
        self.assert_original_targets()
        self.assertFalse(self.fixture.backup_dir.exists())
        self.assertEqual(before, {path: reference(Path(path)) for path in before})
        self.assert_no_reload()

    def test_backup_records_six_paths_and_copies_only_existing_targets(self):
        unrelated = self.fixture.unit_directory / "unrelated.service"
        unrelated.write_bytes(b"unrelated\n")
        result = self.subject.backup(self.fixture.backup_dir)
        self.assertIsInstance(result, dict)
        units = self.fixture.backup_dir / "units"
        self.assertEqual(sorted(path.name for path in units.iterdir()), sorted(OLD_UNIT_NAMES))
        record = json.loads((self.fixture.backup_dir / "backup.json").read_text())
        self.assertEqual(set(record["originals"]), set(MANAGED_UNIT_NAMES))
        self.assertEqual((units / OLD_UNIT_NAMES[0]).read_bytes(), self.fixture.old_handoff)
        self.assertTrue((self.fixture.backup_dir / "backup.json").is_file())
        self.assertTrue((self.fixture.backup_dir / "backup.sha256").is_file())
        self.assert_original_targets()
        self.assert_no_reload()

    def test_install_replaces_real_files_and_rollback_restores_missing_targets(self):
        protection = {row["path"]: reference(Path(row["path"])) for rows in self.fixture.protection.values() for row in rows.values()}
        self.subject.backup(self.fixture.backup_dir)
        self.assertIsInstance(self.subject.install(self.fixture.backup_dir), dict)
        for name in UNIT_NAMES:
            target = self.fixture.unit_directory / name
            self.assertEqual(target.read_bytes(), (self.fixture.candidate / "systemd" / name).read_bytes())
            self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o644)
            self.assertEqual((target.stat().st_uid, target.stat().st_gid), (os.geteuid(), os.getegid()))
        for name in OLD_UNIT_NAMES:
            self.assertFalse((self.fixture.unit_directory / name).exists())
        self.assertEqual(self.fixture.commands.cached_values[UNIT_NAMES[0]]["ActiveState"], "inactive")
        self.assertEqual(self.fixture.commands.cached_values[OLD_UNIT_NAMES[0]]["ActiveState"], "active")
        self.assertEqual(protection, {path: reference(Path(path)) for path in protection})
        self.assertIsInstance(self.subject.rollback(self.fixture.backup_dir), dict)
        self.assert_original_targets()
        self.assertEqual(sum(call[-1] == "daemon-reload" for call in self.fixture.commands.calls), 2)

    def test_rollback_restores_all_original_bytes_and_permissions(self):
        originals = {}
        for index, name in enumerate(MANAGED_UNIT_NAMES):
            path = self.fixture.unit_directory / name
            if name in UNIT_NAMES:
                self.fixture.write(path, (self.fixture.candidate / "systemd" / name).read_bytes(), 0o644)
            else:
                self.fixture.write(path, b"old unit " + bytes([index]) + b"\n", 0o600 | (index * 0o20))
            originals[name] = (path.read_bytes(), stat.S_IMODE(path.stat().st_mode), path.stat().st_uid, path.stat().st_gid)
        self.subject.backup(self.fixture.backup_dir)
        self.subject.install(self.fixture.backup_dir)
        self.subject.rollback(self.fixture.backup_dir)
        for name, expected in originals.items():
            path = self.fixture.unit_directory / name
            self.assertEqual((path.read_bytes(), stat.S_IMODE(path.stat().st_mode), path.stat().st_uid, path.stat().st_gid), expected)

    def test_damaged_helper_prevents_check_and_install(self):
        self.assert_runtime_damage_prevents_install(self.fixture.candidate / "camera_capture.py")

    def test_damaged_elf_prevents_check_and_install(self):
        self.assert_runtime_damage_prevents_install(self.fixture.root / "bin" / "robot-s0")

    def test_damaged_installed_helper_does_not_block_rollback(self):
        self.assert_runtime_damage_allows_rollback(self.fixture.candidate / "camera_capture.py")

    def test_damaged_installed_elf_does_not_block_rollback(self):
        self.assert_runtime_damage_allows_rollback(self.fixture.root / "bin" / "robot-s0")

    def test_install_receipt_cannot_be_overwritten_by_repeated_install(self):
        self.subject.backup(self.fixture.backup_dir)
        self.subject.install(self.fixture.backup_dir)
        receipt = self.fixture.backup_dir / "install-result.json"
        before = receipt.read_bytes()
        with self.assertRaises(REJECTED):
            self.subject.install(self.fixture.backup_dir)
        self.assertEqual(receipt.read_bytes(), before)

    def test_inactive_handoff_is_preserved_without_starting_it(self):
        commands = self.fixture.commands
        commands.overrides[OLD_UNIT_NAMES[0]] = {"ActiveState": "inactive", "SubState": "dead"}
        self.subject.backup(self.fixture.backup_dir)
        self.subject.install(self.fixture.backup_dir)
        self.subject.rollback(self.fixture.backup_dir)
        self.assert_original_targets()
        self.assertEqual(sum(call[-1] == "daemon-reload" for call in commands.calls), 2)
        for call in commands.calls:
            command = call[2:] if call[:2] == ["sudo", "-n"] else call
            if Path(command[0]).name == "systemctl":
                self.assertIn(command[1], ("show", "daemon-reload"))

    def test_manifest_digest_mismatch_is_rejected(self):
        self.subject = installer.S0UnitInstaller(
            self.fixture.candidate, "0" * 64, unit_directory=self.fixture.unit_directory,
            runner=self.fixture.commands, protection_root=self.fixture.protection_root,
        )
        with self.assertRaises(REJECTED):
            self.subject.check()
        self.assert_original_targets()

    def test_candidate_drift_after_backup_prevents_install(self):
        self.subject.backup(self.fixture.backup_dir)
        path = self.fixture.candidate / "systemd" / UNIT_NAMES[1]
        path.chmod(0o644)
        path.write_bytes(b"changed candidate\n")
        with self.assertRaises(REJECTED):
            self.subject.install(self.fixture.backup_dir)
        self.assert_original_targets()
        self.assert_no_reload()

    def test_old_target_drift_after_backup_prevents_install(self):
        self.subject.backup(self.fixture.backup_dir)
        target = self.fixture.unit_directory / OLD_UNIT_NAMES[0]
        changed = b"changed by another administrator\n"
        target.write_bytes(changed)
        with self.assertRaises(REJECTED):
            self.subject.install(self.fixture.backup_dir)
        self.assertEqual(target.read_bytes(), changed)
        self.assertFalse((self.fixture.unit_directory / UNIT_NAMES[1]).exists())
        self.assert_no_reload()

    def test_newly_created_target_after_backup_prevents_install(self):
        self.subject.backup(self.fixture.backup_dir)
        target = self.fixture.unit_directory / UNIT_NAMES[1]
        target.write_bytes(b"concurrent installation\n")
        with self.assertRaises(REJECTED):
            self.subject.install(self.fixture.backup_dir)
        self.assertEqual(target.read_bytes(), b"concurrent installation\n")
        self.assert_no_reload()

    def test_corrupt_backup_unit_prevents_install(self):
        self.subject.backup(self.fixture.backup_dir)
        path = self.fixture.backup_dir / "units" / OLD_UNIT_NAMES[0]
        path.chmod(0o644)
        path.write_bytes(b"corrupt backup\n")
        path.chmod(0o444)
        with self.assertRaises(REJECTED):
            self.subject.install(self.fixture.backup_dir)
        self.assert_original_targets()
        self.assert_no_reload()

    def test_corrupt_backup_manifest_prevents_rollback(self):
        self.subject.backup(self.fixture.backup_dir)
        self.subject.install(self.fixture.backup_dir)
        path = self.fixture.backup_dir / "backup.json"
        path.chmod(0o644)
        path.write_bytes(path.read_bytes() + b" \n")
        path.chmod(0o444)
        with self.assertRaises(REJECTED):
            self.subject.rollback(self.fixture.backup_dir)
        for name in UNIT_NAMES:
            self.assertEqual((self.fixture.unit_directory / name).read_bytes(), (self.fixture.candidate / "systemd" / name).read_bytes())

    def test_second_replace_failure_keeps_partial_install_rollbackable(self):
        self.subject.backup(self.fixture.backup_dir)
        real_replace = os.replace
        replaced = []

        def replace(source, destination, *args, **kwargs):
            destination = Path(destination)
            if destination.parent == self.fixture.unit_directory and destination.name in UNIT_NAMES:
                replaced.append(destination.name)
                if len(replaced) == 2:
                    raise OSError("模拟第二个 unit 替换失败")
            return real_replace(source, destination, *args, **kwargs)

        with patch.object(installer.os, "replace", side_effect=replace):
            with self.assertRaises(REJECTED):
                self.subject.install(self.fixture.backup_dir)
        self.assertEqual(len(replaced), 2)
        first = self.fixture.unit_directory / replaced[0]
        self.assertEqual(first.read_bytes(), (self.fixture.candidate / "systemd" / replaced[0]).read_bytes())
        self.assert_no_reload()
        self.assertTrue((self.fixture.backup_dir / "install-result.json").exists())
        self.subject.rollback(self.fixture.backup_dir)
        self.assert_original_targets()

    def test_partial_install_rolls_back_with_not_found_systemd_cache(self):
        self.subject.backup(self.fixture.backup_dir)
        commands = self.fixture.commands
        handoff_before = commands.cached_values[OLD_UNIT_NAMES[0]].copy()
        self.assertEqual(commands.cached_values[UNIT_NAMES[1]]["LoadState"], "not-found")
        real_replace = os.replace
        replaced = []

        def replace(source, destination, *args, **kwargs):
            destination = Path(destination)
            if destination.parent == self.fixture.unit_directory and destination.name in UNIT_NAMES:
                replaced.append(destination.name)
                if len(replaced) == 3:
                    raise OSError("模拟第三个 unit 替换失败")
            return real_replace(source, destination, *args, **kwargs)

        with patch.object(installer.os, "replace", side_effect=replace):
            with self.assertRaises(REJECTED):
                self.subject.install(self.fixture.backup_dir)
        self.assertEqual(replaced, list(UNIT_NAMES))
        self.assertTrue((self.fixture.unit_directory / UNIT_NAMES[1]).is_file())
        self.assertEqual(commands.cached_values[UNIT_NAMES[1]]["LoadState"], "not-found")
        self.assertEqual(commands.cached_values[UNIT_NAMES[1]]["FragmentPath"], "")
        self.assert_no_reload()
        self.subject.rollback(self.fixture.backup_dir)
        self.assert_original_targets()
        handoff_after = commands.cached_values[OLD_UNIT_NAMES[0]]
        self.assertEqual(
            {name: handoff_after[name] for name in ("LoadState", "ActiveState", "SubState", "MainPID", "Job")},
            {name: handoff_before[name] for name in ("LoadState", "ActiveState", "SubState", "MainPID", "Job")},
        )

    def test_failed_rollback_can_be_explicitly_retried_with_original_receipt_preserved(self):
        self.subject.backup(self.fixture.backup_dir)
        self.subject.install(self.fixture.backup_dir)
        real_unlink = os.unlink
        second_target = self.fixture.unit_directory / UNIT_NAMES[1]
        failed = []

        def unlink(path, *args, **kwargs):
            if Path(path) == second_target and not failed:
                failed.append(str(path))
                raise OSError("模拟回滚第二个动作失败")
            return real_unlink(path, *args, **kwargs)

        with patch.object(installer.os, "unlink", side_effect=unlink):
            with self.assertRaises(REJECTED):
                self.subject.rollback(self.fixture.backup_dir)
        self.assertEqual(failed, [str(second_target)])
        self.assertEqual((self.fixture.unit_directory / OLD_UNIT_NAMES[0]).read_bytes(), self.fixture.old_handoff)
        self.assertTrue(second_target.exists())
        attempts = list(self.fixture.backup_dir.glob("rollback-*-result.json"))
        self.assertEqual(len(attempts), 1)
        first_receipt = attempts[0]
        first_bytes = first_receipt.read_bytes()
        self.assertEqual(json.loads(first_bytes)["status"], "FAIL")
        self.subject.rollback(self.fixture.backup_dir)
        self.assert_original_targets()
        attempts = list(self.fixture.backup_dir.glob("rollback-*-result.json"))
        self.assertEqual(len(attempts), 2)
        self.assertEqual(first_receipt.read_bytes(), first_bytes)
        self.assertEqual(sorted(json.loads(path.read_bytes())["status"] for path in attempts), ["FAIL", "PASS"])

    def test_rollback_reload_failure_can_be_retried_with_stale_loaded_cache(self):
        self.subject.backup(self.fixture.backup_dir)
        self.subject.install(self.fixture.backup_dir)
        commands = self.fixture.commands
        commands.reload_returncode = 1
        with self.assertRaises(REJECTED):
            self.subject.rollback(self.fixture.backup_dir)
        self.assert_original_targets()
        for name in UNIT_NAMES[1:]:
            self.assertEqual(commands.cached_values[name]["LoadState"], "loaded")
            self.assertEqual(commands.cached_values[name]["FragmentPath"], str(self.fixture.unit_directory / name))
        attempts = list(self.fixture.backup_dir.glob("rollback-*-result.json"))
        self.assertEqual(len(attempts), 1)
        first_receipt = attempts[0]
        first_bytes = first_receipt.read_bytes()
        self.assertEqual(json.loads(first_bytes)["status"], "FAIL")
        commands.reload_returncode = 0
        self.subject.rollback(self.fixture.backup_dir)
        self.assert_original_targets()
        attempts = list(self.fixture.backup_dir.glob("rollback-*-result.json"))
        self.assertEqual(len(attempts), 2)
        self.assertEqual(first_receipt.read_bytes(), first_bytes)
        self.assertEqual(sorted(json.loads(path.read_bytes())["status"] for path in attempts), ["FAIL", "PASS"])

    def test_unit_directory_lock_rejects_concurrent_check(self):
        descriptor = os.open(self.fixture.unit_directory, os.O_RDONLY | os.O_DIRECTORY)
        self.addCleanup(os.close, descriptor)
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        with self.assertRaises(REJECTED):
            self.subject.check()
        self.assert_original_targets()

    def test_old_unit_deletion_failure_is_explicitly_rollbackable(self):
        self.subject.backup(self.fixture.backup_dir)
        real_unlink = os.unlink
        failed_target = self.fixture.unit_directory / OLD_UNIT_NAMES[1]

        def unlink(path, *args, **kwargs):
            if Path(path) == failed_target:
                raise OSError("模拟旧文件删除中断")
            return real_unlink(path, *args, **kwargs)

        with patch.object(installer.os, "unlink", side_effect=unlink):
            with self.assertRaises(REJECTED):
                self.subject.install(self.fixture.backup_dir)
        self.assertFalse((self.fixture.unit_directory / OLD_UNIT_NAMES[0]).exists())
        self.assertTrue(failed_target.exists())
        receipt = self.fixture.backup_dir / "install-result.json"
        original_failure = receipt.read_bytes()
        self.assertEqual(json.loads(original_failure)["status"], "FAIL")
        self.assert_no_reload()
        self.subject.rollback(self.fixture.backup_dir)
        self.assert_original_targets()
        self.assertEqual(receipt.read_bytes(), original_failure)

    def test_install_reload_failure_rolls_back_both_stale_cache_directions(self):
        self.subject.backup(self.fixture.backup_dir)
        self.fixture.commands.reload_returncode = 1
        with self.assertRaises(REJECTED):
            self.subject.install(self.fixture.backup_dir)
        for name in UNIT_NAMES:
            self.assertTrue((self.fixture.unit_directory / name).exists())
            self.assertEqual(self.fixture.commands.cached_values[name]["LoadState"], "not-found")
        for name in OLD_UNIT_NAMES:
            self.assertFalse((self.fixture.unit_directory / name).exists())
            self.assertEqual(self.fixture.commands.cached_values[name]["LoadState"], "loaded")
        self.fixture.commands.reload_returncode = 0
        self.subject.rollback(self.fixture.backup_dir)
        self.assert_original_targets()

    def test_new_preflight_cannot_acquire_completion_state_during_install(self):
        self.subject.backup(self.fixture.backup_dir)
        commands = self.fixture.commands

        def runner(argv):
            result = commands(argv)
            if argv == ["systemctl", "daemon-reload"]:
                commands.overrides[UNIT_NAMES[0]] = {"ActiveState": "active", "SubState": "exited"}
            return result

        self.subject.runner = runner
        with self.assertRaisesRegex(ValueError, "preflight 完成态变化"):
            self.subject.install(self.fixture.backup_dir)
        self.assertEqual(json.loads((self.fixture.backup_dir / "install-result.json").read_text())["status"], "FAIL")

    def test_existing_new_preflight_completed_state_is_not_reexecuted(self):
        target = self.fixture.unit_directory / UNIT_NAMES[0]
        candidate_bytes = (self.fixture.candidate / "systemd" / UNIT_NAMES[0]).read_bytes()
        self.fixture.write(target, candidate_bytes, 0o644)
        self.fixture.commands.overrides[UNIT_NAMES[0]] = {"ActiveState": "active", "SubState": "exited"}
        self.subject.backup(self.fixture.backup_dir)
        self.subject.install(self.fixture.backup_dir)
        self.subject.rollback(self.fixture.backup_dir)
        self.assertEqual(target.read_bytes(), candidate_bytes)
        self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o644)
        self.assertEqual(self.fixture.commands.cached_values[OLD_UNIT_NAMES[0]]["ActiveState"], "active")

    def test_old_and_new_business_activity_blocks_backup_without_writes(self):
        for name in installer.BUSINESS_UNITS:
            with self.subTest(unit=name):
                self.fixture.commands.overrides = {name: {"ActiveState": "active", "SubState": "running", "MainPID": "42"}}
                with self.assertRaises(REJECTED):
                    self.subject.backup(self.fixture.backup_dir)
                self.assertFalse(self.fixture.backup_dir.exists())
                self.assert_original_targets()
        self.assert_no_reload()

    def test_conflicting_new_targets_reject_check_backup_and_install_without_writes(self):
        self.subject.backup(self.fixture.backup_dir)
        for name in UNIT_NAMES:
            for wrong_content in (True, False):
                with self.subTest(unit=name, wrong_content=wrong_content):
                    target = self.fixture.unit_directory / name
                    candidate_bytes = (self.fixture.candidate / "systemd" / name).read_bytes()
                    self.fixture.write(target, b"unrelated existing unit\n" if wrong_content else candidate_bytes,
                                       0o644 if wrong_content else 0o640)
                    # 模拟管理员已 reload 的真实冲突，不能仅靠旧缓存不一致拒绝。
                    self.fixture.commands.cached_values.clear()
                    before = {str(path): reference(path) for directory in
                              (self.fixture.unit_directory, self.fixture.backup_dir, self.fixture.protection_root)
                              for path in directory.rglob("*") if path.is_file()}
                    for operation in (self.subject.check,
                                      lambda: self.subject.backup(self.fixture.root / "conflict-backup"),
                                      lambda: self.subject.install(self.fixture.backup_dir)):
                        with self.assertRaisesRegex(ValueError, "新 unit 路径已被非候选文件占用"):
                            operation()
                        self.assertEqual(before, {str(path): reference(path) for directory in
                                         (self.fixture.unit_directory, self.fixture.backup_dir, self.fixture.protection_root)
                                         for path in directory.rglob("*") if path.is_file()})
                        self.assertFalse((self.fixture.root / "conflict-backup").exists())
                    target.unlink()
                    self.fixture.commands.cached_values.clear()
        self.assert_original_targets()
        self.assert_no_reload()

    def test_old_and_new_jobs_block_backup_without_writes(self):
        for name in (*installer.BUSINESS_UNITS, *installer.PREFLIGHT_UNITS):
            with self.subTest(unit=name):
                self.fixture.commands.overrides = {name: {"Job": "7 /org/freedesktop/systemd1/job/7"}}
                with self.assertRaises(REJECTED):
                    self.subject.backup(self.fixture.backup_dir)
                self.assertFalse(self.fixture.backup_dir.exists())
                self.assert_original_targets()
        self.assert_no_reload()

    def test_enabled_or_alias_target_is_rejected(self):
        for name in MANAGED_UNIT_NAMES:
            for state in ("enabled", "enabled-runtime", "alias"):
                with self.subTest(unit=name, state=state):
                    self.fixture.commands.overrides = {name: {"UnitFileState": state}}
                    with self.assertRaises(REJECTED):
                        self.subject.check()
        self.assert_original_targets()
        self.assert_no_reload()

    def test_old_backup_schema_is_rejected_even_with_valid_digest(self):
        self.subject.backup(self.fixture.backup_dir)
        path = self.fixture.backup_dir / "backup.json"
        document = json.loads(path.read_text())
        document.pop("schema")
        raw = json.dumps(document).encode()
        path.chmod(0o600)
        self.fixture.write(path, raw, 0o400)
        (self.fixture.backup_dir / "backup.sha256").chmod(0o600)
        self.fixture.write(self.fixture.backup_dir / "backup.sha256", (hashlib.sha256(raw).hexdigest() + "\n").encode(), 0o400)
        with self.assertRaisesRegex(ValueError, "六路径改名格式"):
            self.subject.install(self.fixture.backup_dir)
        self.assert_original_targets()
        self.assert_no_reload()

    def test_directory_target_is_rejected(self):
        target = self.fixture.unit_directory / UNIT_NAMES[1]
        target.mkdir()
        with self.assertRaises(REJECTED):
            self.subject.check()
        self.assertTrue(target.is_dir())

    def test_symlink_target_is_rejected_without_following_it(self):
        target = self.fixture.unit_directory / UNIT_NAMES[1]
        destination = self.fixture.root / "unrelated"
        destination.write_bytes(b"untouched\n")
        target.symlink_to(destination)
        with self.assertRaises(REJECTED):
            self.subject.check()
        self.assertTrue(target.is_symlink())
        self.assertEqual(destination.read_bytes(), b"untouched\n")

    def test_hardlinked_target_is_rejected(self):
        target = self.fixture.unit_directory / OLD_UNIT_NAMES[0]
        os.link(target, self.fixture.root / "other-link")
        with self.assertRaises(REJECTED):
            self.subject.check()
        self.assertEqual(target.read_bytes(), self.fixture.old_handoff)

    def test_dropin_configuration_is_rejected(self):
        self.fixture.commands.overrides[UNIT_NAMES[0]] = {"DropInPaths": "/etc/systemd/system/xlerobot-handoff.service.d/override.conf"}
        with self.assertRaises(REJECTED):
            self.subject.check()
        self.assert_original_targets()

    def test_masked_unit_is_rejected(self):
        self.fixture.commands.overrides[UNIT_NAMES[1]] = {"LoadState": "masked", "UnitFileState": "masked", "FragmentPath": "/dev/null"}
        with self.assertRaises(REJECTED):
            self.subject.check()
        self.assert_original_targets()

    def test_missing_systemctl_field_is_rejected(self):
        self.fixture.commands.omit_properties.add("MainPID")
        with self.assertRaises(REJECTED):
            self.subject.check()
        self.assert_original_targets()

    def test_missing_systemctl_job_is_rejected(self):
        self.fixture.commands.omit_properties.add("Job")
        with self.assertRaises(REJECTED):
            self.subject.check()
        self.assert_original_targets()

    def test_pending_systemctl_job_is_rejected(self):
        self.fixture.commands.overrides[UNIT_NAMES[1]] = {"Job": "123 /org/freedesktop/systemd1/job/123"}
        with self.assertRaises(REJECTED):
            self.subject.check()
        self.assert_original_targets()

    def test_busy_device_is_rejected(self):
        self.fixture.commands.busy_devices = True
        with self.assertRaises(REJECTED):
            self.subject.check()
        self.assert_original_targets()

    def test_devices_are_only_statted_and_commands_never_start_services(self):
        real_open = os.open
        devices = {row["path"] for row in self.fixture.devices.values()} | {"/dev/null"}

        def open_without_devices(path, *args, **kwargs):
            if str(path) in devices:
                raise AssertionError("安装器不得打开设备：" + str(path))
            return real_open(path, *args, **kwargs)

        with patch.object(installer.os, "open", side_effect=open_without_devices):
            self.subject.backup(self.fixture.backup_dir)
            self.subject.install(self.fixture.backup_dir)
            self.subject.rollback(self.fixture.backup_dir)
        self.assertTrue(self.fixture.commands.calls)
        queried = set()
        for argv in self.fixture.commands.calls:
            command = argv[2:] if argv[:2] == ["sudo", "-n"] else argv
            if Path(command[0]).name == "fuser":
                queried.update(command[1:])
            else:
                self.assertEqual(Path(command[0]).name, "systemctl")
                self.assertIn(command[1], ("show", "daemon-reload"))
        self.assertTrue({row["path"] for row in self.fixture.devices.values()}.issubset(queried))


if __name__ == "__main__":
    unittest.main()
