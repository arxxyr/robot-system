"""爸爸，右9轮单ID SYNC_WRITE46/2B零目标短窗诊断；可能转动，仅当次架空就绪及新GO允许执行。"""
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
RUN_ID = 'base-zero-sync-diagnostic-r1'
TARGET = ('right', 9, 'base_right_wheel')
SOURCE_SHA = 'd97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41'
HELPER_SHA = '70d777bd5c2a19e2ce3f57e68f9abd40eaed2b8a9aa8c592e7fe53a940f1e38c'
HELPER_BYTES = 12220
FIELD_SCOPE = '仅右9/base_right_wheel一次单ID9 SYNC_WRITE Goal_Velocity0（无ACK）→发送完成后至少50ms→Enable→500ms原PV观察→Disable及原STOP51；可能转动，三轮须已离地且底盘稳定支撑'
EXPECTED = {'Torque_Enable': 0, 'Present_Velocity': 0, 'Goal_Velocity': 0,
            'Model_Number': 777, 'Firmware_Major_Version': 3, 'Firmware_Minor_Version': 10,
            'Operating_Mode': 1, 'Phase': 12, 'Lock': 1}
BUDGET_NS = 500_000_000
SAMPLE_COUNT = 3
SAMPLE_OFFSETS_NS = (0, 200_000_000, 400_000_000)
TTL_NS = 600_000_000_000
SYNC_SETTLE_NS = 50_000_000
SYNC_TOOL = {'path': '/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/quality/frozen/feetech_poke', 'sha256': 'b2bfedb1bc420c9957a276cd69f36f5fd47c66d0b7aaa519cf3375e257eabd94', 'bytes': 1507224}  # 爸爸，root已核本轮唯一原生ARM64 ELF；无新现场GO。
SYNC_STDOUT = 'sync-zero Goal_Velocity@9 <- 0: SYNC_WRITE已发送，无ACK；未确认电机接收或实际速度\n'
WRITE_SEQUENCE = (('Goal_Velocity', 0), ('Torque_Enable', 1), ('Torque_Enable', 0))


def require(value, reason):
    if not value:
        raise ValueError(reason)


