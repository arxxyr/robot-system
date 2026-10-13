"""爸爸，固定右9的只读反馈原帧诊断；默认仅计划，任何未知或非零停止值均封锁后续UART。"""

import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import stat
import sys
import time
from types import SimpleNamespace

sys.dont_write_bytecode = True
RUN_ID = "base-feedback-diagnostic-r2"
TARGET = ("right", 9, "base_right_wheel")
SOURCE_SHA = "d97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41"
HELPER_SHA = "70d777bd5c2a19e2ce3f57e68f9abd40eaed2b8a9aa8c592e7fe53a940f1e38c"
HELPER_BYTES = 12220
FIELD_SCOPE = "仅右9/base_right_wheel在TE0/GV0下500ms内0/200/400ms三组TE0→PP56→PV58只读原帧及原STOP51；无WRITE/Enable，三轮已离地且底盘稳定支撑"
EXPECTED = {
    "Model_Number": 777, "Firmware_Major_Version": 3, "Firmware_Minor_Version": 10,
    "Operating_Mode": 1, "Phase": 12, "Lock": 1, "Goal_Velocity": 0,
    "Torque_Enable": 0,
}
PREFLIGHT_REGS = tuple(reg for reg in EXPECTED if reg != "Torque_Enable")
GROUP_REGS = ("Torque_Enable", "Present_Position", "Present_Velocity")
BUDGET_NS = 500_000_000
SAMPLE_COUNT = 3
SAMPLE_OFFSETS_NS = (0, 200_000_000, 400_000_000)
TTL_NS = 600_000_000_000


def require(value, reason):
    if not value:
        raise ValueError(reason)


def reference(path):
    path = Path(path).resolve()
    raw = path.read_bytes()
    return {"path": str(path), "sha256": hashlib.sha256(raw).hexdigest(), "bytes": len(raw)}


def load_frozen(path, sha, size, name):
    path = Path(path)
    require(path.is_absolute() and path == path.resolve() and stat.S_ISREG(path.lstat().st_mode), "冻结源码路径身份错误")
    ref = reference(path)
    require(ref["sha256"] == sha and (size is None or ref["bytes"] == size), "冻结源码SHA/bytes改变")
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module, ref


def prepare_helper(h):
    # 只修改本次加载的模块实例；冻结文件和原META字典均不写回。
    h.META = dict(h.META)
    h.META["Present_Position"] = (56, 2)


def validate_plan(m, plan):
    require(plan["run_id"] == RUN_ID and plan["position_targets"] == [], "仅本轮空位置目标只读诊断")
    deferred = [{"side": side, "motor": motor, "name": name, "decision": "DEFERRED",
                 "reason_code": "BASE_FIRMWARE_CONTRACT_UNVERIFIED"} for side, motor, name in m.BASE_TARGETS]
    require(plan["base_targets"] == deferred and plan.get("firmware_contract_verified") is False, "三轮合同必须仍DEFERRED")
    trial = plan["feedback_diag"]
    require(trial["target"] == list(TARGET) and trial["scope"] == FIELD_SCOPE
            and trial["expected_raw"] == EXPECTED and trial["budget_ns"] == BUDGET_NS
            and trial["sample_offsets_ns"] == list(SAMPLE_OFFSETS_NS)
            and trial["group_read_sequence"] == list(GROUP_REGS)
            and trial["requested_groups"] == SAMPLE_COUNT and trial["minimum_in_budget_pv0_groups"] == SAMPLE_COUNT
            and trial["write_sequence"] == [] and trial["present_position_address"] == 56
            and trial["present_position_width"] == 2 and trial["firmware_contract_verified"] is False,
            "只读right9/原帧/三组/500ms合同不得变化")
    return []


def validate_feedback_go(m, original, go, plan, now):
    validate_plan(m, plan)
    original(go, plan, now)
    require(0 < go["expires_at_ns"] - go["issued_at_ns"] <= TTL_NS, "本轮GO最多600秒")
    facts = go["physical"]["right"]
    require(facts.get("all_three_wheels_off_ground") is True and facts.get("base_stably_supported") is True
            and facts.get("base_confirmation_kind") == "CURRENT_SETUP_READY"
            and isinstance(facts.get("base_confirmation_answer"), str) and facts["base_confirmation_answer"].strip()
            and facts.get("base_confirmation_scope") == FIELD_SCOPE, "必须本次架空就绪及明确只读范围；旧GO不可复用")
    require(type(facts["confirmed_at_ns"]) is int and 0 <= now - facts["confirmed_at_ns"] <= TTL_NS, "本次现场确认已过期或来自未来")


