#!/usr/bin/env python3
"""本轮 Orin V2 纯软件构建；默认仅计划，不安装、不访问设备或服务。"""

import argparse
import datetime
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import sys
import time


INTERRUPTED = None
# 保持 PID/start_ticks/session：先让父进程采实际 bootstrap 身份，再无 shell 地 exec。
# EOF 或任何非批准字节一律退出，不执行目标；源码与真实 Popen argv 进入回执。
BOOTSTRAP = """import os, sys
gate = int(sys.argv[1])
permit = os.read(gate, 1)
os.close(gate)
if permit != b'G':
    os._exit(125)
os.execvpe(sys.argv[2], sys.argv[2:], os.environ)
"""


def save(path, value):
    with path.open("x", encoding="utf-8") as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2)
        stream.write("\n")


def utc():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def proc_entry(pid):
    directory = Path("/proc") / str(pid)
    try:
        fields = (directory / "stat").read_text().rsplit(") ", 1)[1].split()
        return {"pid": int(pid), "start_ticks": int(fields[19]),
                "pgid": int(fields[2]), "sid": int(fields[3]), "state": fields[0]}
    except FileNotFoundError:
        return None


def session_members(sid):
    return [row for path in Path("/proc").iterdir() if path.name.isdigit()
            if (row := proc_entry(path.name)) is not None and row["sid"] == sid]


def stop_owned(process, actions):
    # 仅清理由本 runner 的 start_new_session 创建的会话；不按名称寻找或收养进程。
    for sig, seconds in ((signal.SIGTERM, 5), (signal.SIGKILL, 5)):
        try:
            groups = sorted({row["pgid"] for row in session_members(process.pid)})
        except OSError as error:
            # 查询失败不能阻止自有直接 child 的终止/真实 wait；后续仍报告清场未知。
            actions.append({"sid": process.pid, "query_error": repr(error)})
            groups = []
        for pgid in groups:
            event = {"sid": process.pid, "pgid": pgid, "signal": sig.name}
            try:
                os.killpg(pgid, sig)
                event["sent"] = True
            except ProcessLookupError:
                event["sent"] = False
            except OSError as error:
                event.update(sent=False, error=repr(error))
            actions.append(event)
        if process.poll() is None:
            try:
                process.send_signal(sig)
                actions.append({"pid": process.pid, "signal": sig.name, "sent": True})
            except OSError as error:
                actions.append({"pid": process.pid, "signal": sig.name, "error": repr(error)})
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            process.poll()
            try:
                remaining = session_members(process.pid)
            except OSError:
                remaining = ["查询失败"]
            if not remaining:
                break
            time.sleep(0.05)
        try:
            remaining = session_members(process.pid)
        except OSError:
            remaining = ["查询失败"]
        if not remaining:
            break
    try:
        return process.wait(timeout=1), True
    except subprocess.TimeoutExpired:
        return None, False


