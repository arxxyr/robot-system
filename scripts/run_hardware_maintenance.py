#!/usr/bin/env -S uv run --no-project python
"""爸爸，受监督直接wire维护；默认PLAN_ONLY，精度与停机证据分别判定。"""
import argparse
import contextlib
import fcntl
import hashlib
import importlib.util
import importlib
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
import tomllib
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
MIXED_CHILD_BUDGET_NS = 28_000_000_000
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
# 运行入口显式加载同一份总线配置；导入模块不读取默认文件。
BASE_TARGETS = []
WHEEL_PULSE_TARGETS = {}
WHEEL_STOP_TARGETS = {}
BUS_CONFIG = None
WHEEL_ROLES = ('front','left','right')
WHEEL_LABELS = {'front':'前','left':'左','right':'右'}
DEFAULT_BUS_CONFIG = Path(__file__).resolve().parents[1]/'configs/xlerobot_bus2_full.toml'
RIGHT_PULSE_SCOPE_TEMPLATE = '仅{target}复用已装SDK同一连接：初始TE/GV0，PV0快速通过或±50经一次14读位置稳定性复核→SYNC零速确认→Enable确认→+500原始值持续发送20秒→一次SYNC零速及独立Disable→最多1秒减速观察，PV0通过或首次±50经一次独立14读稳定性复核→整机停止检查（三轮各按同一获准稳定性判据）；SDK稳定性窗口各为1秒五组、1.5秒预算且PP跨度≤1，断扭后窗口可独立追加最多1.5秒，不续期原减速预算或GO；三轮稳定架空并持续观察'
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
# 仅导入现场已核对的SDK源码，不执行旧venv的site/.pth或机器人包。
RIGHT_PULSE_SDK = {'site': '/home/loosqk/repo/dimensional-applications/.venv/lib/python3.12/site-packages',
 'sdk_version': '1.0.0',
 'serial_version': '3.5',
 'refs': [{'path': '/home/loosqk/repo/dimensional-applications/.venv/lib/python3.12/site-packages/scservo_sdk/__init__.py',
           'sha256': 'c3047515bd2992e6fc209386f11fd4c80722c519b21be2a3a7a91f2d96222941',
           'bytes': 148},
          {'path': '/home/loosqk/repo/dimensional-applications/.venv/lib/python3.12/site-packages/scservo_sdk/group_sync_read.py',
           'sha256': 'ccbbb3a864c047bf3a7808a0b455676b198645ee1caffa58dac562339a86ad76',
           'bytes': 3563},
          {'path': '/home/loosqk/repo/dimensional-applications/.venv/lib/python3.12/site-packages/scservo_sdk/group_sync_write.py',
           'sha256': '70d2a9e2157692932dd6e25515e53d600da9ca87ffab656c4660fb0de4d03476',
           'bytes': 2023},
          {'path': '/home/loosqk/repo/dimensional-applications/.venv/lib/python3.12/site-packages/scservo_sdk/packet_handler.py',
           'sha256': '96bb801133436122d39e868bc6492a226222a737c04560ea7a02d0c5eacef2fd',
           'bytes': 273},
          {'path': '/home/loosqk/repo/dimensional-applications/.venv/lib/python3.12/site-packages/scservo_sdk/port_handler.py',
           'sha256': 'c8dbb189ca98705bd71066d5a9a691b4c80b484c77ebe8a9ccc2d6c46321e999',
           'bytes': 3168},
          {'path': '/home/loosqk/repo/dimensional-applications/.venv/lib/python3.12/site-packages/scservo_sdk/protocol_packet_handler.py',
           'sha256': '9932c85b9e2ac671a7e33f39e8e80b05d6592280d9f2bcebd85103845e36e655',
           'bytes': 16331},
          {'path': '/home/loosqk/repo/dimensional-applications/.venv/lib/python3.12/site-packages/scservo_sdk/scservo_def.py',
           'sha256': '068f7ad5c453ad6e338543c706047253495fcd6bf9a632465501ace12fee0627',
           'bytes': 1710},
          {'path': '/home/loosqk/repo/dimensional-applications/.venv/lib/python3.12/site-packages/serial/__init__.py',
           'sha256': '5dec897fdef45a0eaf63eacda1e01d62e1e75490302584ae0238f75376592344',
           'bytes': 3212},
          {'path': '/home/loosqk/repo/dimensional-applications/.venv/lib/python3.12/site-packages/serial/serialposix.py',
           'sha256': '5d56f98513391e1766a1847a04e2e1202ced4172a152803ba1c823a149a9b6f9',
           'bytes': 35127},
          {'path': '/home/loosqk/repo/dimensional-applications/.venv/lib/python3.12/site-packages/serial/serialutil.py',
           'sha256': '3c84f8c7c319f161a85d6f8db5bedc4f65255714589bdcbeee6f2daf65a17c0a',
           'bytes': 21797},
          {'path': '/home/loosqk/repo/dimensional-applications/.venv/lib/python3.12/site-packages/feetech_servo_sdk-1.0.0.dist-info/METADATA',
           'sha256': '2a313b217816c9abbb1dd62f1fb75c2655e00da7c86cd52e005fec595b196af8',
           'bytes': 3412},
          {'path': '/home/loosqk/repo/dimensional-applications/.venv/lib/python3.12/site-packages/pyserial-3.5.dist-info/METADATA',
           'sha256': '42a8ab7e94ef0b7baa7e94cdac65d6b9254c608476f630120c990007bf472943',
           'bytes': 1650}]}
RIGHT_SDK_REGS = {'Firmware_Major_Version':(0,1),'Firmware_Minor_Version':(1,1),
                  'Model_Number':(3,2),'Phase':(18,1),'Operating_Mode':(33,1),
                  'Torque_Enable':(40,1),'Goal_Velocity':(46,2),'Lock':(55,1),
                  'Present_Position':(56,2),'Present_Velocity':(58,2)}
RIGHT_FEEDBACK_SCOPE_TEMPLATE = '仅{target}轮right{motor}先读TE0/GV0，0/250/500/750/1000ms各读PP再PV，末读GV0/TE0，共14次只读；PV限0/±50且PP跨度≤1，仅判物理7稳定性候选，不判整机停止、不写电机'
FRONT_FEEDBACK_SCOPE_TEMPLATE = '仅{target}轮right{motor}以SYNC_READ先读TE0/GV0，0/250/500/750/1000ms各读PP再PV，末读GV0/TE0，共14次只读；PV原字及PP跨度只作诊断，不判停止、不写电机；三轮稳定架空并持续观察'
RIGHT_FEEDBACK_SCOPE = FRONT_FEEDBACK_SCOPE = LEFT_FEEDBACK_SCOPE = None
MIXED_RAW_SCOPE_TEMPLATE = ('仅右总线真实FeetechAdapter维护：六臂1..6取启用前新鲜当前位置为固定锚点，'
                   '校准往返不改原字；{held}保持raw0，{selected}固定raw+500持续20秒，随后raw0观察500毫秒及九电机TE0；'
                   '原17电机停止、三轮独立14读、六臂最终GP等于锚点且PP误差±4及非目标GP保持；'
                   '仅FW3.10/Phase12/Mode1的raw{{0,+500}}合同，不声明SI、SerialTick或S0/S1接入完成')
MIXED_RAW_SCOPE = None
MIXED_REF_IDENTITY = ('uid','gid','mode','device','inode','mtime_ns','ctime_ns','nlink','symlink')
MIXED_ARM_NAMES = ('right_arm_shoulder_pan','right_arm_shoulder_lift','right_arm_elbow_flex',
                   'right_arm_wrist_flex','right_arm_wrist_roll','right_arm_gripper')
ARM_RANGE_REGS = ('Torque_Enable','Present_Velocity','Present_Position')
ARM_RANGE_SCOPE = ('仅右臂物理1..6逐个以单ID SYNC_READ读取TE→PV→PP，共18次只读；TE/PV严格0，'
                   'PP须在0..4095并按原六项校准记录INSIDE/OUTSIDE；不写电机、不判整机停止，'
                   '不提供混合动作的新鲜锚点或动作授权')
GRIPPER_TOLERANCE_RAW = 5
GRIPPER_MAX_INCREMENT_RAW = 256
GRIPPER_TARGETS = {goal:{'goal_raw':goal,'direction':direction,
    'scope':(f'仅右夹爪物理6：原校准范围内新鲜静止初值到固定GP{goal}单程{action}，'
             f'绝对增量须1..{GRIPPER_MAX_INCREMENT_RAW}，不回初值；'
             'GP写自动使能，从首次写尝试起800毫秒软件观测预算，随后独立断扭及原整机停止后验')}
    for goal,action,direction in ((1600,'张开',1),(1397,'合回',-1))}
GRIPPER_RELEASE_SCOPE = GRIPPER_TARGETS[1600]['scope']
LEFT_FEEDBACK_META = {'Torque_Enable':(40,1),'Goal_Velocity':(46,2),
                      'Present_Position':(56,2),'Present_Velocity':(58,2)}
LEFT_FEEDBACK_IDENTITY = ('uid','gid','mode','device','inode','mtime_ns','ctime_ns','nlink','symlink','elf_machine')


def process_name_warning(error):
    """显示名失败只告警，不阻断独立停止命令。"""
    with contextlib.suppress(OSError, ValueError):
        print(f'爸爸，进程显示名设置失败，继续使用原入口：{error!r}',file=sys.stderr,flush=True)


def set_process_name():
    """只在入口启动时设置Linux显示名，导入模块没有命名副作用。"""
    if sys.platform=='linux':
        try:
            Path('/proc/self/comm').write_text('robot-maint',encoding='ascii')
        except OSError as error:
            process_name_warning(error)


