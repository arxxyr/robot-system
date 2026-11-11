"""矩阵驱动打包的离线测试；全部 Git、rustc、Cargo 调用均由夹具代替。"""

import contextlib
import copy
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import stat
import struct
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import unittest
from unittest.mock import patch


REPOSITORY = Path(__file__).resolve().parents[2]
SCRIPTS = REPOSITORY / "scripts"
sys.path.insert(0, str(SCRIPTS))
SPEC = importlib.util.spec_from_file_location("build_package", SCRIPTS / "build_package.py")
builder = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(builder)


PACKAGE_ID = "xlerobot-orin-s0"
COMMIT = "1234567890abcdef1234567890abcdef12345678"
RELEASE_TAG = "v0.1.2+1234567"
EXPECTED_ARGV = [
    "cargo", "build", "--locked", "--release", "--message-format=json", "-p", "robot-s0",
    "--bin", "robot-s0", "--bin", "robot-deploy", "--features", "xlerobot-real,rt_guard",
]
PAYLOAD_NAMES = {
    "bin/robot-s0", "bin/robot-deploy",
    "configs/xlerobot-bus1.toml", "configs/xlerobot-bus2.toml",
    "systemd/robot-s0-preflight.service", "systemd/robot-s0-bus1.service", "systemd/robot-s0-bus2.service",
    "scripts/camera_capture.py", "VERSION", "README.md",
}
REJECTED = (ValueError, OSError)


def elf_bytes(machine=183):
    return struct.pack(
        "<16sHHIQQQIHHHHHH", b"\x7fELF\x02\x01\x01" + b"\0" * 9,
        3, machine, 1, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0,
    )


class FakeBuildCommands:
    def __init__(self, repository, target):
        self.repository = repository
        self.target = target
        self.calls = []
        self.commit = COMMIT
        self.status = ""
        self.rustc_release = "1.102.0-nightly"
        self.rustc_host = "aarch64-unknown-linux-gnu"
        self.rustc_commit = "a" * 40
        self.cargo_returncode = 0
        self.mutate_records = None
        self.expected_argv = EXPECTED_ARGV.copy()
        self.elf_machine = 183
        self.after_cargo = None

    def cargo_package(self, name):
        workspace = tomllib.loads((self.repository / "Cargo.toml").read_text())
        for member in workspace["workspace"]["members"]:
            crate = self.repository / member
            manifest = tomllib.loads((crate / "Cargo.toml").read_text())
            if manifest["package"]["name"] == name:
                return crate, manifest
        raise AssertionError("假 Cargo 找不到选择的 workspace package：" + name)

    @staticmethod
    def local_features(manifest, argv):
        table = manifest.get("features", {})
        pending = ["default"] if "--no-default-features" not in argv and "default" in table else []
        for index, arg in enumerate(argv):
            if arg == "--features":
                pending.extend(argv[index + 1].split(","))
        selected = set()
        while pending:
            name = pending.pop()
            if name in selected or name.startswith("dep:") or "/" in name:
                continue
            if name not in table:
                raise AssertionError("假 Cargo 收到未定义的本地 feature：" + name)
            selected.add(name)
            pending.extend(table[name])
        return sorted(selected)

    def __call__(self, argv, *, cwd, env):
        argv = [str(arg) for arg in argv]
        self.calls.append({"argv": argv, "cwd": Path(cwd), "env": dict(env)})
        if Path(cwd) != self.repository:
            raise AssertionError("构建命令的工作目录必须是夹具仓库")
        executable = Path(argv[0]).name
        if executable == "git":
            if "rev-parse" in argv:
                return subprocess.CompletedProcess(argv, 0, self.commit + "\n", "")
            if "status" in argv:
                return subprocess.CompletedProcess(argv, 0, self.status, "")
            raise AssertionError("未约定的 Git 命令：" + repr(argv))
        if executable == "rustc":
            if argv[1:] != ["-vV"]:
                raise AssertionError("工具链只允许 rustc -vV：" + repr(argv))
            output = (
                f"rustc {self.rustc_release} (aaaaaaaaa 2026-11-10)\n"
                "binary: rustc\n"
                f"commit-hash: {self.rustc_commit}\n"
                "commit-date: 2026-11-10\n"
                f"host: {self.rustc_host}\n"
                f"release: {self.rustc_release}\n"
                "LLVM version: 22.1.0\n"
            )
            return subprocess.CompletedProcess(argv, 0, output, "")
        if executable != "cargo":
            raise AssertionError("离线打包测试拒绝其他命令：" + repr(argv))
        if argv != self.expected_argv:
            raise AssertionError("Cargo 参数与本例矩阵期望不符：" + repr(argv))
        if Path(env.get("CARGO_TARGET_DIR", "")) != self.target:
            raise AssertionError("Cargo 必须使用本轮隔离目录")
        records = []
        package_name = argv[argv.index("-p") + 1]
        crate, manifest = self.cargo_package(package_name)
        features = self.local_features(manifest, argv)
        binaries = [argv[index + 1] for index, arg in enumerate(argv) if arg == "--bin"]
        for name in binaries:
            executable_path = self.target / "release" / name
            executable_path.parent.mkdir(parents=True, exist_ok=True)
            executable_path.write_bytes(elf_bytes(self.elf_machine))
            executable_path.chmod(0o755)
            records.append({
                "reason": "compiler-artifact",
                "package_id": "path+file://" + str(crate) + "#" + package_name + "@0.1.2",
                "target": {"name": name, "kind": ["bin"]},
                "executable": str(executable_path),
                "filenames": [str(executable_path)],
                "features": features.copy(),
                "profile": {"opt_level": "3", "test": False, "debug_assertions": False,
                            "overflow_checks": False, "debuginfo": 0},
                "fresh": False,
            })
        records.append({"reason": "build-finished", "success": True})
        if self.mutate_records is not None:
            self.mutate_records(records)
        if self.after_cargo is not None:
            self.after_cargo(self)
        return subprocess.CompletedProcess(argv, self.cargo_returncode, "\n".join(json.dumps(row) for row in records) + "\n", "")


