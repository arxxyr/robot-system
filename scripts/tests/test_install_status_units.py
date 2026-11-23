"""状态页部署的离线文件事务测试；假 ELF、假 systemd，不访问设备。"""

import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import stat
import struct
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
SPEC = importlib.util.spec_from_file_location("install_status_units", SCRIPTS / "install_status_units.py")
installer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(installer)


class FakeCommands:
    def __init__(self, directory):
        self.directory, self.calls, self.overrides = directory, [], {}
        self.reload_fails = False
        self.stale_cache = False

    def __call__(self, argv):
        self.calls.append(argv)
        if argv == ["systemctl", "daemon-reload"]:
            self.stale_cache = self.reload_fails
            return subprocess.CompletedProcess(argv, int(self.reload_fails), "", "失败" if self.reload_fails else "")
        if argv[:2] != ["systemctl", "show"] or argv[2] not in (*installer.UNITS, *installer.LEFT_UNITS, *installer.RIGHT_UNITS):
            raise AssertionError("禁止的外部命令：" + repr(argv))
        name = argv[2]
        exists = (self.directory / name).exists() and not self.stale_cache
        fields = {"Id": name, "LoadState": "loaded" if exists else "not-found", "ActiveState": "inactive",
                  "SubState": "dead", "Job": "", "UnitFileState": "disabled" if exists else "",
                  "FragmentPath": str(self.directory / name) if exists else "", "DropInPaths": ""}
        fields.update(self.overrides)
        return subprocess.CompletedProcess(argv, 0, "\n".join(key + "=" + fields[key] for key in installer.PROPERTIES), "")


class StatusInstallTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.bin_directory = self.root / "build"
        self.bin_directory.mkdir()
        elf = struct.pack("<16sHHIQQQIHHHHHH", b"\x7fELF\x02\x01\x01" + b"\0" * 9,
                          2, 183, 1, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0)
        for name in installer.BINARIES:
            (self.bin_directory / name).write_bytes(elf)
            (self.bin_directory / name).chmod(0o755)
        self.source = SCRIPTS.parent
        self.configs = []
        for bus in (1, 2):
            path = self.root / f"bus{bus}.toml"
            shutil.copyfile(self.source / f"configs/xlerobot-bus{bus}.toml", path)
            self.configs.append(path)
        self.candidate = self.root / "candidate"
        result = installer.prepare(self.candidate, self.bin_directory, self.configs, "a" * 40, "10.0.0.2:8080", self.source)
        self.digest = result["manifest_sha256"]
        self.units = self.root / "units"
        self.units.mkdir()
        self.commands = FakeCommands(self.units)
        self.subject = installer.StatusUnitInstaller(self.candidate, self.digest, unit_directory=self.units, runner=self.commands)
        self.backup = self.root / "backup"

    def test_prepare_binds_only_current_configs_and_has_no_service_side_effects(self):
        manifest = installer.validate_candidate(self.candidate, self.digest)
        self.assertEqual(set(manifest["files"]), set(installer.FILES))
        self.assertEqual(len(manifest["files"]), 7)
        status = installer.tomllib.loads((self.candidate / "configs/xlerobot-status.toml").read_text())
        self.assertEqual(status["listen"], "10.0.0.2:8080")
        self.assertEqual([row["config"] for row in status["sources"]], list(map(str, self.configs)))
        roles = installer.tomllib.loads((self.candidate / "configs/rpc-roles-status.toml").read_text())
        self.assertEqual(roles["tokens"], {})
        for name in installer.UNITS:
            text = (self.candidate / "systemd" / name).read_text()
            self.assertIn("WorkingDirectory=" + str(self.candidate), text)
            self.assertNotIn("target/release", text)
            self.assertNotIn("\nRequires=", text)
            self.assertNotIn("\nWants=", text)
        self.assertEqual(self.commands.calls, [])

    def test_check_only_queries_three_units_and_pure_validation_works_while_running(self):
        before = list(self.units.iterdir())
        self.assertEqual(self.subject.check()["result"], "CHECK_PASS")
        self.assertEqual(list(self.units.iterdir()), before)
        self.assertEqual({call[2] for call in self.commands.calls}, set(installer.UNITS))
        self.commands.overrides = {"ActiveState": "active", "SubState": "running"}
        with self.assertRaises(ValueError):
            self.subject.check()
        self.assertEqual(installer.validate_candidate(self.candidate, self.digest)["runtime_commit"], "a" * 40)

    def test_cargo_hardlinked_source_is_copied_but_candidate_hardlinks_are_rejected(self):
        os.link(self.bin_directory / "robot-gateway", self.root / "cargo-deps-gateway")
        self.assertEqual((self.bin_directory / "robot-gateway").stat().st_nlink, 2)
        candidate = self.root / "hardlink-source-candidate"
        result = installer.prepare(candidate, self.bin_directory, self.configs, "a" * 40, "10.0.0.2:8080", self.source)
        installer.validate_candidate(candidate, result["manifest_sha256"])
        self.assertEqual((candidate / "bin/robot-gateway").stat().st_nlink, 1)
        os.link(candidate / "bin/robot-gateway", self.root / "forbidden-candidate-link")
        with self.assertRaises(ValueError):
            installer.validate_candidate(candidate, result["manifest_sha256"])

    def test_install_and_rollback_restore_original_content_permissions_and_absence(self):
        first = self.units / installer.UNITS[0]
        first.write_bytes("原先的配置\n".encode())
        first.chmod(0o640)
        before = installer.file_identity(first)
        self.subject.backup(self.backup)
        self.assertEqual(self.subject.apply(self.backup)["status"], "PASS")
        for name in installer.UNITS:
            self.assertEqual((self.units / name).read_bytes(), (self.candidate / "systemd" / name).read_bytes())
            self.assertEqual(stat.S_IMODE((self.units / name).stat().st_mode), 0o644)
        self.assertEqual(self.subject.apply(self.backup, rollback=True)["status"], "PASS")
        self.assertTrue(installer.same_content(installer.file_identity(first), before))
        self.assertFalse((self.units / installer.UNITS[1]).exists())
        self.assertFalse((self.units / installer.UNITS[2]).exists())

    def test_partial_replace_failure_is_preserved_and_rollbackable(self):
        self.subject.backup(self.backup)
        replace = installer.atomic_replace
        def fail_second(path, data, expected):
            if path == self.units / installer.UNITS[1]:
                raise OSError("第二个文件写入失败")
            return replace(path, data, expected)
        with patch.object(installer, "atomic_replace", side_effect=fail_second), self.assertRaises(OSError):
            self.subject.apply(self.backup)
        receipt = json.loads((self.backup / "install-result.json").read_text())
        self.assertEqual(receipt["status"], "FAIL")
        self.assertEqual(receipt["completed"], [installer.UNITS[0]])
        self.assertEqual(self.subject.apply(self.backup, rollback=True)["status"], "PASS")
        self.assertEqual(list(self.units.iterdir()), [])
        self.assertEqual(json.loads((self.backup / "install-result.json").read_text()), receipt)

    def test_reload_failure_and_missing_candidate_binary_do_not_block_rollback(self):
        self.subject.backup(self.backup)
        self.commands.reload_fails = True
        with self.assertRaises(ValueError):
            self.subject.apply(self.backup)
        self.commands.reload_fails = False
        (self.candidate / "bin/robot-status").unlink()
        self.assertEqual(self.subject.apply(self.backup, rollback=True)["status"], "PASS")
        self.assertEqual(list(self.units.iterdir()), [])

    def test_completed_install_is_not_replayed_after_rollback(self):
        self.subject.backup(self.backup)
        self.subject.apply(self.backup)
        self.subject.apply(self.backup, rollback=True)
        with self.assertRaises(FileExistsError):
            self.subject.apply(self.backup)
        self.assertEqual(list(self.units.iterdir()), [])

    def test_unknown_target_drift_after_backup_blocks_install_and_rollback(self):
        self.subject.backup(self.backup)
        target = self.units / installer.UNITS[0]
        target.write_text("外部修改")
        for rollback in (False, True):
            with self.subTest(rollback=rollback), self.assertRaises(ValueError):
                self.subject.apply(self.backup, rollback)
        self.assertEqual(target.read_text(), "外部修改")
        self.assertNotIn(["systemctl", "daemon-reload"], self.commands.calls)

    def test_external_config_drift_is_rejected(self):
        self.configs[0].write_text(self.configs[0].read_text() + "\n# 变化\n")
        with self.assertRaises(ValueError):
            self.subject.check()

    def test_candidate_permission_drift_is_rejected(self):
        binary = self.candidate / "bin/robot-gateway"
        binary.chmod(0o755)
        with self.assertRaises(ValueError):
            installer.validate_candidate(self.candidate, self.digest)

    def test_corrupt_backup_and_changed_manifest_are_rejected(self):
        self.subject.backup(self.backup)
        (self.backup / "backup.sha256").chmod(0o600)
        (self.backup / "backup.sha256").write_text("0" * 64 + "\n")
        with self.assertRaises(ValueError):
            self.subject.apply(self.backup)
        with self.assertRaises(ValueError):
            installer.validate_candidate(self.candidate, "0" * 64)

    def test_running_jobs_dropins_and_unexpected_fragments_are_rejected(self):
        for change in ({"ActiveState": "active", "SubState": "running"}, {"Job": "9 /job"}, {"ActiveState": "activating"},
                       {"DropInPaths": "/tmp/override.conf"}, {"UnitFileState": "enabled"},
                       {"FragmentPath": "/usr/lib/systemd/system/other.service"}):
            with self.subTest(change=change):
                self.commands.overrides = change
                with self.assertRaises(ValueError):
                    self.subject.check()

    def test_symlink_and_hardlink_targets_are_rejected(self):
        other = self.root / "other"
        other.write_text("保留")
        target = self.units / installer.UNITS[0]
        target.symlink_to(other)
        with self.assertRaises(ValueError):
            self.subject.check()
        target.unlink()
        os.link(other, target)
        with self.assertRaises(ValueError):
            self.subject.check()

    def test_listener_rejects_public_unspecified_and_invalid_values(self):
        for value in ("0.0.0.0:8080", "8.8.8.8:8080", "10.0.0.2:0", "10.0.0.2:65536", "localhost:8080"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                installer.validate_listen(value)

    def test_command_allowlist_rejects_control_and_unrelated_services(self):
        with self.assertRaises(ValueError):
            self.subject.target("robot-s0-bus1.service")
        for argv in (["systemctl", "start", installer.UNITS[0]], ["systemctl", "stop", installer.UNITS[2]],
                     ["systemctl", "enable", installer.UNITS[1]], ["systemctl", "show", "robot-s0-bus1.service"],
                     ["fuser", "/dev/ttyUSB0"]):
            with self.subTest(argv=argv), self.assertRaises(ValueError):
                installer.run_command(argv)

    def left_control_candidate(self):
        shutil.copyfile(self.bin_directory / "robot-gateway", self.bin_directory / "robot-s1")
        (self.bin_directory / "robot-s1").chmod(0o755)
        candidate = self.root / "left-control"
        result = installer.prepare(candidate, self.bin_directory, self.configs, "b" * 40,
                                   "10.0.0.2:8080", self.source, left_control=True)
        subject = installer.StatusUnitInstaller(candidate, result["manifest_sha256"],
                                                unit_directory=self.units, runner=self.commands)
        return candidate, result["manifest_sha256"], subject

    def test_left_control_reuses_bus1_gateway_and_only_changes_three_units(self):
        candidate, digest, subject = self.left_control_candidate()
        manifest = installer.validate_candidate(candidate, digest)
        self.assertEqual(set(manifest["files"]), set(installer.LEFT_FILES))
        status = installer.tomllib.loads((candidate / "configs/xlerobot-status.toml").read_text())
        self.assertEqual(status["control"], {"enabled": True})
        self.assertEqual(status["sources"][0]["endpoint"], "http://127.0.0.1:50061")
        self.assertEqual(status["sources"][0]["control"]["motion_config"], "xlerobot-bus1-motion.toml")
        self.assertNotIn("control", status["sources"][1])
        self.assertEqual(status["sources"][1]["endpoint"], "http://127.0.0.1:50062")
        self.assertEqual(subject.check()["result"], "CHECK_PASS")
        self.assertEqual({call[2] for call in self.commands.calls}, set(installer.LEFT_UNITS))
        self.assertNotIn("robot-gateway-bus2.service", subject.units)
        s1 = (candidate / "systemd/robot-s1-bus1.service").read_text()
        self.assertIn("--config " + str(candidate / "configs/xlerobot-bus1-s1.toml"), s1)
        self.assertIn("Requires=robot-s0-bus1.service", s1)
        self.assertIn("CPUAffinity=0-3", s1)
        self.assertIn("Restart=no", s1)
        self.assertNotIn("target/release", s1)
        for unit in ("robot-gateway-bus1.service", "robot-status.service"):
            self.assertNotIn("\nRequires=", (candidate / "systemd" / unit).read_text())

    def test_left_control_preserves_device_config_and_reuses_motion_constraints(self):
        original = installer.tomllib.loads(self.configs[0].read_text())
        candidate, _, _ = self.left_control_candidate()
        composed = installer.tomllib.loads((candidate / "configs/xlerobot-bus1-s1.toml").read_text())
        motion = installer.tomllib.loads((candidate / "configs/xlerobot-bus1-motion.toml").read_text())
        self.assertEqual(composed.pop("s1"), {**original.pop("s1"), **motion["s1"]})
        self.assertEqual(composed, original)
        self.assertEqual((candidate / "configs/xlerobot-bus1-motion.toml").read_bytes(),
                         (self.source / "configs/xlerobot-bus1-motion.toml").read_bytes())
        self.assertNotIn("single_joint", installer.tomllib.loads(self.configs[0].read_text())["s1"])

    def test_left_control_adds_current_profile_to_legacy_s0_config(self):
        text = self.configs[0].read_text()
        begin, end = text.index("[s1.profile]"), text.index("[[sensor]]")
        self.configs[0].write_text(text[:begin] + text[end:])
        before = self.configs[0].read_bytes()
        original = installer.tomllib.loads(before.decode())
        self.assertNotIn("s1", original)
        candidate, _, _ = self.left_control_candidate()
        composed = installer.tomllib.loads((candidate / "configs/xlerobot-bus1-s1.toml").read_text())
        current = installer.tomllib.loads((self.source / "configs/xlerobot-bus1.toml").read_text())
        motion = installer.tomllib.loads((self.source / "configs/xlerobot-bus1-motion.toml").read_text())
        self.assertEqual(composed.pop("s1"), {"profile": current["s1"]["profile"], **motion["s1"]})
        self.assertEqual(composed, original)
        self.assertEqual(self.configs[0].read_bytes(), before)

    def test_left_control_replaces_old_profile_but_preserves_other_s1_and_s0_fields(self):
        text = self.configs[0].read_text().replace("[s1.profile]", "[s1]\ncommand_hz = 50\n\n[s1.profile]")
        self.configs[0].write_text(text.replace('backends = ["scripted"]', 'backends = ["mock_dwell"]'))
        before = self.configs[0].read_bytes()
        original = installer.tomllib.loads(before.decode())
        self.assertEqual(original["s1"]["profile"]["backends"], ["mock_dwell"])
        candidate, _, _ = self.left_control_candidate()
        composed = installer.tomllib.loads((candidate / "configs/xlerobot-bus1-s1.toml").read_text())
        current = installer.tomllib.loads((self.source / "configs/xlerobot-bus1.toml").read_text())
        motion = installer.tomllib.loads((self.source / "configs/xlerobot-bus1-motion.toml").read_text())
        self.assertEqual(composed.pop("s1"), {**original.pop("s1"), "profile": current["s1"]["profile"], **motion["s1"]})
        self.assertEqual(composed, original)
        self.assertEqual(self.configs[0].read_bytes(), before)

    def test_left_control_rejects_missing_current_profile_before_writing_candidate(self):
        original = Path.read_text
        source_config = self.source / "configs/xlerobot-bus1.toml"
        def without_profile(path, *args, **kwargs):
            return "[s1]\n" if path == source_config else original(path, *args, **kwargs)
        with patch.object(Path, "read_text", without_profile), self.assertRaisesRegex(ValueError, "完整S1 profile"):
            self.left_control_candidate()
        self.assertFalse((self.root / "left-control").exists())

    def test_left_control_credentials_are_private_and_bound_to_same_token(self):
        candidate, digest, _ = self.left_control_candidate()
        roles_path = candidate / "configs/rpc-roles-web-control.toml"
        env_path = candidate / "configs/web-control.env"
        roles = installer.tomllib.loads(roles_path.read_text())
        self.assertEqual(len(roles["tokens"]), 1)
        token, role = next(iter(roles["tokens"].items()))
        self.assertEqual(role, "maintainer")
        self.assertEqual(roles["roles"][role]["capabilities"], list(installer.CONTROL_CAPABILITIES))
        self.assertEqual(env_path.read_text(), "ROBOT_TOKEN=" + token + "\n")
        for path in (roles_path, env_path):
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        for name in ("manifest.json", "configs/xlerobot-status.toml", "systemd/robot-status.service"):
            self.assertNotIn(token, (candidate / name).read_text())
        env_path.chmod(0o644)
        with self.assertRaises(ValueError):
            installer.validate_candidate(candidate, digest)

    def test_left_control_install_and_rollback_leave_right_gateway_untouched(self):
        right = self.units / "robot-gateway-bus2.service"
        right.write_text("保留右侧服务\n")
        right_before = installer.file_identity(right)
        web = self.units / "robot-status.service"
        web.write_text("原只读Web\n")
        web.chmod(0o640)
        web_before = installer.file_identity(web)
        candidate, _, subject = self.left_control_candidate()
        subject.backup(self.backup)
        self.assertEqual(subject.apply(self.backup)["status"], "PASS")
        self.assertEqual(installer.file_identity(right), right_before)
        for name in installer.LEFT_UNITS:
            self.assertEqual((self.units / name).read_bytes(), (candidate / "systemd" / name).read_bytes())
        self.assertEqual(subject.apply(self.backup, rollback=True)["status"], "PASS")
        self.assertEqual(installer.file_identity(right), right_before)
        self.assertTrue(installer.same_content(installer.file_identity(web), web_before))
        self.assertFalse((self.units / "robot-s1-bus1.service").exists())
        self.assertFalse((self.units / "robot-gateway-bus1.service").exists())

    def test_left_control_partial_install_rolls_back_without_candidate_runtime(self):
        candidate, _, subject = self.left_control_candidate()
        subject.backup(self.backup)
        replace = installer.atomic_replace
        def fail_web(path, data, expected):
            if path == self.units / "robot-status.service":
                raise OSError("Web替换失败")
            return replace(path, data, expected)
        with patch.object(installer, "atomic_replace", side_effect=fail_web), self.assertRaises(OSError):
            subject.apply(self.backup)
        receipt = json.loads((self.backup / "install-result.json").read_text())
        self.assertEqual(receipt["status"], "FAIL")
        self.assertEqual(receipt["completed"], list(installer.LEFT_UNITS[:2]))
        (candidate / "bin/robot-s1").unlink()
        (candidate / "configs/web-control.env").unlink()
        self.assertEqual(subject.apply(self.backup, rollback=True)["status"], "PASS")
        self.assertEqual(list(self.units.iterdir()), [])
        self.assertEqual(json.loads((self.backup / "install-result.json").read_text()), receipt)

    def test_left_control_rejects_prepare_as_another_user_before_writing_secrets(self):
        with patch.object(installer.pwd, "getpwnam") as account:
            account.return_value.pw_uid = os.geteuid() + 1
            with self.assertRaisesRegex(ValueError, "服务用户"):
                self.left_control_candidate()
        self.assertFalse((self.root / "left-control").exists())

    def right_control_candidate(self, existing=None):
        if existing is None:
            existing, _, _ = self.left_control_candidate()
        candidate = self.root / "right-control"
        # 右侧扩展须复用已有令牌，禁止额外生成。
        with patch.object(installer.secrets, "token_hex", side_effect=AssertionError("禁止轮换左侧令牌")):
            result = installer.prepare(candidate, self.bin_directory, self.configs, "c" * 40,
                                       "127.0.0.1:8080", self.source, right_control=True,
                                       existing_control_candidate=existing)
        subject = installer.StatusUnitInstaller(candidate, result["manifest_sha256"],
                                                unit_directory=self.units, runner=self.commands)
        return candidate, result["manifest_sha256"], subject, existing

    def test_control_modes_are_exclusive_and_right_requires_existing_left_candidate(self):
        with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            installer.main(["prepare", "--candidate", str(self.root / "invalid"), "--left-control", "--right-control"])
        for options in ({"left_control": True, "right_control": True}, {"right_control": True},
                        {"right_control": True, "existing_control_candidate": self.candidate}):
            with self.subTest(options=options), self.assertRaises(ValueError):
                installer.prepare(self.root / "invalid", self.bin_directory, self.configs, "c" * 40,
                                  "127.0.0.1:8080", self.source, **options)
            self.assertFalse((self.root / "invalid").exists())

    def test_right_preserves_left_credentials_motion_and_web_parameters(self):
        original_read = Path.read_text
        def custom_preview(path, *args, **kwargs):
            text = original_read(path, *args, **kwargs)
            return text.replace("interval_ms = 1000", "interval_ms = 2000") if path == self.source / "configs/xlerobot-status.toml" else text
        with patch.object(Path, "read_text", custom_preview):
            existing, _, _ = self.left_control_candidate()
        candidate, digest, _, _ = self.right_control_candidate(existing)
        manifest = installer.validate_candidate(candidate, digest)
        self.assertEqual(set(manifest["files"]), set(installer.RIGHT_FILES))
        self.assertEqual(manifest["existing_control_manifest"], installer.file_identity(existing / "manifest.json"))
        before = installer.tomllib.loads((existing / "configs/xlerobot-status.toml").read_text())
        after = installer.tomllib.loads((candidate / "configs/xlerobot-status.toml").read_text())
        self.assertEqual(after["listen"], "10.0.0.2:8080")
        self.assertEqual(after["preview"]["interval_ms"], 2000)
        self.assertEqual(after["sources"][1].pop("control"),
                         {"motion_config": "xlerobot-bus2-motion.toml", "enable_alignment_tolerance_rad": 0.0061365})
        self.assertEqual(after, before)
        for name in ("rpc-roles-web-control.toml", "web-control.env", "xlerobot-bus1-motion.toml"):
            self.assertEqual((candidate / "configs" / name).read_bytes(), (existing / "configs" / name).read_bytes())
        for name in ("rpc-roles-web-control.toml", "web-control.env"):
            self.assertEqual(stat.S_IMODE((candidate / "configs" / name).stat().st_mode), 0o600)

    def test_right_composes_current_profile_for_legacy_bus_without_changing_hardware(self):
        text = self.configs[1].read_text()
        begin, end = text.index("[s1.profile]"), text.index("[[sensor]]")
        self.configs[1].write_text(text[:begin] + text[end:])
        original_bytes = self.configs[1].read_bytes()
        original = installer.tomllib.loads(original_bytes.decode())
        candidate, _, _, _ = self.right_control_candidate()
        combined = installer.tomllib.loads((candidate / "configs/xlerobot-bus2-s1.toml").read_text())
        s1 = combined.pop("s1")
        self.assertEqual(s1["profile"], {"backends": ["scripted", "ik"], "command_mode": "joint_position"})
        self.assertEqual(combined, original)
        self.assertTrue(combined["hardware"][0]["feetech_bus2_mixed_raw"])
        self.assertEqual(len(s1["single_joint"]["joints"]), 6)
        self.assertEqual(self.configs[1].read_bytes(), original_bytes)
        template = (candidate / "systemd/robot-s1-bus2.service").read_text()
        for expected in ("Requires=robot-s0-bus2.service", "Restart=no", "CPUAffinity=0-3",
                         "--hardware-id right_arm_full_with_base", "--config " + str(candidate / "configs/xlerobot-bus2-s1.toml")):
            self.assertIn(expected, template)

    def test_right_filters_wheel_slots_by_motor_id_not_array_position(self):
        text = self.configs[1].read_text()
        hardware = installer.tomllib.loads(text)["hardware"][0]
        order = [6, 0, 1, 2, 7, 3, 4, 8, 5]
        text = installer.re.sub(r"(?ms)^joints\s*=\s*\[.*?\]", "joints = " + json.dumps([hardware["joints"][i] for i in order]), text, count=1)
        text = installer.re.sub(r"(?m)^motor_ids\s*=.*$", "motor_ids = " + json.dumps([hardware["motor_ids"][i] for i in order]), text)
        text = installer.re.sub(r"(?m)^calibration_range_tolerances_raw\s*=.*$", "calibration_range_tolerances_raw = "
                                + json.dumps([hardware["calibration_range_tolerances_raw"][i] for i in order]), text)
        self.configs[1].write_text(text)
        candidate, _, _, _ = self.right_control_candidate()
        combined = installer.tomllib.loads((candidate / "configs/xlerobot-bus2-s1.toml").read_text())
        self.assertEqual(combined["hardware"], installer.tomllib.loads(text)["hardware"])
        self.assertEqual([joint["name"] for joint in combined["s1"]["single_joint"]["joints"]], hardware["joints"][:6])

    def test_right_rejects_wheel_position_constraint(self):
        existing, _, _ = self.left_control_candidate()
        original_read = Path.read_text
        def wheel_motion(path, *args, **kwargs):
            text = original_read(path, *args, **kwargs)
            return text.replace("right_arm_wrist_roll", "base_left_wheel") if path == self.source / "configs/xlerobot-bus2-motion.toml" else text
        with patch.object(Path, "read_text", wheel_motion), self.assertRaisesRegex(ValueError, "位置关节顺序"):
            self.right_control_candidate(existing)
        self.assertFalse((self.root / "right-control").exists())

    def test_right_install_and_rollback_only_touch_right_three_units(self):
        candidate, _, subject, existing = self.right_control_candidate()
        preserved = {}
        for name in ("robot-s1-bus1.service", "robot-gateway-bus1.service"):
            path = self.units / name
            shutil.copyfile(existing / "systemd" / name, path)
            path.chmod(0o640)
            preserved[name] = installer.file_identity(path)
        web = self.units / "robot-status.service"
        shutil.copyfile(existing / "systemd/robot-status.service", web)
        web.chmod(0o640)
        web_before = installer.file_identity(web)
        self.assertEqual(subject.check()["result"], "CHECK_PASS")
        subject.backup(self.backup)
        self.assertEqual(subject.apply(self.backup)["status"], "PASS")
        self.assertEqual(set(subject.units), set(installer.RIGHT_UNITS))
        for name in installer.RIGHT_UNITS:
            self.assertEqual((self.units / name).read_bytes(), (candidate / "systemd" / name).read_bytes())
        self.assertEqual(subject.apply(self.backup, rollback=True)["status"], "PASS")
        self.assertTrue(installer.same_content(installer.file_identity(web), web_before))
        for name, before in preserved.items():
            self.assertEqual(installer.file_identity(self.units / name), before)
        self.assertFalse((self.units / "robot-s1-bus2.service").exists())
        self.assertFalse((self.units / "robot-gateway-bus2.service").exists())
        self.assertEqual({call[2] for call in self.commands.calls if call[:2] == ["systemctl", "show"]}, set(installer.RIGHT_UNITS))

    def test_right_validation_rejects_changed_existing_left_candidate(self):
        candidate, digest, _, existing = self.right_control_candidate()
        (existing / "configs/web-control.env").write_text("ROBOT_TOKEN=changed\n")
        with self.assertRaises(ValueError):
            installer.validate_candidate(candidate, digest)


if __name__ == "__main__":
    unittest.main()