def make_plan(m, h, bindings, output, artifact_root, own_ref, helper_ref):
    prepare_helper(h)
    h.required_references(bindings, own_ref)
    require(any(h.same_reference(row, helper_ref) for row in bindings["refs"]), "冻结raw辅助未入本轮refs")
    plan = m.make_plan(bindings, output, RUN_ID, artifact_root, ["head-2"])
    plan.update(position_targets=[], firmware_contract_verified=False, scope=FIELD_SCOPE)
    plan["feedback_diag"] = {
        "target": list(TARGET), "scope": FIELD_SCOPE, "wrapper": own_ref, "raw_helper": helper_ref,
        "raw_tool": h.RAW_TOOL, "read_tool": h.TOOL, "strace": h.STRACE,
        "expected_raw": dict(EXPECTED), "budget_ns": BUDGET_NS,
        "sample_offsets_ns": list(SAMPLE_OFFSETS_NS), "requested_groups": SAMPLE_COUNT,
        "minimum_in_budget_pv0_groups": SAMPLE_COUNT, "group_read_sequence": list(GROUP_REGS),
        "write_sequence": [], "present_position_address": 56, "present_position_width": 2,
        "firmware_contract_verified": False,
        "limitations": ["PP只保存原unsigned/完整帧，不解码符号、单位、量化或推断物理位移/停止",
                        "三组原PV0不证明连续500ms物理静止；无补读、重试或动作",
                        "所有WRITE/Enable/Phase/Lock/模式/EEPROM及其他轮主动操作均禁止"],
    }
    validate_plan(m, plan)
    return plan


def new_row():
    return {"side": TARGET[0], "motor": TARGET[1], "name": TARGET[2], "kind": "feedback_diag",
            "status": "NOT_RUN", "preflight": [], "errors": [], "uart_blocked": False,
            "register_writes": 0, "motion_commands": 0, "firmware_contract_verified": False,
            "observation": {"status": "NOT_RUN", "groups": [], "budget_ns": BUDGET_NS}}


def block_uart(engine, error):
    row = engine.feedback_diag
    first = not row["uart_blocked"]
    row["uart_blocked"] = True
    engine.manual_cut = True
    row["errors"].append(repr(error))
    if first:
        print("爸爸，只读诊断停止值或身份未确认，请立即人工断电；禁止后续UART。", flush=True)


class FeedbackReadFailure(ValueError):
    def __init__(self, message, raw):
        super().__init__(message)
        self.raw = raw


def parse_raw(m, h, register, text):
    # 复用冻结原帧校验器，只把已经取得的stdout交回解析；不会再调用真实命令。
    reader = SimpleNamespace(io=SimpleNamespace(command=lambda argv, **kwargs: text))
    parser = SimpleNamespace(DEVICES=m.DEVICES, strict_json=m.strict_json, verify_ref=lambda *args, **kwargs: None)
    raw = h.raw_read(parser, reader, register)
    require(all(type(raw[field]) is int for field in ("motor_id", "requested_register_address", "width"))
            and type(raw["tx"]) is list and all(type(byte) is int and 0 <= byte <= 255 for byte in raw["tx"]),
            "完整原帧字段必须为整数，禁止bool冒充字节")
    return raw


def install_guard(m, h, engine):
    if not hasattr(engine, "feedback_diag"):
        engine.feedback_diag = new_row()
    require(not getattr(engine.io, "feedback_guard_installed", False)
            or getattr(engine.io, "feedback_guard_engine", None) is engine, "只读执行器归属改变")
    if getattr(engine.io, "feedback_guard_installed", False):
        return
    original = engine.io.command
    tool_paths = (h.TOOL["path"], h.RAW_TOOL["path"], h.STRACE["path"])

    def command(argv, **kwargs):
        if not argv:
            return original(argv, **kwargs)
        if str(argv[0]) not in tool_paths:
            if any(str(arg) in tool_paths for arg in argv[1:]):
                error = ValueError("不得以前缀命令绕过具名READ名单")
                block_uart(engine, error)
                raise error
            return original(argv, **kwargs)
        require(not engine.feedback_diag["uart_blocked"] and not engine.manual_cut, "粘性封锁：禁止后续UART")
        try:
            check_go = getattr(engine, "feedback_go_check", None)
            if check_go is not None:
                check_go()
            path = str(argv[0])
            require(path != h.STRACE["path"], "不得直接提交strace绕过具名READ名单")
            raw_read = path == h.RAW_TOOL["path"]
            if raw_read:
                require(len(argv) == 5 and argv[1:4] == [m.DEVICES["right"], "9", "read-raw"]
                        and argv[4] in h.META, "raw仅right9九字段加PP只读；禁止WRITE/其他目标")
            else:
                require(len(argv) == 5 and argv[3] == "read" and argv[4] in m.REGS
                        and any(argv[1:3] == [m.DEVICES[side], motor]
                                for side, motor in (key.split(":") for key in m.MOTOR_KEYS)),
                        "普通工具仅原17电机REGS只读；禁止所有WRITE/Enable")
            text = original(argv, **kwargs)
            if raw_read:
                raw = parse_raw(m, h, argv[4], text)
                if argv[4] in m.STOP_REGS and raw["raw_unsigned"] != 0:
                    raise FeedbackReadFailure("raw停止值非零:" + argv[4], raw)
            else:
                match = re.fullmatch(rf"read {re.escape(argv[4])}@{argv[2]} = (-?\d+)\n", text)
                require(match is not None, "普通READ完整stdout或身份未知")
                require(argv[4] not in m.STOP_REGS or int(match[1]) == 0, "普通READ停止值非零:" + argv[4])
            if check_go is not None:
                try:
                    check_go()
                except BaseException as error:
                    if raw_read:
                        raise FeedbackReadFailure("READ完成时本次GO失效:" + repr(error), raw) from error
                    raise
            return text
        except BaseException as error:
            block_uart(engine, error)
            raise

    engine.io.command = command
    engine.io.feedback_guard_engine = engine
    engine.io.feedback_guard_installed = True