def configure_bus_mapping(path):
    """唯一映射源为运行时指定TOML的joints/motor_ids，不读取校准标签推断轮位。"""
    global BASE_TARGETS, WHEEL_PULSE_TARGETS, WHEEL_STOP_TARGETS, BUS_CONFIG
    global RIGHT_FEEDBACK_SCOPE, FRONT_FEEDBACK_SCOPE, LEFT_FEEDBACK_SCOPE, MIXED_RAW_SCOPE
    actual=Path(path).resolve(strict=True)
    require(actual.is_file(),'右总线配置必须为普通文件')
    data=actual.read_bytes();document=tomllib.loads(data.decode('utf-8'))
    hardware=document.get('hardware')
    require(type(hardware) is list and len(hardware)==1,'维护配置必须恰含一条右总线')
    row=hardware[0];names=row.get('joints');ids=row.get('motor_ids')
    require(row.get('adapter')=='feetech_sts3215' and row.get('address')==DEVICES['right'],
            '维护配置必须为既有右总线设备与Feetech适配器')
    require(type(names) is list and type(ids) is list and len(names)==len(ids)==9
            and all(type(name) is str for name in names) and len(set(names))==9
            and all(type(motor) is int for motor in ids) and len(set(ids))==9,
            '总线joints/motor_ids必须为九个唯一名称及整数ID')
    mapping=dict(zip(names,ids))
    require(set(mapping)==set(MIXED_ARM_NAMES)|{f'base_{role}_wheel' for role in WHEEL_ROLES}
            and all(mapping[name]==motor for motor,name in enumerate(MIXED_ARM_NAMES,1))
            and {mapping[f'base_{role}_wheel'] for role in WHEEL_ROLES}=={7,8,9},
            '固定六臂1..6及三轮7/8/9映射不合法')
    BASE_TARGETS=[('right',mapping[f'base_{role}_wheel'],f'base_{role}_wheel') for role in WHEEL_ROLES]
    WHEEL_PULSE_TARGETS=dict(zip(WHEEL_ROLES,BASE_TARGETS))
    WHEEL_STOP_TARGETS={motor:role for role,(_,motor,_) in WHEEL_PULSE_TARGETS.items()}
    BUS_CONFIG={'path':str(actual),'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()}
    scopes={role:feedback_scope(motor) for role,(_,motor,_) in WHEEL_PULSE_TARGETS.items()}
    RIGHT_FEEDBACK_SCOPE=scopes['right'];FRONT_FEEDBACK_SCOPE=scopes['front'];LEFT_FEEDBACK_SCOPE=scopes['left']
    held='/'.join(wheel_label(motor) for motor in wheel_ids() if motor!=9)
    MIXED_RAW_SCOPE=MIXED_RAW_SCOPE_TEMPLATE.format(held=held,selected=wheel_label(9))
    return dict(BUS_CONFIG)


def wheel_ids():
    require(BUS_CONFIG is not None,'必须先显式加载右总线配置')
    return [motor for _,motor,_ in BASE_TARGETS]


def wheel_label(motor):
    return f'{WHEEL_LABELS[WHEEL_STOP_TARGETS[motor]]}{motor}'


def feedback_scope(motor):
    template=RIGHT_FEEDBACK_SCOPE_TEMPLATE if motor==7 else FRONT_FEEDBACK_SCOPE_TEMPLATE
    return template.format(target=WHEEL_LABELS[WHEEL_STOP_TARGETS[motor]],motor=motor)


def verify_bus_config(plan):
    """执行和GO每次复核原配置字节身份；禁止只信进程已加载的映射。"""
    reference=plan.get('bus_config')
    require(type(reference) is dict and set(reference)=={'path','bytes','sha256'}
            and Path(reference['path']).is_absolute() and reference==BUS_CONFIG,
            '计划未绑定本次实际右总线配置')
    data=Path(reference['path']).read_bytes()
    require(len(data)==reference['bytes'] and hashlib.sha256(data).hexdigest()==reference['sha256'],
            '右总线配置字节身份变化，旧计划/GO已失效')


def feedback_entry(plan):
    entries=[(key,plan[key]) for key in (f'{role}_wheel_feedback_only' for role in WHEEL_ROLES) if key in plan]
    require(len(entries)==1,'只读计划必须恰选一个具名轮')
    key,contract=entries[0]
    role=key.removesuffix('_wheel_feedback_only')
    require(type(contract) is dict and contract.get('target')==list(WHEEL_PULSE_TARGETS[role]),'只读目标必须匹配配置轮位')
    return key,contract['target'][1]


def feedback_contract(motor):
    return right_feedback_contract() if motor==7 else front_feedback_contract(motor=motor)


def left_stop_contract(*,motor=7):
    """保留旧入口名，按motor统一处理底盘三轮；默认物理7供既有只读入口使用。"""
    require(type(motor) is int and motor in WHEEL_STOP_TARGETS,'停止稳定性仅接受固定底盘三轮')
    return {'version':f'RIGHT{motor}_STABILITY_V1',
            'target':list(WHEEL_PULSE_TARGETS[WHEEL_STOP_TARGETS[motor]]),
            'raw_tool':dict(RIGHT_PULSE_RAW),'reads':14,'register_writes':0,
            'sample_offsets_ns':[0,250_000_000,500_000_000,750_000_000,1_000_000_000],
            'capture_budget_ns':1_500_000_000,'torque_enable':0,'goal_velocity':0,
            'present_velocity_raw_unsigned':[0,50,0x8032],'position_span_max':1,
            'include_snapshot_position':True}


def right_feedback_contract():
    return {'scope':feedback_scope(7),'target':list(WHEEL_PULSE_TARGETS[WHEEL_STOP_TARGETS[7]]),
            'raw_tool':dict(RIGHT_PULSE_RAW),'reads':14,'register_writes':0,
            'sample_offsets_ns':[0,250_000_000,500_000_000,750_000_000,1_000_000_000],
            'capture_budget_ns':1_500_000_000,'same_frame':False,'stop_verified':False,
            'limitations':['1500ms是包含首末TE/GV及五组读取的软件预算，不是硬截止',
                           'PP/PV先后采样；非零PV保留原值，物理7候选通过不等于整机停止或动作授权',
                           '本轮SYNC_READ时序不替代R2普通READ故障原帧；未复现不表示已修复']}


def front_feedback_contract(*,motor=8):
    require(type(motor) is int and motor in (8,9),'诊断只接受物理8或9')
    return {'scope':feedback_scope(motor),
            'target':list(WHEEL_PULSE_TARGETS[WHEEL_STOP_TARGETS[motor]]),
            'raw_tool':dict(RIGHT_PULSE_RAW),'reads':14,'register_writes':0,
            'sample_offsets_ns':[0,250_000_000,500_000_000,750_000_000,1_000_000_000],
            'capture_budget_ns':1_500_000_000,'torque_enable':0,'goal_velocity':0,
            'present_velocity_policy':'RECORD_RAW_ONLY','position_span_policy':'RECORD_ONLY',
            'same_frame':False,'stop_verified':False,'firmware_contract_verified':False,
            'limitations':['SYNC_READ时序诊断不替代故障普通READ；未复现不表示修复',
                           'PV原字和PP跨度不解码物理运动，不应用物理7稳定性或整机停止判据']}


def left_feedback_tool_ref(plan):
    refs=[ref for ref in plan['bindings'].get('refs',[])
          if all(ref.get(k)==v for k,v in RIGHT_PULSE_RAW.items())
          and all(k in ref for k in LEFT_FEEDBACK_IDENTITY)]
    require(len(refs)==1,'轮只读必须唯一完整b2身份引用')
    return refs[0]


def validate_right_feedback_plan(plan):
    key,motor=feedback_entry(plan)
    require(motor==7 and not any(key in plan for key in ('right_wheel_pulse','bus2_mixed_raw_hold'))
            and plan.get('position_targets')==[] and plan.get(key)==right_feedback_contract()
            and plan.get('scope')==feedback_scope(motor),'轮只读禁止混入动作或更改固定序列')
    require(RIGHT_PULSE_RAW['path'] in plan['bindings'].get('binary_paths',[]),
            '轮只读进程名单缺b2')
    left_feedback_tool_ref(plan)
    require(all(row.get('decision')=='DEFERRED' and row.get('reason_code')=='BASE_FIRMWARE_CONTRACT_UNVERIFIED'
                for row in plan.get('base_targets',[])) and len(plan.get('base_targets',[]))==3,
            '轮只读不得解除三轮合同DEFERRED')


def validate_front_feedback_plan(plan,*,motor=8):
    contract=front_feedback_contract(motor=motor)
    key,selected=feedback_entry(plan)
    require(selected==motor and 'right_wheel_pulse' not in plan and 'bus2_mixed_raw_hold' not in plan
            and plan.get('position_targets')==[] and plan.get('scope')==contract['scope']
            and plan.get(key)==contract,'仅物理8或9的14读诊断，禁止混入动作或更改合同')
    require(RIGHT_PULSE_RAW['path'] in plan['bindings'].get('binary_paths',[]),'前轮只读进程名单缺b2')
    left_feedback_tool_ref(plan)
    require(plan.get('base_targets')==[{'side':side,'motor':motor,'name':name,'decision':'DEFERRED',
            'reason_code':'BASE_FIRMWARE_CONTRACT_UNVERIFIED'} for side,motor,name in BASE_TARGETS],
            '前轮诊断不得解除三轮合同DEFERRED')


def mixed_raw_refs(bindings):
    refs=bindings.get('bus2_mixed_raw_hold')
    require(isinstance(refs,dict) and set(refs)=={'tool','arm_calibration'},
            '混合维护必须绑定本次新ELF及右臂校准引用')
    for name,ref in refs.items():
        require(isinstance(ref,dict) and isinstance(ref.get('path'),str)
                and Path(ref['path']).is_absolute()
                and re.fullmatch(r'[0-9a-f]{64}',ref.get('sha256','')) is not None
                and type(ref.get('bytes')) is int and ref['bytes']>0
                and all(key in ref for key in MIXED_REF_IDENTITY)
                and all(type(ref[key]) is int and ref[key]>=0 for key in MIXED_REF_IDENTITY[:-1])
                and ref.get('symlink') is False and ref.get('nlink')==1,
                '混合维护引用缺少完整身份')
        require(any(all(actual.get(k)==v for k,v in ref.items())
                    for actual in bindings.get('refs',[])), '混合维护引用未进入完整refs')
        if name=='tool':
            require(ref['sha256'] not in (TOOL_SHA,RIGHT_PULSE_RAW['sha256'],RIGHT_PULSE_SYNC['sha256'])
                    and ref.get('elf_machine')==183 and ref['path'] in bindings.get('binary_paths',[]),
                    '混合维护不能复用旧poke产物或遗漏进程名单')
    require(any(all(ref.get(k)==v for k,v in RIGHT_PULSE_TRACE.items()) for ref in bindings.get('refs',[]))
            and RIGHT_PULSE_TRACE['path'] in bindings.get('binary_paths',[]), '混合维护缺少冻结strace')
    return refs


def arm_calibration_ref(bindings):
    """只取原右臂校准引用；只读诊断不依赖未使用的混合ELF或strace。"""
    refs=bindings.get('bus2_mixed_raw_hold')
    ref=refs.get('arm_calibration') if isinstance(refs,dict) else None
    require(isinstance(ref,dict) and isinstance(ref.get('path'),str)
            and Path(ref['path']).is_absolute()
            and re.fullmatch(r'[0-9a-f]{64}',ref.get('sha256','')) is not None
            and type(ref.get('bytes')) is int and ref['bytes']>0
            and all(key in ref for key in MIXED_REF_IDENTITY)
            and all(type(ref[key]) is int and ref[key]>=0 for key in MIXED_REF_IDENTITY[:-1])
            and ref.get('symlink') is False and ref.get('nlink')==1,
            '右臂校准引用缺少完整身份')
    require(any(all(actual.get(key)==value for key,value in ref.items())
                for actual in bindings.get('refs',[])), '右臂校准引用未进入完整refs')
    return ref


def verify_arm_calibration(plan):
    ref=arm_calibration_ref(plan['bindings'])
    data=verify_ref(ref);info=Path(ref['path']).lstat()
    actual={'uid':info.st_uid,'gid':info.st_gid,'mode':stat.S_IMODE(info.st_mode),
            'device':info.st_dev,'inode':info.st_ino,'mtime_ns':info.st_mtime_ns,
            'ctime_ns':info.st_ctime_ns,'nlink':info.st_nlink,'symlink':False}
    require(not Path(ref['path']).is_symlink() and all(ref.get(key)==value for key,value in actual.items()),
            '右臂校准引用元数据变化')
    return mixed_calibration_rows(strict_json(data.decode()))


def arm_range_contract(bindings):
    return {'schema':'MAINTENANCE_BUS2_ARM_RANGE_V1','scope':ARM_RANGE_SCOPE,
            'targets':[['right',motor,name] for motor,name in enumerate(MIXED_ARM_NAMES,1)],
            'raw_tool':dict(RIGHT_PULSE_RAW),'arm_calibration':dict(arm_calibration_ref(bindings)),
            'operation':'sync_read','registers':list(ARM_RANGE_REGS),'reads':18,'register_writes':0,
            'torque_enable':0,'present_velocity':0,'position_raw_min':0,'position_raw_max':4095,
            'stop_verified':False,'same_frame':False,'mixed_anchor_permitted':False}


def validate_arm_range_plan(plan):
    require(not any(key in plan for key in ('right_wheel_pulse','right_wheel_feedback_only',
                'front_wheel_feedback_only','left_wheel_feedback_only','bus2_mixed_raw_hold'))
            and plan.get('position_targets')==[] and plan.get('scope')==ARM_RANGE_SCOPE
            and plan.get('bus2_arm_range_only')==arm_range_contract(plan['bindings']),
            '右臂范围只读禁止混用动作、轮诊断或改动固定合同')
    require(RIGHT_PULSE_RAW['path'] in plan['bindings'].get('binary_paths',[]),'右臂范围只读进程名单缺b2')
    left_feedback_tool_ref(plan)
    require(plan.get('base_targets')==[{'side':side,'motor':motor,'name':name,'decision':'DEFERRED',
            'reason_code':'BASE_FIRMWARE_CONTRACT_UNVERIFIED'} for side,motor,name in BASE_TARGETS],
            '右臂范围只读不得解除三轮合同DEFERRED')


def mixed_raw_contract(bindings):
    refs=mixed_raw_refs(bindings)
    return {'schema':'MAINTENANCE_BUS2_MIXED_RAW_V1','scope':MIXED_RAW_SCOPE,
            'tool':dict(refs['tool']),'arm_calibration':dict(refs['arm_calibration']),
            'trace':dict(RIGHT_PULSE_TRACE),'operation':'mixed-hold-wheel9',
            'motor_ids':[*range(1,7),*wheel_ids()],'arm_ids':[1,2,3,4,5,6],'arm_names':list(MIXED_ARM_NAMES),
            'wheel_ids':wheel_ids(),'wheel_raw_goal':[500 if motor==9 else 0 for motor in wheel_ids()],
            'metadata':{key:value for key,value in RIGHT_PULSE_METADATA.items() if key!='Lock'},
            'allowed_raw_targets':[0,500],
            'pulse_duration_ns':20_000_000_000,'zero_window_ns':500_000_000,
            'enable_budget_ns':21_000_000_000,'child_budget_ns':MIXED_CHILD_BUDGET_NS,
            'anchor_freshness_ns':200_000_000,'return_tolerance_raw':4,
            'cycle_period_ns':50_000_000,'cycle_count':400,'wheel_sample_count':79,
            'sample_period_ns':250_000_000,'position_step_raw_max':512,
            'velocity_raw_max':1000,'zero_offsets_ns':[50_000_000,250_000_000,450_000_000],
            'zero_final_raw_unsigned':[0,50,0x8032],'typed_velocity_profile_released':False}


def validate_mixed_raw_plan(plan):
    require(not any(key in plan for key in ('right_wheel_pulse','right_wheel_feedback_only',
                'front_wheel_feedback_only','left_wheel_feedback_only'))
            and plan.get('position_targets')==[] and plan.get('scope')==MIXED_RAW_SCOPE
            and plan.get('bus2_mixed_raw_hold')==mixed_raw_contract(plan['bindings']),
            '混合维护禁止混用旧动作或改动固定raw合同')
    require(plan.get('base_targets')==[{'side':side,'motor':motor,'name':name,
            'decision':'RAW_DIAGNOSTIC' if motor==9 else 'RAW_ZERO_HOLD',
            'reason_code':'FW310_PHASE12_RAW_0_500_ONLY'} for side,motor,name in BASE_TARGETS],
            '混合维护不得扩展为通用底盘合同')


def verify_mixed_raw_refs(plan):
    for name,ref in mixed_raw_refs(plan['bindings']).items():
        data=verify_ref(ref,executable=name=='tool');info=Path(ref['path']).lstat()
        actual={'uid':info.st_uid,'gid':info.st_gid,'mode':stat.S_IMODE(info.st_mode),
                'device':info.st_dev,'inode':info.st_ino,'mtime_ns':info.st_mtime_ns,
                'ctime_ns':info.st_ctime_ns,'nlink':info.st_nlink,'symlink':False}
        require(all(ref.get(key)==value for key,value in actual.items()),'混合维护引用元数据变化')
        if name=='arm_calibration':
            calibration=strict_json(data.decode())
    return calibration


def mixed_calibration_rows(calibration):
    fields={'id','drive_mode','homing_offset','range_min','range_max'}
    require(isinstance(calibration,dict) and set(calibration)==set(MIXED_ARM_NAMES),'右臂校准必须为固定六项')
    rows=[]
    for index,name in enumerate(MIXED_ARM_NAMES,1):
        value=calibration[name]
        require(isinstance(value,dict) and set(value)==fields
                and all(type(value[key]) is int for key in fields)
                and value['drive_mode'] in (0,1)
                and value['id']==index
                and 0<=value['range_min']<value['range_max']<=4095,'右臂校准字段错误')
        rows.append(dict(value))
    rows.sort(key=lambda row:row['id'])
    require([row['id'] for row in rows]==[1,2,3,4,5,6],'右臂校准ID必须唯一为1..6')
    return rows


def parse_mixed_raw_result(text,plan,expiry,calibration):
    """只验证child自身RAW时域；不拿父MONOTONIC与child绝对ns比较。"""
    require(len(text.splitlines())==1,'混合child必须输出唯一完整JSON行')
    row=strict_json(text);contract=plan['bus2_mixed_raw_hold']
    keys={'schema','operation','result','error','serial_device','arm_calibration_path',
          'go_expires_at_unix_ns','motor_ids','wheel_ids','wheel_raw_goal','pulse_duration_ns',
          'zero_window_ns','enable_budget_ns','loaded_calibration','arm_anchors','wheel_anchors',
          'arm_final','cycle_count','cycles','wheel_samples','zero_samples','timing','cleanup'}
    require(isinstance(row,dict) and set(row)==keys,'混合child原件schema字段错误')
    def integer(value,low=0,high=None):
        require(type(value) is int and value>=low and (high is None or value<=high),'混合原件整数越界')
        return value
    def array(value,count,low=0,high=4095):
        require(type(value) is list and len(value)==count,'混合原件数组长度错误')
        return [integer(item,low,high) for item in value]
    require(row['schema']=='FEETECH_MIXED_WHEEL9_V1' and row['operation']==contract['operation']
            and row['serial_device']==DEVICES['right']
            and row['arm_calibration_path']==contract['arm_calibration']['path']
            and integer(row['go_expires_at_unix_ns'])==expiry,'混合原件目标/校准/GO错误')
    for key,count in (('motor_ids',9),('wheel_ids',3),('wheel_raw_goal',3)):
        require(array(row[key],count,0,500)==contract[key],'混合原件固定目标变化')
    for key in ('pulse_duration_ns','zero_window_ns','enable_budget_ns'):
        require(integer(row[key])==contract[key],'混合原件固定预算/周期数变化')
    require(row['result'] in ('PASS','FAIL'),'混合child结果类型错误')
    if row['result']=='FAIL':
        require(type(row['error']) is str and row['error'].strip(),'混合child失败缺少原始错误')
        raise ValueError('混合child失败: '+row['error'])
    require(row['error'] is None,'混合child失败原件不得变成PASS')
    # 周期、时间及收尾字段属于成功验收；失败原件只报告错误，不据其推断执行阶段。
    require(integer(row['cycle_count'])==contract['cycle_count'],'混合原件固定预算/周期数变化')
    expected=mixed_calibration_rows(calibration)
    require(row['loaded_calibration']==expected
            and all(type(value) is int for item in row['loaded_calibration'] for value in item.values()),
            'child加载校准与冻结原件不符')
    timing=row['timing']
    time_keys=['enable_attempt_ns','first_nonzero_ns','zero_started_ns','zero_finished_ns',
               'disable_attempt_ns','disable_finished_ns','finished_ns']
    require(isinstance(timing,dict) and set(timing)==set(time_keys),'混合时间字段错误')
    times=[integer(timing[key],1) for key in time_keys]
    require(times==sorted(times) and times[2]-times[1]>=contract['pulse_duration_ns']
            and times[5]-times[0]<=contract['enable_budget_ns'],'混合启用/20秒/断扭预算失败')
    anchors=row['arm_anchors'];final=row['arm_final']
    require(type(anchors) is list and len(anchors)==6 and type(final) is list and len(final)==6,
            '混合六臂锚点/末置缺失')
    for index,(anchor,last,cal) in enumerate(zip(anchors,final,expected),1):
        require(isinstance(anchor,dict) and set(anchor)=={'motor_id','raw','goal_raw','calibration_min',
                    'calibration_max','sampled_ns'} and integer(anchor['motor_id'])==index
                and integer(anchor['calibration_min'])==cal['range_min']
                and integer(anchor['calibration_max'])==cal['range_max'],'混合锚点身份/范围错误')
        value=integer(anchor['raw'],cal['range_min'],cal['range_max'])
        require(integer(anchor['goal_raw'],0,4095)==value
                and 0<=times[1]-integer(anchor['sampled_ns'],1)<=contract['anchor_freshness_ns'],
                '六臂实际Goal不等于新鲜原位锚点')
        require(isinstance(last,dict) and set(last)=={'motor_id','raw'}
                and integer(last['motor_id'])==index
                and abs(integer(last['raw'],0,4095)-value)<=4,'六臂末置偏离固定锚点')
    wheel_anchors=row['wheel_anchors']
    require(type(wheel_anchors) is list and len(wheel_anchors)==2,'未选两轮新鲜锚点缺失')
    for anchor,motor in zip(wheel_anchors,(motor for motor in contract['wheel_ids'] if motor!=9)):
        require(isinstance(anchor,dict) and set(anchor)=={'motor_id','raw','sampled_ns'}
                and integer(anchor['motor_id'])==motor
                and 0<=times[1]-integer(anchor['sampled_ns'],1)<=contract['anchor_freshness_ns'],
                '未选轮锚点身份/新鲜度错误')
        integer(anchor['raw'],0,4095)
    cycles=row['cycles']
    require(type(cycles) is list and len(cycles)==contract['cycle_count'],'混合周期集合不完整')
    previous=None
    for index,cycle in enumerate(cycles):
        require(isinstance(cycle,dict) and set(cycle)=={'started_ns','finished_ns','source_deadline_ns'},
                '混合周期字段错误')
        start=integer(cycle['started_ns'],1);end=integer(cycle['finished_ns'],1)
        deadline=integer(cycle['source_deadline_ns'],1)
        if index==0:
            require(start<=times[1]<=end<=deadline
                    and deadline<=times[1]+contract['cycle_period_ns']
                    and deadline<=min(anchor['sampled_ns']+contract['anchor_freshness_ns'] for anchor in anchors),
                    '混合首周期不满足实际GV尝试点或锚点来源期限')
        else:
            expected_deadline=times[1]+(index+1)*contract['cycle_period_ns']
            require(deadline==expected_deadline
                    and max(previous,deadline-contract['cycle_period_ns'])<=start<=end<=deadline,
                    '混合周期来源期限变化或逾期')
        previous=end
    selected=contract['wheel_ids'].index(9)
    held=[slot for slot,motor in enumerate(contract['wheel_ids']) if motor!=9]
    samples=row['wheel_samples'];travel=0;positive=False;last=None
    require(type(samples) is list and len(samples)==contract['wheel_sample_count'],'混合反馈集合不完整')
    for index,sample in enumerate(samples,1):
        require(isinstance(sample,dict) and set(sample)=={'started_ns','finished_ns','position_raw',
                'velocity_word','arm_position_raw'},'混合反馈字段错误')
        cycle=cycles[index*5]
        require(cycle['started_ns']<=integer(sample['started_ns'],1)
                <=integer(sample['finished_ns'],1)<=cycle['finished_ns'],'反馈不属于本轮对应周期')
        positions=array(sample['position_raw'],3);words=array(sample['velocity_word'],3,0,65535)
        arm=array(sample['arm_position_raw'],6)
        require(all(abs(value-anchor['raw'])<=4 for value,anchor in zip(arm,anchors)),
                '六臂运动中偏离固定锚点')
        for wheel,anchor in zip(held,wheel_anchors):
            require(words[wheel] in (0,50,0x8032)
                    and abs(positions[wheel]-anchor['raw'])<=4,'未选轮运动护栏失败')
        require(words[selected]<=contract['velocity_raw_max'],'物理9运动反馈方向/范围失败')
        positive|=words[selected]>0
        if last is not None:
            step=(positions[selected]-last)%4096
            require(step<=contract['position_step_raw_max'],'物理9相邻位置越界或逆向')
            travel+=step
        last=positions[selected]
    require(positive and travel>0,'物理9缺少正速度及正位移联合反馈')
    zero=row['zero_samples']
    require(type(zero) is list and len(zero)==3,'物理9零速三组原件缺失')
    for index,sample in enumerate(zero):
        require(isinstance(sample,dict) and set(sample)=={'started_ns','finished_ns','torque_enable',
                'goal_velocity_word','present_velocity_word'},'物理9零速原件字段错误')
        scheduled=times[3]+contract['zero_offsets_ns'][index]
        end=min(times[2]+contract['zero_window_ns'],times[3]+contract['zero_offsets_ns'][index+1]) if index<2 else times[2]+contract['zero_window_ns']
        require(scheduled<=integer(sample['started_ns'],1)<=integer(sample['finished_ns'],1)<=end
                and integer(sample['torque_enable'])==1 and integer(sample['goal_velocity_word'])==0,
                '物理9零速时间/TE1/GV0失败')
        word=integer(sample['present_velocity_word'],0,65535)
        require(word in contract['zero_final_raw_unsigned'] if index==2 else word<=1000 or word==0x8032,
                '物理9零速反馈不满足原合同')
    require(zero[-1]['finished_ns']<=times[4],'九电机断扭早于零速观察完成')
    cleanup=row['cleanup']
    require(isinstance(cleanup,dict) and set(cleanup)=={'zero_attempted','zero_ok','disable_attempted',
                'disable_ok','torque_enable_after'} and all(cleanup.get(key) is True for key in
                ('zero_attempted','zero_ok','disable_attempted','disable_ok'))
            and array(cleanup['torque_enable_after'],9,0,1)==[0]*9,'九电机独立收尾未确认')
    return row


def run_bus2_mixed_raw_hold(engine):
    """一次真实adapter维护child；正常或异常都继续复用外层整机停止及身份后验。"""
    plan=engine.plan;io=engine.io;validate_mixed_raw_plan(plan);engine.check_gate()
    calibration=verify_mixed_raw_refs(plan);mixed_calibration_rows(calibration)
    contract=plan['bus2_mixed_raw_hold'];expiry=engine.go_expires_at_unix_ns
    arms=[{'side':'right','motor':motor,'name':name,'kind':'position_hold','status':'FAIL','cleanup_errors':[]}
          for motor,name in enumerate(MIXED_ARM_NAMES,1)]
    wheel={'side':'right','motor':9,'name':WHEEL_PULSE_TARGETS[WHEEL_STOP_TARGETS[9]][2],'kind':'velocity',
           'status':'FAIL','raw_velocity':500,'cleanup_errors':[]}
    engine.rows.extend([*arms,wheel])
    argv=[contract['tool']['path'],DEVICES['right'],'9',contract['operation'],
          '--arm-calibration',contract['arm_calibration']['path'],'--go-expires-at-ns',str(expiry),
          '--wheel-ids',','.join(map(str,contract['wheel_ids']))]
    trace=io.output/f'bus2-mixed-raw-command_{io.sequence+1:04d}.strace'
    failure=None
    try:
        verify_ref(RIGHT_PULSE_TRACE,executable=True)
        with trace.open('xb'):pass
        traced=[RIGHT_PULSE_TRACE['path'],'-f','-I','3','-ttt','-xx','-s','256','-yy','-e',
                'trace=openat,close,read,write,ioctl','-o',str(trace),*argv]
        def stop_check():
            io.check();engine.right_pulse_go_check()
            if io.child is None:
                require(expiry-time.time_ns()>=MIXED_CHILD_BUDGET_NS,'本次GO不足完整28秒child预算')
        engine.mixed_stop_check=stop_check
        engine.mixed_trace_argv=tuple(traced)
        text=io.command(traced,allowed=(0,1),mixed_stop_check=stop_check)
        require(io.child is None and not io.evidence_errors,'混合child/原件未闭合')
        engine.right_pulse_go_check();verify_mixed_raw_refs(plan)
        engine.mixed_raw_result=strict_json(text)
        result=parse_mixed_raw_result(text,plan,expiry,calibration)
        for probe,anchor in zip(arms,result['arm_anchors']):
            probe.update(status='PASS',anchor_raw=anchor['raw'],sampled_ns=anchor['sampled_ns'],
                         hold_verification={'status':'PASS','tolerance_raw':4,'feedback_samples':79})
        wheel.update(status='PASS',evidence_command_sequence=io.sequence)
    except BaseException as error:
        failure=error;wheel['error']=repr(error);wheel['cleanup_errors'].append(repr(error))
        if hasattr(engine,'right_pulse_block'):engine.right_pulse_block(error)
        else:engine.manual_cut=True
        raise
    finally:
        try:
            info=trace.lstat();raw=trace.read_bytes()
            require(stat.S_ISREG(info.st_mode) and info.st_nlink==1 and raw,'混合原帧strace原件无效')
            if failure is None:
                require(re.search(rb'(?:^|\n)[^\n]*\+\+\+ exited with 0 \+\+\+\n?\Z',raw),
                        '混合strace缺成功自然终态')
            evidence={'path':str(trace),'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest(),
                      'original_argv':argv,'motor_ids':contract['motor_ids'],
                      'capture_status':'NATURAL_SUCCESS_TERMINAL' if failure is None else 'ORIGINAL_COMMAND_FAILED',
                      'wire_frame_review':'PENDING_ROOT_REVIEW_OF_ACTUAL_WIRE_STRACE'}
            engine.mixed_wire_evidence=evidence
            if io.commands:io.commands[-1]['mixed_wire_trace']=evidence
        except BaseException as error:
            io.evidence_errors.append('mixed_wire_trace:'+repr(error));engine.manual_cut=True
            wheel['cleanup_errors'].append(repr(error));wheel['status']='FAIL'
            if hasattr(engine,'right_pulse_block'):engine.right_pulse_block(error)
            if failure is None:raise


def pulse_scope(wheel,*,zero_stop_check=False):
    require(wheel in WHEEL_PULSE_TARGETS,'仅接受固定的前轮、左轮或右轮')
    scope=RIGHT_PULSE_SCOPE_TEMPLATE.format(target=wheel_label(WHEEL_PULSE_TARGETS[wheel][1]))
    if zero_stop_check:
        scope+='；20秒后额外SYNC零速并保持使能，500毫秒内三组TE1/GV0/PV读取，三轮前两组允许正向0..1000及原字0x8032，末组原字限0/50/0x8032并保留原值，再按原路径断扭'
    return scope


def pulse_wheel(plan):
    target=plan.get('right_wheel_pulse',{}).get('target')
    choices=[name for name,value in WHEEL_PULSE_TARGETS.items() if target==list(value)]
    require(len(choices)==1,'动作目标必须是右总线具名的7、8或9，禁止任意ID/名称组合')
    return choices[0]


def right_pulse_contract(wheel='right',*,zero_stop_check=False):
    contract={'scope':pulse_scope(wheel,zero_stop_check=zero_stop_check),
            'target':list(WHEEL_PULSE_TARGETS[wheel]),'raw_velocity':500,
            'observation_ns':20_000_000_000,'write_period_ns':50_000_000,'sample_period_ns':250_000_000,
            'enable_budget_ns':500_000_000,'sample_budget_ns':250_000_000,
            'nonzero_window_fail_ns':21_000_000_000,'velocity_raw_max':1000,'position_step_raw_max':512,
            'settle_budget_ns':1_000_000_000,'settle_offsets_ns':[50_000_000,250_000_000,500_000_000,750_000_000,950_000_000],
            'sdk':{**RIGHT_PULSE_SDK,'refs':[dict(ref) for ref in RIGHT_PULSE_SDK['refs']]},
            'sdk_read_registers':{name:list(value) for name,value in RIGHT_SDK_REGS.items()},
            'register_writes':{'40':[0,1],'46':[0,500]},
            'metadata':dict(RIGHT_PULSE_METADATA),'raw_tool':dict(RIGHT_PULSE_RAW),
            'sdk_stop_stability':{'trigger_raw_unsigned':[50,0x8032],
                                  'contract':left_stop_contract(motor=WHEEL_PULSE_TARGETS[wheel][1])},
            'raw_helper':dict(RIGHT_PULSE_HELPER),'strace':dict(RIGHT_PULSE_TRACE),
            'write_tool':dict(RIGHT_PULSE_WRITE),'sync_tool':dict(RIGHT_PULSE_SYNC),
            'firmware_contract_verified':False,
            'limitations':['原始值尚未标定物理单位；20秒/21秒及停稳1秒是软件预算，不是硬件自动截止',
                           '50ms为名义发送间隔，读数耗时可造成跳槽；绝对截止不续期，不补发过期槽',
                           '仅相邻模4096位置差值及累计诊断，不提供物理里程计',
                           '只诊断一次正向动作，不改变Phase/模式/保护、三轮合同或生产Velocity链']}
    if zero_stop_check:
        contract['zero_stop_check']={'budget_ns':500_000_000,'cleanup_reserve_ns':100_000_000,
            'sample_offsets_ns':[50_000_000,250_000_000,450_000_000],
            'offset_origin':'ZERO_SYNC_FINISHED','torque_enable':1,'goal_velocity':0,
            'intermediate_sample_indices':[0,1],'intermediate_extra_raw_unsigned':[0x8032],
            'final_present_velocity_raw_unsigned':[0,50,0x8032]}
    return contract


def validate_right_pulse_plan(plan):
    require('front_wheel_feedback_only' not in plan and 'left_wheel_feedback_only' not in plan
            and 'bus2_mixed_raw_hold' not in plan,'轮只读或混合维护不得混用pulse')
    require(RIGHT_PULSE_SDK.get('sdk_version')=='1.0.0' and RIGHT_PULSE_SDK.get('serial_version')=='3.5'
            and len(RIGHT_PULSE_SDK.get('refs',[]))==12,'右轮SDK闭包尚未完整冻结')
    require(isinstance(RIGHT_PULSE_SYNC['sha256'],str)
            and re.fullmatch(r'[0-9a-f]{64}',RIGHT_PULSE_SYNC['sha256']) is not None
            and type(RIGHT_PULSE_SYNC['bytes']) is int and RIGHT_PULSE_SYNC['bytes']>0,
            '右轮SYNC工具尚未完整冻结绑定')
    wheel=pulse_wheel(plan)
    expected=right_pulse_contract(wheel,zero_stop_check='zero_stop_check' in plan['right_wheel_pulse'])
    require(plan.get('position_targets') == [] and plan.get('right_wheel_pulse') == expected
            and plan.get('scope')==expected['scope'],
            '右轮诊断必须空位置目标及完整固定合同')
    bindings=plan['bindings']
    matches=lambda actual,expected: isinstance(actual,dict) and all(actual.get(key)==value for key,value in expected.items())
    require(matches(bindings.get('tool'),RIGHT_PULSE_WRITE), '右轮诊断必须原冻结f201完整引用')
    refs=(RIGHT_PULSE_WRITE,RIGHT_PULSE_RAW,RIGHT_PULSE_SYNC,RIGHT_PULSE_HELPER,RIGHT_PULSE_TRACE,
          *RIGHT_PULSE_SDK['refs'])
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


def gripper_release_contract(bindings,goal=1600):
    require(type(goal) is int and goal in GRIPPER_TARGETS,'右夹爪仅接受固定整数目标1600或1397')
    calibration=bindings.get('calibrations',{}).get('right_arm_gripper')
    require(isinstance(calibration,dict) and calibration.get('id')==6
            and calibration.get('drive_mode')==0 and calibration.get('range_min')==1384
            and calibration.get('range_max')==2847,'右夹爪必须保留原ID6/drive0/1384..2847标定')
    require(any(all(ref.get(key)==value for key,value in RIGHT_PULSE_TRACE.items())
                for ref in bindings.get('refs',[]))
            and RIGHT_PULSE_TRACE['path'] in bindings.get('binary_paths',[]),'右夹爪抓帧缺冻结strace身份')
    return {**GRIPPER_TARGETS[goal],'target':['right',6,'right_arm_gripper'],
            'maximum_increment_raw':GRIPPER_MAX_INCREMENT_RAW,'tolerance_raw':GRIPPER_TOLERANCE_RAW,'observation_budget_ms':800,
            'return_to_initial':False,'calibration':dict(calibration)}


def validate_gripper_release_plan(plan):
    contract=plan.get('right_gripper_release')
    require(isinstance(contract,dict),'缺少右夹爪单程合同')
    require(contract==gripper_release_contract(plan['bindings'],contract.get('goal_raw'))
            and plan.get('position_targets')==[] and plan.get('scope')==contract['scope']
            and plan.get('position_wait_ms')==800 and plan.get('return_tolerance_raw')==GRIPPER_TOLERANCE_RAW
            and not any(key in plan for key in ('right_wheel_pulse','right_wheel_feedback_only',
                'front_wheel_feedback_only','left_wheel_feedback_only','bus2_mixed_raw_hold','bus2_arm_range_only')),
            '右夹爪单程计划禁止混合其他动作或改动固定合同')


def position_targets_from_plan(plan):
    """执行只接受计划中已绑定的目标；不从全局四项列表补跑未选电机。"""
    for motor,prefix in WHEEL_STOP_TARGETS.items():
        require(plan.get(f'{prefix}{motor}_stop_contract')==left_stop_contract(motor=motor),'缺少当次固定三轮停止合同')
    if 'right_gripper_release' in plan:
        validate_gripper_release_plan(plan)
        return []
    if 'bus2_arm_range_only' in plan:
        validate_arm_range_plan(plan)
        return []
    if 'bus2_mixed_raw_hold' in plan:
        validate_mixed_raw_plan(plan)
        return []
    if any(f'{role}_wheel_feedback_only' in plan for role in WHEEL_ROLES):
        _,motor=feedback_entry(plan)
        if motor==7:validate_right_feedback_plan(plan)
        else:validate_front_feedback_plan(plan,motor=motor)
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
              right_wheel_feedback_only=False,wheel_pulse_target=None,zero_stop_check=False,front_wheel_feedback_only=False,
              left_wheel_feedback_only=False,bus2_mixed_raw_hold=False,bus2_arm_range_only=False,right_gripper_release=False,
              gripper_goal=1600):
    require(type(gripper_goal) is int and gripper_goal in GRIPPER_TARGETS
            and (right_gripper_release or gripper_goal==1600),'夹爪目标仅用于本次固定单右6动作')
    require(wheel_pulse_target is None or (type(wheel_pulse_target) is str and wheel_pulse_target in ('front','left')),
            '新单轮选择仅接受front或left')
    require(not (right_wheel_pulse and wheel_pulse_target is not None),'旧右轮入口与具名轮选择不能混用')
    pulse_requested=right_wheel_pulse or wheel_pulse_target is not None
    require(type(zero_stop_check) is bool and (not zero_stop_check or pulse_requested),
            '零速保持使能检查只能附加到单轮pulse')
    require(type(right_wheel_pulse) is bool and type(right_wheel_feedback_only) is bool
            and type(front_wheel_feedback_only) is bool and type(left_wheel_feedback_only) is bool
            and type(bus2_mixed_raw_hold) is bool and type(bus2_arm_range_only) is bool
            and type(right_gripper_release) is bool
            and sum((bool(pulse_requested),right_wheel_feedback_only,front_wheel_feedback_only,
                     left_wheel_feedback_only,bus2_mixed_raw_hold,bus2_arm_range_only,right_gripper_release))<=1
            and not ((pulse_requested or right_wheel_feedback_only or front_wheel_feedback_only
                      or left_wheel_feedback_only or bus2_mixed_raw_hold or bus2_arm_range_only or right_gripper_release)
                     and position_target_names is not None),
            '轮诊断禁止互相混用或混用位置选择')
    position_targets = [] if (pulse_requested or right_wheel_feedback_only or front_wheel_feedback_only
                             or left_wheel_feedback_only or bus2_mixed_raw_hold or bus2_arm_range_only or right_gripper_release) else select_position_targets(position_target_names)
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
            "bindings": bindings, "bus_config":dict(BUS_CONFIG or {}), "script_sha256": hashlib.sha256(script).hexdigest(), "script_bytes": len(script),
            "position_targets": position_targets, "position_delta_raw": 24, "minimum_excursion_raw": 12,
            "return_tolerance_raw": 4, "position_wait_ms": 800, "base_order": wheel_ids(),
            "base_raw": [20, -20], "pulse_target_ms": 150, "nonzero_window_fail_ns": 1_000_000_000,
            "observed_velocity_raw_max": 40, "observed_wheel_delta_raw_min": 3, "observed_wheel_delta_raw_max": 64,
            "base_targets": [{'side':side,'motor':motor,'name':name,'decision':'DEFERRED',
                              'reason_code':'BASE_FIRMWARE_CONTRACT_UNVERIFIED'} for side,motor,name in BASE_TARGETS],
            "limitations": ["停止寄存器证据不替代物理独立断电确认",
                            "base仅mode1/Goal0合同；Model/Phase解释门未闭合，本软件结果不授权新base动作"],
            "scope": "单电机独立维护动作/停止验证；非整车运动学、S0/S1接入、H2/D15或生产验收"}
    for motor,prefix in WHEEL_STOP_TARGETS.items():
        plan[f'{prefix}{motor}_stop_contract']=left_stop_contract(motor=motor)
    if pulse_requested:
        plan['right_wheel_pulse']=right_pulse_contract(wheel_pulse_target or 'right',zero_stop_check=zero_stop_check)
        plan['scope']=plan['right_wheel_pulse']['scope']
        validate_right_pulse_plan(plan)
    for role,requested in (('right',right_wheel_feedback_only),('front',front_wheel_feedback_only),('left',left_wheel_feedback_only)):
        if requested:
            motor=WHEEL_PULSE_TARGETS[role][1]
            plan[f'{role}_wheel_feedback_only']=feedback_contract(motor)
            plan['scope']=feedback_scope(motor)
            if motor==7:validate_right_feedback_plan(plan)
            else:validate_front_feedback_plan(plan,motor=motor)
    if bus2_mixed_raw_hold:
        plan['bus2_mixed_raw_hold']=mixed_raw_contract(bindings)
        plan['scope']=MIXED_RAW_SCOPE
        plan['base_targets']=[{'side':side,'motor':motor,'name':name,
            'decision':'RAW_DIAGNOSTIC' if motor==9 else 'RAW_ZERO_HOLD',
            'reason_code':'FW310_PHASE12_RAW_0_500_ONLY'} for side,motor,name in BASE_TARGETS]
        plan['limitations']=['仅本次FW3.10/Phase12/Mode1的raw{0,+500}维护合同，不解除通用profile/SI门',
                             '维护真实adapter链；不声明SerialTick、S0/S1、D15或生产验收完成']
        validate_mixed_raw_plan(plan)
    if bus2_arm_range_only:
        plan['bus2_arm_range_only']=arm_range_contract(bindings)
        plan['scope']=ARM_RANGE_SCOPE
        plan['limitations']=['六臂PP仅为顺序采样的范围诊断，不是混合动作200毫秒新鲜锚点',
                             '无寄存器写入，不判整机停止或物理安全，不解除D15或生产验收']
        validate_arm_range_plan(plan)
    if right_gripper_release:
        plan['right_gripper_release']=gripper_release_contract(bindings,gripper_goal)
        plan['scope']=plan['right_gripper_release']['scope']
        plan['return_tolerance_raw']=GRIPPER_TOLERANCE_RAW
        validate_gripper_release_plan(plan)
    return plan


