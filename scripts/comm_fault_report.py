"""当前 Physical H2 原报告的只读复核；不补造结果、不执行进程或硬件操作。"""
from __future__ import annotations

import csv
from dataclasses import dataclass
import hashlib
import io
import json
import re
from pathlib import Path


def require(condition, message):
    if not condition:
        raise ValueError(message)


@dataclass(frozen=True)
class ArmProfile:
    side: str
    robot_id: str
    hardware_id: str
    config: Path
    repository_config: Path
    trust: Path
    device: str
    usb_serial: str
    calibration_name: str
    evidence_directory: str
    serial_cpu: int


def arm_profile(side="left"):
    """仅允许已冻结 D2 的两侧，不接受输入文件自定义硬件映射。"""
    require(isinstance(side, str) and side in ("left", "right"), "未知臂侧")
    serial, cpu = ("5A7C121427", 4) if side == "left" else ("5A7C122340", 5)
    return ArmProfile(side, f"xlerobot_{side}", f"{side}_arm",
                      Path(f"/etc/robot/xlerobot-{side}-arm.toml"),
                      Path(f"configs/xlerobot-{side}-arm.toml"),
                      Path(f"/var/lib/robot/xlerobot_{side}"),
                      f"/dev/serial/by-id/usb-1a86_USB_Single_Serial_{serial}-if00",
                      serial, f"{side}_arm.json", f"orin_sla_{side}", cpu)


def strict_json(text):
    def pairs(items):
        value = {}
        for key, item in items:
            require(key not in value, f"JSON 重复字段：{key}")
            value[key] = item
        return value

    def invalid(value):
        raise ValueError(f"JSON 非有限常量：{value}")

    return json.loads(text, object_pairs_hook=pairs, parse_constant=invalid)


def integer(value, label, minimum=0):
    require(type(value) is int and value >= minimum, f"{label} 必须为整数且 >= {minimum}")
    return value


def nonempty(value, label):
    require(isinstance(value, str) and value.strip(), f"{label} 为空")
    return value


