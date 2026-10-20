#!/usr/bin/env -S uv run --no-project python
"""爸爸，受监督直接wire维护；默认PLAN_ONLY，精度与停机证据分别判定。"""
import argparse
import contextlib
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace

TOOL_SHA = "f2014ee4008311e7a8725235a39f0d1b3913391f17cbd977b7441f120539114b"
TOOL_BYTES = 1469904
SERIALS = {"left": "5A7C121427", "right": "5A7C122340"}
DEVICES = {side: f"/dev/serial/by-id/usb-1a86_USB_Single_Serial_{serial}-if00"
           for side, serial in SERIALS.items()}
UNITS = [f"xlerobot-{role}-{side}{'-arm' if role == 's0' else ''}.service"
         for side in SERIALS for role in ("s0", "s1", "gateway")]
OLD_UNITS = ["xlerobot-coordinator.service", "xlerobot-web.service",
             "xlerobot-s0-bus1.service", "xlerobot-s0-bus2.service"]
MAINTENANCE_NAMESPACE = Path('/home/loosqk/robot-checks')
STOP_REGS = ('Torque_Enable','Present_Velocity','Goal_Velocity')
MOTOR_KEYS = tuple(f'{side}:{motor}' for side in SERIALS
                   for motor in range(1,9 if side=='left' else 10))
# 维护动作只要求当次现场安全条件；底盘动作仍固定 DEFERRED。
FACTS = ("supported", "unloaded", "area_clear",
         "continuous_presence_until_cleanup", "immediate_power_cut")
WRITE_REGS = {"Torque_Enable", "Goal_Position", "Goal_Velocity"}
REGS = ("Operating_Mode", "Torque_Enable", "Present_Position", "Present_Velocity",
        "Goal_Position", "Goal_Velocity")
TARGETS = [("left", 5, "left_arm_wrist_roll"), ("right", 5, "right_arm_wrist_roll"),
           ("left", 7, "head_motor_1"), ("left", 8, "head_motor_2")]
POSITION_TARGETS = dict(zip(('left-wrist', 'right-wrist', 'head-1', 'head-2'), TARGETS))
BASE_TARGETS = [('right',8,'base_front_wheel'),('right',7,'base_left_wheel'),('right',9,'base_right_wheel')]
RIGHT_PULSE_SCOPE = '仅右9先SYNC零速确认→Enable确认→SYNC+500原始值，Enable起500ms两次速度/位置观察→SYNC零速→独立Disable及原STOP51；三轮已架空稳定支撑并持续观察'
RIGHT_PULSE_RAW = {'path':'/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/quality/frozen/feetech_poke',
                   'sha256':'b2bfedb1bc420c9957a276cd69f36f5fd47c66d0b7aaa519cf3375e257eabd94','bytes':1507224}
RIGHT_PULSE_HELPER = {'path':'/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/diagnostic-candidate/fixtures/raw_right9_from_r14_r2.py',
                      'sha256':'70d777bd5c2a19e2ce3f57e68f9abd40eaed2b8a9aa8c592e7fe53a940f1e38c','bytes':12220}
RIGHT_PULSE_TRACE = {'path':'/usr/bin/strace',
                     'sha256':'5392f54d889a87341d49f970d91e21ab8c1f70a835ac9ead6ee4d17e532ae7f7','bytes':1583504}
RIGHT_PULSE_WRITE = {'path':'/home/loosqk/robot-checks/h2-serial-recovery-20260930-r1/readback-tool-r1/feetech_poke',
                     'sha256':TOOL_SHA,'bytes':TOOL_BYTES}
# Orin原生冻结产物；未完整绑定时禁止生成可执行计划。
RIGHT_PULSE_SYNC = {'path':'/home/loosqk/robot-checks/right-wheel-reference-20261020-r2/quality/frozen/feetech_poke',
                    'sha256':'22693d15bbf926a9a99b8f295c8fffbff92341b8151e9da1620b7d94701820b0','bytes':1517688}
RIGHT_PULSE_METADATA = {'Model_Number':777,'Firmware_Major_Version':3,'Firmware_Minor_Version':10,
                        'Operating_Mode':1,'Phase':12,'Lock':1}
LEFT_FEEDBACK_SCOPE = '仅左轮right7先读TE0，0/250/500/750/1000ms各读PP再PV，末读TE0，共12次只读；非零PV仅作诊断，不判停止通过、不写电机'
LEFT_FEEDBACK_META = {'Torque_Enable':(40,1),'Present_Position':(56,2),'Present_Velocity':(58,2)}
LEFT_FEEDBACK_IDENTITY = ('uid','gid','mode','device','inode','mtime_ns','ctime_ns','nlink','symlink','elf_machine')


def left_feedback_contract():
    return {'scope':LEFT_FEEDBACK_SCOPE,'target':['right',7,'base_left_wheel'],
            'raw_tool':dict(RIGHT_PULSE_RAW),'reads':12,'register_writes':0,
            'sample_offsets_ns':[0,250_000_000,500_000_000,750_000_000,1_000_000_000],
            'capture_budget_ns':1_500_000_000,'same_frame':False,'stop_verified':False,
            'limitations':['1500ms是包含首末TE及五组读取的软件预算，不是硬截止',
                           'PP/PV先后采样；非零PV保留原值，不转换成停止PASS或动作授权',
                           '本轮SYNC_READ时序不替代R2普通READ故障原帧；未复现不表示已修复']}


def left_feedback_tool_ref(plan):
    refs=[ref for ref in plan['bindings'].get('refs',[])
          if all(ref.get(k)==v for k,v in RIGHT_PULSE_RAW.items())
          and all(k in ref for k in LEFT_FEEDBACK_IDENTITY)]
    require(len(refs)==1,'左轮只读必须唯一完整b2身份引用')
    return refs[0]


def validate_left_feedback_plan(plan):
    require('right_wheel_pulse' not in plan and plan.get('position_targets')==[]
            and plan.get('left_wheel_feedback_only')==left_feedback_contract(),
            '左轮只读禁止混入动作或更改固定序列')
    require(RIGHT_PULSE_RAW['path'] in plan['bindings'].get('binary_paths',[]),
            '左轮只读进程名单缺b2')
    left_feedback_tool_ref(plan)
    require(all(row.get('decision')=='DEFERRED' and row.get('reason_code')=='BASE_FIRMWARE_CONTRACT_UNVERIFIED'
                for row in plan.get('base_targets',[])) and len(plan.get('base_targets',[]))==3,
            '左轮只读不得解除三轮合同DEFERRED')


def right_pulse_contract():
    return {'scope':RIGHT_PULSE_SCOPE,'target':['right',9,'base_right_wheel'],'raw_velocity':500,
            'observation_ns':500_000_000,'sample_offsets_ns':[100_000_000,400_000_000],
            'nonzero_window_fail_ns':1_000_000_000,'velocity_raw_max':1000,'position_delta_raw_max':512,
            'metadata':dict(RIGHT_PULSE_METADATA),'raw_tool':dict(RIGHT_PULSE_RAW),
            'raw_helper':dict(RIGHT_PULSE_HELPER),'strace':dict(RIGHT_PULSE_TRACE),
            'write_tool':dict(RIGHT_PULSE_WRITE),'sync_tool':dict(RIGHT_PULSE_SYNC),
            'firmware_contract_verified':False,
            'limitations':['原始值尚未标定物理单位；500ms/1s是软件观察/失败门，不是硬件自动截止',
                           '只诊断一次正向动作，不改变Phase/模式/保护、三轮合同或生产Velocity链']}


def validate_right_pulse_plan(plan):
    require(isinstance(RIGHT_PULSE_SYNC['sha256'],str)
            and re.fullmatch(r'[0-9a-f]{64}',RIGHT_PULSE_SYNC['sha256']) is not None
            and type(RIGHT_PULSE_SYNC['bytes']) is int and RIGHT_PULSE_SYNC['bytes']>0,
            '右轮SYNC工具尚未完整冻结绑定')
    require(plan.get('position_targets') == [] and plan.get('right_wheel_pulse') == right_pulse_contract(),
            '右轮诊断必须空位置目标及完整固定合同')
    bindings=plan['bindings']
    matches=lambda actual,expected: isinstance(actual,dict) and all(actual.get(key)==value for key,value in expected.items())
    require(matches(bindings.get('tool'),RIGHT_PULSE_WRITE), '右轮诊断必须原冻结f201完整引用')
    refs=(RIGHT_PULSE_WRITE,RIGHT_PULSE_RAW,RIGHT_PULSE_SYNC,RIGHT_PULSE_HELPER,RIGHT_PULSE_TRACE)
    require(all(any(matches(actual,reference) for actual in bindings.get('refs',[])) for reference in refs),
            '右轮工具/helper/strace引用缺失')
    require(all(reference['path'] in bindings.get('binary_paths',[])
                for reference in (RIGHT_PULSE_WRITE,RIGHT_PULSE_RAW,RIGHT_PULSE_SYNC,RIGHT_PULSE_TRACE)),
            '右轮二进制绑定缺失')
    require(plan.get('base_targets') == [{'side':side,'motor':motor,'name':name,'decision':'DEFERRED',
             'reason_code':'BASE_FIRMWARE_CONTRACT_UNVERIFIED'} for side,motor,name in BASE_TARGETS],
            '三轮固件合同必须仍DEFERRED')


def require(value, reason):
    if not value:
        raise ValueError(reason)


def strict_json(text):
    def pairs(items):
        result = {}
        for key, value in items:
            require(key not in result, f"重复 JSON 字段：{key}")
            result[key] = value
        return result
    def invalid(value):
        raise ValueError(f"非有限 JSON：{value}")
    return json.loads(text, object_pairs_hook=pairs, parse_constant=invalid)


def digest(value):
    data = json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode()
    return hashlib.sha256(data).hexdigest()


def write_new(path, value):
    with Path(path).open("x") as output:
        json.dump(value, output, ensure_ascii=False, indent=2)
        output.write("\n")


