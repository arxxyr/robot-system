#!/usr/bin/env -S uv run --no-project python
"""爸爸，受监督直接wire维护；默认PLAN_ONLY，精度与停机证据分别判定。"""
import argparse
import contextlib
import copy
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
UNITS = [f"robot-{role}-{side}{'-arm' if role == 's0' else ''}.service"
         for side in SERIALS for role in ("s0", "s1", "gateway")]
OLD_UNITS = [name.replace('robot-', 'xlerobot-', 1) for name in UNITS] + [
    "xlerobot-coordinator.service", "xlerobot-web.service", "xlerobot-handoff.service",
    "xlerobot-s0-bus1.service", "xlerobot-s0-bus2.service", "robot-s0-bus1.service", "robot-s0-bus2.service"]
HANDOFF_UNIT = 'robot-s0-preflight.service'
MAINTENANCE_NAMESPACE = Path('/home/loosqk/robot-checks')
STOP_REGS = ('Torque_Enable','Present_Velocity','Goal_Velocity')
MIXED_CHILD_BUDGET_NS = 28_000_000_000
S0_MIXED_SCOPE = ('默认S0独占右总线，typed ClearEStop/SetEnabled及心跳命令流；'
                  '六臂冻结新鲜反馈位置，配置前轮raw+500持续20秒，其余轮raw0；'
                  '500毫秒零速原字观测、Disable及EStop，退出并取回右锁后原整机STOP51；'
                  '左双锁全程持有，不解除D15或物理验收')
S0_POSITION_SCOPE = ('默认S0独占左总线，typed ClearEStop/SetEnabled及心跳命令流；'
                     '左臂六关节与头部两关节冻结新鲜反馈原位保持20秒，400拍逐拍检查八关节±4raw；'
                     'Disable及EStop，退出并取回左锁后原整机STOP51与原4raw位置检查；'
                     '右双锁全程持有，不启动相机，不解除D15或物理验收')
S0_CAMERA_SCOPE = '同活动hardware_id的1..3路配置sensor并行采集完整20秒，核对逐帧新鲜度、原BGR及helper退出/设备释放'


def s0_scope(active_side,with_cameras=False):
    scope=S0_MIXED_SCOPE if active_side=='right' else S0_POSITION_SCOPE
    return scope.replace('不启动相机，','')+'；'+S0_CAMERA_SCOPE if with_cameras else scope
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
LEFT_BUS_CONFIG = None
WHEEL_ROLES = ('front','left','right')
WHEEL_LABELS = {'front':'前','left':'左','right':'右'}
DEFAULT_BUS_CONFIG = Path(__file__).resolve().parents[1]/'configs/xlerobot-bus2.toml'
DEFAULT_LEFT_BUS_CONFIG = Path(__file__).resolve().parents[1]/'configs/xlerobot-bus1.toml'
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
                   'PP须在0..4095，按原六项校准及配置容差记录INSIDE/WITHIN_TOLERANCE/OUTSIDE；不写电机、不判整机停止，'
                   '不提供混合动作的新鲜锚点或动作授权')
ARM_ELBOW_RANGE_SCOPE = ('仅右肘物理3以单ID SYNC_READ读取TE→PV→PP，共3次只读；TE/PV严格0，'
                         'PP须在0..4095，按原物理3校准及配置容差记录INSIDE/WITHIN_TOLERANCE/OUTSIDE；不写电机、不判整机停止，'
                         '不提供混合动作的新鲜锚点或动作授权')