def timed_raw(m, h, engine, register, destination, deadline=None):
    sample = {"register": register, "started_ns": engine.io.now(), "status": "UNKNOWN"}
    sequence_before = getattr(engine.io, "sequence", None)
    commands_before = len(getattr(engine.io, "commands", []))
    destination.append(sample)
    try:
        require(deadline is None or sample["started_ns"] < deadline, "500ms原期限已到，禁止新请求")
        sample["raw"] = h.raw_read(m, engine, register)
        sample["status"] = "READ_VALID"
    except BaseException as error:
        if isinstance(error, FeedbackReadFailure):
            sample["raw"] = error.raw
            sample["status"] = "FRAME_VALID_REJECTED"
        sample["error"] = repr(error)
        block_uart(engine, error)
        raise
    finally:
        sample["finished_ns"] = engine.io.now()
        sequence_after = getattr(engine.io, "sequence", None)
        if (sequence_before is not None and sequence_after is not None and sequence_after > sequence_before
                and len(getattr(engine.io, "commands", [])) > commands_before):
            sample["command_sequence"] = sequence_after
            sample.update({key: engine.io.commands[-1].get(key) for key in ("stdout_bytes", "stdout_sha256")})
        if deadline is not None:
            sample["completed_in_budget"] = sample["finished_ns"] <= deadline
    require(deadline is None or sample["completed_in_budget"], "原帧完成越过500ms原期限")
    return sample


def run_feedback_diag(m, h, engine):
    validate_plan(m, engine.plan)
    prepare_helper(h)
    install_guard(m, h, engine)
    row = engine.feedback_diag
    require(row["status"] == "NOT_RUN" and not row["uart_blocked"], "只读诊断只允许首次运行")
    row["status"] = "FAIL"
    observation = row["observation"]
    try:
        for register in PREFLIGHT_REGS:
            sample = timed_raw(m, h, engine, register, row["preflight"])
            require(sample["raw"]["raw_unsigned"] == EXPECTED[register], "型号/固件/Mode/Phase/Lock/GV不符:" + register)
        engine.check_gate()
        start = engine.io.now()
        deadline = start + BUDGET_NS
        observation.update(status="FAIL", started_ns=start, deadline_ns=deadline)
        for index, offset in enumerate(SAMPLE_OFFSETS_NS):
            scheduled = start + offset
            now = engine.io.now()
            require(offset == 0 or now <= scheduled, "前组完成已跨原分散请求时刻，不补读或重排")
            if now < scheduled:
                engine.io.sleep((scheduled - now) / 1_000_000_000)
            next_slot = start + SAMPLE_OFFSETS_NS[index + 1] if index + 1 < SAMPLE_COUNT else deadline
            require(engine.io.now() < next_slot, "等待已跨下一固定时刻或500ms期限，不补读")
            if hasattr(engine.io, "check"):
                engine.io.check()
            require(engine.io.now() < deadline, "500ms原期限已到，不补读")
            group = {"scheduled_ns": scheduled, "requested_offset_ns": offset,
                     "started_ns": engine.io.now(), "reads": [], "status": "FAIL"}
            group["request_lateness_ns"] = max(0, group["started_ns"] - scheduled)
            observation["groups"].append(group)
            for register in GROUP_REGS:
                timed_raw(m, h, engine, register, group["reads"], deadline)
            group.update(status="PASS", completed_in_budget=True, raw_pv_zero=True)
        observation["in_budget_pv0_groups"] = len(observation["groups"])
        require(observation["in_budget_pv0_groups"] == SAMPLE_COUNT, "原期限内不足三组PV0原帧")
        remaining = deadline - engine.io.now()
        if remaining > 0:
            engine.io.sleep(remaining / 1_000_000_000)
        if getattr(engine, "feedback_go_check", None) is not None:
            engine.feedback_go_check()
        observation["status"] = "PASS"
        row["status"] = "PASS"
    except BaseException as error:
        block_uart(engine, error)
        raise
    finally:
        if "started_ns" in observation:
            observation.update(finished_ns=engine.io.now(), elapsed_ns=engine.io.now() - observation["started_ns"])
    return row


