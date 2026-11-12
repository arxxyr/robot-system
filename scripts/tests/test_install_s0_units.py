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
    """只模拟安装及固定三 unit 自启命令，启动或停止立即失败。"""

    def __init__(self, unit_directory):
        self.unit_directory = unit_directory
        self.calls = []
        self.omit_properties = set()
        self.overrides = {}
        self.busy_devices = False
        self.reload_returncode = 0
        self.cached_values = {}
        self.autostart = dict.fromkeys(UNIT_NAMES, "disabled")
        self.autostart_returncode = 0
        self.autostart_partial = None
        self.autostart_stderr = ""

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
        if command[1] in ("enable", "disable"):
            if command != ["systemctl", command[1], *UNIT_NAMES]:
                raise AssertionError("自启必须固定三 unit 且不得带选项：" + repr(argv))
            names = UNIT_NAMES if self.autostart_partial is None else UNIT_NAMES[:self.autostart_partial]
            for name in names:
                self.autostart[name] = "enabled" if command[1] == "enable" else "disabled"
            return subprocess.CompletedProcess(argv, self.autostart_returncode, "", self.autostart_stderr)
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
                "Result": "success",
                "ExecMainCode": "0",
                "ExecMainStatus": "0",
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
            if unit in self.autostart and not missing:
                values["UnitFileState"] = self.autostart[unit]
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
            self.write(config, ("[robot]\nid = \"" + self.domains[side]["robot_id"]
                                + "\"\n[[hardware]]\nauto_enable = false\n").encode(), 0o444)
        deploy = self.root / "bin" / "robot-deploy"
        s0 = self.root / "bin" / "robot-s0"
        handoff = (
            "[Service]\nType=oneshot\nRemainAfterExit=yes\n"
            + f"WorkingDirectory={self.candidate}\n"
            + f"ExecStart={deploy} preflight-handoff --left-config {self.config('left')} --right-config {self.config('right')}\n"
            + "[Install]\nWantedBy=multi-user.target\n"
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
                + "[Install]\nWantedBy=multi-user.target\n"
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
        if path.exists():
            path.chmod(0o644)
        path.write_text(json.dumps(self.manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        path.chmod(0o444)
        self.manifest_sha256 = hashlib.sha256(path.read_bytes()).hexdigest()

    def next_candidate(self):
        root = self.root / "next"
        candidate = root / "candidate"
        document = json.loads(json.dumps(self.manifest))
        document["root"] = str(root)
        for name, ref in document["binaries"].items():
            path = root / "bin" / name
            self.write(path, Path(ref["path"]).read_bytes(), ref["mode"])
            document["binaries"][name] = {**reference(path), "elf_machine": 183}
        for name, ref in document["candidate_files"].items():
            path = candidate / name
            content = Path(ref["path"]).read_text().replace(str(self.candidate), str(candidate))
            content = content.replace(str(self.root / "bin"), str(root / "bin"))
            self.write(path, content.encode(), ref["mode"])
            document["candidate_files"][name] = reference(path)
        document["devices_stat_only"] = {name: device_identity(Path(ref["path"])) for name, ref in self.devices.items()}
        path = candidate / "manifest.json"
        self.write(path, (json.dumps(document, ensure_ascii=False, indent=2) + "\n").encode(), 0o444)
        return candidate, hashlib.sha256(path.read_bytes()).hexdigest()

    def make_installer(self, *, candidate=None, manifest_sha256=None, **options):
        return installer.S0UnitInstaller(
            candidate if candidate is not None else self.candidate,
            manifest_sha256 if manifest_sha256 is not None else self.manifest_sha256,
            unit_directory=self.unit_directory,
            runner=self.commands,
            protection_root=self.protection_root,
            **options,
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

    def prepare_autostart(self, fixture=None):
        fixture = fixture or self.fixture
        subject = fixture.make_installer()
        subject.backup(fixture.backup_dir)
        subject.install(fixture.backup_dir)
        for files in fixture.protection.values():
            for name in ("estop_latch", "estop_latch.bak"):
                Path(files[name]["path"]).write_bytes(b"1\n")
        fixture.installation_backup_dir = fixture.backup_dir
        fixture.backup_dir = fixture.root / "autostart-backup"
        subject.backup(fixture.backup_dir)
        fixture.commands.calls.clear()
        return subject

    def assert_no_autostart_changes(self, commands):
        self.assertFalse(any(call[:2] in (["systemctl", "enable"], ["systemctl", "disable"])
                             for call in commands.calls))

    def prepare_upgrade(self, fixture=None):
        fixture = fixture or self.fixture
        previous = fixture.make_installer()
        previous.backup(fixture.backup_dir)
        previous.install(fixture.backup_dir)
        candidate, digest = fixture.next_candidate()
        fixture.backup_dir = fixture.root / "upgrade-backup"
        fixture.commands.calls.clear()
        return fixture.make_installer(candidate=candidate, manifest_sha256=digest,
                                      previous_candidate=fixture.candidate,
                                      previous_manifest_sha256=fixture.manifest_sha256)

    def stopped_failed_upgrade(self, fixture=None):
        fixture = fixture or self.fixture
        subject = self.prepare_upgrade(fixture)
        fixture.commands.autostart = dict.fromkeys(UNIT_NAMES, "enabled")
        fixture.commands.overrides = {
            name: {"ActiveState": "failed", "SubState": "failed", "Result": "exit-code",
                   "ExecMainCode": "1", "ExecMainStatus": "1"}
            for name in UNIT_NAMES[1:]
        }
        return subject

    @staticmethod
    def rewrite_backup(fixture, record):
        raw = (json.dumps(record, ensure_ascii=False, indent=2) + "\n").encode()
        (fixture.backup_dir / "backup.json").chmod(0o600)
        (fixture.backup_dir / "backup.sha256").chmod(0o600)
        fixture.write(fixture.backup_dir / "backup.json", raw, 0o400)
        fixture.write(fixture.backup_dir / "backup.sha256", (hashlib.sha256(raw).hexdigest() + "\n").encode(), 0o400)

    def test_explicit_previous_candidate_upgrades_and_rolls_back_six_paths(self):
        subject = self.prepare_upgrade()
        targets = {name: subject._target(name) for name in MANAGED_UNIT_NAMES}
        protection = {ref["path"]: reference(Path(ref["path"])) for files in self.fixture.protection.values() for ref in files.values()}
        default = self.fixture.make_installer(candidate=subject.candidate, manifest_sha256=subject.manifest_sha256)
        with self.assertRaisesRegex(ValueError, "非候选文件占用"):
            default.check()
        self.assertEqual(subject.check()["result"], "CHECK_PASS")
        subject.backup(self.fixture.backup_dir)
        record = json.loads((self.fixture.backup_dir / "backup.json").read_text())
        self.assertEqual(record["previous_candidate"]["manifest_sha256"], self.fixture.manifest_sha256)
        self.assertEqual(set(record["previous_candidate"]["units"]), set(UNIT_NAMES))
        self.assertEqual(subject.install(self.fixture.backup_dir)["status"], "PASS")
        for name in UNIT_NAMES:
            self.assertEqual((self.fixture.unit_directory / name).read_bytes(), (subject.candidate / "systemd" / name).read_bytes())
        default.rollback(self.fixture.backup_dir)
        for name, expected in targets.items():
            actual = subject._target(name)
            self.assertEqual(actual["exists"], expected["exists"])
            if expected["exists"]:
                self.assertTrue(installer.same_content(actual, expected))
        self.assertEqual(protection, {path: reference(Path(path)) for path in protection})
        self.assert_no_autostart_changes(self.fixture.commands)
        self.assertEqual(sum(call == ["systemctl", "daemon-reload"] for call in self.fixture.commands.calls), 2)

    def test_previous_parameters_must_be_paired_and_are_rejected_by_rollback_and_autostart(self):
        for options in ({"previous_candidate": self.fixture.root / "previous"},
                        {"previous_manifest_sha256": "a" * 64}):
            with self.subTest(options=options), self.assertRaisesRegex(ValueError, "必须成对"):
                self.fixture.make_installer(**options)
        subject = self.prepare_upgrade()
        for action in (subject.autostart_check, lambda: subject.enable_autostart(self.fixture.backup_dir),
                       lambda: subject.disable_autostart(self.fixture.backup_dir), lambda: subject.rollback(self.fixture.backup_dir)):
            with self.assertRaisesRegex(ValueError, "不接受上一版"):
                action()
        for operation in ("rollback", "autostart-check", "enable-autostart", "disable-autostart"):
            with self.subTest(operation=operation), patch("builtins.print"), patch.object(installer, "S0UnitInstaller") as constructor:
                code = installer.main([operation, "--candidate", str(subject.candidate), "--manifest-sha256", subject.manifest_sha256,
                                       "--previous-candidate", str(self.fixture.candidate),
                                       "--previous-manifest-sha256", self.fixture.manifest_sha256])
                self.assertEqual(code, 1)
                constructor.assert_not_called()
        self.assert_no_autostart_changes(self.fixture.commands)
        self.assert_no_reload()

    def test_enabled_failed_upgrade_and_rollback_preserve_autostart_failure_and_protection(self):
        subject = self.stopped_failed_upgrade()
        # 每份状态按备份分别保存，允许既有三份自启状态不一致。
        self.fixture.commands.autostart[UNIT_NAMES[1]] = "disabled"
        before = subject._services(upgrade=True)
        originals = {name: subject._target(name) for name in MANAGED_UNIT_NAMES}
        protection = {ref["path"]: reference(Path(ref["path"])) for rows in self.fixture.protection.values() for ref in rows.values()}
        default = self.fixture.make_installer(candidate=subject.candidate, manifest_sha256=subject.manifest_sha256)
        with self.assertRaises(ValueError):
            default.check()
        self.assertEqual(subject.check()["result"], "CHECK_PASS")
        subject.backup(self.fixture.backup_dir)
        record = json.loads((self.fixture.backup_dir / "backup.json").read_text())
        self.assertEqual(record["services"], before)
        self.assertEqual(subject.install(self.fixture.backup_dir)["status"], "PASS")
        self.assertEqual(subject._services(upgrade=True), before)
        for path in [self.fixture.candidate / "manifest.json", *[self.fixture.candidate / "systemd" / name for name in UNIT_NAMES]]:
            path.unlink()
        # 回滚保留当下终态，不要求回到备份时的终态。
        self.fixture.commands.overrides[UNIT_NAMES[1]] = {"ActiveState": "inactive", "SubState": "dead", "Result": "success",
                                                        "ExecMainCode": "1", "ExecMainStatus": "0"}
        rollback_before = default._services(upgrade=True)
        self.assertEqual(default.rollback(self.fixture.backup_dir)["status"], "PASS")
        self.assertEqual(default._services(upgrade=True), rollback_before)
        for name, expected in originals.items():
            current = subject._target(name)
            self.assertEqual(current["exists"], expected["exists"])
            if expected["exists"]:
                self.assertTrue(installer.same_content(current, expected))
        self.assertEqual(protection, {path: reference(Path(path)) for path in protection})
        self.assert_no_autostart_changes(self.fixture.commands)
        self.assertEqual(sum(call == ["systemctl", "daemon-reload"] for call in self.fixture.commands.calls), 2)

    def test_legacy_same_name_backup_rolls_back_strictly_without_exit_code_properties(self):
        subject = self.prepare_upgrade()
        original = {name: subject._target(name) for name in MANAGED_UNIT_NAMES}
        subject.backup(self.fixture.backup_dir)
        record = json.loads((self.fixture.backup_dir / "backup.json").read_text())
        for service in record["services"].values():
            service.pop("ExecMainCode")
            service.pop("ExecMainStatus")
        self.rewrite_backup(self.fixture, record)
        self.assertEqual(subject.install(self.fixture.backup_dir)["status"], "PASS")
        for path in [self.fixture.candidate / "manifest.json", *[self.fixture.candidate / "systemd" / name for name in UNIT_NAMES]]:
            path.unlink()
        self.fixture.commands.calls.clear()
        rollback = self.fixture.make_installer(candidate=subject.candidate, manifest_sha256=subject.manifest_sha256)
        self.assertEqual(rollback.rollback(self.fixture.backup_dir)["status"], "PASS")
        for name, expected in original.items():
            current = rollback._target(name)
            self.assertEqual(current["exists"], expected["exists"])
            if expected["exists"]:
                self.assertTrue(installer.same_content(current, expected))
        self.assertEqual(json.loads((self.fixture.backup_dir / "backup.json").read_text()), record)
        self.assertEqual(sum(call == ["systemctl", "daemon-reload"] for call in self.fixture.commands.calls), 1)
        self.assert_no_autostart_changes(self.fixture.commands)

    def test_legacy_same_name_backup_does_not_allow_enabled_or_failed_install_or_rollback(self):
        subject = self.prepare_upgrade()
        subject.backup(self.fixture.backup_dir)
        record = json.loads((self.fixture.backup_dir / "backup.json").read_text())
        for service in record["services"].values():
            service.pop("ExecMainCode")
            service.pop("ExecMainStatus")
        self.rewrite_backup(self.fixture, record)
        self.fixture.commands.calls.clear()
        rollback = self.fixture.make_installer(candidate=subject.candidate, manifest_sha256=subject.manifest_sha256)
        before = {name: rollback._target(name) for name in MANAGED_UNIT_NAMES}
        failed = {"ActiveState": "failed", "SubState": "failed", "Result": "exit-code", "ExecMainCode": "1", "ExecMainStatus": "1"}
        for name in UNIT_NAMES:
            for override in ({"UnitFileState": "enabled"}, failed):
                with self.subTest(name=name, override=override):
                    self.fixture.commands.overrides = {name: override}
                    for operation in (subject.install, rollback.rollback):
                        with self.assertRaises(ValueError):
                            operation(self.fixture.backup_dir)
                    self.assertEqual({name: rollback._target(name) for name in MANAGED_UNIT_NAMES}, before)
        self.assertFalse((self.fixture.backup_dir / "install-result.json").exists())
        self.assertFalse(list(self.fixture.backup_dir.glob("rollback-*-result.json")))
        self.assert_no_reload()
        self.assert_no_autostart_changes(self.fixture.commands)

    def test_upgrade_failure_exception_rejects_other_units_and_unknown_terminal_states(self):
        subject = self.stopped_failed_upgrade()
        failed = self.fixture.commands.overrides[UNIT_NAMES[1]].copy()
        invalid = [(name, failed) for name in (UNIT_NAMES[0], *OLD_UNIT_NAMES, installer.BUSINESS_UNITS[0])]
        invalid += [(UNIT_NAMES[1], {**failed, **change}) for change in (
            {"Result": "signal"}, {"ExecMainCode": "2"}, {"ExecMainCode": "exited"}, {"ExecMainStatus": "2"},
            {"ActiveState": "active", "SubState": "running"}, {"MainPID": "42"}, {"Job": "1 /job/1"})]
        for name, value in invalid:
            with self.subTest(name=name, value=value):
                self.fixture.commands.overrides = {name: value}
                before = {unit: subject._target(unit) for unit in MANAGED_UNIT_NAMES}
                with self.assertRaises(ValueError):
                    subject.backup(self.fixture.backup_dir)
                self.assertFalse(self.fixture.backup_dir.exists())
                self.assertEqual({unit: subject._target(unit) for unit in MANAGED_UNIT_NAMES}, before)
        self.fixture.commands.overrides = {UNIT_NAMES[1]: failed}
        for key in ("ExecMainCode", "ExecMainStatus"):
            with self.subTest(missing=key):
                self.fixture.commands.omit_properties = {key}
                with self.assertRaises(ValueError):
                    subject.check()
        self.assert_no_reload()
        self.assert_no_autostart_changes(self.fixture.commands)

    def test_same_name_upgrade_and_bound_rollback_reject_live_old_unit_paths(self):
        subject = self.stopped_failed_upgrade()
        subject.backup(self.fixture.backup_dir)
        rollback = self.fixture.make_installer(candidate=subject.candidate, manifest_sha256=subject.manifest_sha256)
        for name in OLD_UNIT_NAMES:
            with self.subTest(name=name):
                path = self.fixture.unit_directory / name
                self.fixture.write(path, b"unexpected old unit\n")
                self.fixture.commands.cached_values.clear()
                for operation in (subject.check, lambda: subject.install(self.fixture.backup_dir),
                                  lambda: rollback.rollback(self.fixture.backup_dir)):
                    with self.assertRaisesRegex(ValueError, "旧三 unit 路径全部不存在"):
                        operation()
                path.unlink()
                self.fixture.commands.cached_values.clear()
        self.assert_no_reload()
        self.assert_no_autostart_changes(self.fixture.commands)

    def test_bound_rollback_rejects_incomplete_or_unbound_previous_backup(self):
        subject = self.stopped_failed_upgrade()
        subject.backup(self.fixture.backup_dir)
        original = json.loads((self.fixture.backup_dir / "backup.json").read_text())
        rollback = self.fixture.make_installer(candidate=subject.candidate, manifest_sha256=subject.manifest_sha256)
        for damage in ("null", "extra", "missing", "identity", "sha", "unit-path", "unit-content", "missing-original", "old-original",
                       "service-code", "mixed-service", "autostart"):
            with self.subTest(damage=damage):
                record = json.loads(json.dumps(original))
                previous = record["previous_candidate"]
                if damage == "null":
                    record["previous_candidate"] = None
                elif damage == "extra":
                    previous["unexpected"] = True
                elif damage == "missing":
                    previous.pop("units")
                elif damage == "identity":
                    previous["manifest"]["unexpected"] = True
                elif damage == "sha":
                    previous["manifest_sha256"] = "f" * 64
                elif damage == "unit-path":
                    previous["units"][UNIT_NAMES[0]]["path"] += ".other"
                elif damage == "unit-content":
                    previous["units"][UNIT_NAMES[0]]["sha256"] = "f" * 64
                elif damage == "missing-original":
                    record["originals"][UNIT_NAMES[0]]["exists"] = False
                    record["saved"].pop(UNIT_NAMES[0])
                elif damage == "old-original":
                    old = OLD_UNIT_NAMES[0]
                    record["originals"][old] = {**record["originals"][UNIT_NAMES[0]], "path": str(self.fixture.unit_directory / old)}
                    target = self.fixture.backup_dir / "units" / old
                    self.fixture.write(target, (self.fixture.backup_dir / "units" / UNIT_NAMES[0]).read_bytes(), 0o400)
                    record["saved"][old] = installer.file_identity(target)
                elif damage == "service-code":
                    record["services"][UNIT_NAMES[0]].pop("ExecMainCode")
                elif damage == "mixed-service":
                    record["services"][UNIT_NAMES[0]].pop("ExecMainCode")
                    record["services"][UNIT_NAMES[0]].pop("ExecMainStatus")
                else:
                    record["services"][UNIT_NAMES[0]]["UnitFileState"] = "alias"
                self.rewrite_backup(self.fixture, record)
                with self.assertRaises(ValueError):
                    rollback.rollback(self.fixture.backup_dir)
        self.assertFalse(list(self.fixture.backup_dir.glob("rollback-*-result.json")))
        self.assert_no_reload()
        self.assert_no_autostart_changes(self.fixture.commands)

    def test_upgrade_and_bound_rollback_reject_autostart_drift_before_transaction(self):
        subject = self.stopped_failed_upgrade()
        subject.backup(self.fixture.backup_dir)
        rollback = self.fixture.make_installer(candidate=subject.candidate, manifest_sha256=subject.manifest_sha256)
        originals = {name: subject._target(name) for name in MANAGED_UNIT_NAMES}
        for name in UNIT_NAMES:
            with self.subTest(name=name):
                self.fixture.commands.autostart[name] = "disabled"
                for operation in (lambda: subject.install(self.fixture.backup_dir), lambda: rollback.rollback(self.fixture.backup_dir)):
                    with self.assertRaisesRegex(ValueError, "换入前自启状态与备份不一致"):
                        operation()
                self.fixture.commands.autostart[name] = "enabled"
                self.assertEqual({name: subject._target(name) for name in MANAGED_UNIT_NAMES}, originals)
        self.assert_no_reload()
        self.assert_no_autostart_changes(self.fixture.commands)

    def test_upgrade_and_bound_rollback_detect_terminal_or_autostart_drift_after_reload(self):
        for rollback in (False, True):
            for damage in ("autostart", "terminal"):
                with self.subTest(rollback=rollback, damage=damage):
                    fixture = DeploymentFixture()
                    self.addCleanup(fixture.cleanup)
                    subject = self.stopped_failed_upgrade(fixture)
                    subject.backup(fixture.backup_dir)
                    if rollback:
                        subject.install(fixture.backup_dir)
                        subject = fixture.make_installer(candidate=subject.candidate, manifest_sha256=subject.manifest_sha256)
                    failure_before = fixture.commands.overrides[UNIT_NAMES[1]].copy()

                    def command(argv):
                        result = fixture.commands(argv)
                        if argv == ["systemctl", "daemon-reload"]:
                            if damage == "autostart":
                                fixture.commands.autostart[UNIT_NAMES[0]] = "disabled"
                            else:
                                fixture.commands.overrides[UNIT_NAMES[1]] = {"ActiveState": "inactive", "SubState": "dead", "Result": "success"}
                        return result

                    subject.runner = command
                    with self.assertRaisesRegex(ValueError, "换入后自启状态" if damage == "autostart" else "服务终态变化"):
                        (subject.rollback if rollback else subject.install)(fixture.backup_dir)
                    receipt = (next(fixture.backup_dir.glob("rollback-*-result.json")) if rollback
                               else fixture.backup_dir / "install-result.json")
                    self.assertEqual(json.loads(receipt.read_text())["status"], "FAIL")
                    self.assert_no_autostart_changes(fixture.commands)
                    if damage == "autostart":
                        self.assertEqual(fixture.commands.overrides[UNIT_NAMES[1]], failure_before)

    def test_enabled_failed_partial_upgrade_rolls_back_with_original_failure_receipt(self):
        subject = self.stopped_failed_upgrade()
        subject.backup(self.fixture.backup_dir)
        original = {name: subject._target(name) for name in MANAGED_UNIT_NAMES}
        real_replace = os.replace

        def replace(source, destination, *args, **kwargs):
            if Path(destination) == self.fixture.unit_directory / UNIT_NAMES[1]:
                raise OSError("模拟同名升级第二份写入失败")
            return real_replace(source, destination, *args, **kwargs)

        with patch.object(installer.os, "replace", side_effect=replace), self.assertRaises(OSError):
            subject.install(self.fixture.backup_dir)
        receipt = self.fixture.backup_dir / "install-result.json"
        failed = receipt.read_bytes()
        rollback = self.fixture.make_installer(candidate=subject.candidate, manifest_sha256=subject.manifest_sha256)
        self.assertEqual(rollback.rollback(self.fixture.backup_dir)["status"], "PASS")
        self.assertEqual(receipt.read_bytes(), failed)
        self.assertEqual(json.loads(failed)["status"], "FAIL")
        for name, expected in original.items():
            current = subject._target(name)
            self.assertEqual(current["exists"], expected["exists"])
            if expected["exists"]:
                self.assertTrue(installer.same_content(current, expected))
        self.assert_no_autostart_changes(self.fixture.commands)

    def test_previous_manifest_sha_and_static_contract_are_verified(self):
        for damage in ("sha", "schema", "root", "domain", "protection-path", "unit-path"):
            with self.subTest(damage=damage):
                fixture = DeploymentFixture()
                self.addCleanup(fixture.cleanup)
                subject = self.prepare_upgrade(fixture)
                if damage == "sha":
                    subject.previous_manifest_sha256 = "f" * 64
                else:
                    if damage == "schema":
                        fixture.manifest["schema"] = "UNKNOWN"
                    elif damage == "root":
                        fixture.manifest["root"] = str(fixture.root / "other")
                    elif damage == "domain":
                        fixture.manifest["protection_domains"]["right"]["robot_id"] = "other"
                    elif damage == "protection-path":
                        fixture.manifest["protection"]["right"]["estop_latch"]["path"] += ".other"
                    elif damage == "unit-path":
                        fixture.manifest["candidate_files"]["systemd/" + UNIT_NAMES[0]]["path"] += ".other"
                    fixture.freeze_manifest()
                    subject.previous_manifest_sha256 = fixture.manifest_sha256
                with self.assertRaises(ValueError):
                    subject.check()
                self.assertFalse(fixture.backup_dir.exists())
                self.assertFalse(any(call == ["systemctl", "daemon-reload"] for call in fixture.commands.calls))

    def test_previous_requires_complete_unmixed_stopped_units_without_dropins(self):
        subject = self.prepare_upgrade()
        target = self.fixture.unit_directory / UNIT_NAMES[1]
        original = target.read_bytes()
        for damage in ("missing", "mixed", "unknown", "mode"):
            with self.subTest(damage=damage):
                if damage == "missing":
                    target.unlink()
                elif damage == "mixed":
                    self.fixture.write(target, (subject.candidate / "systemd" / target.name).read_bytes())
                elif damage == "unknown":
                    self.fixture.write(target, b"unknown unit\n")
                else:
                    target.chmod(0o640)
                self.fixture.commands.cached_values.clear()
                with self.assertRaises(ValueError):
                    subject.backup(self.fixture.backup_dir)
                self.fixture.write(target, original)
                self.assertFalse(self.fixture.backup_dir.exists())
        for override in ({"ActiveState": "active", "SubState": "running"}, {"MainPID": "42"}, {"Job": "1 /job/1"},
                         {"UnitFileState": "enabled-runtime"}, {"UnitFileState": "alias"},
                         {"UnitFileState": "masked"}, {"DropInPaths": "/other.conf"}):
            with self.subTest(override=override):
                self.fixture.commands.overrides = {target.name: override}
                with self.assertRaises(ValueError):
                    subject.backup(self.fixture.backup_dir)
                self.assertFalse(self.fixture.backup_dir.exists())
        self.fixture.commands.overrides = {}
        dropin = Path(str(target) + ".d")
        dropin.mkdir()
        with self.assertRaisesRegex(ValueError, "drop-in"):
            subject.backup(self.fixture.backup_dir)
        dropin.rmdir()
        self.assert_no_reload()
        self.assert_no_autostart_changes(self.fixture.commands)

    def test_previous_source_or_target_drift_after_backup_prevents_install(self):
        for damage in ("source-content", "source-identity", "manifest-identity", "target-content", "target-identity"):
            with self.subTest(damage=damage):
                fixture = DeploymentFixture()
                self.addCleanup(fixture.cleanup)
                subject = self.prepare_upgrade(fixture)
                subject.backup(fixture.backup_dir)
                backup = (fixture.backup_dir / "backup.json").read_bytes()
                path = (fixture.candidate / "systemd" / UNIT_NAMES[1] if damage.startswith("source")
                        else fixture.candidate / "manifest.json" if damage.startswith("manifest")
                        else fixture.unit_directory / UNIT_NAMES[1])
                before = reference(path)
                if damage.endswith("content"):
                    self.damage_runtime_file(path)
                else:
                    subject._atomic_replace(path, path.read_bytes(), before)
                    self.assertNotEqual(reference(path)["inode"], before["inode"])
                with self.assertRaises(ValueError):
                    subject.install(fixture.backup_dir)
                self.assertEqual((fixture.backup_dir / "backup.json").read_bytes(), backup)
                self.assertFalse((fixture.backup_dir / "install-result.json").exists())
                self.assertFalse(any(call == ["systemctl", "daemon-reload"] for call in fixture.commands.calls))

    def test_install_previous_selection_must_match_backup(self):
        subject = self.prepare_upgrade()
        subject.backup(self.fixture.backup_dir)
        for options in ({}, {"previous_candidate": self.fixture.candidate, "previous_manifest_sha256": "f" * 64},
                        {"previous_candidate": self.fixture.root / "other", "previous_manifest_sha256": self.fixture.manifest_sha256}):
            with self.subTest(options=options):
                selected = self.fixture.make_installer(candidate=subject.candidate, manifest_sha256=subject.manifest_sha256, **options)
                with self.assertRaisesRegex(ValueError, "上一版选择与备份不一致"):
                    selected.install(self.fixture.backup_dir)
        self.assertFalse((self.fixture.backup_dir / "install-result.json").exists())
        self.assert_no_reload()

    def test_previous_runtime_and_old_device_generation_are_not_upgrade_dependencies(self):
        self.subject.backup(self.fixture.backup_dir)
        self.subject.install(self.fixture.backup_dir)
        device = Path(self.fixture.devices["right"]["path"])
        replacement = device.with_name("next-enumeration")
        replacement.symlink_to(os.readlink(device))
        os.replace(replacement, device)
        self.assertNotEqual(device.lstat().st_ino, self.fixture.devices["right"]["inode"])
        candidate, digest = self.fixture.next_candidate()
        subject = self.fixture.make_installer(candidate=candidate, manifest_sha256=digest,
                                              previous_candidate=self.fixture.candidate,
                                              previous_manifest_sha256=self.fixture.manifest_sha256)
        for path in (self.fixture.root / "bin/robot-s0", self.fixture.candidate / "camera_capture.py", self.fixture.config("right")):
            path.unlink()
        self.fixture.backup_dir = self.fixture.root / "upgrade-backup"
        self.fixture.commands.calls.clear()
        self.assertEqual(subject.check()["result"], "CHECK_PASS")
        subject.backup(self.fixture.backup_dir)
        replacement.symlink_to(os.readlink(device))
        os.replace(replacement, device)
        for action in (subject.check, lambda: subject.install(self.fixture.backup_dir)):
            with self.assertRaisesRegex(ValueError, "设备链接身份变化"):
                action()
        self.assert_no_reload()

    def test_partial_upgrade_rolls_back_without_previous_source_files(self):
        subject = self.prepare_upgrade()
        originals = {name: subject._target(name) for name in MANAGED_UNIT_NAMES}
        subject.backup(self.fixture.backup_dir)
        real_replace, replaced = os.replace, []

        def replace(source, destination, *args, **kwargs):
            target = Path(destination)
            if target.parent == self.fixture.unit_directory and target.name in UNIT_NAMES:
                replaced.append(target.name)
                if len(replaced) == 2:
                    raise OSError("模拟升级第二份 unit 换入失败")
            return real_replace(source, destination, *args, **kwargs)

        with patch.object(installer.os, "replace", side_effect=replace), self.assertRaises(OSError):
            subject.install(self.fixture.backup_dir)
        receipt = self.fixture.backup_dir / "install-result.json"
        failure = receipt.read_bytes()
        self.assertEqual(json.loads(failure)["status"], "FAIL")
        for path in [self.fixture.candidate / "manifest.json", *[self.fixture.candidate / "systemd" / name for name in UNIT_NAMES]]:
            path.unlink()
        rollback = self.fixture.make_installer(candidate=subject.candidate, manifest_sha256=subject.manifest_sha256)
        self.assertEqual(rollback.rollback(self.fixture.backup_dir)["status"], "PASS")
        self.assertEqual(receipt.read_bytes(), failure)
        for name, expected in originals.items():
            actual = rollback._target(name)
            self.assertEqual(actual["exists"], expected["exists"])
            if expected["exists"]:
                self.assertTrue(installer.same_content(actual, expected))
        self.assert_no_autostart_changes(self.fixture.commands)

    def test_autostart_check_is_read_only_reports_actual_states_and_rx(self):
        subject = self.prepare_autostart()
        self.fixture.commands.autostart[UNIT_NAMES[1]] = "enabled"
        before = {str(path): reference(path) for path in self.fixture.root.rglob("*") if path.is_file()}
        result = subject.autostart_check()
        self.assertEqual(result["result"], "AUTOSTART_CHECK_PASS")
        self.assertEqual(result["autostart"], self.fixture.commands.autostart)
        self.assertEqual(result["protection_values"]["left"]["rx_trust_marker"], "0\n")
        self.assertFalse(result["rx_authorize_executed"])
        self.assertFalse(result["hardware_started"])
        self.assertEqual(before, {str(path): reference(path) for path in self.fixture.root.rglob("*") if path.is_file()})
        self.assert_no_autostart_changes(self.fixture.commands)
        self.assert_no_reload()
        for action in (subject.check, lambda: subject.install(self.fixture.backup_dir), lambda: subject.rollback(self.fixture.backup_dir)):
            with self.assertRaisesRegex(ValueError, "启用状态"):
                action()

    def test_autostart_enable_disable_keeps_batch_backup_and_allows_original_rollback(self):
        subject = self.prepare_autostart()
        backup = (self.fixture.backup_dir / "backup.json").read_bytes()
        self.assertFalse((self.fixture.backup_dir / "install-result.json").exists())
        protected = {ref["path"]: reference(Path(ref["path"])) for files in self.fixture.protection.values() for ref in files.values()}
        enabled = subject.enable_autostart(self.fixture.backup_dir)
        self.assertEqual(enabled["status"], "PASS")
        self.assertEqual(set(enabled["before"].values()), {"disabled"})
        self.assertEqual(set(enabled["after"].values()), {"enabled"})
        with self.assertRaisesRegex(ValueError, "必须全部 disabled"):
            subject.enable_autostart(self.fixture.backup_dir)
        disabled = subject.disable_autostart(self.fixture.backup_dir)
        self.assertEqual(disabled["status"], "PASS")
        self.assertEqual(set(disabled["after"].values()), {"disabled"})
        changes = [call for call in self.fixture.commands.calls if call[:2] in (["systemctl", "enable"], ["systemctl", "disable"])]
        self.assertEqual(changes, [["systemctl", "enable", *UNIT_NAMES], ["systemctl", "disable", *UNIT_NAMES]])
        self.assertEqual((self.fixture.backup_dir / "backup.json").read_bytes(), backup)
        self.assertFalse((self.fixture.backup_dir / "install-result.json").exists())
        self.assertEqual(protected, {path: reference(Path(path)) for path in protected})
        subject.rollback(self.fixture.installation_backup_dir)
        self.assert_original_targets()

    def test_autostart_only_relaxes_new_units_enabled_or_disabled_state(self):
        subject = self.prepare_autostart()
        for name, state in [(name, "enabled") for name in OLD_UNIT_NAMES] + [(name, "enabled-runtime") for name in UNIT_NAMES] + [(UNIT_NAMES[0], "static")]:
            with self.subTest(name=name, state=state):
                self.fixture.commands.overrides = {name: {"UnitFileState": state}}
                with self.assertRaises(ValueError):
                    subject.autostart_check()
        self.fixture.commands.overrides = {}
        for key, value in (("Job", "123 /job/123"), ("MainPID", "123"), ("ActiveState", "activating")):
            with self.subTest(key=key):
                self.fixture.commands.overrides = {UNIT_NAMES[1]: {key: value}}
                with self.assertRaises(ValueError):
                    subject.autostart_check()
        self.assert_no_autostart_changes(self.fixture.commands)

    def test_autostart_requires_backup_of_installed_and_disabled_candidate_units(self):
        subject = self.prepare_autostart()
        with self.assertRaisesRegex(ValueError, "已安装且全部 disabled"):
            subject.enable_autostart(self.fixture.installation_backup_dir)
        path = self.fixture.backup_dir / "backup.json"
        original = json.loads(path.read_text())
        for name in UNIT_NAMES:
            with self.subTest(name=name):
                changed = json.loads(json.dumps(original))
                changed["services"][name]["UnitFileState"] = "enabled"
                raw = json.dumps(changed).encode()
                path.chmod(0o600)
                self.fixture.write(path, raw, 0o400)
                checksum = self.fixture.backup_dir / "backup.sha256"
                checksum.chmod(0o600)
                self.fixture.write(checksum, (hashlib.sha256(raw).hexdigest() + "\n").encode(), 0o400)
                with self.assertRaisesRegex(ValueError, "已安装且全部 disabled"):
                    subject.enable_autostart(self.fixture.backup_dir)
        self.assert_no_autostart_changes(self.fixture.commands)
        receipts = list(self.fixture.backup_dir.glob("enable-autostart-*-result.json"))
        self.assertEqual(len(receipts), 3)
        self.assertTrue(all(json.loads(path.read_text())["status"] == "FAIL" for path in receipts))

    def test_autostart_enable_rejects_true_or_missing_auto_enable(self):
        for hardware in ("[[hardware]]\nauto_enable=true\n", "[[hardware]]\n", "", "[[hardware]]\nauto_enable=0\n"):
            with self.subTest(hardware=hardware):
                fixture = DeploymentFixture()
                self.addCleanup(fixture.cleanup)
                config = fixture.config("right")
                config.chmod(0o644)
                fixture.write(config, ("[robot]\nid='test'\n" + hardware).encode(), 0o444)
                fixture.manifest["candidate_files"]["configs/xlerobot-bus2.toml"] = reference(config)
                fixture.freeze_manifest()
                subject = self.prepare_autostart(fixture)
                with self.assertRaisesRegex(ValueError, "auto_enable=false"):
                    subject.enable_autostart(fixture.backup_dir)
                self.assert_no_autostart_changes(fixture.commands)

    def test_autostart_rejects_missing_or_additional_install_directives(self):
        for directives in ("", "WantedBy=other.target\n", "WantedBy=multi-user.target\nAlso=old.service\n"):
            with self.subTest(directives=directives):
                fixture = DeploymentFixture()
                self.addCleanup(fixture.cleanup)
                path = fixture.candidate / "systemd" / UNIT_NAMES[0]
                path.chmod(0o644)
                fixture.write(path, (path.read_text().split("[Install]")[0] + "[Install]\n" + directives).encode(), 0o444)
                fixture.manifest["candidate_files"]["systemd/" + UNIT_NAMES[0]] = reference(path)
                fixture.freeze_manifest()
                subject = self.prepare_autostart(fixture)
                with self.assertRaisesRegex(ValueError, "multi-user.target"):
                    subject.autostart_check()
                self.assert_no_autostart_changes(fixture.commands)

    def test_autostart_enable_requires_exact_double_estop_on_both_sides(self):
        subject = self.prepare_autostart()
        for side in ("left", "right"):
            for name in ("estop_latch", "estop_latch.bak"):
                path = Path(self.fixture.protection[side][name]["path"])
                for value in (b"0\n", b"", b"1\r\n"):
                    with self.subTest(side=side, name=name, value=value):
                        path.write_bytes(value)
                        with self.assertRaisesRegex(ValueError, "双侧双 EStop"):
                            subject.enable_autostart(self.fixture.backup_dir)
                path.write_bytes(b"1\n")
        self.assert_no_autostart_changes(self.fixture.commands)

    def test_autostart_file_drift_blocks_enable_and_disable_with_failure_receipt(self):
        subject = self.prepare_autostart()
        self.damage_runtime_file(self.fixture.unit_directory / UNIT_NAMES[1])
        for operation in (subject.enable_autostart, subject.disable_autostart):
            with self.assertRaises(ValueError):
                operation(self.fixture.backup_dir)
        self.assert_no_autostart_changes(self.fixture.commands)
        for prefix in ("enable", "disable"):
            paths = list(self.fixture.backup_dir.glob(prefix + "-autostart-*-result.json"))
            self.assertEqual(len(paths), 1)
            self.assertEqual(json.loads(paths[0].read_text())["status"], "FAIL")

    def test_partial_enable_failure_is_preserved_and_explicit_disable_restores_all_three(self):
        subject = self.prepare_autostart()
        self.fixture.commands.autostart_partial = 1
        self.fixture.commands.autostart_returncode = 1
        with self.assertRaises(ValueError):
            subject.enable_autostart(self.fixture.backup_dir)
        self.assertEqual(self.fixture.commands.autostart, dict(zip(UNIT_NAMES, ("enabled", "disabled", "disabled"))))
        receipt_path = next(self.fixture.backup_dir.glob("enable-autostart-*-result.json"))
        original_failure = receipt_path.read_bytes()
        receipt = json.loads(original_failure)
        self.assertEqual(receipt["status"], "FAIL")
        self.assertTrue(receipt["protection_unchanged"])
        self.assertEqual(receipt["services_after_failure"][UNIT_NAMES[0]]["UnitFileState"], "enabled")
        self.fixture.commands.autostart_partial = None
        self.fixture.commands.autostart_returncode = 0
        self.fixture.commands.autostart_stderr = 'Removed "/etc/systemd/system/multi-user.target.wants/robot-s0-preflight.service".\n'
        self.assertEqual(subject.disable_autostart(self.fixture.backup_dir)["status"], "PASS")
        self.assertEqual(set(self.fixture.commands.autostart.values()), {"disabled"})
        self.assertEqual(receipt_path.read_bytes(), original_failure)
        self.assertEqual(sum(call[:2] == ["systemctl", "enable"] for call in self.fixture.commands.calls), 1)

    def test_identical_unit_replacement_still_invalidates_autostart_backup_identity(self):
        subject = self.prepare_autostart()
        target = self.fixture.unit_directory / UNIT_NAMES[1]
        before = reference(target)
        subject._atomic_replace(target, target.read_bytes(), before)
        self.assertEqual(reference(target)["sha256"], before["sha256"])
        self.assertNotEqual(reference(target)["inode"], before["inode"])
        with self.assertRaisesRegex(ValueError, "已备份候选"):
            subject.enable_autostart(self.fixture.backup_dir)
        self.assert_no_autostart_changes(self.fixture.commands)

    def test_enable_protection_change_is_sticky_and_explicit_disable_still_removes_autostart(self):
        subject = self.prepare_autostart()
        path = Path(self.fixture.protection["right"]["estop_latch"]["path"])
        commands = self.fixture.commands
        def change_protection(argv):
            result = commands(argv)
            if argv == ["systemctl", "enable", *UNIT_NAMES]:
                path.write_bytes(b"0\n")
            return result
        subject.runner = change_protection
        with self.assertRaisesRegex(ValueError, "双侧双 EStop"):
            subject.enable_autostart(self.fixture.backup_dir)
        receipt_path = next(self.fixture.backup_dir.glob("enable-autostart-*-result.json"))
        original_failure = receipt_path.read_bytes()
        failure = json.loads(original_failure)
        self.assertEqual(failure["status"], "FAIL")
        self.assertFalse(failure["protection_unchanged"])
        self.assertEqual(subject.disable_autostart(self.fixture.backup_dir)["status"], "PASS")
        self.assertEqual(receipt_path.read_bytes(), original_failure)
        self.assertEqual(path.read_bytes(), b"0\n")
        self.assertEqual(set(commands.autostart.values()), {"disabled"})

    def test_disable_remains_available_if_estop_was_changed_after_enable(self):
        subject = self.prepare_autostart()
        subject.enable_autostart(self.fixture.backup_dir)
        path = Path(self.fixture.protection["right"]["estop_latch"]["path"])
        path.write_bytes(b"0\n")
        self.assertEqual(subject.disable_autostart(self.fixture.backup_dir)["status"], "PASS")
        self.assertEqual(path.read_bytes(), b"0\n")
        self.assertEqual(set(self.fixture.commands.autostart.values()), {"disabled"})

    def test_device_reenumeration_blocks_enable_but_does_not_block_disable(self):
        subject = self.prepare_autostart()
        subject.enable_autostart(self.fixture.backup_dir)
        device = Path(self.fixture.devices["right"]["path"])
        replacement = device.with_name("reenumerated-right")
        replacement.symlink_to(os.readlink(device))
        os.replace(replacement, device)
        self.assertNotEqual(device.lstat().st_ino, self.fixture.devices["right"]["inode"])
        for operation in (subject.autostart_check, lambda: subject.enable_autostart(self.fixture.backup_dir)):
            with self.assertRaisesRegex(ValueError, "设备链接身份变化"):
                operation()
        self.fixture.commands.calls.clear()
        result = subject.disable_autostart(self.fixture.backup_dir)
        self.assertEqual(result["status"], "PASS")
        self.assertEqual(result["protection_before"], result["protection_after"])
        self.assertEqual(set(self.fixture.commands.autostart.values()), {"disabled"})
        self.assertFalse(any("fuser" in call for call in self.fixture.commands.calls))
        with self.assertRaisesRegex(ValueError, "设备链接身份变化"):
            subject.enable_autostart(self.fixture.backup_dir)

    def test_missing_runtime_binary_or_config_does_not_block_disabling_autostart(self):
        for relative in ("bin/robot-s0", "candidate/configs/xlerobot-bus2.toml"):
            with self.subTest(relative=relative):
                fixture = DeploymentFixture()
                self.addCleanup(fixture.cleanup)
                subject = self.prepare_autostart(fixture)
                subject.enable_autostart(fixture.backup_dir)
                (fixture.root / relative).unlink()
                for operation in (subject.autostart_check, lambda: subject.enable_autostart(fixture.backup_dir)):
                    with self.assertRaises(REJECTED):
                        operation()
                result = subject.disable_autostart(fixture.backup_dir)
                self.assertEqual(result["status"], "PASS")
                self.assertEqual(result["protection_before"], result["protection_after"])
                self.assertEqual(set(fixture.commands.autostart.values()), {"disabled"})

    def test_disable_accepts_stopped_failed_current_units_without_clearing_failure(self):
        subject = self.prepare_autostart()
        subject.enable_autostart(self.fixture.backup_dir)
        failed = {"ActiveState": "failed", "SubState": "failed", "MainPID": "0", "Job": "", "Result": "exit-code"}
        self.fixture.commands.overrides = {name: failed.copy() for name in UNIT_NAMES}
        for operation in (subject.check, subject.autostart_check,
                          lambda: subject.enable_autostart(self.fixture.backup_dir)):
            with self.assertRaisesRegex(ValueError, "未停稳"):
                operation()
        self.fixture.commands.calls.clear()
        result = subject.disable_autostart(self.fixture.backup_dir)
        self.assertEqual(result["status"], "PASS")
        self.assertEqual(result["protection_before"], result["protection_after"])
        for name in UNIT_NAMES:
            for phase in ("services_before", "services_after"):
                self.assertEqual({key: result[phase][name][key] for key in failed}, failed)
        self.assertEqual(self.fixture.commands.overrides, {name: failed for name in UNIT_NAMES})
        changes = [call for call in self.fixture.commands.calls if call[:2] != ["systemctl", "show"]]
        self.assertEqual(changes, [["systemctl", "disable", *UNIT_NAMES]])

    def test_disable_failed_exception_does_not_allow_other_units_live_pid_or_job(self):
        subject = self.prepare_autostart()
        subject.enable_autostart(self.fixture.backup_dir)
        failed = {"ActiveState": "failed", "SubState": "failed", "Result": "exit-code"}
        invalid = [(name, failed) for name in (*OLD_UNIT_NAMES, installer.BUSINESS_UNITS[0])]
        invalid += [(name, {**failed, **change}) for name in UNIT_NAMES for change in (
            {"MainPID": "123"}, {"Job": "123 /job/123"}, {"SubState": "auto-restart"},
        )]
        self.fixture.commands.calls.clear()
        for name, fields in invalid:
            with self.subTest(name=name, fields=fields):
                self.fixture.commands.overrides = {name: fields}
                with self.assertRaises(ValueError):
                    subject.disable_autostart(self.fixture.backup_dir)
        self.assert_no_autostart_changes(self.fixture.commands)

    def test_command_allowlist_accepts_only_exact_autostart_group_and_rejects_dangerous_commands(self):
        allowed = [["systemctl", operation, *UNIT_NAMES] for operation in ("enable", "disable")]
        rejected = [["systemctl", operation, *UNIT_NAMES] for operation in ("start", "stop", "restart", "reset-failed", "reboot")]
        rejected += [["systemctl", "enable", *UNIT_NAMES, "--now"], ["systemctl", "enable", "--runtime", *UNIT_NAMES],
                     ["systemctl", "enable", UNIT_NAMES[0]], ["systemctl", "disable", *OLD_UNIT_NAMES],
                     ["systemctl", "enable", *UNIT_NAMES, "other.service"], ["robot-deploy", "rx-trust", "authorize"]]
        with patch.object(installer.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "", "")) as run:
            for argv in allowed:
                installer.run_command(argv)
            self.assertEqual(run.call_count, 2)
            for argv in rejected:
                with self.subTest(argv=argv), self.assertRaisesRegex(ValueError, "禁止的命令"):
                    installer.run_command(argv)
            self.assertEqual(run.call_count, 2)

    def test_autostart_stderr_accepts_only_successful_symlink_diagnostics(self):
        for operation, message in (("enable", 'Created symlink "/fixture/link" → "/fixture/unit".\n'),
                                   ("disable", 'Removed "/fixture/link".\n')):
            with self.subTest(operation=operation):
                self.subject.runner = lambda argv: subprocess.CompletedProcess(argv, 0, "", message)
                self.subject._command(["systemctl", operation, *UNIT_NAMES])
                self.subject.runner = lambda argv: subprocess.CompletedProcess(argv, 1, "", message)
                with self.assertRaises(ValueError):
                    self.subject._command(["systemctl", operation, *UNIT_NAMES])
        for message in ("Failed to enable unit\n", "Warning: unexpected state\n", "Created symlink /fixture/link\nunknown\n"):
            self.subject.runner = lambda argv: subprocess.CompletedProcess(argv, 0, "", message)
            with self.assertRaises(ValueError):
                self.subject._command(["systemctl", "enable", *UNIT_NAMES])

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

    def test_check_installed_units_accepts_caller_with_different_uid_and_gid(self):
        self.subject.backup(self.fixture.backup_dir)
        self.subject.install(self.fixture.backup_dir)
        self.fixture.commands.calls.clear()
        before = {str(path): reference(path) for path in self.fixture.root.rglob("*") if path.is_file()}
        owner = self.fixture.unit_directory.stat()
        with patch.object(installer.os, "geteuid", return_value=owner.st_uid + 1), \
                patch.object(installer.os, "getegid", return_value=owner.st_gid + 1):
            result = self.subject.check()
        self.assertEqual(result["result"], "CHECK_PASS")
        self.assertEqual(before, {path: reference(Path(path)) for path in before})
        self.assert_no_reload()

    def test_check_installed_units_rejects_wrong_uid_or_gid(self):
        self.subject.backup(self.fixture.backup_dir)
        self.subject.install(self.fixture.backup_dir)
        self.fixture.commands.calls.clear()
        target = self.fixture.unit_directory / UNIT_NAMES[0]
        real_identity = installer.installed_file_identity
        for field in ("uid", "gid"):
            with self.subTest(field=field):
                def wrong_owner(path):
                    row = real_identity(path)
                    if Path(path) == target:
                        row[field] += 1
                    return row

                with patch.object(installer, "installed_file_identity", side_effect=wrong_owner):
                    with self.assertRaisesRegex(ValueError, "新 unit 路径已被非候选文件占用"):
                        self.subject.check()
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