def validate_go(go, plan, now_ns):
    verify_bus_config(plan)
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
    if 'bus2_arm_range_only' in plan:
        facts=go['physical']['right']
        verify_arm_calibration(plan)
        require(0 < go['expires_at_ns']-go['issued_at_ns'] <= 600_000_000_000
                and 0 <= now_ns-facts['confirmed_at_ns'] <= 600_000_000_000,
                '右臂范围只读现场/GO最多600秒')
        require(facts.get('arm_range_confirmation_scope')==ARM_RANGE_SCOPE
                and isinstance(facts.get('arm_range_confirmation_answer'),str)
                and facts['arm_range_confirmation_answer'].strip(), '缺本轮右臂范围只读具体现场确认')
    if any(key in plan for key in ('right_wheel_pulse','right_wheel_feedback_only','front_wheel_feedback_only',
                                  'left_wheel_feedback_only','bus2_mixed_raw_hold')):
        facts=go['physical']['right']
        require(0 < go['expires_at_ns']-go['issued_at_ns'] <= 600_000_000_000
                and 0 <= now_ns-facts['confirmed_at_ns'] <= 600_000_000_000,
                '右轮诊断现场/GO最多600秒')
        require(all(facts.get(key) is True for key in ('all_three_wheels_off_ground','base_stably_supported',
                    'continuous_wheel_observation'))
                and facts.get('base_confirmation_kind')=='CURRENT_SETUP_READY'
                and facts.get('base_confirmation_scope')==(
                    MIXED_RAW_SCOPE if 'bus2_mixed_raw_hold' in plan else
                    LEFT_FEEDBACK_SCOPE if 'left_wheel_feedback_only' in plan else
                    FRONT_FEEDBACK_SCOPE if 'front_wheel_feedback_only' in plan else
                    RIGHT_FEEDBACK_SCOPE if 'right_wheel_feedback_only' in plan else plan['right_wheel_pulse']['scope'])
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

    def reap(self, row, failures, *, mixed_stop_check=None):
        while True:
            if mixed_stop_check is not None and not row.get('mixed_stop_requested'):
                try:
                    mixed_stop_check()
                    require(self.now()-row['started_ns'] <= MIXED_CHILD_BUDGET_NS,
                            '混合维护child超过固定28秒预算')
                except BaseException as error:
                    # 仅新混合入口：所属独立进程组收到一次协作停止，仍持有真实wait，不kill。
                    row['mixed_stop_requested'] = repr(error)
                    failures.append('mixed_stop:' + repr(error))
                    try:
                        require(os.getpgid(self.child.pid)==self.child.pid,
                                '混合维护child不再拥有原独立进程组')
                        os.killpg(self.child.pid,signal.SIGINT)
                        row['signals_sent'].append({'signal':int(signal.SIGINT),
                            'process_group':self.child.pid,'sent_ns':self.now()})
                    except BaseException as signal_error:
                        failures.append('mixed_stop_signal:' + repr(signal_error))
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
                if self.now() - row['started_ns'] > row.get('budget_ns',2_000_000_000):
                    if not row['over_budget']:
                        print('爸爸，child超预算，仍持有真实wait；请立即人工断电。', flush=True)
                    row['over_budget'] = True
            except BaseException as error:
                failures.append('wait观测:' + repr(error))

    @contextlib.contextmanager
    def named_feetech_command(self, argv, row):
        """仅给已验证工具的exec名加前缀；原路径继续用于全部安全门。"""
        actual=[str(value) for value in argv]
        references={ref['path']:ref for ref in
                    (self.tool,RIGHT_PULSE_RAW,RIGHT_PULSE_WRITE,RIGHT_PULSE_SYNC,
                     getattr(self,'mixed_feetech_tool',None)) if ref is not None}
        index=0
        if actual and actual[0]==RIGHT_PULSE_TRACE['path']:
            flags=['-ttt','-xx','-s','256','-yy','-e','trace=openat,close,read,write,ioctl','-o']
            prefixes=([RIGHT_PULSE_TRACE['path'],*flags],
                      [RIGHT_PULSE_TRACE['path'],'-f','-I','3',*flags])
            index=next((len(prefix)+1 for prefix in prefixes
                        if actual[:len(prefix)]==prefix),len(actual))
        if sys.platform!='linux' or index>=len(actual) or actual[index] not in references:
            yield actual
            return
        # 只验证原target；verify_ref拒绝符号链接的规则保持不变。
        verify_ref(references[actual[index]],executable=True)
        directory=None
        def warning(error):
            row.setdefault('process_name_warnings',[]).append(repr(error))
            process_name_warning(error)
        try:
            directory=tempfile.TemporaryDirectory(prefix='robot-feetech-')
            os.chmod(directory.name,0o700)
            alias=Path(directory.name)/'robot-feetech'
            alias.symlink_to(actual[index])
            row['requested_argv']=list(actual)
            actual[index]=str(alias)
            row['argv']=list(actual)
        except OSError as error:
            warning(error)
        try:
            yield actual
        finally:
            if directory is not None:
                try:
                    directory.cleanup()
                except OSError as error:
                    warning(error)

    def command(self, argv, *, cleanup=False, allowed=(0,), input_raw=None, mixed_stop_check=None):
        if getattr(self,'arm_range_read_only',False):
            self.check_arm_range_command(argv)
        require(mixed_stop_check is None or callable(mixed_stop_check),'混合维护停止检查必须可调用')
        require(mixed_stop_check is None or not cleanup,'混合维护长child不得作为清场命令重发')
        if not cleanup:
            self.check()
        require(self.child is None, "禁止同时拥有第二 child")
        self.sequence += 1
        prefix = self.output / f"command_{self.sequence:04d}"
        row = {"sequence": self.sequence, "argv": [str(v) for v in argv],
               "started_ns": self.now(), "pid": None, "reaped": False,
               "signals_sent": [], "over_budget": False, "cleanup": cleanup}
        if mixed_stop_check is not None:
            row['budget_ns']=MIXED_CHILD_BUDGET_NS
        failures = []
        out, err = b'', b''
        naming_stack=contextlib.ExitStack()
        try:
            actual_argv=naming_stack.enter_context(self.named_feetech_command(argv,row))
            # 原始输出先落匿名文件；证据目录写失败不会阻止后续独立安全收尾命令。
            with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr, tempfile.TemporaryFile() as stdin:
                if input_raw is not None:
                    stdin.write(input_raw)
                    stdin.seek(0)
                front_prelaunch=getattr(self,'front_feedback_before_spawn',None)
                if front_prelaunch is not None:
                    front_prelaunch(argv)
                arm_prelaunch=getattr(self,'arm_range_before_spawn',None)
                if arm_prelaunch is not None:
                    arm_prelaunch(argv)
                if mixed_stop_check is not None:
                    mixed_stop_check()
                self.child = subprocess.Popen(actual_argv, stdin=stdin if input_raw is not None else subprocess.DEVNULL,
                                              stdout=stdout, stderr=stderr, start_new_session=True,
                                              env={**os.environ, 'PATH':'/usr/sbin:/usr/bin:/sbin:/bin', 'LC_ALL':'C',
                                                   'RUST_LOG':'error'})
                row["pid"] = self.child.pid
                # 一旦获得child，任何wait异常都保有所有权并继续自然wait，不让finally收尾被第二child门挡住。
                code = self.reap(row, failures, mixed_stop_check=mixed_stop_check)
                row['over_budget'] |= self.now() - row['started_ns'] > row.get('budget_ns',2_000_000_000)
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
                self.reap(row, failures, mixed_stop_check=mixed_stop_check)
            row["error"] = repr(error)
            raise
        finally:
            # 异常分支也先真实reap，最后才移除显示名链接。
            naming_stack.close()
            row["finished_ns"] = self.now()
            row['owner_errors'] = failures
            self.commands.append(row)
            try:
                self.journal.write(json.dumps(row, ensure_ascii=False) + "\n")
                self.journal.flush()
            except BaseException as error:
                self.evidence_errors.append('journal:' + repr(error))
                raise ValueError('命令journal写失败，child已真实回收') from error

    def check_arm_range_command(self,argv):
        """仅本模式拒绝所有UART写及范围外读；身份检查仍可执行。"""
        if not any(device in argv for device in DEVICES.values()):
            return
        if (len(argv)==2 and argv[0]=='fuser' and argv[1] in DEVICES.values()
                or len(argv)==4 and argv[:3]==['sudo','-n','fuser'] and argv[3] in DEVICES.values()
                or len(argv)==5 and argv[:4]==['udevadm','info','--query=property','--name']
                   and argv[4] in DEVICES.values()):
            return
        require(not getattr(self,'arm_range_uart_blocked',False),'右臂范围只读UART已封锁')
        require(len(argv)==5 and argv[:2]==[RIGHT_PULSE_RAW['path'],DEVICES['right']]
                and argv[2] in tuple(map(str,range(1,7))) and argv[3]=='read-raw'
                and argv[4] in ARM_RANGE_REGS,'右臂范围只读拒绝UART写或范围外命令')

    def poke(self, side, motor, action, register, value=None, *, cleanup=False):
        require(not getattr(self,'arm_range_read_only',False) or action!='write', '右臂范围只读禁止寄存器写')
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
        if getattr(self,'gripper_release_capture',False) and (side,motor)==('right',6) and action=='write':
            text=capture_gripper_release_wire(self,argv,cleanup=cleanup)
        else:
            text = self.command(argv, cleanup=cleanup)
        pattern = (rf"read {register}@{motor} = (-?\d+)\n" if action == "read"
                   else rf"write {register}@{motor} <- {value} ok\n")
        match = re.fullmatch(pattern, text)
        require(match is not None, "poke完整stdout不匹配")
        return int(match[1]) if action == "read" else None


def capture_gripper_release_wire(io,argv,*,cleanup=False):
    """沿原strace原件路径抓取右6写；抓帧准备失败也不跳过唯一断扭。"""
    trace=io.output/f'right6-release-command_{io.sequence+1:04d}.strace'
    try:
        verify_ref(RIGHT_PULSE_TRACE,executable=True)
        with trace.open('xb'):pass
    except BaseException:
        if cleanup and argv[-3:]==['write','Torque_Enable','0']:
            io.command(argv,cleanup=True)
        raise
    succeeded=False
    try:
        text=io.command([RIGHT_PULSE_TRACE['path'],'-ttt','-xx','-s','256','-yy','-e',
                        'trace=openat,close,read,write,ioctl','-o',str(trace),*argv],cleanup=cleanup)
        succeeded=True
        return text
    finally:
        try:
            info=trace.lstat();raw=trace.read_bytes()
            require(stat.S_ISREG(info.st_mode) and info.st_nlink==1 and raw,'右夹爪写入strace原件无效')
            if succeeded:
                require(re.search(rb'(?:^|\n)[^\n]*\+\+\+ exited with 0 \+\+\+\n?\Z',raw),'右夹爪strace缺成功自然终态')
            io.commands[-1]['gripper_release_wire_trace']={'path':str(trace),'bytes':len(raw),
                'sha256':hashlib.sha256(raw).hexdigest(),'original_argv':list(argv),
                'capture_status':'NATURAL_SUCCESS_TERMINAL' if succeeded else 'ORIGINAL_COMMAND_FAILED',
                'wire_frame_review':'PENDING_ROOT_REVIEW_OF_ACTUAL_WIRE_STRACE'}
        except BaseException as error:
            io.evidence_errors.append('gripper_release_wire_trace:'+repr(error))
            if succeeded:raise


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

    def witness_position(self, side, motor, target, anchor, stage, evidence, *, started_ns=None,
                         check_ready=lambda:None,expected_direction=0,maximum_excursion_raw=32):
        started=self.io.now() if started_ns is None else started_ns
        deadline = started + self.plan["position_wait_ms"] * 1_000_000
        row={'stage':stage,'status':'FAIL','target_raw':target,'anchor_raw':anchor,
             'tolerance_raw':self.plan['return_tolerance_raw'],'budget_ms':self.plan['position_wait_ms'],
             'started_ns':started,'deadline_ns':deadline,'samples':[], 'final_signed_error_raw':None}
        evidence[stage]=row
        while self.io.now() < deadline:
            check_ready()
            position = self.read(side, motor, "Present_Position")
            stamp=self.io.now()
            check_ready()
            row['samples'].append({'stamp_ns':stamp,'position_raw':position,'signed_error_raw':position-target})
            row.update(finished_ns=stamp,elapsed_ns=stamp-started,final_signed_error_raw=position-target)
            if abs(position-anchor)>maximum_excursion_raw:
                row['failure_code']='POSITION_EXCURSION_LIMIT'
                raise PositionAccuracyError(row)
            if expected_direction*(position-anchor)<0:
                row['failure_code']='POSITION_DIRECTION_MISMATCH'
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

    def gripper_release(self, calibration):
        """按固定目标单程张开或合回；不复用往返锚点或改变标定。"""
        require(not self.rows,'右夹爪单程只允许本次唯一动作')
        validate_gripper_release_plan(self.plan)
        self.check_gate()
        ready=getattr(self,'left_stop_go_check',self.check_gate)
        contract=self.plan['right_gripper_release'];goal=contract['goal_raw']
        row={'side':'right','motor':6,'name':'right_arm_gripper','kind':'position_release','status':'FAIL',
             'goal_raw':goal,'return_to_initial':False,'position_accuracy':{'status':'NOT_RUN','stages':{}}}
        self.rows.append(row);error=None
        def stopped_at(initial=None):
            ready()
            require(self.read('right',6,'Operating_Mode')==0
                    and self.read('right',6,'Torque_Enable')==0
                    and self.read('right',6,'Present_Velocity')==0,'右夹爪模式/断扭/静止状态漂移')
            value=self.read('right',6,'Present_Position')
            require(type(value) is int and calibration['range_min']<=value<=calibration['range_max']
                    and 0<(goal-value)*contract['direction']<=contract['maximum_increment_raw']
                    and (initial is None or value==initial),'右夹爪初值/增量/稳定性不符，不重算目标')
            return value
        try:
            require(calibration==contract['calibration'] and calibration['range_min']<=goal<=calibration['range_max'],
                    '右夹爪原标定或固定目标范围变化')
            initial=stopped_at();row['initial_raw']=initial
            self.io.sleep(.05);stopped_at(initial)
            ready();row['goal_write_attempt_ns']=self.io.now()
            row['observation_deadline_ns']=row['goal_write_attempt_ns']+800_000_000
            self.write('right',6,'Goal_Position',goal)
            require(self.read('right',6,'Goal_Position')==goal,'右夹爪GP精确读回不匹配')
            require(self.read('right',6,'Torque_Enable')==1,'右夹爪GP写后自动使能未确认')
            samples=self.witness_position('right',6,goal,initial,'release',row['position_accuracy']['stages'],
                        started_ns=row['goal_write_attempt_ns'],check_ready=ready,expected_direction=contract['direction'],
                        maximum_excursion_raw=contract['maximum_increment_raw'])
            require((samples[-1]['position_raw']-initial)*contract['direction']>0,'右夹爪没有向目标方向实际移动')
            row['position_accuracy']['status']='PASS'
        except PositionAccuracyError as failure:
            row['position_accuracy']['status']='FAIL';row['accuracy_failure']=failure.evidence;error=repr(failure)
        except BaseException as failure:
            error=repr(failure)
        finally:
            row['cleanup_errors']=self.safe_off('right',6,False)
            row['error']=error;row['status']='PASS' if error is None and not row['cleanup_errors'] else 'FAIL'
        require(row['status']=='PASS',f'右夹爪单程动作失败：{row}')

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


def left_feedback_raw(io, reference, register, check_ready, *, motor=7):
    """复用b2的只读原帧输出；此入口不能构造write或其他目标命令。"""
    require(register in LEFT_FEEDBACK_META,'轮只读字段越界')
    require(type(motor) is int and motor in (7,8,9),'只读原帧仅接受物理7、8或9')
    return read_feedback_raw(io,reference,register,check_ready,motor=motor,
                             validate=lambda raw:validate_left_feedback_raw(raw,register,motor=motor))


def arm_range_raw(io,reference,register,check_ready,*,motor):
    require(type(motor) is int and motor in range(1,7) and register in ARM_RANGE_REGS,
            '右臂范围只读仅接受物理1..6的TE/PV/PP')
    return read_feedback_raw(io,reference,register,check_ready,motor=motor,
                             validate=lambda raw:validate_arm_range_raw(raw,register,motor=motor))


def read_feedback_raw(io,reference,register,check_ready,*,motor,validate):
    """冻结b2逐ID SYNC_READ，保留完整TX/RX；白名单由各窄入口约束。"""
    verify_ref(reference,executable=True)
    path=Path(reference['path'])
    info=path.lstat()
    actual={'uid':info.st_uid,'gid':info.st_gid,'mode':stat.S_IMODE(info.st_mode),
            'device':info.st_dev,'inode':info.st_ino,'mtime_ns':info.st_mtime_ns,'ctime_ns':info.st_ctime_ns,
            'nlink':info.st_nlink,'symlink':path.is_symlink(),'elf_machine':183}
    require(all(reference.get(k)==v for k,v in actual.items()) and info.st_nlink==1
            and not path.is_symlink(),'轮只读b2完整身份变化')
    check_ready()
    text=io.command([reference['path'],DEVICES['right'],str(motor),'read-raw',register])
    require(len(text.splitlines())==1,'轮原帧输出不是单行JSON')
    raw=strict_json(text)
    validate(raw)
    return raw


def validate_left_feedback_raw(raw,register, *, motor=7):
    """采集和停止判定共用原帧检查；不信报告中的PASS字段。"""
    require(type(motor) is int and motor in (7,8,9),'只读原帧仅接受物理7、8或9')
    return validate_feedback_raw(raw,register,motor=motor)


def validate_arm_range_raw(raw,register,*,motor):
    require(type(motor) is int and motor in range(1,7) and register in ARM_RANGE_REGS,
            '右臂范围原帧目标/字段越界')
    return validate_feedback_raw(raw,register,motor=motor)


def validate_feedback_raw(raw,register,*,motor):
    address,width=LEFT_FEEDBACK_META[register]
    require(isinstance(raw,dict) and set(raw)=={
        'operation','serial_device','motor_id','register','requested_register_address','width','raw_unsigned','tx','rx'}
        and all(type(raw[k]) is int for k in ('motor_id','requested_register_address','width'))
        and (raw['operation'],raw['serial_device'],raw['motor_id'],raw['register'],
             raw['requested_register_address'],raw['width'])==('sync_read',DEVICES['right'],motor,register,address,width),
        '轮原帧目标/寄存器不符')
    tx=[255,255,254,5,130,address,width,motor]
    tx.append((~sum(tx[2:]))&255)
    rx=raw['rx'];value=raw['raw_unsigned']
    require(type(raw['tx']) is list and all(type(v) is int for v in raw['tx']) and raw['tx']==tx
            and type(rx) is list and all(type(v) is int and 0<=v<=255 for v in rx)
            and len(rx)==6+width and rx[:5]==[255,255,motor,width+2,0] and sum(rx[2:])&255==255
            and type(value) is int and int.from_bytes(bytes(rx[5:5+width]),'little')==value,
            '轮原帧ID/长度/错误码/校验/原字不符')
    return value


def validate_arm_range_capture(row,plan):
    validate_arm_range_plan(plan)
    calibrations=verify_arm_calibration(plan)
    require(row.get('kind')=='bus2_arm_range_diagnostic' and row.get('plan_sha256')==digest(plan)
            and row.get('register_writes')==0 and row.get('stop_verified') is False
            and row.get('same_frame') is False and row.get('mixed_anchor_permitted') is False
            and not row.get('errors') and not row.get('uart_blocked'),'右臂范围采集身份/结果变化')
    samples=row.get('samples',[]);ranges=row.get('ranges',[])
    require(len(samples)==18 and len(ranges)==6,'右臂范围18读原件不完整')
    start,finish=row.get('started_ns'),row.get('finished_ns')
    require(type(start) is int and type(finish) is int and start<=finish,'右臂范围采集时刻缺失')
    previous_finish=start;previous_sequence=-1
    for index,sample in enumerate(samples):
        motor=index//3+1;register=ARM_RANGE_REGS[index%3]
        begin,end,command=(sample.get(key) for key in ('started_ns','finished_ns','command_sequence'))
        require(sample.get('motor')==motor and sample.get('register')==register
                and sample.get('status')=='READ_VALID' and not sample.get('error')
                and all(type(value) is int for value in (begin,end,command))
                and previous_finish<=begin<=end<=finish and command>previous_sequence,
                '右臂范围读取顺序/时刻/命令序号变化')
        value=validate_arm_range_raw(sample.get('raw'),register,motor=motor)
        if register!='Present_Position':require(value==0,'右臂范围TE/PV非零')
        else:
            require(0<=value<=4095,'右臂范围PP不在单圈范围')
            calibration=calibrations[motor-1]
            status='INSIDE' if calibration['range_min']<=value<=calibration['range_max'] else 'OUTSIDE'
            require(ranges[motor-1]=={'motor':motor,'name':MIXED_ARM_NAMES[motor-1],'position_raw':value,
                    'range_min':calibration['range_min'],'range_max':calibration['range_max'],'status':status,
                    'sampled_ns':end},'右臂范围校准判定变化')
        previous_finish,previous_sequence=end,command
    require(row.get('range_status')==('OUTSIDE' if any(value['status']=='OUTSIDE' for value in ranges) else 'INSIDE'),
            '右臂范围汇总变化')


def run_bus2_arm_range_only(engine,check_go):
    require(not engine.rows and not getattr(engine.io,'arm_range_uart_blocked',False), '右臂范围单次序列已经执行或封锁')
    validate_arm_range_plan(engine.plan)
    io=engine.io;reference=left_feedback_tool_ref(engine.plan)
    row={'kind':'bus2_arm_range_diagnostic','status':'FAIL','samples':[],'ranges':[],'errors':[],
         'register_writes':0,'stop_verified':False,'same_frame':False,'mixed_anchor_permitted':False,
         'uart_blocked':False,'started_ns':io.now(),'plan_sha256':digest(engine.plan)}
    engine.rows.append(row)
    expected_argv=None
    def ready():
        io.check();check_go()
        require(io.child is None and not io.evidence_errors and not getattr(io,'arm_range_uart_blocked',False),
                '右臂范围child/原件异常或UART已封锁')
    def prelaunch(argv):
        require(list(argv)==expected_argv,'右臂范围只读偏离本次固定读取')
        ready()
    installed=False
    try:
        ready();calibrations=verify_arm_calibration(engine.plan)
        require(not hasattr(io,'arm_range_before_spawn'),'右臂范围采集不可嵌套')
        io.arm_range_before_spawn=prelaunch;installed=True
        for motor,calibration in enumerate(calibrations,1):
            for register in ARM_RANGE_REGS:
                ready()
                expected_argv=[reference['path'],DEVICES['right'],str(motor),'read-raw',register]
                sample={'motor':motor,'register':register,'started_ns':io.now(),'status':'UNKNOWN'}
                row['samples'].append(sample)
                try:
                    raw=arm_range_raw(io,reference,register,ready,motor=motor)
                    sample.update(raw=raw,status='READ_VALID',finished_ns=io.now(),command_sequence=io.sequence)
                    ready()
                    value=raw['raw_unsigned']
                    if register!='Present_Position':require(value==0,'右臂范围TE/PV非零，停止后续UART')
                    else:
                        require(0<=value<=4095,'右臂范围PP不在单圈范围，停止后续UART')
                        row['ranges'].append({'motor':motor,'name':MIXED_ARM_NAMES[motor-1],
                            'position_raw':value,'range_min':calibration['range_min'],'range_max':calibration['range_max'],
                            'status':'INSIDE' if calibration['range_min']<=value<=calibration['range_max'] else 'OUTSIDE',
                            'sampled_ns':sample['finished_ns']})
                except BaseException as error:
                    sample.update(finished_ns=io.now(),error=repr(error));raise
        row['range_status']='OUTSIDE' if any(value['status']=='OUTSIDE' for value in row['ranges']) else 'INSIDE'
        row['finished_ns']=io.now()
        validate_arm_range_capture(row,engine.plan)
        row['status']='DIAGNOSTIC_CAPTURED'
    except BaseException as error:
        row['errors'].append(repr(error));row['uart_blocked']=True
        io.arm_range_uart_blocked=True;engine.manual_cut=True
        raise
    finally:
        row['finished_ns']=io.now()
        if installed and getattr(io,'arm_range_before_spawn',None) is prelaunch:
            del io.arm_range_before_spawn
    return row


def validate_left_stop_window(row,plan,anchor,*,motor=7):
    """保留旧入口名，按motor核验各轮独立原帧及稳定性，不计算平均速度。"""
    contract=left_stop_contract(motor=motor)
    prefix=WHEEL_STOP_TARGETS[motor]
    require(plan.get(f'{prefix}{motor}_stop_contract')==contract
            and row.get('plan_sha256')==digest(plan),'轮停止合同或计划绑定变化')
    require((row.get('side'),row.get('motor'),row.get('name'))==tuple(contract['target'])
            and row.get('kind')==f'{prefix}{motor}_stop_stability'
            and row.get('same_frame') is False,'停止窗口目标或种类变化')
    validate_stop_stability_samples(row,contract,anchor,
        lambda sample,register:validate_left_feedback_raw(sample.get('raw'),register,motor=motor))


def validate_stop_stability_value(register,value,contract,positions):
    """只验证原始值和位置跨度，SYNC与SDK普通READ分别保留各自原帧。"""
    require(type(value) is int,'底盘稳定性原值缺失')
    if register in ('Torque_Enable','Goal_Velocity'):
        require(value==0,'底盘稳定性TE/GV非零')
    elif register=='Present_Velocity':
        require(value in contract['present_velocity_raw_unsigned'],'底盘PV超出稳定性集合')
    else:
        require(register=='Present_Position' and 0<=value<=4095,'底盘稳定性PP范围未知')
        positions.append(value)
        require(max(positions)-min(positions)<=contract['position_span_max'],'底盘PP跨度超过1')


def validate_stop_stability_samples(row,contract,anchor,read_value):
    """共用14读的值与时刻验证；原帧验证由真实传输类型提供。"""
    require(row.get('anchor_raw')==anchor and (anchor is None or type(anchor) is int and 0<=anchor<=4095),
            '轮快照锚点不符')
    start,observation,finish,deadline=(row.get(k) for k in
                                     ('started_ns','observation_started_ns','finished_ns','deadline_ns'))
    require(all(type(v) is int for v in (start,observation,finish,deadline))
            and start<=observation<=finish<=deadline==start+contract['capture_budget_ns'],
            '轮窗口时刻/预算不符')
    sequence=[('Torque_Enable',None),('Goal_Velocity',None)]
    sequence += [(register,offset) for offset in contract['sample_offsets_ns']
                 for register in ('Present_Position','Present_Velocity')]
    sequence += [('Goal_Velocity',None),('Torque_Enable',None)]
    samples=row.get('samples')
    require(isinstance(samples,list) and len(samples)==14 and not row.get('errors')
            and row.get('register_writes')==0,'轮14读原件不完整')
    positions=[] if anchor is None else [anchor]
    previous_finish=start; previous_sequence=-1
    for sample,(register,offset) in zip(samples,sequence):
        require(sample.get('register')==register and sample.get('scheduled_offset_ns')==offset
                and not sample.get('error'),'轮读取顺序/结果不符')
        begin,end,command=(sample.get(k) for k in ('started_ns','finished_ns','command_sequence'))
        require(all(type(v) is int for v in (begin,end,command))
                and previous_finish<=begin<deadline and begin<=end<=finish and command>previous_sequence,
                '轮读取时刻/命令序号不符')
        if offset is not None:
            require(begin>=observation+offset,'轮采样早于固定时刻')
        validate_stop_stability_value(register,read_value(sample,register),contract,positions)
        previous_finish,previous_sequence=end,command
    require(samples[1]['finished_ns']<=observation<=samples[2]['started_ns'],
            '轮观察起点不在前置与首组之间')
    return max(positions)-min(positions)


def validate_front_feedback_window(row,plan,*,motor=8):
    """只核采集合同和原帧，不从速度或位置变化推导停止。"""
    validate_front_feedback_plan(plan,motor=motor)
    contract=plan[feedback_entry(plan)[0]]
    require((row.get('side'),row.get('motor'),row.get('name'))==tuple(contract['target'])
            and row.get('plan_sha256')==digest(plan) and row.get('stop_verified') is False
            and row.get('kind')==f'{WHEEL_STOP_TARGETS[motor]}{motor}_feedback_diagnostic'
            and row.get('same_frame') is False
            and row.get('anchor_raw') is None
            and row.get('register_writes')==0 and not row.get('errors'),'前轮采集身份/合同变化')
    start,observation,finish,deadline=(row.get(k) for k in
        ('started_ns','observation_started_ns','finished_ns','deadline_ns'))
    require(all(type(v) is int for v in (start,observation,finish,deadline))
            and start<=observation<=finish<=deadline==start+contract['capture_budget_ns'],'前轮采集时刻/预算不符')
    sequence=[('Torque_Enable',None),('Goal_Velocity',None)]
    sequence += [(register,offset) for offset in contract['sample_offsets_ns']
                 for register in ('Present_Position','Present_Velocity')]
    sequence += [('Goal_Velocity',None),('Torque_Enable',None)]
    require(len(row.get('samples',[]))==14,'前轮14读原件不完整')
    positions=[]
    previous_finish=start;previous_sequence=-1
    for sample,(register,offset) in zip(row['samples'],sequence):
        begin,end,command=(sample.get(k) for k in ('started_ns','finished_ns','command_sequence'))
        limit=deadline if offset is None or offset==contract['sample_offsets_ns'][-1] else observation+offset+250_000_000
        require(sample.get('register')==register and sample.get('scheduled_offset_ns')==offset
                and sample.get('status')=='READ_VALID' and not sample.get('error')
                and all(type(v) is int for v in (begin,end,command))
                and previous_finish<=begin<limit and begin<=end<=min(finish,limit)
                and command>previous_sequence,'前轮读取顺序/时刻/结果不符')
        require(offset is None or begin>=observation+offset,'前轮采样早于固定时刻')
        value=validate_left_feedback_raw(sample.get('raw'),register,motor=motor)
        if register in ('Torque_Enable','Goal_Velocity'):
            require(value==0,'前轮TE/GV非零')
        elif register=='Present_Position':positions.append(value)
        previous_finish,previous_sequence=end,command
    require(type(row.get('position_span_raw')) is int and row['position_span_raw']==max(positions)-min(positions),
            '前轮PP跨度记录不符')
    require(row['samples'][1]['finished_ns']<=observation<=row['samples'][2]['started_ns'],
            '前轮观察起点不在前置与首组之间')


def collect_left_stop_window(engine,check_go,anchor=None, *, motor=7,stop_recheck=False):
    """三轮复用固定14读；物理8/9独立诊断仍仅记录，不生成停止凭据。"""
    require(type(motor) is int and motor in (7,8,9),'窗口仅接受物理7、8或9')
    require(type(stop_recheck) is bool,'停止复核选择必须为布尔值')
    front=motor in (8,9) and not stop_recheck
    prefix=WHEEL_STOP_TARGETS[motor]
    blocked='front_feedback_blocked' if front else prefix+'_stop_blocked'
    require(not getattr(engine,blocked,False),'只读窗口UART已封锁')
    if front:
        validate_front_feedback_plan(engine.plan,motor=motor)
        require(anchor is None,'前轮诊断不使用停止快照锚点')
    else:
        require(engine.plan.get(f'{prefix}{motor}_stop_contract')==left_stop_contract(motor=motor),
                '停止稳定性合同缺失或变化')
    io=engine.io
    contract=engine.plan[feedback_entry(engine.plan)[0] if front
                         else f'{prefix}{motor}_stop_contract']
    reference=left_feedback_tool_ref(engine.plan)
    row={'side':'right','motor':motor,'name':WHEEL_PULSE_TARGETS[prefix][2],
         'kind':f'{prefix}{motor}_feedback_diagnostic' if front
                else f'{prefix}{motor}_stop_stability',
         'status':'FAIL','samples':[],'errors':[],'register_writes':0,'same_frame':False,
         'stop_verified':False,'uart_blocked':False,'started_ns':io.now(),
         'anchor_raw':anchor,'plan_sha256':digest(engine.plan)}
    setattr(engine,'front_feedback_evidence' if front else prefix+'_stop_evidence',row)
    deadline=row['started_ns']+contract['capture_budget_ns']
    read_deadline=deadline
    row['deadline_ns']=deadline
    positions=[] if anchor is None else [anchor]
    installed_prelaunch=None

    def ready(after=False):
        io.check()
        check_go()
        require(io.child is None and not io.evidence_errors,'轮只读child/原件异常')
        limit=read_deadline if front else deadline
        require(io.now()<=limit if after else io.now()<limit,'只读采集超窗')

    def read(register,offset=None):
        ready()
        sample={'register':register,'scheduled_offset_ns':offset,'started_ns':io.now(),'status':'UNKNOWN'}
        row['samples'].append(sample)
        try:
            raw=left_feedback_raw(io,reference,register,ready,motor=motor)
            sample.update(raw=raw,status='READ_VALID')
            # 原字与原帧先保留，再检查返回时授权和窗口；不为补末TE追加UART。
            sample['finished_ns']=io.now()
            sample['command_sequence']=io.sequence
            ready(after=True)
            value=raw['raw_unsigned']
            if not front:
                validate_stop_stability_value(register,value,contract,positions)
            elif register in ('Torque_Enable','Goal_Velocity'):
                require(value==0,'轮TE/GV非零，停止后续只读')
            elif register=='Present_Position':
                positions.append(value)
        except BaseException as error:
            sample.update(finished_ns=io.now(),error=repr(error))
            raise

    try:
        if front:
            require(not hasattr(io,'front_feedback_before_spawn'),'前轮采集不可嵌套')
            def prelaunch(argv):
                require(len(argv)==5 and argv[:4]==[reference['path'],DEVICES['right'],str(motor),'read-raw']
                        and argv[4] in LEFT_FEEDBACK_META,'诊断采集中仅允许本次固定电机四字段READ')
                ready()
            io.front_feedback_before_spawn=prelaunch
            installed_prelaunch=prelaunch
        else:
            engine.left_stop_window_active=ready
            engine.stop_window_motor=motor
        read('Torque_Enable')
        read('Goal_Velocity')
        row['observation_started_ns']=io.now()
        for index,offset in enumerate(contract['sample_offsets_ns']):
            if front:
                read_deadline=min(deadline,row['observation_started_ns']+contract['sample_offsets_ns'][index+1]) if (
                    index+1<len(contract['sample_offsets_ns'])) else deadline
            wait=row['observation_started_ns']+offset-io.now()
            if wait>0:
                io.sleep(wait/1_000_000_000)
            read('Present_Position',offset)
            read('Present_Velocity',offset)
        read_deadline=deadline
        read('Goal_Velocity')
        read('Torque_Enable')
        row['finished_ns']=io.now()
        if front:
            row['position_span_raw']=max(positions)-min(positions)
            validate_front_feedback_window(row,engine.plan,motor=motor)
            row['status']='DIAGNOSTIC_CAPTURED'
        else:
            validate_left_stop_window(row,engine.plan,anchor,motor=motor)
            row['status']='PASS'
    except BaseException as error:
        row['errors'].append(repr(error))
        row['uart_blocked']=True
        setattr(engine,blocked,True)
        engine.manual_cut=True
        if hasattr(engine,'right_pulse_block'):
            engine.right_pulse_block(error)
        raise
    finally:
        if front:
            if installed_prelaunch is not None and getattr(io,'front_feedback_before_spawn',None) is installed_prelaunch:
                del io.front_feedback_before_spawn
        else:
            engine.left_stop_window_active=None
            engine.stop_window_motor=None
        if row['status'] not in ('PASS','DIAGNOSTIC_CAPTURED'):
            row['finished_ns']=io.now()
    return row


def run_right_wheel_feedback(engine, check_go):
    require(not engine.rows,'轮只读单次序列已经执行')
    validate_right_feedback_plan(engine.plan)
    try:
        collect_left_stop_window(engine,check_go,motor=7)
    finally:
        evidence=getattr(engine,WHEEL_STOP_TARGETS[7]+'_stop_evidence',None)
        if evidence is not None:engine.rows.append(evidence)


def run_front_wheel_feedback(engine,check_go,*,motor=8):
    require(not engine.rows,'前轮只读单次序列已经执行')
    validate_front_feedback_plan(engine.plan,motor=motor)
    try:
        collect_left_stop_window(engine,check_go,motor=motor)
    finally:
        if getattr(engine,'front_feedback_evidence',None) is not None:
            engine.rows.append(engine.front_feedback_evidence)


def capture_right_pulse_wire(io,argv,original,attempt,**kwargs):
    """保留固定底盘目标的同次普通TE原帧，禁止错标电机ID。"""
    motor=next((value[1] for value in WHEEL_PULSE_TARGETS.values()
                if argv[1:3]==[DEVICES['right'],str(value[1])]),None)
    require(motor is not None,'TE抓帧仅接受固定底盘电机')
    reading=argv[3]=='read'
    label='read' if reading else 'write'
    def trace_error(error):
        attempt.setdefault('trace_errors',[]).append(repr(error))
        io.evidence_errors.append('right_pulse_'+label+'_trace:'+repr(error))

    try:
        verify_ref(RIGHT_PULSE_TRACE,executable=True)
        trace=io.output/f'right{motor}-pulse-{label}-command_{io.sequence+1:04d}.strace'
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
                      'motor_ids':[motor],
                      'ack':'NONE_SYNC_WRITE' if sync else ('READ_STATUS_REQUIRED' if reading else 'ORDINARY_WRITE_ACK_REQUIRED'),
                      'capture_status':'NATURAL_SUCCESS_TERMINAL' if succeeded else 'ORIGINAL_COMMAND_FAILED',
                      'wire_frame_review':'PENDING_ROOT_REVIEW_OF_ACTUAL_WIRE_STRACE'}
            attempt['wire_trace']=evidence
            io.commands[-1]['ordinary_read_trace' if reading else 'diagnostic_write_trace']=evidence
        except BaseException as error:
            trace_error(error)
            if succeeded:
                raise


