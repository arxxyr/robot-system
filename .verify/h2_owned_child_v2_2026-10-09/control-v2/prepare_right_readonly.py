#!/usr/bin/env -S uv run --no-project python
"""右 H2 一次性只读准备；只写独占证据目录，永远不能生成现场 GO。"""

import argparse
import hashlib
import importlib
import json
import os
from pathlib import Path
import platform
import pwd
import re
import stat
import sys
import time
import tomllib
from types import SimpleNamespace


RELEASE_ROOT = Path("/home/loosqk/robot-checks/h2-exit-observation-20261009-r1/release-r1")
BUILD_HEAD = "a96c34ae26b56fc9e8d2deef17c3b829c3d87d0d"
FORMAL_ROOT = Path("/home/loosqk/repo/robot/robot-system")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def save(path, value):
    with path.open("x", encoding="utf-8") as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2, allow_nan=False)
        stream.write("\n")


def reference(path, api):
    return {"path": str(path), "sha256": api.file_hash(path)}


def latch_snapshot(directory):
    """不用运行态写入 API；只读普通文件，不跟随符号链接或打开设备。"""
    result = {}
    for name in ("estop_latch", "estop_latch.bak"):
        path = directory / name
        try:
            before = path.lstat()
        except FileNotFoundError:
            result[name] = {"missing": True, "state": "UNKNOWN"}
            continue
        require(stat.S_ISREG(before.st_mode) and before.st_nlink == 1, "latch 不是独立普通文件")
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
        try:
            opened = os.fstat(fd)
            require((before.st_dev, before.st_ino) == (opened.st_dev, opened.st_ino), "latch 打开时变化")
            data = os.read(fd, 4)
            after = os.fstat(fd)
            require((opened.st_size, opened.st_mtime_ns, opened.st_ctime_ns) ==
                    (after.st_size, after.st_mtime_ns, after.st_ctime_ns), "latch 读取期间变化")
        finally:
            os.close(fd)
        state = {b"0": "NOT_LATCHED", b"0\n": "NOT_LATCHED", b"1": "LATCHED", b"1\n": "LATCHED"}.get(data, "UNKNOWN")
        result[name] = {"path": str(path), "bytes": opened.st_size, "hex_prefix": data.hex(),
                        "sha256": hashlib.sha256(data).hexdigest() if opened.st_size == len(data) else None,
                        "uid": opened.st_uid, "gid": opened.st_gid, "mode": stat.S_IMODE(opened.st_mode),
                        "device": opened.st_dev, "inode": opened.st_ino, "state": state}
    return result


def validate_paths(options):
    require(platform.system() == "Linux" and platform.machine() == "aarch64", "只允许 Linux aarch64 原生只读准备")
    require(re.fullmatch(r"[0-9a-f]{40}", options.expected_head or ""), "expected-head 必须为完整提交")
    for name in ("source", "release_root", "out", "planned_output"):
        path = getattr(options, name)
        require(path.is_absolute() and ".." not in path.parts and path == path.resolve(), name + " 要求规范绝对路径")
    require(options.source.is_dir() and options.release_root == RELEASE_ROOT and RELEASE_ROOT.is_dir(), "源码/固定 release 根不符")
    roots = [options.source, options.release_root, options.out, options.planned_output]
    require(all(not a.is_relative_to(b) and not b.is_relative_to(a)
                for i, a in enumerate(roots) for b in roots[i + 1:]), "四个目录必须两两独立")
    for path in (options.out, options.planned_output):
        require(not path.exists() and path.parent.is_dir(), "准备和计划运行目录必须是已有父目录下的新目录")
        require(not path.is_relative_to(FORMAL_ROOT) and not path.is_relative_to("/etc")
                and not path.is_relative_to("/var/lib/robot") and not path.is_relative_to("/dev"), "输出落入受保护路径")


