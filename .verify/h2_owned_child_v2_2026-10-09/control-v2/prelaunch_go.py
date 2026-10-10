#!/usr/bin/env -S uv run --no-project python
"""右 H2 GO 门：默认只校验；用 uv python find 选解释器后 shell exec，勿保留 uv run 父进程。"""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import time
from types import SimpleNamespace


def require(condition, message):
    if not condition:
        raise ValueError(message)


def canonical(path):
    require(path.is_absolute() and ".." not in path.parts and path == path.resolve(),
            "要求规范绝对路径：" + str(path))


def read_regular(path):
    canonical(path)
    before = path.lstat()
    require(stat.S_ISREG(before.st_mode) and before.st_nlink == 1, "不是独立普通文件：" + str(path))
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        opened = os.fstat(fd)
        require((before.st_dev, before.st_ino) == (opened.st_dev, opened.st_ino), "读取时文件身份变化")
        with os.fdopen(fd, "rb", closefd=False) as stream:
            content = stream.read()
        after = os.fstat(fd)
        require((opened.st_size, opened.st_mtime_ns, opened.st_ctime_ns) ==
                (after.st_size, after.st_mtime_ns, after.st_ctime_ns), "读取期间文件变化")
    finally:
        os.close(fd)
    return content


def strict_json(content):
    def pairs(items):
        result = {}
        for key, value in items:
            require(key not in result, "重复 JSON 字段：" + key)
            result[key] = value
        return result
    def invalid(value):
        raise ValueError("非法 JSON 常量：" + value)
    return json.loads(content, object_pairs_hook=pairs, parse_constant=invalid)


def pinned(path, digest):
    require(re.fullmatch(r"[0-9a-f]{64}", digest or ""), "缺少完整 SHA256")
    raw = read_regular(path)
    require(hashlib.sha256(raw).hexdigest() == digest, "原件 SHA 改变：" + str(path))
    return strict_json(raw)


def reference(ref):
    return pinned(Path(ref["path"]), ref["sha256"])


def validate_output_boundary(output, root, manifests):
    protected = [root, Path("/home/loosqk/repo/robot/robot-system"),
                 Path("/etc"), Path("/var/lib/robot"), Path("/dev")]
    for kind in ("production", "measurement"):
        directory = Path(manifests[kind]["artifact_directory"])
        canonical(directory)
        protected.append(directory.parent)
    for path in protected:
        require(not output.is_relative_to(path) and not path.is_relative_to(output),
                "输出与源码/正式/冻结/受保护目录重合")


def git(root, *arguments):
    result = subprocess.run(["git", "-C", str(root), *arguments], capture_output=True,
                            timeout=30, check=False, env={**os.environ, "GIT_OPTIONAL_LOCKS": "0"})
    require(result.returncode == 0, "只读 Git 查询失败：" + result.stderr.decode(errors="replace"))
    return result.stdout


def tracked_driver(path, root, expected_head):
    canonical(root)
    canonical(path)
    require(re.fullmatch(r"[0-9a-f]{40}", expected_head or ""), "expected-head 必须为完整提交")
    require(path == root / "scripts/run_post_e_h2.py", "驱动必须是输入仓库的已跟踪入口")
    require(git(root, "rev-parse", "HEAD").decode().strip() == expected_head, "HEAD 不符")
    require(git(root, "status", "--porcelain=v1", "--untracked-files=all") == b"", "源码不 clean")
    hashes = {}
    for name in ("run_post_e_h2.py", "post_e_h2_report.py", "post_e_h2_artifacts.py"):
        raw = read_regular(root / "scripts" / name)
        require(git(root, "show", "HEAD:scripts/" + name) == raw, "驱动/helper 与已跟踪 HEAD 不符")
        hashes[name] = hashlib.sha256(raw).hexdigest()
    sys.dont_write_bytecode = True
    sys.path.insert(0, str(root / "scripts"))
    spec = importlib.util.spec_from_file_location("right_h2_tracked_driver", path)
    driver = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(driver)
    return driver, hashes