GRIPPER_TOLERANCE_RAW = 5
GRIPPER_MAX_INCREMENT_RAW = 256
GRIPPER_TARGETS = {goal:{'goal_raw':goal,'direction':direction,
    'scope':(f'仅右夹爪物理6：原校准范围加配置容差内新鲜静止初值到固定GP{goal}单程{action}，'
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


def read_bus_configuration(path,side):
    """容差与joints/motor_ids同槽读取，缺省保持原校准边界。"""
    actual=Path(path).resolve(strict=True)
    require(actual.is_file(),'总线配置必须为普通文件')
    data=actual.read_bytes();document=tomllib.loads(data.decode('utf-8'))
    hardware=document.get('hardware')
    require(type(hardware) is list and len(hardware)==1,'维护配置必须恰含一条总线')
    row=hardware[0];names=row.get('joints');ids=row.get('motor_ids')
    count=8 if side=='left' else 9
    require(row.get('adapter')=='feetech_sts3215' and row.get('address')==DEVICES[side],
            '维护配置必须为既有总线设备与Feetech适配器')
    require(type(names) is list and type(ids) is list and len(names)==len(ids)==count
            and all(type(name) is str for name in names) and len(set(names))==count
            and all(type(motor) is int for motor in ids) and len(set(ids))==count,
            '总线joints/motor_ids必须为完整唯一名称及整数ID')
    tolerances=row.get('calibration_range_tolerances_raw',[0]*count)
    require(type(tolerances) is list and len(tolerances)==count
            and all(type(value) is int and 0<=value<=4095 for value in tolerances),
            'calibration_range_tolerances_raw必须与motor_ids等长且每项为0..4095整数')
    return dict(zip(names,ids)),{'path':str(actual),'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest(),
                               'motor_ids':ids,'calibration_range_tolerances_raw':tolerances}


def configure_bus_mapping(path,left_path=None):
    """唯一映射及逐电机容差源为本次显式绑定的左右总线TOML。"""
    global BASE_TARGETS, WHEEL_PULSE_TARGETS, WHEEL_STOP_TARGETS, BUS_CONFIG, LEFT_BUS_CONFIG
    global RIGHT_FEEDBACK_SCOPE, FRONT_FEEDBACK_SCOPE, LEFT_FEEDBACK_SCOPE, MIXED_RAW_SCOPE
    mapping,right=read_bus_configuration(path,'right')
    require(set(mapping)==set(MIXED_ARM_NAMES)|{f'base_{role}_wheel' for role in WHEEL_ROLES}
            and all(mapping[name]==motor for motor,name in enumerate(MIXED_ARM_NAMES,1))
            and {mapping[f'base_{role}_wheel'] for role in WHEEL_ROLES}=={7,8,9},
            '固定六臂1..6及三轮7/8/9映射不合法')
    if left_path is None:
        left_path=LEFT_BUS_CONFIG['path'] if LEFT_BUS_CONFIG is not None else DEFAULT_LEFT_BUS_CONFIG
    left_mapping,left=read_bus_configuration(left_path,'left')
    left_names=[name.replace('right_','left_',1) for name in MIXED_ARM_NAMES]+['head_motor_1','head_motor_2']
    require(left_mapping==dict(zip(left_names,range(1,9))),'固定左六臂1..6及头部7/8映射不合法')
    BASE_TARGETS=[('right',mapping[f'base_{role}_wheel'],f'base_{role}_wheel') for role in WHEEL_ROLES]
    WHEEL_PULSE_TARGETS=dict(zip(WHEEL_ROLES,BASE_TARGETS))
    WHEEL_STOP_TARGETS={motor:role for role,(_,motor,_) in WHEEL_PULSE_TARGETS.items()}
    BUS_CONFIG=right;LEFT_BUS_CONFIG=left
    scopes={role:feedback_scope(motor) for role,(_,motor,_) in WHEEL_PULSE_TARGETS.items()}
    RIGHT_FEEDBACK_SCOPE=scopes['right'];FRONT_FEEDBACK_SCOPE=scopes['front'];LEFT_FEEDBACK_SCOPE=scopes['left']
    MIXED_RAW_SCOPE=mixed_raw_scope(9)
    return copy.deepcopy(BUS_CONFIG)


def wheel_ids():
    require(BUS_CONFIG is not None,'必须先显式加载右总线配置')
    return [motor for _,motor,_ in BASE_TARGETS]


def wheel_label(motor):
    return f'{WHEEL_LABELS[WHEEL_STOP_TARGETS[motor]]}{motor}'


def mixed_raw_scope(selected_motor_id):
    require(type(selected_motor_id) is int and selected_motor_id in (7,8,9),'混合维护仅接受物理7/8/9')
    held='/'.join(wheel_label(motor) for motor in wheel_ids() if motor!=selected_motor_id)
    return MIXED_RAW_SCOPE_TEMPLATE.format(held=held,selected=wheel_label(selected_motor_id))


def feedback_scope(motor):
    template=RIGHT_FEEDBACK_SCOPE_TEMPLATE if motor==7 else FRONT_FEEDBACK_SCOPE_TEMPLATE
    return template.format(target=WHEEL_LABELS[WHEEL_STOP_TARGETS[motor]],motor=motor)


def verify_bus_config(plan):
    """执行和GO每次复核原配置字节身份；禁止只信进程已加载的映射。"""
    for key,current in (('bus_config',BUS_CONFIG),('left_bus_config',LEFT_BUS_CONFIG)):
        reference=plan.get(key)
        require(type(reference) is dict and set(reference)=={'path','bytes','sha256','motor_ids','calibration_range_tolerances_raw'}
                and Path(reference['path']).is_absolute() and reference==current,
                '计划未绑定本次实际左右总线配置')
        data=Path(reference['path']).read_bytes()
        require(len(data)==reference['bytes'] and hashlib.sha256(data).hexdigest()==reference['sha256'],
                '总线配置字节身份变化，旧计划/GO已失效')


def motor_range_tolerance(plan,side,motor):
    require(side in SERIALS and type(motor) is int,'校准容差目标错误')
    reference=(BUS_CONFIG if side=='right' else LEFT_BUS_CONFIG) if plan is None else plan.get(
        'bus_config' if side=='right' else 'left_bus_config')
    require(isinstance(reference,dict),'缺少绑定总线的逐电机校准容差')
    ids=reference.get('motor_ids');values=reference.get('calibration_range_tolerances_raw')
    require(type(ids) is list and motor in ids and type(values) is list and len(values)==len(ids)
            and all(type(value) is int and 0<=value<=4095 for value in values),'逐电机校准容差字段错误')
    return values[ids.index(motor)]


def effective_calibration_bounds(calibration,tolerance):
    require(type(tolerance) is int and 0<=tolerance<=4095
            and all(type(calibration[key]) is int for key in ('range_min','range_max'))
            and 0<=calibration['range_min']<=calibration['range_max']<=4095,'校准边界/容差错误')
    return max(0,calibration['range_min']-tolerance),min(4095,calibration['range_max']+tolerance)


def calibration_range_status(value,calibration,plan,side,motor):
    low,high=effective_calibration_bounds(calibration,motor_range_tolerance(plan,side,motor))
    return ('INSIDE' if calibration['range_min']<=value<=calibration['range_max'] else
            'WITHIN_TOLERANCE' if low<=value<=high else 'OUTSIDE')


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


def arm_range_contract(bindings,arm_range_id=None):
    require(arm_range_id is None or type(arm_range_id) is int and arm_range_id==3,'右臂单ID只读仅接受物理3')
    targets=[['right',motor,MIXED_ARM_NAMES[motor-1]] for motor in (range(1,7) if arm_range_id is None else (3,))]
    return {'schema':'MAINTENANCE_BUS2_ARM_RANGE_V1','scope':ARM_RANGE_SCOPE if arm_range_id is None else ARM_ELBOW_RANGE_SCOPE,
            'targets':targets,
            'raw_tool':dict(RIGHT_PULSE_RAW),'arm_calibration':dict(arm_calibration_ref(bindings)),
            'operation':'sync_read','registers':list(ARM_RANGE_REGS),'reads':3*len(targets),'register_writes':0,
            'torque_enable':0,'present_velocity':0,'position_raw_min':0,'position_raw_max':4095,
            'stop_verified':False,'same_frame':False,'mixed_anchor_permitted':False}


def validate_arm_range_plan(plan):
    contract=plan.get('bus2_arm_range_only')
    require(isinstance(contract,dict),'缺少右臂范围只读合同')
    selected=3 if contract.get('targets')==[['right',3,MIXED_ARM_NAMES[2]]] else None
    require(not any(key in plan for key in ('right_wheel_pulse','right_wheel_feedback_only',
                'front_wheel_feedback_only','left_wheel_feedback_only','bus2_mixed_raw_hold','right_gripper_release'))
            and plan.get('position_targets')==[] and plan.get('scope')==contract.get('scope')
            and contract==arm_range_contract(plan['bindings'],selected),
            '右臂范围只读禁止混用动作、轮诊断或改动固定合同')
    require(RIGHT_PULSE_RAW['path'] in plan['bindings'].get('binary_paths',[]),'右臂范围只读进程名单缺b2')
    left_feedback_tool_ref(plan)
    require(plan.get('base_targets')==[{'side':side,'motor':motor,'name':name,'decision':'DEFERRED',
            'reason_code':'BASE_FIRMWARE_CONTRACT_UNVERIFIED'} for side,motor,name in BASE_TARGETS],
            '右臂范围只读不得解除三轮合同DEFERRED')


def mixed_raw_contract(bindings,selected_motor_id=9):
    refs=mixed_raw_refs(bindings)
    scope=mixed_raw_scope(selected_motor_id)
    return {'schema':'MAINTENANCE_BUS2_MIXED_RAW_V1','scope':scope,'selected_motor_id':selected_motor_id,
            'tool':dict(refs['tool']),'arm_calibration':dict(refs['arm_calibration']),
            'trace':dict(RIGHT_PULSE_TRACE),'operation':'mixed-hold-wheel9' if selected_motor_id==9 else 'mixed-hold-wheel',
            'motor_ids':[*range(1,7),*wheel_ids()],'arm_ids':[1,2,3,4,5,6],'arm_names':list(MIXED_ARM_NAMES),
            'calibration_range_tolerances_raw':[motor_range_tolerance(None,'right',motor) for motor in range(1,7)],
            'wheel_ids':wheel_ids(),'wheel_raw_goal':[500 if motor==selected_motor_id else 0 for motor in wheel_ids()],
            'metadata':{key:value for key,value in RIGHT_PULSE_METADATA.items() if key!='Lock'},
            'allowed_raw_targets':[0,500],
            'pulse_duration_ns':20_000_000_000,'zero_window_ns':500_000_000,
            'enable_budget_ns':21_000_000_000,'child_budget_ns':MIXED_CHILD_BUDGET_NS,
            'anchor_freshness_ns':200_000_000,'return_tolerance_raw':4,
            'cycle_period_ns':50_000_000,'cycle_count':400,'wheel_sample_count':79,
            'sample_period_ns':250_000_000,'position_step_raw_max':512,
            'velocity_raw_max':1000,'zero_offsets_ns':[50_000_000,250_000_000,450_000_000],
            'zero_final_raw_unsigned':[0,50,0x8032],'typed_velocity_profile_released':False}


def s0_mixed_contract(bindings,with_cameras=False):
    return s0_hold_contract(bindings,'right',with_cameras)


def s0_position_contract(bindings,with_cameras=False):
    return s0_hold_contract(bindings,'left',with_cameras)


def s0_path_identity(path):
    path=Path(path);info=path.lstat()
    require(path.is_absolute() and str(path)==os.path.normpath(path),'相机身份路径不是规范绝对路径')
    return {'path':str(path),'resolved':str(path.resolve(strict=True)),'device':info.st_dev,'inode':info.st_ino,
            'mode':stat.S_IMODE(info.st_mode),'uid':info.st_uid,'gid':info.st_gid,
            'mtime_ns':info.st_mtime_ns,'ctime_ns':info.st_ctime_ns,
            'symlink_target':os.readlink(path) if stat.S_ISLNK(info.st_mode) else None}


def s0_python_paths(python):
    paths=[];current=Path(python);seen=set()
    while True:
        require(str(current) not in seen and len(paths)<16,'相机Python符号链接循环或过深')
        seen.add(str(current));row=s0_path_identity(current);paths.append(row)
        if row['symlink_target'] is None:
            require(stat.S_ISREG(current.lstat().st_mode) and os.access(current,os.X_OK),
                    '相机Python最终路径不是可执行普通文件')
            return paths
        target=Path(row['symlink_target'])
        current=Path(os.path.normpath(target if target.is_absolute() else current.parent/target))


def s0_camera_device_identity(device):
    path=Path(device);identity=s0_path_identity(path);info=path.stat()
    require(path.parent==Path('/dev/v4l/by-path') and identity['symlink_target'] is not None
            and stat.S_ISCHR(info.st_mode) and Path(identity['resolved']).parent==Path('/dev')
            and re.fullmatch(r'video[0-9]+',Path(identity['resolved']).name),
            '相机须使用稳定by-path并解析到当前video字符设备')
    return {**identity,'target_device':info.st_dev,'target_inode':info.st_ino,'rdev':info.st_rdev,
            'target_mode':stat.S_IMODE(info.st_mode),'target_uid':info.st_uid,'target_gid':info.st_gid}


def verify_s0_camera_identity(camera):
    require(s0_python_paths(camera['python'])==camera['python_paths'],'相机固定Python路径/venv身份变化')
    require(s0_camera_device_identity(camera['device'])==camera['device_identity'],
            '相机稳定by-path或当前video字符设备身份变化')
    return {'id':camera['id'],'python_paths':camera['python_paths'],'device_identity':camera['device_identity']}


def s0_camera_contract(document,hardware_id,bindings,references):
    sensors=document.get('sensor')
    require(isinstance(sensors,list) and sensors,'显式相机模式须有配置sensor')
    ids=set();hardware_ids={row.get('id') for row in document.get('hardware',[])}
    for sensor in sensors:
        require(isinstance(sensor,dict) and sensor.get('adapter')=='opencv_v4l2'
                and isinstance(sensor.get('id'),str) and re.fullmatch(r'[A-Za-z0-9_-]+',sensor['id'])
                and sensor['id'] not in ids and isinstance(sensor.get('hardware_id'),str)
                and sensor['hardware_id'] in hardware_ids
                and all(isinstance(sensor.get(key),str) and Path(sensor[key]).is_absolute()
                        for key in ('device','python','helper'))
                and all(type(sensor.get(key)) is int and sensor[key]>0 for key in ('width','height','fps'))
                and sensor['width']*sensor['height']*3<=16*1024*1024-4096,
                '配置sensor身份/adapter/形状非法')
        ids.add(sensor['id'])
    selected=[sensor for sensor in sensors if sensor['hardware_id']==hardware_id]
    rows=bindings.get('cameras')
    require(1<=len(selected)<=3 and isinstance(rows,list)
            and [row.get('id') for row in rows]==[sensor['id'] for sensor in selected],
            '活动hardware_id须绑定配置的1..3路sensor')
    result=[]
    for sensor,row in zip(selected,rows):
        keys=('id','device','width','height','fps','python','helper')
        require(all(row.get(key)==sensor[key] for key in keys),'相机绑定必须来自同一TOML')
        paths=s0_python_paths(sensor['python']);executable=row.get('python_executable',{})
        venv=row.get('venv_config',{});venv_path=Path(sensor['python']).parent.parent/'pyvenv.cfg'
        require(row.get('python_paths')==paths and executable.get('path')==paths[-1]['resolved']
                and venv.get('path')==str(venv_path),'相机固定Python实际解释器/venv路径未绑定')
        helper=references.get(sensor['helper'])
        for ref in (helper,executable,venv):
            require(isinstance(ref,dict) and ref.get('path') in references and references[ref['path']]==ref
                    and all(key in ref for key in MIXED_REF_IDENTITY)
                    and ref.get('symlink') is False and ref.get('nlink')==1,
                    '相机helper/Python/venv必须进入完整refs')
        require(sensor['helper'] in bindings.get('binary_paths',[])
                and executable['path'] not in bindings.get('binary_paths',[]),
                '只把配置helper纳入进程名单，禁止通用Python放行')
        camera={key:sensor[key] for key in keys}
        camera.update(helper_ref=dict(helper),python_executable=dict(executable),venv_config=dict(venv),
                      python_paths=copy.deepcopy(paths),device_identity=copy.deepcopy(row.get('device_identity')))
        verify_s0_camera_identity(camera)
        result.append(camera)
    return result


def s0_hold_contract(bindings,active_side,with_cameras=False):
    """同份TOML及既有引用绑定默认S0，不借用维护adapter内部锚点合同。"""
    require(active_side in SERIALS,'默认S0活动侧错误')
    config=BUS_CONFIG if active_side=='right' else LEFT_BUS_CONFIG
    require(config is not None,'默认S0必须先绑定活动总线配置')
    document=tomllib.loads(Path(config['path']).read_text())
    hardware=document['hardware'][0]
    robot=document.get('robot',{}).get('id')
    hardware_id=hardware.get('id')
    require(isinstance(robot,str) and robot and isinstance(hardware_id,str) and hardware_id
            and bindings.get('s0_hardware_id')==hardware_id
            and hardware.get('auto_enable') is False
            and hardware.get('feetech_bus2_mixed_raw',False) is (active_side=='right'),
            '默认S0须同robot/hardware身份且显式禁止auto_enable并匹配活动侧模式')
    require(type(with_cameras) is bool,'相机必须显式opt-in')
    require(not document.get('camera') and not document.get('cameras')
            and (with_cameras or not document.get('sensor')),'本轮默认S0不得隐式启动相机或helper')
    references={ref['path']:ref for ref in bindings.get('refs',[])}
    if active_side=='left' or with_cameras:
        ref=references.get(config['path'])
        require(isinstance(ref,dict) and all(ref.get(key)==config[key] for key in ('path','bytes','sha256'))
                and all(key in ref for key in MIXED_REF_IDENTITY)
                and ref.get('symlink') is False and ref.get('nlink')==1,
                '左侧默认S0实际配置必须进入完整refs')
        config={**config,**ref}
    binaries={}
    for name in ('s0_daemon','supervised_demo'):
        ref=bindings.get(name)
        require(isinstance(ref,dict) and ref.get('path') in references and references[ref['path']]==ref
                and ref['path'] in bindings.get('binary_paths',[]) and ref.get('elf_machine')==183
                and all(key in ref for key in MIXED_REF_IDENTITY)
                and ref.get('symlink') is False and ref.get('nlink')==1,
                '默认S0与sender必须绑定完整ELF引用及进程名单')
        binaries[name]=dict(ref)
    require(binaries['s0_daemon']['path']!=binaries['supervised_demo']['path'],'S0与sender不得混用产物')
    paths=hardware.get('calibration_paths')
    require(type(paths) is list and paths and len(set(paths))==len(paths)
            and all(isinstance(path,str) and Path(path).is_absolute() and path in references for path in paths),
            '默认S0校准必须沿TOML实际路径进入原refs')
    protection=bindings.get('protection',{}).get(active_side,{})
    names=('rx_trust.lock','estop_latch.lock','rx_trust_marker','rx_trust_marker.bak','estop_latch','estop_latch.bak')
    require(set(protection)==set(names),'默认S0需要活动侧既有六项保护文件绑定')
    trust=Path(protection['rx_trust.lock']['path']).parent
    latch=Path(protection['estop_latch']['path'])
    require(all(Path(protection[name]['path'])==trust/name for name in names if name.startswith('rx_'))
            and latch.name=='estop_latch' and Path(protection['estop_latch.bak']['path'])==Path(str(latch)+'.bak')
            and Path(protection['estop_latch.lock']['path'])==latch.parent/'estop_latch.lock',
            '默认S0必须使用原信任目录和latch路径，禁止空目录替代')
    contract={'schema':'MAINTENANCE_S0_MIXED_V1' if active_side=='right' else 'MAINTENANCE_S0_POSITION_HOLD_V1',
            'scope':s0_scope(active_side,with_cameras),
            'robot_id':robot,'hardware_id':hardware_id,**binaries,
            'config':dict(config),'calibrations':[dict(references[path]) for path in paths],
            'rx_trust_dir':str(trust),'latch_path':str(latch),'motor_ids':list(hardware['motor_ids']),
            'child_budget_ns':MIXED_CHILD_BUDGET_NS,'daemon_ready_budget_ns':8_000_000_000,
            'daemon_exit_budget_ns':8_000_000_000,'final_latch_budget_ns':1_000_000_000}
    if with_cameras:
        contract['cameras']=s0_camera_contract(document,hardware_id,bindings,references)
    if active_side=='right':
        contract.update(selected_motor_id=WHEEL_PULSE_TARGETS['front'][1],
            wheel_ids=[motor for name,motor in zip(hardware['joints'],hardware['motor_ids'])
                       if name.startswith('base_') and name.endswith('_wheel')])
    else:
        contract.update(joint_names=list(hardware['joints']),hold_duration_ns=20_000_000_000,
                        cycle_period_ns=50_000_000,cycle_count=400,anchor_freshness_ns=200_000_000,
                        return_tolerance_raw=4,enable_budget_ns=300_000_000,
                        enabled_budget_ns=21_000_000_000,disable_budget_ns=200_000_000)
    return contract


def validate_s0_mixed_plan(plan):
    with_cameras='cameras' in plan.get('s0_mixed_hold',{})
    require(plan.get('s0_mixed_hold')==s0_mixed_contract(plan['bindings'],with_cameras)
            and plan.get('scope')==s0_scope('right',with_cameras) and plan.get('position_targets')==[]
            and not any(key in plan for key in ('right_wheel_pulse','right_wheel_feedback_only',
                'front_wheel_feedback_only','left_wheel_feedback_only','bus2_mixed_raw_hold',
                'bus2_arm_range_only','right_gripper_release','s0_position_hold')),
            '默认S0模式不得混用维护动作或修改固定合同')


def validate_s0_position_plan(plan):
    with_cameras='cameras' in plan.get('s0_position_hold',{})
    require(plan.get('s0_position_hold')==s0_position_contract(plan['bindings'],with_cameras)
            and plan.get('scope')==s0_scope('left',with_cameras) and plan.get('position_targets')==[]
            and not any(key in plan for key in ('right_wheel_pulse','right_wheel_feedback_only',
                'front_wheel_feedback_only','left_wheel_feedback_only','bus2_mixed_raw_hold',
                'bus2_arm_range_only','right_gripper_release','s0_mixed_hold'))
            and plan.get('base_targets')==[{'side':side,'motor':motor,'name':name,'decision':'DEFERRED',
                'reason_code':'BASE_FIRMWARE_CONTRACT_UNVERIFIED'} for side,motor,name in BASE_TARGETS],
            '左侧默认S0不得混用其他动作、解除轮子DEFERRED或修改固定合同')


def s0_plan_entry(plan):
    require(sum(key in plan for key in ('s0_mixed_hold','s0_position_hold'))==1,'默认S0须唯一活动侧')
    key='s0_position_hold' if 's0_position_hold' in plan else 's0_mixed_hold'
    return key,'left' if key=='s0_position_hold' else 'right'


def verify_s0_mixed_refs(plan):
    key,active_side=s0_plan_entry(plan)
    (validate_s0_mixed_plan if active_side=='right' else validate_s0_position_plan)(plan)
    contract=plan[key]
    for ref in s0_contract_refs(contract,active_side):
        verify_ref(ref,executable=ref in (contract['s0_daemon'],contract['supervised_demo']))
        verify_s0_ref_metadata(ref)
    verify_bus_config(plan)
    for camera in contract.get('cameras',[]):verify_s0_camera_identity(camera)


def s0_contract_refs(contract,active_side):
    refs=[contract['s0_daemon'],contract['supervised_demo'],*contract['calibrations'],
          *([contract['config']] if active_side=='left' or 'cameras' in contract else [])]
    for camera in contract.get('cameras',[]):
        refs.extend([camera['helper_ref'],camera['python_executable'],camera['venv_config']])
    return list({ref['path']:ref for ref in refs}.values())


def verify_s0_ref_metadata(ref):
    info=Path(ref['path']).lstat()
    actual={'uid':info.st_uid,'gid':info.st_gid,'mode':stat.S_IMODE(info.st_mode),
            'device':info.st_dev,'inode':info.st_ino,'mtime_ns':info.st_mtime_ns,
            'ctime_ns':info.st_ctime_ns,'nlink':info.st_nlink,'symlink':not stat.S_ISREG(info.st_mode)}
    require(all(ref.get(key)==value for key,value in actual.items()),'默认S0产物/校准元数据变化')


def verify_s0_protection(bindings,phase,active_side='right'):
    """非活动侧不变；活动侧锁保留inode，持久值只允许本轮合法替换。"""
    require(active_side in SERIALS,'默认S0活动侧错误')
    expected_marker=b'0\n' if phase=='authorized' else b'1\n'
    evidence=[]
    for side,files in bindings['protection'].items():
        for name,ref in files.items():
            if side!=active_side:
                evidence.append(verify_protection_file(side,name,ref));continue
            path=Path(ref['path']);info=path.lstat()
            mode=stat.S_IMODE(info.st_mode)
            latch_writer_mode=(name in ('estop_latch','estop_latch.bak') and phase in ('running','final')
                               and info.st_dev==ref['device'] and info.st_ino!=ref['inode'] and mode==0o640)
            require(stat.S_ISREG(info.st_mode) and info.st_nlink==1
                    and all(actual==ref[key] for key,actual in
                            (('uid',info.st_uid),('gid',info.st_gid)))
                    and (mode==ref['mode'] or latch_writer_mode),
                    f'{side}侧保护文件身份/权限变化：{name} phase={phase} '
                    f'expected dev/ino/mode/uid/gid/nlink={ref["device"]}/{ref["inode"]}/{ref["mode"]:04o}/'
                    f'{ref["uid"]}/{ref["gid"]}/1 actual={info.st_dev}/{info.st_ino}/{mode:04o}/'
                    f'{info.st_uid}/{info.st_gid}/{info.st_nlink}')
            raw=path.read_bytes()
            if name.endswith('.lock'):
                require(info.st_ino==ref['inode'] and info.st_dev==ref['device'],'活动侧锁inode变化')
            elif name.startswith('rx_trust_marker'):
                require(raw==expected_marker,'活动侧marker阶段值错误')
            else:
                require(raw in ((b'0\n',b'1\n') if phase=='running' else (b'1\n',)),
                        '活动侧latch阶段值错误')
            evidence.append({'path':str(path),'device':info.st_dev,'inode':info.st_ino,'mode':mode,
                             'sha256':hashlib.sha256(raw).hexdigest(),
                             'bytes':len(raw),'phase':phase})
    return evidence


def s0_camera_snapshot_refs(result,plan,contract):
    """图像必须覆盖本次保持窗口；原BGR独立留SHA，不用结束快照代替连续帧。"""
    cameras=contract.get('cameras')
    if cameras is None:
        require('camera_observations' not in result,'无相机计划不得接受sensor回执')
        return []
    observations=result.get('camera_observations',{})
    start=result['nonzero_start_ns'] if 'nonzero_start_ns' in result else result['hold_start_ns']
    end=result['nonzero_end_ns'] if 'nonzero_end_ns' in result else result['hold_end_ns']
    require(isinstance(observations,dict) and observations.get('schema')=='S0_CAMERA_OBSERVATIONS_V1'
            and observations.get('window_started_ns')==start
            and observations.get('window_finished_ns')==end and end-start==20_000_000_000,
            '相机观测必须绑定同一完整20秒控制窗口')
    rows=observations.get('cameras')
    require(isinstance(rows,list) and [row.get('id') for row in rows]==[row['id'] for row in cameras],
            '相机逐路集合/顺序与配置不符')
    references=[]
    for expected,row in zip(cameras,rows):
        width=expected['width'];height=expected['height'];size=width*height*3
        require(row.get('device')==expected['device'] and row.get('width')==width
                and row.get('height')==height and row.get('step')==width*3
                and row.get('fps_requested')==expected['fps']
                and row.get('encoding')=='bgr8' and row.get('color_order')=='BGR'
                and all(type(row.get(key)) is int for key in ('width','height','step','fps_requested')),
                '相机身份/帧形状与配置不符')
        frames=row.get('frames')
        require(isinstance(frames,list) and len(frames)>=2,'相机缺连续帧观测')
        previous_stamp=None;previous_observed=None
        for frame in frames:
            stamp=frame.get('stamp_ns');observed=frame.get('observed_ns')
            require(type(stamp) is int and type(observed) is int and stamp>=0
                    and 0<=observed-stamp<=1_000_000_000
                    and start<=observed<=end,'相机帧时刻/新鲜度不符')
            if previous_stamp is not None:
                require(0<stamp-previous_stamp<=1_000_000_000
                        and 0<observed-previous_observed<=1_000_000_000,
                        '相机帧不严格推进或失流超过1秒')
            previous_stamp=stamp;previous_observed=observed
        require(frames[0]['observed_ns']<=start+1_000_000_000
                and frames[0]['stamp_ns']<=start+1_000_000_000
                and frames[-1]['stamp_ns']>=end-1_000_000_000
                and frames[-1]['observed_ns']>=end-1_000_000_000,
                '相机连续帧未覆盖完整20秒窗口首尾')
        snapshot=row.get('snapshot',{})
        path=Path(plan['output'])/f"camera-{expected['id']}.bgr"
        require(snapshot.get('path')==str(path) and snapshot.get('bytes')==size
                and snapshot.get('stamp_ns')==frames[-1]['stamp_ns']
                and type(snapshot.get('saved_at_ns')) is int and snapshot['saved_at_ns']>=result['finished_ns']
                and snapshot.get('encoding')=='bgr8' and snapshot.get('color_order')=='BGR',
                '相机末帧原件身份/编码不符')
        info=path.lstat()
        require(stat.S_ISREG(info.st_mode) and info.st_nlink==1 and info.st_uid==os.getuid()
                and info.st_size==size,'相机原件不是本次独立完整BGR文件')
        raw=path.read_bytes()
        require(len(raw)==size,'相机原件大小变化')
        references.append({'id':expected['id'],'path':str(path),'bytes':len(raw),
            'sha256':hashlib.sha256(raw).hexdigest(),'uid':info.st_uid,'gid':info.st_gid,
            'mode':stat.S_IMODE(info.st_mode),'device':info.st_dev,'inode':info.st_ino,
            'mtime_ns':info.st_mtime_ns,'ctime_ns':info.st_ctime_ns,'nlink':info.st_nlink,'symlink':False})
    return references


def verify_s0_camera_report(report):
    plan=report['plan'];key,_=s0_plan_entry(plan);contract=plan[key]
    if 'cameras' not in contract:return None
    expected_ids=[camera['id'] for camera in contract['cameras']]
    helpers=report.get('s0_camera_helpers',[]);exits=report.get('s0_camera_helper_exit_checks',[])
    require(report.get('s0_camera_handoff_closed') is True
            and sorted(row.get('id') for row in helpers)==sorted(expected_ids)
            and len(helpers)==len(expected_ids) and len(exits)==len(expected_ids),
            '相机helper绑定/退出证据未闭合')
    release=report.get('s0_camera_release_checks',[])
    require(len(release)==2*len(expected_ids)
            and {(row.get('id'),row.get('path'),row.get('elevated')) for row in release}
                =={(camera['id'],camera['device'],elevated) for camera in contract['cameras'] for elevated in (False,True)}
            and all(row.get('stdout_empty') is True and row.get('exit_code')==1 for row in release),
            '相机设备ordinary/sudo释放证据不完整')
    daemon=report['s0_daemon']['daemon_identity']
    for camera in contract['cameras']:
        helper=next(row for row in helpers if row['id']==camera['id']);identity=helper['identity']
        exit_row=[row for row in exits if row.get('id')==camera['id']]
        require(helper.get('parent_pid')==daemon['pid'] and identity.get('argv')==s0_camera_argv(camera)
                and identity.get('exe')==camera['python_executable']['path']
                and identity.get('uids')==daemon['uids'] and identity.get('boot_id')==daemon['boot_id']
                and identity.get('pgid')==daemon['pgid']
                and len(exit_row)==1 and exit_row[0].get('identity')==identity
                and exit_row[0].get('terminated') is True
                and exit_row[0].get('observation',{}).get('status')=='vanished',
                '相机helper最终身份/原PID终结不符')
    checks=report.get('s0_camera_helper_checks',[])
    require(any(row.get('phase')=='runtime'
                and sorted(helper['id'] for helper in row.get('helpers',[]))==sorted(expected_ids)
                for row in checks),'相机缺控制期间完整helper核验')
    actual=s0_camera_snapshot_refs(report['s0_sender_result'],plan,contract)
    require(report.get('s0_camera_snapshots')==actual,'相机原BGR SHA/文件身份变化或首次引用缺失')
    return actual


def parse_s0_mixed_result(text,plan,expiry):
    result=strict_json(text);contract=plan['s0_mixed_hold']
    require(isinstance(result,dict) and result.get('schema')=='SUPERVISED_S0_MIXED_V1'
            and result.get('config')==contract['config']['path']
            and result.get('hardware_id')==contract['hardware_id']
            and result.get('selected_motor_id')==contract['selected_motor_id']
            and result.get('wheel_ids')==contract['wheel_ids']
            and result.get('go_expires_at_unix_ns')==expiry
            and type(result.get('layout_id')) is int and 0<=result['layout_id']<2**64,
            '默认S0 sender回执身份不符')
    require(result.get('result')=='SOFTWARE_PATH_PASS' and not result.get('error')
            and result.get('cleanup_errors')==[] and result.get('manual_power_cut_required') is False,
            '默认S0 sender失败：'+str(result.get('error',result.get('result'))))
    require(all(result.get(key) is True for key in ('motion_position_verified','disable_retired',
                'torque_off_confirmed','diagnostics_dropped_zero_verified','final_estop_published'))
            and result.get('motion_velocity_raw')=='PASS'
            and result.get('physical_motion')=='NOT_OBSERVED' and result.get('stop51')=='NOT_RUN'
            and result.get('full_hardware_acceptance') is False and result.get('deployable') is False,
            '默认S0 sender停止/观测证据不完整或越过物理验收边界')
    anchor=result.get('anchor',{});raw=anchor.get('raw');target=anchor.get('target')
    require(isinstance(raw,list) and len(raw)==9 and all(type(v) is int and 0<=v<=4095 for v in raw)
            and isinstance(target,list) and len(target)==9
            and all(type(v) in (int,float) and abs(v)<float('inf') for v in target),
            '默认S0发送端固定锚点不完整')
    timing=('enable_attempt_ns','nonzero_start_ns','nonzero_end_ns','zero_attempt_ns','zero_end_ns',
            'zero_publish_return_ns','final_estop_attempt_ns','final_estop_publish_return_ns','finished_ns')
    require(all(type(result.get(key)) is int and result[key]>=0 for key in timing)
            and type(anchor.get('stamp_ns')) is int,'默认S0时刻缺失')
    enabled=result['enable_attempt_ns'];start=result['nonzero_start_ns'];zero=result['zero_attempt_ns']
    require(start>=enabled and result['nonzero_end_ns']==start+20_000_000_000
            and start+20_000_000_000<=zero<=result['zero_publish_return_ns']<result['zero_end_ns']
            and result['zero_end_ns']==zero+500_000_000
            and result['zero_end_ns']+200_000_000<=enabled+21_000_000_000
            and 0<=start-anchor['stamp_ns']<200_000_000
            and result['zero_end_ns']<=result['final_estop_attempt_ns']
            <=result['final_estop_publish_return_ns']<=result['finished_ns'],
            '默认S0固定20秒/500毫秒/21秒或新鲜锚点期限不符')
    cycles=result.get('cycles');motion=result.get('motion_samples');zero_samples=result.get('zero_samples')
    require(isinstance(cycles,list) and len(cycles)==400 and isinstance(motion,list) and len(motion)==79
            and isinstance(zero_samples,list) and len(zero_samples)==3,'默认S0周期/观测组数不完整')
    for index,row in enumerate(cycles):
        scheduled=start+index*50_000_000
        deadline=min(scheduled+50_000_000,anchor['stamp_ns']+200_000_000) if index==0 else scheduled+50_000_000
        source=min(row['publish_attempt_ns']+200_000_000,enabled+21_000_000_000)
        if index==0:source=min(source,anchor['stamp_ns']+200_000_000)
        require(row.get('cycle')==index and row.get('scheduled_ns')==scheduled
                and row.get('local_deadline_ns')==deadline
                and scheduled<=row['publish_attempt_ns']<=row['publish_return_ns']<deadline
                and row['publish_attempt_ns']<row['source_deadline_ns']==source,
                '默认S0绝对拍次/命令期限错误')
    previous=None;travel=0;positive=False
    for index,row in enumerate(motion,1):
        require(row.get('window')==index and start+index*250_000_000<=row['started_ns']
                <start+(index+1)*250_000_000
                and row['started_ns']<=row['finished_ns']<=start+(index+1)*250_000_000,'默认S0运动采样窗错误')
        wheels=row.get('wheels',[])
        require([w.get('id') for w in wheels]==contract['wheel_ids'],'默认S0运动采样轮身份错误')
        for wheel in wheels:
            position=wheel.get('position');velocity=wheel.get('velocity');motor=wheel['id']
            require(type(position) is int and 0<=position<=4095 and type(velocity) is int,'默认S0运动原字非法')
            if motor==contract['selected_motor_id']:
                require(0<=velocity<=1000,'默认S0前轮速度原字越界')
                positive|=velocity>0
                if previous is not None:
                    step=(position-previous)%4096
                    require(step<=512,'默认S0前轮位置步长越界');travel+=step
                previous=position
            else:
                require(velocity in (0,50,0x8032)
                        and abs(position-raw[contract['motor_ids'].index(motor)])<=4,
                        '默认S0未选轮越界')
    require(positive and travel>0 and result.get('position_travel_raw')==travel,'默认S0缺少前轮正向运动原字证据')
    for index,(row,(low,high)) in enumerate(zip(zero_samples,((50,250),(250,450),(450,500)))):
        require(zero+low*1_000_000<=row['started_ns']<=row['finished_ns']<=zero+high*1_000_000
                and [w.get('id') for w in row.get('wheels',[])]==contract['wheel_ids'],
                '默认S0零速观测窗口/轮身份错误')
        require(all(type(w.get('torque')) is int and w['torque']==1 and w.get('goal')==0
                    and type(w.get('velocity')) is int
                    and (w['velocity'] in (0,50,0x8032) if index==2 else
                         0<=w['velocity']<=1000 or w['velocity']==0x8032)
                    for w in row['wheels']),'默认S0零速原字不满足原合同')
    s0_camera_snapshot_refs(result,plan,contract)
    return result


def parse_s0_position_result(text,plan,expiry):
    result=strict_json(text);contract=plan['s0_position_hold']
    require(isinstance(result,dict) and result.get('schema')=='SUPERVISED_S0_POSITION_HOLD_V1'
            and result.get('config')==contract['config']['path']
            and result.get('hardware_id')==contract['hardware_id']
            and isinstance(result.get('motor_ids'),list) and all(type(motor) is int for motor in result['motor_ids'])
            and result.get('motor_ids')==contract['motor_ids']
            and result.get('go_expires_at_unix_ns')==expiry
            and type(result.get('layout_id')) is int and 0<=result['layout_id']<2**64,
            '左侧默认S0 sender回执身份不符')
    require(result.get('result')=='SOFTWARE_PATH_PASS' and not result.get('error')
            and result.get('cleanup_errors')==[] and result.get('manual_power_cut_required') is False
            and all(result.get(key) is True for key in ('hold_position_verified','disable_retired',
                    'torque_off_confirmed','final_estop_published'))
            and result.get('physical_motion')=='NOT_OBSERVED' and result.get('stop51')=='NOT_RUN'
            and result.get('full_hardware_acceptance') is False and result.get('deployable') is False,
            '左侧默认S0 sender失败或停止/观测证据不完整')
    anchor=result.get('anchor',{});raw=anchor.get('raw');target=anchor.get('target')
    require(isinstance(raw,list) and len(raw)==8 and all(type(v) is int and 0<=v<=4095 for v in raw)
            and isinstance(target,list) and len(target)==8
            and all(type(v) in (int,float) and abs(v)<float('inf') for v in target)
            and type(anchor.get('stamp_ns')) is int and anchor['stamp_ns']>=0,
            '左侧默认S0固定八关节锚点不完整')
    timing=('enable_attempt_ns','enable_deadline_ns','enable_transition_deadline_ns','enable_confirmed_ns',
            'hold_start_ns','hold_end_ns','disable_attempt_ns',
            'torque_off_confirmed_ns','final_estop_attempt_ns','final_estop_publish_return_ns','finished_ns')
    require(all(type(result.get(key)) is int and result[key]>=0 for key in timing),'左侧默认S0时刻缺失')
    enabled=result['enable_attempt_ns'];start=result['hold_start_ns'];end=result['hold_end_ns']
    require(result['enable_deadline_ns']==enabled+21_000_000_000
            and result['enable_transition_deadline_ns']==enabled+300_000_000
            and enabled<=result['enable_confirmed_ns']<=result['enable_transition_deadline_ns']
            and result['enable_confirmed_ns']<=start<=result['enable_transition_deadline_ns']
            and end==start+20_000_000_000
            and 0<=start-anchor['stamp_ns']<200_000_000
            and end<=result['disable_attempt_ns']<=result['torque_off_confirmed_ns']
            <=result['disable_attempt_ns']+200_000_000
            and result['torque_off_confirmed_ns']<=result['final_estop_attempt_ns']
            <=result['final_estop_publish_return_ns']<=result['finished_ns']<=enabled+21_000_000_000,
            '左侧默认S0启用300毫秒/保持20秒/停止200毫秒/总21秒或锚点期限不符')
    cycles=result.get('cycles');samples=result.get('hold_samples')
    require(isinstance(cycles,list) and len(cycles)==400 and isinstance(samples,list) and len(samples)==400,
            '左侧默认S0周期/逐拍反馈组数不完整')
    previous=anchor['stamp_ns']
    for index,(row,sample) in enumerate(zip(cycles,samples)):
        scheduled=start+index*50_000_000
        deadline=min(scheduled+50_000_000,anchor['stamp_ns']+200_000_000) if index==0 else scheduled+50_000_000
        require(isinstance(row,dict) and isinstance(sample,dict)
                and all(type(row.get(key)) is int and row[key]>=0 for key in
                        ('cycle','scheduled_ns','local_deadline_ns','publish_attempt_ns','publish_return_ns','source_deadline_ns')),
                '左侧默认S0周期时刻字段错误')
        source=min(row['publish_attempt_ns']+200_000_000,enabled+21_000_000_000)
        if index==0:source=min(source,anchor['stamp_ns']+200_000_000)
        require(row['cycle']==index and row['scheduled_ns']==scheduled and row['local_deadline_ns']==deadline
                and scheduled<=row['publish_attempt_ns']<=row['publish_return_ns']<deadline
                and row['publish_attempt_ns']<row['source_deadline_ns']==source,
                '左侧默认S0绝对拍次/命令期限错误')
        observed=sample.get('observed_ns');stamp=sample.get('stamp_ns');positions=sample.get('raw')
        require(type(sample.get('cycle')) is int and sample['cycle']==index
                and type(stamp) is int and type(observed) is int and previous<stamp<=observed
                and observed-stamp<200_000_000
                and row['publish_return_ns']<=observed<deadline
                and isinstance(positions,list) and len(positions)==8
                and all(type(value) is int and 0<=value<=4095 and abs(value-anchor_raw)<=4
                        for value,anchor_raw in zip(positions,raw)),
                '左侧默认S0逐拍反馈未推进、不新鲜、越窗或相对锚点超过4raw')
        previous=stamp
    s0_camera_snapshot_refs(result,plan,contract)
    return result


def s0_trust_argv(contract,operation):
    return [contract['s0_daemon']['path'],'rx-trust',operation,'--robot-id',contract['robot_id'],
            '--rx-trust-dir',contract['rx_trust_dir'],'--latch-path',contract['latch_path']]


def s0_same_process(actual,expected):
    return all(actual.get(key)==expected.get(key) for key in
               ('pid','start_ticks','boot_id','pgid','uids','exe','argv'))


def s0_process_identity(io,proc_helper,pid):
    """普通读取遇EACCES时仍沿原有限提权探针，不忽略未知身份。"""
    row=proc_helper.read_process(pid)
    if row['status']=='observed':return row['identity']
    require(row['status']=='permission','默认S0进程身份未知：'+repr(row))
    snapshot=proc_helper.process_snapshot(row.get('before',{}).get('pgid',pid),io.output,[],
                                         probe_runner=proc_helper.privileged_process_probe)
    io.process_snapshots.append(snapshot)
    identities=[identity for identity in snapshot['related'] if identity.get('pid')==pid]
    require(not snapshot['unreadable'] and len(identities)==1,'默认S0提权身份核验失败')
    return identities[0]


def s0_camera_argv(camera):
    return [camera['python'],'-I','-S','-B','-u',camera['helper'],'--device',camera['device'],
            '--width',str(camera['width']),'--height',str(camera['height']),'--fps',str(camera['fps'])]


def s0_child_pids(pid,proc=Path('/proc')):
    """按PPID枚举直接子进程，兼容未启用task/children接口的Orin内核。"""
    children=set()
    for path in proc.iterdir():
        if not path.name.isdigit():continue
        child=int(path.name)
        try:parent=s0_parent_pid(child,proc=proc)
        except FileNotFoundError:continue
        if parent==pid:children.add(child)
    return sorted(children)


def s0_parent_pid(pid,proc=Path('/proc')):
    raw=(proc/str(pid)/'stat').read_text();opening=raw.find('(');closing=raw.rfind(')')
    require(opening>0 and closing>opening and int(raw[:opening].strip())==pid,'helper stat身份非法')
    parent=int(raw[closing+2:].split()[1])
    require(parent>=0,'helper父PID非法')
    return parent


def inspect_s0_camera_helpers(io,proc_helper,contract,daemon_identity,*,complete,cleanup=False):
    """只给每路已配置相机绑定一个原PID；已绑定后不允许替换或丢失。"""
    cameras=contract.get('cameras',[])
    if not cameras:return []
    previous={row['id']:row for row in io.s0_camera_helpers}
    children=s0_child_pids(daemon_identity['pid']);found={}
    for pid in children:
        identity=s0_process_identity(io,proc_helper,pid)
        matches=[camera for camera in cameras if identity.get('argv')==s0_camera_argv(camera)]
        require(len(matches)==1,'S0存在未知或重复相机helper argv')
        camera=matches[0];camera_id=camera['id']
        parent=s0_parent_pid(pid)
        require(camera_id not in found and parent==daemon_identity['pid']
                and identity.get('uids')==daemon_identity['uids']
                and identity.get('boot_id')==daemon_identity['boot_id']
                and identity.get('pgid')==daemon_identity['pgid']
                and identity.get('exe')==camera['python_executable']['path']
                and identity.get('zombie',False) is False,
                '相机helper父进程/UID/boot/exe身份不符')
        verified=s0_process_identity(io,proc_helper,pid)
        require(s0_same_process(verified,identity),'相机helper核验期间身份变化')
        row={'id':camera_id,'parent_pid':parent,'identity':identity}
        if camera_id in previous:
            require(parent==previous[camera_id]['parent_pid']
                    and s0_same_process(identity,previous[camera_id]['identity']),'相机helper原PID被替换')
            row=previous[camera_id]
        else:
            io.s0_camera_helpers.append(row)
            io.s0_identities.append(identity)
        found[camera_id]=row
    missing=set(previous)-set(found)
    if cleanup:
        for camera_id in sorted(missing):
            observed=proc_helper.read_process(previous[camera_id]['identity']['pid'])
            require(observed.get('status')=='vanished','已知相机helper脱离S0且原PID未证实终结')
    else:
        require(not missing,'已绑定相机helper提前退出或脱离S0')
    require(not complete or set(found)=={camera['id'] for camera in cameras},'配置相机helper未完整启动')
    io.s0_camera_helper_checks.append({'phase':'cleanup' if cleanup else 'runtime' if complete else 'starting',
        'daemon_pid':daemon_identity['pid'],'children':children,'helpers':copy.deepcopy(list(found.values()))})
    return list(found.values())


def verify_s0_camera_helpers_gone(io,proc_helper):
    rows=[];failures=[]
    for helper in io.s0_camera_helpers:
        identity=helper['identity']
        observed=proc_helper.read_process(identity['pid'])
        row={'id':helper['id'],'identity':copy.deepcopy(identity),'observation':observed}
        if observed.get('status')=='permission':
            try:row['remaining_identity']=s0_process_identity(io,proc_helper,identity['pid'])
            except BaseException as error:row['error']=repr(error)
        row['terminated']=observed.get('status')=='vanished'
        rows.append(row)
        if not row['terminated']:failures.append(helper['id'])
    io.s0_camera_helper_exit_checks=rows
    require(not failures,'原相机helper PID未证实终结：'+repr(failures))
    return rows


def s0_ready_chunk(pending,chunk):
    """只认原daemon完整就绪行；EOF半行留到下一次读取。"""
    lines=(pending+chunk).split(b'\n');pending=lines.pop()
    require(len(pending)<=65536 and all(len(line)<=65536 for line in lines),'默认S0启动日志行过长')
    ready=any(re.fullmatch(rb'boot_ready spawned_ns=[0-9]+ first_exit_ns=[0-9]+ ready_ns=[0-9]+',line)
              for line in lines)
    return pending,ready


def verify_s0_daemon_locks(bindings,identity,active_side='right'):
    """锁内容与内核FLOCK所有者同时绑定，busy本身不等于本轮daemon持有。"""
    require(active_side in SERIALS,'默认S0活动侧错误')
    rows=[row.split() for row in Path('/proc/locks').read_text().splitlines()]
    for name in ('rx_trust.lock','estop_latch.lock'):
        ref=bindings['protection'][active_side][name]
        key=f'{os.major(ref["device"]):02x}:{os.minor(ref["device"]):02x}:{ref["inode"]}'
        require(any(len(row)>=6 and row[1:4]==['FLOCK','ADVISORY','WRITE']
                    and row[4]==str(identity['pid']) and row[5]==key for row in rows)
                and Path(ref['path']).read_bytes()==f'{identity["pid"]}\n'.encode(),
                '默认S0未真实独占既有活动侧双锁')


def run_s0_mixed_hold(engine,go,proc_helper,locks,report):
    """仅交接活动侧；真实退出并取回双锁之前绝不恢复维护UART。"""
    io=engine.io;plan=engine.plan;bindings=plan['bindings'];key,active_side=s0_plan_entry(plan);contract=plan[key]
    io.s0_contract=contract;io.s0_post_ready=False
    io.s0_environment=proc_helper.clean_environment(os.environ)
    io.s0_sender_started=False
    io.s0_identities=[];io.s0_daemon_record={'argv':None,'pid':None,'reaped':False,'signals_sent':[]}
    io.s0_camera_helpers=[];io.s0_camera_helper_checks=[];io.s0_camera_helper_exit_checks=[]
    record=io.s0_daemon_record;report['s0_daemon']=record
    report['s0_cleanup_errors']=[]
    daemon_log=io.output/'s0.jsonl';sender_output=io.output/'s0-sender.json'
    active_locks=[locks[(active_side,name)] for name in ('rx_trust.lock','estop_latch.lock')]
    released=set();authorized=False;spawned=False;log=None;failure=None;pinned_latches=[]

    def collect_sender_result():
        info=sender_output.lstat()
        require(stat.S_ISREG(info.st_mode) and info.st_nlink==1,'sender结果不是本次独立普通文件')
        raw=sender_output.read_bytes()
        report['s0_sender_result_ref']={'path':str(sender_output),'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest()}
        report['s0_sender_result']=strict_json(raw.decode())
        return raw

    def bind(process,argv,role):
        identity=s0_process_identity(io,proc_helper,process.pid)
        require(identity.get('pid')==process.pid and identity.get('pgid')==process.pid
                and identity.get('uids')==[os.getuid()]*4
                and identity.get('boot_id')==bindings['host']['boot_id']
                and identity.get('exe')==str(argv[0]) and identity.get('argv')==list(argv),
                '默认S0/sender启动身份不符')
        io.s0_identities.append(identity)
        record[role+'_identity']=identity
        return identity

    io.s0_bind_process=bind

    def daemon_alive():
        require(io.s0_daemon is not None and io.s0_daemon.poll() is None,'默认S0已提前退出')
        identity=s0_process_identity(io,proc_helper,io.s0_daemon.pid)
        require(s0_same_process(identity,record['daemon_identity']),'默认S0身份被替换或不可读')
        return identity

    def runtime_gate():
        io.check();validate_go(go,plan,time.time_ns())
        for ref in s0_contract_refs(contract,active_side):
            verify_s0_ref_metadata(ref)
        for camera in contract.get('cameras',[]):verify_s0_camera_identity(camera)
        identity=daemon_alive()
        inspect_s0_camera_helpers(io,proc_helper,contract,identity,complete=True)
        # poll只回收本轮自有sender，消解其自然退出与/proc读取之间的竞态。
        if io.child is not None:io.child.poll()
        inspect_processes(io,bindings,proc_helper,owned=io.s0_identities)
        verify_s0_protection(bindings,'running',active_side)
        verify_s0_daemon_locks(bindings,identity,active_side)

    def stop_daemon():
        process=io.s0_daemon
        if process is None:return
        if process.poll() is None:
            try:
                identity=s0_process_identity(io,proc_helper,process.pid)
                require('daemon_identity' in record and s0_same_process(identity,record['daemon_identity']),
                        '停止前默认S0身份不符')
            except BaseException as error:
                report['s0_cleanup_errors'].append('daemon_identity:'+repr(error));engine.manual_cut=True
            # 这里只操作仍由本函数持有的真实Popen；身份准入失败不能跳过必要停止。
            try:
                process.send_signal(signal.SIGTERM)
                record['signals_sent'].append({'signal':int(signal.SIGTERM),'sent_ns':io.now()})
            except BaseException as error:
                report['s0_cleanup_errors'].append('daemon_signal:'+repr(error));engine.manual_cut=True
        try:
            code=process.wait(timeout=contract['daemon_exit_budget_ns']/1e9)
            record.update(exit_code=code,reaped=True,finished_ns=io.now())
            io.s0_daemon=None
            require(code==0,'默认S0退出码非零')
        except BaseException as error:
            report['s0_cleanup_errors'].append('daemon_exit:'+repr(error))
            if io.s0_daemon is not None:engine.manual_cut=True

    try:
        verify_s0_mixed_refs(plan);engine.check_gate()
        for camera in contract.get('cameras',[]):
            for elevated in (False,True):inspect_holder(io,camera['device'],elevated)
        require(io.child is None and io.s0_daemon is None,'默认S0模式已有自有进程')
        require(go['expires_at_ns']-time.time_ns()>=contract['daemon_ready_budget_ns']+MIXED_CHILD_BUDGET_NS,
                '本次GO不足启动及完整sender预算')
        for name in ('estop_latch','estop_latch.bak'):
            ref=bindings['protection'][active_side][name]
            fd=os.open(ref['path'],os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC)
            pinned_latches.append(fd)
            info=os.fstat(fd)
            require(stat.S_ISREG(info.st_mode) and info.st_nlink==1
                    and (info.st_dev,info.st_ino,info.st_uid,info.st_gid,stat.S_IMODE(info.st_mode))
                    ==(ref['device'],ref['inode'],ref['uid'],ref['gid'],ref['mode']),
                    '持有原活动侧latch inode前身份变化：'+name)
        # 只读FD阻止原inode在本轮多次rename后被回收复用，不阻碍S0合法替换路径。
        fcntl.flock(active_locks[0].fileno(),fcntl.LOCK_UN);released.add(0)
        authorized=True
        io.command(s0_trust_argv(contract,'authorize'),s0_control=True)
        report['s0_authorized_protection']=verify_s0_protection(bindings,'authorized',active_side)
        io.check();validate_go(go,plan,time.time_ns());verify_s0_mixed_refs(plan)
        fcntl.flock(active_locks[1].fileno(),fcntl.LOCK_UN);released.add(1)
        argv=[contract['s0_daemon']['path'],'--config',contract['config']['path'],
              '--hardware-id',contract['hardware_id'],'--latch-path',contract['latch_path'],
              '--rx-trust-dir',contract['rx_trust_dir'],'--notify-mode','standalone']
        record.update(argv=argv,started_ns=io.now(),log_path=str(daemon_log))
        log=daemon_log.open('xb')
        environment=dict(io.s0_environment)
        environment.update(PATH='/usr/sbin:/usr/bin:/sbin:/bin',LC_ALL='C',ROBOT_LOG_FORMAT='json',
                           RUST_LOG='info,s0_wheel_observation=debug',NO_COLOR='1')
        io.s0_daemon=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=log,stderr=subprocess.STDOUT,
                                       start_new_session=True,env=environment)
        spawned=True;record['pid']=io.s0_daemon.pid
        record['daemon_identity']=bind(io.s0_daemon,argv,'daemon')
        ready_deadline=io.now()+contract['daemon_ready_budget_ns']
        with daemon_log.open('rb') as reader:
            pending=b''
            while True:
                io.check();validate_go(go,plan,time.time_ns());daemon_alive()
                pending,ready=s0_ready_chunk(pending,reader.read(65536))
                if ready:
                    runtime_gate();record['ready_ns']=io.now();break
                require(io.now()<ready_deadline,'默认S0启动超过固定8秒预算')
                io.sleep(.01)
        argv=[contract['supervised_demo']['path'],'--s0-mixed-hold' if active_side=='right' else '--s0-position-hold',
              '--config',contract['config']['path'],
              '--hardware-id',contract['hardware_id'],'--s0-log',str(daemon_log),'--output',str(sender_output),
              '--ack-safety-checklist','--go-expires-at-unix-ns',str(go['expires_at_ns'])]
        if 'cameras' in contract:argv.append('--with-cameras')
        io.s0_sender_argv=tuple(argv)
        def sender_gate():
            runtime_gate()
            if io.child is None:
                require(go['expires_at_ns']-time.time_ns()>=MIXED_CHILD_BUDGET_NS,'本次GO不足完整28秒sender预算')
        io.command(argv,allowed=(0,1),mixed_stop_check=sender_gate,s0_sender=True)
        require(io.child is None,'sender尚未真实回收')
        raw=collect_sender_result()
        if report['s0_sender_result'].get('manual_power_cut_required') is True:
            engine.manual_cut=True
        require(io.commands[-1]['exit_code']==0,'默认S0 sender退出码非零')
        result=(parse_s0_mixed_result if active_side=='right' else parse_s0_position_result)(raw.decode(),plan,go['expires_at_ns'])
        if 'cameras' in contract:report['s0_camera_snapshots']=s0_camera_snapshot_refs(result,plan,contract)
        targets=list(zip(range(1,7),MIXED_ARM_NAMES)) if active_side=='right' else list(zip(contract['motor_ids'],contract['joint_names']))
        for motor,name in targets:
            slot=contract['motor_ids'].index(motor)
            engine.rows.append({'side':active_side,'motor':motor,'name':name,'kind':'position_hold','status':'PASS',
                'anchor_raw':result['anchor']['raw'][slot],'sampled_ns':result['anchor']['stamp_ns'],
                'anchor_source':'sender_frozen_joint_state','cleanup_errors':[]})
        if active_side=='right':
            selected=contract['selected_motor_id']
            engine.rows.append({'side':'right','motor':selected,'name':WHEEL_PULSE_TARGETS['front'][2],
                                'kind':'velocity','status':'PASS','raw_velocity':500,'cleanup_errors':[]})
        # EStop发布不等于持久化；断扭后只读等双latch，不追加命令或延长启用预算。
        deadline=io.now()+contract['final_latch_budget_ns']
        while True:
            daemon_alive()
            latch=bindings['protection'][active_side]
            if all(Path(latch[name]['path']).read_bytes()==b'1\n' for name in ('estop_latch','estop_latch.bak')):
                report['s0_final_latch_observed_ns']=io.now();break
            require(io.now()<deadline,'sender EStop后固定1秒内未见双latch1')
            io.sleep(.01,cleanup=True)
    except BaseException as error:
        failure=error
        if io.s0_sender_started:
            engine.manual_cut=True
        raise
    finally:
        if io.s0_sender_started and io.child is None and 's0_sender_result' not in report:
            # command停止门可能先抛错；只读保存已回收sender的原件，不重新判成功。
            try:collect_sender_result()
            except BaseException as error:
                report['s0_sender_result_error']=repr(error)
                io.evidence_errors.append('s0_sender_result:'+repr(error))
        camera_handoff_ok=True
        if spawned and 'cameras' in contract and io.s0_daemon is not None and io.s0_daemon.poll() is None:
            # 启动/就绪前失败也先留下直接helper身份；未知child不能在停止后被忽略。
            try:
                identity=s0_process_identity(io,proc_helper,io.s0_daemon.pid)
                inspect_s0_camera_helpers(io,proc_helper,contract,identity,complete=False,cleanup=True)
            except BaseException as error:
                camera_handoff_ok=False
                report['s0_cleanup_errors'].append('camera_pre_stop:'+repr(error));engine.manual_cut=True
        try:stop_daemon()
        except BaseException as error:
            report['s0_cleanup_errors'].append('daemon_cleanup:'+repr(error));engine.manual_cut=True
        # Popen之前失败才撤销本次未消费授权；不重启、不重发sender、不写marker绕过。
        if authorized and not spawned:
            try:io.command(s0_trust_argv(contract,'revoke'),cleanup=True,s0_control=True)
            except BaseException as error:report['s0_cleanup_errors'].append('revoke:'+repr(error))
        if log is not None:
            try:log.close()
            except BaseException as error:
                io.evidence_errors.append('s0_log_close:'+repr(error))
        if spawned:
            try:
                raw=daemon_log.read_bytes()
                record.update(log_bytes=len(raw),log_sha256=hashlib.sha256(raw).hexdigest())
            except BaseException as error:io.evidence_errors.append('s0_log:'+repr(error))
        if 'cameras' in contract:
            report['s0_camera_release_checks']=[]
            try:
                require(io.s0_daemon is None,'daemon尚未真实wait，禁止相机退出判定')
                verify_s0_camera_helpers_gone(io,proc_helper)
                for camera in contract['cameras']:
                    verify_s0_camera_identity(camera)
                    for elevated in (False,True):
                        evidence=inspect_holder(io,camera['device'],elevated,cleanup=True)
                        report['s0_camera_release_checks'].append({'id':camera['id'],**evidence})
            except BaseException as error:
                camera_handoff_ok=False
                report['s0_cleanup_errors'].append('camera_exit:'+repr(error));engine.manual_cut=True
            report['s0_camera_helpers']=copy.deepcopy(io.s0_camera_helpers)
            report['s0_camera_helper_checks']=copy.deepcopy(io.s0_camera_helper_checks)
            report['s0_camera_helper_exit_checks']=copy.deepcopy(io.s0_camera_helper_exit_checks)
            report['s0_camera_handoff_closed']=camera_handoff_ok
        if io.child is None and io.s0_daemon is None and camera_handoff_ok:
            try:
                inspect_processes(io,bindings,proc_helper,cleanup=True)
                for index in sorted(released):
                    ref=bindings['protection'][active_side][('rx_trust.lock','estop_latch.lock')[index]]
                    require(Path(ref['path']).stat().st_ino==os.fstat(active_locks[index].fileno()).st_ino==ref['inode'],
                            '回收活动侧锁前inode变化')
                    fcntl.flock(active_locks[index].fileno(),fcntl.LOCK_EX|fcntl.LOCK_NB)
                for elevated in (False,True):inspect_holder(io,DEVICES[active_side],elevated,cleanup=True)
                report['s0_final_protection']=verify_s0_protection(bindings,'final',active_side)
                io.s0_post_ready=True
            except BaseException as error:report['s0_cleanup_errors'].append('handoff_back:'+repr(error))
        if not io.s0_post_ready:
            engine.manual_cut=True
        for fd in pinned_latches:
            try:os.close(fd)
            except BaseException as error:report['s0_cleanup_errors'].append('latch_pin_close:'+repr(error))
        if report['s0_cleanup_errors'] and failure is None:
            raise ValueError('默认S0收尾失败：'+repr(report['s0_cleanup_errors']))


def validate_mixed_raw_plan(plan):
    contract=plan.get('bus2_mixed_raw_hold')
    require(isinstance(contract,dict),'缺少混合维护合同')
    selected=contract.get('selected_motor_id')
    require(not any(key in plan for key in ('right_wheel_pulse','right_wheel_feedback_only',
                'front_wheel_feedback_only','left_wheel_feedback_only'))
            and plan.get('position_targets')==[] and plan.get('scope')==mixed_raw_scope(selected)
            and contract==mixed_raw_contract(plan['bindings'],selected),
            '混合维护禁止混用旧动作或改动固定raw合同')
    require(plan.get('base_targets')==[{'side':side,'motor':motor,'name':name,
            'decision':'RAW_DIAGNOSTIC' if motor==selected else 'RAW_ZERO_HOLD',
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
    row=strict_json(text);contract=plan['bus2_mixed_raw_hold'];selected_motor=contract['selected_motor_id']
    keys={'schema','operation','result','error','serial_device','arm_calibration_path',
          'go_expires_at_unix_ns','motor_ids','wheel_ids','wheel_raw_goal','pulse_duration_ns',
          'zero_window_ns','enable_budget_ns','loaded_calibration','arm_anchors','wheel_anchors',
          'arm_final','cycle_count','cycles','wheel_samples','zero_samples','timing','cleanup'}
    tolerance_key='calibration_range_tolerances_raw'
    legacy_tolerances=isinstance(row,dict) and tolerance_key not in row and contract[tolerance_key]==[0]*6
    if not legacy_tolerances:keys.add(tolerance_key)
    legacy_schema=isinstance(row,dict) and row.get('schema')=='FEETECH_MIXED_WHEEL9_V1'
    if not legacy_schema:keys.add('selected_motor_id')
    require(isinstance(row,dict) and set(row)==keys,'混合child原件schema字段错误')
    def integer(value,low=0,high=None):
        require(type(value) is int and value>=low and (high is None or value<=high),'混合原件整数越界')
        return value
    def array(value,count,low=0,high=4095):
        require(type(value) is list and len(value)==count,'混合原件数组长度错误')
        return [integer(item,low,high) for item in value]
    require(array(row.get(tolerance_key,[0]*6),6)==contract[tolerance_key],'混合原件逐电机校准容差与计划不符')
    require((legacy_schema and selected_motor==9
             or row['schema']=='FEETECH_MIXED_WHEEL_V1' and integer(row['selected_motor_id'],7,9)==selected_motor)
            and row['operation']==contract['operation']
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
        low,high=effective_calibration_bounds(cal,contract[tolerance_key][index-1])
        value=integer(anchor['raw'],low,high)
        require(integer(anchor['goal_raw'],0,4095)==value
                and 0<=times[1]-integer(anchor['sampled_ns'],1)<=contract['anchor_freshness_ns'],
                '六臂实际Goal不等于新鲜原位锚点')
        require(isinstance(last,dict) and set(last)=={'motor_id','raw'}
                and integer(last['motor_id'])==index
                and abs(integer(last['raw'],0,4095)-value)<=4,'六臂末置偏离固定锚点')
    wheel_anchors=row['wheel_anchors']
    require(type(wheel_anchors) is list and len(wheel_anchors)==2,'未选两轮新鲜锚点缺失')
    for anchor,motor in zip(wheel_anchors,(motor for motor in contract['wheel_ids'] if motor!=selected_motor)):
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
    selected=contract['wheel_ids'].index(selected_motor)
    held=[slot for slot,motor in enumerate(contract['wheel_ids']) if motor!=selected_motor]
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
        require(words[selected]<=contract['velocity_raw_max'],f'物理{selected_motor}运动反馈方向/范围失败')
        positive|=words[selected]>0
        if last is not None:
            step=(positions[selected]-last)%4096
            require(step<=contract['position_step_raw_max'],f'物理{selected_motor}相邻位置越界或逆向')
            travel+=step
        last=positions[selected]
    require(positive and travel>0,f'物理{selected_motor}缺少正速度及正位移联合反馈')
    zero=row['zero_samples']
    require(type(zero) is list and len(zero)==3,f'物理{selected_motor}零速三组原件缺失')
    for index,sample in enumerate(zero):
        require(isinstance(sample,dict) and set(sample)=={'started_ns','finished_ns','torque_enable',
                'goal_velocity_word','present_velocity_word'},f'物理{selected_motor}零速原件字段错误')
        scheduled=times[3]+contract['zero_offsets_ns'][index]
        end=min(times[2]+contract['zero_window_ns'],times[3]+contract['zero_offsets_ns'][index+1]) if index<2 else times[2]+contract['zero_window_ns']
        require(scheduled<=integer(sample['started_ns'],1)<=integer(sample['finished_ns'],1)<=end
                and integer(sample['torque_enable'])==1 and integer(sample['goal_velocity_word'])==0,
                f'物理{selected_motor}零速时间/TE1/GV0失败')
        word=integer(sample['present_velocity_word'],0,65535)
        require(word in contract['zero_final_raw_unsigned'] if index==2 else word<=1000 or word==0x8032,
                f'物理{selected_motor}零速反馈不满足原合同')
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
    contract=plan['bus2_mixed_raw_hold'];expiry=engine.go_expires_at_unix_ns;selected=contract['selected_motor_id']
    arms=[{'side':'right','motor':motor,'name':name,'kind':'position_hold','status':'FAIL','cleanup_errors':[]}
          for motor,name in enumerate(MIXED_ARM_NAMES,1)]
    wheel={'side':'right','motor':selected,'name':WHEEL_PULSE_TARGETS[WHEEL_STOP_TARGETS[selected]][2],'kind':'velocity',
           'status':'FAIL','raw_velocity':500,'cleanup_errors':[]}
    engine.rows.extend([*arms,wheel])
    argv=[contract['tool']['path'],DEVICES['right'],str(selected),contract['operation'],
          '--arm-calibration',contract['arm_calibration']['path'],'--go-expires-at-ns',str(expiry),
          '--wheel-ids',','.join(map(str,contract['wheel_ids'])),
          '--calibration-range-tolerances-raw',','.join(map(str,contract['calibration_range_tolerances_raw']))]
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
    if 's0_mixed_hold' in plan:
        validate_s0_mixed_plan(plan)
        return []
    if 's0_position_hold' in plan:
        validate_s0_position_plan(plan)
        return []
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
              gripper_goal=1600,arm_range_id=None,mixed_wheel_id=None,s0_mixed_hold=False,s0_position_hold=False,
              with_cameras=False):
    require(type(with_cameras) is bool and (not with_cameras or s0_mixed_hold or s0_position_hold),
            '相机只能显式附加到默认S0保持模式')
    require(arm_range_id is None or bus2_arm_range_only and type(arm_range_id) is int and arm_range_id==3,
            '单ID3选择只能附加到右臂范围只读')
    require(mixed_wheel_id is None or bus2_mixed_raw_hold and type(mixed_wheel_id) is int and mixed_wheel_id in (7,8,9),
            '混合选轮只能附加到混合维护且仅接受物理7/8/9')
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
            and type(right_gripper_release) is bool and type(s0_mixed_hold) is bool and type(s0_position_hold) is bool
            and sum((bool(pulse_requested),right_wheel_feedback_only,front_wheel_feedback_only,
                     left_wheel_feedback_only,bus2_mixed_raw_hold,bus2_arm_range_only,right_gripper_release,s0_mixed_hold,s0_position_hold))<=1
            and not ((pulse_requested or right_wheel_feedback_only or front_wheel_feedback_only
                      or left_wheel_feedback_only or bus2_mixed_raw_hold or bus2_arm_range_only or right_gripper_release or s0_mixed_hold or s0_position_hold)
                     and position_target_names is not None),
            '轮诊断禁止互相混用或混用位置选择')
    position_targets = [] if (pulse_requested or right_wheel_feedback_only or front_wheel_feedback_only
                             or left_wheel_feedback_only or bus2_mixed_raw_hold or bus2_arm_range_only or right_gripper_release or s0_mixed_hold or s0_position_hold) else select_position_targets(position_target_names)
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
            "bindings": bindings, "bus_config":copy.deepcopy(BUS_CONFIG or {}),
            "left_bus_config":copy.deepcopy(LEFT_BUS_CONFIG or {}),
            "script_sha256": hashlib.sha256(script).hexdigest(), "script_bytes": len(script),
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
        selected=9 if mixed_wheel_id is None else mixed_wheel_id
        plan['bus2_mixed_raw_hold']=mixed_raw_contract(bindings,selected)
        plan['scope']=plan['bus2_mixed_raw_hold']['scope']
        plan['base_targets']=[{'side':side,'motor':motor,'name':name,
            'decision':'RAW_DIAGNOSTIC' if motor==selected else 'RAW_ZERO_HOLD',
            'reason_code':'FW310_PHASE12_RAW_0_500_ONLY'} for side,motor,name in BASE_TARGETS]
        plan['limitations']=['仅本次FW3.10/Phase12/Mode1的raw{0,+500}维护合同，不解除通用profile/SI门',
                             '维护真实adapter链；不声明SerialTick、S0/S1、D15或生产验收完成']
        validate_mixed_raw_plan(plan)
    if s0_mixed_hold:
        plan['s0_mixed_hold']=s0_mixed_contract(bindings,with_cameras)
        plan['scope']=s0_scope('right',with_cameras)
        plan['base_targets']=[{'side':side,'motor':motor,'name':name,
            'decision':'RAW_DIAGNOSTIC' if motor==WHEEL_PULSE_TARGETS['front'][1] else 'RAW_ZERO_HOLD',
            'reason_code':'DEFAULT_S0_SUPERVISED_RAW_0_500_ONLY'} for side,motor,name in BASE_TARGETS]
        plan['limitations']=['默认S0软件链与原STOP51分开留证；冻结反馈不是adapter内部enable锚点',
                             'D15 OPEN/DEFERRED；不声明物理动作、无人值守或生产验收完成']
        validate_s0_mixed_plan(plan)
    if s0_position_hold:
        plan['s0_position_hold']=s0_position_contract(bindings,with_cameras)
        plan['scope']=s0_scope('left',with_cameras)
        plan['limitations']=['左总线默认S0八关节原位保持；'+('配置sensor同步采集；' if with_cameras else '不包含相机；')+'不调整电机参数',
                             '原STOP51与原4raw位置检查独立后验；D15 OPEN/DEFERRED，deployable=false']
        validate_s0_position_plan(plan)
    if bus2_arm_range_only:
        plan['bus2_arm_range_only']=arm_range_contract(bindings,arm_range_id)
        plan['scope']=plan['bus2_arm_range_only']['scope']
        plan['limitations']=['所选关节PP仅为顺序采样的范围诊断，不是混合动作200毫秒新鲜锚点',
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
    if 's0_mixed_hold' in plan:
        validate_s0_mixed_plan(plan)
    if 's0_position_hold' in plan:
        validate_s0_position_plan(plan)
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
    if 's0_position_hold' in plan:
        facts=go['physical']['left']
        require(0<go['expires_at_ns']-go['issued_at_ns']<=600_000_000_000
                and 0<=now_ns-facts['confirmed_at_ns']<=600_000_000_000,
                '左侧默认S0现场/GO最多600秒')
        require(facts.get('position_hold_confirmation_scope')==plan['s0_position_hold']['scope']
                and isinstance(facts.get('position_hold_confirmation_answer'),str)
                and facts['position_hold_confirmation_answer'].strip(),'缺本轮左侧八关节保持具体现场确认')
    if 'bus2_arm_range_only' in plan:
        facts=go['physical']['right']
        verify_arm_calibration(plan)
        require(0 < go['expires_at_ns']-go['issued_at_ns'] <= 600_000_000_000
                and 0 <= now_ns-facts['confirmed_at_ns'] <= 600_000_000_000,
                '右臂范围只读现场/GO最多600秒')
        require(facts.get('arm_range_confirmation_scope')==plan['bus2_arm_range_only']['scope']
                and isinstance(facts.get('arm_range_confirmation_answer'),str)
                and facts['arm_range_confirmation_answer'].strip(), '缺本轮右臂范围只读具体现场确认')
    if any(key in plan for key in ('right_wheel_pulse','right_wheel_feedback_only','front_wheel_feedback_only',
                                  'left_wheel_feedback_only','bus2_mixed_raw_hold','s0_mixed_hold')):
        facts=go['physical']['right']
        require(0 < go['expires_at_ns']-go['issued_at_ns'] <= 600_000_000_000
                and 0 <= now_ns-facts['confirmed_at_ns'] <= 600_000_000_000,
                '右轮诊断现场/GO最多600秒')
        require(all(facts.get(key) is True for key in ('all_three_wheels_off_ground','base_stably_supported',
                    'continuous_wheel_observation'))
                and facts.get('base_confirmation_kind')=='CURRENT_SETUP_READY'
                and facts.get('base_confirmation_scope')==(
                    plan['s0_mixed_hold']['scope'] if 's0_mixed_hold' in plan else
                    plan['bus2_mixed_raw_hold']['scope'] if 'bus2_mixed_raw_hold' in plan else
                    LEFT_FEEDBACK_SCOPE if 'left_wheel_feedback_only' in plan else
                    FRONT_FEEDBACK_SCOPE if 'front_wheel_feedback_only' in plan else
                    RIGHT_FEEDBACK_SCOPE if 'right_wheel_feedback_only' in plan else plan['right_wheel_pulse']['scope'])
                and isinstance(facts.get('base_confirmation_answer'),str) and facts['base_confirmation_answer'].strip(),
                '缺本轮架空支撑/持续观察及具体动作现场确认')


def is_read_only_journal_command(argv):
    """仅识别既有按boot/Invocation读取日志的命令，不接受journalctl管理选项。"""
    return (len(argv)==7 and list(argv[:5])==['sudo','-n','journalctl','--no-pager','--output=json']
            and all(isinstance(value,str) and re.fullmatch(prefix+r'[0-9a-f]{32}',value)
                    for prefix,value in zip(('_BOOT_ID=','_SYSTEMD_INVOCATION_ID='),argv[5:])))


class RealExecutor:
    """普通命令仅一个child；S0专用分支另持daemon，两者都必须真实回收。"""
    def __init__(self, tool, output):
        self.tool, self.output = tool, Path(output)
        self.interrupted = None
        self.sequence = 0
        self.child = None
        self.s0_daemon = None
        self.s0_post_ready = False
        self.commands = []
        self.process_snapshots = []
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
                        if row.get('diagnostic_journal'):
                            print('爸爸，日志采集超预算，仍持有真实wait；停止状态由最终退出和读回确认。', flush=True)
                        else:
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

    def command(self, argv, *, cleanup=False, allowed=(0,), input_raw=None, mixed_stop_check=None,
                s0_sender=False,s0_control=False,diagnostic_journal=False):
        require(type(diagnostic_journal) is bool, '日志诊断类别必须为布尔值')
        if diagnostic_journal:
            require(is_read_only_journal_command(argv) and input_raw is None and mixed_stop_check is None
                    and not s0_sender and not s0_control, '日志诊断类别仅允许原只读journalctl命令')
        if s0_sender or s0_control:
            contract=self.s0_contract
            expected=tuple(self.s0_sender_argv) if s0_sender else None
            controls=[s0_trust_argv(contract,operation) for operation in ('authorize','revoke')]
            require(not (s0_sender and s0_control)
                    and (tuple(argv)==expected if s0_sender else list(argv) in controls),
                    'S0专用日志兼容仅用于本轮绑定sender/授权命令')
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
        if diagnostic_journal:
            row['diagnostic_journal'] = True
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
                                              env={**(self.s0_environment if s0_sender or s0_control else os.environ), 'PATH':'/usr/sbin:/usr/bin:/sbin:/bin', 'LC_ALL':'C',
                                                   'RUST_LOG':'error'})
                row["pid"] = self.child.pid
                if s0_sender:
                    self.s0_sender_started=True
                    row['launch_identity']=self.s0_bind_process(self.child,actual_argv,'sender')
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
            require(s0_sender or s0_control or err == b'', 'child stderr非空，原件保留')
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
                and argv[2] in tuple(map(str,getattr(self,'arm_range_motor_ids',range(1,7)))) and argv[3]=='read-raw'
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
        low,high=effective_calibration_bounds(calibration,motor_range_tolerance(self.plan,'right',6))
        row={'side':'right','motor':6,'name':'right_arm_gripper','kind':'position_release','status':'FAIL',
             'goal_raw':goal,'return_to_initial':False,'position_accuracy':{'status':'NOT_RUN','stages':{}}}
        self.rows.append(row);error=None
        def stopped_at(initial=None):
            ready()
            require(self.read('right',6,'Operating_Mode')==0
                    and self.read('right',6,'Torque_Enable')==0
                    and self.read('right',6,'Present_Velocity')==0,'右夹爪模式/断扭/静止状态漂移')
            value=self.read('right',6,'Present_Position')
            require(type(value) is int and low<=value<=high
                    and 0<(goal-value)*contract['direction']<=contract['maximum_increment_raw']
                    and (initial is None or value==initial),'右夹爪初值/增量/稳定性不符，不重算目标')
            return value
        try:
            require(calibration==contract['calibration'] and low<=goal<=high,
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
            low,high=effective_calibration_bounds(calibration,motor_range_tolerance(self.plan,side,motor))
            require(low <= anchor <= goal <= high, "锚点/目标超有效校准范围")
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
    targets=plan['bus2_arm_range_only']['targets']
    require(len(samples)==3*len(targets) and len(ranges)==len(targets),'右臂范围读取原件不完整')
    start,finish=row.get('started_ns'),row.get('finished_ns')
    require(type(start) is int and type(finish) is int and start<=finish,'右臂范围采集时刻缺失')
    previous_finish=start;previous_sequence=-1
    for index,sample in enumerate(samples):
        _,motor,name=targets[index//3];register=ARM_RANGE_REGS[index%3]
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
            status=calibration_range_status(value,calibration,plan,'right',motor)
            require(ranges[index//3]=={'motor':motor,'name':name,'position_raw':value,
                    'range_min':calibration['range_min'],'range_max':calibration['range_max'],'status':status,
                    'sampled_ns':end},'右臂范围校准判定变化')
        previous_finish,previous_sequence=end,command
    require(row.get('range_status')==('OUTSIDE' if any(value['status']=='OUTSIDE' for value in ranges) else
            'WITHIN_TOLERANCE' if any(value['status']=='WITHIN_TOLERANCE' for value in ranges) else 'INSIDE'),
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
        for _,motor,name in engine.plan['bus2_arm_range_only']['targets']:
            calibration=calibrations[motor-1]
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
                        row['ranges'].append({'motor':motor,'name':name,
                            'position_raw':value,'range_min':calibration['range_min'],'range_max':calibration['range_max'],
                            'status':calibration_range_status(value,calibration,engine.plan,'right',motor),
                            'sampled_ns':sample['finished_ns']})
                except BaseException as error:
                    sample.update(finished_ns=io.now(),error=repr(error));raise
        row['range_status']=('OUTSIDE' if any(value['status']=='OUTSIDE' for value in row['ranges']) else
                            'WITHIN_TOLERANCE' if any(value['status']=='WITHIN_TOLERANCE' for value in row['ranges']) else 'INSIDE')
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
    if 'right_wheel_pulse' in engine.plan:wheel=pulse_wheel(engine.plan)
    elif 'bus2_mixed_raw_hold' in engine.plan:
        wheel=WHEEL_STOP_TARGETS[engine.plan['bus2_mixed_raw_hold']['selected_motor_id']]
    else:wheel='right'
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
    text = io.command(["systemctl", "show", unit, "--property=Id,LoadState,ActiveState,SubState,MainPID,Job"],cleanup=cleanup)
    fields = dict(line.split("=", 1) for line in text.splitlines() if "=" in line)
    require(fields.get('Id') == unit, '服务Id不完整')
    state = (fields.get('ActiveState'), fields.get('SubState'))
    require(fields.get('LoadState') in ('loaded', 'not-found') and fields.get('MainPID')=='0'
            and fields.get('Job') in ('', '0', '0 /')
            and (state == ('inactive', 'dead') or unit in (HANDOFF_UNIT, 'xlerobot-handoff.service')
                 and state == ('active', 'exited')), f'业务服务未停稳或预检未完成：{unit}')
    return fields


def inspect_processes(io,bindings,proc_helper,*,cleanup=False,owned=()):
    def proc_probe(candidates):
        argv=['sudo','-n','/usr/bin/python3','-I','-S','-c',proc_helper.PROC_PROBE_CODE]
        request={'schema':'P7_PROC_PROBE_V1','candidates':candidates}
        raw=io.command(argv,input_raw=json.dumps(request).encode(),cleanup=cleanup)
        return {'exit_code':0,'response':strict_json(raw),'request':request}
    # sender仍由command持有时，复用原只读探针自己的有限子进程，不占用执行器child槽。
    probe=proc_helper.privileged_process_probe if owned else proc_probe
    snapshot=proc_helper.process_snapshot(-1,io.output,[Path(p) for p in bindings['binary_paths']],probe_runner=probe)
    io.process_snapshots.append(snapshot)
    if owned:
        fields=('pid','start_ticks','boot_id','pgid','uids','exe','argv')
        require(not snapshot['unreadable'] and all(any(all(row.get(key)==expected.get(key) for key in fields)
                    for expected in owned) for row in snapshot['related']),
                '存在非本轮精确绑定的控制进程或身份不可读')
        snapshot['s0_owned_identities']=copy.deepcopy(list(owned))
        snapshot['s0_owned_only']=True
    else:
        require(snapshot['clean'], '控制/维护进程残留或身份不可读')
    return snapshot


def inspect_holder(io,device,elevated,*,cleanup=False):
    text=io.command((['sudo','-n'] if elevated else [])+['fuser',device],allowed=(1,),cleanup=cleanup)
    require(text=='','串口被占用')
    return {'path':device,'elevated':elevated,'stdout_empty':True,'exit_code':1}


def idle(io, bindings, proc_helper, *, cleanup=False):
    # 前置保持fail-fast；后验直接调用下面独立函数，逐项留证。
    for unit in UNITS + OLD_UNITS + [HANDOFF_UNIT]:
        inspect_unit(io,unit,cleanup=cleanup)
    inspect_processes(io,bindings,proc_helper,cleanup=cleanup)
    for device in DEVICES.values():
        for elevated in (False,True):
            inspect_holder(io,device,elevated,cleanup=cleanup)
    for camera in bindings.get('cameras',[]):
        require(s0_camera_device_identity(camera['device'])==camera['device_identity'],'相机字符设备身份变化')
        for elevated in (False,True):inspect_holder(io,camera['device'],elevated,cleanup=cleanup)


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
    s0_mode=any(key in report.get('plan',{}) for key in ('s0_mixed_hold','s0_position_hold'))
    active_side=s0_plan_entry(report['plan'])[1] if s0_mode else None
    active_paths={ref['path'] for ref in bindings['protection'].get(active_side,{}).values()} if s0_mode else set()
    tasks=[(f"ref:{reference['path']}",lambda reference=reference:verify_ref(reference))
           for reference in bindings['refs'] if reference['path'] not in active_paths]
    tasks += [(f'protection:{side}/{name}',lambda side=side,name=name,reference=reference:verify_protection_file(side,name,reference))
              for side,files in bindings['protection'].items() for name,reference in files.items() if not (s0_mode and side==active_side)]
    if s0_mode:
        tasks.append(('s0_protection',lambda:verify_s0_protection(bindings,'final',active_side)))
        contract=report['plan'][s0_plan_entry(report['plan'])[0]]
        if 'cameras' in contract:
            tasks += [(f"camera_identity:{camera['id']}",lambda camera=camera:verify_s0_camera_identity(camera))
                      for camera in contract['cameras']]
            tasks.append(('camera_helpers_terminated',lambda:verify_s0_camera_helpers_gone(io,proc_helper)))
            tasks += [(f"camera_holder:{camera['id']}:{elevated}",
                       lambda camera=camera,elevated=elevated:inspect_holder(io,camera['device'],elevated,cleanup=True))
                      for camera in contract['cameras'] for elevated in (False,True)]
            tasks.append(('camera_report',lambda:verify_s0_camera_report(report)))
    tasks += [(f'service:{unit}',lambda unit=unit:inspect_unit(io,unit,cleanup=True))
              for unit in UNITS+OLD_UNITS+[HANDOFF_UNIT]]
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
    if 's0_position_hold' in report.get('plan',{}):
        try:
            plan=report['plan'];validate_s0_position_plan(plan)
            require(report.get('s0_post_ready') is True and not report.get('s0_cleanup_errors'),
                    '左侧默认S0未完成锁交回/收尾')
            result=parse_s0_position_result(json.dumps(report['s0_sender_result']),plan,report['go']['expires_at_ns'])
            verify_s0_camera_report(report)
            contract=plan['s0_position_hold']
            require([(p['side'],p['motor'],p['kind']) for p in probes]
                    ==[('left',motor,'position_hold') for motor in contract['motor_ids']],
                    '左侧默认S0八关节探针集合错误')
            for slot,probe in enumerate(probes):
                require(probe.get('name')==contract['joint_names'][slot]
                        and probe.get('anchor_source')=='sender_frozen_joint_state'
                        and probe.get('anchor_raw')==result['anchor']['raw'][slot]
                        and probe.get('sampled_ns')==result['anchor']['stamp_ns'],
                        '左侧默认S0冻结反馈目标不符')
        except BaseException:
            return 'FAIL'
        return 'PASS'
    if len(probes)!=7:
        return 'FAIL'
    if any(t.get('decision')=='DEFERRED' for t in report.get('plan',{}).get('base_targets',[])):
        return 'FAIL'
    if 's0_mixed_hold' in report.get('plan',{}):
        try:
            plan=report['plan'];validate_s0_mixed_plan(plan)
            require(report.get('s0_post_ready') is True and not report.get('s0_cleanup_errors'),
                    '默认S0未完成锁交回/收尾')
            result=parse_s0_mixed_result(json.dumps(report['s0_sender_result']),plan,report['go']['expires_at_ns'])
            verify_s0_camera_report(report)
            selected=plan['s0_mixed_hold']['selected_motor_id']
            require([(p['side'],p['motor'],p['kind']) for p in probes]
                    ==[('right',motor,'position_hold') for motor in range(1,7)]+[('right',selected,'velocity')],
                    '默认S0探针集合错误')
            for probe in probes[:6]:
                slot=plan['s0_mixed_hold']['motor_ids'].index(probe['motor'])
                require(probe.get('anchor_source')=='sender_frozen_joint_state'
                        and probe.get('anchor_raw')==result['anchor']['raw'][slot]
                        and probe.get('sampled_ns')==result['anchor']['stamp_ns'],
                        '默认S0冻结反馈目标不符')
        except BaseException:
            return 'FAIL'
        return 'PASS'
    if 'bus2_mixed_raw_hold' in report.get('plan',{}):
        try:
            validate_mixed_raw_plan(report['plan'])
            selected=report['plan']['bus2_mixed_raw_hold']['selected_motor_id']
            require([(p['side'],p['motor'],p['kind']) for p in probes]
                    ==[('right',motor,'position_hold') for motor in range(1,7)]+[('right',selected,'velocity')],
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
    s0_mode=any(key in plan for key in ('s0_mixed_hold','s0_position_hold'))
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
    if arm_range_only:
        io.arm_range_motor_ids=tuple(target[1] for target in plan['bus2_arm_range_only']['targets'])
    io.process_snapshots=[]
    for signum in (signal.SIGINT,signal.SIGTERM,signal.SIGHUP,signal.SIGQUIT):
        signal.signal(signum,io.mark_signal)
    engine = None
    report = {'schema':'MAINTENANCE_RUN_V2','plan':plan,'go':go,'result':'FAIL','hardware_layer':'直接wire维护',
              'S0_S1_head_base_integration':False,'H2_D15_closed':False,'deployable':False}
    if 'bus2_mixed_raw_hold' in plan:
        report['hardware_layer']='维护层真实FeetechAdapter混合链'
    if s0_mode:
        report.update(hardware_layer='默认S0左侧八关节保持与原维护STOP51' if 's0_position_hold' in plan else '默认S0混合命令与原维护STOP51',
                      outer_maintenance_plan_go_locks_connected=True,physical_motion='NOT_OBSERVED')
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
            lock_handles={}
            for side in SERIALS:
                for name in ('rx_trust.lock','estop_latch.lock'):
                    reference=bindings['protection'][side][name]
                    lock=stack.enter_context(Path(reference['path']).open('rb'))
                    fcntl.flock(lock.fileno(),fcntl.LOCK_EX|fcntl.LOCK_NB)
                    lock_handles[(side,name)]=lock
                    report['owned_locks'].append({'side':side,'name':name,'path':reference['path'],'owner_pid':os.getpid(),
                                                  'inode':os.fstat(lock.fileno()).st_ino,'exclusive_flock':True})
            if s0_mode:io.s0_post_ready=True
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
                    if s0_mode:
                        run_s0_mixed_hold(engine,go,proc_helper,lock_handles,report)
                    else:
                        run_motion_probes(engine,bindings,plan)
            finally:
                if s0_mode and not io.s0_post_ready:
                    report['stop_verification']={'status':'UNKNOWN','issues':[{'code':'S0_HANDOFF_NOT_CLOSED'}]}
                    report['position_verification']={'status':'UNKNOWN','issues':[{'code':'S0_UART_POSTCHECK_BLOCKED'}]}
                    finish_identity_verification(report,io,bindings,proc_helper)
                elif diagnostic_only:
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
        report['child_active']=io.child is not None or getattr(io,'s0_daemon',None) is not None
        if s0_mode:report['s0_post_ready']=io.s0_post_ready
        if report['child_active']:
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
    parser.add_argument('--left-bus-config',type=Path,default=DEFAULT_LEFT_BUS_CONFIG,
                        help='左总线TOML，逐电机校准容差与joints/motor_ids同序；与右总线一起绑定计划')
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
                        help='仅FW3.10/Phase12：六臂原位保持及所选轮raw+500/20秒，默认物理9；复用整机停止后验')
    parser.add_argument('--s0-mixed-hold',action='store_true',help=S0_MIXED_SCOPE)
    parser.add_argument('--s0-position-hold',action='store_true',help=S0_POSITION_SCOPE)
    parser.add_argument('--with-cameras',action='store_true',help='仅附加默认S0保持：'+S0_CAMERA_SCOPE)
    parser.add_argument('--mixed-wheel-id',type=int,choices=(7,8,9),
                        help='仅附加到混合维护：选择物理7/8/9，缺省物理9；其余两轮保持raw0')
    parser.add_argument('--bus2-arm-range-only',action='store_true',
                        help='仅右臂1..6逐ID SYNC_READ TE/PV/PP，共18读；记录校准范围，不写电机、不判停止')
    parser.add_argument('--arm-range-id',type=int,choices=(3,),
                        help='仅附加到右臂范围只读：只选右肘物理3，共3读；缺省仍六臂18读')
    grippers=parser.add_mutually_exclusive_group()
    grippers.add_argument('--right-gripper-release',action='store_true',help=GRIPPER_RELEASE_SCOPE)
    grippers.add_argument('--right-gripper-close',action='store_true',help=GRIPPER_TARGETS[1397]['scope'])
    options=parser.parse_args(argv)
    configure_bus_mapping(options.bus_config,options.left_bus_config)
    require(not options.with_cameras or options.s0_mixed_hold or options.s0_position_hold,
            '相机只能显式附加到默认S0保持模式')
    require(options.arm_range_id is None or options.bus2_arm_range_only,'单ID3选择只能附加到右臂范围只读')
    require(options.mixed_wheel_id is None or options.bus2_mixed_raw_hold,'混合选轮只能附加到混合维护')
    require(options.wheel_pulse is None or len(options.wheel_pulse)==1,'每次仅接受一个轮目标')
    wheel_target=options.wheel_pulse[0] if options.wheel_pulse else None
    pulse_requested=options.right_wheel_pulse or wheel_target is not None
    gripper_requested=options.right_gripper_release or options.right_gripper_close
    gripper_goal=1397 if options.right_gripper_close else 1600
    require(not options.zero_stop_check or pulse_requested,'零速保持使能检查只能附加到单轮pulse')
    require(sum((bool(pulse_requested),options.right_wheel_feedback_only,options.front_wheel_feedback_only,
                 options.left_wheel_feedback_only,options.bus2_mixed_raw_hold,options.bus2_arm_range_only,
                 gripper_requested,options.s0_mixed_hold,options.s0_position_hold))<=1
            and not ((pulse_requested or options.right_wheel_feedback_only or options.front_wheel_feedback_only
                      or options.left_wheel_feedback_only or options.bus2_mixed_raw_hold or options.bus2_arm_range_only
                      or gripper_requested or options.s0_mixed_hold or options.s0_position_hold)
                     and options.position_target is not None),'轮诊断禁止互相混用或混用位置选择')
    selected_targets = [] if (pulse_requested or options.right_wheel_feedback_only or options.front_wheel_feedback_only
                             or options.left_wheel_feedback_only or options.bus2_mixed_raw_hold
                             or options.bus2_arm_range_only or gripper_requested or options.s0_mixed_hold or options.s0_position_hold) else select_position_targets(options.position_target)
    if not all((options.bindings,options.output,options.run_id,options.artifact_root)):
        require(not options.execute,'执行参数不完整')
        print(json.dumps({'result':'PLAN_ONLY','actions_started':0,'required':['bindings','output','run-id','artifact-root'],
                          'position_targets':selected_targets,
                          'scope':s0_scope('left',options.with_cameras) if options.s0_position_hold else
                              s0_scope('right',options.with_cameras) if options.s0_mixed_hold else
                              GRIPPER_TARGETS[gripper_goal]['scope'] if gripper_requested else
                              (ARM_ELBOW_RANGE_SCOPE if options.arm_range_id==3 else ARM_RANGE_SCOPE)
                              if options.bus2_arm_range_only else
                              mixed_raw_scope(9 if options.mixed_wheel_id is None else options.mixed_wheel_id)
                              if options.bus2_mixed_raw_hold else '单电机独立维护；不运行硬件、不改保护'},ensure_ascii=False))
        return 0
    plan=make_plan(strict_json(options.bindings.read_text()),options.output,options.run_id,options.artifact_root,
                   options.position_target,right_wheel_pulse=options.right_wheel_pulse,
                   right_wheel_feedback_only=options.right_wheel_feedback_only,wheel_pulse_target=wheel_target,
                   zero_stop_check=options.zero_stop_check,front_wheel_feedback_only=options.front_wheel_feedback_only,
                   left_wheel_feedback_only=options.left_wheel_feedback_only,
                   bus2_mixed_raw_hold=options.bus2_mixed_raw_hold,bus2_arm_range_only=options.bus2_arm_range_only,
                   right_gripper_release=gripper_requested,gripper_goal=gripper_goal,arm_range_id=options.arm_range_id,
                   mixed_wheel_id=options.mixed_wheel_id,s0_mixed_hold=options.s0_mixed_hold,
                   s0_position_hold=options.s0_position_hold,with_cameras=options.with_cameras)
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