def prepare(options, driver, artifacts):
    options.out.mkdir()
    backend_options = SimpleNamespace(side="right", expected_head=options.expected_head,
                                      output=options.planned_output, inputs_sha256="NOT_AUTHORIZED")
    journal = driver.Journal(options.out, backend_options)
    journal.state.update(schema="P7_H2_READONLY_PREPARATION_JOURNAL_V1", authorization="NOT_AUTHORIZED",
                         mode="READONLY", checks=[], planned_output=str(options.planned_output),
                         helper_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest())
    journal.save()
    backend = driver.RealBackend(backend_options, journal)
    values = {}

    def check(name, action, validate=None, dependencies=()):
        row = {"name": name, "status": "FAIL", "started_ns": time.time_ns()}
        journal.state["checks"].append(row)
        try:
            require(all(item in values for item in dependencies), "依赖检查未通过：" + ",".join(dependencies))
            row["value"] = action()
            if validate is not None:
                validate(row["value"])
            values[name] = row["value"]
            row["status"] = "PASS"
        except Exception as error:
            row["error"] = repr(error)
            journal.fail(name + ": " + repr(error))
        finally:
            row["finished_ns"] = time.time_ns()
            journal.save()

    profile = backend.profile
    inputs = {"repository": str(options.source)}
    backend.inputs = inputs

    def release_check():
        inputs.update(source=reference(RELEASE_ROOT / "source_before.json", driver),
                      **{kind: reference(RELEASE_ROOT / (kind + ".json"), driver)
                         for kind in ("production", "measurement")})
        prior = artifacts.strict_json((RELEASE_ROOT / "result.json").read_text())
        require(prior["status"] == "PASS" and prior["source_after"] == prior["isolation_after"] == "PASS", "原 release 未通过")
        source = driver.pinned_json(inputs["source"])
        require(len(source["files"]) == 597, "编译输入不是已验 597 项")
        inputs["source_manifest_sha256"] = source["source_manifest_sha256"]
        for kind in ("production", "measurement"):
            require({k: prior["artifacts"][kind][k] for k in ("path", "sha256")} == inputs[kind], "release 清单引用变化")
            manifest = driver.pinned_json(inputs[kind])
            require(manifest["schema"] == "P7_RELEASE_ARTIFACTS_V2" and manifest["build_head"] == BUILD_HEAD
                    and manifest["artifact_directory"] == str(RELEASE_ROOT / kind), "不是本轮固定 V2 冻结集合")
        return {"build_head": BUILD_HEAD, "source_files": 597, "result": reference(RELEASE_ROOT / "result.json", driver)}

    check("release", release_check)
    # 仅当前端内存身份满足 source_check；它不是两端 alignment 回执，不写入草案。
    def local_identity():
        value = {"role": "orin", "host": os.uname().nodename,
                 "boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip()}
        backend.alignment = {"hosts": [value]}
        return {"scope": "仅当前 Orin 身份；不是两端 alignment", "identity": value}

    check("orin_identity", local_identity)
    check("source_before", backend.source_check, dependencies=("release", "orin_identity"))
    check("artifacts", backend.artifact_check, dependencies=("source_before",))
    seed = {"files": {**{"installed:" + name: {"path": str(FORMAL_ROOT / "target/release" / name)}
                         for name in artifacts.INSTALLED_NAMES},
                      "config:left": {"path": "/etc/robot/xlerobot_left_arm.toml"},
                      "config:right": {"path": "/etc/robot/xlerobot_right_arm.toml"},
                      "config:roles": {"path": "/etc/robot/rpc_roles.toml"}}}

    def installed():
        actual = artifacts.snapshot_installed(seed)
        save(options.out / "installed_before.json", actual)
        artifacts.validate_installed_snapshot(actual, actual)
        inputs["installed"] = reference(options.out / "installed_before.json", driver)
        return actual

    check("installed_before", installed)
    check("formal_before", backend.formal_check, dependencies=("installed_before", "artifacts"))

    def configuration():
        require(driver.file_hash(profile.config) == driver.file_hash(options.source / profile.repository_config),
                "D2 配置与当前源码不同，不读取其引用的标定")
        config = tomllib.loads(profile.config.read_text())
        paths = config["hardware"][0]["calibration_paths"]
        require(len(paths) == 1, "D2 标定路径必须唯一")
        calibration = Path(paths[0])
        require(calibration.is_absolute() and calibration.name == profile.calibration_name, "标定路径身份不符")
        inputs.update(config_sha256=driver.file_hash(profile.config), calibration=reference(calibration, driver))
        return backend.config_check()

    check("config", configuration)
    check("runtime", lambda: driver.inspect_runtime_preconditions(profile.trust),
          lambda value: driver.validate_runtime_preconditions(value, profile.trust))
    check("device", backend.device_check)

    def state_files():
        account = pwd.getpwnam("loosqk")
        result = {}
        for side in ("left", "right"):
            trust = driver.arm_profile(side).trust
            require(trust == trust.resolve() and trust.is_dir(), "trust 目录不是真实目录")
            result[side] = {"markers": driver.marker_snapshot(trust, None, account.pw_uid, account.pw_gid),
                            "latches": latch_snapshot(trust)}
        return result

    def valid_state_files(value):
        for side in value.values():
            require(all(row.get("missing") or row.get("hex") in ("300a", "310a") for row in side["markers"].values()), "marker 内容未知")
            require(all(row["state"] != "UNKNOWN" for row in side["latches"].values()), "latch 缺失或格式未知")

    check("state_files_before", state_files, valid_state_files)
    binaries = [FORMAL_ROOT / "target/release" / name for name in artifacts.INSTALLED_NAMES]
    binaries += [RELEASE_ROOT / kind / name for kind in ("production", "measurement")
                 for _, names, _, _ in artifacts.BUILD_SPECS[kind].values() for name in names]
    check("process_snapshot", lambda: driver.process_snapshot(-1, options.planned_output, binaries),
          lambda value: require(value["clean"], "存在相关进程或不可读进程身份"))
    check("source_after", backend.source_check,
          lambda value: require(value == values["source_before"], "源码前后变化"), ("source_before",))
    check("artifacts_after", backend.artifact_check,
          lambda value: require(value == values["artifacts"], "产物前后变化"), ("artifacts", "source_after"))
    check("formal_after", backend.formal_check, dependencies=("formal_before",))
    check("state_files_after", state_files,
          lambda value: require(value == values["state_files_before"], "marker/latch 前后变化"), ("state_files_before",))
    check("planned_output_uncreated", lambda: not options.planned_output.exists(),
          lambda value: require(value, "只读准备期间计划运行目录已出现"))
    draft = {"schema": "P7_H2_PREPARATION_DRAFT_V1", "authorization": "NOT_AUTHORIZED", "side": "right",
             "expected_head": options.expected_head, "planned_output": str(options.planned_output),
             "candidate_inputs": inputs, "physical": None, "alignment": None,
             "missing": ["本次现场事实与独立 GO", "完整两端同 HEAD/clean/源 alignment"],
             "scope": "仅准备草案，不能作为 run_post_e_h2 --inputs；不能由本脚本生成 GO。"}
    save(options.out / "preparation_inputs_draft.json", draft)
    journal.state.update(status="FAIL" if journal.state["failures"] else "PASS", authorization="NOT_AUTHORIZED",
                         finished_ns=time.time_ns(), preparation_inputs=reference(options.out / "preparation_inputs_draft.json", driver))
    journal.save()
    require(not options.planned_output.exists(), "计划运行目录不得由准备脚本创建")
    print(json.dumps({"status": journal.state["status"], "authorization": "NOT_AUTHORIZED",
                      "checks": len(journal.state["checks"]), "failures": len(journal.state["failures"]),
                      "out": str(options.out), "planned_output_created": False}, ensure_ascii=False))
    return 0 if journal.state["status"] == "PASS" else 1


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("source", "release-root", "out", "planned-output"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--expected-head", required=True)
    options = parser.parse_args(argv)
    validate_paths(options)
    sys.dont_write_bytecode = True
    sys.path.insert(0, str(options.source / "scripts"))
    driver = importlib.import_module("run_post_e_h2")
    artifacts = importlib.import_module("post_e_h2_artifacts")
    return prepare(options, driver, artifacts)


if __name__ == "__main__":
    raise SystemExit(main())