def validate_authorization(inputs, go, inputs_sha, output, expected_head, api, now_ns):
    """只验证 root 提供的实际回执；不补字段、不生成现场事实或 GO。"""
    canonical(output)
    require(not output.exists() and output.parent.is_dir(), "运行输出必须为已有父目录下的新目录")
    require(inputs.get("schema") == "P7_H2_INPUTS_V1", "准备草案不能作为执行输入")
    refs = {name: reference(inputs[name]) for name in
            ("source", "production", "measurement", "installed", "alignment", "physical", "calibration")}
    validate_output_boundary(output, Path(inputs["repository"]), refs)
    require(go.get("schema") == "P7_H2_RIGHT_ROOT_GO_V1" and go.get("approved") is True, "缺少显式 root GO")
    require(go.get("side") == inputs.get("side") == "right", "GO/输入必须绑定右臂")
    require(go.get("expected_head") == inputs.get("expected_head") == expected_head, "GO/输入/HEAD 不一致")
    require(go.get("output") == inputs.get("output") == str(output), "GO/输入未绑定本次输出")
    require(go.get("inputs_sha256") == inputs_sha, "GO 未绑定 inputs SHA")
    require(go.get("physical_sha256") == inputs["physical"]["sha256"] and
            go.get("alignment_sha256") == inputs["alignment"]["sha256"], "GO 的现场或对齐绑定错误")
    require(type(go.get("signed_at_ns")) is int and type(go.get("expires_at_ns")) is int and
            0 < go["signed_at_ns"] <= now_ns < go["expires_at_ns"], "GO 尚未生效或已过期")
    api.validate_facts(inputs, refs["alignment"], refs["physical"],
                       SimpleNamespace(side="right", expected_head=expected_head, output=output), now_ns)
    return {name: inputs[name]["sha256"] for name in refs}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, epilog="实际启动须先 uv python find 3.12，再 shell exec taskset ... <解释器> 本脚本；含 output 参数的 uv run 父进程会被既有进程门正确拒绝，不设白名单。")
    for name in ("inputs", "go", "driver", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    for name in ("inputs-sha256", "go-sha256", "expected-head"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--execute", action="store_true")
    options = parser.parse_args(argv)
    inputs = pinned(options.inputs, options.inputs_sha256)
    go = pinned(options.go, options.go_sha256)
    root = Path(inputs["repository"])
    canonical(options.output)
    require(not options.output.is_relative_to(root) and not root.is_relative_to(options.output),
            "运行目录与源码必须分离")
    driver, hashes = tracked_driver(options.driver, root, options.expected_head)
    # Git/加载之后才查最终期限；exec 前再次读回 pinned 输入与三个已跟踪脚本。
    refs = validate_authorization(inputs, go, options.inputs_sha256, options.output,
                                  options.expected_head, driver, time.time_ns())
    require(pinned(options.inputs, options.inputs_sha256) == inputs and
            pinned(options.go, options.go_sha256) == go, "GO 校验期间输入发生变化")
    for name, digest in hashes.items():
        require(hashlib.sha256(read_regular(root / "scripts" / name)).hexdigest() == digest, "exec 前驱动/helper 改变")
    now = time.time_ns()
    require(go["signed_at_ns"] <= now < go["expires_at_ns"], "exec 前 GO 已过期")
    command = [sys.executable, str(options.driver), "--execute", "--side", "right",
               "--physical-reset-confirmed", "--expected-head", options.expected_head,
               "--inputs", str(options.inputs), "--inputs-sha256", options.inputs_sha256,
               "--output", str(options.output)]
    print(json.dumps({"status": "PASS", "mode": "EXEC_HANDOFF" if options.execute else "CHECK_ONLY",
                      "head": options.expected_head, "clean": True, "output_absent": True,
                      "go_expires_at_ns": go["expires_at_ns"], "checked_at_ns": now,
                      "inputs_sha256": options.inputs_sha256, "go_sha256": options.go_sha256,
                      "references": refs, "driver_hashes": hashes, "validate_facts": "PASS",
                      "exec_argv": command, "pid": os.getpid()}, ensure_ascii=False), flush=True)
    if options.execute:
        # stdout 阻塞也不能延长授权有效期；最后一次期限检查之后不再打印。
        require(pinned(options.go, options.go_sha256) == go, "exec 前 GO 原件改变")
        require(go["signed_at_ns"] <= time.time_ns() < go["expires_at_ns"], "交接时 GO 已过期")
        os.execv(sys.executable, command)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        print(json.dumps({"status": "FAIL", "mode": "PREEXEC_REFUSED", "error": repr(error)}, ensure_ascii=False), file=sys.stderr)
        raise SystemExit(2)