class PackageFixture:
    def __init__(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="robot-package-test-")
        self.root = Path(self.temporary.name).resolve()
        self.repository = self.root / "repository"
        self.target = self.root / "isolated-target"
        self.output = self.root / "output"
        self.repository.mkdir()
        self.matrix = tomllib.loads((REPOSITORY / "configs" / "package-matrix.toml").read_text())
        self.write_matrix()
        self.write("Cargo.toml", '[workspace]\nmembers = ["crates/robot-s0"]\n\n[workspace.package]\nversion = "0.1.2"\n')
        self.write("Cargo.lock", "version = 4\n")
        self.write("rust-toolchain.toml", '[toolchain]\nchannel = "nightly"\n')
        self.write("crates/robot-s0/Cargo.toml", (REPOSITORY / "crates/robot-s0/Cargo.toml").read_text())
        self.write("crates/robot-s0/src/bin/robot-s0.rs", "fn main() {}\n")
        self.write("crates/robot-s0/src/bin/robot-deploy.rs", "fn main() {}\n")
        for row in self.matrix["packages"][PACKAGE_ID]["files"]:
            source = row["source"]
            self.write(source, (REPOSITORY / source).read_bytes())
        self.write("configs/xlerobot-left-arm.toml", (REPOSITORY / "configs/xlerobot-left-arm.toml").read_bytes())
        self.frozen_release = self.repository / "target" / "release" / "robot-s0"
        self.write("target/release/robot-s0", "正式产物占位不可覆盖\n")
        self.commands = FakeBuildCommands(self.repository, self.target)

    def write(self, relative, content):
        path = self.repository / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content.encode() if isinstance(content, str) else content)
        return path

    def write_matrix(self):
        lines = []
        for package_id, entry in self.matrix["packages"].items():
            lines.append("[packages." + package_id + "]")
            for name in ("target", "cargo_package"):
                lines.append(name + " = " + json.dumps(entry[name]))
            lines.append("default_features = " + str(entry["default_features"]).lower())
            for name in ("features", "bins"):
                lines.append(name + " = " + json.dumps(entry[name]))
            lines.append("files = [")
            for row in entry["files"]:
                lines.append("  { source = " + json.dumps(row["source"]) + ", destination = " + json.dumps(row["destination"]) + " },")
            lines.extend(["]", ""])
        self.write("configs/package-matrix.toml", "\n".join(lines))

    def build(self, package_id=PACKAGE_ID, **kwargs):
        return builder.build_package(
            self.repository, package_id, self.target, self.output,
            runner=self.commands, **kwargs,
        )

    def cleanup(self):
        self.temporary.cleanup()