def validate_artifact_root(root, *, filesystem=False):
    root=Path(root)
    require(root.is_absolute() and root==Path(os.path.normpath(root)), '维护根目录必须为规范绝对路径')
    require(root.is_relative_to(MAINTENANCE_NAMESPACE) and root!=MAINTENANCE_NAMESPACE,
            '维护根目录必须是robot-checks内独立子目录')
    if filesystem:
        require(root==root.resolve() and stat.S_ISDIR(root.lstat().st_mode)
                and root.stat().st_uid==os.getuid(), '维护根目录身份/符号链接错误')
    return root


def artifact_path(path, root, *, existing=False):
    root=validate_artifact_root(root,filesystem=True)
    path = Path(path)
    require(path.is_absolute() and path == path.resolve(), '维护产物路径必须规范且无符号链接')
    require(path.is_relative_to(root) and path != root,
            '维护输出/GO只能在本轮独立产物目录')
    if existing:
        require(stat.S_ISREG(path.lstat().st_mode), 'GO不是普通文件')
    else:
        require(not path.exists() and path.parent.is_dir(), '输出已存在或父目录缺失')
    return path


def verify_ref(reference, *, executable=False):
    path = Path(reference["path"])
    require(path.is_absolute(), "引用路径不是绝对路径")
    info = path.lstat()
    require(stat.S_ISREG(info.st_mode), f"引用不是普通文件：{path}")
    data = path.read_bytes()
    require(len(data) == reference["bytes"] and hashlib.sha256(data).hexdigest() == reference["sha256"],
            f"引用 SHA/bytes 改变：{path}")
    if executable:
        require(os.access(path, os.X_OK) and data[:4] == b"\x7fELF" and data[4:6] == b"\x02\x01"
                and int.from_bytes(data[18:20], "little") == 183, "工具不是冻结 ARM64 ELF")
    return data


def select_position_targets(names=None):
    """缺省保持四项原顺序；显式选择非空且唯一，按输入顺序绑定具体电机。"""
    if names is None:
        names = list(POSITION_TARGETS)
    require(isinstance(names, (list, tuple)) and names, '位置目标计划必须非空')
    require(all(type(name) is str and name in POSITION_TARGETS for name in names), '未知位置目标')
    require(len(set(names)) == len(names), '位置目标重复，禁止开串口')
    return [POSITION_TARGETS[name] for name in names]


def position_targets_from_plan(plan):
    """执行只接受计划中已绑定的目标；不从全局四项列表补跑未选电机。"""
    if 'left_wheel_feedback_only' in plan:
        validate_left_feedback_plan(plan)
        return []
    if 'right_wheel_pulse' in plan:
        validate_right_pulse_plan(plan)
        return []
    values = plan.get('position_targets')
    require(isinstance(values, (list, tuple)) and values, '缺少非空位置目标计划')
    targets = []
    for value in values:
        require(isinstance(value, (list, tuple)) and len(value) == 3
                and type(value[0]) is str and type(value[1]) is int and type(value[2]) is str,
                '位置目标字段错误')
        target = tuple(value)
        require(target in POSITION_TARGETS.values(), '位置目标不在固定白名单')
        require(target not in targets, '位置目标计划重复')
        targets.append(target)
    return targets


def make_plan(bindings, output, run_id, artifact_root, position_target_names=None, *, right_wheel_pulse=False,
              left_wheel_feedback_only=False):
    require(type(right_wheel_pulse) is bool and type(left_wheel_feedback_only) is bool
            and not (right_wheel_pulse and left_wheel_feedback_only)
            and not ((right_wheel_pulse or left_wheel_feedback_only) and position_target_names is not None),
            '轮诊断禁止互相混用或混用位置选择')
    position_targets = [] if right_wheel_pulse or left_wheel_feedback_only else select_position_targets(position_target_names)
    require(bindings["schema"] == "MAINTENANCE_BINDINGS_V1", "绑定 schema 错误")
    require(bindings["tool"]["sha256"] == TOOL_SHA and bindings["tool"]["bytes"] == TOOL_BYTES,
            "必须采用 root 指定冻结 poke")
    require(set(bindings["usb"]) == set(SERIALS), "USB 双端绑定缺失")
    for side in SERIALS:
        require(bindings["usb"][side]["device"] == DEVICES[side]
                and bindings["usb"][side]["serial"] == SERIALS[side], "USB D2 身份错误")
    require(Path(output).is_absolute() and re.fullmatch(r"[a-z0-9][a-z0-9-]{0,39}", run_id),
            "输出目录或 run-id 错误")
    require(Path(output)==Path(os.path.normpath(output)), '输出目录不是规范路径')
    script = Path(__file__).read_bytes()
    root=validate_artifact_root(artifact_root)
    require(Path(output).is_relative_to(root) and Path(output)!=root,'输出不在显式维护根目录内')
    plan = {"schema": "MAINTENANCE_PLAN_V2", "run_id": run_id, "output": str(output), "artifact_root":str(root),
            "bindings": bindings, "script_sha256": hashlib.sha256(script).hexdigest(), "script_bytes": len(script),
            "position_targets": position_targets, "position_delta_raw": 24, "minimum_excursion_raw": 12,
            "return_tolerance_raw": 4, "position_wait_ms": 800, "base_order": [8, 7, 9],
            "base_raw": [20, -20], "pulse_target_ms": 150, "nonzero_window_fail_ns": 1_000_000_000,
            "observed_velocity_raw_max": 40, "observed_wheel_delta_raw_min": 3, "observed_wheel_delta_raw_max": 64,
            "base_targets": [{'side':side,'motor':motor,'name':name,'decision':'DEFERRED',
                              'reason_code':'BASE_FIRMWARE_CONTRACT_UNVERIFIED'} for side,motor,name in BASE_TARGETS],
            "limitations": ["停止寄存器证据不替代物理独立断电确认",
                            "base仅mode1/Goal0合同；Model/Phase解释门未闭合，本软件结果不授权新base动作"],
            "scope": "单电机独立维护动作/停止验证；非整车运动学、S0/S1接入、H2/D15或生产验收"}
    if right_wheel_pulse:
        plan['right_wheel_pulse']=right_pulse_contract()
        plan['scope']=RIGHT_PULSE_SCOPE
        validate_right_pulse_plan(plan)
    if left_wheel_feedback_only:
        plan['left_wheel_feedback_only']=left_feedback_contract()
        plan['scope']=LEFT_FEEDBACK_SCOPE
        validate_left_feedback_plan(plan)
    return plan


def validate_go(go, plan, now_ns):
    position_targets_from_plan(plan)
    require(plan['schema']=='MAINTENANCE_PLAN_V2' and go["schema"] == "MAINTENANCE_GO_V2" and go["issued_by"] == "root"
            and go["plan_sha256"] == digest(plan) and go['artifact_root']==plan['artifact_root'],
            "拒绝旧V1或未绑定显式根目录的root当次GO")
    require(type(go["issued_at_ns"]) is int and type(go["expires_at_ns"]) is int
            and go["issued_at_ns"] <= now_ns < go["expires_at_ns"]
            and 0 < go["expires_at_ns"] - go["issued_at_ns"] <= 3600_000_000_000, "GO 时效错误")
    require(go["host"] == plan["bindings"]["host"], "GO host/boot 身份错误")
    require(isinstance(go["source"], str) and go["source"].strip(), "现场事实来源缺失")
    require(set(go["physical"]) == set(SERIALS), "双端现场事实缺失")
    for side in SERIALS:
        facts = go["physical"][side]
        require(all(facts.get(key) is True for key in FACTS), "现场事实不完整")
        require(type(facts["confirmed_at_ns"]) is int
                and go["issued_at_ns"] - 3600_000_000_000 <= facts["confirmed_at_ns"] <= now_ns,
                "现场确认时间错误")
    if 'right_wheel_pulse' in plan or 'left_wheel_feedback_only' in plan:
        facts=go['physical']['right']
        require(0 < go['expires_at_ns']-go['issued_at_ns'] <= 600_000_000_000
                and 0 <= now_ns-facts['confirmed_at_ns'] <= 600_000_000_000,
                '右轮诊断现场/GO最多600秒')
        require(all(facts.get(key) is True for key in ('all_three_wheels_off_ground','base_stably_supported',
                    'continuous_wheel_observation'))
                and facts.get('base_confirmation_kind')=='CURRENT_SETUP_READY'
                and facts.get('base_confirmation_scope')==(
                    LEFT_FEEDBACK_SCOPE if 'left_wheel_feedback_only' in plan else RIGHT_PULSE_SCOPE)
                and isinstance(facts.get('base_confirmation_answer'),str) and facts['base_confirmation_answer'].strip(),
                '缺本轮架空支撑/持续观察及具体动作现场确认')


