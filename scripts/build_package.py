#!/usr/bin/env python3
"""按矩阵构建机器人控制系统候选包；不安装、不启动服务、不访问硬件。"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import os
from pathlib import Path, PurePosixPath
import re
import shlex
import stat
import subprocess
import sys
import tarfile
import tomllib

# 固定解释器以 -I -S 启动时，只显式导入随脚本提供的纯验证函数。
sys.path.insert(0, str(Path(__file__).resolve().parent))
from artifact_checks import (
    INSTALLED_NAMES, canonical, elf_identity, fixed_build_environment,
    parse_rustc_identity, require, sha256, strict_json,
)

REPOSITORY = Path(__file__).resolve().parent.parent
SOURCE_DOMAINS = ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo", "crates",
                  "configs", "scripts", "3rd-party", "tools/rr-memory-model")
SAFE_NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9_-]*")
MATRIX_KEYS = {"target", "cargo_package", "default_features", "features", "bins", "files"}


def run_command(argv, *, cwd, env):
    return subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True, check=False)


def _relative(value):
    require(isinstance(value, str) and value and "\\" not in value, "要求非空相对路径")
    path = PurePosixPath(value)
    require(not path.is_absolute() and ".." not in path.parts and str(path) == value and value != ".",
            "相对路径不规范或越界：" + value)
    return path


def _source_file(repository, name):
    path = repository / _relative(name)
    info = path.lstat()
    require(stat.S_ISREG(info.st_mode) and path.resolve(strict=True) == path
            and path.is_relative_to(repository), "源码必须为仓库内非链接普通文件：" + name)
    return path


def _validate_entry(entry):
    require(isinstance(entry, dict) and set(entry) - {"id", "_expected_features", "_source_domains"} == MATRIX_KEYS,
            "打包矩阵字段错误")
    require(entry["target"] == "aarch64-unknown-linux-gnu", "当前仅支持 ARM64 Linux 原生构建")
    require(isinstance(entry["cargo_package"], str) and SAFE_NAME.fullmatch(entry["cargo_package"]),
            "Cargo 包名称不安全")
    require(type(entry["default_features"]) is bool, "default_features 必须为布尔值")
    for field in ("bins", "features"):
        values = entry[field]
        pattern = SAFE_NAME if field == "bins" else re.compile(r"[A-Za-z0-9_][A-Za-z0-9_.+-]*")
        require(isinstance(values, list) and all(isinstance(name, str) and pattern.fullmatch(name) for name in values)
                and len(values) == len(set(values)), field + " 必须为无重复的安全名称数组")
    require(entry["bins"] and all(not name.startswith("test-") for name in entry["features"]),
            "二进制集合为空或生产组合含测试 feature")
    require("id" not in entry or isinstance(entry["id"], str) and SAFE_NAME.fullmatch(entry["id"]),
            "打包项名称不安全")
    require(isinstance(entry["files"], list) and entry["files"], "必须显式选择非空静态文件集合")
    sources, destinations = set(), set()
    for row in entry["files"]:
        require(isinstance(row, dict) and set(row) == {"source", "destination"}, "文件映射字段错误")
        source, destination = _relative(row["source"]), _relative(row["destination"])
        require(source.parts[0] not in {"VERSION", "README.md", "manifest.json", "bin"}
                and destination.parts[0] not in {"VERSION", "README.md", "manifest.json", "bin"},
                "文件映射占用保留路径")
        require(str(source) not in sources and str(destination) not in destinations, "文件映射重复")
        require(all(not destination.is_relative_to(PurePosixPath(old))
                    and not PurePosixPath(old).is_relative_to(destination) for old in destinations),
                "包内文件与目录冲突")
        sources.add(str(source))
        destinations.add(str(destination))


def _workspace_packages(repository):
    document = tomllib.loads(_source_file(repository, "Cargo.toml").read_text())
    members = document.get("workspace", {}).get("members", [])
    require(isinstance(members, list) and members, "workspace.members 不能为空")
    packages = {}
    for member in members:
        member = str(_relative(member))
        # 只展开 workspace 已声明的成员路径，不另起 Cargo 查询或扫描仓库。
        directories = sorted(repository.glob(member))
        require(directories, "workspace 成员不存在：" + member)
        for directory in directories:
            name = (directory / "Cargo.toml").relative_to(repository).as_posix()
            path = _source_file(repository, name)
            manifest = tomllib.loads(path.read_text())
            package_name = manifest.get("package", {}).get("name")
            require(isinstance(package_name, str) and package_name not in packages, "workspace 包名缺失或重复")
            packages[package_name] = (path.parent, manifest)
    return packages


def _feature_closure(entry, manifest):
    features = manifest.get("features", {})
    require(isinstance(features, dict), "crate features 字段错误")
    pending = list(entry["features"])
    if entry["default_features"] and "default" in features:
        pending.append("default")
    result = set()
    while pending:
        name = pending.pop()
        if name in result:
            continue
        require(name in features and not name.startswith("test-"), "未知或禁止的本地 feature：" + name)
        values = features[name]
        require(isinstance(values, list) and all(isinstance(value, str) and value for value in values),
                "crate feature 定义错误：" + name)
        result.add(name)
        pending.extend(value for value in values if not value.startswith("dep:") and "/" not in value)
    return sorted(result)


def _validate_binaries(repository, entry, crate_root, manifest, features):
    binaries, explicit_paths = {}, set()
    for row in manifest.get("bin", []):
        require(isinstance(row, dict) and isinstance(row.get("name"), str), "crate bin 声明错误")
        name = row["name"]
        require(name not in binaries, "crate bin 名称重复")
        path = row.get("path")
        if path is None:
            choices = [crate_root / "src/bin" / (name + ".rs"), crate_root / "src/bin" / name / "main.rs"]
            if name == manifest["package"]["name"]:
                choices.append(crate_root / "src/main.rs")
            matches = [choice for choice in choices if choice.is_file()]
            require(len(matches) == 1, "显式 bin 缺唯一源码入口：" + name)
            source = matches[0]
        else:
            source = crate_root / _relative(path)
        binaries[name] = (source, row.get("required-features", []))
        explicit_paths.add(source)
    if manifest.get("package", {}).get("autobins", True):
        implicit = [(path.stem, path) for path in (crate_root / "src/bin").glob("*.rs")]
        main = crate_root / "src/main.rs"
        if main.is_file():
            implicit.append((manifest["package"]["name"], main))
        for name, source in implicit:
            if name not in binaries and source not in explicit_paths:
                binaries[name] = (source, [])
    for name in entry["bins"]:
        require(name in binaries, "crate 中不存在二进制：" + name)
        source, required = binaries[name]
        _source_file(repository, source.relative_to(repository).as_posix())
        require(isinstance(required, list) and all(isinstance(value, str) and value in features for value in required),
                "二进制 required-features 未满足：" + name)


def load_matrix(repository: Path) -> dict:
    repository = Path(repository).resolve(strict=True)
    matrix = tomllib.loads(_source_file(repository, "configs/package-matrix.toml").read_text())
    require(set(matrix) == {"packages"} and isinstance(matrix["packages"], dict)
            and matrix["packages"], "矩阵必须包含非空 packages")
    packages = _workspace_packages(repository)
    for package_id, entry in matrix["packages"].items():
        require(SAFE_NAME.fullmatch(package_id), "打包项名称不安全")
        _validate_entry(entry)
        require(entry["cargo_package"] in packages, "Cargo 包不在 workspace.members 中")
        crate_root, manifest = packages[entry["cargo_package"]]
        features = _feature_closure(entry, manifest)
        _validate_binaries(repository, entry, crate_root, manifest, features)
        entry["_expected_features"] = features
        selected_sources = [crate_root.relative_to(repository).as_posix(),
                            *(row["source"] for row in entry["files"])]
        entry["_source_domains"] = list(dict.fromkeys(
            name for name in selected_sources
            if not any(name == domain or name.startswith(domain + "/") for domain in SOURCE_DOMAINS)))
        for row in entry["files"]:
            _source_file(repository, row["source"])
    return matrix["packages"]


def select_package(repository: Path, package_id: str) -> dict:
    require(isinstance(package_id, str) and SAFE_NAME.fullmatch(package_id), "打包项名称不安全")
    entries = load_matrix(repository)
    require(package_id in entries, "未知打包项：" + package_id)
    return dict(entries[package_id], id=package_id)


def build_argv(entry) -> list[str]:
    _validate_entry(entry)
    argv = ["cargo", "build", "--locked", "--release", "--message-format=json", "-p", entry["cargo_package"]]
    for binary in entry["bins"]:
        argv.extend(["--bin", binary])
    if not entry["default_features"]:
        argv.append("--no-default-features")
    if entry["features"]:
        argv.extend(["--features", ",".join(entry["features"])])
    return argv


def _absolute(path):
    path = Path(path)
    require(path.is_absolute() and ".." not in path.parts and str(path) == os.path.normpath(path),
            "要求规范绝对路径：" + str(path))
    require(path.resolve(strict=False) == path, "目录不能经符号链接：" + str(path))
    return path


def _isolated_target(repository, target_dir, installed_paths=()):
    target_dir = _absolute(target_dir)
    directories = {repository / "target/release", *(Path(path).parent for path in installed_paths)}
    for directory in directories:
        for protected in {directory, directory.resolve(strict=False)}:
            require(not target_dir.is_relative_to(protected) and not protected.is_relative_to(target_dir),
                    "Cargo 缓存不能覆盖正式安装目录或包含其目录")
    target_dir.mkdir(parents=True, exist_ok=True)
    require(target_dir.is_dir(), "Cargo 缓存不是目录")
    return target_dir


def _validate_isolation(repository, target_dir, binaries, installed_paths=()):
    protected_paths, protected_inodes = set(), set()
    installed_paths = {Path(path) for path in installed_paths}
    installed_paths.update(repository / "target/release" / name for name in {*INSTALLED_NAMES, *binaries})
    for path in installed_paths:
        if os.path.lexists(path):
            resolved, info = path.resolve(strict=True), path.stat()
            require(stat.S_ISREG(info.st_mode), "正式 release 安装件不是普通文件")
            protected_paths.add(resolved)
            protected_inodes.add((info.st_dev, info.st_ino))
    for directory, dirs, files in os.walk(target_dir, followlinks=False):
        require(all(not (Path(directory) / name).is_symlink() for name in dirs), "Cargo 缓存含链接目录")
        for name in files:
            path = Path(directory) / name
            info = path.stat()
            require(path.resolve(strict=True) not in protected_paths
                    and (info.st_dev, info.st_ino) not in protected_inodes,
                    "Cargo 缓存链接到正式 release 安装件")
    for name in binaries:
        path = target_dir / "release" / name
        if os.path.lexists(path):
            require(stat.S_ISREG(path.lstat().st_mode), "Cargo 预期输出不能为链接或特殊文件")


def _command(runner, argv, repository, env):
    row = runner(argv, cwd=repository, env=env)
    require(row.returncode == 0, "命令失败：" + shlex.join(argv) + "\n" + row.stderr)
    return row.stdout


def _git_snapshot(runner, repository, env, source_domains=SOURCE_DOMAINS):
    git_env = {key: value for key, value in env.items() if not key.startswith("GIT_")}
    commit = _command(runner, ["git", "rev-parse", "--verify", "HEAD^{commit}"], repository, git_env).strip()
    require(re.fullmatch(r"[0-9a-f]{40}", commit), "Git HEAD 不是完整提交")
    status = _command(runner, ["git", "status", "--porcelain=v1", "--untracked-files=all", "--",
                               *source_domains], repository, git_env)
    require(not status.strip(), "编译或打包相关源码存在已跟踪或未跟踪改动：\n" + status)
    return commit


def _cargo_artifacts(stdout, entry, target_dir):
    records = [strict_json(line) for line in stdout.splitlines() if line.strip()]
    require(all(isinstance(row, dict) for row in records), "Cargo JSON 记录类型错误")
    finished = [row for row in records if row.get("reason") == "build-finished"]
    require(len(finished) == 1 and finished[0].get("success") is True, "Cargo 未唯一成功完成")
    artifacts = [row for row in records if row.get("reason") == "compiler-artifact"]
    require(all(isinstance(row.get("features"), list)
                and all(isinstance(name, str) and not name.startswith("test-") for name in row["features"])
                for row in artifacts), "生产 Cargo 闭包含测试 feature 或非法记录")
    result = {}
    expected_features = entry["_expected_features"]
    for name in entry["bins"]:
        matches = [row for row in artifacts if row.get("target", {}).get("name") == name
                   and row["target"].get("kind") == ["bin"] and row.get("executable")]
        require(len(matches) == 1, "Cargo executable 不唯一或缺失：" + name)
        row = matches[0]
        package_id = row.get("package_id", "")
        require(isinstance(package_id, str), "Cargo package 身份错误")
        package_path, separator, identity = package_id.rpartition("#")
        # 兼容 Cargo 的显式名称与旧路径名称格式，不从路径放行不同包名。
        legacy_identity = (re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.+-]+)?", identity)
                           and package_path.endswith("/" + entry["cargo_package"]))
        require(separator and (identity.split("@")[0] == entry["cargo_package"] or legacy_identity),
                "Cargo package 身份错误")
        require(sorted(row["features"]) == sorted(expected_features), "实际 Cargo feature 闭包错误")
        profile = row.get("profile", {})
        require(profile.get("opt_level") == "3" and profile.get("test") is False
                and profile.get("debug_assertions") is False and profile.get("overflow_checks") is False
                and profile.get("debuginfo") in (None, 0), "实际 Cargo profile 不是批准 release")
        executable = _absolute(row["executable"])
        require(executable.name == name and executable.parent == target_dir / "release"
                and stat.S_ISREG(executable.lstat().st_mode), "Cargo executable 不在本次原生 release 目录")
        elf_identity(executable, 183)
        result[name] = executable
    return result


def _unit_requirements(text):
    fields, section = {}, None
    text = re.sub(r"\\\n[ \t]*", " ", text)
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith(("#", ";")):
            continue
        if line.startswith("[") and line.endswith("]"):
            section = line
        elif section == "[Service]" and "=" in line:
            key, value = line.split("=", 1)
            fields.setdefault(key, []).append(value)
    require(len(fields.get("ExecStart", [])) == 1 and len(fields.get("WorkingDirectory", [])) == 1,
            "unit 缺唯一 ExecStart 或 WorkingDirectory")
    argv = shlex.split(fields["ExecStart"][0])
    require(argv, "unit ExecStart 为空")
    row = {"executable": argv[0], "working_directory": fields["WorkingDirectory"][0]}
    for option in ("--rx-trust-dir", "--latch-path", "--config", "--left-config", "--right-config", "--s0-bin"):
        positions = [index for index, value in enumerate(argv) if value == option]
        require(len(positions) <= 1, "unit 路径参数重复：" + option)
        if positions:
            require(positions[0] + 1 < len(argv), "unit 路径参数缺值")
            row[option[2:].replace("-", "_")] = argv[positions[0] + 1]
    require(all(isinstance(path, str) and PurePosixPath(path).is_absolute() for path in row.values()),
            "unit 外部路径不是绝对路径")
    return row


def _external_requirements(repository, entry):
    result = {"configs": {}, "units": {}}
    for mapping in entry["files"]:
        path = _source_file(repository, mapping["source"])
        if mapping["destination"].startswith("configs/") and path.suffix == ".toml":
            document = tomllib.loads(path.read_text())
            hardware = [{key: row[key] for key in ("id", "address", "calibration_paths") if key in row}
                        for row in document.get("hardware", [])]
            sensors = []
            for source in document.get("sensor", []):
                row = {key: source[key] for key in ("id", "python", "device", "helper") if key in source}
                if "python" in row:
                    row["venv_config"] = str(PurePosixPath(row["python"]).parent.parent / "pyvenv.cfg")
                sensors.append(row)
            result["configs"][mapping["destination"]] = {"hardware": hardware, "sensors": sensors}
        elif mapping["destination"].startswith("systemd/") and path.suffix == ".service":
            result["units"][mapping["destination"]] = _unit_requirements(path.read_text())
    return result


def _file_record(path):
    info = path.lstat()
    require(stat.S_ISREG(info.st_mode), "包内来源必须为非链接普通文件")
    return {"sha256": sha256(path), "bytes": info.st_size, "mode": stat.S_IMODE(info.st_mode)}


def _bytes_record(data, mode=0o644):
    return {"sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data), "mode": mode}


class _HashingReader:
    def __init__(self, stream):
        self.stream, self.digest = stream, hashlib.sha256()

    def read(self, size):
        data = self.stream.read(size)
        self.digest.update(data)
        return data


def _archive(path, sources, generated, files, manifest):
    created = False
    try:
        with path.open("xb") as output:
            created = True
            with gzip.GzipFile(filename="", mode="wb", fileobj=output, mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode="w|") as archive:
                    for name in sorted(files):
                        record = files[name]
                        member = tarfile.TarInfo(name)
                        member.size, member.mode, member.mtime = record["bytes"], record["mode"], 0
                        if name in generated:
                            archive.addfile(member, io.BytesIO(generated[name]))
                        else:
                            fd = os.open(sources[name], os.O_RDONLY | os.O_NOFOLLOW)
                            with os.fdopen(fd, "rb") as stream:
                                info = os.fstat(stream.fileno())
                                require(stat.S_ISREG(info.st_mode) and info.st_size == record["bytes"]
                                        and stat.S_IMODE(info.st_mode) == record["mode"], "打包来源身份变化")
                                reader = _HashingReader(stream)
                                archive.addfile(member, reader)
                                require(reader.digest.hexdigest() == record["sha256"], "打包期间来源变化：" + name)
                    data = canonical(manifest) + b"\n"
                    member = tarfile.TarInfo("manifest.json")
                    member.size, member.mode, member.mtime = len(data), 0o644, 0
                    archive.addfile(member, io.BytesIO(data))
    except BaseException:
        if created:
            path.unlink()
        raise


def build_package(repository: Path, package_id: str, target_dir: Path, output_dir: Path,
                  *, runner=run_command) -> dict:
    repository = Path(repository).resolve(strict=True)
    entry = select_package(repository, package_id)
    external = _external_requirements(repository, entry)
    installed_paths = {row[key] for row in external["units"].values()
                       for key in ("executable", "s0_bin") if key in row}
    target_dir = _isolated_target(repository, target_dir, installed_paths)
    _validate_isolation(repository, target_dir, entry["bins"], installed_paths)
    output_dir = _absolute(output_dir)
    require(not output_dir.is_relative_to(target_dir), "归档目录不能位于 Cargo 缓存内")
    output_dir.mkdir(parents=True, exist_ok=True)
    root_manifest = tomllib.loads(_source_file(repository, "Cargo.toml").read_text())
    version = root_manifest["workspace"]["package"]["version"]
    require(isinstance(version, str) and re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version), "根版本不是当前 SemVer")
    toolchain = tomllib.loads(_source_file(repository, "rust-toolchain.toml").read_text())
    require(toolchain["toolchain"]["channel"] == "nightly", "必须保留浮动 nightly 工具链")
    env, shown_env, removed_env_keys = fixed_build_environment(target_dir)
    env["RUSTUP_AUTO_INSTALL"] = "0"
    source_domains = (*SOURCE_DOMAINS, *entry["_source_domains"])
    commit = _git_snapshot(runner, repository, env, source_domains)
    release_tag = f"v{version}+{commit[:7]}"
    basename = f"robot-system-{release_tag}-{package_id}"
    paths = {"archive": output_dir / (basename + ".tar.gz"),
             "manifest_path": output_dir / (basename + ".manifest.json"),
             "build_log": output_dir / (basename + ".build.log"),
             "rustc_record": output_dir / (basename + ".rustc-vV.txt")}
    require(all(not os.path.lexists(path) for path in paths.values()), "同名归档、清单或日志已存在，拒绝覆盖")
    rustc_text = _command(runner, ["rustc", "-vV"], repository, env)
    rustc = parse_rustc_identity(rustc_text, 183)
    require(rustc["host"] == entry["target"], "必须在矩阵 target 的原生主机构建")
    with paths["rustc_record"].open("x") as stream:
        stream.write(rustc_text)
    argv = build_argv(entry)
    # 先独占创建日志；失败仍保留原 Cargo 输出，不重试未知结果。
    with paths["build_log"].open("x") as log:
        completed = runner(argv, cwd=repository, env=env)
        log.write(completed.stdout)
        log.write("\n--- Cargo stderr ---\n" + completed.stderr)
        log.write("\n退出码：" + str(completed.returncode) + "\n")
    require(completed.returncode == 0, "Cargo 构建失败，见：" + str(paths["build_log"]))
    binaries = _cargo_artifacts(completed.stdout, entry, target_dir)
    after_rustc = _command(runner, ["rustc", "-vV"], repository, env)
    require(after_rustc == rustc_text, "构建前后实际 rustc 身份变化")
    _validate_isolation(repository, target_dir, entry["bins"], installed_paths)
    sources = {"bin/" + name: path for name, path in binaries.items()}
    sources.update({row["destination"]: _source_file(repository, row["source"]) for row in entry["files"]})
    readme = (f"# robot-system 机器人控制系统候选包\n\n构型：{package_id}\n版本：{release_tag}\n提交：{commit}\n\n"
              "本包保存既有安装布局的原始配置和 systemd 模板，路径未改写。必须按既有候选冻结与预检流程"
              "核对二进制、配置、外部依赖和保护域后，才可安装；不能解包后直接运行。\n\n"
              "manifest.json 列出 calibration_paths、串口与相机设备、固定 Python、pyvenv.cfg、helper、"
              "rx-trust-dir、latch 等外部要求。校准、固定 Python/venv 与保护状态未读取或复制；"
              "helper 按矩阵归档，模板仍保留原绝对路径，须在候选冻结时重新绑定。秘密、marker 与 latch 不在包内。\n\n"
              "D15：OPEN/DEFERRED；deployable=false。构建与打包未安装 unit、未启动服务、未操作硬件。"
              "软件包不代表运行、实际动作或停止验收通过。\n").encode()
    generated = {"VERSION": (release_tag + "\n").encode(), "README.md": readme}
    files = {name: _file_record(path) for name, path in sources.items()}
    files.update({name: _bytes_record(data) for name, data in generated.items()})
    manifest = {"schema": "ROBOT_SYSTEM_PACKAGE_V1", "package_id": package_id, "version": version,
                "commit": commit, "release_tag": release_tag,
                "matrix": {key: entry[key] for key in MATRIX_KEYS},
                "build": {"argv": argv, "features": entry["_expected_features"],
                          "profile": "release", "environment": shown_env, "removed_env_keys": removed_env_keys},
                "rustc": {"argv": ["rustc", "-vV"], "identity": rustc, "version_verbose": rustc_text},
                "external_requirements": external, "files": files,
                "D15": "OPEN/DEFERRED", "deployable": False, "installed": False, "hardware_started": False}
    archive_created = False
    try:
        # 独占创建压缩流；不生成重复 staging 目录。
        _archive(paths["archive"], sources, generated, files, manifest)
        archive_created = True
        require(_git_snapshot(runner, repository, env, source_domains) == commit, "构建前后 Git HEAD 变化")
        with paths["manifest_path"].open("xb") as stream:
            stream.write(canonical(manifest) + b"\n")
    except BaseException:
        if archive_created and os.path.lexists(paths["archive"]):
            paths["archive"].unlink()
        raise
    return {**{key: str(path) for key, path in paths.items()}, "manifest": manifest}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--list", action="store_true", help="只读列出矩阵，不调用 Git、rustc 或 Cargo")
    parser.add_argument("--package", help="当前打包项 ID")
    parser.add_argument("--target-dir", type=Path, help="绝对路径的隔离 Cargo 缓存，可复用现有目录")
    parser.add_argument("--output-dir", type=Path, help="绝对路径的产物目录；拒绝同名覆盖")
    args = parser.parse_args(argv)
    if args.list:
        require(args.package is None and args.target_dir is None and args.output_dir is None,
                "--list 不能与构建选项混用")
        for package_id, entry in load_matrix(REPOSITORY).items():
            print(package_id + "\t" + entry["target"])
        return 0
    require(args.package is not None and args.target_dir is not None and args.output_dir is not None,
            "构建必须指定 --package、--target-dir 和 --output-dir")
    result = build_package(REPOSITORY, args.package, args.target_dir, args.output_dir)
    print(result["archive"])
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ValueError, OSError, KeyError, TypeError) as error:
        print("打包失败：" + str(error), file=sys.stderr)
        raise SystemExit(1)
