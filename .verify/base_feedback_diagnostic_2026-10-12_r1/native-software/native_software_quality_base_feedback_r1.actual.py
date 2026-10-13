"""爸爸，右9反馈候选的原生纯软件核验；冻结输入为空时先拒绝。"""

import hashlib
import importlib.util
import json
from pathlib import Path
import sys
from types import SimpleNamespace

sys.dont_write_bytecode = True
FROZEN_INPUTS = {'candidate/base_feedback_diagnostic_r1.py': {'sha256': '87e3c135e4c78f5232a8ba8aa88c999051d0826b339ed4c5b31795c794fb7b9c', 'bytes': 18648}, 'candidate/test_base_feedback_diagnostic_r1.py': {'sha256': 'c20998e1d7ff30f7a6e3ff75a4943fa47f0653e99dcac4e0dc9d96d9b0f28afc', 'bytes': 34451}, 'candidate/fixtures/run_hardware_maintenance.py': {'sha256': 'd97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41', 'bytes': 50168}, 'candidate/fixtures/raw_right9_from_r14_r1.py': {'sha256': '406295f0bf97ca4a482241a565500d2bae1e226c6a8959bcebbd3264c50b9dfa', 'bytes': 12216}, 'candidate/fixtures/maintenance_bindings_base_zero_diagnostic_r4.json': {'sha256': '347a65dbfe0b3d553f7df06b413507b1491c073ced8a283f498bf7a81f51be86', 'bytes': 30051}, 'boot/verify_pure_base_feedback_r1.py': {'sha256': 'ae33cc06a568acecd1e5e145932f90edf77084ecec13ca96fde6701efba57bf2', 'bytes': 9472}}
assert isinstance(FROZEN_INPUTS, dict) and FROZEN_INPUTS, "爸爸，候选尚未由root冻结，不启动任何child。"
BASE = Path("/home/loosqk/robot-checks/base-feedback-r1-software")
SOURCE = Path("/home/loosqk/robot-checks/position-return-20261010-r1/position-return-r7/source")
TARGET = Path("/home/loosqk/robot-checks/xlerobot-velocity-foundation-20261008-r7/build")
OUT = BASE / "quality"
HEAD = "894084738d95a7239611c2b8fbecfe081d3a7fc9"
SOURCE_SHA = "068277d843b191bc8283bab38e667b0f6a4d30e296e8c5a99a97cfe19654fcf8"
HISTORICAL_COMPILER = "080cc27146389dbb5c8772c476b1937be9c6110f"
FIVE = [
    "crates/robot-adapters/feetech/examples/feetech_poke.rs",
    "crates/robot-adapters/feetech/src/bus.rs",
    "crates/robot-adapters/feetech/src/bus_raw_read_tests.rs",
    "crates/robot-adapters/feetech/src/diagnostic.rs",
    "crates/robot-adapters/feetech/src/registers.rs",
]


def file_ref(path):
    raw = path.read_bytes()
    return {"sha256": hashlib.sha256(raw).hexdigest(), "bytes": len(raw)}


def check_inputs():
    actual = {}
    for name, expected in FROZEN_INPUTS.items():
        relative = Path(name)
        assert not relative.is_absolute() and ".." not in relative.parts
        path = BASE / relative
        assert path == path.resolve() and not path.is_symlink() and path.is_file()
        actual[name] = file_ref(path)
        assert actual[name] == expected, (name, actual[name])
    return actual


inputs_before = check_inputs()


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


a = module("feedback_existing_artifacts", Path("/home/loosqk/robot-checks/h2-exit-observation-20261009-r1/source/scripts/post_e_h2_artifacts.py"))
r = module("feedback_existing_runner", Path("/home/loosqk/robot-checks/h2-right-ready-20261009-r1/bootstrap/run_release_v2.py"))
assert a.sha256(Path(r.__file__)) == "d6b20da28de8d5ae077a93cb2fb5ba7f27a66b8145df9e7e91951cb14ac0b12b"
assert a.sha256(Path(a.__file__)) == "bd2b3f7bb8527cd9ecec73894171638e9cf378656b89e284a585c3baa37b8731"
expected = a.strict_json(Path("/home/loosqk/robot-checks/position-return-20261011-r12/native-quality/boot/host_source_final_r12.json").read_text())
assert expected["source_manifest_sha256"] == SOURCE_SHA and len(expected["files"]) == 597
OUT.mkdir()
runner = r.Runner(SimpleNamespace(source=SOURCE, target=TARGET, out=OUT, build_head=HEAD), a)
runner.env["CARGO_HOME"] = "/home/loosqk/robot-checks/viz-orin-a6957cc-20260930-r1/cargo-home"
runner.shown["CARGO_HOME"] = runner.env["CARGO_HOME"]
runner.env["PATH"] = "/home/loosqk/.cargo/bin:/home/loosqk/.local/bin:" + runner.env["PATH"]
runner.shown["PATH"] = runner.env["PATH"]
for key in ("ALL_PROXY", "all_proxy", "http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY", "LD_PRELOAD"):
    runner.env.pop(key, None)