def distribution(values):
    require(values, "分布没有原始值")
    ordered = sorted(values)
    return {"count": len(values), "p50": ordered[(len(values) * 50 + 99) // 100 - 1],
            "p99": ordered[(len(values) * 99 + 99) // 100 - 1], "max": ordered[-1]}


def verify_ack(ack, request_id, dry_run):
    require(ack.get("ok") is True and ack.get("kind") == "ack", "未取得真实成功 Ack")
    require(ack.get("request_id") == nonempty(request_id, "请求 ID"), "Ack 请求身份错误")
    require(ack.get("dry_run") is dry_run, "Ack dry_run 错误")
    integer(ack.get("cleared_at_ns"), "Ack 完成戳", 1)


def verify_final_stop(value, boundaries):
    require(value.get("ok") is True, "final_stop 未成功")
    require(re.fullmatch(r"Ok\([1-9][0-9]*\)", value.get("estop", "")), "ExternalEStop 未成功")
    disarm = value.get("disarmed", "")
    match = re.fullmatch(r"Ok\(\(([1-9][0-9]*), BoundarySnapshot \{ .+ \}\)\)", disarm)
    require(match, "Disarm 缺真实成功回执")
    require("failure: None" in disarm and "status: Disarmed" in disarm
            and "armed_seq: 0," in disarm, "Disarm 含残余或 sticky failure")
    last = boundaries[-1]
    require(last["mode"] == "Disarm" and last["sequence"] == int(match[1]),
            "最终 Disarm 与原 boundary 不对应")
    state = value.get("get_state", {})
    require(state.get("attempted") is True and state.get("ok") is True
            and state.get("safety_state") == 3, "未取得真实 GetState 状态3")
    integer(state.get("state_stamp_ns"), "GetState 时间戳", 1)
    disable = value.get("disable", {})
    require(disable.get("attempted") is True, "Disable 未执行")
    request = disable.get("request", {})
    require(request.get("enabled") is False and request.get("dry_run") is False,
            "最终请求不是 live Disable")
    nonempty(request.get("idempotency_key"), "Disable 幂等键")
    verify_ack(disable, request.get("request_id"), False)
    start = integer(disable.get("started_ns"), "Disable 开始", 1)
    require(integer(disable.get("finished_ns"), "Disable 结束", 1) >= start, "Disable 时间倒序")


def verify_final_trace(value, boundaries):
    """最终 child stop 后的测量有效性门；不替代真实 final_stop 安全回执。"""
    require(value["valid"] is True and value["children_stopped"] is True
            and value["installed"] is True and value["error"] is None
            and value["read_error"] is None, "最终 trace 未确认有效/停止/安装")
    require(integer(value["dropped"], "最终丢事件") == 0, "最终 trace 丢事件")
    snap = value["boundary"]
    require(isinstance(snap, dict), "最终采样边界未知")
    sequence = integer(snap["request_seq"], "最终请求序号", 1)
    require(integer(snap["receipt_seq"], "最终回执序号", 1) == sequence
            and sequence == boundaries[-1]["sequence"] and boundaries[-1]["mode"] == "Disarm",
            "最终 trace 与最终 Disarm 不对应")
    require(snap["request_mode"] == snap["mode"] == "Some(Disarm)"
            and snap["status"] == "Disarmed" and snap["gate_active"] is False
            and snap["failure"] == "None", "最终采样边界含未知/残余/sticky状态")
    require(integer(snap["armed_seq"], "最终armed序号") == 0
            and integer(snap["failure_seq"], "最终失败序号") == 0, "最终采样仍armed或锁存失败")


def verify_rt(value, tool_pid, side="left"):
    profile = arm_profile(side)
    require(value.get("passed") is True, "PX-56 未通过")
    require(value.get("profile") == {"serial_cpu": profile.serial_cpu, "serial_policy": 1,
            "serial_priority": 70, "ordinary_policy": 0, "ordinary_priority": 0,
            "ordinary_cpus": [0, 1, 2, 3]}, "PX-56 所选臂 profile 改变")
    require(0 < value["started_ns"] <= value["finished_ns"], "PX-56 时间错误")
    rows = value.get("processes", [])
    require(len(rows) == 3 and {r["role"] for r in rows} == {"tool", "s0", "gateway"},
            "PX-56 进程范围错误")
    pids = {}
    for row in rows:
        require(row.get("validation_error") is None, "PX-56 有原始错误")
        snapshot = row["snapshot"]
        pid = integer(snapshot["pid"], "PX-56 PID", 1)
        pids[row["role"]] = pid
        tids = snapshot["tids_before"]
        require(snapshot["errors"] == [] and tids and tids == sorted(set(tids))
                and tids == snapshot["tids_after"] and pid in tids, "TID 清单不完整/不稳定")
        threads = snapshot["threads"]
        require(sorted(t["tid"] for t in threads) == tids, "TID 与调度行不对应")
        serial = []
        for thread in threads:
            nonempty(thread["comm"], "线程名")
            is_serial = row["role"] == "s0" and thread["comm"] == "s0_serial_io_th"
            expected = (1, 70, [profile.serial_cpu]) if is_serial else (0, 0, [0, 1, 2, 3])
            require((thread["policy"], thread["priority"], thread["cpus"]) == expected,
                    "实际线程调度不满足 PX-56")
            if is_serial:
                require(thread["tid"] != pid, "主线程冒充串口线程")
                serial.append(thread["tid"])
        require(len(serial) == (1 if row["role"] == "s0" else 0), "串口线程数量错误")
    require(len(set(pids.values())) == 3 and pids["tool"] == tool_pid, "PX-56 PID 身份错误")
    return pids


def verify_background(value):
    require(value.get("valid_responses") is True and value.get("valid_cadence") is True,
            "背景请求/节拍未通过")
    require(value.get("first_failure") is None and value.get("failures") == []
            and value.get("admission_rejection") is None and value.get("unfinished") == [],
            "背景存在错误/未完成项")
    for key, expected in {"rr_target_per_second": 20, "tracked_limit": 64,
                          "client_timeout_ns": 2_000_000_000, "drain_budget_ns": 2_500_000_000,
                          "sigma_probe_period_ns": 5_000_000}.items():
        require(value.get(key) == expected, f"背景预算改变：{key}")
    require(1 <= value["peak_tracked"] <= 64, "背景并发计数错误")
    start, stop, drained, end = (integer(value[key], key, 1) for key in
                               ("started_ns", "stop_ns", "drained_ns", "finished_ns"))
    require(start < stop <= drained <= end and drained - stop <= 2_500_000_000, "背景 drain 超预算")
    requests = value["rr_requests"]
    require(len(requests) >= 2 and value["launched"] == value["completed"]
            == value["successful_unique_writes"] == len(requests), "背景原始行与计数不一致")
    keys, ids, starts = set(), set(), []
    for index, row in enumerate(requests):
        require(row["sequence"] == index and row["due_ns"] == start + index * 50_000_000,
                "背景绝对 due 序列错误")
        require(row["ok"] is True and row["terminal"] is True, "背景有失败原始行")
        sent = integer(row["started_ns"], "背景首poll", 1)
        require(row["due_ns"] <= sent < row["due_ns"] + 50_000_000 and sent <= stop,
                "背景迟发或 stop 后新增")
        require(row["deadline_ns"] == sent + 2_000_000_000
                and sent <= row["finished_ns"] <= min(row["deadline_ns"], drained), "背景响应超预算")
        verify_ack(row["response"], row["request_id"], True)
        key = nonempty(row["idempotency_key"], "背景幂等键")
        require(key not in keys and row["request_id"] not in ids, "背景请求身份重复")
        keys.add(key); ids.add(row["request_id"]); starts.append(sent)
    require(value["rr_gap_ns"] == distribution([b-a for a,b in zip(starts, starts[1:])]),
            "背景间隔分布与原始行不同")
    sigma = value["sigma_raw_ns"]
    require(sigma and all(type(n) is int and n >= 0 for n in sigma)
            and value["sigma_est_ns"] == distribution(sigma), "调度延迟分布缺失/不一致")
    return keys, ids


def verify_boundaries(rows):
    require(rows, "缺 boundary 原件")
    seen = set()
    for row in rows:
        seq = integer(row["sequence"], "boundary序号", 1)
        require(seq not in seen and row.get("error") is None, "boundary 重复或错误")
        seen.add(seq)
        snap = row["snapshot"]
        mode = row["mode"]
        require(mode in ("ArmIdle", "ArmInflight", "Disarm"), "未知 boundary mode")
        require(snap["request_seq"] == snap["receipt_seq"] == seq
                and snap["request_mode"] == snap["mode"] == f"Some({mode})"
                and snap["failure"] == "None", "boundary 不完整或 sticky failure")
        require(0 < row["started_ns"] <= snap["ready_ns"] < snap["request_deadline_ns"]
                and row["finished_ns"] >= snap["ready_ns"], "boundary 截止不符")
        require((snap["status"] == "Disarmed" and snap["armed_seq"] == 0) if mode == "Disarm"
                else (snap["status"] == "Ready" and snap["armed_seq"] == seq), "boundary未真实完成")
    return {r["sequence"]: r for r in rows}


def verify_trace(trace, row):
    """复核 Rust analyze 的物理因果链、真实 lease 与 1–5ms 载荷原件。"""
    require(trace and [e["stamp_ns"] for e in trace] == sorted(e["stamp_ns"] for e in trace), "trace 时间未排序")
    def unique(kind, success=False):
        values = [e for e in trace if e["kind"] == kind and (not success or e["b"] == 1)]
        require(len(values) == 1, f"trace {kind} 不唯一/缺失")
        return values[0]
    start_kind = "ExternalEnter" if row["external"] else "TopicReceived"
    start, primitive, serial, ipc, commit, off = (unique(start_kind), unique("PrimitiveCommitted"),
        unique("SerialClaim", True), unique("IpcClaim", True), unique("IpcEStopCommit"), unique("AllOffTrusted"))
    require((start["stamp_ns"], primitive["stamp_ns"], off["stamp_ns"]) ==
            (row["t0_ns"], row["t1_ns"], row["all_off_ns"]), "trace与采样时间不对应")
    require(primitive["b"] <= 1, "Merged/LatchOnly不是物理断扭")
    faults = {"CommErrorObserved", "FirstCommError", "MailboxPublishing", "MailboxPublished", "FaultIpcCommit"}
    bool_kinds = {"NormalAcquire", "NormalRelease", "SerialClaim", "IpcClaim", "SafetyReturn", "ConfirmReturn"}
    cas_kinds = {"NormalAcquire", "NormalRelease", "SerialClaim", "IpcClaim", "PrimitiveCommitted"}
    allowed = bool_kinds | cas_kinds | {start_kind, "IpcEStopCommit", "AllOffTrusted", "SafetyStart", "InjectedDelay"}
    for event in trace:
        require(event["kind"] in allowed and event["kind"] not in faults
                and event["sample_seq"] == row["sequence"] and event["attempt"] == row["attempt"]
                and event["external"] == str(row["external"]).lower(), "trace混入故障/未知事件/另一轮")
        require(event["kind"] == start_kind or event["generation"] == primitive["generation"], "trace代次拼接")
        require(event["kind"] not in bool_kinds or event["b"] <= 1, "trace成功标志非bool")
        require(event["kind"] not in cas_kinds or event["a"] < 2**64-1, "CAS缺失或耗尽")
    active, first, delivered, last_confirm, count, attempts = None, None, None, None, 0, 0
    for event in trace:
        kind, stamp = event["kind"], event["stamp_ns"]
        if kind == "SafetyStart":
            require(active is None and delivered is None and event["a"] == attempts+1 <= 3, "Safety调用序列错误")
            active = event; attempts += 1
            if first is None: first = stamp
        elif kind == "SafetyReturn":
            require(active is not None and active["a"] == event["a"] and active["stamp_ns"] <= stamp, "Safety没有配对")
            active = None
            if event["b"] == 1: delivered = stamp
        elif kind == "ConfirmReturn":
            require(delivered is not None and delivered <= stamp <= off["stamp_ns"]
                    and (last_confirm is None or last_confirm < stamp) and event["a"] <= 32 and count < 5, "Confirm调用非法")
            require((event["a"] == 0 and event["b"] == 1) == (stamp == off["stamp_ns"]), "全关Confirm不对应")
            last_confirm = stamp; count += 1
    require(active is None and delivered is not None and last_confirm == off["stamp_ns"] and first is not None, "缺完整物理断扭证据")
    require(start["stamp_ns"] <= primitive["stamp_ns"] <= serial["stamp_ns"] <= first
            and primitive["stamp_ns"] <= ipc["stamp_ns"] <= commit["stamp_ns"], "物理因果顺序错误")
    acquired, lease, active_at_commit = None, None, False
    for event in trace:
        if event["stamp_ns"] >= off["stamp_ns"] or event["b"] != 1:
            continue
        if event["kind"] == "NormalAcquire":
            require(acquired is None and event["stamp_ns"] < primitive["stamp_ns"], "normal重复或原语后新启")
            acquired = event
        elif event["kind"] == "NormalRelease":
            require(event["stamp_ns"] <= first, "Safety开始时normal未退")
            if acquired is not None:
                require(acquired["normal_seq"] == event["normal_seq"], "lease序号错误")
                if acquired["stamp_ns"] <= start["stamp_ns"] < event["stamp_ns"]:
                    lease = (acquired, event)
                active_at_commit |= acquired["stamp_ns"] <= primitive["stamp_ns"] < event["stamp_ns"]
                acquired = None
            else:
                require(event["stamp_ns"] <= start["stamp_ns"], "lease缺acquire")
    require(acquired is None and bool(primitive["b"]) == active_at_commit, "ACTIVE与完整lease不符")
    require(row["matched"] is ((lease is not None) == row["requested_in_flight"]), "matched与真实lease分类不符")
    if row["requested_in_flight"] and lease is not None:
        begin, end = lease
        delays = [e for e in trace if e["kind"] == "InjectedDelay" and
                  (begin["stamp_ns"] <= e["stamp_ns"] <= end["stamp_ns"] or
                   e["stamp_ns"] < end["stamp_ns"] and e["stamp_ns"]+e["b"] > begin["stamp_ns"])]
        require(len(delays) == 1, "在途载荷缺失或重复")
        delay = delays[0]
        require(delay["normal_seq"] == begin["normal_seq"] and 1_000_000 <= delay["a"] <= 5_000_000
                and delay["b"] >= delay["a"] and begin["stamp_ns"] <= delay["stamp_ns"]
                and delay["stamp_ns"] + delay["b"] <= end["stamp_ns"], "1–5ms真实载荷越界或身份不符")


def verify_report(directory: Path, expected: dict) -> dict:
    """重新读取当前报告/原件并逐项核验；expected 来自已复核的运行清单。"""
    directory = Path(directory)
    profile = arm_profile(expected.get("side", "left"))
    require(expected["config"] == str(profile.config), "期待配置与所选侧不符")
    evidence = directory / profile.evidence_directory
    hashes = {}

    def read(path):
        require(path.is_file() and not path.is_symlink(), f"原件缺失或非普通文件：{path}")
        content = path.read_bytes()
        require(content, f"原件为空：{path}")
        hashes[str(path.relative_to(directory))] = hashlib.sha256(content).hexdigest()
        return content.decode()

    def json_file(name):
        return strict_json(read(evidence / name))

    report = strict_json(read(directory / "report.json"))
    require(report["schema_version"] == 1 and report["status"] == "PASS"
            and report["hardware"] == {"H1": "NEEDS_HUMAN", "H2": "PASS",
                                       "H3": "NEEDS_HUMAN", "D15": "OPEN"}, "不是完整 H2 PASS 原报告")
    invocation = report["invocation"]
    require(invocation["phase"] == "OrinSla" and invocation["case_filter"] is None
            and invocation["hardware_id"] == profile.hardware_id and invocation["config"] == expected["config"]
            and invocation["config_sha256"] == expected["config_sha256"], "报告调用身份错误")
    bin_dir = Path(expected["bin_dir"])
    require(invocation["current_exe"] == str(bin_dir / "comm_fault_e2e")
            and invocation["current_exe_sha256"] == expected["artifacts"]["comm_fault_e2e"]["sha256"],
            "报告 current_exe 错误")
    artifacts = report["artifacts"]
    require(len(artifacts) == 3 and {a["name"] for a in artifacts} == set(expected["artifacts"]),
            "报告三件身份不完整")
    for artifact in artifacts:
        actual = expected["artifacts"][artifact["name"]]
        require(artifact["path"] == str(bin_dir / artifact["name"])
                and artifact["sha256"] == actual["sha256"] and artifact["build"] == actual["build"],
                "报告产物与已冻结清单不同")
    cases = report["cases"]
    require(len(cases) == 1 and cases[0]["case"] == "orin_sla"
            and cases[0]["ac"] == "AC-12/H2" and cases[0]["status"] == "PASS", "H2 场景错误")
    result = cases[0]["evidence"]
    environment = json_file("environment.json")
    require(result["environment"] == environment, "环境原件与报告不同")
    for key, value in {"gateway_idempotency_max_entries": 8192, "gateway_idempotency_ttl_secs": 300,
                       "background_tracked_limit": 64, "wire_active_limit_unchanged": 4,
                       "s48_cache_observation": False, "hardware_id": profile.hardware_id,
                       "address": profile.device,
                       "current_exe": str(bin_dir / "comm_fault_e2e"),
                       "config_sha256": expected["config_sha256"],
                       "calibration_sha256": expected["calibration_sha256"]}.items():
        require(environment.get(key) == value, f"Physical 环境字段错误：{key}")
    require(environment["artifacts"] == {role: expected["artifacts"][name]["build"] for role, name in
            (("tool", "comm_fault_e2e"), ("s0", "robot-s0"), ("gateway", "robot-gateway"))},
            "Physical 预检产物与报告不同")
    gateway_args = environment["gateway_arguments"]
    for flag, value in (("--idempotency-max-entries", "8192"), ("--idempotency-ttl-secs", "300")):
        require(gateway_args.count(flag) == 1 and gateway_args[gateway_args.index(flag) + 1] == value,
                "Gateway 实际参数未保原容量/TTL")
    rt = json_file("rt_preflight.json")
    require(rt == result["rt_preflight"], "RT 原件与报告不同")
    pids = verify_rt(rt, expected["tool_pid"], profile.side)
    boundaries = [strict_json(line) for line in read(evidence / "boundary.jsonl").splitlines()]
    boundary_index = verify_boundaries(boundaries)
    final = json_file("final_stop.json")
    require(final == result["final_state"], "final_stop 原件与报告不同")
    verify_final_stop(final, boundaries)
    final_trace = json_file("final_trace.json")
    require(json.dumps(final_trace, sort_keys=True) == json.dumps(result["final_trace"], sort_keys=True),
            "final_trace 原件与报告不同")
    verify_final_trace(final_trace, boundaries)
    measurement = result["measurement"]
    for key, value in {"trace_dropped": 0, "hardware_only": True, "runtime_only": False,
                       "in_flight_exercised": True, "mock_lease": False,
                       "external_entry_exercised": True, "topic_entry_exercised": True,
                       "idle_exercised": True, "events_csv": str(evidence / "events.csv")}.items():
        require(measurement.get(key) == value, f"Physical 测量字段错误：{key}")
    background = json_file("background.json")
    require(background == measurement["background"], "背景原件与报告不同")
    keys, ids = verify_background(background)
    attempts = [strict_json(line) for line in read(evidence / "attempts.jsonl").splitlines()]
    require(attempts, "没有采样原件")
    require(len(boundaries) == 2 * len(attempts) + 1, "逐次Arm/Disarm原件缺失或增项")
    for index, attempt in enumerate(attempts):
        armed, disarmed = boundaries[2*index:2*index+2]
        require(armed["sequence"] == attempt["sequence"] and disarmed["mode"] == "Disarm"
                and armed["finished_ns"] <= attempt["t0_ns"] <= attempt["all_off_ns"] <= disarmed["started_ns"],
                "逐次Arm/Disarm次序或物理收尾不对应")
    groups = measurement["groups"]
    expected_groups = [(False, False), (False, True), (True, False), (True, True)]
    require([(g["external"], g["in_flight"]) for g in groups] == expected_groups, "四组缺失/重复/乱序")
    events = {}
    raw = csv.DictReader(io.StringIO(read(evidence / "events.csv")))
    require(raw.fieldnames == ["attempt", "external", "sample_seq", "stamp_ns", "kind", "source",
                              "normal_seq", "generation", "a", "b"], "原始 trace 列错误")
    for row in raw:
        for key in ("attempt", "sample_seq", "stamp_ns", "source", "normal_seq", "generation", "a", "b"):
            row[key] = integer(int(row[key]), f"trace {key}")
        require(row["external"] in ("true", "false"), "trace 入口类型错误")
        events.setdefault(row["sample_seq"], []).append(row)
    seen_sequences, ordered_groups = set(), []
    for group, identity in zip(groups, expected_groups):
        require(json_file(f"sla_{str(identity[0]).lower()}_{str(identity[1]).lower()}.json") == group,
                "组原件与报告不同")
        rows = [r for r in attempts if (r.get("external"), r.get("requested_in_flight")) == identity]
        require(1100 <= len(rows) <= 11000, "组尝试数越界")
        accepted, a_values, b_values, retries = 0, [], [], []
        cas = {"NormalAcquire": [], "SerialClaim": [], "IpcClaim": []}
        for index, row in enumerate(rows):
            require(row.get("invalid") is not True and row["attempt"] == index
                    and row["warmup"] is (accepted < 100) and type(row["matched"]) is bool, "采样尝试非法")
            seq = integer(row["sequence"], "采样序号", 1)
            require(seq not in seen_sequences and seq in boundary_index, "采样序号重复/缺边界")
            seen_sequences.add(seq); ordered_groups.append(identity)
            boundary = boundary_index[seq]
            require(boundary["mode"] == ("ArmInflight" if identity[1] else "ArmIdle"), "采样边界类型不符")
            require(rt["finished_ns"] <= row["t0_ns"] <= row["t1_ns"] <= row["all_off_ns"], "采样时序/PX-56顺序错误")
            trace = events.get(seq, [])
            verify_trace(trace, row)
            require(all(e["external"] == str(identity[0]).lower() for e in trace), "trace 混入另一入口")
            for kind, stamp in [("ExternalEnter" if identity[0] else "TopicReceived", row["t0_ns"]),
                                ("PrimitiveCommitted", row["t1_ns"]), ("AllOffTrusted", row["all_off_ns"])]:
                matching = [e for e in trace if e["kind"] == kind]
                require(len(matching) == 1 and matching[0]["stamp_ns"] == stamp, "采样与真实 trace 不对应")
            if row["matched"]:
                if accepted >= 100:
                    a_values.append(row["t1_ns"] - row["t0_ns"])
                    b_values.append(row["all_off_ns"] - row["t0_ns"])
                    retries.append(next(e["a"] for e in trace if e["kind"] == "PrimitiveCommitted"))
                    for kind, values in cas.items():
                        values.extend(e["a"] for e in trace if e["kind"] == kind)
                accepted += 1
        require(accepted == 1100 and group["warmup"] == 100 and group["samples"] == 1000
                and group["class_mismatch_retries"] == len(rows) - 1100 and group["passed"] is True,
                "原100预热+1000样本不完整")
        require(group["sla_a_ns"] == distribution(a_values) and group["sla_b_ns"] == distribution(b_values)
                and group["primitive_cas_retries"] == distribution(retries), "组分布与原样本不同")
        for kind, field in (("NormalAcquire", "acquire_cas_retries"), ("SerialClaim", "serial_claim_cas_retries"),
                            ("IpcClaim", "ipc_claim_cas_retries")):
            require(group[field] == (distribution(cas[kind]) if cas[kind] else None), "CAS 分布与原trace不同")
        target = 75_000_000 if identity[1] else 20_000_000
        require(group["sla_b_target_ns"] == target and group["sla_a_ns"]["p99"] <= 2_000_000
                and group["sla_b_ns"]["p99"] <= target, "原 P99 预算未满足")
    require(set(events) == seen_sequences, "trace混入未采样epoch")
    require(len(seen_sequences) == len(attempts)
            and [(r["external"], r["requested_in_flight"]) for r in attempts] == ordered_groups,
            "尝试原件混入未知组或顺序改变")
    require(background["started_ns"] <= attempts[0]["t0_ns"]
            and attempts[-1]["all_off_ns"] <= background["stop_ns"], "背景窗口未覆盖完整测量")
    recoveries = json_file("recovery_writes.json")
    require(len(recoveries) == 2 * len(attempts), "恢复请求与尝试数量不对应")
    for index, row in enumerate(recoveries):
        require(row["method"] == ("ClearEStop" if index % 2 == 0 else "SetEnabled(true)")
                and row["ok"] is True and rt["finished_ns"] <= row["started_ns"] <= row["finished_ns"]
                <= attempts[index // 2]["t0_ns"], "恢复写失败/顺序错误")
        if index % 2:
            require(row["started_ns"] >= recoveries[index-1]["finished_ns"], "Clear/Enable次序错误")
        else:
            previous = attempts[index//2-1]["all_off_ns"] if index else rt["finished_ns"]
            require(row["started_ns"] >= previous + 600_000_000, "恢复600ms窗口缩短")
        verify_ack(row["response"], row["request_id"], False)
        require(row["idempotency_key"] not in keys and row["request_id"] not in ids, "真实新写身份重复")
        keys.add(nonempty(row["idempotency_key"], "恢复幂等键")); ids.add(row["request_id"])
    require(final["disable"]["request"]["idempotency_key"] not in keys
            and final["disable"]["request"]["request_id"] not in ids, "最终 Disable 重用了旧身份")
    for name in ("s0.stdout.log", "s0.stderr.log", "gateway.stdout.log", "gateway.stderr.log"):
        path = evidence / name
        require(path.is_file() and not path.is_symlink(), f"缺进程日志：{name}")
        hashes[str(path.relative_to(directory))] = hashlib.sha256(path.read_bytes()).hexdigest()
    return {"status": "PASS", "groups": 4, "samples": 4000, "warmup": 400,
            "attempts": len(attempts), "pids": pids, "original_hashes": hashes,
            "child_stop_basis": "原始PASS仅在真实gateway/S0 stop各退出0后生成；另核外层PID清场",
            "deployable": False, "D15": "OPEN"}