class Runner:
    def __init__(self, options, artifacts):
        self.options, self.a = options, artifacts
        self.env, self.shown, self.removed = artifacts.fixed_build_environment(options.target)
        self.receipts = []

    def command(self, name, argv, timeout=60):
        self.a.require(INTERRUPTED is None, "监督已收到中断，不再启动命令")
        folder = self.options.out / name
        folder.mkdir()
        row = {"name": name, "argv": argv, "cwd": str(self.options.source),
               "env": self.shown, "removed_env_keys": self.removed,
               "started_at": utc(), "started_monotonic_ns": time.monotonic_ns(),
               "timeout_seconds": timeout, "stdout": str(folder / "stdout.log"),
               "stderr": str(folder / "stderr.log"), "launch_receipt": str(folder / "launch.json"),
               "completion_receipt": str(folder / "completion.json"), "pid": None,
               "identity": None, "signals": [], "reaped": False, "exit_code": None}
        process, error, residual = None, None, []
        gate_read, gate_write = None, None
        with Path(row["stdout"]).open("xb") as output, Path(row["stderr"]).open("xb") as errors:
            try:
                gate_read, gate_write = os.pipe()
                bootstrap = [sys.executable, "-c", BOOTSTRAP, str(gate_read), *argv]
                row.update(bootstrap_argv=bootstrap, bootstrap_sha256=hashlib.sha256(BOOTSTRAP.encode()).hexdigest(),
                           identity_phase="bootstrap", argv_semantics="收到父进程 GO 后同 PID exec 的目标命令")
                process = subprocess.Popen(bootstrap, cwd=self.options.source, env=self.env,
                                           stdin=subprocess.DEVNULL, stdout=output, stderr=errors,
                                           start_new_session=True, pass_fds=(gate_read,))
                os.close(gate_read)
                gate_read = None
                row["pid"] = process.pid
                identity = proc_entry(process.pid)
                self.a.require(identity is not None and identity["sid"] == process.pid,
                               "未取得自有 child 的新会话身份")
                identity.update(boot_id=Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
                                exe=os.readlink(f"/proc/{process.pid}/exe"))
                row["identity"] = identity
                save(Path(row["launch_receipt"]), row)
                self.a.require(INTERRUPTED is None, "启动放行前收到中断")
                self.a.require(os.write(gate_write, b"G") == 1, "启动管道没有写入完整 GO")
                os.close(gate_write)
                gate_write = None
                row["exec_gate_released"] = True
                deadline = row["started_monotonic_ns"] + timeout * 1_000_000_000
                while row["exit_code"] is None:
                    if INTERRUPTED is not None:
                        raise RuntimeError(f"监督收到信号 {INTERRUPTED}")
                    remaining = (deadline - time.monotonic_ns()) / 1e9
                    if remaining <= 0:
                        raise TimeoutError("自有命令超过外层监督期限")
                    try:
                        row["exit_code"] = process.wait(timeout=min(0.25, remaining))
                        row["reaped"] = True
                    except subprocess.TimeoutExpired:
                        pass
                residual = session_members(process.pid)
                self.a.require(not residual, "直接 child 已退出，但本会话仍有子进程")
                self.a.require(row["exit_code"] == 0, "命令原始退出码非零")
            except BaseException as caught:
                error = repr(caught)
                # 先关闭启动门；bootstrap 即使赶在终止信号前返回，也只能因 EOF 拒绝 exec。
                for fd in (gate_read, gate_write):
                    if fd is not None:
                        os.close(fd)
                gate_read = gate_write = None
                if process is not None:
                    try:
                        row["exit_code"], row["reaped"] = stop_owned(process, row["signals"])
                    except BaseException as cleanup_error:
                        error += "; 收尾失败：" + repr(cleanup_error)
        row.update(ended_monotonic_ns=time.monotonic_ns(), finished_at=utc(),
                   session_residual_before_cleanup=residual)
        try:
            row["session_residual_after_cleanup"] = session_members(process.pid) if process else []
        except OSError as caught:
            error = (error or "") + "; 会话复核失败：" + repr(caught)
        if row.get("session_residual_after_cleanup"):
            error = (error or "") + "; 自有会话未清空"
        if error:
            row["runner_error"] = error
        for key in ("stdout", "stderr"):
            path = Path(row[key])
            row[key + "_bytes"], row[key + "_sha256"] = path.stat().st_size, self.a.sha256(path)
        save(Path(row["completion_receipt"]), row)
        reference = {"path": row["completion_receipt"], "sha256": self.a.sha256(row["completion_receipt"])}
        self.receipts.append(reference)
        print(json.dumps({"name": name, "exit_code": row["exit_code"], "reaped": row["reaped"],
                          "runner_error": error}, ensure_ascii=False), flush=True)
        self.a.require(error is None, name + " 失败；保留原始回执")
        return reference, self.a.validate_completion(reference)

    def source(self, label):
        _, head = self.command(label + "_head", ["git", "rev-parse", "HEAD"])
        _, status = self.command(label + "_status", ["git", "status", "--porcelain=v1", "--untracked-files=all"])
        _, files = self.command(label + "_files", ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        self.a.require(Path(head["stdout"]).read_text().strip() == self.options.build_head,
                       "实际源码 HEAD 与批准构建 HEAD 不符")
        self.a.require(Path(status["stdout"]).read_bytes() == b"", "源码工作区非 clean")
        names = Path(files["stdout"]).read_text().rstrip("\0").split("\0")
        value = self.a.snapshot_source(self.options.source, names)
        save(self.options.out / (label + ".json"), value)
        return value

    def freeze(self, kind, source, source_ref):
        directory = self.options.out / kind
        directory.mkdir()
        manifest = {"schema": "P7_RELEASE_ARTIFACTS_V2", "kind": kind, "deployable": False,
                    "build_head": self.options.build_head, "build_root": str(self.options.source),
                    "toolchain": "nightly", "machine": 183, "source_manifest": source_ref,
                    "source_manifest_sha256": source["source_manifest_sha256"],
                    "artifact_directory": str(directory), "builds": {}, "artifacts": {}}
        for build in self.a.BUILD_SPECS[kind]:
            ref, _ = self.command(kind + "_" + build, self.a.build_argv(kind, build), 7200)
            row, outputs = self.a.validate_cargo_build(ref, kind, build)
            manifest["builds"][build] = ref
            for name, actual in outputs.items():
                cargo_path, frozen = Path(actual["executable"]), directory / name
                original = self.a.elf_identity(cargo_path)
                with cargo_path.open("rb") as incoming, frozen.open("xb") as outgoing:
                    shutil.copyfileobj(incoming, outgoing)
                frozen.chmod(original["mode"])
                self.a.require(self.a.elf_identity(cargo_path) == original == self.a.elf_identity(frozen),
                               "Cargo 输出冻结时身份变化")
                item = {**original, "path": str(frozen), "cargo_path": str(cargo_path), "build": build,
                        "compiler_artifact": actual, "features": actual["features"], "profile": "release",
                        "command": row["argv"], "exit_code": 0, "build_head": self.options.build_head,
                        "source_manifest_sha256": source["source_manifest_sha256"]}
                sidecar = Path(str(frozen) + ".build.json")
                save(sidecar, item)
                manifest["artifacts"][name] = {**item, "sidecar": {"path": str(sidecar), "sha256": self.a.sha256(sidecar)}}
        # 生产集合在开始测量构建之前已逐件复制，避免同名 Cargo 输出覆盖冻结件。
        return manifest


def run(options, a):
    options.out.mkdir()
    runner = Runner(options, a)
    installed = {name: options.formal_root / "target/release" / name for name in a.INSTALLED_NAMES}
    result = {"schema": "P7_NATIVE_PREPARATION_V2", "status": "FAIL", "deployable": False,
              "started_at": utc(), "runner_sha256": a.sha256(Path(__file__)),
              "artifacts_module_sha256": a.sha256(options.source / "scripts/post_e_h2_artifacts.py"),
              "scope": "仅软件编译、纯 Python 回归和默认 PLAN_ONLY；没有安装、服务、串口或 RX 操作。"}
    before, formal_before = None, None
    try:
        isolation = a.validate_target_isolation(options.target, installed)
        save(options.out / "isolation_before.json", isolation)
        formal_before = isolation["installed"]
        before = runner.source("source_before")
        source_ref = {"path": str(options.out / "source_before.json"),
                      "sha256": a.sha256(options.out / "source_before.json")}
        rustc_before, rustc_row = runner.command("rustc_before", ["rustc", "-vV"])
        identity = a.parse_rustc_identity(Path(rustc_row["stdout"]).read_text())
        runner.command("fmt", ["cargo", "fmt", "--all", "--", "--check"], 300)
        runner.command("clippy", ["cargo", "clippy", "--locked", "--all", "--all-targets", "--", "-D", "warnings"], 7200)
        runner.command("python_tests", ["uv", "run", "--no-project", "python", "-m", "unittest", "discover",
                                        "-s", "scripts/tests", "-p", "test_*.py", "-v"], 300)
        _, plan = runner.command("default_plan", ["uv", "run", "--no-project", "python", "scripts/run_post_e_h2.py"])
        a.require(a.strict_json(Path(plan["stdout"]).read_text()).get("mode") == "PLAN_ONLY",
                  "默认 H2 入口没有返回 PLAN_ONLY")
        sets = {kind: runner.freeze(kind, before, source_ref) for kind in ("production", "measurement")}
        rustc_after, _ = runner.command("rustc_after", ["rustc", "-vV"])
        result["artifacts"] = {}
        for kind, manifest in sets.items():
            manifest.update(rustc_before=rustc_before, rustc_after=rustc_after, rustc_identity=identity)
            verified = a.validate_artifact_set(manifest, source_manifest=before, expected_kind=kind,
                                                expected_head=options.build_head)
            path = options.out / (kind + ".json")
            save(path, manifest)
            result["artifacts"][kind] = {"path": str(path), "sha256": a.sha256(path), "validation": verified}
        result["status"] = "PASS"
    except BaseException as error:
        result["error"] = repr(error)
    finally:
        for label, check in (("source_after", lambda: runner.source("source_after")),
                             ("isolation_after", lambda: a.validate_target_isolation(options.target, installed))):
            try:
                actual = check()
                if label == "source_after":
                    a.require(before is not None and actual == before, "编译源前后不一致或缺初始记录")
                else:
                    save(options.out / (label + ".json"), actual)
                    a.require(formal_before is not None and actual["installed"] == formal_before, "正式四件身份改变")
                result[label] = "PASS"
            except BaseException as error:
                result[label] = repr(error)
                result["status"] = "FAIL"
        try:
            a_path = options.source / "scripts/post_e_h2_artifacts.py"
            a.require(a.sha256(a_path) == result["artifacts_module_sha256"], "共享验证脚本身份改变")
        except BaseException as error:
            result.update(status="FAIL", module_check_error=repr(error))
        result.update(finished_at=utc(), receipts=runner.receipts, interrupted_signal=INTERRUPTED)
        save(options.out / "result.json", result)
    return 0 if result["status"] == "PASS" else 1


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-builds", action="store_true", help="仅执行隔离软件准备，不是 H2 execute")
    for name in ("source", "target", "out", "formal-root"):
        parser.add_argument("--" + name, type=Path)
    parser.add_argument("--build-head")
    options = parser.parse_args(argv)
    if not options.run_builds:
        print(json.dumps({"mode": "PLAN_ONLY", "actions_started": 0, "deployable": False,
                          "required": ["--run-builds", "--source", "--target", "--out", "--build-head", "--formal-root"],
                          "steps": ["来源和正式四件只读隔离", "rustc before", "fmt/Clippy/Python/默认计划",
                                    "生产三 recipe 冻结四件", "测量三 recipe 冻结三件", "rustc after/V2/来源和正式四件复核"]}, ensure_ascii=False))
        return 0
    if platform.system() != "Linux" or platform.machine() != "aarch64":
        parser.error("构建模式仅允许 Orin 原生 Linux aarch64")
    if not re.fullmatch(r"[0-9a-f]{40}", options.build_head or ""):
        parser.error("必须提供完整 build-head")
    for name in ("source", "target", "out", "formal_root"):
        path = getattr(options, name)
        if path is None or not path.is_absolute() or ".." in path.parts or path != path.resolve():
            parser.error(name + " 必须为规范绝对路径")
        if name != "out" and not path.is_dir():
            parser.error(name + " 必须为已存在目录")
    if options.out.exists() or not options.out.parent.is_dir():
        parser.error("out 必须是已存在父目录下的全新目录")
    roots = [options.source, options.target, options.out, options.formal_root]
    if any(left.is_relative_to(right) or right.is_relative_to(left)
           for index, left in enumerate(roots) for right in roots[index + 1:]):
        parser.error("源码、输出、缓存与正式根必须两两独立，不得包含或重合")
    spec = importlib.util.spec_from_file_location("post_e_h2_artifacts", options.source / "scripts/post_e_h2_artifacts.py")
    a = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(a)
    previous = {}
    def interrupted(signum, _frame):
        global INTERRUPTED
        INTERRUPTED = signum
    try:
        for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
            previous[sig] = signal.signal(sig, interrupted)
        return run(options, a)
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)


if __name__ == "__main__":
    raise SystemExit(main())
