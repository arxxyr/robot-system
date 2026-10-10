"""爸爸，仅头1原参数负向24往返的平台Load原帧对照；默认PLAN_ONLY，无参数写。"""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import sys
import time

sys.dont_write_bytecode = True
REVERSE_SHA = 'c30a0e129cc3435cc647480200c9a31ab83a72f0828c28caaf2efb60cd7a6aaa'
REVERSE_BYTES = 12658
TARGET = ('left', 7, 'head_motor_1')
LOAD_FIELDS = {'operation', 'serial_device', 'motor_id', 'register', 'requested_register_address',
               'width', 'raw_unsigned', 'tx', 'rx'}


def require(value, reason):
    if not value:
        raise ValueError(reason)


def reference(path):
    path = Path(path).resolve()
    raw = path.read_bytes()
    return {'path': str(path), 'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}


def load_reverse(path):
    ref = reference(path)
    require(ref['sha256'] == REVERSE_SHA and ref['bytes'] == REVERSE_BYTES, '必须冻结R9逆向辅助')
    spec = importlib.util.spec_from_file_location('head1_frozen_reverse_r10', ref['path'])
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module, ref


def load_tool_reference(bindings, h):
    tool = bindings.get('load_tool')
    require(type(tool) is dict and {'path', 'sha256', 'bytes'} <= set(tool), '缺少明确load_tool完整引用')
    require(type(tool['path']) is str and Path(tool['path']).is_absolute()
            and type(tool['sha256']) is str and re.fullmatch('[0-9a-f]{64}', tool['sha256'])
            and type(tool['bytes']) is int and tool['bytes'] > 0, 'load_tool引用字段错误')
    require(tool['path'] not in (h.TOOL['path'], h.RAW_TOOL['path']), 'Load必须采用单独冻结的新诊断工具')
    matches = [row for row in bindings['refs'] if row.get('path') == tool['path']]
    require(matches and all(row == tool for row in matches), 'load_tool与refs必须完整一致')
    require(tool['path'] in bindings['binary_paths'], 'load_tool必须加入binary_paths')
    return dict(tool)


def read_load_raw(m, engine, tool, deadline, attempt_evidence=None):
    # 沿用原command预算；这不是绝对READ截止，迟返只能留证并拒验。
    m.verify_ref(tool, executable=True)
    started = engine.io.now()
    if not deadline - 500_000_000 <= started <= deadline - 400_000_000:
        return {'status': 'NOT_SAMPLED', 'reason_code': 'LOAD_TOOL_VERIFICATION_MISSED_TRIGGER_WINDOW',
                'verified_at_ns': started, 'earliest_read_start_ns': deadline - 500_000_000,
                'latest_read_start_ns': deadline - 400_000_000}
    device = m.DEVICES['left']
    if attempt_evidence is not None:
        require(attempt_evidence['raw_read_attempts'] == 0, 'Load原帧最多一次')
        attempt_evidence.update(raw_read_attempts=1, raw_command_started_ns=started)
    text = engine.io.command([tool['path'], device, '7', 'read-raw', 'Present_Load'])
    finished = engine.io.now()
    require(len(text.splitlines()) == 1, 'Load raw stdout行数错误')
    raw = m.strict_json(text)
    require(type(raw) is dict and set(raw) == LOAD_FIELDS, 'Load raw JSON字段错误')
    require((raw['operation'], raw['serial_device'], raw['register']) == ('sync_read', device, 'Present_Load')
            and all(type(raw[key]) is int for key in ('motor_id', 'requested_register_address', 'width', 'raw_unsigned'))
            and (raw['motor_id'], raw['requested_register_address'], raw['width']) == (7, 60, 2),
            'Load raw字段身份错误')
    tx = [255, 255, 254, 5, 130, 60, 2, 7]
    tx.append((~sum(tx[2:])) & 255)
    require(type(raw['tx']) is list and all(type(value) is int and 0 <= value <= 255 for value in raw['tx'])
            and raw['tx'] == tx, 'Load原TX错误')
    rx = raw['rx']
    require(type(rx) is list and all(type(value) is int and 0 <= value <= 255 for value in rx)
            and len(rx) == 8 and rx[:5] == [255, 255, 7, 4, 0] and (sum(rx[2:]) & 255) == 255
            and int.from_bytes(bytes(rx[5:7]), 'little') == raw['raw_unsigned'], 'Load原RX/checksum/原字错误')
    return {'status': 'SAMPLED', 'started_ns': started, 'finished_ns': finished, 'raw': raw}


def witness_position(m, engine, side, motor, target, anchor, stage, evidence, original, tool):
    if stage != 'outward':
        return original(engine, side, motor, target, anchor, stage, evidence)
    require((side, motor) == TARGET[:2], 'Load对照只允许left7')
    require(engine.plan['position_wait_ms'] == 800 and engine.plan['return_tolerance_raw'] == 4
            and engine.plan['minimum_excursion_raw'] == 12, '原800ms/±4/最小12位移门不允许变化')
    started = engine.io.now()
    deadline = started + 800_000_000
    load = {'status': 'NOT_SAMPLED', 'raw_read_attempts': 0, 'maximum_raw_reads': 1, 'probe_considered': False,
            'tool': tool, 'trigger_earliest_elapsed_ns': 300_000_000, 'trigger_latest_elapsed_ns': 400_000_000,
            'minimum_remaining_ns': 400_000_000, 'minimum_negative_excursion_raw': 12,
            'interpretation': '仅原字，无负载扭矩或电流解码',
            'read_deadline_contract': '既有command预算；不提供硬实时绝对READ截止'}
    row = {'stage': stage, 'status': 'FAIL', 'target_raw': target, 'anchor_raw': anchor,
           'tolerance_raw': 4, 'budget_ms': 800, 'started_ns': started, 'deadline_ns': deadline,
           'samples': [], 'final_signed_error_raw': None, 'first_qualified_sample': None,
           'first_qualified_elapsed_ns': None,
           'observation_policy': 'FIRST_QUALIFIED_WITHIN_ORIGINAL_DEADLINE_THEN_OBSERVE_TO_SAME_DEADLINE',
           'load_diagnostic': load}
    evidence[stage] = row

    def record(position, stamp):
        sample = {'stamp_ns': stamp, 'position_raw': position, 'signed_error_raw': position - target}
        row['samples'].append(sample)
        row.update(finished_ns=stamp, elapsed_ns=stamp - started, final_signed_error_raw=position - target)
        if abs(position - anchor) > 32:
            row['failure_code'] = 'POSITION_EXCURSION_LIMIT'
            raise m.PositionAccuracyError(row)
        # 与R9相同：只有原deadline内首次合格可以计数，期限不续期。
        if row['first_qualified_sample'] is None and stamp <= deadline and abs(position - target) <= 4:
            row['first_qualified_sample'] = dict(sample)
            row['first_qualified_elapsed_ns'] = stamp - started
        return sample

    while engine.io.now() < deadline:
        previous = row['samples'][-1] if row['samples'] else None
        sample = record(engine.read(side, motor, 'Present_Position'), engine.io.now())
        now = engine.io.now()
        if (not load['probe_considered'] and previous is not None
                and previous['position_raw'] == sample['position_raw']
                and abs(sample['signed_error_raw']) > 4
                and anchor - sample['position_raw'] >= engine.plan['minimum_excursion_raw']
                and 300_000_000 <= now - started <= 400_000_000
                and deadline - now >= 400_000_000):
            load.update(status='FAIL', probe_considered=True, trigger_ns=now,
                        predecessor_sample=dict(previous), before_sample=dict(sample))
            try:
                result = read_load_raw(m, engine, tool, deadline, load)
                if result['status'] == 'NOT_SAMPLED':
                    # 校验耗过窗口：保留SKIP理由，零次新工具command，不补读或重试。
                    load.update(status='NOT_SAMPLED', skip=result)
                else:
                    load['read'] = result
                    require(engine.io.now() < deadline, 'Load原帧迟返，禁止额外夹后PP读取')
                    # 仍用原位置工具顺序读一次PP，禁止第二个UART读者并行。
                    after = record(engine.read(side, motor, 'Present_Position'), engine.io.now())
                    load['after_sample'] = dict(after)
                    require(after['stamp_ns'] <= deadline and engine.io.now() <= deadline, 'Load夹后PP迟返')
                    load.update(status='SAMPLED', plateau_bracket_confirmed=(after['position_raw'] == sample['position_raw']))
            except BaseException as error:
                load.update(status='FAIL', error=repr(error), finished_ns=engine.io.now())
                # 原样交给冻结R9分类：读取诊断故障为ACTION_FAILED，真实PP越界仍为原精度失败。
                raise
        if engine.io.now() >= deadline:
            break
        remaining = deadline - engine.io.now()
        if remaining > 0:
            engine.io.sleep(min(0.05, remaining / 1_000_000_000))
    finished = engine.io.now()
    row.update(finished_ns=finished, elapsed_ns=finished - started)
    if row['first_qualified_sample'] is not None:
        row['status'] = 'PASS'
        return row['samples']
    row['failure_code'] = 'POSITION_ACCURACY_TIMEOUT'
    raise m.PositionAccuracyError(row)


def checked_position(m, h, r, engine, side, motor, name, calibration, tool, reader=None):
    require((side, motor, name) == TARGET, '本轮只允许头1left7')
    command = engine.io.command
    tool_paths = (engine.io.tool['path'], h.RAW_TOOL['path'], tool['path'])
    load_commands = 0

    def guard(argv, **kwargs):
        nonlocal load_commands
        if str(argv[0]) in tool_paths:
            require(not engine.manual_cut, '已要求人工断电，粘性拒绝全部UART')
        if str(argv[0]) == tool['path']:
            require(argv == [tool['path'], m.DEVICES['left'], '7', 'read-raw', 'Present_Load'],
                    '新Load工具只能执行left7一次具名只读')
            require(load_commands == 0, 'Load原帧最多一次，不重试')
            load_commands += 1
        elif str(argv[0]) in tool_paths and 'write' in argv:
            require(len(argv) == 6 and argv[0] == engine.io.tool['path'] and argv[1:3] == [m.DEVICES['left'], '7']
                    and argv[3] == 'write' and argv[4] in ('Goal_Position', 'Torque_Enable'),
                    '动作写白名单仅原f201 left7 GP/TE')
        return command(argv, **kwargs)

    engine.io.command = guard
    original = lambda e, *args: r.reverse_position(m, e, *args)
    return r.checked_position(m, h, engine, side, motor, name, calibration, original, reader)


def install(m, h, r, tool):
    h.install_stop_wire_capture(m)
    original_witness = m.ProbeEngine.witness_position
    m.ProbeEngine.witness_position = lambda engine, side, motor, target, anchor, stage, evidence: witness_position(
        m, engine, side, motor, target, anchor, stage, evidence, original_witness, tool)
    m.ProbeEngine.position = lambda engine, side, motor, name, calibration: checked_position(
        m, h, r, engine, side, motor, name, calibration, tool)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('maintenance', 'helper', 'reverse-helper', 'bindings', 'output', 'artifact-root', 'approval'):
        parser.add_argument('--' + name, type=Path)
    parser.add_argument('--run-id')
    parser.add_argument('--execute', action='store_true')
    options = parser.parse_args(argv)
    if not all((options.maintenance, options.helper, options.reverse_helper, options.bindings,
                options.output, options.run_id, options.artifact_root)):
        require(not options.execute, '执行输入不完整')
        print(json.dumps({'status': 'PLAN_ONLY', 'target': TARGET, 'fixed_p': 16, 'fixed_d': 43, 'fixed_i': 0,
                          'delta_raw': -24, 'tolerance_raw': 4, 'position_wait_ms': 800,
                          'actions_started': 0, 'parameter_writes': 0, 'motion_commands': 0,
                          'load_raw_reads': 0, 'maximum_load_raw_reads': 1}, ensure_ascii=False))
        return 0
    require(not options.execute or options.approval is not None, '必须root本次新Load对照GO，旧R9不适用')
    r, reverse_ref = load_reverse(options.reverse_helper)
    h, helper_ref = r.load_helper(options.helper)
    m = h.load_source(options.maintenance)
    bindings = m.strict_json(options.bindings.read_text())
    own_ref = reference(__file__)
    h.required_references(bindings, own_ref)
    require(all(any(h.same_reference(row, ref) for row in bindings['refs']) for ref in (helper_ref, reverse_ref)),
            '原939与冻结R9辅助必须加入完整refs')
    tool = load_tool_reference(bindings, h)
    plan = m.make_plan(bindings, options.output, options.run_id, options.artifact_root, ['head-1'])
    plan['position_delta_raw'] = -24
    plan['head1_load_observation'] = {
        'wrapper': own_ref, 'reverse_helper': reverse_ref, 'readonly_stop_wire_helper': helper_ref,
        'raw_tool': h.RAW_TOOL, 'load_tool': tool, 'target': TARGET, 'original_parameter_backup': h.BACKUP,
        'fixed_p': 16, 'fixed_d': 43, 'fixed_i': 0, 'parameter_writes': 0,
        'direction_delta_raw': -24, 'maximum_excursion_from_anchor_raw': 32,
        'position_body': 'UNMODIFIED_FROZEN_R9_REVERSE', 'position_body_source_sha256': h.SOURCE_SHA,
        'hold_until_original_deadline': True, 'return_witness': 'UNMODIFIED_FROZEN_D97',
        'load': {'register': 'Present_Load', 'address': 60, 'width': 2, 'maximum_raw_reads': 1,
                 'trigger': 'TWO_EQUAL_PP_ERROR_GT4_NEGATIVE_EXCURSION_GE12_ELAPSED300_TO400MS_REMAINING_GE400MS',
                 'trigger_earliest_elapsed_ns': 300_000_000, 'trigger_latest_elapsed_ns': 400_000_000,
                 'minimum_negative_excursion_raw': 12, 'minimum_remaining_ns': 400_000_000,
                 'verification_misses_window': 'NOT_SAMPLED_ZERO_NEW_TOOL_COMMAND_NO_RETRY',
                 'after_raw': 'ONE_ORIGINAL_TOOL_PP_READ', 'no_trigger': 'NOT_SAMPLED_NO_RETRY',
                 'read_deadline_contract': 'EXISTING_COMMAND_BUDGET_NO_HARD_REALTIME_ABSOLUTE_READ'},
        'action_write_whitelist': {'tool': h.TOOL, 'side': 'left', 'motor': 7,
                                   'registers': ['Goal_Position', 'Torque_Enable']},
        'ordinary_stop_wire': {'tool': h.TOOL, 'strace': h.STRACE, 'flags': h.STRACE_FLAGS,
                               'right_device': h.RIGHT_DEVICE, 'motors': [8, 9], 'additional_uart_reads': 0,
                               'wire_frame_review': 'PENDING_ROOT_REVIEW_OF_COMPLETE_STRACE'}}
    if not options.execute:
        print(json.dumps({'status': 'PLAN_ONLY', 'plan_sha256': m.digest(plan), 'plan': plan}, ensure_ascii=False, indent=2))
        return 0
    go = m.strict_json(options.approval.read_text())
    m.validate_go(go, plan, time.time_ns())
    m.verify_ref(h.STRACE, executable=True)
    m.verify_ref(tool, executable=True)
    install(m, h, r, tool)
    return m.execute(plan, go, options.approval)


if __name__ == '__main__':
    try:
        sys.exit(main())
    except Exception as error:
        print('爸爸，头1Load对照前置拒绝：' + repr(error), file=sys.stderr)
        sys.exit(2)