runner.env.update(HTTP_PROXY="http://10.0.1.80:10808", HTTPS_PROXY="http://10.0.1.80:10808", PYTHONDONTWRITEBYTECODE="1")
runner.shown.update(HTTP_PROXY=runner.env["HTTP_PROXY"], HTTPS_PROXY=runner.env["HTTPS_PROXY"])
result = {
    "schema": "BASE_FEEDBACK_R1_NATIVE_SOFTWARE_V1", "status": "FAIL", "started_at": r.utc(),
    "scope": "代理更新浮动nightly→实际fmt→根Clippy及纯Python内存/默认PLAN/缺GO拒绝/静态完整PLAN；不访问UART/服务/现场/USB",
    "native_git_head": HEAD, "native_worktree": "五个dirty源码，不宣称Git对齐",
    "source_expected": SOURCE_SHA,
    "historical_compiler_only_not_pin": HISTORICAL_COMPILER,
    "inputs_before": inputs_before,
    "baseline_USB": "仅旧binding静态fixture，当前身份/供电/看护UNKNOWN；本recipe不生成ready/binding/GO",
}


def text(name, argv, timeout=7200):
    _, row = runner.command(name, argv, timeout)
    return Path(row["stdout"]).read_text()


def snapshot(label):
    head = text(label + "_head", ["git", "rev-parse", "HEAD"])
    status = text(label + "_status", ["git", "status", "--porcelain=v1", "--untracked-files=all"])
    names = text(label + "_files", ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"])
    assert head.strip() == HEAD and status == "".join(" M " + name + "\n" for name in sorted(FIVE))
    value = a.snapshot_source(SOURCE, names.rstrip("\0").split("\0"))
    assert len(value["files"]) == 597 and value["source_manifest_sha256"] == SOURCE_SHA
    r.save(OUT / (label + ".json"), value)
    return value


try:
    runner.command("nightly_update", ["rustup", "update", "nightly", "--no-self-update"], 7200)
    before = snapshot("source_before")
    compiler = a.parse_rustc_identity(text("rustc_before", ["rustc", "-vV"]))
    assert compiler["host"] == "aarch64-unknown-linux-gnu"
    result["compiler"] = compiler
    runner.command("fmt", ["cargo", "fmt", "--all"], 300)
    fmt_after = a.snapshot_source(SOURCE, list(expected["files"]))
    r.save(OUT / "fmt_after.json", fmt_after)
    assert fmt_after == before, "新nightly实际fmt改变源码，保留失败并停本轮，不自动修正或回退。"
    runner.command("clippy", ["cargo", "clippy", "--all", "--all-targets", "--", "-D", "warnings"], 7200)
    runner.command("pure_python_software", [sys.executable, str(BASE / "boot/verify_pure_base_feedback_r1.py")], 300)
    pure = a.strict_json((BASE / "pure-software/result.json").read_text())
    assert pure["status"] == "PASS" and pure["hardware_actions"] == pure["uart_reads"] == pure["parameter_writes"] == 0
    after = snapshot("source_after")
    compiler_after = a.parse_rustc_identity(text("rustc_after", ["rustc", "-vV"]))
    inputs_after = check_inputs()
    assert after == before and compiler_after == compiler and inputs_after == inputs_before
    result.update(status="PASS", compiler_before_after="PASS", source_before_after="PASS", inputs_after=inputs_after,
                  frozen_inputs_before_after="PASS", distinct_memory_methods=pure["distinct_memory_methods"],
                  python_direct_children=len(pure["commands"]), release_builds=0, Rust_tests_run=0,
                  UART_operations=0, hardware_actions=0, parameter_writes=0, new_GO=False,
                  pure_software_result=file_ref(BASE / "pure-software/result.json"))
except BaseException as error:
    result["error"] = repr(error)
finally:
    result.update(finished_at=r.utc(), receipts=runner.receipts)
    r.save(OUT / "result.json", result)
    print(json.dumps(result, ensure_ascii=False), flush=True)
raise SystemExit(0 if result["status"] == "PASS" else 1)
