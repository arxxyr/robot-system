"""P.7 专用的产物、编译输入及安装状态只读验证；导入不执行外部命令。"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shlex
import stat
import subprocess

TOOLCHAIN = "nightly"
# 仅显式历史验读使用；新准备与现场 execute 不接受旧合同。
LEGACY_TOOLCHAIN = "nightly-2026-09-10"
LEGACY_RUSTC_COMMIT = "a36d05efab632d1ddf902b6a5c33b6d5d3b64131"
INPUT_DOMAINS = (
    "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo", "crates", "configs",
    "3rd-party/Cargo.toml", "3rd-party/Cargo.lock", "3rd-party/iceoryx2",
    "3rd-party/iceoryx2-cal", "3rd-party/flatc", "tools/rr-memory-model",
)
BUSINESS_UNITS = tuple(f"{prefix}-{role}-{side}{'-arm' if role == 's0' else ''}.service"
                       for prefix in ('robot', 'xlerobot') for role in ('s0', 's1', 'gateway')
                       for side in ('left', 'right')) + tuple(
                           f'{prefix}-s0-bus{bus}.service' for prefix in ('robot', 'xlerobot') for bus in (1, 2))
HANDOFF_UNIT = "robot-s0-preflight.service"
HANDOFF_UNITS = (HANDOFF_UNIT, "xlerobot-handoff.service")
INSTALLED_NAMES = ("robot-s0", "robot-deploy", "robot-s1", "robot-gateway")
# 参数顺序也属于批准合同，避免偷偷增加 profile、target 或 feature。
BUILD_SPECS = {
    "production": {
        "s0": ("robot-s0", ("robot-s0", "robot-deploy"),
               ("--features", "xlerobot-real,rt_guard"),
               ("default", "feetech", "mock", "rt_guard", "xlerobot", "xlerobot-real")),
        "s1": ("robot-s1", ("robot-s1",), (),
               ("backend-hold-pose", "backend-ik", "backend-mock-dwell", "backend-scripted", "default")),
        "gateway": ("robot-rpc", ("robot-gateway",), ("--no-default-features",), ()),
    },
    "measurement": {
        "s0": ("robot-s0", ("robot-s0",),
               ("--no-default-features", "--features", "xlerobot-real,rt_guard,test-comm-fault-hook"),
               ("feetech", "mock", "rt_guard", "test-comm-fault-hook", "xlerobot", "xlerobot-real")),
        "gateway": ("robot-rpc", ("robot-gateway",),
                    ("--no-default-features", "--features", "test-comm-fault-hook"), ("test-comm-fault-hook",)),
        "tool": ("robot-tests", ("comm_fault_e2e",),
                 ("--no-default-features", "--features", "comm-fault-e2e,real-hw"),
                 ("comm-fault-e2e", "comm-fault-rpc", "real-hw")),
    },
}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False).encode()


def sha256(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def strict_json(text):
    def pairs(items):
        result = {}
        for key, value in items:
            require(key not in result, "JSON 重复键：" + key)
            result[key] = value
        return result

    def invalid(value):
        raise ValueError("JSON 非有限常量：" + value)

    return json.loads(text, object_pairs_hook=pairs, parse_constant=invalid)


def _absolute(path):
    value = Path(path)
    require(value.is_absolute() and ".." not in value.parts, "要求无越界的绝对路径")
    return value


def regular_file(path):
    value = _absolute(path)
    info = value.lstat()
    require(stat.S_ISREG(info.st_mode), "要求普通文件，拒绝符号链接：" + str(path))
    return value, info


def read_pinned_json(reference):
    path, _ = regular_file(reference["path"])
    require(sha256(path) == reference["sha256"], "JSON 文件身份变化：" + str(path))
    return strict_json(path.read_text())


def validate_source_manifest(value):
    require(set(value) == {"domains", "files", "source_manifest_sha256"}, "编译源清单字段错误")
    require(value["domains"] == list(INPUT_DOMAINS) and value["files"], "编译源域不完整")
    for name, item in value["files"].items():
        path = PurePosixPath(name)
        require(not path.is_absolute() and ".." not in path.parts
                and str(path) == name and any(name == d or name.startswith(d + "/") for d in INPUT_DOMAINS),
                "编译源路径越界")
        require(set(item) == {"sha256", "bytes", "executable"}
                and re.fullmatch(r"[0-9a-f]{64}", item["sha256"])
                and type(item["bytes"]) is int and item["bytes"] >= 0
                and type(item["executable"]) is bool, "源身份字段错误")
    require(value["source_manifest_sha256"] == hashlib.sha256(canonical(value["files"])).hexdigest(),
            "编译源清单摘要不匹配")
    return value


def snapshot_source(root, inventory):
    """调用者提供 fresh Git 全量 inventory；本函数过滤固定编译域并逐文件读取。"""
    root = Path(root).resolve(strict=True)
    require(len(inventory) == len(set(inventory)), "Git inventory 有重复路径")
    files = {}
    for name in sorted(inventory):
        if not any(name == d or name.startswith(d + "/") for d in INPUT_DOMAINS):
            continue
        relative = PurePosixPath(name)
        require(not relative.is_absolute() and ".." not in relative.parts, "源路径越界")
        path, info = regular_file(root / relative)
        require(path.resolve().is_relative_to(root), "编译源真实路径越界")
        files[name] = {"sha256": sha256(path), "bytes": info.st_size,
                       "executable": bool(info.st_mode & stat.S_IXUSR)}
    return validate_source_manifest({"domains": list(INPUT_DOMAINS), "files": files,
                                    "source_manifest_sha256": hashlib.sha256(canonical(files)).hexdigest()})


def build_argv(kind, build):
    package, binaries, options, _ = BUILD_SPECS[kind][build]
    argv = ["cargo", "build", "--locked", "--release", "--message-format=json", "-p", package]
    for binary in binaries:
        argv.extend(["--bin", binary])
    return argv + list(options)


def validate_completion(reference):
    """核原启动、真实 wait、命令身份和原日志，不接受异常后碰巧 exit 0。"""
    row = read_pinned_json(reference)
    require(row["completion_receipt"] == reference["path"], "completion 自身路径不符")
    require("runner_error" not in row and row.get("exit_code") == 0 and row.get("reaped") is True
            and row.get("signals") == [], "命令没有自然成功完成")
    launch_path, _ = regular_file(row["launch_receipt"])
    launch = strict_json(launch_path.read_text())
    for key in ("name", "argv", "cwd", "env", "removed_env_keys", "started_at", "started_monotonic_ns",
                "timeout_seconds", "stdout", "stderr", "launch_receipt", "completion_receipt"):
        require(key in row and row[key] == launch.get(key), "原 launch/completion 不一致：" + key)
    require("runner_error" not in launch and launch.get("reaped") is False
            and launch.get("exit_code") is None and launch.get("signals") == [], "原 launch 非启动状态")
    require(type(row["started_monotonic_ns"]) is int and type(row["ended_monotonic_ns"]) is int
            and row["ended_monotonic_ns"] >= row["started_monotonic_ns"], "完成时序错误")
    for key in ("stdout", "stderr"):
        path, info = regular_file(row[key])
        require(info.st_size == row[key + "_bytes"] and sha256(path) == row[key + "_sha256"], "原日志变化")
    return row


def receipt_boot_id(row):
    """单调时钟只在同一次系统启动内可比较，兼容原回执中的 boot_id 位置。"""
    boot = row.get("boot_id") or row.get("identity", {}).get("boot_id")
    require(isinstance(boot, str) and re.fullmatch(r"[0-9a-f-]{36}", boot), "回执缺本次系统启动标识")
    return boot


def validate_build_environment(env, toolchain=TOOLCHAIN):
    """rustc 与 Cargo 共用完整显式环境，不允许额外编译覆盖。"""
    require(env.get("RUSTUP_TOOLCHAIN") == toolchain and env.get("RUSTC_WRAPPER") == ""
            and env.get("CARGO_BUILD_JOBS") == "2" and env.get("CARGO_TERM_COLOR") == "never", "构建环境不符")
    require(set(env) == {"RUSTUP_TOOLCHAIN", "RUSTC_WRAPPER", "CARGO_BUILD_JOBS", "CARGO_TERM_COLOR",
                         "CARGO_TARGET_DIR", "PATH"}, "构建环境清单有未批准覆盖")
    require(isinstance(env["PATH"], str) and env["PATH"], "构建 PATH 缺失")


def validate_cargo_build(reference, kind, build, *, allow_legacy=False):
    row = validate_completion(reference)
    require(row["argv"] == build_argv(kind, build), "Cargo 命令不属于批准 release 构建")
    env = row["env"]
    validate_build_environment(env, LEGACY_TOOLCHAIN if allow_legacy else TOOLCHAIN)
    target_root = _absolute(env["CARGO_TARGET_DIR"]).resolve(strict=True)
    records = [strict_json(line) for line in Path(row["stdout"]).read_text().splitlines() if line.strip()]
    finished = [r for r in records if r.get("reason") == "build-finished"]
    require(len(finished) == 1 and finished[0].get("success") is True, "Cargo 未唯一自然完成")
    artifacts = [r for r in records if r.get("reason") == "compiler-artifact"]
    if kind == "production":
        require(all(not f.startswith("test-") for r in artifacts for f in r["features"]), "生产闭包泄漏 test-* feature")
    package, names, _, features = BUILD_SPECS[kind][build]
    result = {}
    for name in names:
        matches = [r for r in artifacts if r.get("target", {}).get("name") == name
                   and r["target"].get("kind") == ["bin"] and r.get("executable")]
        require(len(matches) == 1, "Cargo executable 不唯一：" + name)
        item = matches[0]
        package_id = item["package_id"]
        require(package_id.split("#")[-1].split("@")[0] == package or ("/" + package + "#") in package_id,
                "Cargo package 身份错误")
        require(sorted(item["features"]) == sorted(features), "实际 Cargo feature 闭包错误")
        profile = item["profile"]
        require(profile.get("opt_level") == "3" and profile.get("test") is False
                and profile.get("debug_assertions") is False and profile.get("overflow_checks") is False
                and profile.get("debuginfo") in (None, 0), "实际 Cargo profile 不是批准 release")
        executable = _absolute(item["executable"])
        require(executable.name == name and executable.parent.resolve() == target_root / "release",
                "Cargo executable 不在本次原生 release 目录")
        result[name] = item
    return row, result


def elf_identity(path, expected_machine=183):
    path, info = regular_file(path)
    with path.open("rb") as stream:
        header = stream.read(64)
    require(len(header) == 64 and header[:6] == b"\x7fELF\x02\x01"
            and int.from_bytes(header[18:20], "little") == expected_machine
            and int.from_bytes(header[16:18], "little") in (2, 3), "非批准架构的可执行 ELF")
    require(info.st_mode & stat.S_IXUSR, "ELF 没有用户执行位")
    return {"sha256": sha256(path), "bytes": info.st_size, "mode": stat.S_IMODE(info.st_mode),
            "elf_machine": expected_machine}


def parse_rustc_identity(version, expected_machine=183):
    """严格解析实际 -vV 的唯一字段；未知提交、非 nightly 和异架构均拒绝。"""
    fields = {}
    for line in version.splitlines():
        if ": " in line:
            key, value = line.split(": ", 1)
            require(key not in fields, "rustc -vV 字段重复：" + key)
            fields[key] = value
    require(all(key in fields for key in ("release", "commit-hash", "host")), "rustc -vV 必要字段缺失")
    require(re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+-nightly", fields["release"]), "实际 rustc 不是 nightly")
    require(re.fullmatch(r"[0-9a-f]{40}", fields["commit-hash"]), "实际 rustc 提交未知或非法")
    hosts = {183: "aarch64-unknown-linux-gnu", 62: "x86_64-unknown-linux-gnu"}
    require(expected_machine in hosts and fields["host"] == hosts[expected_machine], "实际 rustc host 与 ELF 架构不符")
    return {"release": fields["release"], "commit_hash": fields["commit-hash"], "host": fields["host"]}


def validate_toolchain_window(manifest, expected_machine):
    """验证准备驱动已固定的前后身份与时间窗；本函数不更新工具链或联网。"""
    require(manifest.get("toolchain") == TOOLCHAIN, "V2 只接受浮动 nightly 准备合同")
    before, after = [validate_completion(manifest[key]) for key in ("rustc_before", "rustc_after")]
    for row in (before, after):
        require(row["argv"] == ["rustc", "-vV"], "缺实际 rustc -vV 工具链回执")
        validate_build_environment(row["env"])
        require(row["cwd"] == manifest["build_root"], "rustc 与构建根目录不一致")
        identity = parse_rustc_identity(Path(row["stdout"]).read_text(), expected_machine)
        require(identity == manifest.get("rustc_identity"), "前后实际 rustc 身份与清单不符")
    require(before["env"] == after["env"], "前后 rustc 环境变化")
    require(receipt_boot_id(before) == receipt_boot_id(after), "前后 rustc 跨 boot")
    require(before["ended_monotonic_ns"] <= after["started_monotonic_ns"], "rustc 前后时间窗倒序")
    return before, after


def validate_artifact_set(manifest, *, source_manifest, expected_kind, expected_head,
                          expected_machine=183, build_ancestor_verified=False, allow_legacy=False):
    legacy = manifest.get("schema") == "P7_RELEASE_ARTIFACTS_V1"
    require((manifest.get("schema") == "P7_RELEASE_ARTIFACTS_V2" or (legacy and allow_legacy is True))
            and manifest.get("kind") == expected_kind,
            "产物集合类型错误")
    require(expected_kind in BUILD_SPECS and manifest.get("deployable") is False, "产物不得宣称可部署")
    require(re.fullmatch(r"[0-9a-f]{40}", expected_head) and re.fullmatch(r"[0-9a-f]{40}", manifest["build_head"]),
            "Git HEAD 格式错误")
    inherited = manifest["build_head"] != expected_head
    require(not inherited or build_ancestor_verified is True, "不同 build HEAD 缺调用者祖先验证")
    validate_source_manifest(source_manifest)
    require(read_pinned_json(manifest["source_manifest"]) == source_manifest, "完整编译输入变化，不能继承产物")
    require(manifest["source_manifest_sha256"] == source_manifest["source_manifest_sha256"], "源摘要不符")
    require(manifest["machine"] == expected_machine, "产物架构不符")
    if legacy:
        require(manifest["toolchain"] == LEGACY_TOOLCHAIN and manifest["rustc_commit"] == LEGACY_RUSTC_COMMIT,
                "历史工具链不符")
        rustc = validate_completion(manifest["rustc_receipt"])
        require(rustc["argv"] == ["rustc", "-vV"] and rustc["env"].get("RUSTUP_TOOLCHAIN") == LEGACY_TOOLCHAIN,
                "缺历史 rustc 工具链回执")
        version = Path(rustc["stdout"]).read_text()
        host = "aarch64-unknown-linux-gnu" if expected_machine == 183 else "x86_64-unknown-linux-gnu"
        require("commit-hash: " + LEGACY_RUSTC_COMMIT + "\n" in version and "host: " + host + "\n" in version,
                "历史 rustc 身份不符")
    else:
        before, after = validate_toolchain_window(manifest, expected_machine)
    specs = BUILD_SPECS[expected_kind]
    require(set(manifest["builds"]) == set(specs), "构建回执集合不完整")
    expected_names = {name for spec in specs.values() for name in spec[1]}
    require(set(manifest["artifacts"]) == expected_names, "产物集合不完整")
    frozen_paths = set()
    for build, reference in manifest["builds"].items():
        row, artifacts = validate_cargo_build(reference, expected_kind, build, allow_legacy=legacy)
        require(row["cwd"] == manifest["build_root"], "构建根目录不一致")
        if not legacy:
            require(row["env"] == before["env"] and receipt_boot_id(row) == receipt_boot_id(before),
                    "Cargo 与 rustc 环境或 boot 不同")
            require(before["ended_monotonic_ns"] <= row["started_monotonic_ns"]
                    <= row["ended_monotonic_ns"] <= after["started_monotonic_ns"], "Cargo 不在 rustc 前后身份时间窗内")
        for name, actual in artifacts.items():
            item = manifest["artifacts"][name]
            require(item["build"] == build and item["compiler_artifact"] == actual
                    and item["cargo_path"] == actual["executable"], "ELF 与 Cargo 原记录不对应")
            require(item["features"] == actual["features"] and item["profile"] == "release"
                    and item["command"] == row["argv"] and item["exit_code"] == 0
                    and item["build_head"] == manifest["build_head"]
                    and item["source_manifest_sha256"] == manifest["source_manifest_sha256"], "sidecar 构建身份不符")
            frozen = _absolute(item["path"])
            require(frozen.parent == _absolute(manifest["artifact_directory"]) and frozen.name == name,
                    "冻结 ELF 目录或名称错误")
            require(item["sidecar"]["path"] == str(frozen) + ".build.json", "sidecar 文件名错误")
            require(frozen.resolve() != Path(item["cargo_path"]).resolve(), "冻结 ELF 仍指向 Cargo 可变输出")
            require(frozen.resolve() not in frozen_paths, "冻结 ELF 路径复用")
            frozen_paths.add(frozen.resolve())
            require(frozen.stat().st_nlink == 1, "冻结 ELF 仍有硬链接")
            identity = elf_identity(frozen, expected_machine)
            require(all(item[key] == value for key, value in identity.items()), "冻结 ELF 身份变化")
            require(read_pinned_json(item["sidecar"]) == {k: v for k, v in item.items() if k != "sidecar"},
                    "ELF sidecar 不符")
    return {"valid": True, "actual_build_head": manifest["build_head"], "runtime_head": expected_head,
            "inherited_from_ancestor": inherited, "kind": expected_kind,
            "artifacts": {name: row["path"] for name, row in manifest["artifacts"].items()}}


# systemd 的依赖关系是集合；只规范化这些字段的显示顺序，重复仍保留。
UNIT_DEPENDENCY_PROPERTIES = frozenset({
    "Requires", "Wants", "After", "Before", "BindsTo", "PartOf", "Conflicts",
})

UNIT_PROPERTIES = (
    "Id", "LoadState", "ActiveState", "SubState", "Job", "UnitFileState", "FragmentPath", "DropInPaths",
    "User", "Group", "WorkingDirectory", "After", "Before", "Requires", "Wants", "BindsTo", "PartOf",
    "Conflicts", "Restart", "RemainAfterExit", "LimitRTPRIO", "LimitRTPRIOSoft", "LimitMEMLOCK",
    "LimitMEMLOCKSoft", "CPUAffinity", "Nice", "CPUSchedulingPolicy", "CPUSchedulingPriority",
    "ExecStart", "ExecStop", "Environment", "EnvironmentFiles",
)


# 主驱动仍使用项目 Python；这个 Linux 属性探针复用系统已有的 python3-dbus。
# 缺绑定或 D-Bus 查询失败一律拒绝，不安装依赖，不调用 StartUnit/RefUnit。
SYSTEMD_EMPTY_PROPERTY_PROBE = r'''
import dbus
import json
import sys

unit = sys.argv[1]
names = json.loads(sys.argv[2])
bus = dbus.SystemBus(private=True)
try:
    manager = dbus.Interface(bus.get_object("org.freedesktop.systemd1", "/org/freedesktop/systemd1"),
                             "org.freedesktop.systemd1.Manager")
    path = manager.LoadUnit(unit)
    properties = dbus.Interface(bus.get_object("org.freedesktop.systemd1", path),
                                "org.freedesktop.DBus.Properties")
    result = {"unit": str(properties.Get("org.freedesktop.systemd1.Unit", "Id")),
              "object_path": str(path), "load_state": str(properties.Get("org.freedesktop.systemd1.Unit", "LoadState")),
              "python": sys.version, "dbus_file": dbus.__file__, "properties": {}}
    for name in names:
        value = properties.Get("org.freedesktop.systemd1.Service", name)
        result["properties"][name] = {"array": isinstance(value, dbus.Array),
                                      "element_signature": str(getattr(value, "signature", "")),
                                      "count": len(value)}
finally:
    bus.close()
result["connection_closed"] = True
print(json.dumps(result))
'''

SYSTEMD_EMPTY_PROPERTY_SIGNATURES = {"ExecStop": "(sasbttttuii)", "EnvironmentFiles": "(sb)"}


def confirm_empty_systemd_properties(unit, names):
    """只对 show 省略的两个复杂属性，以同一 D-Bus 连接核实际 typed 空数组。"""
    require(unit in (*BUSINESS_UNITS, *HANDOFF_UNITS) and names
            and set(names) <= set(SYSTEMD_EMPTY_PROPERTY_SIGNATURES), "未批准的空属性查询")
    result = subprocess.run(["/usr/bin/python3", "-c", SYSTEMD_EMPTY_PROPERTY_PROBE,
                             unit, json.dumps(sorted(names))],
                            capture_output=True, text=True, timeout=20, check=False)
    require(result.returncode == 0, "systemd typed 属性查询失败：" + unit)
    proof = strict_json(result.stdout)
    require(proof.get("unit") == unit and proof.get("load_state") == "loaded"
            and proof.get("connection_closed") is True
            and re.fullmatch(r"/org/freedesktop/systemd1/unit/[A-Za-z0-9_]+", proof.get("object_path", ""))
            and set(proof.get("properties", {})) == set(names), "systemd typed 属性来源或集合错误")
    for name in names:
        value = proof["properties"][name]
        require(value.get("array") is True
                and value.get("element_signature") == SYSTEMD_EMPTY_PROPERTY_SIGNATURES[name]
                and type(value.get("count")) is int and value["count"] == 0,
                "systemd 属性未被证明是准确签名的空数组：" + name)
    return proof


def read_systemd_unit(unit):
    require(unit in (*BUSINESS_UNITS, *HANDOFF_UNITS), "未批准读取的 unit")
    result = subprocess.run(["systemctl", "show", unit, "--property=" + ",".join(UNIT_PROPERTIES)],
                            capture_output=True, text=True, timeout=20, check=False)
    require(result.returncode == 0, "systemctl 只读查询失败：" + unit)
    fields = dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)
    fields.pop("MainPID", None)  # 兼容旧快照读取器。
    missing = set(UNIT_PROPERTIES) - set(fields)
    if fields.get('LoadState') == 'not-found':
        require(fields.get('Id') == unit and fields.get('Job') in ('', '0', '0 /')
                and ((fields.get('ActiveState'), fields.get('SubState')) == ('inactive', 'dead')
                     or unit in HANDOFF_UNITS and (fields.get('ActiveState'), fields.get('SubState')) == ('active', 'exited')),
                '缺失服务仍有运行状态或Job：' + unit)
        # 未安装项没有可查询的typed执行属性，不为占用匹配要求旧unit重新存在。
        fields.update({name: '' for name in missing})
        missing = set()
    require(set(fields) <= set(UNIT_PROPERTIES) and missing <= set(SYSTEMD_EMPTY_PROPERTY_SIGNATURES),
            "unit 属性缺失或有额外字段")
    if missing:
        confirm_empty_systemd_properties(unit, missing)
        fields.update({name: "" for name in missing})
    require(set(fields) == set(UNIT_PROPERTIES), "unit 属性缺失")
    for key in ("ExecStart", "ExecStop", "Environment", "EnvironmentFiles"):
        raw = fields[key]
        safe = {"full_value_sha256": hashlib.sha256(raw.encode()).hexdigest()}
        if key in ("ExecStart", "ExecStop"):
            # systemctl 在每条已配置命令后拼接上次执行结果；保留命令和参数本身。
            command = re.sub(r" ; start_time=[^;}]* ; stop_time=[^;}]* ; pid=[0-9]+ ; code=[^;}]* ; status=[^;}]* (?=})",
                             " ", raw)
            safe["command_sha256"] = hashlib.sha256(command.encode()).hexdigest()
        if key == "Environment":
            safe["names"] = sorted(item.split("=", 1)[0] for item in shlex.split(raw) if "=" in item)
        if key == "EnvironmentFiles":
            safe["paths"] = re.findall(r"(/[^ ;()]+)", raw)
        fields[key] = safe
    for key in UNIT_DEPENDENCY_PROPERTIES:
        fields[key] = " ".join(sorted(fields[key].split()))
    return fields


def installed_file_identity(path):
    path = _absolute(path)
    original = path.lstat()
    resolved = path.resolve(strict=True)
    # marker 从来不属于本接口的批准输入，拒绝误传。
    require(not resolved.is_relative_to("/var/lib/robot"), "禁止读取机器人运行态 marker")
    real = resolved.stat()
    require(stat.S_ISREG(real.st_mode), "安装快照只允许普通文件")
    return {"path": str(path), "resolved": str(resolved), "exists": True,
            "link_target": os.readlink(path) if stat.S_ISLNK(original.st_mode) else None,
            "mode": stat.S_IMODE(real.st_mode), "uid": real.st_uid, "gid": real.st_gid,
            "bytes": real.st_size, "device": real.st_dev, "inode": real.st_ino, "sha256": sha256(resolved)}


def snapshot_installed(reference, *, unit_reader=read_systemd_unit):
    """只读刷新明确清单及新旧服务；不读取环境值、marker 或串口。"""
    required = {"installed:" + name for name in INSTALLED_NAMES} | {"config:left", "config:right", "config:roles"}
    require(required <= set(reference["files"]), "安装参考清单缺四件/三配置")
    files = {name: installed_file_identity(item["path"]) for name, item in sorted(reference["files"].items())}
    units = {name: unit_reader(name) for name in (*BUSINESS_UNITS, *HANDOFF_UNITS)}
    # 新增 drop-in 或 EnvironmentFile 必须进入真实快照，不可只读旧路径漏掉漂移。
    covered = {row["path"] for row in files.values()}
    for row in units.values():
        if row['LoadState'] == 'not-found':
            continue
        paths = [row["FragmentPath"], *shlex.split(row["DropInPaths"]), *row["EnvironmentFiles"]["paths"]]
        for path in paths:
            if path and path not in covered:
                files["discovered:" + path] = installed_file_identity(path)
                covered.add(path)
    return {"schema": "P7_INSTALLED_STATE_V1", "host": os.uname().nodename,
            "boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip(), "files": files, "units": units}


def validate_installed_snapshot(actual, expected):
    require(actual.get("schema") == expected.get("schema") == "P7_INSTALLED_STATE_V1", "安装状态 schema 错误")
    required = {"installed:" + name for name in INSTALLED_NAMES} | {"config:left", "config:right", "config:roles"}
    require(required <= set(actual.get("files", {})) and actual.get("host") and actual.get("boot_id"),
            "安装身份缺四件、三配置或主机身份")
    require({k: v for k, v in actual.items() if k != "units"} ==
            {k: v for k, v in expected.items() if k != "units"}, "安装文件、配置或主机身份变化")
    require(actual["units"].keys() == expected["units"].keys(), "安装的服务集合变化")
    require(set(actual["units"]) == {*BUSINESS_UNITS, *HANDOFF_UNITS}, "新旧服务清单不完整")
    for name in (*BUSINESS_UNITS, *HANDOFF_UNITS):
        row = actual["units"][name]
        prior = expected["units"][name]
        for key in row.keys() | prior.keys():
            if key == "MainPID":
                continue
            require(key in row and key in prior, f"安装的服务属性缺失：{name}/{key}")
            value, old = row[key], prior[key]
            if key in ("ExecStart", "ExecStop"):
                # 老快照只有原始摘要，仍可验证原运行记录未变的旧产物。
                digest = "command_sha256" if "command_sha256" in value and "command_sha256" in old else "full_value_sha256"
                value, old = value[digest], old[digest]
            require(value == old, f"安装的服务配置或状态变化：{name}/{key}")
        state = (row['ActiveState'], row['SubState'])
        require(row["LoadState"] in ('loaded', 'not-found')
                and row['Job'] in ('', '0', '0 /')
                and (state == ('inactive', 'dead') or name in HANDOFF_UNITS and state == ('active', 'exited')),
                "业务服务未停止或预检服务未完成：" + name)
    return {"valid": True, "installed_unchanged": True, "six_services_inactive": True}


def validate_target_isolation(target_root, installed):
    """Cargo 前核目录、所有可能输出及硬链接；不创建目录、不修改安装件。"""
    root = _absolute(target_root).resolve(strict=True)
    require(root.is_dir(), "必须复用已存在的独立 Cargo 缓存")
    require(set(installed) == set(INSTALLED_NAMES), "隔离校验必须包含实际安装四件")
    identities = [installed_file_identity(path) for path in installed.values()]
    protected_paths = {Path(row["resolved"]) for row in identities}
    protected_inodes = {(row["device"], row["inode"]) for row in identities}
    require(all(not path.is_relative_to(root) for path in protected_paths), "Cargo target 包含现役安装件")
    # 目录中任意现存普通文件都不能硬链接安装件，包括 deps 中的 Cargo 别名。
    for directory, dirs, files in os.walk(root, followlinks=False):
        require(all(not (Path(directory) / name).is_symlink() for name in dirs), "Cargo 缓存含链接目录")
        for name in files:
            path = Path(directory) / name
            resolved = path.resolve(strict=True)
            require(resolved not in protected_paths, "Cargo 输出链接到安装件")
            info = path.stat()
            require((info.st_dev, info.st_ino) not in protected_inodes, "Cargo 输出硬链接到安装件")
    return {"valid": True, "target_root": str(root), "installed": identities}


def fixed_build_environment(target_root, inherited=None):
    """清除构建与测试覆盖，只公开明确设置的变量；保留缓存和凭据。"""
    env = dict(os.environ if inherited is None else inherited)
    fixed = {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTDOCFLAGS", "RUSTC", "RUSTDOC",
             "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "RUST_TEST_THREADS", "CLIPPY_ARGS",
             "CARGO_INCREMENTAL", "NOTIFY_SOCKET", "INVOCATION_ID", "WATCHDOG_USEC", "WATCHDOG_PID"}
    removed = []
    for key in list(env):
        if key in fixed or key.startswith(("CARGO_PROFILE_", "CARGO_TARGET_", "CARGO_BUILD_", "ROBOT_TEST_")):
            removed.append(key)
            del env[key]
    env.update(RUSTUP_TOOLCHAIN=TOOLCHAIN, RUSTC_WRAPPER="", CARGO_TERM_COLOR="never",
               CARGO_TARGET_DIR=str(_absolute(target_root).resolve(strict=True)), CARGO_BUILD_JOBS="2")
    shown = {key: env[key] for key in ("RUSTUP_TOOLCHAIN", "RUSTC_WRAPPER", "CARGO_TERM_COLOR",
                                      "CARGO_TARGET_DIR", "CARGO_BUILD_JOBS", "PATH")}
    return env, shown, sorted(removed)