def load_right_pulse_sdk():
    """基础Python由固定启动参数-I -S隔离；只使用已核对的SDK/serial包。"""
    require(sys.flags.isolated and sys.flags.no_site,'SDK执行必须基础Python -I -S隔离启动')
    for reference in RIGHT_PULSE_SDK['refs']:
        verify_ref(reference)
    require(not any(name=='serial' or name.startswith('serial.') or name=='scservo_sdk'
                    or name.startswith('scservo_sdk.') for name in sys.modules),
            'SDK导入前已有同名模块，禁止混用外部包')
    site=RIGHT_PULSE_SDK['site']
    sys.path.insert(0,site)
    try:
        serial=importlib.import_module('serial')
        sdk=importlib.import_module('scservo_sdk')
    finally:
        sys.path.remove(site)
    expected={reference['path'] for reference in RIGHT_PULSE_SDK['refs']}
    for name,module in tuple(sys.modules.items()):
        if name=='serial' or name.startswith('serial.') or name=='scservo_sdk' or name.startswith('scservo_sdk.'):
            require(getattr(module,'__file__',None) in expected,'SDK实际模块不在固定闭包:'+name)
    require(serial.VERSION==RIGHT_PULSE_SDK['serial_version'],'pyserial版本变化')
    for reference in RIGHT_PULSE_SDK['refs']:
        verify_ref(reference)
    return sdk