class PackageBuilderTests(unittest.TestCase):
    def setUp(self):
        self.fixture = PackageFixture()
        self.addCleanup(self.fixture.cleanup)

    def assert_no_cargo(self):
        self.assertFalse(any(Path(call["argv"][0]).name == "cargo" for call in self.fixture.commands.calls))

    def assert_no_archive(self):
        self.assertEqual(list(self.fixture.output.glob("*.tar.gz")), [])

    def assert_matrix_rejected(self, mutate):
        mutate(self.fixture.matrix["packages"][PACKAGE_ID])
        self.fixture.write_matrix()
        with self.assertRaises(REJECTED):
            builder.select_package(self.fixture.repository, PACKAGE_ID)
        self.assert_no_cargo()

    def assert_archive_names(self, result, names):
        with tarfile.open(result["archive"], "r:gz") as package:
            self.assertEqual(set(package.getnames()), set(names) | {"manifest.json"})
        self.assertEqual(set(result["manifest"]["files"]), set(names))

    def external_formal_binaries(self):
        formal = self.fixture.root / "formal-installation"
        for name in ("robot-s0", "robot-deploy"):
            path = formal / "target" / "release" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(elf_bytes())
            path.chmod(0o755)
        for row in self.fixture.matrix["packages"][PACKAGE_ID]["files"]:
            if not row["source"].endswith(".service"):
                continue
            path = self.fixture.repository / row["source"]
            text = re.sub(r"(?m)^ExecStart=\S+", "ExecStart=" + str(formal / "target" / "release" / "robot-deploy"), path.read_text())
            text = re.sub(r"--s0-bin\s+\S+", "--s0-bin " + str(formal / "target" / "release" / "robot-s0"), text)
            path.write_text(text)
        return formal

    def test_real_matrix_selects_existing_s0_package(self):
        entries = builder.load_matrix(REPOSITORY)
        self.assertEqual(set(entries), {PACKAGE_ID})
        entry = builder.select_package(self.fixture.repository, PACKAGE_ID)
        self.assertEqual(entry["id"], PACKAGE_ID)
        self.assertEqual(entry["target"], "aarch64-unknown-linux-gnu")
        self.assertEqual(entry["cargo_package"], "robot-s0")
        self.assertIs(entry["default_features"], True)
        self.assertEqual(entry["features"], ["xlerobot-real", "rt_guard"])
        self.assertEqual(entry["bins"], ["robot-s0", "robot-deploy"])
        self.assertEqual(len(entry["files"]), 6)
        self.assert_no_cargo()

    def test_build_argv_preserves_native_production_command(self):
        entry = builder.select_package(self.fixture.repository, PACKAGE_ID)
        self.assertEqual(builder.build_argv(entry), EXPECTED_ARGV)
        self.assertNotIn("--target", builder.build_argv(entry))

    def test_new_matrix_entry_changes_native_features_bins_and_payload(self):
        alternate = copy.deepcopy(self.fixture.matrix["packages"][PACKAGE_ID])
        alternate.update(default_features=False, features=["mock-only"], bins=["robot-s0"])
        alternate["files"] = [row for row in alternate["files"] if row["destination"] != "systemd/robot-s0-bus2.service"]
        self.fixture.matrix["packages"]["mock-s0"] = alternate
        self.fixture.write_matrix()
        self.assertEqual(set(builder.load_matrix(self.fixture.repository)), {PACKAGE_ID, "mock-s0"})
        expected = [
            "cargo", "build", "--locked", "--release", "--message-format=json", "-p", "robot-s0",
            "--bin", "robot-s0", "--no-default-features", "--features", "mock-only",
        ]
        self.fixture.commands.expected_argv = expected
        entry = builder.select_package(self.fixture.repository, "mock-s0")
        self.assertEqual(builder.build_argv(entry), expected)
        result = self.fixture.build("mock-s0")
        self.assertEqual(result["manifest"]["package_id"], "mock-s0")
        self.assertEqual(Path(result["archive"]).name, "robot-system-" + RELEASE_TAG + "-mock-s0.tar.gz")
        self.assert_archive_names(result, PAYLOAD_NAMES - {"bin/robot-deploy", "systemd/robot-s0-bus2.service"})
        self.assertEqual(FakeBuildCommands.local_features(self.fixture.commands.cargo_package("robot-s0")[1], expected), ["mock", "mock-only"])

    def test_editing_matrix_changes_default_closure_and_adds_selected_file(self):
        entry = self.fixture.matrix["packages"][PACKAGE_ID]
        entry.update(bins=["robot-s0"], features=[])
        entry["files"].append({"source": "configs/xlerobot-left-arm.toml", "destination": "configs/xlerobot-left-arm.toml"})
        self.fixture.write_matrix()
        expected = ["cargo", "build", "--locked", "--release", "--message-format=json", "-p", "robot-s0", "--bin", "robot-s0"]
        self.fixture.commands.expected_argv = expected
        self.assertEqual(builder.build_argv(builder.select_package(self.fixture.repository, PACKAGE_ID)), expected)
        result = self.fixture.build()
        self.assert_archive_names(result, (PAYLOAD_NAMES - {"bin/robot-deploy"}) | {"configs/xlerobot-left-arm.toml"})
        self.assertEqual(FakeBuildCommands.local_features(self.fixture.commands.cargo_package("robot-s0")[1], expected), ["default", "feetech", "mock", "xlerobot"])

    def test_matrix_can_select_another_existing_workspace_package(self):
        self.fixture.write("Cargo.toml", '[workspace]\nmembers = ["crates/robot-s0", "crates/matrix-runtime"]\n\n[workspace.package]\nversion = "0.1.2"\n')
        self.fixture.write("crates/matrix-runtime/Cargo.toml", '[package]\nname = "matrix-runtime"\nversion.workspace = true\n\n[features]\nlocal = []\n')
        self.fixture.write("crates/matrix-runtime/src/bin/matrix_daemon.rs", "fn main() {}\n")
        self.fixture.matrix["packages"]["matrix-runtime"] = {
            "target": "aarch64-unknown-linux-gnu", "cargo_package": "matrix-runtime",
            "default_features": False, "features": ["local"], "bins": ["matrix_daemon"],
            "files": [{"source": "scripts/camera_capture.py", "destination": "scripts/camera_capture.py"}],
        }
        self.fixture.write_matrix()
        expected = [
            "cargo", "build", "--locked", "--release", "--message-format=json", "-p", "matrix-runtime",
            "--bin", "matrix_daemon", "--no-default-features", "--features", "local",
        ]
        self.fixture.commands.expected_argv = expected
        entry = builder.select_package(self.fixture.repository, "matrix-runtime")
        self.assertEqual(builder.build_argv(entry), expected)
        result = self.fixture.build("matrix-runtime")
        self.assert_archive_names(result, {"bin/matrix_daemon", "scripts/camera_capture.py", "VERSION", "README.md"})

    def test_list_only_reads_matrix_without_build_commands(self):
        output = io.StringIO()
        with patch.object(builder, "REPOSITORY", self.fixture.repository), patch.object(
            builder, "run_command", side_effect=AssertionError("--list 不得运行构建命令")
        ), contextlib.redirect_stdout(output):
            result = builder.main(["--list"])
        self.assertIn(result, (None, 0))
        self.assertIn(PACKAGE_ID, output.getvalue())
        self.assertFalse(self.fixture.target.exists())
        self.assertFalse(self.fixture.output.exists())

    def test_unknown_package_is_rejected(self):
        with self.assertRaises(REJECTED):
            builder.select_package(self.fixture.repository, "unknown-robot")
        with self.assertRaises(REJECTED):
            builder.build_package(self.fixture.repository, "unknown-robot", self.fixture.target, self.fixture.output, runner=self.fixture.commands)
        self.assert_no_cargo()

    def test_source_parent_traversal_is_rejected(self):
        self.assert_matrix_rejected(lambda entry: entry["files"][0].update(source="../outside.toml"))

    def test_absolute_source_is_rejected(self):
        self.assert_matrix_rejected(lambda entry: entry["files"][0].update(source=str(self.fixture.root / "outside.toml")))

    def test_destination_parent_traversal_is_rejected(self):
        self.assert_matrix_rejected(lambda entry: entry["files"][0].update(destination="../outside.toml"))

    def test_absolute_destination_is_rejected(self):
        self.assert_matrix_rejected(lambda entry: entry["files"][0].update(destination="/etc/systemd/system/unsafe.service"))

    def test_duplicate_source_is_rejected(self):
        self.assert_matrix_rejected(lambda entry: entry["files"][1].update(source=entry["files"][0]["source"]))

    def test_duplicate_destination_is_rejected(self):
        self.assert_matrix_rejected(lambda entry: entry["files"][1].update(destination=entry["files"][0]["destination"]))

    def test_reserved_manifest_destination_is_rejected(self):
        self.assert_matrix_rejected(lambda entry: entry["files"][0].update(destination="manifest.json"))

    def test_symlink_source_is_rejected(self):
        row = self.fixture.matrix["packages"][PACKAGE_ID]["files"][0]
        source = self.fixture.repository / row["source"]
        replacement = self.fixture.root / "outside-source"
        replacement.write_bytes(source.read_bytes())
        source.unlink()
        source.symlink_to(replacement)
        with self.assertRaises(REJECTED):
            builder.select_package(self.fixture.repository, PACKAGE_ID)
        self.assert_no_cargo()

    def test_formal_target_tree_is_rejected(self):
        with self.assertRaises(REJECTED):
            builder.build_package(self.fixture.repository, PACKAGE_ID, self.fixture.repository / "target", self.fixture.output, runner=self.fixture.commands)
        self.assert_no_cargo()
        self.assert_no_archive()

    def test_external_formal_installation_target_is_rejected_before_cargo(self):
        formal = self.external_formal_binaries()
        with self.assertRaises(REJECTED):
            builder.build_package(self.fixture.repository, PACKAGE_ID, formal / "target", self.fixture.output, runner=self.fixture.commands)
        self.assert_no_cargo()
        self.assert_no_archive()

    def test_hardlinked_external_formal_binary_in_cache_is_rejected_before_cargo(self):
        formal = self.external_formal_binaries()
        cached = self.fixture.target / "release" / "robot-s0"
        cached.parent.mkdir(parents=True)
        os.link(formal / "target" / "release" / "robot-s0", cached)
        with self.assertRaises(REJECTED):
            self.fixture.build()
        self.assert_no_cargo()
        self.assert_no_archive()

    def test_wrong_host_is_rejected_before_cargo(self):
        self.fixture.commands.rustc_host = "x86_64-unknown-linux-gnu"
        with self.assertRaises(REJECTED):
            self.fixture.build()
        self.assert_no_cargo()
        self.assert_no_archive()

    def test_non_nightly_rustc_is_rejected_before_cargo(self):
        self.fixture.commands.rustc_release = "1.102.0"
        with self.assertRaises(REJECTED):
            self.fixture.build()
        self.assert_no_cargo()
        self.assert_no_archive()

    def test_non_nightly_toolchain_file_is_rejected(self):
        self.fixture.write("rust-toolchain.toml", '[toolchain]\nchannel = "stable"\n')
        with self.assertRaises(REJECTED):
            self.fixture.build()
        self.assert_no_cargo()

    def test_dirty_source_is_rejected_before_cargo(self):
        self.fixture.commands.status = " M Cargo.toml\n"
        with self.assertRaises(REJECTED):
            self.fixture.build()
        self.assert_no_cargo()
        self.assert_no_archive()

    def test_successful_package_has_exact_payload_and_verified_digests(self):
        result = self.fixture.build()
        archive = Path(result["archive"])
        self.assertTrue(archive.is_absolute())
        self.assertEqual(archive.name, "robot-system-" + RELEASE_TAG + "-" + PACKAGE_ID + ".tar.gz")
        manifest = result["manifest"]
        self.assertEqual(manifest["schema"], "ROBOT_SYSTEM_PACKAGE_V1")
        self.assertEqual(manifest["version"], "0.1.2")
        self.assertEqual(manifest["commit"], COMMIT)
        self.assertEqual(manifest["release_tag"], RELEASE_TAG)
        self.assertEqual(manifest["package_id"], PACKAGE_ID)
        self.assertFalse(manifest["deployable"])
        self.assertFalse(manifest["installed"])
        self.assertFalse(manifest["hardware_started"])
        self.assertEqual(manifest["D15"], "OPEN/DEFERRED")
        self.assertEqual(json.loads(Path(result["manifest_path"]).read_text()), manifest)
        with tarfile.open(archive, "r:gz") as package:
            members = package.getmembers()
            self.assertTrue(all(member.isfile() for member in members))
            self.assertEqual({member.name for member in members}, PAYLOAD_NAMES | {"manifest.json"})
            self.assertEqual(json.loads(package.extractfile("manifest.json").read()), manifest)
            self.assertEqual(set(manifest["files"]), PAYLOAD_NAMES)
            for name in PAYLOAD_NAMES:
                data = package.extractfile(name).read()
                row = manifest["files"][name]
                self.assertEqual(row["sha256"], hashlib.sha256(data).hexdigest(), name)
                self.assertEqual(row["bytes"], len(data), name)
                self.assertEqual(row["mode"], stat.S_IMODE(package.getmember(name).mode), name)
            self.assertEqual(package.extractfile("VERSION").read().decode().strip(), RELEASE_TAG)
            for row in self.fixture.matrix["packages"][PACKAGE_ID]["files"]:
                self.assertEqual(package.extractfile(row["destination"]).read(), (self.fixture.repository / row["source"]).read_bytes())
        self.assertTrue(all(path.is_file() for path in self.fixture.output.iterdir()))

    def test_cargo_uses_isolated_target_and_preserves_formal_release_file(self):
        before = self.fixture.frozen_release.read_bytes()
        self.fixture.build()
        cargo = [call for call in self.fixture.commands.calls if Path(call["argv"][0]).name == "cargo"]
        self.assertEqual(len(cargo), 1)
        self.assertEqual(cargo[0]["argv"], EXPECTED_ARGV)
        self.assertEqual(cargo[0]["env"]["CARGO_TARGET_DIR"], str(self.fixture.target))
        self.assertEqual(cargo[0]["env"]["RUSTUP_TOOLCHAIN"], "nightly")
        self.assertEqual(cargo[0]["env"]["CARGO_BUILD_JOBS"], "2")
        self.assertEqual(self.fixture.frozen_release.read_bytes(), before)

    def test_missing_json_artifact_does_not_use_existing_release_file(self):
        self.fixture.commands.mutate_records = lambda rows: rows.pop(1)
        with self.assertRaises(REJECTED):
            self.fixture.build()
        self.assertTrue((self.fixture.target / "release" / "robot-deploy").is_file())
        self.assert_no_archive()

    def test_wrong_feature_closure_is_rejected(self):
        self.fixture.commands.mutate_records = lambda rows: rows[0].update(features=["xlerobot-real"])
        with self.assertRaises(REJECTED):
            self.fixture.build()
        self.assert_no_archive()

    def test_test_feature_leak_is_rejected(self):
        self.fixture.commands.mutate_records = lambda rows: rows[0]["features"].append("test-freeze-hook")
        with self.assertRaises(REJECTED):
            self.fixture.build()
        self.assert_no_archive()

    def test_duplicate_artifact_is_rejected(self):
        self.fixture.commands.mutate_records = lambda rows: rows.insert(0, copy.deepcopy(rows[0]))
        with self.assertRaises(REJECTED):
            self.fixture.build()
        self.assert_no_archive()

    def test_wrong_profile_or_elf_machine_is_rejected(self):
        for defect in ("profile", "machine"):
            with self.subTest(defect=defect):
                fixture = PackageFixture()
                self.addCleanup(fixture.cleanup)
                if defect == "profile":
                    fixture.commands.mutate_records = lambda rows: rows[0]["profile"].update(opt_level="0")
                else:
                    fixture.commands.elf_machine = 62
                with self.assertRaises(REJECTED):
                    fixture.build()
                self.assertEqual(sum(Path(call["argv"][0]).name == "cargo" for call in fixture.commands.calls), 1)
                self.assertEqual(list(fixture.output.glob("*.tar.gz")), [])

    def test_spoofed_package_id_suffix_is_rejected(self):
        self.fixture.commands.mutate_records = lambda rows: rows[0].update(package_id="path+file:///fixture/robot-s0#robot-other@0.1.2")
        with self.assertRaises(REJECTED):
            self.fixture.build()
        self.assert_no_archive()

    def test_test_feature_in_dependency_artifact_is_rejected(self):
        def add_dependency(rows):
            rows.insert(0, {
                "reason": "compiler-artifact", "package_id": "path+file:///fixture#dependency@0.1.0",
                "target": {"name": "dependency", "kind": ["lib"]}, "executable": None,
                "features": ["test-unapproved"],
            })

        self.fixture.commands.mutate_records = add_dependency
        with self.assertRaises(REJECTED):
            self.fixture.build()
        self.assert_no_archive()

    def test_source_head_or_status_change_after_cargo_is_rejected(self):
        for defect in ("head", "status"):
            with self.subTest(defect=defect):
                fixture = PackageFixture()
                self.addCleanup(fixture.cleanup)
                if defect == "head":
                    fixture.commands.after_cargo = lambda commands: setattr(commands, "commit", "b" * 40)
                else:
                    fixture.commands.after_cargo = lambda commands: setattr(commands, "status", " M Cargo.toml\n")
                with self.assertRaises(REJECTED):
                    fixture.build()
                self.assertEqual(sum(Path(call["argv"][0]).name == "cargo" for call in fixture.commands.calls), 1)
                self.assertEqual(list(fixture.output.glob("*.tar.gz")), [])

    def test_failed_build_finished_is_rejected(self):
        self.fixture.commands.mutate_records = lambda rows: rows[-1].update(success=False)
        with self.assertRaises(REJECTED):
            self.fixture.build()
        self.assert_no_archive()

    def test_missing_build_finished_is_rejected(self):
        self.fixture.commands.mutate_records = lambda rows: rows.pop()
        with self.assertRaises(REJECTED):
            self.fixture.build()
        self.assert_no_archive()

    def test_failed_cargo_exit_is_rejected(self):
        self.fixture.commands.cargo_returncode = 1
        with self.assertRaises(REJECTED):
            self.fixture.build()
        self.assert_no_archive()

    def test_existing_archive_is_never_overwritten(self):
        result = self.fixture.build()
        archive = Path(result["archive"])
        before = archive.read_bytes()
        with self.assertRaises(REJECTED):
            self.fixture.build()
        self.assertEqual(archive.read_bytes(), before)

    def test_symlink_archive_destination_is_rejected_before_cargo(self):
        self.fixture.output.mkdir()
        existing = self.fixture.root / "unrelated-file"
        existing.write_bytes(b"must remain unchanged\n")
        archive = self.fixture.output / ("robot-system-" + RELEASE_TAG + "-" + PACKAGE_ID + ".tar.gz")
        archive.symlink_to(existing)
        with self.assertRaises(REJECTED):
            self.fixture.build()
        self.assert_no_cargo()
        self.assertTrue(archive.is_symlink())
        self.assertEqual(existing.read_bytes(), b"must remain unchanged\n")

    def test_package_does_not_read_external_calibration_or_protection_state(self):
        real_open = Path.open

        def open_local_only(path, *args, **kwargs):
            if str(path).startswith(("/home/loosqk/", "/var/lib/robot/")):
                raise AssertionError("打包不得读取部署主机的外部文件：" + str(path))
            return real_open(path, *args, **kwargs)

        with patch.object(Path, "open", open_local_only):
            result = self.fixture.build()
        with tarfile.open(result["archive"], "r:gz") as package:
            self.assertEqual(set(package.getnames()), PAYLOAD_NAMES | {"manifest.json"})
        self.assertTrue(result["manifest"]["external_requirements"])
        self.assertTrue(all(Path(call["argv"][0]).name in ("git", "rustc", "cargo") for call in self.fixture.commands.calls))


if __name__ == "__main__":
    unittest.main()
