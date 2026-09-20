"""共享校验的纯 fixture 测试；不执行 Cargo、SSH、服务或机器人 ELF。"""
import copy
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import post_e_h2_artifacts as a


class ArtifactTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.target = self.root / "cache"
        (self.target / "release").mkdir(parents=True)
        self.source_root = self.root / "source"
        self.source_root.mkdir()
        (self.source_root / "Cargo.toml").write_text("[workspace]\n")
        self.source = a.snapshot_source(self.source_root, ["Cargo.toml"])
        self.source_ref = self.write("source.json", self.source)
        self.head = "a" * 40
        self.env = a.fixed_build_environment(self.target, {"PATH": "/usr/bin"})[1]
        self.counter = 0

    def write(self, name, value):
        path = self.root / name
        path.write_text(json.dumps(value))
        return {"path": str(path), "sha256": a.sha256(path)}

    def receipt(self, argv, stdout):
        self.counter += 1
        directory = self.root / str(self.counter)
        directory.mkdir()
        out = directory / "stdout.log"
        err = directory / "stderr.log"
        out.write_text(stdout)
        err.write_text("")
        row = {"name": "fixture", "argv": argv, "cwd": str(self.source_root), "env": self.env,
               "removed_env_keys": [], "started_at": "fixture", "started_monotonic_ns": 1,
               "timeout_seconds": None, "stdout": str(out), "stderr": str(err),
               "launch_receipt": str(directory / "launch.json"),
               "completion_receipt": str(directory / "completion.json"), "pid": 42,
               "identity": {"pid": 42, "start_ticks": 12, "boot_id": "a" * 8 + "-" + "a" * 27,
                            "exe": "/fixture/cargo"},
               "signals": [], "reaped": False, "exit_code": None}
        Path(row["launch_receipt"]).write_text(json.dumps(row))
        row.update(reaped=True, exit_code=0, ended_monotonic_ns=2)
        for key in ("stdout", "stderr"):
            path = Path(row[key])
            row[key + "_sha256"] = a.sha256(path)
            row[key + "_bytes"] = path.stat().st_size
        Path(row["completion_receipt"]).write_text(json.dumps(row))
        return {"path": row["completion_receipt"], "sha256": a.sha256(row["completion_receipt"])}

    def elf(self, path, machine=183):
        header = bytearray(64)
        header[:6] = b"\x7fELF\x02\x01"
        header[16:18] = (3).to_bytes(2, "little")
        header[18:20] = machine.to_bytes(2, "little")
        path.write_bytes(header)
        path.chmod(0o755)

    def cargo(self, kind, build, mutate=None):
        package, names, _, features = a.BUILD_SPECS[kind][build]
        records = []
        for name in names:
            path = self.target / "release" / name
            self.elf(path)
            records.append({"reason": "compiler-artifact", "package_id": "path+file:///fixture#" + package + "@0.1.2",
                            "target": {"name": name, "kind": ["bin"]}, "executable": str(path),
                            "features": list(features), "profile": {"opt_level": "3", "test": False,
                            "debug_assertions": False, "overflow_checks": False, "debuginfo": 0}})
        records.append({"reason": "build-finished", "success": True})
        if mutate:
            mutate(records)
        return self.receipt(a.build_argv(kind, build), "\n".join(json.dumps(r) for r in records))

    def manifest(self, kind):
        directory = self.root / kind
        directory.mkdir()
        rustc = self.receipt(["rustc", "-vV"], "commit-hash: " + a.RUSTC_COMMIT + "\nhost: aarch64-unknown-linux-gnu\n")
        result = {"schema": "P7_RELEASE_ARTIFACTS_V1", "kind": kind, "deployable": False,
                  "build_head": self.head, "build_root": str(self.source_root), "toolchain": a.TOOLCHAIN,
                  "rustc_commit": a.RUSTC_COMMIT, "machine": 183, "rustc_receipt": rustc,
                  "source_manifest": self.source_ref, "source_manifest_sha256": self.source["source_manifest_sha256"],
                  "artifact_directory": str(directory), "builds": {}, "artifacts": {}}
        for build in a.BUILD_SPECS[kind]:
            reference = self.cargo(kind, build)
            row, records = a.validate_cargo_build(reference, kind, build)
            result["builds"][build] = reference
            for name, record in records.items():
                frozen = directory / name
                self.elf(frozen)
                item = {**a.elf_identity(frozen), "path": str(frozen), "cargo_path": record["executable"],
                        "build": build, "compiler_artifact": record, "features": record["features"],
                        "profile": "release", "command": row["argv"], "exit_code": 0,
                        "build_head": self.head, "source_manifest_sha256": result["source_manifest_sha256"]}
                sidecar = self.write(kind + "/" + name + ".build.json", item)
                result["artifacts"][name] = {**item, "sidecar": sidecar}
        return result

    def validate(self, manifest, **kwargs):
        return a.validate_artifact_set(manifest, source_manifest=self.source,
                                      expected_kind=manifest["kind"], expected_head=self.head, **kwargs)

    def test_both_complete_sets(self):
        for kind in a.BUILD_SPECS:
            with self.subTest(kind=kind):
                self.assertTrue(self.validate(self.manifest(kind))["valid"])

    def test_cargo_contract_rejections(self):
        mutations = [lambda r: r.pop(), lambda r: r.append(r[0]),
                     lambda r: r[0]["features"].append("test-evil"),
                     lambda r: r[0]["profile"].update(opt_level="0"),
                     lambda r: r[0]["profile"].update(debug_assertions=True),
                     lambda r: r[0].update(package_id="path+file:///fixture#wrong@1"),
                     lambda r: r[-1].update(success=False),
                     lambda r: r[0].update(executable=str(self.root / "s0_daemon"))]
        for mutate in mutations:
            with self.subTest(mutation=mutate):
                ref = self.cargo("production", "s0", mutate)
                with self.assertRaises(ValueError):
                    a.validate_cargo_build(ref, "production", "s0")

    def test_dependency_test_feature_rejected(self):
        def mutate(records):
            records.insert(0, {"reason": "compiler-artifact", "features": ["test-bad"]})
        with self.assertRaises(ValueError):
            a.validate_cargo_build(self.cargo("production", "s1", mutate), "production", "s1")

    def test_completion_rejections(self):
        for key, value in (("exit_code", 1), ("reaped", False), ("signals", ["TERM"]),
                           ("runner_error", "timeout"), ("ended_monotonic_ns", 0)):
            ref = self.receipt(["test"], "")
            row = a.read_pinned_json(ref)
            row[key] = value
            ref = self.write(Path(ref["path"]).relative_to(self.root), row)
            with self.subTest(key=key), self.assertRaises(ValueError):
                a.validate_completion(ref)
        ref = self.receipt(["test"], "")
        row = a.read_pinned_json(ref)
        Path(row["stdout"]).write_text("changed")
        with self.assertRaises(ValueError):
            a.validate_completion(ref)

    def test_artifact_mutation_rejections(self):
        original = self.manifest("measurement")
        for key, value in (("deployable", True), ("machine", 62), ("build_head", "b" * 40),
                           ("source_manifest_sha256", "0" * 64), ("toolchain", "stable")):
            manifest = copy.deepcopy(original)
            manifest[key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                self.validate(manifest)
        manifest = copy.deepcopy(original)
        manifest["artifacts"].pop("s0_daemon")
        with self.assertRaises(ValueError):
            self.validate(manifest)

    def test_hardlink_and_elf_mutations_rejected(self):
        manifest = self.manifest("measurement")
        frozen = Path(manifest["artifacts"]["s0_daemon"]["path"])
        link = self.root / "alias"
        os.link(frozen, link)
        with self.assertRaises(ValueError):
            self.validate(manifest)
        link.unlink()
        self.elf(frozen, 62)
        with self.assertRaises(ValueError):
            self.validate(manifest)
        self.elf(frozen)
        frozen.chmod(0o644)
        with self.assertRaises(ValueError):
            self.validate(manifest)

    def test_source_mutation_and_traversal_rejected(self):
        (self.source_root / "Cargo.toml").write_text("changed")
        self.assertNotEqual(self.source, a.snapshot_source(self.source_root, ["Cargo.toml"]))
        for names in (["Cargo.toml", "Cargo.toml"], ["crates/../../outside"]):
            with self.assertRaises(ValueError):
                a.snapshot_source(self.source_root, names)
        for bad in ('{"x":1,"x":2}', '{"x":NaN}'):
            with self.assertRaises(ValueError):
                a.strict_json(bad)

    def test_installation_isolation(self):
        installed_dir = self.root / "installed"
        installed_dir.mkdir()
        installed = {name: installed_dir / name for name in a.INSTALLED_NAMES}
        for path in installed.values():
            self.elf(path)
        self.assertTrue(a.validate_target_isolation(self.target, installed)["valid"])
        with self.assertRaises(ValueError):
            a.validate_target_isolation(installed_dir, installed)
        alias = self.target / "release/dependency"
        os.link(installed["s0_daemon"], alias)
        with self.assertRaises(ValueError):
            a.validate_target_isolation(self.target, installed)
        alias.unlink()
        alias.symlink_to(installed["s0_daemon"])
        with self.assertRaises(ValueError):
            a.validate_target_isolation(self.target, installed)

    def test_installed_snapshot_rejections(self):
        units = {name: {"LoadState": "loaded", "ActiveState": "inactive", "SubState": "dead", "MainPID": "0"}
                 for name in a.BUSINESS_UNITS}
        units[a.HANDOFF_UNIT] = {"LoadState": "loaded", "ActiveState": "active", "SubState": "exited"}
        baseline = {"schema": "P7_INSTALLED_STATE_V1", "units": units, "host": "fixture", "boot_id": "fixture",
                    "files": {name: {} for name in [*("installed:" + n for n in a.INSTALLED_NAMES),
                                                     "config:left", "config:right", "config:roles"]}}
        self.assertTrue(a.validate_installed_snapshot(baseline, baseline)["valid"])
        drift = copy.deepcopy(baseline)
        drift["boot_id"] = "changed"
        with self.assertRaises(ValueError):
            a.validate_installed_snapshot(drift, baseline)
        for key, value in (("ActiveState", "active"), ("SubState", "running"), ("MainPID", "10")):
            drift = copy.deepcopy(baseline)
            drift["units"][a.BUSINESS_UNITS[0]][key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                a.validate_installed_snapshot(drift, drift)

    def test_systemd_dependency_order_only(self):
        properties = {key: "" for key in a.UNIT_PROPERTIES}
        properties.update(Id=a.BUSINESS_UNITS[0], LoadState="loaded", ActiveState="inactive",
                          SubState="dead", MainPID="0", DropInPaths="/fixture/10.conf /fixture/20.conf")
        for key in a.UNIT_DEPENDENCY_PROPERTIES:
            properties[key] = "b.service a.service a.service"

        def read(values):
            reply = SimpleNamespace(returncode=0, stdout="\n".join(k + "=" + v for k, v in values.items()))
            with patch.object(a.subprocess, "run", return_value=reply):
                return a.read_systemd_unit(a.BUSINESS_UNITS[0])

        def snapshot(row):
            units = {name: dict(row) for name in a.BUSINESS_UNITS}
            units[a.HANDOFF_UNIT] = {"LoadState": "loaded"}
            return {"schema": "P7_INSTALLED_STATE_V1", "units": units, "host": "fixture", "boot_id": "fixture",
                    "files": {name: {} for name in [*("installed:" + n for n in a.INSTALLED_NAMES),
                                                     "config:left", "config:right", "config:roles"]}}

        baseline = snapshot(read(properties))
        for key in a.UNIT_DEPENDENCY_PROPERTIES:
            reordered = dict(properties, **{key: "a.service b.service a.service"})
            self.assertTrue(a.validate_installed_snapshot(snapshot(read(reordered)), baseline)["valid"])
            for changed in ("a.service b.service", "a.service a.service b.service c.service",
                            "a.service a.service a.service b.service"):
                with self.subTest(key=key, changed=changed), self.assertRaises(ValueError):
                    a.validate_installed_snapshot(snapshot(read(dict(properties, **{key: changed}))), baseline)
        for key, value in (("DropInPaths", "/fixture/20.conf /fixture/10.conf"),
                           ("EnvironmentFiles", "/fixture/b.env /fixture/a.env"),
                           ("Environment", "A=1 B=2")):
            with self.subTest(ordered=key), self.assertRaises(ValueError):
                a.validate_installed_snapshot(snapshot(read(dict(properties, **{key: value}))), baseline)
        # 真正相同成员的有序字段重排也不能被依赖集合规则吞掉。
        for key, before, after in (("EnvironmentFiles", "/fixture/a.env /fixture/b.env", "/fixture/b.env /fixture/a.env"),
                                   ("Environment", "A=1 B=2", "B=2 A=1")):
            with self.subTest(reordered=key), self.assertRaises(ValueError):
                a.validate_installed_snapshot(snapshot(read(dict(properties, **{key: after}))),
                                              snapshot(read(dict(properties, **{key: before}))))

    def test_clean_build_environment(self):
        contaminated = {"PATH": "/usr/bin", "CARGO_PROFILE_RELEASE_OPT_LEVEL": "0",
                        "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS": "bad",
                        "RUSTC_WORKSPACE_WRAPPER": "bad", "ROBOT_TEST_FAKE": "1", "RUSTFLAGS": "bad",
                        "CARGO_BUILD_TARGET": "wrong", "CARGO_BUILD_RUSTFLAGS": "bad"}
        env, shown, removed = a.fixed_build_environment(self.target, contaminated)
        self.assertEqual(set(removed), set(contaminated) - {"PATH"})
        self.assertEqual(env, shown)
        self.assertEqual(env["CARGO_BUILD_JOBS"], "2")
        self.assertEqual(env["RUSTC_WRAPPER"], "")


if __name__ == "__main__":
    unittest.main()