def install_right_pulse_guard(engine,helper):
    """快照沿用原READ；SDK连接活动期间禁止CLI抢用串口，失败后不再UART。"""
    io=engine.io
    original=io.command
    holder=SimpleNamespace(command=lambda _io,argv,**kwargs:original(argv,**kwargs))
    helper.install_stop_wire_capture(SimpleNamespace(RealExecutor=holder,verify_ref=verify_ref))
    traced=lambda argv,**kwargs:holder.command(io,argv,**kwargs)
    wheel=pulse_wheel(engine.plan) if 'right_wheel_pulse' in engine.plan else WHEEL_STOP_TARGETS[9] if 'bus2_mixed_raw_hold' in engine.plan else 'right'
    side,motor,name=WHEEL_PULSE_TARGETS[wheel]
    row={'side':side,'motor':motor,'name':name,'kind':'raw_velocity_diagnostic',
         'status':'NOT_RUN','before_raw':[],'samples':[],'cleanup_raw':[],'writes':[],
         'sdk_operations':[],'errors':[],'cleanup_errors':[],'uart_blocked':False,'normal_failed':False,
         'metadata_verified':False,'goal_verified':False,'stopped_confirmed':False,
         'initial_zero_attempted':False,'initial_zero_verified':False,
         'enable_attempted':False,'enable_verified':False,'velocity_attempted':False,
         'off_attempted':False,'zero_attempted':False,'firmware_contract_verified':False}
    engine.right_pulse=row
    engine.right_pulse_helper=helper
    engine.right_sdk_active=False

    def block(error):
        row['uart_blocked']=True
        engine.manual_cut=True
        row['cleanup_errors'].append(repr(error))
        print('爸爸，单轮诊断停止/身份未知，请立即人工断电；禁止后续UART。',flush=True)
    engine.right_pulse_block=block
    tool_paths=(RIGHT_PULSE_WRITE['path'],RIGHT_PULSE_RAW['path'],RIGHT_PULSE_SYNC['path'],RIGHT_PULSE_TRACE['path'])

    def command(argv,**kwargs):
        path=str(argv[0]) if argv else ''
        cleanup=kwargs.get('cleanup',False)
        if path not in tool_paths:
            require(not any(str(arg) in tool_paths for arg in argv[1:]),'禁止其他前缀绕过右轮工具名单')
            return original(argv,**kwargs)
        require(not row['uart_blocked'],'右轮诊断UART已粘性封锁')
        require(not engine.right_sdk_active,'SDK连接未关闭，禁止CLI访问串口')
        try:
            mixed_argv=getattr(engine,'mixed_trace_argv',None)
            if 'bus2_mixed_raw_hold' in engine.plan and mixed_argv is not None and tuple(argv)==mixed_argv:
                require(not cleanup and not row['normal_failed']
                        and not getattr(engine,'mixed_command_used',False)
                        and set(kwargs)=={'allowed','mixed_stop_check'}
                        and kwargs['allowed']==(0,1)
                        and kwargs['mixed_stop_check'] is engine.mixed_stop_check,
                        '仅本次唯一混合child允许固定长预算')
                validate_mixed_raw_plan(engine.plan)
                verify_mixed_raw_refs(engine.plan)
                engine.mixed_command_used=True
                return original(argv,**kwargs)
            window=getattr(engine,'left_stop_window_active',None)
            if window is not None:
                require(path==RIGHT_PULSE_RAW['path'] and len(argv)==5
                        and argv[1:4]==[DEVICES['right'],str(engine.stop_window_motor),'read-raw']
                        and argv[4] in LEFT_FEEDBACK_META,'轮停止窗口禁止写入或其他目标读取')
                window()
                return original(argv,**kwargs)
            require(cleanup or not row['normal_failed'],'动作错误后禁止新常规UART')
            if not cleanup:
                check_go=getattr(engine,'right_pulse_go_check',None)
                if check_go is not None:
                    check_go()
            require(path==RIGHT_PULSE_WRITE['path'] and len(argv)==5 and argv[3]=='read'
                    and argv[4] in REGS and any(argv[1:3]==[DEVICES[side],str(motor)]
                    for side in SERIALS for motor in range(1,9 if side=='left' else 10)),
                    'SDK候选禁止CLI动作及其他原帧读取')
            if argv==[RIGHT_PULSE_WRITE['path'],DEVICES['right'],str(motor),'read','Torque_Enable']:
                text=capture_right_pulse_wire(io,argv,original,{},**kwargs)
            else:
                text=traced(argv,**kwargs)
            match=re.fullmatch(rf'read {re.escape(argv[4])}@{argv[2]} = (-?\d+)\n',text)
            require(match is not None,'普通READ回执未知')
            stop_motor=getattr(engine,'stop_snapshot_motor',None)
            candidate=(stop_motor in WHEEL_STOP_TARGETS
                       and argv[1:5]==[DEVICES['right'],str(stop_motor),'read','Present_Velocity']
                       and engine.plan.get(f'{WHEEL_STOP_TARGETS[stop_motor]}{stop_motor}_stop_contract')==left_stop_contract(motor=stop_motor)
                       and int(match[1]) in (0,50,-50))
            require(argv[4] not in STOP_REGS or int(match[1])==0 or candidate,
                    '原停止READ非零:'+argv[4])
            if not cleanup and check_go is not None:
                check_go()
            return text
        except BaseException as error:
            row['normal_failed']=True
            block(error)
            raise
    io.command=command


