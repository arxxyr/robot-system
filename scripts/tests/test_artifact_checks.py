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
import artifact_checks as a


class ArtifactTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
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
               "removed_env_keys": [], "started_at": "fixture", "started_monotonic_ns": self.counter * 10,
               "timeout_seconds": None, "stdout": str(out), "stderr": str(err),
               "launch_receipt": str(directory / "launch.json"),
               "completion_receipt": str(directory / "completion.json"), "pid": 42,
               "identity": {"pid": 42, "start_ticks": 12, "boot_id": "a" * 8 + "-" + "a" * 27,
                            "exe": "/fixture/cargo"},
               "signals": [], "reaped": False, "exit_code": None}
        Path(row["launch_receipt"]).write_text(json.dumps(row))
        row.update(reaped=True, exit_code=0, ended_monotonic_ns=self.counter * 10 + 1)
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

    def version(self, commit="b" * 40, release="1.101.0-nightly", host="aarch64-unknown-linux-gnu"):
        return f"rustc {release}\nbinary: rustc\ncommit-hash: {commit}\ncommit-date: 2026-09-30\nhost: {host}\nrelease: {release}\nLLVM version: 23.1.0\n"

    def rewrite_receipt(self, reference, mutate):
        """修改夹具原始 launch/completion，两者仍一致，专测更内层合同。"""
        row = a.read_pinned_json(reference)
        mutate(row)
        launch = {k: v for k, v in row.items() if k not in (
            "ended_monotonic_ns", "stdout_sha256", "stdout_bytes", "stderr_sha256", "stderr_bytes")}
        launch.update(reaped=False, exit_code=None)
        Path(row["launch_receipt"]).write_text(json.dumps(launch))
        for key in ("stdout", "stderr"):
            path = Path(row[key])
            row[key + "_sha256"] = a.sha256(path)
            row[key + "_bytes"] = path.stat().st_size
        return self.write(Path(reference["path"]).relative_to(self.root), row)

    def manifest(self, kind, legacy=False):
        directory = self.root / kind
        directory.mkdir()
        self.env["RUSTUP_TOOLCHAIN"] = a.LEGACY_TOOLCHAIN if legacy else a.TOOLCHAIN
        version = self.version(commit=a.LEGACY_RUSTC_COMMIT) if legacy else self.version()
        rustc = self.receipt(["rustc", "-vV"], version)
        result = {"schema": "P7_RELEASE_ARTIFACTS_V1" if legacy else "P7_RELEASE_ARTIFACTS_V2", "kind": kind, "deployable": False,
                  "build_head": self.head, "build_root": str(self.source_root), "toolchain": a.TOOLCHAIN,
                  "machine": 183,
                  "source_manifest": self.source_ref, "source_manifest_sha256": self.source["source_manifest_sha256"],
                  "artifact_directory": str(directory), "builds": {}, "artifacts": {}}
        if legacy:
            result.update(toolchain=a.LEGACY_TOOLCHAIN, rustc_commit=a.LEGACY_RUSTC_COMMIT, rustc_receipt=rustc)
        else:
            result.update(rustc_before=rustc, rustc_identity=a.parse_rustc_identity(version))
        for build in a.BUILD_SPECS[kind]:
            reference = self.cargo(kind, build)
            row, records = a.validate_cargo_build(reference, kind, build, allow_legacy=legacy)
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
        if not legacy:
            result["rustc_after"] = self.receipt(["rustc", "-vV"], version)
        return result

    def validate(self, manifest, **kwargs):
        return a.validate_artifact_set(manifest, source_manifest=self.source,
                                      expected_kind=manifest["kind"], expected_head=self.head, **kwargs)

    def test_both_complete_sets(self):
        for kind in a.BUILD_SPECS:
            with self.subTest(kind=kind):
                self.assertTrue(self.validate(self.manifest(kind))["valid"])

    def test_legacy_requires_explicit_read_only_opt_in(self):
        manifest = self.manifest("measurement", legacy=True)
        with self.assertRaises(ValueError):
            self.validate(manifest)
        self.assertTrue(self.validate(manifest, allow_legacy=True)["valid"])

    def test_rustc_identity_requires_unique_actual_nightly_fields(self):
        self.assertEqual(a.parse_rustc_identity(self.version()), {
            "commit_hash": "b" * 40, "release": "1.101.0-nightly", "host": "aarch64-unknown-linux-gnu"})
        # 未来 nightly 同样通过；合同绑定本轮实际身份，不写死当前版本。
        self.assertEqual(a.parse_rustc_identity(self.version(release="1.120.0-nightly"))["release"], "1.120.0-nightly")
        for version in (
            self.version(release="1.101.0"), self.version(release="1.101.0-beta.1"),
            self.version(commit="unknown"), self.version(commit="b" * 39),
            self.version(host="x86_64-unknown-linux-gnu"), self.version().replace("release: 1.101.0-nightly\n", ""),
            self.version() + "commit-hash: " + "b" * 40 + "\n", self.version() + "release: 1.120.0-nightly\n",
        ):
            with self.subTest(version=version), self.assertRaises(ValueError):
                a.parse_rustc_identity(version)

    def test_toolchain_window_rejects_identity_environment_boot_and_time_drift(self):
        original = self.manifest("measurement")
        mutations = (
            ("rustc_after", lambda r: Path(r["stdout"]).write_text(self.version(commit="c" * 40))),
            ("rustc_after", lambda r: Path(r["stdout"]).write_text(self.version(release="1.120.0-nightly"))),
            ("rustc_after", lambda r: Path(r["stdout"]).write_text(self.version(host="x86_64-unknown-linux-gnu"))),
            ("rustc_after", lambda r: r["env"].update(PATH="/other/bin")),
            ("rustc_after", lambda r: r["identity"].update(boot_id="b" * 8 + "-" + "b" * 27)),
            ("rustc_before", lambda r: r.update(cwd="/other/source")),
            ("rustc_before", lambda r: r.update(argv=["rustc", "-V"])),
            ("rustc_before", lambda r: r["env"].update(RUSTFLAGS="-C opt-level=0")),
            ("rustc_after", lambda r: r.update(started_monotonic_ns=0)),
            ("rustc_after", lambda r: r.update(exit_code=1)),
            ("rustc_before", lambda r: r.update(reaped=False)),
            ("rustc_after", lambda r: r.update(runner_error="timeout")),
            ("rustc_after", lambda r: r.update(signals=["TERM"])),
        )
        for key, mutate in mutations:
            reference = original[key]
            row = a.read_pinned_json(reference)
            output = Path(row["stdout"]).read_text()
            manifest = copy.deepcopy(original)
            manifest[key] = self.rewrite_receipt(reference, mutate)
            with self.subTest(key=key, mutate=mutate), self.assertRaises(ValueError):
                self.validate(manifest)
            Path(row["stdout"]).write_text(output)
            original[key] = self.rewrite_receipt(manifest[key], lambda r: (r.clear(), r.update(row)))

    def test_each_build_must_use_same_boot_environment_and_bounded_window(self):
        manifest = self.manifest("measurement")
        for build in a.BUILD_SPECS["measurement"]:
            original = a.read_pinned_json(manifest["builds"][build])
            for mutate in (
                lambda r: r["identity"].update(boot_id="c" * 8 + "-" + "c" * 27),
                lambda r: r["env"].update(PATH="/other/bin"),
                lambda r: r["env"].update(CARGO_TARGET_DIR=str(self.source_root)),
                lambda r: r.update(cwd="/other/source"),
                lambda r: r.update(started_monotonic_ns=0),
                lambda r: r.update(ended_monotonic_ns=1_000_000),
            ):
                manifest["builds"][build] = self.rewrite_receipt(manifest["builds"][build], mutate)
                with self.subTest(build=build, mutate=mutate), self.assertRaises(ValueError):
                    self.validate(manifest)
                manifest["builds"][build] = self.rewrite_receipt(
                    manifest["builds"][build], lambda r: (r.clear(), r.update(original)))

    def test_cargo_contract_rejections(self):
        mutations = [lambda r: r.pop(), lambda r: r.append(r[0]),
                     lambda r: r[0]["features"].append("test-evil"),
                     lambda r: r[0]["profile"].update(opt_level="0"),
                     lambda r: r[0]["profile"].update(debug_assertions=True),
                     lambda r: r[0].update(package_id="path+file:///fixture#wrong@1"),
                     lambda r: r[-1].update(success=False),
                     lambda r: r[0].update(executable=str(self.root / "robot-s0"))]
        for mutate in mutations:
            with self.subTest(mutation=mutate):
                ref = self.cargo("production", "s0", mutate)
                with self.assertRaises(ValueError):
                    a.validate_cargo_build(ref, "production", "s0")

    def test_measurement_tool_accepts_transitive_rpc_feature_without_recipe_change(self):
        # 独立固定实际 Cargo 输出闭包，不能从待测 BUILD_SPECS 复制期望值。
        features = ["comm-fault-e2e", "comm-fault-rpc", "real-hw"]
        ref = self.cargo("measurement", "tool", lambda rows: rows[0].update(features=features))
        row, artifacts = a.validate_cargo_build(ref, "measurement", "tool")
        self.assertEqual(artifacts["comm_fault_e2e"]["features"], features)
        self.assertEqual(row["argv"], [
            "cargo", "build", "--locked", "--release", "--message-format=json", "-p", "robot-tests",
            "--bin", "comm_fault_e2e", "--no-default-features", "--features", "comm-fault-e2e,real-hw",
        ])

    def test_measurement_tool_rejects_missing_extra_and_duplicate_features(self):
        for features in (
            ["comm-fault-e2e", "real-hw"],
            ["comm-fault-e2e", "comm-fault-rpc", "real-hw", "supervised-demo"],
            ["comm-fault-e2e", "comm-fault-rpc", "comm-fault-rpc", "real-hw"],
        ):
            with self.subTest(features=features):
                ref = self.cargo("measurement", "tool", lambda rows: rows[0].update(features=features))
                with self.assertRaisesRegex(ValueError, "实际 Cargo feature 闭包错误"):
                    a.validate_cargo_build(ref, "measurement", "tool")

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
        manifest["artifacts"].pop("robot-s0")
        with self.assertRaises(ValueError):
            self.validate(manifest)

    def test_hardlink_and_elf_mutations_rejected(self):
        manifest = self.manifest("measurement")
        frozen = Path(manifest["artifacts"]["robot-s0"]["path"])
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
        os.link(installed["robot-s0"], alias)
        with self.assertRaises(ValueError):
            a.validate_target_isolation(self.target, installed)
        alias.unlink()
        alias.symlink_to(installed["robot-s0"])
        with self.assertRaises(ValueError):
            a.validate_target_isolation(self.target, installed)

    def test_installed_snapshot_rejections(self):
        units = {name: {"LoadState": "loaded", "ActiveState": "inactive", "SubState": "dead", "MainPID": "0", "Job": "0"}
                 for name in a.BUSINESS_UNITS}
        for name in a.HANDOFF_UNITS:
            units[name] = {"LoadState": "loaded", "ActiveState": "active", "SubState": "exited", "MainPID": "0", "Job": "0"}
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

    def test_absent_legacy_unit_is_occupancy_only_and_job_is_rejected(self):
        fields = dict(Id=a.BUSINESS_UNITS[0], LoadState='not-found', ActiveState='inactive', SubState='dead', MainPID='0', Job='0')
        for job in ('0', '123 /org/freedesktop/systemd1/job/123'):
            reply = SimpleNamespace(returncode=0, stdout='\n'.join(key + '=' + value for key, value in dict(fields, Job=job).items()))
            with patch.object(a.subprocess, 'run', return_value=reply), \
                 patch.object(a, 'confirm_empty_systemd_properties', side_effect=AssertionError('缺失项不得要求加载旧unit')):
                if job == '0':
                    self.assertEqual(a.read_systemd_unit(a.BUSINESS_UNITS[0])['LoadState'], 'not-found')
                else:
                    with self.assertRaises(ValueError):
                        a.read_systemd_unit(a.BUSINESS_UNITS[0])

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
            for name in a.HANDOFF_UNITS:
                units[name] = {"LoadState": "loaded", "ActiveState": "inactive", "SubState": "dead", "MainPID": "0", "Job": "0"}
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

    def test_systemd_omitted_arrays_require_typed_empty(self):
        unit = a.BUSINESS_UNITS[0]
        properties = {key: "" for key in a.UNIT_PROPERTIES}
        properties.update(Id=unit, LoadState="loaded", ActiveState="inactive", SubState="dead", MainPID="0")

        def show(missing=()):
            return SimpleNamespace(returncode=0, stdout="\n".join(k + "=" + v for k, v in properties.items()
                                                                 if k not in missing))

        def proof(names):
            return {"unit": unit, "object_path": "/org/freedesktop/systemd1/unit/fixture_2eservice",
                    "load_state": "loaded", "connection_closed": True,
                    "properties": {name: {"array": True, "element_signature": a.SYSTEMD_EMPTY_PROPERTY_SIGNATURES[name],
                                           "count": 0} for name in names}}

        with patch.object(a.subprocess, "run", return_value=show()) as run:
            normal = a.read_systemd_unit(unit)
            self.assertEqual(run.call_count, 1)
        for names in (("ExecStop",), ("EnvironmentFiles",), ("ExecStop", "EnvironmentFiles")):
            with patch.object(a.subprocess, "run", side_effect=[show(names), SimpleNamespace(returncode=0, stdout=json.dumps(proof(names)))]) as run:
                self.assertEqual(a.read_systemd_unit(unit), normal)
                self.assertEqual(run.call_count, 2)
                args = run.call_args.args[0]
                self.assertEqual(args[:2], ["/usr/bin/python3", "-c"])
                self.assertEqual(args[3], unit)
                self.assertEqual(json.loads(args[4]), sorted(names))
        mutations = [lambda p: p["properties"]["ExecStop"].update(count=1),
                     lambda p: p["properties"]["ExecStop"].update(count=False),
                     lambda p: p["properties"]["ExecStop"].update(array=False),
                     lambda p: p["properties"]["ExecStop"].update(element_signature="(sb)"),
                     lambda p: p.update(unit="other.service"), lambda p: p.update(load_state="not-found"),
                     lambda p: p.update(connection_closed=False), lambda p: p.update(object_path="/wrong"),
                     lambda p: p["properties"].pop("ExecStop"),
                     lambda p: p["properties"].update(Unknown={})]
        for mutate in mutations:
            value = proof(["ExecStop"])
            mutate(value)
            with self.subTest(mutate=mutate), patch.object(a.subprocess, "run", side_effect=[show(["ExecStop"]), SimpleNamespace(returncode=0, stdout=json.dumps(value))]), self.assertRaises(ValueError):
                a.read_systemd_unit(unit)
        for response in (SimpleNamespace(returncode=1, stdout=""), SimpleNamespace(returncode=0, stdout="{}")):
            with patch.object(a.subprocess, "run", side_effect=[show(["ExecStop"]), response]), self.assertRaises(ValueError):
                a.read_systemd_unit(unit)
        with patch.object(a.subprocess, "run", side_effect=[show(["ExecStop"]), TimeoutError("超时")]), self.assertRaises(TimeoutError):
            a.read_systemd_unit(unit)
        for missing in (("MainPID",), ("ExecStop", "Environment")):
            with patch.object(a.subprocess, "run", return_value=show(missing)) as run, self.assertRaises(ValueError):
                a.read_systemd_unit(unit)
            self.assertEqual(run.call_count, 1)

    def test_systemd_probe_private_connection_closes_on_success_and_failure(self):
        import contextlib
        import io

        class Array(list):
            signature = "(sasbttttuii)"

        class Bus:
            def __init__(self):
                self.closed = False
                self.loads = []
                self.fail_load = False
                self.fail_get = False

            def get_object(self, service, path):
                return self

            def LoadUnit(self, unit):
                self.loads.append(unit)
                if self.fail_load:
                    raise RuntimeError("配置加载失败")
                return "/org/freedesktop/systemd1/unit/fixture_2eservice"

            def Get(self, interface, name):
                if self.fail_get:
                    raise RuntimeError("属性读取失败")
                if name == "Id":
                    return a.BUSINESS_UNITS[0]
                if name == "LoadState":
                    return "loaded"
                return Array()

            def close(self):
                self.closed = True

        for failure in (None, "load", "get"):
            bus = Bus()
            bus.fail_load = failure == "load"
            bus.fail_get = failure == "get"
            calls = []

            def system_bus(*, private):
                calls.append(private)
                return bus

            fake = SimpleNamespace(SystemBus=system_bus, Interface=lambda obj, interface: obj,
                                   Array=Array, __file__="/fixture/dbus.py")
            with patch.dict(sys.modules, {"dbus": fake}), patch.object(sys, "argv", ["probe", a.BUSINESS_UNITS[0], '["ExecStop"]']), contextlib.redirect_stdout(io.StringIO()) as output:
                if failure:
                    with self.assertRaises(RuntimeError):
                        exec(a.SYSTEMD_EMPTY_PROPERTY_PROBE, {})
                else:
                    exec(a.SYSTEMD_EMPTY_PROPERTY_PROBE, {})
                    self.assertTrue(json.loads(output.getvalue())["connection_closed"])
            self.assertEqual(calls, [True])
            self.assertEqual(bus.loads, [a.BUSINESS_UNITS[0]])
            self.assertTrue(bus.closed)

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
        self.assertEqual(env["RUSTUP_TOOLCHAIN"], "nightly")


if __name__ == "__main__":
    unittest.main()