class RealExecutor:
    """仅一个自有 child；超预算也持有并真实等待，不强杀、不伪造回收。"""
    def __init__(self, tool, output):
        self.tool, self.output = tool, Path(output)
        self.interrupted = None
        self.sequence = 0
        self.child = None
        self.commands = []
        self.evidence_errors = []
        self.journal = (self.output / "commands.jsonl").open("x")

    def mark_signal(self, signum, _frame):
        self.interrupted = signum

    def check(self):
        require(self.interrupted is None, f"收到信号 {self.interrupted}，停止后续运动")

    def now(self):
        return time.monotonic_ns()

    def sleep(self, seconds, cleanup=False):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if not cleanup:
                self.check()
            time.sleep(min(0.01, max(0, deadline - time.monotonic())))

    def reap(self, row, failures):
        while True:
            try:
                code = self.child.wait(timeout=0.1)
                row.update(exit_code=code, reaped=True)
                self.child = None
                return code
            except subprocess.TimeoutExpired:
                pass
            except BaseException as error:
                failures.append('wait:' + repr(error))
            try:
                if self.now() - row['started_ns'] > 2_000_000_000:
                    if not row['over_budget']:
                        print('爸爸，child超预算，仍持有真实wait；请立即人工断电。', flush=True)
                    row['over_budget'] = True
            except BaseException as error:
                failures.append('wait观测:' + repr(error))

    def command(self, argv, *, cleanup=False, allowed=(0,), input_raw=None):
        if not cleanup:
            self.check()
        require(self.child is None, "禁止同时拥有第二 child")
        self.sequence += 1
        prefix = self.output / f"command_{self.sequence:04d}"
        row = {"sequence": self.sequence, "argv": [str(v) for v in argv],
               "started_ns": self.now(), "pid": None, "reaped": False,
               "signals_sent": [], "over_budget": False, "cleanup": cleanup}
        failures = []
        out, err = b'', b''
        try:
            # 原始输出先落匿名文件；证据目录写失败不会阻止后续独立安全收尾命令。
            with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr, tempfile.TemporaryFile() as stdin:
                if input_raw is not None:
                    stdin.write(input_raw)
                    stdin.seek(0)
                self.child = subprocess.Popen(argv, stdin=stdin if input_raw is not None else subprocess.DEVNULL,
                                              stdout=stdout, stderr=stderr, start_new_session=True,
                                              env={**os.environ, 'PATH':'/usr/sbin:/usr/bin:/sbin:/bin', 'LC_ALL':'C',
                                                   'RUST_LOG':'error'})
                row["pid"] = self.child.pid
                # 一旦获得child，任何wait异常都保有所有权并继续自然wait，不让finally收尾被第二child门挡住。
                code = self.reap(row, failures)
                row['over_budget'] |= self.now() - row['started_ns'] > 2_000_000_000
                stdout.seek(0); stderr.seek(0)
                out, err = stdout.read(), stderr.read()
            for suffix, raw in (('.stdout',out),('.stderr',err)):
                try:
                    with prefix.with_suffix(suffix).open('xb') as stream:
                        stream.write(raw)
                except BaseException as error:
                    failures.append('evidence:' + repr(error))
                    self.evidence_errors.append(repr(error))
            row.update(stdout_path=str(prefix.with_suffix(".stdout")), stderr_path=str(prefix.with_suffix(".stderr")),
                       stdout_sha256=hashlib.sha256(out).hexdigest(), stderr_sha256=hashlib.sha256(err).hexdigest(),
                       stdout_bytes=len(out), stderr_bytes=len(err))
            require(code in allowed and not row["over_budget"], f"child 退出/预算失败：{argv}")
            require(err == b'', 'child stderr非空，原件保留')
            require(not failures, f'child回收/证据异常：{failures}')
            if not cleanup:
                self.check()
            return out.decode()
        except BaseException as error:
            if self.child is not None:
                # 包括输出/计时/回执处理异常；不让仍活着的自有child逃离当前command。
                self.reap(row, failures)
            row["error"] = repr(error)
            raise
        finally:
            row["finished_ns"] = self.now()
            row['owner_errors'] = failures
            self.commands.append(row)
            try:
                self.journal.write(json.dumps(row, ensure_ascii=False) + "\n")
                self.journal.flush()
            except BaseException as error:
                self.evidence_errors.append('journal:' + repr(error))
                raise ValueError('命令journal写失败，child已真实回收') from error

    def poke(self, side, motor, action, register, value=None, *, cleanup=False):
        require(side in SERIALS and 1 <= motor <= (8 if side == "left" else 9), "目标 ID 越界")
        require(register in REGS and action in ("read", "write"), "维护操作越界")
        require(action != "write" or register in WRITE_REGS, "禁止 EEPROM/mode/Lock/保护写")
        if action == "write":
            require(type(value) is int and ((register == "Torque_Enable" and value in (0, 1))
                    or (register == "Goal_Velocity" and abs(value) <= 20)
                    or (register == "Goal_Position" and 0 <= value <= 4095)), "写值越界")
        verify_ref(self.tool, executable=True)
        argv = [self.tool["path"], DEVICES[side], str(motor), action, register]
        if action == "write":
            argv.append(str(value))
        text = self.command(argv, cleanup=cleanup)
        pattern = (rf"read {register}@{motor} = (-?\d+)\n" if action == "read"
                   else rf"write {register}@{motor} <- {value} ok\n")
        match = re.fullmatch(pattern, text)
        require(match is not None, "poke完整stdout不匹配")
        return int(match[1]) if action == "read" else None


class PositionAccuracyError(ValueError):
    """强类型精度证据；样本直接来自本轮读取，不解析旧错误字符串。"""
    def __init__(self,evidence):
        self.evidence=evidence
        super().__init__(evidence['failure_code'])


class ProbeEngine:
    def __init__(self, executor, plan, check_gate=lambda: None):
        self.io, self.plan, self.check_gate = executor, plan, check_gate
        self.rows = []
        self.manual_cut = False

    def read(self, side, motor, register, cleanup=False):
        return self.io.poke(side, motor, "read", register, cleanup=cleanup)

    def write(self, side, motor, register, value, cleanup=False):
        self.io.poke(side, motor, "write", register, value, cleanup=cleanup)

    def safe_off(self, side, motor, velocity):
        errors = []
        # 零速失败仍独立尝试断扭；每项原错误保留，不用后来的成功覆盖。
        operations = [("Goal_Velocity", 0)] if velocity else []
        operations.append(("Torque_Enable", 0))
        for register, value in operations:
            try:
                self.write(side, motor, register, value, cleanup=True)
            except BaseException as error:
                errors.append(repr(error))
        try:
            self.io.sleep(0.05, cleanup=True)
        except BaseException as error:
            errors.append(repr(error))
        for register, expected in ([('Goal_Velocity', 0)] if velocity else []) + [('Torque_Enable', 0), ('Present_Velocity', 0)]:
            try:
                require(self.read(side, motor, register, cleanup=True) == expected, f"停机未确认：{register}")
            except BaseException as error:
                errors.append(repr(error))
        if errors:
            self.manual_cut = True
            print(f'爸爸，{side}/{motor}安全收尾异常，请立即人工断电；禁止后续运动。', flush=True)
        return errors

    def witness_position(self, side, motor, target, anchor, stage, evidence):
        started=self.io.now()
        deadline = started + self.plan["position_wait_ms"] * 1_000_000
        row={'stage':stage,'status':'FAIL','target_raw':target,'anchor_raw':anchor,
             'tolerance_raw':self.plan['return_tolerance_raw'],'budget_ms':self.plan['position_wait_ms'],
             'started_ns':started,'deadline_ns':deadline,'samples':[], 'final_signed_error_raw':None}
        evidence[stage]=row
        while self.io.now() < deadline:
            position = self.read(side, motor, "Present_Position")
            stamp=self.io.now()
            row['samples'].append({'stamp_ns':stamp,'position_raw':position,'signed_error_raw':position-target})
            row.update(finished_ns=stamp,elapsed_ns=stamp-started,final_signed_error_raw=position-target)
            if abs(position-anchor)>32:
                row['failure_code']='POSITION_EXCURSION_LIMIT'
                raise PositionAccuracyError(row)
            # 未放宽800ms：读取在预算之后才返回，即使目标误差合格也不能PASS。
            if stamp<=deadline and abs(position - target) <= self.plan["return_tolerance_raw"]:
                row['status']='PASS'
                return row['samples']
            # 已到期限的失败样本直接收尾，不额外延迟断扭。
            if stamp>=deadline:
                break
            self.io.sleep(0.05)
        row.update(failure_code='POSITION_ACCURACY_TIMEOUT',finished_ns=self.io.now(),elapsed_ns=self.io.now()-started)
        raise PositionAccuracyError(row)

    def position(self, side, motor, name, calibration):
        self.check_gate()
        row = {"side": side, "motor": motor, "name": name, "kind": "position", "status": "FAIL",
               'position_accuracy':{'status':'NOT_RUN','stages':{}}}
        self.rows.append(row)
        error = None
        try:
            require(calibration["id"] == motor, "标定 ID 不匹配")
            require(self.read(side, motor, "Operating_Mode") == 0, "位置模式漂移，不切 mode")
            require(self.read(side, motor, "Torque_Enable") == 0 and self.read(side, motor, "Present_Velocity") == 0,
                    "目标非断扭/静止")
            anchor = self.read(side, motor, "Present_Position")
            goal = anchor + self.plan["position_delta_raw"]
            require(calibration["range_min"] <= anchor <= goal <= calibration["range_max"], "锚点/目标超标定范围")
            row.update(anchor_raw=anchor, goal_raw=goal)
            self.write(side, motor, "Goal_Position", anchor)
            self.io.sleep(0.02)
            require(self.read(side, motor, "Goal_Position") == anchor, "follow-current读回不匹配，禁止上扭")
            self.io.sleep(0.05)
            require(abs(self.read(side,motor,'Present_Position')-anchor) <= self.plan['return_tolerance_raw'],
                    '上扭前锚点漂移，拒绝追旧目标且不重试')
            self.write(side, motor, "Torque_Enable", 1)
            require(self.read(side, motor, "Torque_Enable") == 1, "上扭读回不匹配")
            self.write(side, motor, "Goal_Position", goal)
            require(self.read(side, motor, "Goal_Position") == goal, "出程Goal读回不匹配")
            row["outward"] = self.witness_position(side, motor, goal, anchor,'outward',row['position_accuracy']['stages'])
            require(max(v['position_raw'] - anchor for v in row['outward']) >= self.plan['minimum_excursion_raw'],
                    "零位移或实际出程不足")
            self.write(side, motor, "Goal_Position", anchor)
            require(self.read(side, motor, "Goal_Position") == anchor, "回程Goal读回不匹配")
            row["return"] = self.witness_position(side, motor, anchor, anchor,'return',row['position_accuracy']['stages'])
            row['position_accuracy']['status']='PASS'
        except PositionAccuracyError as failure:
            row['position_accuracy']['status']='FAIL'
            row['error_class']='POSITION_ACCURACY_FAILED'
            row['accuracy_failure']=failure.evidence
            error=repr(failure)
        except BaseException as failure:
            row['error_class']='ACTION_FAILED'
            error = repr(failure)
        finally:
            row["cleanup_errors"] = self.safe_off(side, motor, False)
            row["error"] = error
            row["status"] = "PASS" if error is None and not row["cleanup_errors"] else "FAIL"
        require(row["status"] == "PASS", f"位置维护失败：{row}")

    def pulse(self, motor, value):
        side = "right"
        row = {"raw_velocity": value, "status": "FAIL"}
        start = None
        try:
            anchor = self.read(side, motor, "Present_Position")
            require(0 <= anchor <= 4095, "轮编码器范围未知")
            start = self.io.now()
            self.write(side, motor, "Goal_Velocity", value)
            require(self.read(side, motor, "Goal_Velocity") == value, "有符号GoalVelocity读回不匹配")
            velocity = self.read(side, motor, "Present_Velocity")
            require(velocity * value > 0 and abs(velocity) <= self.plan['observed_velocity_raw_max'],
                    "轮速零值/方向/观测幅度不匹配")
            self.io.sleep(self.plan["pulse_target_ms"] / 1000)
            position = self.read(side, motor, "Present_Position")
            require(0 <= position <= 4095, "轮编码器范围未知")
            delta = (position - anchor + 2048) % 4096 - 2048
            row.update(anchor_raw=anchor, position_raw=position, velocity_raw=velocity, delta_raw=delta)
            require(delta * value > 0 and self.plan['observed_wheel_delta_raw_min'] <= abs(delta)
                    <= self.plan['observed_wheel_delta_raw_max'],
                    "编码器零位移/方向/观测幅度不匹配")
        except BaseException as error:
            row["error"] = repr(error)
        finally:
            try:
                self.write(side, motor, "Goal_Velocity", 0, cleanup=True)
                row['zero_write_confirmed'] = True
            except BaseException as error:
                row["zero_error"] = repr(error)
                row['zero_write_confirmed'] = False
                self.manual_cut = True
                print(f'爸爸，right/{motor}零速写未确认，立即人工断电；仍独立尝试断扭。', flush=True)
            if start is not None:
                finished = self.io.now()
                row.update(nonzero_write_started_ns=start, zero_write_attempt_finished_ns=finished)
                if row['zero_write_confirmed']:
                    row.update(zero_write_finished_ns=finished, nonzero_window_ns=finished-start)
                if finished-start > self.plan["nonzero_window_fail_ns"]:
                    row["window_error"] = "真实非零窗口超过1秒；非硬件自动截止保证"
        row["status"] = "PASS" if not any(key in row for key in ("error", "zero_error", "window_error")) else "FAIL"
        return row

    def wheel(self, motor):
        self.check_gate()
        row = {"side": "right", "motor": motor, "kind": "velocity", "status": "FAIL", "pulses": []}
        self.rows.append(row)
        error = None
        try:
            require(self.read("right", motor, "Operating_Mode") == 1, "轮模式漂移，不切mode")
            require(self.read("right", motor, "Torque_Enable") == 0
                    and self.read("right", motor, "Present_Velocity") == 0, "轮非断扭/静止")
            self.write("right", motor, "Goal_Velocity", 0)
            require(self.read("right", motor, "Goal_Velocity") == 0, "上扭前零目标未确认")
            self.io.sleep(0.05)
            self.write("right", motor, "Torque_Enable", 1)
            require(self.read("right", motor, "Torque_Enable") == 1, "轮上扭读回未确认")
            for value in self.plan["base_raw"]:
                pulse = self.pulse(motor, value)
                row["pulses"].append(pulse)
                require(pulse["status"] == "PASS", "单轮脉冲失败，停止后续动作")
                require(self.read("right", motor, "Goal_Velocity") == 0
                        and self.read("right", motor, "Present_Velocity") == 0, "反向前零速未确认")
        except BaseException as failure:
            error = repr(failure)
        finally:
            row["cleanup_errors"] = self.safe_off("right", motor, True)
            row["error"] = error
            row["status"] = "PASS" if error is None and not row["cleanup_errors"] else "FAIL"
        require(row["status"] == "PASS", f"轮维护失败：{row}")