def run_right_wheel_pulse(engine):
    """一次仅选一个固定轮，同一SDK连接持续发送；不创建child或更改子进程预算。"""
    row=engine.right_pulse
    io=engine.io
    engine.rows.append(row)
    row['status']='FAIL'
    wheel=pulse_wheel(engine.plan)
    side,motor,name=WHEEL_PULSE_TARGETS[wheel]
    require((row['side'],row['motor'],row['name'])==(side,motor,name),'SDK实际目标与计划不符')
    validate_right_pulse_plan(engine.plan)
    contract=engine.plan['right_wheel_pulse']
    port=None
    wire=None
    active=None
    wire_count=0
    wire_errors=[]

    def wire_log(direction,data,**extra):
        nonlocal wire_count
        wire_count+=1
        event={'sequence':wire_count,'operation':active['sequence'] if active else None,
               'direction':direction,'at_ns':io.now(),'bytes':list(data),**extra}
        try:
            wire.write(json.dumps(event,ensure_ascii=False)+'\n')
            wire.flush()
        except BaseException as error:
            wire_errors.append(repr(error))
            io.evidence_errors.append('sdk_wire:'+repr(error))

    def ready(deadline=None,*,cleanup=False):
        if not cleanup:
            io.check()
        check_go=getattr(engine,'right_pulse_go_check',None)
        if check_go is not None:
            check_go()
        require(not wire_errors,'SDK原帧记录失败')
        if deadline is not None:
            require(io.now()<deadline,'SDK操作开始前绝对期限已耗尽')

    def frame(motor,instruction,parameters):
        data=[255,255,motor,len(parameters)+2,instruction,*parameters]
        return data+[(~sum(data[2:]))&255]

    def operation(kind,register,value=None,*,cleanup=False,deadline=None):
        nonlocal active
        if not cleanup or kind=='read':
            ready(deadline,cleanup=cleanup)
        require(not row['uart_blocked'] and engine.right_sdk_active,'SDK串口非本轮活动连接')
        address,width=RIGHT_SDK_REGS[register]
        require(kind=='read' or (register=='Torque_Enable' and kind=='write' and value in (0,1))
                or (register=='Goal_Velocity' and kind=='sync' and value in (0,500)),
                'SDK写入不在本次固定目标名单')
        parameters=([address,width] if kind=='read' else [address,value] if kind=='write'
                    else [address,width,motor,value&255,value>>8])
        expected=frame(254 if kind=='sync' else motor,0x83 if kind=='sync' else 2 if kind=='read' else 3,parameters)
        entry={'sequence':len(row['sdk_operations'])+1,'kind':kind,'motor':motor,'register':register,
               'address':address,'width':width,'value':value,'started_ns':io.now(),
               'cleanup':cleanup,'status':'UNKNOWN','expected_tx':expected,'tx_attempts':0,
               'deadline_ns':deadline,'rx_bytes':[],'wire_first_sequence':wire_count+1}
        row['sdk_operations'].append(entry)
        if kind!='read':
            row['writes'].append(entry)
        active=entry
        try:
            if kind=='read':
                data,comm,error=packet.readTxRx(port,motor,address,width)
                entry.update(data=list(data),comm=comm,error_code=error)
                require(comm==sdk.COMM_SUCCESS and error==0 and len(data)==width
                        and all(type(v) is int and 0<=v<=255 for v in data),'SDK读取通信/状态/长度未知:'+register)
                result=int.from_bytes(bytes(data),'little')
                entry['raw_unsigned']=result
            elif kind=='write':
                comm,error=packet.write1ByteTxRx(port,motor,address,value)
                entry.update(comm=comm,error_code=error)
                require(comm==sdk.COMM_SUCCESS and error==0,'SDK使能写ACK未知')
                result=None
            else:
                writer.clearParam()
                require(writer.addParam(motor,bytearray((value&255,value>>8))),'SDK单轮SYNC参数拒绝')
                comm=writer.txPacket()
                entry.update(comm=comm,ack='NONE_SYNC_WRITE')
                require(comm==sdk.COMM_SUCCESS,'SDK SYNC发送未知')
                result=None
            require(entry['tx_attempts']==1,'SDK命令没有恰好发送一次')
            if kind=='sync':
                require(not entry['rx_bytes'],'SDK SYNC不应读取或宣称目标ACK')
            else:
                payload=entry['data'] if kind=='read' else []
                response=[255,255,motor,len(payload)+2,0,*payload]
                response.append((~sum(response[2:]))&255)
                require(entry['rx_bytes']==response,'SDK实际返回帧身份/长度/ACK不符')
            entry['status']='READ_VALID' if kind=='read' else 'SENT_NO_ACK' if kind=='sync' else 'ACK_VALID'
            if not cleanup:
                ready()
                require(deadline is None or io.now()<=deadline,'SDK操作返回超过绝对期限')
            return result
        except BaseException as error:
            entry['error']=repr(error)
            raise
        finally:
            entry.update(finished_ns=io.now(),wire_last_sequence=wire_count)
            # 同一线程调用已返回；异常可能留下SDK纯软件busy位，不能阻止独立TE0尝试。
            if getattr(port,'is_using',False):
                entry['sdk_busy_reset']=True
                port.is_using=False
            active=None

    def read(register,destination,*,cleanup=False,deadline=None):
        before=len(row['sdk_operations'])
        try:
            return operation('read',register,cleanup=cleanup,deadline=deadline)
        finally:
            if len(row['sdk_operations'])>before:
                entry=row['sdk_operations'][-1]
                if entry['kind']=='read' and entry['register']==register:
                    destination.append(entry)

    def stability_value(sample,register):
        entry=sample.get('raw',{});address,width=RIGHT_SDK_REGS[register]
        data=entry.get('data',[])
        require(entry.get('kind')=='read' and entry.get('status')=='READ_VALID'
                and (entry.get('motor'),entry.get('register'),entry.get('address'),entry.get('width'))==(motor,register,address,width)
                and entry.get('comm')==sdk.COMM_SUCCESS and entry.get('error_code')==0 and entry.get('tx_attempts')==1
                and len(data)==width and all(type(v) is int and 0<=v<=255 for v in data)
                and entry.get('expected_tx')==frame(motor,2,[address,width])
                and entry.get('rx_bytes')==frame(motor,0,data)
                and entry.get('raw_unsigned')==int.from_bytes(bytes(data),'little'),'SDK稳定性普通READ原帧不符')
        return entry['raw_unsigned']

    def stable_window(phase,anchor=None):
        key=phase+'_stability'
        require(phase in ('initial','post_disable') and key not in row,'SDK稳定性窗口不得复用或重试')
        check=contract['sdk_stop_stability']['contract']
        window={'side':side,'motor':motor,'name':name,'kind':'sdk_stop_stability','transport':'SDK_ORDINARY_READ',
                'phase':phase,'status':'FAIL','started_ns':io.now(),'samples':[],'errors':[],
                'register_writes':0,'anchor_raw':anchor,'plan_sha256':digest(engine.plan)}
        row[key]=window
        window['deadline_ns']=window['started_ns']+check['capture_budget_ns']
        positions=[] if anchor is None else [anchor]

        def sample(register,offset=None):
            ready(window['deadline_ns'])
            current={'register':register,'scheduled_offset_ns':offset,'started_ns':io.now(),'status':'UNKNOWN'}
            window['samples'].append(current)
            entries=[]
            try:
                try:
                    value=read(register,entries,deadline=window['deadline_ns'])
                finally:
                    # SDK返回后先留真实普通READ entry，即使返回超时也不能丢原帧或补读。
                    current['finished_ns']=io.now()
                    if entries:
                        current.update(raw=entries[-1],command_sequence=entries[-1]['sequence'],status=entries[-1]['status'])
                ready()
                require(io.now()<=window['deadline_ns'],'SDK稳定性窗口读取返回超时')
                validate_stop_stability_value(register,value,check,positions)
            except BaseException as error:
                current.update(finished_ns=io.now(),error=repr(error))
                raise

        try:
            sample('Torque_Enable');sample('Goal_Velocity')
            window['observation_started_ns']=io.now()
            for offset in check['sample_offsets_ns']:
                ready(window['deadline_ns'])
                io.sleep(max(0,window['observation_started_ns']+offset-io.now())/1_000_000_000)
                sample('Present_Position',offset);sample('Present_Velocity',offset)
            sample('Goal_Velocity');sample('Torque_Enable')
            window['finished_ns']=io.now()
            window['position_span_raw']=validate_stop_stability_samples(window,check,anchor,stability_value)
            window['status']='PASS'
        except BaseException as error:
            window.update(finished_ns=io.now());window['errors'].append(repr(error))
            raise

    try:
        engine.check_gate()
        sdk=load_right_pulse_sdk()
        wire_path=io.output/f'right{motor}-sdk-wire.jsonl'
        wire=wire_path.open('x')
        row['wire_path']=str(wire_path)
        port=sdk.PortHandler(DEVICES['right'])
        packet=sdk.PacketHandler(0)
        writer=sdk.GroupSyncWrite(port,packet,46,2)
        raw_write,raw_read=port.writePort,port.readPort
        def write_port(data):
            require(active is not None and list(data)==active['expected_tx'] and active['tx_attempts']==0,
                    'SDK实际发送不是本轮唯一获准指令')
            if not active['cleanup'] or active['kind']=='read':
                # SDK可能先在clearPort/flush中耗时；真实写前再核绝对期限及GO。
                ready(active['deadline_ns'],cleanup=active['cleanup'])
            active['tx_attempts']+=1
            try:
                count=raw_write(data)
            except BaseException as error:
                wire_log('tx',data,error=repr(error),written_count=None)
                raise
            wire_log('tx',data,written_count=count)
            return count
        def read_port(length):
            require(active is not None,'SDK读取不属于本轮操作')
            try:
                data=raw_read(length)
            except BaseException as error:
                wire_log('rx',(),requested_length=length,error=repr(error))
                raise
            if data:
                active['rx_bytes'].extend(data)
                wire_log('rx',data,requested_length=length)
            return data
        port.writePort,port.readPort=write_port,read_port
        def timeout(packet_length):
            port.packet_start_time=port.getCurrentTime()
            port.packet_timeout=port.tx_time_per_byte*packet_length+port.tx_time_per_byte*3.0+50
        port.setPacketTimeout=timeout
        ready()
        engine.right_sdk_active=True
        row['connection']={'owner_pid':os.getpid(),'device':DEVICES['right'],'baudrate':1_000_000,
                           'open_attempted':True,'close_attempted':False,'closed':False}
        require(port.setBaudRate(1_000_000) and port.is_open,'SDK串口打开失败')
        ready()
        for register,expected in {**RIGHT_PULSE_METADATA,'Torque_Enable':0,'Present_Velocity':0,'Goal_Velocity':0}.items():
            word=read(register,row['before_raw'])
            require(word==expected or register=='Present_Velocity' and word in contract['sdk_stop_stability']['trigger_raw_unsigned'],
                    'SDK元数据/初始静止不符:'+register)
            if register=='Present_Velocity':initial_velocity=word
        anchor=read('Present_Position',row['before_raw'])
        require(0<=anchor<=4095,'SDK初始位置超单圈范围')
        row['anchor_raw']=anchor
        if initial_velocity in contract['sdk_stop_stability']['trigger_raw_unsigned']:
            stable_window('initial',anchor)
        row['metadata_verified']=True
        row['initial_zero_attempted']=True
        operation('sync','Goal_Velocity',0)
        row['initial_zero_finished_ns']=io.now()
        io.sleep(0.05)
        require(read('Goal_Velocity',row['before_raw'])==0,'初始SYNC零目标读回不符')
        row['initial_zero_verified']=True
        row['enable_attempted']=True
        row['enable_attempt_started_ns']=io.now()
        enable_deadline=io.now()+contract['enable_budget_ns']
        operation('write','Torque_Enable',1,deadline=enable_deadline)
        require(read('Torque_Enable',row['before_raw'],deadline=enable_deadline)==1,'SDK Enable读回未确认')
        row['enable_verified']=True
        ready(enable_deadline)
        started=io.now()
        deadline=started+contract['observation_ns']
        row.update(first_velocity_attempt_started_ns=started,observation_deadline_ns=deadline,
                   velocity_attempted=True,skipped_write_slots=0)
        operation('sync','Goal_Velocity',500,deadline=deadline)
        require(read('Goal_Velocity',row['before_raw'],deadline=deadline)==500,'SDK +500目标读回不符')
        row['goal_verified']=True
        next_write=started+contract['write_period_ns']
        next_sample=started+contract['sample_period_ns']
        previous=anchor
        cumulative=0
        while io.now()<deadline:
            ready()
            io.sleep(max(0,min(next_write,next_sample,deadline)-io.now())/1_000_000_000)
            ready()
            if io.now()>=deadline:
                break
            if io.now()>=next_write:
                skipped=(io.now()-next_write)//contract['write_period_ns']
                row['skipped_write_slots']+=skipped
                operation('sync','Goal_Velocity',500,deadline=deadline)
                next_write+=int(skipped+1)*contract['write_period_ns']
            if io.now()>=next_sample:
                require(io.now()<next_sample+contract['sample_budget_ns'],'SDK相邻采样未及时开始')
                sample={'offset_ns':next_sample-started,'raw':[]}
                row['samples'].append(sample)
                sample_deadline=min(deadline,next_sample+contract['sample_budget_ns'])
                word=read('Present_Velocity',sample['raw'],deadline=sample_deadline)
                require(0<=word<=contract['velocity_raw_max'],'SDK运动速度反向/负零或超过raw上限')
                position=read('Present_Position',sample['raw'],deadline=sample_deadline)
                require(0<=position<=4095,'SDK观察位置超单圈范围')
                delta=(position-previous+2048)%4096-2048
                sample.update(velocity_raw=word,position_raw=position,previous_position_raw=previous,delta_raw=delta)
                require(0<=delta<=contract['position_step_raw_max'],'SDK相邻位移反向或超过固定步长上限')
                cumulative+=delta
                sample['cumulative_delta_raw']=cumulative
                previous=position
                next_sample+=contract['sample_period_ns']
        row.update(observation_finished_ns=io.now(),cumulative_delta_raw=cumulative)
        ready()
        require(any(sample['velocity_raw']>0 for sample in row['samples'])
                and cumulative>0,'SDK未观察到正向速度及位置变化')
        if 'zero_stop_check' in contract:
            check=contract['zero_stop_check']
            zero={'status':'FAIL','started_ns':io.now(),'samples':[]}
            row['zero_stop_check']=zero
            zero['deadline_ns']=min(zero['started_ns']+check['budget_ns'],
                row['enable_attempt_started_ns']+contract['nonzero_window_fail_ns']-check['cleanup_reserve_ns'])
            operation('sync','Goal_Velocity',0,deadline=zero['deadline_ns'])
            zero['sync_finished_ns']=io.now()
            for index,offset in enumerate(check['sample_offsets_ns']):
                sample_deadline=zero['deadline_ns']
                if index+1<len(check['sample_offsets_ns']):
                    sample_deadline=min(sample_deadline,zero['sync_finished_ns']+check['sample_offsets_ns'][index+1])
                ready(sample_deadline)
                io.sleep(max(0,zero['sync_finished_ns']+offset-io.now())/1_000_000_000)
                ready(sample_deadline)
                sample={'offset_ns':offset,'deadline_ns':sample_deadline,'raw':[]}
                zero['samples'].append(sample)
                for register in ('Torque_Enable','Goal_Velocity','Present_Velocity'):
                    word=read(register,sample['raw'],deadline=sample_deadline)
                    if register=='Present_Velocity':
                        extra=(index in check['intermediate_sample_indices'] and word in check['intermediate_extra_raw_unsigned']
                               or index==len(check['sample_offsets_ns'])-1 and word in check['final_present_velocity_raw_unsigned'])
                        require(0<=word<=contract['velocity_raw_max'] or extra,'SDK保持使能零速检查的速度越界')
                    else:
                        expected=check['torque_enable' if register=='Torque_Enable' else 'goal_velocity']
                        require(word==expected,'SDK保持使能零速检查的使能/目标不符:'+register)
                sample['velocity_raw']=word
            require(zero['samples'][-1]['velocity_raw'] in check['final_present_velocity_raw_unsigned'],
                    'SDK保持使能零速检查末组原字超出0/±50')
            zero.update(status='PASS',finished_ns=io.now())
    except BaseException as error:
        row['normal_failed']=True
        row['errors'].append(repr(error))
    finally:
        if row['writes']:
            if row['velocity_attempted']:
                row['zero_attempted']=True
                try:
                    operation('sync','Goal_Velocity',0,cleanup=True)
                except BaseException as error:
                    row['cleanup_errors'].append(repr(error))
            row['off_attempted']=True
            try:
                operation('write','Torque_Enable',0,cleanup=True)
                row['disable_finished_ns']=io.now()
                if 'enable_attempt_started_ns' in row:
                    row['enabled_attempt_window_ns']=io.now()-row['enable_attempt_started_ns']
                    require(row['enabled_attempt_window_ns']<=contract['nonzero_window_fail_ns'],
                            'SDK启用尝试至断扭完成超过21秒；非硬件自动截止')
            except BaseException as error:
                row['cleanup_errors'].append(repr(error))
            if not row['errors'] and not row['cleanup_errors']:
                try:
                    row['settle_deadline_ns']=io.now()+contract['settle_budget_ns']
                    settle_start=io.now()
                    row['settle_samples']=[]
                    for offset in contract['settle_offsets_ns']:
                        ready(row['settle_deadline_ns'],cleanup=True)
                        io.sleep(max(0,settle_start+offset-io.now())/1_000_000_000,cleanup=True)
                        sample={'offset_ns':offset,'raw':[]}
                        row['settle_samples'].append(sample)
                        for register in ('Torque_Enable','Goal_Velocity','Present_Velocity'):
                            ready(row['settle_deadline_ns'],cleanup=True)
                            word=read(register,sample['raw'],cleanup=True,deadline=row['settle_deadline_ns'])
                            row['cleanup_raw'].append(sample['raw'][-1])
                            ready(cleanup=True)
                            require(io.now()<=row['settle_deadline_ns'],'SDK停稳读回超过1秒')
                            require(word==0 if register!='Present_Velocity' else
                                    0<=word<=contract['velocity_raw_max'] or word in contract['sdk_stop_stability']['trigger_raw_unsigned'],
                                    'SDK断扭后使能/目标非零或速度越界:'+register)
                        sample['velocity_raw']=word
                        if word==0:
                            row['stopped_confirmed']=True
                            break
                        if word in contract['sdk_stop_stability']['trigger_raw_unsigned']:
                            row['settle_finished_ns']=io.now()
                            stable_window('post_disable')
                            row['stopped_confirmed']=True
                            break
                    require(row['stopped_confirmed'],'SDK断扭后减速或稳定性未确认')
                    row.setdefault('settle_finished_ns',io.now())
                except BaseException as error:
                    row['cleanup_errors'].append(repr(error))
        if port is not None:
            connection=row.setdefault('connection',{'closed':False,'close_attempted':False})
            connection['close_attempted']=True
            try:
                port.closePort()
                require(not port.is_open and not getattr(getattr(port,'ser',None),'is_open',False),
                        'SDK串口关闭后仍活动')
                connection.update(closed=True,closed_ns=io.now())
                engine.right_sdk_active=False
            except BaseException as error:
                row['cleanup_errors'].append(repr(error))
        if wire is not None:
            try:
                wire.close()
                raw=Path(row['wire_path']).read_bytes()
                row['wire_evidence']={'path':row['wire_path'],'sha256':hashlib.sha256(raw).hexdigest(),
                                      'bytes':len(raw),'events':wire_count}
            except BaseException as error:
                row['cleanup_errors'].append(repr(error))
                io.evidence_errors.append('sdk_wire_close:'+repr(error))
        try:
            for reference in RIGHT_PULSE_SDK['refs']:
                verify_ref(reference)
        except BaseException as error:
            row['cleanup_errors'].append(repr(error))
        if row['errors'] or row['cleanup_errors'] or wire_errors:
            engine.right_pulse_block(ValueError('SDK本轮失败，关闭连接后禁止后续UART'))
    row['status']='PASS' if not row['errors'] and not row['cleanup_errors'] else 'FAIL'
    require(row['status']=='PASS','右轮SDK单次20秒诊断失败，禁止重试')


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


