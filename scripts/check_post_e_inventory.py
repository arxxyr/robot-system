#!/usr/bin/env -S uv run --no-project python
"""对比实现前 Cargo 清单，防止删用例、新增忽略或零测试被误记为回归通过。

只验证用例保留，不把 --list 输出当作执行通过。输入必须来自完整成功的
cargo test --workspace --all-features --tests -- --list 及 -- --ignored --list。
"""

import argparse
import hashlib
import json
import re
from pathlib import Path


def read_inventory(path: Path) -> dict[str, set[str]]:
    """按实际运行的 integration 二进制划分，拒绝重复或未结束的清单。"""
    result: dict[str, set[str]] = {}
    active: str | None = None
    finished = True
    for raw in path.read_text().splitlines():
        line = raw.strip()
        if line.startswith("Running "):
            if not finished:
                raise ValueError(f"{path}: {active} 没有完整清单结尾")
            active = None
            match = re.match(r"Running tests/([^ ]+) \(", line)
            if match:
                active = Path(match.group(1)).stem
                if active in result:
                    raise ValueError(f"{path}: target {active} 重复，无法唯一归属")
                result[active] = set()
                finished = False
            continue
        if active is None:
            continue
        if line.endswith(": test"):
            name = line.removesuffix(": test")
            if name in result[active]:
                raise ValueError(f"{path}: {active}::{name} 重复")
            result[active].add(name)
        elif match := re.fullmatch(r"(\d+) tests?, (\d+) benchmarks?", line):
            if len(result[active]) != int(match.group(1)):
                raise ValueError(f"{path}: {active} 清单数量与 libtest 汇总不符")
            finished = True
            active = None
    if not finished:
        raise ValueError(f"{path}: {active} 清单被截断")
    if not result:
        raise ValueError(f"{path}: 没有 integration target，不能视为通过")
    return result


def validate_baseline(baseline: dict) -> None:
    """基线必须完整且唯一，不能以空基线绕过覆盖保留门。"""
    targets = baseline.get("targets")
    if not isinstance(targets, list) or not targets:
        raise ValueError("baseline.targets 必须是非空列表")
    seen = set()
    count = 0
    ignored_count = 0
    for target in targets:
        if not isinstance(target, dict):
            raise ValueError("baseline target 必须是对象")
        name = target.get("target")
        if not isinstance(name, str) or not name or name in seen:
            raise ValueError("baseline target 名称为空或重复")
        seen.add(name)
        names = target.get("tests")
        ignored = target.get("baseline_ignored_tests")
        if (not isinstance(names, list) or not names
                or any(not isinstance(name, str) or not name for name in names)
                or len(set(names)) != len(names)):
            raise ValueError(f"baseline {name} 用例列表为空或非法")
        if (not isinstance(ignored, list)
                or any(not isinstance(name, str) for name in ignored)
                or len(set(ignored)) != len(ignored)
                or not set(ignored).issubset(names)):
            raise ValueError(f"baseline {name} ignored 列表非法")
        count += len(names)
        ignored_count += len(ignored)
    if count != baseline.get("integration_count") or ignored_count != baseline.get("ignored_integration_count"):
        raise ValueError("baseline 汇总数量与具名清单不符")


def compare(baseline: dict, current: dict, ignored: dict) -> list[dict]:
    validate_baseline(baseline)
    failures = []
    known_ignored = {}
    for target in baseline["targets"]:
        name = target["target"]
        expected = set(target["tests"])
        allowed = set(target["baseline_ignored_tests"])
        known_ignored[name] = allowed
        for collection, label in [(current, "完整"), (ignored, "ignored")]:
            if name not in collection:
                failures.append({"target": name, "reason": f"缺少{label}清单"})
        missing = sorted(expected - current.get(name, set()))
        if missing:
            failures.append({"target": name, "reason": "原用例丢失", "tests": missing})
    for name, names in ignored.items():
        added = sorted(names - known_ignored.get(name, set()))
        if added:
            failures.append({"target": name, "reason": "新增 ignored", "tests": added})
        absent = sorted(names - current.get(name, set()))
        if absent:
            failures.append({"target": name, "reason": "两份清单不一致", "tests": absent})
    return failures


def main() -> int:
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, default=root / ".verify/post_e_comm_fault_hardening/baseline.json")
    parser.add_argument("--current-list", type=Path, required=True)
    parser.add_argument("--current-ignored", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        baseline = json.loads(args.baseline.read_text())
        current = read_inventory(args.current_list)
        ignored = read_inventory(args.current_ignored)
        failures = compare(baseline, current, ignored)
    except (OSError, ValueError, KeyError) as error:
        parser.exit(2, f"清单校验无法完成：{error}\n")
    report = {
        "scope": "仅检查用例保留与新增 ignored；不证明测试执行通过",
        "status": "FAIL" if failures else "PASS",
        "baseline_commit": baseline["baseline_commit"],
        "baseline_targets": len(baseline["targets"]),
        "current_targets": len(current),
        "current_tests": sum(map(len, current.values())),
        "current_ignored": sum(map(len, ignored.values())),
        "inputs": {
            str(path): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in [args.baseline, args.current_list, args.current_ignored]
        },
        "failures": failures,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(f"清单保留：{report['status']}；{report['current_targets']} targets / "
          f"{report['current_tests']} 用例 / {report['current_ignored']} ignored")
    return int(bool(failures))


if __name__ == "__main__":
    raise SystemExit(main())