def load_right_pulse_helper():
    """仅复用原帧校验/停止抓帧，不调用历史试验入口或其旧源码加载器。"""
    for reference in (RIGHT_PULSE_WRITE,RIGHT_PULSE_RAW,RIGHT_PULSE_SYNC,RIGHT_PULSE_TRACE):
        verify_ref(reference,executable=True)
    verify_ref(RIGHT_PULSE_HELPER)
    spec=importlib.util.spec_from_file_location('maintenance_right_pulse_raw',RIGHT_PULSE_HELPER['path'])
    helper=importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    helper.RAW_TOOL=dict(RIGHT_PULSE_RAW)
    helper.META={**helper.META,'Present_Position':(56,2)}
    return helper


def left_feedback_raw(io, reference, register, check_ready):
    """复用b2的只读原帧输出；此入口不能构造write或其他目标命令。"""
    require(register in LEFT_FEEDBACK_META,'左轮只读字段越界')
    verify_ref(reference,executable=True)
    path=Path(reference['path'])
    info=path.lstat()
    actual={'uid':info.st_uid,'gid':info.st_gid,'mode':stat.S_IMODE(info.st_mode),
            'device':info.st_dev,'inode':info.st_ino,'mtime_ns':info.st_mtime_ns,'ctime_ns':info.st_ctime_ns,
            'nlink':info.st_nlink,'symlink':path.is_symlink(),'elf_machine':183}
    require(all(reference.get(k)==v for k,v in actual.items()) and info.st_nlink==1
            and not path.is_symlink(),'左轮只读b2完整身份变化')
    check_ready()
    text=io.command([reference['path'],DEVICES['right'],'7','read-raw',register])
    require(len(text.splitlines())==1,'左轮原帧输出不是单行JSON')
    raw=strict_json(text)
    address,width=LEFT_FEEDBACK_META[register]
    require(isinstance(raw,dict) and set(raw)=={
        'operation','serial_device','motor_id','register','requested_register_address','width','raw_unsigned','tx','rx'}
        and all(type(raw[k]) is int for k in ('motor_id','requested_register_address','width'))
        and (raw['operation'],raw['serial_device'],raw['motor_id'],raw['register'],
             raw['requested_register_address'],raw['width'])==('sync_read',DEVICES['right'],7,register,address,width),
        '左轮原帧目标/寄存器不符')
    tx=[255,255,254,5,130,address,width,7]
    tx.append((~sum(tx[2:]))&255)
    rx=raw['rx'];value=raw['raw_unsigned']
    require(type(raw['tx']) is list and all(type(v) is int for v in raw['tx']) and raw['tx']==tx
            and type(rx) is list and all(type(v) is int and 0<=v<=255 for v in rx)
            and len(rx)==6+width and rx[:5]==[255,255,7,width+2,0] and sum(rx[2:])&255==255
            and type(value) is int and int.from_bytes(bytes(rx[5:5+width]),'little')==value,
            '左轮原帧ID/长度/错误码/校验/原字不符')
    return raw


def run_left_wheel_feedback(engine, check_go):
    """仅采集固定12读；非零PV不等于停止通过，异常不补读或重试。"""
    require(not engine.rows,'左轮只读单次序列已经执行')
    validate_left_feedback_plan(engine.plan)
    io=engine.io
    contract=engine.plan['left_wheel_feedback_only']
    reference=left_feedback_tool_ref(engine.plan)
    row={'side':'right','motor':7,'name':'base_left_wheel','kind':'feedback_only',
         'status':'FAIL','samples':[],'errors':[],'register_writes':0,'same_frame':False,
         'stop_verified':False,'uart_blocked':False,'started_ns':io.now()}
    engine.rows.append(row)
    deadline=row['started_ns']+contract['capture_budget_ns']
    row['deadline_ns']=deadline

    def ready():
        io.check()
        check_go()
        require(io.child is None and not io.evidence_errors,'左轮只读child/原件异常')
        require(io.now()<deadline,'左轮只读启动前已超窗')

    def read(register,offset=None):
        ready()
        sample={'register':register,'scheduled_offset_ns':offset,'started_ns':io.now(),'status':'UNKNOWN'}
        row['samples'].append(sample)
        try:
            raw=left_feedback_raw(io,reference,register,ready)
            sample.update(raw=raw,status='READ_VALID')
            # 原字与原帧先保留，再检查返回时授权和窗口；不为补末TE追加UART。
            sample['finished_ns']=io.now()
            sample['command_sequence']=io.sequence
            io.check()
            check_go()
            require(io.child is None and not io.evidence_errors,'左轮只读child/原件异常')
            require(io.now()<=deadline,'左轮只读返回超过采集窗口')
            if register=='Torque_Enable':
                require(raw['raw_unsigned']==0,'左轮TE非零，停止后续只读')
        except BaseException as error:
            sample.update(finished_ns=io.now(),error=repr(error))
            raise

    try:
        read('Torque_Enable')
        row['observation_started_ns']=io.now()
        for offset in contract['sample_offsets_ns']:
            wait=row['observation_started_ns']+offset-io.now()
            if wait>0:
                io.sleep(wait/1_000_000_000)
            read('Present_Position',offset)
            read('Present_Velocity',offset)
        read('Torque_Enable')
        row['status']='DIAGNOSTIC_CAPTURED'
    except BaseException as error:
        row['errors'].append(repr(error))
        row['uart_blocked']=True
        engine.manual_cut=True
        raise
    finally:
        row['finished_ns']=io.now()


def capture_right_pulse_wire(io,argv,original,attempt,**kwargs):
    """保留同次动作和右9普通TE原帧；抓帧故障不阻止独立TE0，绝不重发。"""
    reading=argv[3]=='read'
    label='read' if reading else 'write'
    def trace_error(error):
        attempt.setdefault('trace_errors',[]).append(repr(error))
        io.evidence_errors.append('right_pulse_'+label+'_trace:'+repr(error))

    try:
        verify_ref(RIGHT_PULSE_TRACE,executable=True)
        trace=io.output/f'right9-pulse-{label}-command_{io.sequence+1:04d}.strace'
        with trace.open('xb'):
            pass
    except BaseException as error:
        trace_error(error)
        if kwargs.get('cleanup') and argv[-3:]==['write','Torque_Enable','0']:
            # 这里尚未启动child：只执行这一笔原始断扭，不是失败后重发。
            attempt['untraced_disable_stdout']=original(argv,**kwargs)
        raise
    sync=argv[3] in ('sync-zero','sync-velocity')
    velocity=sync
    succeeded=False
    try:
        text=original([RIGHT_PULSE_TRACE['path'],'-ttt','-xx','-s','256','-yy','-e',
                       'trace=openat,close,read,write,ioctl','-o',str(trace),*argv],**kwargs)
        succeeded=True
        return text
    finally:
        try:
            info=trace.lstat()
            require(stat.S_ISREG(info.st_mode) and info.st_nlink==1,'动作strace原件身份错误')
            raw=trace.read_bytes()
            require(raw,'动作strace原件为空')
            if succeeded:
                require(re.search(rb'(?:^|\n)[^\n]*\+\+\+ exited with 0 \+\+\+\n?\Z',raw),
                        '动作strace未记录成功终态')
            evidence={'path':str(trace),'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest(),
                      'original_argv':list(argv),'requested_register_address':46 if velocity else 40,
                      'width':2 if velocity else 1,'instruction':0x83 if sync else (0x02 if reading else 0x03),
                      'motor_ids':[9],
                      'ack':'NONE_SYNC_WRITE' if sync else ('READ_STATUS_REQUIRED' if reading else 'ORDINARY_WRITE_ACK_REQUIRED'),
                      'capture_status':'NATURAL_SUCCESS_TERMINAL' if succeeded else 'ORIGINAL_COMMAND_FAILED',
                      'wire_frame_review':'PENDING_ROOT_REVIEW_OF_ACTUAL_WIRE_STRACE'}
            attempt['wire_trace']=evidence
            io.commands[-1]['ordinary_read_trace' if reading else 'diagnostic_write_trace']=evidence
        except BaseException as error:
            trace_error(error)
            if succeeded:
                raise