def install(m, h, go=None):
    require(not getattr(m, "feedback_wrapper_installed", False), "本轮wrapper不可重复安装")
    prepare_helper(h)
    original_targets, original_go = m.position_targets_from_plan, m.validate_go
    original_finish, original_init, original_classify = m.finish_verification, m.ProbeEngine.__init__, m.classify_run_result
    m.position_targets_from_plan = lambda plan: validate_plan(m, plan) if "feedback_diag" in plan else original_targets(plan)
    m.validate_go = lambda go, plan, now: validate_feedback_go(m, original_go, go, plan, now)
    # 内层仅冻结普通PV命令可以扩展strace；外层从不直接许可strace。
    h.install_stop_wire_capture(m)

    def init(engine, *args, **kwargs):
        original_init(engine, *args, **kwargs)
        # main执行入口必传当次GO；内存替身可不装真实授权时钟。
        engine.feedback_go_check = (lambda: validate_feedback_go(m, original_go, go, engine.plan, time.time_ns())) if go is not None else None
        install_guard(m, h, engine)

    def motion(engine, bindings, plan):
        try:
            run_feedback_diag(m, h, engine)
        finally:
            for side, motor, name in m.BASE_TARGETS:
                engine.rows.append({"side": side, "motor": motor, "name": name, "kind": "velocity",
                                    "status": "NOT_RUN", "decision": "DEFERRED",
                                    "reason_code": "BASE_FIRMWARE_CONTRACT_UNVERIFIED"})
        raise ValueError("BASE_FIRMWARE_CONTRACT_UNVERIFIED：三轮DEFERRED，无wheel/pulse或WRITE")

    def finish(report, io, engine, bindings, proc):
        try:
            original_finish(report, io, engine, bindings, proc)
        finally:
            report["feedback_diag"] = engine.feedback_diag
            report["position_function_test"] = "NOT_RUN_NO_ACTIVE_MOTION"
            if (report.get("stop_verification", {}).get("status") != "PASS"
                    or report.get("identity_verification", {}).get("status") != "PASS"):
                block_uart(engine, ValueError("原STOP51或完整身份后验未确认"))

    m.ProbeEngine.__init__ = init
    m.run_motion_probes = motion
    m.finish_verification = finish
    m.classify_run_result = lambda report: ("NEEDS_MANUAL_POWER_CUT" if report.get("feedback_diag", {}).get("uart_blocked") else original_classify(report))
    m.feedback_wrapper_installed = True


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("maintenance", "raw-helper", "bindings", "output", "artifact-root", "approval"):
        parser.add_argument("--" + name, type=Path)
    parser.add_argument("--execute", action="store_true")
    args = parser.parse_args(argv)
    if not all((args.maintenance, args.raw_helper, args.bindings, args.output, args.artifact_root)):
        require(not args.execute, "执行输入不完整，零I/O拒绝")
        print(json.dumps({"status": "PLAN_ONLY", "run_id": RUN_ID, "target": TARGET, "scope": FIELD_SCOPE,
                          "actions_started": 0, "register_writes": 0, "motion_commands": 0,
                          "position_targets": [], "new_GO": False}, ensure_ascii=False))
        return 0
    if args.execute:
        require(args.approval is not None, "必须root本次新只读范围GO；旧GO不可复用")
    m, _ = load_frozen(args.maintenance, SOURCE_SHA, None, "feedback_original_d97")
    h, helper_ref = load_frozen(args.raw_helper, HELPER_SHA, HELPER_BYTES, "feedback_raw_right9")
    bindings = m.strict_json(args.bindings.read_text())
    plan = make_plan(m, h, bindings, args.output, args.artifact_root, reference(__file__), helper_ref)
    if not args.execute:
        print(json.dumps({"status": "PLAN_ONLY", "actions_started": 0, "plan_sha256": m.digest(plan), "plan": plan}, ensure_ascii=False, indent=2))
        return 0
    go = m.strict_json(args.approval.read_text())
    install(m, h, go)
    m.validate_go(go, plan, time.time_ns())
    return m.execute(plan, go, args.approval)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except ValueError as error:
        print("爸爸，只读诊断前置拒绝：" + str(error), file=sys.stderr)
        raise SystemExit(2)