def snapshot(io, *, cleanup=False, engine=None):
    values, errors={},[]
    if engine is not None:
        # 每轮只使用本次对象凭据；上一轮或JSON旧报告均不能充当新窗口。
        for prefix in WHEEL_STOP_TARGETS.values():
            setattr(engine,prefix+'_stop_receipt',None)
            setattr(engine,prefix+'_stop_evidence',None)
        for motor,prefix in WHEEL_STOP_TARGETS.items():
            if getattr(engine,prefix+'_stop_blocked',False):
                return values,[{'motor':f'right:{motor}','register':'Present_Velocity','error':'停止窗口已粘性封锁'}]
    for side in SERIALS:
        for motor in range(1,9 if side=='left' else 10):
            row={}
            values[f'{side}:{motor}']=row
            for register in REGS:
                try:
                    if engine is not None:engine.stop_snapshot_motor=motor if side=='right' else None
                    row[register]=io.poke(side,motor,'read',register,cleanup=cleanup)
                except BaseException as error:
                    errors.append({'motor':f'{side}:{motor}','register':register,'error':repr(error)})
                finally:
                    if engine is not None:engine.stop_snapshot_motor=None
            if engine is not None and side=='right' and motor in WHEEL_STOP_TARGETS:
                prefix=WHEEL_STOP_TARGETS[motor]
                try:
                    require(set(row)==set(REGS) and row['Operating_Mode']==1
                            and type(row['Torque_Enable']) is int and row['Torque_Enable']==0
                            and type(row['Goal_Velocity']) is int and row['Goal_Velocity']==0
                            and type(row['Present_Position']) is int and 0<=row['Present_Position']<=4095
                            and type(row['Present_Velocity']) is int and row['Present_Velocity'] in (0,50,-50),
                            '停止快照不满足固定窗口前置')
                    evidence=collect_left_stop_window(engine,engine.left_stop_go_check,row['Present_Position'],
                                                      motor=motor,stop_recheck=True)
                    setattr(engine,prefix+'_stop_receipt',SimpleNamespace(values=values,plan_sha256=digest(engine.plan),
                                                                         evidence=evidence))
                except BaseException as error:
                    errors.append({'motor':f'right:{motor}','register':'Present_Velocity','error':repr(error)})
                    setattr(engine,prefix+'_stop_blocked',True)
                    engine.manual_cut=True
                    if hasattr(engine,'right_pulse_block'):
                        engine.right_pulse_block(error)
                    return values,errors
    return values,errors


def validate_left_stop_receipt(values,engine,*,motor=7):
    """保留旧入口名，三轮凭据分别绑定当前快照对象和本轮原帧。"""
    require(type(motor) is int and motor in WHEEL_STOP_TARGETS,'停止凭据仅接受固定底盘三轮')
    prefix=WHEEL_STOP_TARGETS[motor]
    receipt=getattr(engine,prefix+'_stop_receipt',None)
    require(type(receipt) is SimpleNamespace and receipt.values is values
            and receipt.plan_sha256==digest(engine.plan)
            and receipt.evidence is getattr(engine,prefix+'_stop_evidence',None)
            and not getattr(engine,prefix+'_stop_blocked',False),'缺少本轮停止窗口凭据')
    require(values[f'right:{motor}']['Present_Velocity'] in (0,50,-50),'停止快照PV超出候选集合')
    validate_left_stop_window(receipt.evidence,engine.plan,values[f'right:{motor}']['Present_Position'],motor=motor)