def install_right_pulse_guard(engine,helper):
    """只包本轮执行器；初始/最终停止未知立即封锁，动作错误仅保留一次固定收尾。"""
    io=engine.io
    original=io.command
    holder=SimpleNamespace(command=lambda _io,argv,**kwargs:original(argv,**kwargs))
    helper.install_stop_wire_capture(SimpleNamespace(RealExecutor=holder,verify_ref=verify_ref))
    traced=lambda argv,**kwargs:holder.command(io,argv,**kwargs)
    row={'side':'right','motor':9,'name':'base_right_wheel','kind':'raw_velocity_diagnostic',
         'status':'NOT_RUN','before_raw':[],'samples':[],'cleanup_raw':[],'writes':[],
         'errors':[],'cleanup_errors':[],'uart_blocked':False,'normal_failed':False,
         'metadata_verified':False,'goal_verified':False,'stopped_confirmed':False,
         'initial_zero_attempted':False,'initial_zero_verified':False,
         'enable_attempted':False,'enable_verified':False,'velocity_attempted':False,
         'off_attempted':False,'zero_attempted':False,'firmware_contract_verified':False}
    engine.right_pulse=row
    engine.right_pulse_helper=helper

    def block(error):
        row['uart_blocked']=True
        engine.manual_cut=True
        row['cleanup_errors'].append(repr(error))
        print('爸爸，右轮诊断停止/身份未知，请立即人工断电；禁止后续UART。',flush=True)

    engine.right_pulse_block=block
    tool_paths=(RIGHT_PULSE_WRITE['path'],RIGHT_PULSE_RAW['path'],RIGHT_PULSE_SYNC['path'],RIGHT_PULSE_TRACE['path'])

    def returned(text,cleanup):
        if not cleanup:
            check_go=getattr(engine,'right_pulse_go_check',None)
            if check_go is not None:
                check_go()
            if 'observation_deadline_ns' in row:
                require(io.now()<=row['observation_deadline_ns'],'右轮命令返回超过观察窗口')
        return text

    def command(argv,**kwargs):
        path=str(argv[0]) if argv else ''
        cleanup=kwargs.get('cleanup',False)
        if path not in tool_paths:
            require(not any(str(arg) in tool_paths for arg in argv[1:]),'禁止其他前缀绕过右轮工具名单')
            return original(argv,**kwargs)
        require(not row['uart_blocked'],'右轮诊断UART已粘性封锁')
        cleanup_zero=False
        try:
            require(path!=RIGHT_PULSE_TRACE['path'],'不得直接调用strace绕过命令名单')
            require(cleanup or not row['normal_failed'],'动作错误后禁止新常规UART')
            if not cleanup:
                check_go=getattr(engine,'right_pulse_go_check',None)
                if check_go is not None:
                    check_go()
                if 'observation_deadline_ns' in row:
                    require(io.now()<row['observation_deadline_ns'],'右轮命令开始前观察窗口已耗尽')
            zero=argv==[RIGHT_PULSE_RAW['path'],DEVICES['right'],'9','sync-zero']
            velocity=argv==[RIGHT_PULSE_SYNC['path'],DEVICES['right'],'9','sync-velocity','500']
            sync=zero or velocity
            writing=sync or (path==RIGHT_PULSE_WRITE['path'] and len(argv)>3 and argv[3]=='write')
            if writing:
                if zero:
                    if cleanup:
                        require(row['velocity_attempted'] and not row['zero_attempted'] and not row['off_attempted'],
                                '仅非零尝试后、独立断扭前一次收尾SYNC零目标')
                        row['zero_attempted']=True
                        cleanup_zero=True
                    else:
                        require(row['metadata_verified'] and not row['writes'],'仅初始一次SYNC零目标')
                        row['initial_zero_attempted']=True
                elif argv==[RIGHT_PULSE_WRITE['path'],DEVICES['right'],'9','write','Torque_Enable','1']:
                    require(not cleanup and row['initial_zero_verified'] and not row['enable_attempted'],
                            '仅初始零速读回后一次Enable')
                    row['enable_attempted']=True
                elif velocity:
                    require(not cleanup and row['enable_verified'] and not row['velocity_attempted']
                            and not row['off_attempted'],'仅Enable确认后一次SYNC+500')
                    row['velocity_attempted']=True
                elif argv==[RIGHT_PULSE_WRITE['path'],DEVICES['right'],'9','write','Torque_Enable','0']:
                    require(cleanup and row['writes'] and not row['off_attempted'],'仅一次独立Disable')
                    row['off_attempted']=True
                else:
                    raise ValueError('右轮诊断写目标/寄存器/数值越界')
                reference=RIGHT_PULSE_RAW if zero else RIGHT_PULSE_SYNC if velocity else RIGHT_PULSE_WRITE
                attempt={'argv':list(argv),'started_ns':io.now(),'cleanup':cleanup,'status':'UNKNOWN'}
                row['writes'].append(attempt)
                verify_ref(reference,executable=True)
                text=capture_right_pulse_wire(io,argv,original,attempt,**kwargs)
                expected=(f'{argv[3]} Goal_Velocity@9 <- {500 if velocity else 0}: SYNC_WRITE已发送，无ACK；未确认电机接收或实际速度\n'
                          if sync else f'write {argv[4]}@9 <- {argv[5]} ok\n')
                require(text==expected,'右轮写入回执未知')
                attempt.update(finished_ns=io.now(),status='SENT_NO_ACK' if sync else 'ACK_STDOUT_VALID')
                return returned(text,cleanup)
            if path==RIGHT_PULSE_RAW['path']:
                require(len(argv)==5 and argv[1:4]==[DEVICES['right'],'9','read-raw']
                        and argv[4] in helper.META
                        and (not cleanup or argv[4] in ('Torque_Enable','Present_Velocity','Goal_Velocity')),
                        '右轮原帧读取越界')
                return traced(argv,**kwargs)
            require(path==RIGHT_PULSE_WRITE['path'] and len(argv)==5 and argv[3]=='read'
                    and argv[4] in REGS and any(argv[1:3]==[DEVICES[side],str(motor)]
                    for side in SERIALS for motor in range(1,9 if side=='left' else 10)),
                    '普通READ不在原17电机名单')
            if argv==[RIGHT_PULSE_WRITE['path'],DEVICES['right'],'9','read','Torque_Enable']:
                text=capture_right_pulse_wire(io,argv,original,{},**kwargs)
            else:
                text=traced(argv,**kwargs)
            match=re.fullmatch(rf'read {re.escape(argv[4])}@{argv[2]} = (-?\d+)\n',text)
            require(match is not None,'普通READ回执未知')
            require(argv[4] not in STOP_REGS or int(match[1])==0,'原停止READ非零:'+argv[4])
            return returned(text,cleanup)
        except BaseException as error:
            row['normal_failed']=True
            # 动作中失败保留独立TE0；初始/最终普通READ未知立即阻止后续UART。
            ordinary_read=path==RIGHT_PULSE_WRITE['path'] and len(argv)>3 and argv[3]=='read'
            if ordinary_read or (cleanup and not cleanup_zero):
                block(error)
            raise

    io.command=command


