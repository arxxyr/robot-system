"""在临时仓库用假的 Cargo/rustc/harness 验证路径、日志及退出码，不编译或运行真实 smoke。"""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "run_clear_estop_smoke.sh"
HOST = "x86_64-unknown-linux-gnu"
BINARIES = ("robot-s0", "robot-gateway", "clear_estop_e2e")
FAKE_DRIVER = r'''
import json
import os
from pathlib import Path
import sys

role, *args = sys.argv[1:]
receipts = Path(os.environ["SMOKE_TEST_RECEIPTS"])
print(role + " stdout", flush=True)
print(role + " stderr", file=sys.stderr, flush=True)
if role == "cargo":
    (receipts / "cargo.json").write_text(json.dumps({"args": args, "cwd": os.getcwd()}))
    if code := int(os.environ.get("SMOKE_TEST_BUILD_EXIT", "0")):
        sys.exit(code)
    assert args[0] == "build"
    target_dir = Path(args[args.index("--target-dir") + 1])
    target = args[args.index("--target") + 1]
    output = target_dir / target / "release"
    output.mkdir(parents=True, exist_ok=True)
    for name in ("robot-s0", "robot-gateway"):
        (output / name).write_text("新构建的 " + name)
    harness = output / "clear_estop_e2e"
    harness.write_text("""#!/usr/bin/env bash
exec "$SMOKE_TEST_PYTHON" "$SMOKE_TEST_DRIVER" harness "$0" "$@"
""")
    harness.chmod(0o755)
elif role == "harness":
    executable, *argv = args
    output = Path(executable).parent
    assert argv == ["--bin-dir", str(output), "--case", "all"]
    for name in ("robot-s0", "robot-gateway"):
        assert (output / name).read_text() == "新构建的 " + name
    (receipts / "harness.json").write_text(json.dumps({"executable": executable, "args": argv}))
    sys.exit(int(os.environ.get("SMOKE_TEST_HARNESS_EXIT", "0")))
else:
    raise AssertionError("未知假的工具入口：" + role)
'''


class ClearEStopSmokeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="clear estop smoke ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.repo = self.root / "repo with spaces"
        self.script = self.repo / "scripts" / SCRIPT.name
        self.script.parent.mkdir(parents=True)
        shutil.copyfile(SCRIPT, self.script)
        self.receipts = self.root / "receipts"
        self.receipts.mkdir()
        self.fake_bin = self.root / "fake bin"
        self.fake_bin.mkdir()
        self.driver = self.root / "fake tools.py"
        self.driver.write_text(FAKE_DRIVER)
        cargo = self.fake_bin / "cargo"
        cargo.write_text('#!/usr/bin/env bash\nexec "$SMOKE_TEST_PYTHON" "$SMOKE_TEST_DRIVER" cargo "$@"\n')
        cargo.chmod(0o755)
        rustc = self.fake_bin / "rustc"
        rustc.write_text(f"#!/usr/bin/env bash\nprintf '%s\\n' 'rustc fixture' 'host: {HOST}'\n")
        rustc.chmod(0o755)
        self.env = dict(os.environ)
        for key in ("CARGO_TARGET_DIR", "CARGO_BUILD_TARGET", "RUSTC"):
            self.env.pop(key, None)
        self.env.update(
            PATH=str(self.fake_bin) + os.pathsep + self.env["PATH"],
            RUSTC=str(rustc),
            SMOKE_TEST_PYTHON=sys.executable,
            SMOKE_TEST_DRIVER=str(self.driver),
            SMOKE_TEST_RECEIPTS=str(self.receipts),
        )
        self.formal = self.repo / "target" / "release"
        self.formal.mkdir(parents=True)
        for name in BINARIES:
            (self.formal / name).write_text("正式产物不得覆盖：" + name)

    def run_smoke(self, **environment):
        result = subprocess.run(
            ["bash", str(self.script)], cwd=self.root,
            env={**self.env, **environment}, text=True, capture_output=True, timeout=10,
        )
        for name in BINARIES:
            self.assertEqual((self.formal / name).read_text(), "正式产物不得覆盖：" + name)
        return result

    def receipt(self, name):
        return json.loads((self.receipts / (name + ".json")).read_text())

    def assert_output_directory(self, target_parent):
        target = target_parent / "clear-estop-smoke"
        output = target / HOST / "release"
        cargo = self.receipt("cargo")
        args = cargo["args"]
        self.assertEqual(cargo["cwd"], str(self.repo))
        self.assertEqual(args[args.index("--target-dir") + 1], str(target))
        self.assertEqual(args[args.index("--target") + 1], HOST)
        self.assertIn("--locked", args)
        self.assertIn("--release", args)
        self.assertEqual([args[i + 1] for i, arg in enumerate(args) if arg == "--bin"], list(BINARIES))
        self.assertEqual(args[args.index("--features") + 1], "robot-tests/clear-estop-e2e")
        harness = self.receipt("harness")
        self.assertEqual(harness["executable"], str(output / "clear_estop_e2e"))
        self.assertEqual(harness["args"], ["--bin-dir", str(output), "--case", "all"])

    def test_default_directory_keeps_formal_artifacts_and_logs_all_output(self):
        result = self.run_smoke()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_output_directory(self.repo / "target")
        logs = list((self.repo / "logs").glob("clear_estop_smoke_*.log"))
        self.assertEqual(len(logs), 1)
        log = logs[0].read_text()
        for expected in ("repo root", "cargo stdout", "cargo stderr", "harness stdout", "harness stderr", "exit code = 0"):
            self.assertIn(expected, log)

    def test_external_target_directory_is_used_for_both_build_and_execution(self):
        external = self.root / "external-cache"
        result = self.run_smoke(CARGO_TARGET_DIR=str(external))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_output_directory(external)

    def test_external_target_directory_with_spaces_is_not_split(self):
        external = self.root / "external cache with spaces"
        result = self.run_smoke(CARGO_TARGET_DIR=str(external))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_output_directory(external)

    def test_relative_target_directory_is_resolved_from_repository(self):
        result = self.run_smoke(CARGO_TARGET_DIR="relative cache")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_output_directory(self.repo / "relative cache")

    def test_external_cross_target_does_not_change_native_smoke_paths(self):
        result = self.run_smoke(CARGO_BUILD_TARGET="aarch64-unknown-linux-gnu")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_output_directory(self.repo / "target")

    def test_build_failure_returns_its_code_and_never_runs_harness(self):
        result = self.run_smoke(SMOKE_TEST_BUILD_EXIT="53")
        self.assertEqual(result.returncode, 53, result.stdout + result.stderr)
        self.assertTrue((self.receipts / "cargo.json").exists())
        self.assertFalse((self.receipts / "harness.json").exists())

    def test_harness_failure_returns_its_actual_code(self):
        for code in (2, 17):
            with self.subTest(code=code):
                result = self.run_smoke(SMOKE_TEST_HARNESS_EXIT=str(code))
                self.assertEqual(result.returncode, code, result.stdout + result.stderr)
                self.assert_output_directory(self.repo / "target")


if __name__ == "__main__":
    unittest.main()