def reference(path):
    path = Path(path).resolve()
    raw = path.read_bytes()
    return {'path': str(path), 'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}


def load_frozen(path, sha, size, name):
    path = Path(path)
    require(path.is_absolute() and path == path.resolve() and stat.S_ISREG(path.lstat().st_mode), '源码路径身份错误')
    ref = reference(path)
    require(ref['sha256'] == sha and (size is None or ref['bytes'] == size), '冻结源码SHA/bytes改变')
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value, ref


def validate_sync_tool():
    require(isinstance(SYNC_TOOL, dict) and set(SYNC_TOOL) == {'path', 'sha256', 'bytes'},
            '新SYNC原生ELF未绑定，零I/O拒绝；禁止代用旧ELF')
    require(type(SYNC_TOOL['path']) is str and Path(SYNC_TOOL['path']).is_absolute()
            and type(SYNC_TOOL['sha256']) is str and re.fullmatch('[0-9a-f]{64}', SYNC_TOOL['sha256'])
            and type(SYNC_TOOL['bytes']) is int and SYNC_TOOL['bytes'] > 0, '新SYNC ELF pin字段错误')
    return dict(SYNC_TOOL)


def sync_contract():
    return {'tool': validate_sync_tool(), 'instruction': 0x83, 'register_address': 46,
            'width': 2, 'motor_ids': [9], 'raw_unsigned': 0, 'ack': 'NONE',
            'result': 'sent_no_ACK', 'minimum_settle_ns': SYNC_SETTLE_NS,
            'settle_clock': 'same_monotonic_from_send_completion_to_before_Enable'}


class GOExpired(ValueError):
    """爸爸，GO到期禁止新常规UART；已尝试Enable时保留原固定TE0独立收尾。"""


def new_row():
    return {'side': TARGET[0], 'motor': TARGET[1], 'name': TARGET[2], 'kind': 'zero_diag',
            'status': 'NOT_RUN', 'before_raw': [], 'verification_raw': [], 'write_attempts': [],
            'observation': {'status': 'NOT_RUN', 'samples': [], 'budget_ns': BUDGET_NS,
                            'minimum_in_budget_pv0_samples': 3}, 'cleanup_errors': [], 'errors': [],
            'uart_blocked': False, 'firmware_contract_verified': False}


def install_guard(m, h, engine):
    if not hasattr(engine, 'zero_diag'):
        engine.zero_diag = new_row()
    require(not getattr(engine.io, 'zero_guard_installed', False)
            or getattr(engine.io, 'zero_guard_engine', None) is engine, '执行器归属改变')
    if getattr(engine.io, 'zero_guard_installed', False):
        return
    original = engine.io.command
    sync_tool = validate_sync_tool()
    tool_paths = (h.TOOL['path'], h.RAW_TOOL['path'], h.STRACE['path'], sync_tool['path'])
    write_index = 0

    def command(argv, **kwargs):
        nonlocal write_index
        path = str(argv[0]) if argv else ''
        if path not in tool_paths:
            metadata = any(argv in (["fuser", device], ["sudo", "-n", "fuser", device],
                                   ["udevadm", "info", "--query=property", "--name", device])
                           for device in m.DEVICES.values())
            if (any(str(arg) in tool_paths for arg in argv[1:])
                    or (any(str(arg) in m.DEVICES.values() for arg in argv[1:]) and not metadata)):
                error = ValueError('未知工具或前缀不得绕过本轮UART名单')
                block_uart(engine, engine.zero_diag, error)
                raise error
            return original(argv, **kwargs)
        row = engine.zero_diag
        require(not row['uart_blocked'] and not engine.manual_cut, '粘性封锁：禁止后续UART')
        try:
            check_go = getattr(engine, 'zero_go_check', None)
            if check_go is not None and not kwargs.get('cleanup', False):
                check_go()
            require(path != h.STRACE['path'], '不得直接绕过具名工具名单')
            sync = path == sync_tool['path']
            writing = sync or (path == h.TOOL['path'] and len(argv) > 3 and argv[3] == 'write')
            if writing:
                require(write_index < 3, '只允许原三写，不重试')
                if write_index == 0:
                    require(sync and argv == [sync_tool['path'], m.DEVICES['right'], '9', 'sync-zero'],
                            '首写仅新工具单ID9 SYNC Goal0；普通GV写禁止')
                    m.verify_ref(sync_tool, executable=True)
                else:
                    require(not sync and len(argv) == 6 and argv[:4] == [h.TOOL['path'], m.DEVICES['right'], '9', 'write']
                            and argv[4:] == ['Torque_Enable', str(WRITE_SEQUENCE[write_index][1])],
                            '后两写仅原TE1→TE0；禁止其他目标或参数写')
                    if write_index == 1:
                        require(engine.io.now() >= row['sync_sent_finished_ns'] + SYNC_SETTLE_NS,
                                'SYNC发送完成后未满原50ms，禁止Enable')
                require((write_index == 2) == bool(kwargs.get('cleanup', False)), 'Disable仅独立收尾')
                attempt = {'register': WRITE_SEQUENCE[write_index][0], 'value': WRITE_SEQUENCE[write_index][1],
                           'started_ns': engine.io.now(), 'cleanup': bool(kwargs.get('cleanup', False)),
                           'operation': 'SYNC_WRITE' if sync else 'WRITE', 'ack': 'NONE' if sync else 'REQUIRED'}
                row['write_attempts'].append(attempt)
                write_index += 1
                text = original(argv, **kwargs)
                if sync:
                    require(text == SYNC_STDOUT, 'SYNC发送stdout未知；不能伪称ACK或接收')
                    row['sync_sent_finished_ns'] = engine.io.now()
                    attempt['result'] = 'sent_no_ACK'
                else:
                    require(text == f'write {argv[4]}@9 <- {argv[5]} ok\n', '写ACK stdout未知')
                    attempt['result'] = 'ACK_STDOUT_VALID'
                attempt['finished_ns'] = engine.io.now()
            elif path == h.RAW_TOOL['path']:
                require(len(argv) == 5 and argv[1:4] == [m.DEVICES['right'], '9', 'read-raw']
                        and argv[4] in EXPECTED and not kwargs.get('cleanup', False),
                        'raw仅right9入口/观察九字段只读，不冒充独立cleanup')
                text = original(argv, **kwargs)
                # 返回时GO门前先核同份原帧；到期不能掩盖坏帧或引出一次补读。
                reader = SimpleNamespace(io=SimpleNamespace(command=lambda *args, **kw: text))
                parser = SimpleNamespace(DEVICES=m.DEVICES, strict_json=m.strict_json,
                                         verify_ref=lambda *args, **kw: None)
                raw = h.raw_read(parser, reader, argv[4])
                active = any(attempt['register'] == 'Torque_Enable' and attempt['value'] == 1
                             for attempt in row['write_attempts'])
                expected = 1 if argv[4] == 'Torque_Enable' and active else EXPECTED[argv[4]]
                if argv[4] != 'Present_Velocity' or not active:
                    require(raw['raw_unsigned'] == expected, '入口或Goal0/Enable原读回不符:' + argv[4])
            else:
                require(path == h.TOOL['path'] and len(argv) == 5 and argv[3] == 'read' and argv[4] in m.REGS
                        and any(argv[1:3] == [m.DEVICES[side], motor]
                                for side, motor in (key.split(':') for key in m.MOTOR_KEYS)), '原17电机读取名单错误')
                text = original(argv, **kwargs)
                match = re.fullmatch(rf'read {re.escape(argv[4])}@{argv[2]} = (-?\d+)\n', text)
                require(match is not None, '普通READ完整stdout/身份未知')
                require(argv[4] not in m.STOP_REGS or int(match[1]) == 0, '普通停止READ未确认零值:' + argv[4])
            if check_go is not None and not kwargs.get('cleanup', False):
                check_go()
            return text
        except BaseException as error:
            attempted_enable = any(attempt['register'] == 'Torque_Enable' and attempt['value'] == 1
                                   for attempt in row['write_attempts'])
            if not isinstance(error, GOExpired) or not attempted_enable:
                block_uart(engine, row, error)
            raise

    engine.io.command = command
    engine.io.zero_guard_engine = engine
    engine.io.zero_guard_installed = True


def validate_plan(m, plan):
    require(plan['run_id'] == RUN_ID and plan['position_targets'] == [], '仅本轮空位置目标诊断')
    expected = [{'side': side, 'motor': motor, 'name': name, 'decision': 'DEFERRED',
                 'reason_code': 'BASE_FIRMWARE_CONTRACT_UNVERIFIED'} for side, motor, name in m.BASE_TARGETS]
    require(plan['base_targets'] == expected and plan.get('firmware_contract_verified') is False, '三轮合同必须仍DEFERRED')
    trial = plan['zero_diag']
    require(trial['target'] == list(TARGET) and trial['scope'] == FIELD_SCOPE
            and trial['expected_raw'] == EXPECTED and trial['budget_ns'] == BUDGET_NS
            and trial['requested_pv_samples'] == SAMPLE_COUNT and trial['minimum_in_budget_pv0_samples'] == 3
            and trial['sample_offsets_ns'] == list(SAMPLE_OFFSETS_NS)
            and trial['write_sequence'] == [list(row) for row in WRITE_SEQUENCE]
            and trial['goal_velocity_address'] == 46 and trial['goal_velocity_width'] == 2,
            '零目标/Enable/500ms/原帧名单不得变化')
    require(trial['sync_write'] == sync_contract(), '本轮SYNC工具/0x83/单ID/无ACK/50ms合同不得变化')
    return []


def validate_base_go(m, original, go, plan, now):
    validate_plan(m, plan)
    original(go, plan, now)
    require(0 < go['expires_at_ns'] - go['issued_at_ns'] <= TTL_NS, '本轮GO最多600秒')
    facts = go['physical']['right']
    require(facts.get('all_three_wheels_off_ground') is True and facts.get('base_stably_supported') is True
            and facts.get('base_confirmation_kind') == 'CURRENT_SETUP_READY'
            and isinstance(facts.get('base_confirmation_answer'), str) and facts['base_confirmation_answer'].strip()
            and facts.get('base_confirmation_scope') == FIELD_SCOPE,
            '必须本次三轮已离地且稳定支撑；能力答复不构成现场就绪')
    require(type(facts['confirmed_at_ns']) is int and 0 <= now - facts['confirmed_at_ns'] <= TTL_NS,
            '架空现场确认过期或未来时间')


def make_plan(m, h, bindings, output, artifact_root, own_ref, helper_ref):
    sync_tool = validate_sync_tool()
    require(any(h.same_reference(row, sync_tool) for row in bindings['refs'])
            and sync_tool['path'] in bindings['binary_paths'], '新SYNC工具完整引用必须入refs及binary_paths')
    require(sync_tool['path'] not in (h.TOOL['path'], h.RAW_TOOL['path'], h.STRACE['path']), '禁止旧ELF代替本轮SYNC工具')
    m.verify_ref(sync_tool, executable=True)
    h.required_references(bindings, own_ref)
    require(any(h.same_reference(row, helper_ref) for row in bindings['refs']), '本轮raw辅助未入refs')
    plan = m.make_plan(bindings, output, RUN_ID, artifact_root, ['head-2'])
    plan['position_targets'] = []
    plan['firmware_contract_verified'] = False
    plan['scope'] = FIELD_SCOPE
    plan['zero_diag'] = {'target': list(TARGET), 'scope': FIELD_SCOPE, 'wrapper': own_ref,
                         'raw_helper': helper_ref, 'raw_tool': h.RAW_TOOL, 'write_tool': h.TOOL,
                         'strace': h.STRACE, 'expected_raw': dict(EXPECTED), 'budget_ns': BUDGET_NS,
                         'requested_pv_samples': SAMPLE_COUNT, 'minimum_in_budget_pv0_samples': 3,
                         'sample_offsets_ns': list(SAMPLE_OFFSETS_NS),
                         'write_sequence': [list(row) for row in WRITE_SEQUENCE],
                         'goal_velocity_address': 46, 'goal_velocity_width': 2, 'sync_write': sync_contract(),
                         'firmware_contract_verified': False,
                         'limitations': ['本次可能转动；500ms仅软件观察预算，不保证硬件断扭时限',
                                         '仅0/200/400ms三个分散时刻原PV0，不证明连续500ms物理静止；无补读/重试',
                                         '不写41..47连续块/Phase/Lock/mode/EEPROM，不接生产速度TX']}
    validate_plan(m, plan)
    return plan


def block_uart(engine, row, error):
    row['uart_blocked'] = True
    engine.manual_cut = True
    row.setdefault('errors', []).append(repr(error))
    print('爸爸，零速诊断停止/写入/身份未确认，请立即人工断电；禁止后续UART。', flush=True)


def timed_raw(m, h, engine, row, register, destination, cleanup=False):
    sample = {'register': register, 'started_ns': engine.io.now(), 'status': 'UNKNOWN'}
    sequence_before = getattr(engine.io, 'sequence', None)
    commands_before = len(getattr(engine.io, 'commands', []))
    destination.append(sample)
    try:
        sample['raw'] = h.raw_read(m, engine, register, cleanup=cleanup)
        sample['status'] = 'READ_VALID'
        sample['raw_json_bytes'] = len(json.dumps(sample['raw'], separators=(',', ':')).encode())
    except BaseException as error:
        sample['error'] = repr(error)
        if not isinstance(error, GOExpired):
            block_uart(engine, row, error)
        raise
    finally:
        sample['finished_ns'] = engine.io.now()
        sequence_after = getattr(engine.io, 'sequence', None)
        if (sequence_before is not None and sequence_after is not None and sequence_after > sequence_before
                and len(getattr(engine.io, 'commands', [])) > commands_before):
            sample['command_sequence'] = sequence_after
            sample.update({key: engine.io.commands[-1].get(key) for key in ('stdout_bytes', 'stdout_sha256')})
    return sample


def run_zero_diag(m, h, engine):
    validate_plan(m, engine.plan)
    install_guard(m, h, engine)
    row = engine.zero_diag
    require(row['status'] == 'NOT_RUN' and not row['uart_blocked'], '本轮诊断仅首次执行')
    row['status'] = 'FAIL'
    attempted_enable = False
    error = None
    def gate():
        try:
            engine.check_gate()
        except BaseException as failure:
            block_uart(engine, row, failure)
            raise
    try:
        gate()
        for register, expected in EXPECTED.items():
            sample = timed_raw(m, h, engine, row, register, row['before_raw'])
            if sample['raw']['raw_unsigned'] != expected:
                failure = ValueError('新鲜入口停止/型号/固件/Mode/Phase/Lock不符:' + register)
                block_uart(engine, row, failure)
                raise failure
        gate()
        try:
            engine.io.command([validate_sync_tool()['path'], m.DEVICES['right'], '9', 'sync-zero'])
        except BaseException as failure:
            if not row['uart_blocked'] and not isinstance(failure, GOExpired):
                block_uart(engine, row, failure)
            raise
        goal = timed_raw(m, h, engine, row, 'Goal_Velocity', row['verification_raw'])
        if goal['raw']['raw_unsigned'] != 0:
            failure = ValueError('Goal0读回未确认')
            block_uart(engine, row, failure)
            raise failure
        enable_deadline = row['sync_sent_finished_ns'] + SYNC_SETTLE_NS
        row['enable_earliest_ns'] = enable_deadline
        remaining = enable_deadline - engine.io.now()
        if remaining > 0:
            engine.io.sleep(remaining / 1_000_000_000)
        require(engine.io.now() >= enable_deadline, '发送完成后的原50ms下界未到，禁止Enable')
        gate()
        attempted_enable = True
        try:
            engine.write('right', 9, 'Torque_Enable', 1)
        except BaseException as failure:
            if not row['uart_blocked'] and not isinstance(failure, GOExpired):
                block_uart(engine, row, failure)
            raise
        enabled = timed_raw(m, h, engine, row, 'Torque_Enable', row['verification_raw'])
        if enabled['raw']['raw_unsigned'] != 1:
            failure = ValueError('Enable读回未确认')
            block_uart(engine, row, failure)
            raise failure
        observation = row['observation']
        start = engine.io.now()
        deadline = start + BUDGET_NS
        observation.update(status='FAIL', started_ns=start, deadline_ns=deadline)
        for offset in SAMPLE_OFFSETS_NS:
            scheduled = start + offset
            now = engine.io.now()
            require(offset == 0 or now <= scheduled, '原分散请求时刻已错过，不补读')
            if now < scheduled:
                engine.io.sleep((scheduled - now) / 1_000_000_000)
            require(engine.io.now() < deadline, '500ms原期限已到，不补读')
            engine.io.check()
            sample = timed_raw(m, h, engine, row, 'Present_Velocity', observation['samples'])
            sample['scheduled_ns'] = scheduled
            sample['requested_offset_ns'] = offset
            sample['request_lateness_ns'] = max(0, sample['started_ns'] - scheduled)
            sample['completed_in_budget'] = sample['finished_ns'] <= deadline
            sample['raw_zero'] = sample['raw']['raw_unsigned'] == 0
            require(sample['raw_zero'], '观察PV原帧非零，立即Disable收尾')
            require(sample['completed_in_budget'], 'PV原帧完成越过500ms原期限')
        qualified = [sample for sample in observation['samples'] if sample['completed_in_budget'] and sample['raw_zero']]
        observation['in_budget_pv0_samples'] = len(qualified)
        require(len(qualified) >= 3, '原期限内不足3个PV0原帧')
        remaining = deadline - engine.io.now()
        if remaining > 0:
            engine.io.sleep(remaining / 1_000_000_000)
        if getattr(engine, 'zero_go_check', None) is not None:
            engine.zero_go_check()
        observation.update(status='PASS', finished_ns=engine.io.now(), elapsed_ns=engine.io.now() - start)
    except BaseException as failure:
        error = failure
        row['errors'].append(repr(failure))
    finally:
        observation = row['observation']
        if 'started_ns' in observation:
            observation.setdefault('finished_ns', engine.io.now())
            observation.setdefault('elapsed_ns', observation['finished_ns'] - observation['started_ns'])
            observation.setdefault('in_budget_pv0_samples', sum(sample.get('completed_in_budget') is True
                                   and sample.get('raw_zero') is True for sample in observation['samples']))
        if attempted_enable and not engine.manual_cut:
            row['cleanup_errors'] = engine.safe_off('right', 9, False)
            if row['cleanup_errors']:
                block_uart(engine, row, ValueError('Disable/目标停止未确认:' + repr(row['cleanup_errors'])))
        elif engine.manual_cut:
            row['cleanup_errors'].append('粘性封锁：禁止继续UART；立即人工断电')
        row['status'] = 'PASS' if error is None and observation['status'] == 'PASS' and not row['cleanup_errors'] and not engine.manual_cut else 'FAIL'
    require(row['status'] == 'PASS', '独立zero_diag失败，原帧/越界/非零/原FAIL保留')
    return row


def install_write_capture(m, h):
    original = m.RealExecutor.command
    def command(io, argv, **kwargs):
        sync = argv == [validate_sync_tool()['path'], m.DEVICES['right'], '9', 'sync-zero']
        exact = sync or (len(argv) == 6 and argv[:4] == [h.TOOL['path'], m.DEVICES['right'], '9', 'write']
                         and tuple(argv[4:]) in (('Torque_Enable', '1'), ('Torque_Enable', '0')))
        if not exact:
            return original(io, argv, **kwargs)
        m.verify_ref(h.STRACE, executable=True)
        trace = io.output / f'right9-zero-write-command_{io.sequence + 1:04d}.strace'
        with trace.open('xb'):
            pass
        succeeded = False
        try:
            text = original(io, [h.STRACE['path'], *h.STRACE_FLAGS, '-o', str(trace), *argv], **kwargs)
            succeeded = True
            return text
        finally:
            info = trace.lstat()
            require(stat.S_ISREG(info.st_mode) and info.st_nlink == 1, 'WRITE strace原件身份错误')
            raw = trace.read_bytes()
            require(raw, 'WRITE strace原件为空')
            if succeeded:
                require(re.search(rb'(?:^|\n)[^\n]*\+\+\+ exited with 0 \+\+\+\n?\Z', raw),
                        'WRITE strace原件未记录成功终态')
            io.commands[-1]['diagnostic_write_trace'] = {'path': str(trace), 'bytes': len(raw),
                'sha256': hashlib.sha256(raw).hexdigest(), 'original_argv': list(argv),
                'requested_register_address': 46 if sync else 40, 'width': 2 if sync else 1,
                'instruction': 0x83 if sync else 0x03, 'motor_ids': [9],
                'ack': 'NONE_SYNC_WRITE' if sync else 'ORDINARY_WRITE_ACK_REQUIRED',
                'capture_status': 'NATURAL_SUCCESS_TERMINAL' if succeeded else 'ORIGINAL_COMMAND_FAILED',
                'wire_frame_review': 'PENDING_ROOT_REVIEW_OF_ACTUAL_WRITE_STRACE'}
    m.RealExecutor.command = command


def install(m, h, go=None):
    require(not getattr(m, 'zero_wrapper_installed', False), '本轮wrapper不可重复安装')
    original_targets = m.position_targets_from_plan
    original_go = m.validate_go
    original_finish = m.finish_verification
    original_init, original_classify = m.ProbeEngine.__init__, m.classify_run_result
    m.position_targets_from_plan = lambda plan: validate_plan(m, plan) if 'zero_diag' in plan else original_targets(plan)
    m.validate_go = lambda go, plan, now: validate_base_go(m, original_go, go, plan, now)
    def init(engine, *args, **kwargs):
        original_init(engine, *args, **kwargs)
        def check_go():
            now = time.time_ns()
            if now >= go['expires_at_ns']:
                raise GOExpired('本次GO已到期，禁止后续常规UART；不续期，仅原TE0独立收尾')
            validate_base_go(m, original_go, go, engine.plan, now)
        engine.zero_go_check = check_go if go is not None else None
        install_guard(m, h, engine)

    def motion(engine, bindings, plan):
        try:
            run_zero_diag(m, h, engine)
        finally:
            for side, motor, name in m.BASE_TARGETS:
                engine.rows.append({'side': side, 'motor': motor, 'name': name, 'kind': 'velocity',
                                    'status': 'NOT_RUN', 'decision': 'DEFERRED',
                                    'reason_code': 'BASE_FIRMWARE_CONTRACT_UNVERIFIED'})
        raise ValueError('BASE_FIRMWARE_CONTRACT_UNVERIFIED：底盘三项DEFERRED，未调用wheel/pulse')
    def finish(report, io, engine, bindings, proc):
        try:
            original_finish(report, io, engine, bindings, proc)
        finally:
            report['zero_diag'] = engine.zero_diag
            report['position_function_test'] = 'NOT_RUN_NO_NEW_HEAD_OR_ARM_FUNCTION_RESULT'
            if report.get('stop_verification', {}).get('status') != 'PASS' or report.get('identity_verification', {}).get('status') != 'PASS':
                block_uart(engine, report['zero_diag'], ValueError('原STOP51或完整身份后验未确认'))
    m.ProbeEngine.__init__ = init
    m.classify_run_result = lambda report: ('NEEDS_MANUAL_POWER_CUT' if report.get('zero_diag', {}).get('uart_blocked') else original_classify(report))
    m.zero_wrapper_installed = True
    m.run_motion_probes = motion
    m.finish_verification = finish
    h.install_stop_wire_capture(m)
    install_write_capture(m, h)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('maintenance', 'raw-helper', 'bindings', 'output', 'artifact-root', 'approval'):
        parser.add_argument('--' + name, type=Path)
    parser.add_argument('--execute', action='store_true')
    args = parser.parse_args(argv)
    if args.execute:
        validate_sync_tool()  # 未绑定先拒绝，不读取输入/创建输出/启动child。
    if not all((args.maintenance, args.raw_helper, args.bindings, args.output, args.artifact_root)):
        require(not args.execute, '执行输入不完整，零I/O拒绝')
        print(json.dumps({'status': 'PLAN_ONLY', 'run_id': RUN_ID, 'target': TARGET,
                          'scope': FIELD_SCOPE, 'actions_started': 0, 'register_writes': 0,
                          'motion_commands': 0, 'position_targets': [], 'new_GO': False}, ensure_ascii=False))
        return 0
    if args.execute:
        require(args.approval is not None, '必须root本次新架空就绪GO；能力答复和旧GO不适用')
    m, _ = load_frozen(args.maintenance, SOURCE_SHA, None, 'base_zero_original_d97')
    h, helper_ref = load_frozen(args.raw_helper, HELPER_SHA, HELPER_BYTES, 'base_zero_raw_right9')
    bindings = m.strict_json(args.bindings.read_text())
    plan = make_plan(m, h, bindings, args.output, args.artifact_root, reference(__file__), helper_ref)
    if not args.execute:
        print(json.dumps({'status': 'PLAN_ONLY', 'actions_started': 0, 'plan_sha256': m.digest(plan), 'plan': plan}, ensure_ascii=False, indent=2))
        return 0
    go = m.strict_json(args.approval.read_text())
    install(m, h, go)
    m.validate_go(go, plan, time.time_ns())
    return m.execute(plan, go, args.approval)


if __name__ == '__main__':
    try:
        sys.exit(main())
    except Exception as error:
        print('爸爸，底盘零目标诊断前置拒绝：' + repr(error), file=sys.stderr)
        sys.exit(2)