def run_right_wheel_pulse(engine):
    row=engine.right_pulse
    io=engine.io
    engine.rows.append(row)
    row['status']='FAIL'
    helper=engine.right_pulse_helper
    module=SimpleNamespace(verify_ref=verify_ref,DEVICES=DEVICES,strict_json=strict_json)

    def read(register,destination,*,cleanup=False):
        sample={'register':register,'started_ns':io.now(),'status':'UNKNOWN'}
        destination.append(sample)
        try:
            raw=helper.raw_read(module,engine,register,cleanup=cleanup)
            sample.update(raw=raw,finished_ns=io.now(),status='READ_VALID')
            # 先保留本次已完整校验原帧，再检查返回时GO；过期仍进入独立TE0收尾。
            check_go=getattr(engine,'right_pulse_go_check',None)
            if not cleanup and check_go is not None:
                check_go()
            if not cleanup and 'observation_deadline_ns' in row:
                require(io.now()<=row['observation_deadline_ns'],'右轮原帧读取完成超窗')
            return raw['raw_unsigned']
        except BaseException:
            row['normal_failed']=True
            raise

    def write(register,value,*,cleanup=False):
        return io.command([RIGHT_PULSE_WRITE['path'],DEVICES['right'],'9','write',register,str(value)],cleanup=cleanup)

    try:
        engine.check_gate()
        for register,expected in {**RIGHT_PULSE_METADATA,'Torque_Enable':0,'Present_Velocity':0,'Goal_Velocity':0}.items():
            require(read(register,row['before_raw'])==expected,'右轮元数据/初始静止不符:'+register)
        anchor=read('Present_Position',row['before_raw'])
        require(0<=anchor<=4095,'右轮初始位置超单圈范围')
        row['anchor_raw']=anchor
        row['metadata_verified']=True
        io.command([RIGHT_PULSE_RAW['path'],DEVICES['right'],'9','sync-zero'])
        row['initial_zero_finished_ns']=io.now()
        io.sleep(0.05)
        require(read('Goal_Velocity',row['before_raw'])==0,'初始SYNC零目标读回不符')
        row['initial_zero_verified']=True
        started=io.now()
        row['enable_attempt_started_ns']=started
        deadline=started+500_000_000
        row['observation_deadline_ns']=deadline
        write('Torque_Enable',1)
        require(read('Torque_Enable',row['before_raw'])==1,'Enable读回未确认')
        row['enable_verified']=True
        io.command([RIGHT_PULSE_SYNC['path'],DEVICES['right'],'9','sync-velocity','500'])
        require(read('Goal_Velocity',row['before_raw'])==500,'右轮SYNC+500读回不符')
        row['goal_verified']=True
        for offset in (100_000_000,400_000_000):
            require(io.now()<deadline,'右轮观察窗口已耗尽')
            io.sleep(max(0,started+offset-io.now())/1_000_000_000)
            sample={'offset_ns':offset,'raw':[]}
            row['samples'].append(sample)
            require(io.now()<deadline,'右轮速度读取前已超窗')
            word=read('Present_Velocity',sample['raw'])
            velocity=-(word&0x7fff) if word&0x8000 else word
            require(io.now()<=deadline,'右轮速度读取完成超窗')
            require(0<=velocity<=1000,'右轮速度反向或超过固定raw上限')
            position=read('Present_Position',sample['raw'])
            require(io.now()<=deadline,'右轮位置读取完成超窗')
            require(0<=position<=4095,'右轮观察位置超单圈范围')
            delta=(position-anchor+2048)%4096-2048
            sample.update(velocity_raw=velocity,position_raw=position,delta_raw=delta)
            require(0<=delta<=512,'右轮位移反向或超过固定单圈差值上限')
        require(any(s['velocity_raw']>0 for s in row['samples'])
                and any(s['delta_raw']>0 for s in row['samples']),'右轮未观察到正向速度及位置变化')
    except BaseException as error:
        row['normal_failed']=True
        row['errors'].append(repr(error))
    finally:
        if row['writes']:
            zero_error=None
            if row['velocity_attempted']:
                try:
                    io.command([RIGHT_PULSE_RAW['path'],DEVICES['right'],'9','sync-zero'],cleanup=True)
                except BaseException as error:
                    # 清零错误不封住独立TE0；TE0后保持未知且不继续UART。
                    zero_error=error
                    row['cleanup_errors'].append(repr(error))
            try:
                # 与既有Python顺序一致：清零在前，独立断扭必须是最后一次动作写。
                write('Torque_Enable',0,cleanup=True)
                row['disable_finished_ns']=io.now()
                if 'enable_attempt_started_ns' in row:
                    elapsed=io.now()-row['enable_attempt_started_ns']
                    row['enabled_attempt_window_ns']=elapsed
                    if elapsed>1_000_000_000:
                        row['errors'].append('启用尝试到Disable完成超过1秒；非硬件自动截止保证')
                if zero_error is not None:
                    raise ValueError('收尾SYNC零目标未知，独立断扭后封锁UART') from zero_error
                io.sleep(0.05,cleanup=True)
                for register in ('Torque_Enable','Present_Velocity','Goal_Velocity'):
                    require(read(register,row['cleanup_raw'],cleanup=True)==0,'独立断扭停止未确认:'+register)
                row['stopped_confirmed']=True
            except BaseException as error:
                if not row['uart_blocked']:
                    engine.right_pulse_block(error)
        elif row['errors']:
            # 无写入仍保留未知/不符事实；不追加一轮UART尝试恢复结论。
            engine.right_pulse_block(ValueError(row['errors'][-1]))
    row['status']='PASS' if not row['errors'] and not row['cleanup_errors'] else 'FAIL'
    require(row['status']=='PASS','右轮单次非零诊断失败，禁止重试')


def inspect_unit(io,unit,*,cleanup=False):
    text = io.command(["systemctl", "show", unit, "--property=Id,LoadState,ActiveState,SubState,MainPID"],cleanup=cleanup)
    fields = dict(line.split("=", 1) for line in text.splitlines() if "=" in line)
    require(fields.get('Id') == unit, '服务Id不完整')
    if unit == 'xlerobot-handoff.service':
        require(fields.get('LoadState')=='loaded' and fields.get('ActiveState')=='active'
                and fields.get('SubState')=='exited' and fields.get('MainPID')=='0', 'handoff既有状态漂移')
    else:
        require(fields.get('LoadState') in (('loaded',) if unit in UNITS else ('loaded','not-found'))
                and fields.get('ActiveState')=='inactive' and fields.get('MainPID')=='0'
                and fields.get('SubState')=='dead', f'服务未停稳：{unit}')
    return fields


def inspect_processes(io,bindings,proc_helper,*,cleanup=False):
    def proc_probe(candidates):
        argv=['sudo','-n','/usr/bin/python3','-I','-S','-c',proc_helper.PROC_PROBE_CODE]
        request={'schema':'P7_PROC_PROBE_V1','candidates':candidates}
        raw=io.command(argv,input_raw=json.dumps(request).encode(),cleanup=cleanup)
        return {'exit_code':0,'response':strict_json(raw),'request':request}
    snapshot=proc_helper.process_snapshot(-1,io.output,[Path(p) for p in bindings['binary_paths']],probe_runner=proc_probe)
    io.process_snapshots.append(snapshot)
    require(snapshot['clean'], '控制/维护进程残留或身份不可读')
    return snapshot


def inspect_holder(io,device,elevated,*,cleanup=False):
    text=io.command((['sudo','-n'] if elevated else [])+['fuser',device],allowed=(1,),cleanup=cleanup)
    require(text=='','串口被占用')
    return {'path':device,'elevated':elevated,'stdout_empty':True,'exit_code':1}


def idle(io, bindings, proc_helper, *, cleanup=False):
    # 前置保持fail-fast；后验直接调用下面独立函数，逐项留证。
    for unit in UNITS + OLD_UNITS + ['xlerobot-handoff.service']:
        inspect_unit(io,unit,cleanup=cleanup)
    inspect_processes(io,bindings,proc_helper,cleanup=cleanup)
    for device in DEVICES.values():
        for elevated in (False,True):
            inspect_holder(io,device,elevated,cleanup=cleanup)


def usb_identity(io, side, expected, *, cleanup=False):
    text = io.command(["udevadm", "info", "--query=property", "--name", DEVICES[side]],cleanup=cleanup)
    fields = dict(line.split("=", 1) for line in text.splitlines() if "=" in line)
    require(fields.get("ID_SERIAL_SHORT") == SERIALS[side] and fields.get("ID_VENDOR_ID") == "1a86"
            and fields.get("ID_MODEL_ID") == "55d3" and fields.get("ID_USB_DRIVER") == "cdc_acm", "USB身份改变")
    require(fields.get('DEVPATH')==expected['devpath'] and fields.get('USEC_INITIALIZED')==expected['usec_initialized'],
            'USB枚举代次改变')
    require(str(Path(DEVICES[side]).resolve()) == expected['resolved'], "USB tty映射改变")
    info=Path(DEVICES[side]).stat()
    require(stat.S_ISCHR(info.st_mode) and info.st_rdev==expected['rdev']
            and os.access(DEVICES[side],os.R_OK|os.W_OK),'USB设备类型/权限改变')
    return {'serial':fields['ID_SERIAL_SHORT'],'devpath':fields['DEVPATH'],'usec_initialized':fields['USEC_INITIALIZED'],
            'resolved':str(Path(DEVICES[side]).resolve()),'rdev':info.st_rdev}


def protected_and_refs(bindings):
    for reference in bindings['refs']:
        verify_ref(reference)
    for side, files in bindings['protection'].items():
        for name, reference in files.items():
            verify_protection_file(side,name,reference)


def verify_protection_file(side,name,reference):
    data = verify_ref(reference)
    info = Path(reference['path']).lstat()
    require(info.st_uid == reference['uid'] and info.st_gid == reference['gid']
            and stat.S_IMODE(info.st_mode) == reference['mode'] and info.st_ino == reference['inode'],
            f"保护身份变化：{side}/{name}")
    if name in ('rx_trust_marker', 'rx_trust_marker.bak', 'estop_latch', 'estop_latch.bak'):
        require(data == b'1\n', "必须保留既有Dirty/EStop，不clear保护")
    return {'path':reference['path'],'sha256':hashlib.sha256(data).hexdigest(),'bytes':len(data),
            'uid':info.st_uid,'gid':info.st_gid,'mode':stat.S_IMODE(info.st_mode),'inode':info.st_ino}


def snapshot(io, *, cleanup=False):
    values, errors={},[]
    for side in SERIALS:
        for motor in range(1,9 if side=='left' else 10):
            row={}
            values[f'{side}:{motor}']=row
            for register in REGS:
                try:
                    row[register]=io.poke(side,motor,'read',register,cleanup=cleanup)
                except BaseException as error:
                    errors.append({'motor':f'{side}:{motor}','register':register,'error':repr(error)})
    return values,errors


def classify_stop_snapshot(values, read_errors=(), cleanup_errors=(), *, child_active=False):
    """停止证据只含固定17电机的3个停止寄存器；精度/位置字段不参与。"""
    issues=[]; observed={}
    if not isinstance(values,dict):
        return {'status':'UNKNOWN','issues':[{'code':'STOP_SNAPSHOT_MISSING'}],'observed':{},'motors_expected':17}
    if set(values)!=set(MOTOR_KEYS):
        issues.append({'code':'STOP_MOTOR_SET_INCOMPLETE','status':'UNKNOWN'})
    for key in MOTOR_KEYS:
        row=values.get(key,{})
        row=row if isinstance(row,dict) else {}
        observed[key]={}
        for register in STOP_REGS:
            actual=row.get(register)
            observed[key][register]=actual
            if type(actual) is not int:
                issues.append({'code':'STOP_REGISTER_UNKNOWN','status':'UNKNOWN','motor':key,'register':register,'actual':actual})
            elif actual!=0:
                issues.append({'code':'STOP_REGISTER_NONZERO','status':'FAIL','motor':key,'register':register,'actual':actual})
    for error in read_errors:
        if not isinstance(error,dict) or error.get('register') in STOP_REGS:
            issues.append({'code':'STOP_READ_FAILED','status':'UNKNOWN','read_error':error})
    for error in cleanup_errors:
        issues.append({'code':'STOP_CLEANUP_FAILED','status':'FAIL','cleanup_error':error})
    if child_active:
        issues.append({'code':'STOP_CHILD_NOT_REAPED','status':'UNKNOWN'})
    status='UNKNOWN' if any(e['status']=='UNKNOWN' for e in issues) else 'FAIL' if issues else 'PASS'
    return {'status':status,'issues':issues,'observed':observed,'motors_expected':17,'registers':list(STOP_REGS)}