def classify_stop_snapshot(values, read_errors=(), cleanup_errors=(), *, child_active=False, engine=None):
    """固定17电机停止读回；底盘三轮各用本轮14读稳定性证据，原始PV不归零。"""
    issues=[]; observed={}
    stability={motor:False for motor in WHEEL_STOP_TARGETS}
    if not isinstance(values,dict):
        return {'status':'UNKNOWN','issues':[{'code':'STOP_SNAPSHOT_MISSING'}],'observed':{},'motors_expected':17}
    if set(values)!=set(MOTOR_KEYS):
        issues.append({'code':'STOP_MOTOR_SET_INCOMPLETE','status':'UNKNOWN'})
    if engine is not None:
        for motor,prefix in WHEEL_STOP_TARGETS.items():
            try:
                validate_left_stop_receipt(values,engine,motor=motor)
                stability[motor]=True
            except BaseException as error:
                issues.append({'code':f'{prefix.upper()}{motor}_STABILITY_UNCONFIRMED','status':'UNKNOWN','error':repr(error)})
    for key in MOTOR_KEYS:
        row=values.get(key,{})
        row=row if isinstance(row,dict) else {}
        observed[key]={}
        for register in STOP_REGS:
            actual=row.get(register)
            observed[key][register]=actual
            if type(actual) is not int:
                issues.append({'code':'STOP_REGISTER_UNKNOWN','status':'UNKNOWN','motor':key,'register':register,'actual':actual})
            elif actual!=0 and not (register=='Present_Velocity' and actual in (50,-50)
                                    and any(key==f'right:{motor}' and valid for motor,valid in stability.items())):
                issues.append({'code':'STOP_REGISTER_NONZERO','status':'FAIL','motor':key,'register':register,'actual':actual})
    for error in read_errors:
        if not isinstance(error,dict) or error.get('register') in STOP_REGS:
            issues.append({'code':'STOP_READ_FAILED','status':'UNKNOWN','read_error':error})
    for error in cleanup_errors:
        issues.append({'code':'STOP_CLEANUP_FAILED','status':'FAIL','cleanup_error':error})
    if child_active:
        issues.append({'code':'STOP_CHILD_NOT_REAPED','status':'UNKNOWN'})
    status='UNKNOWN' if any(e['status']=='UNKNOWN' for e in issues) else 'FAIL' if issues else 'PASS'
    return {'status':status,'issues':issues,'observed':observed,'motors_expected':17,'registers':list(STOP_REGS),
            **{f'{prefix}{motor}_stability_used':stability[motor] for motor,prefix in WHEEL_STOP_TARGETS.items()}}


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
        if probe is not None and (probe['kind'] in ('position','position_hold') and 'anchor_raw' in probe
                                 or probe['kind']=='position_release' and (side,motor)==('right',6) and probe.get('goal_raw') in GRIPPER_TARGETS):
            anchor=probe['goal_raw'] if probe['kind']=='position_release' else probe['anchor_raw']
            tolerance=GRIPPER_TOLERANCE_RAW if probe['kind']=='position_release' else 4
            if goal!=anchor or abs(actual-anchor)>tolerance:
                issues.append({'code':'POSITION_FINAL_ANCHOR_ERROR','status':'FAIL','motor':key,'target_raw':anchor,
                               'actual_raw':actual,'goal_raw':goal,'signed_error_raw':actual-anchor,'tolerance_raw':tolerance})
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
        values,read_errors=snapshot(io,cleanup=True,engine=engine)
    except BaseException as error:
        values,read_errors={},[{'code':'FINAL_SNAPSHOT_ERROR','error':repr(error)}]
    report['after'],report['after_errors']=values,read_errors
    for motor,prefix in WHEEL_STOP_TARGETS.items():
        report[f'after_{prefix}{motor}_stability']=getattr(engine,prefix+'_stop_evidence',None)
    cleanup_errors=[{'motor':f"{row['side']}:{row['motor']}",'error':error}
                    for row in engine.rows for error in row.get('cleanup_errors',[])]
    if engine.manual_cut:
        cleanup_errors.append({'error':'已发生未确认的安全收尾，粘性失败不被最终零值覆盖'})
    report['stop_verification']=classify_stop_snapshot(values,read_errors,cleanup_errors,
                                                      child_active=io.child is not None,engine=engine)
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
            or any(p['status']!='PASS' for p in probes) or report.get('evidence_errors')):
        return 'FAIL'
    if 'right_gripper_release' in report.get('plan',{}):
        try:
            validate_gripper_release_plan(report['plan'])
            require([(p['side'],p['motor'],p['kind']) for p in probes]==[('right',6,'position_release')],
                    '右夹爪单程探针必须唯一为物理6')
            require(probes[0].get('goal_raw')==report['plan']['right_gripper_release']['goal_raw'],
                    '右夹爪探针目标与本次计划不符')
        except BaseException:
            return 'FAIL'
        return 'PASS'
    if len(probes)!=7:
        return 'FAIL'
    if any(t.get('decision')=='DEFERRED' for t in report.get('plan',{}).get('base_targets',[])):
        return 'FAIL'
    if 'bus2_mixed_raw_hold' in report.get('plan',{}):
        try:
            validate_mixed_raw_plan(report['plan'])
            require([(p['side'],p['motor'],p['kind']) for p in probes]
                    ==[('right',motor,'position_hold') for motor in range(1,7)]+[('right',9,'velocity')],
                    '混合维护探针集合错误')
            result=report['mixed_raw_result']
            require(result['result']=='PASS' and result['error'] is None,'缺少本次混合child成功原件')
            for probe,anchor in zip(probes[:6],result['arm_anchors']):
                require(type(probe.get('anchor_raw')) is int and probe['anchor_raw']==anchor['raw']
                        and probe.get('sampled_ns')==anchor['sampled_ns']
                        and probe.get('hold_verification')=={'status':'PASS','tolerance_raw':4,'feedback_samples':79},
                        '混合保持探针与本次锚点原件不符')
        except BaseException:
            return 'FAIL'
    return 'PASS'


def run_motion_probes(engine,bindings,plan):
    if 'right_gripper_release' in plan:
        position_targets_from_plan(plan)
        engine.gripper_release(bindings['calibrations']['right_arm_gripper'])
        return
    if 'bus2_mixed_raw_hold' in plan:
        validate_mixed_raw_plan(plan)
        run_bus2_mixed_raw_hold(engine)
        return
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


def validate_snapshot(values, before=None, probes=(), *, engine=None):
    if engine is not None:
        for motor in WHEEL_STOP_TARGETS:
            validate_left_stop_receipt(values,engine,motor=motor)
    targets={(row['side'],row['motor']):row for row in probes
             if row.get('status')!='NOT_RUN' and row.get('decision')!='DEFERRED'}
    for key,row in values.items():
        side,motor=key.split(':'); motor=int(motor)
        require(set(row)==set(REGS), f'寄存器缺失：{key}')
        require(all(type(row[register]) is int for register in STOP_REGS)
                and row['Torque_Enable']==row['Goal_Velocity']==0
                and (row['Present_Velocity']==0 or side=='right' and motor in WHEEL_STOP_TARGETS and engine is not None
                     and row['Present_Velocity'] in (50,-50)), f'非安全静止：{key}')
        require(row['Operating_Mode']==(1 if side=='right' and motor>=7 else 0),f'mode漂移：{key}')
        require(0<=row['Present_Position']<=4095, f'位置范围未知：{key}')
        if before is None:
            continue
        old=before[key]
        probe=targets.get((side,motor))
        if probe is not None and (probe['kind'] in ('position','position_hold') and 'anchor_raw' in probe
                                 or probe['kind']=='position_release' and (side,motor)==('right',6) and probe.get('goal_raw') in GRIPPER_TARGETS):
            target=probe['goal_raw'] if probe['kind']=='position_release' else probe['anchor_raw']
            tolerance=GRIPPER_TOLERANCE_RAW if probe['kind']=='position_release' else 4
            require(row['Goal_Position']==target
                    and abs(row['Present_Position']-target)<=tolerance,f'最终锚点未确认：{key}')
        else:
            require(row['Goal_Position']==old['Goal_Position'],f'非目标GoalPosition变化：{key}')
            if probe is None:
                require(abs(row['Present_Position']-old['Present_Position'])<=4,f'非目标位置变化：{key}')


def execute(plan, go, approval):
    verify_bus_config(plan)
    require(plan['schema']=='MAINTENANCE_PLAN_V2','拒绝旧V1计划')
    position_targets_from_plan(plan)
    require(RIGHT_PULSE_RAW['path'] in plan['bindings'].get('binary_paths',[]),'停止窗口进程名单缺b2')
    left_feedback_tool_ref(plan)
    bindings = plan['bindings']
    host = bindings['host']
    require(sys.platform == 'linux' and os.uname().machine == 'aarch64'
            and os.uname().nodename == host['hostname'] and os.getuid() == os.geteuid() == host['uid']
            and os.getgid() == os.getegid() == host['gid']
            and Path('/proc/sys/kernel/random/boot_id').read_text().strip() == host['boot_id'], "真实主机身份不符")
    validate_go(go, plan, time.time_ns())
    arm_range_only='bus2_arm_range_only' in plan
    feedback_only=any(f'{role}_wheel_feedback_only' in plan for role in WHEEL_ROLES)
    diagnostic_only=arm_range_only or feedback_only
    feedback_motor=feedback_entry(plan)[1] if feedback_only else None
    front_feedback_only=feedback_motor in (8,9)
    feedback_verification=('arm_range_capture' if arm_range_only else f'{WHEEL_STOP_TARGETS[feedback_motor]}{feedback_motor}_feedback_capture'
                           if front_feedback_only else f'{WHEEL_STOP_TARGETS[7]}7_candidate_verification')
    pulse_helper=load_right_pulse_helper() if any(key in plan for key in ('right_wheel_pulse','bus2_mixed_raw_hold')) else None
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
    io.gripper_release_capture='right_gripper_release' in plan
    if 'bus2_mixed_raw_hold' in plan:
        io.mixed_feetech_tool=plan['bus2_mixed_raw_hold']['tool']
    io.arm_range_read_only=arm_range_only
    io.process_snapshots=[]
    for signum in (signal.SIGINT,signal.SIGTERM,signal.SIGHUP,signal.SIGQUIT):
        signal.signal(signum,io.mark_signal)
    engine = None
    report = {'schema':'MAINTENANCE_RUN_V2','plan':plan,'go':go,'result':'FAIL','hardware_layer':'直接wire维护',
              'S0_S1_head_base_integration':False,'H2_D15_closed':False,'deployable':False}
    if 'bus2_mixed_raw_hold' in plan:
        report['hardware_layer']='维护层真实FeetechAdapter混合链'
    if diagnostic_only:
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
            engine.left_stop_go_check=lambda:validate_go(go,plan,time.time_ns())
            engine.go_expires_at_unix_ns=go['expires_at_ns']
            if pulse_helper is not None:
                install_right_pulse_guard(engine,pulse_helper)
                engine.right_pulse_go_check=lambda:validate_go(go,plan,time.time_ns())
            try:
                if arm_range_only:
                    run_bus2_arm_range_only(engine,lambda:validate_go(go,plan,time.time_ns()))
                elif feedback_only:
                    if front_feedback_only:
                        run_front_wheel_feedback(engine,lambda:validate_go(go,plan,time.time_ns()),motor=feedback_motor)
                    else:run_right_wheel_feedback(engine,lambda:validate_go(go,plan,time.time_ns()))
                else:
                    report['before'],report['before_errors']=snapshot(io,engine=engine)
                    for motor,prefix in WHEEL_STOP_TARGETS.items():
                        report[f'before_{prefix}{motor}_stability']=getattr(engine,prefix+'_stop_evidence',None)
                    require(not report['before_errors'],'初始17电机读取失败')
                    validate_snapshot(report['before'],engine=engine)
                    run_motion_probes(engine,bindings,plan)
            finally:
                if diagnostic_only:
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
            if hasattr(engine,'mixed_raw_result'):
                report['mixed_raw_result']=engine.mixed_raw_result
        report['received_signal']=io.interrupted
        report['child_active']=io.child is not None
        if io.child is not None:
            report['result']='NEEDS_MANUAL_POWER_CUT'
        report['commands']=io.commands
        report['process_snapshots']=io.process_snapshots
        report['evidence_errors']=io.evidence_errors
        if arm_range_only:
            captured=(engine is not None and len(engine.rows)==1
                      and engine.rows[0]['status']=='DIAGNOSTIC_CAPTURED' and not engine.manual_cut)
            if captured:
                try:validate_arm_range_capture(engine.rows[0],plan)
                except BaseException as error:
                    captured=False;report['error']=repr(error)
            valid=(captured and not report.get('error') and io.interrupted is None
                   and io.child is None and not io.evidence_errors
                   and report.get('identity_verification',{}).get('status')=='PASS')
            report[feedback_verification]={'status':'DIAGNOSTIC_CAPTURED' if valid else 'FAIL',
                'contract':'BUS2_ARM_RANGE_DIAGNOSTIC_ONLY',
                'range_status':engine.rows[0].get('range_status','UNKNOWN') if engine and engine.rows else 'UNKNOWN',
                'mixed_anchor_permitted':False}
            report['result']='DIAGNOSTIC_CAPTURED' if valid else 'NEEDS_MANUAL_POWER_CUT'
            if not valid:
                print('爸爸，右臂范围只读采集或身份失败，请立即人工断电；禁止后续UART。',flush=True)
        elif feedback_only:
            captured=(engine is not None and len(engine.rows)==1
                      and engine.rows[0]['status']==('DIAGNOSTIC_CAPTURED' if front_feedback_only else 'PASS')
                      and len(engine.rows[0]['samples'])==14 and not engine.manual_cut)
            if captured:
                try:
                    if front_feedback_only:validate_front_feedback_window(engine.rows[0],plan,motor=feedback_motor)
                    else:validate_left_stop_window(engine.rows[0],plan,None,motor=7)
                except BaseException as error:
                    captured=False
                    report['error']=repr(error)
            valid=(captured and not report.get('error') and io.interrupted is None
                   and io.child is None and not io.evidence_errors
                   and report.get('identity_verification',{}).get('status')=='PASS')
            report[feedback_verification]={'status':('DIAGNOSTIC_CAPTURED' if front_feedback_only else 'PASS') if valid else 'FAIL',
                'contract':f'PHYSICAL{feedback_motor}_DIAGNOSTIC_ONLY' if front_feedback_only else 'RIGHT7_STABILITY_V1'}
            report['result']='DIAGNOSTIC_CAPTURED' if valid else 'NEEDS_MANUAL_POWER_CUT'
            if not valid:
                print('爸爸，轮只读采集或身份失败，请立即人工断电；禁止后续UART。',flush=True)
        elif engine:
            report['result']=classify_run_result(report)
        report['manual_power_cut_required']=report['result']=='NEEDS_MANUAL_POWER_CUT'
        # 先确认journal关闭结果再写最终报告，不能留下PASS原件后才因关闭失败exit1。
        try:
            io.journal.close()
        except BaseException as error:
            report['journal_close_error']=repr(error)
            if diagnostic_only:
                report[feedback_verification]['status']='FAIL'
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
            if diagnostic_only:
                report['result']='NEEDS_MANUAL_POWER_CUT'
                report['manual_power_cut_required']=True
                report[feedback_verification]['status']='FAIL'
            elif report['result'] != 'NEEDS_MANUAL_POWER_CUT':
                report['result']='FAIL'
            report['report_write_error']=repr(error)
            print(json.dumps(report,ensure_ascii=False),flush=True)
    return 0 if report['result'] in ('PASS','DIAGNOSTIC_CAPTURED') else 1


def main(argv=None):
    set_process_name()
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bus-config',type=Path,default=DEFAULT_BUS_CONFIG,
                        help='右总线TOML，joints/motor_ids为唯一轮位映射；计划绑定其绝对路径和原始字节身份')
    parser.add_argument('--bindings',type=Path)
    parser.add_argument('--output',type=Path)
    parser.add_argument('--run-id')
    parser.add_argument('--artifact-root',type=Path)
    parser.add_argument('--execute',action='store_true')
    parser.add_argument('--approval',type=Path)
    parser.add_argument('--position-target', action='append', choices=tuple(POSITION_TARGETS),
                        help='可重复指定腕部/头部目标，按指定顺序执行；缺省为原四项')
    wheels=parser.add_mutually_exclusive_group()
    wheels.add_argument('--right-wheel-pulse',action='store_true',
                        help='配置右轮入口：同一SDK连接固定+500持续20秒')
    wheels.add_argument('--wheel-pulse',choices=('front','left'),action='append',
                        help='一次仅选配置前轮或左轮，固定+500持续20秒；禁止混用位置选择')
    parser.add_argument('--right-wheel-feedback-only',action='store_true',
                        help='仅配置右轮14次只读，物理7用严格稳定性、8/9仅作诊断；不写电机、不判整机停止')
    parser.add_argument('--front-wheel-feedback-only',action='store_true',
                        help='仅配置前轮14次只读；物理7用严格稳定性、8/9仅诊断，不判整机停止')
    parser.add_argument('--left-wheel-feedback-only',action='store_true',
                        help='仅配置左轮14次只读；物理7用严格稳定性、8/9仅诊断，不判整机停止')
    parser.add_argument('--zero-stop-check',action='store_true',
                        help='仅附加单轮pulse：零速后保持使能，500毫秒内三组检查，末组PV原字限0/50/0x8032')
    parser.add_argument('--bus2-mixed-raw-hold',action='store_true',
                        help='仅FW3.10/Phase12：六臂原位保持及物理9 raw+500/20秒；复用整机停止后验')
    parser.add_argument('--bus2-arm-range-only',action='store_true',
                        help='仅右臂1..6逐ID SYNC_READ TE/PV/PP，共18读；记录校准范围，不写电机、不判停止')
    grippers=parser.add_mutually_exclusive_group()
    grippers.add_argument('--right-gripper-release',action='store_true',help=GRIPPER_RELEASE_SCOPE)
    grippers.add_argument('--right-gripper-close',action='store_true',help=GRIPPER_TARGETS[1397]['scope'])
    options=parser.parse_args(argv)
    configure_bus_mapping(options.bus_config)
    require(options.wheel_pulse is None or len(options.wheel_pulse)==1,'每次仅接受一个轮目标')
    wheel_target=options.wheel_pulse[0] if options.wheel_pulse else None
    pulse_requested=options.right_wheel_pulse or wheel_target is not None
    gripper_requested=options.right_gripper_release or options.right_gripper_close
    gripper_goal=1397 if options.right_gripper_close else 1600
    require(not options.zero_stop_check or pulse_requested,'零速保持使能检查只能附加到单轮pulse')
    require(sum((bool(pulse_requested),options.right_wheel_feedback_only,options.front_wheel_feedback_only,
                 options.left_wheel_feedback_only,options.bus2_mixed_raw_hold,options.bus2_arm_range_only,
                 gripper_requested))<=1
            and not ((pulse_requested or options.right_wheel_feedback_only or options.front_wheel_feedback_only
                      or options.left_wheel_feedback_only or options.bus2_mixed_raw_hold or options.bus2_arm_range_only
                      or gripper_requested)
                     and options.position_target is not None),'轮诊断禁止互相混用或混用位置选择')
    selected_targets = [] if (pulse_requested or options.right_wheel_feedback_only or options.front_wheel_feedback_only
                             or options.left_wheel_feedback_only or options.bus2_mixed_raw_hold
                             or options.bus2_arm_range_only or gripper_requested) else select_position_targets(options.position_target)
    if not all((options.bindings,options.output,options.run_id,options.artifact_root)):
        require(not options.execute,'执行参数不完整')
        print(json.dumps({'result':'PLAN_ONLY','actions_started':0,'required':['bindings','output','run-id','artifact-root'],
                          'position_targets':selected_targets,
                          'scope':GRIPPER_TARGETS[gripper_goal]['scope'] if gripper_requested else '单电机独立维护；不运行硬件、不改保护'},ensure_ascii=False))
        return 0
    plan=make_plan(strict_json(options.bindings.read_text()),options.output,options.run_id,options.artifact_root,
                   options.position_target,right_wheel_pulse=options.right_wheel_pulse,
                   right_wheel_feedback_only=options.right_wheel_feedback_only,wheel_pulse_target=wheel_target,
                   zero_stop_check=options.zero_stop_check,front_wheel_feedback_only=options.front_wheel_feedback_only,
                   left_wheel_feedback_only=options.left_wheel_feedback_only,
                   bus2_mixed_raw_hold=options.bus2_mixed_raw_hold,bus2_arm_range_only=options.bus2_arm_range_only,
                   right_gripper_release=gripper_requested,gripper_goal=gripper_goal)
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