def classify_position_snapshot(values,before,probes):
    """误差与非目标变化保留独立FAIL；停止寄存器完整通过不抹掉精度错误。"""
    issues=[]
    targets={(row['side'],row['motor']):row for row in probes
             if row.get('status')!='NOT_RUN' and row.get('decision')!='DEFERRED'}
    for probe in probes:
        if probe.get('position_accuracy',{}).get('status')=='FAIL':
            issues.append({'code':'POSITION_ACCURACY_FAILED','status':'FAIL','motor':f"{probe['side']}:{probe['motor']}",
                           'evidence':probe['position_accuracy']})
    for key in MOTOR_KEYS:
        side,motor=key.split(':'); motor=int(motor)
        row=values.get(key,{}); old=before.get(key,{})
        actual=row.get('Present_Position'); goal=row.get('Goal_Position')
        if type(actual) is not int or type(goal) is not int or not 0<=actual<=4095:
            issues.append({'code':'POSITION_REGISTER_UNKNOWN','status':'UNKNOWN','motor':key})
            continue
        probe=targets.get((side,motor))
        if probe is not None and probe['kind']=='position' and 'anchor_raw' in probe:
            anchor=probe['anchor_raw']
            if goal!=anchor or abs(actual-anchor)>4:
                issues.append({'code':'POSITION_FINAL_ANCHOR_ERROR','status':'FAIL','motor':key,'target_raw':anchor,
                               'actual_raw':actual,'goal_raw':goal,'signed_error_raw':actual-anchor,'tolerance_raw':4})
        else:
            if type(old.get('Goal_Position')) is not int or type(old.get('Present_Position')) is not int:
                issues.append({'code':'POSITION_BASELINE_UNKNOWN','status':'UNKNOWN','motor':key})
                continue
            if goal!=old['Goal_Position']:
                issues.append({'code':'NONTARGET_GOAL_CHANGED','status':'FAIL','motor':key,'actual_raw':goal,'expected_raw':old['Goal_Position']})
            if probe is None and abs(actual-old['Present_Position'])>4:
                issues.append({'code':'NONTARGET_POSITION_CHANGED','status':'FAIL','motor':key,
                               'signed_error_raw':actual-old['Present_Position'],'tolerance_raw':4})
    status='UNKNOWN' if any(e['status']=='UNKNOWN' for e in issues) else 'FAIL' if issues else 'PASS'
    return {'status':status,'issues':issues}


def classify_mode_snapshot(values):
    issues=[]
    for key in MOTOR_KEYS:
        side,motor=key.split(':'); expected=1 if side=='right' and int(motor)>=7 else 0
        actual=values.get(key,{}).get('Operating_Mode')
        if type(actual) is not int:
            issues.append({'code':'MODE_UNKNOWN','status':'UNKNOWN','motor':key})
        elif actual!=expected:
            issues.append({'code':'MODE_CHANGED','status':'FAIL','motor':key,'actual':actual,'expected':expected})
    return {'status':'UNKNOWN' if any(e['status']=='UNKNOWN' for e in issues) else 'FAIL' if issues else 'PASS','issues':issues}


def finish_verification(report,io,engine,bindings,proc_helper):
    """每个后验独立尝试；精度失败不能短路停止/身份后验。"""
    try:
        values,read_errors=snapshot(io,cleanup=True)
    except BaseException as error:
        values,read_errors={},[{'code':'FINAL_SNAPSHOT_ERROR','error':repr(error)}]
    report['after'],report['after_errors']=values,read_errors
    cleanup_errors=[{'motor':f"{row['side']}:{row['motor']}",'error':error}
                    for row in engine.rows for error in row.get('cleanup_errors',[])]
    if engine.manual_cut:
        cleanup_errors.append({'error':'已发生未确认的安全收尾，粘性失败不被最终零值覆盖'})
    report['stop_verification']=classify_stop_snapshot(values,read_errors,cleanup_errors,child_active=io.child is not None)
    try:
        report['position_verification']=classify_position_snapshot(values,report.get('before',{}),engine.rows)
    except BaseException as error:
        report['position_verification']={'status':'UNKNOWN','issues':[{'code':'POSITION_VERIFIER_ERROR','error':repr(error)}]}
    identity_checks=[]
    try:
        mode=classify_mode_snapshot(values)
        identity_checks.append({'name':'modes',**mode})
    except BaseException as error:
        identity_checks.append({'name':'modes','status':'UNKNOWN','error':repr(error)})
    finish_identity_verification(report,io,bindings,proc_helper,identity_checks)
    if report['stop_verification']['status']!='PASS' or report['identity_verification']['status']!='PASS':
        print('爸爸，停止或身份后验未确认，请立即人工断电；禁止后续运动。',flush=True)


def finish_identity_verification(report,io,bindings,proc_helper,identity_checks=None):
    """复用非UART后验；只读序列不借此产生停止判定。"""
    if identity_checks is None:
        identity_checks=[]
    tasks=[(f"ref:{reference['path']}",lambda reference=reference:verify_ref(reference)) for reference in bindings['refs']]
    tasks += [(f'protection:{side}/{name}',lambda side=side,name=name,reference=reference:verify_protection_file(side,name,reference))
              for side,files in bindings['protection'].items() for name,reference in files.items()]
    tasks += [(f'service:{unit}',lambda unit=unit:inspect_unit(io,unit,cleanup=True))
              for unit in UNITS+OLD_UNITS+['xlerobot-handoff.service']]
    tasks.append(('process_snapshot',lambda:inspect_processes(io,bindings,proc_helper,cleanup=True)))
    tasks += [(f"holder:{side}:{'sudo' if elevated else 'ordinary'}",
               lambda side=side,elevated=elevated:inspect_holder(io,DEVICES[side],elevated,cleanup=True))
              for side in SERIALS for elevated in (False,True)]
    tasks += [(f'usb:{side}',lambda side=side:usb_identity(io,side,bindings['usb'][side],cleanup=True)) for side in SERIALS]
    for name,verify in tasks:
        try:
            evidence=verify()
            # 文件原字节保存在绑定SHA，避免把二进制写入JSON；读回/进程元数据结构化保留。
            identity_checks.append({'name':name,'status':'PASS',
                                    'evidence':{'bytes':len(evidence),'sha256':hashlib.sha256(evidence).hexdigest()}
                                    if isinstance(evidence,bytes) else evidence})
        except BaseException as error:
            identity_checks.append({'name':name,'status':'FAIL' if isinstance(error,ValueError) else 'UNKNOWN','error':repr(error)})
    if io.evidence_errors:
        identity_checks.append({'name':'original_evidence','status':'UNKNOWN','errors':list(io.evidence_errors)})
    else:
        identity_checks.append({'name':'original_evidence','status':'PASS'})
    identity_status='UNKNOWN' if any(c['status']=='UNKNOWN' for c in identity_checks) else 'FAIL' if any(c['status']=='FAIL' for c in identity_checks) else 'PASS'
    report['identity_verification']={'status':identity_status,'checks':identity_checks}


def classify_run_result(report):
    stop=report.get('stop_verification',{}).get('status','UNKNOWN')
    identity=report.get('identity_verification',{}).get('status','UNKNOWN')
    if stop!='PASS' or identity!='PASS' or report.get('child_active'):
        return 'NEEDS_MANUAL_POWER_CUT'
    probes=report.get('probes',[])
    if (report.get('error') or report.get('received_signal') is not None
            or report.get('position_verification',{}).get('status')!='PASS'
            or len(probes)!=7 or any(p['status']!='PASS' for p in probes) or report.get('evidence_errors')):
        return 'FAIL'
    if any(t.get('decision')=='DEFERRED' for t in report.get('plan',{}).get('base_targets',[])):
        return 'FAIL'
    return 'PASS'


def run_motion_probes(engine,bindings,plan):
    if 'right_wheel_pulse' in plan:
        validate_right_pulse_plan(plan)
        run_right_wheel_pulse(engine)
    for side,motor,name in position_targets_from_plan(plan):
        engine.position(side,motor,name,bindings['calibrations'][name])
    # 固定拒绝未闭合的固件合同；任何bindings里的verified布尔值都不是可接受证明。
    # 动作分支不调用wheel/pulse，无底盘motion WRITE/enable；17电机固定快照仍只读。
    for side,motor,name in BASE_TARGETS:
        engine.rows.append({'side':side,'motor':motor,'name':name,'kind':'velocity','status':'NOT_RUN',
                            'decision':'DEFERRED','reason_code':'BASE_FIRMWARE_CONTRACT_UNVERIFIED'})
    raise ValueError('BASE_FIRMWARE_CONTRACT_UNVERIFIED：底盘三项DEFERRED，未调用wheel/pulse')


def validate_snapshot(values, before=None, probes=()):
    targets={(row['side'],row['motor']):row for row in probes
             if row.get('status')!='NOT_RUN' and row.get('decision')!='DEFERRED'}
    for key,row in values.items():
        side,motor=key.split(':'); motor=int(motor)
        require(set(row)==set(REGS), f'寄存器缺失：{key}')
        require(row['Torque_Enable']==row['Present_Velocity']==row['Goal_Velocity']==0, f'非安全静止：{key}')
        require(row['Operating_Mode']==(1 if side=='right' and motor>=7 else 0),f'mode漂移：{key}')
        require(0<=row['Present_Position']<=4095, f'位置范围未知：{key}')
        if before is None:
            continue
        old=before[key]
        probe=targets.get((side,motor))
        if probe is not None and probe['kind']=='position' and 'anchor_raw' in probe:
            require(row['Goal_Position']==probe['anchor_raw']
                    and abs(row['Present_Position']-probe['anchor_raw'])<=4,f'最终锚点未确认：{key}')
        else:
            require(row['Goal_Position']==old['Goal_Position'],f'非目标GoalPosition变化：{key}')
            if probe is None:
                require(abs(row['Present_Position']-old['Present_Position'])<=4,f'非目标位置变化：{key}')


def execute(plan, go, approval):
    require(plan['schema']=='MAINTENANCE_PLAN_V2','拒绝旧V1计划')
    position_targets_from_plan(plan)
    bindings = plan['bindings']
    host = bindings['host']
    require(sys.platform == 'linux' and os.uname().machine == 'aarch64'
            and os.uname().nodename == host['hostname'] and os.getuid() == os.geteuid() == host['uid']
            and os.getgid() == os.getegid() == host['gid']
            and Path('/proc/sys/kernel/random/boot_id').read_text().strip() == host['boot_id'], "真实主机身份不符")
    validate_go(go, plan, time.time_ns())
    feedback_only='left_wheel_feedback_only' in plan
    pulse_helper=load_right_pulse_helper() if 'right_wheel_pulse' in plan else None
    script=Path(__file__).read_bytes()
    require(hashlib.sha256(script).hexdigest()==plan['script_sha256'] and len(script)==plan['script_bytes'], '执行脚本SHA/bytes变化')
    root=plan['artifact_root']
    output=artifact_path(plan['output'],root)
    approval=artifact_path(approval,root,existing=True)
    consumed=artifact_path(str(approval)+'.consumed',root)
    require(output != approval.parent and not output.is_relative_to(approval),'输出/GO路径冲突')
    output.mkdir(mode=0o750, parents=False, exist_ok=False)
    # 输出目录及独立claim均不覆盖；前置失败也消费本次GO，不静默重试。
    write_new(consumed, {'plan_sha256':digest(plan),'consumed_at_ns':time.time_ns()})
    io = RealExecutor(bindings['tool'], plan['output'])
    io.process_snapshots=[]
    for signum in (signal.SIGINT,signal.SIGTERM,signal.SIGHUP,signal.SIGQUIT):
        signal.signal(signum,io.mark_signal)
    engine = None
    report = {'schema':'MAINTENANCE_RUN_V2','plan':plan,'go':go,'result':'FAIL','hardware_layer':'直接wire维护',
              'S0_S1_head_base_integration':False,'H2_D15_closed':False,'deployable':False}
    if feedback_only:
        report['stop_verification']={'status':'NOT_EVALUATED_DIAGNOSTIC_ONLY'}
        report['position_verification']={'status':'NOT_EVALUATED_DIAGNOSTIC_ONLY'}
    try:
        with contextlib.ExitStack() as stack:
            protected_and_refs(bindings)
            verify_ref(bindings['proc_helper'])
            spec=importlib.util.spec_from_file_location('maintenance_proc_identity',bindings['proc_helper']['path'])
            proc_helper=importlib.util.module_from_spec(spec); spec.loader.exec_module(proc_helper)
            # 瞬时占用门先查USB及四锁；持有后不把自己的锁判为空。
            idle(io,bindings,proc_helper)
            for side in SERIALS:
                for name in ('rx_trust.lock','estop_latch.lock'):
                    for prefix in ([],['sudo','-n']):
                        require(io.command(prefix+['fuser',bindings['protection'][side][name]['path']],allowed=(1,))=='',
                                '保护锁有持有者')
            report['owned_locks']=[]
            for side in SERIALS:
                for name in ('rx_trust.lock','estop_latch.lock'):
                    reference=bindings['protection'][side][name]
                    lock=stack.enter_context(Path(reference['path']).open('rb'))
                    fcntl.flock(lock.fileno(),fcntl.LOCK_EX|fcntl.LOCK_NB)
                    report['owned_locks'].append({'side':side,'name':name,'path':reference['path'],'owner_pid':os.getpid(),
                                                  'inode':os.fstat(lock.fileno()).st_ino,'exclusive_flock':True})
            def gate():
                io.check()
                validate_go(go,plan,time.time_ns())
                protected_and_refs(bindings)
                idle(io,bindings,proc_helper)
                for side in SERIALS:
                    usb_identity(io,side,bindings['usb'][side])
            gate()
            engine=ProbeEngine(io,plan,gate)
            if pulse_helper is not None:
                install_right_pulse_guard(engine,pulse_helper)
                engine.right_pulse_go_check=lambda:validate_go(go,plan,time.time_ns())
            try:
                if feedback_only:
                    run_left_wheel_feedback(engine,lambda:validate_go(go,plan,time.time_ns()))
                else:
                    report['before'],report['before_errors']=snapshot(io)
                    require(not report['before_errors'],'初始17电机读取失败')
                    validate_snapshot(report['before'])
                    run_motion_probes(engine,bindings,plan)
            finally:
                if feedback_only:
                    # 仅非UART后验；异常后不补读末TE，也不调用停止/恢复写。
                    finish_identity_verification(report,io,bindings,proc_helper)
                else:
                    # 仍持四合作锁完成三类独立后验；不把精度误差当作停止证据失败。
                    finish_verification(report,io,engine,bindings,proc_helper)
            io.check()
            require(not io.evidence_errors,'原始证据保存失败')
            report['result']='PASS'
    except BaseException as error:
        report['error']=repr(error)
    finally:
        if engine:
            report['probes']=engine.rows
        report['received_signal']=io.interrupted
        report['child_active']=io.child is not None
        if io.child is not None:
            report['result']='NEEDS_MANUAL_POWER_CUT'
        report['commands']=io.commands
        report['process_snapshots']=io.process_snapshots
        report['evidence_errors']=io.evidence_errors
        if feedback_only:
            captured=(engine is not None and len(engine.rows)==1
                      and engine.rows[0]['status']=='DIAGNOSTIC_CAPTURED'
                      and len(engine.rows[0]['samples'])==12 and not engine.manual_cut)
            valid=(captured and not report.get('error') and io.interrupted is None
                   and io.child is None and not io.evidence_errors
                   and report.get('identity_verification',{}).get('status')=='PASS')
            report['result']='DIAGNOSTIC_CAPTURED' if valid else 'NEEDS_MANUAL_POWER_CUT'
            if not valid:
                print('爸爸，左轮只读采集或身份失败，请立即人工断电；禁止后续UART。',flush=True)
        elif engine:
            report['result']=classify_run_result(report)
        report['manual_power_cut_required']=report['result']=='NEEDS_MANUAL_POWER_CUT'
        # 先确认journal关闭结果再写最终报告，不能留下PASS原件后才因关闭失败exit1。
        try:
            io.journal.close()
        except BaseException as error:
            report['journal_close_error']=repr(error)
            if engine:
                identity=report.setdefault('identity_verification',{'checks':[]})
                identity['status']='UNKNOWN'
                identity['checks'].append({'name':'journal_close','status':'UNKNOWN','error':repr(error)})
                report['result']='NEEDS_MANUAL_POWER_CUT'
                report['manual_power_cut_required']=True
            else:
                report['result']='FAIL'
            print(f'爸爸，journal关闭失败，原始证据不完整：{error!r}',flush=True)
        try:
            write_new(Path(plan['output'])/'run.json',report)
        except BaseException as error:
            if feedback_only:
                report['result']='NEEDS_MANUAL_POWER_CUT'
                report['manual_power_cut_required']=True
            elif report['result'] != 'NEEDS_MANUAL_POWER_CUT':
                report['result']='FAIL'
            report['report_write_error']=repr(error)
            print(json.dumps(report,ensure_ascii=False),flush=True)
    return 0 if report['result'] in ('PASS','DIAGNOSTIC_CAPTURED') else 1


def main(argv=None):
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bindings',type=Path)
    parser.add_argument('--output',type=Path)
    parser.add_argument('--run-id')
    parser.add_argument('--artifact-root',type=Path)
    parser.add_argument('--execute',action='store_true')
    parser.add_argument('--approval',type=Path)
    parser.add_argument('--position-target', action='append', choices=tuple(POSITION_TARGETS),
                        help='可重复指定腕部/头部目标，按指定顺序执行；缺省为原四项')
    parser.add_argument('--right-wheel-pulse',action='store_true',
                        help='仅右9固定+500原始速度单次500ms诊断；禁止混用位置选择')
    parser.add_argument('--left-wheel-feedback-only',action='store_true',
                        help='仅左轮right7固定12次只读时序；不写电机、不判停止通过')
    options=parser.parse_args(argv)
    require(not (options.right_wheel_pulse and options.left_wheel_feedback_only)
            and not ((options.right_wheel_pulse or options.left_wheel_feedback_only)
                     and options.position_target is not None),'轮诊断禁止互相混用或混用位置选择')
    selected_targets = [] if options.right_wheel_pulse or options.left_wheel_feedback_only else select_position_targets(options.position_target)
    if not all((options.bindings,options.output,options.run_id,options.artifact_root)):
        require(not options.execute,'执行参数不完整')
        print(json.dumps({'result':'PLAN_ONLY','actions_started':0,'required':['bindings','output','run-id','artifact-root'],
                          'position_targets':selected_targets,
                          'scope':'单电机独立维护；不运行硬件、不改保护'},ensure_ascii=False))
        return 0
    plan=make_plan(strict_json(options.bindings.read_text()),options.output,options.run_id,options.artifact_root,
                   options.position_target,right_wheel_pulse=options.right_wheel_pulse,
                   left_wheel_feedback_only=options.left_wheel_feedback_only)
    if not options.execute:
        print(json.dumps({'result':'PLAN_ONLY','actions_started':0,'plan_sha256':digest(plan),'plan':plan},ensure_ascii=False,indent=2))
        return 0
    require(options.approval is not None,'必须指定root本次GO')
    return execute(plan,strict_json(options.approval.read_text()),options.approval)


if __name__=='__main__':
    try:
        result=main()
    except Exception as error:
        print(f'爸爸，前置拒绝：{error}',file=sys.stderr)
        sys.exit(2)
    sys.exit(result)
