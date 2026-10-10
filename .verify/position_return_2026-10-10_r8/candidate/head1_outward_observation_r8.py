"""爸爸，仅头1原P16/I0出程观察至原800ms截止，再用原函数回程；默认PLAN_ONLY。"""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import time

sys.dont_write_bytecode = True
HELPER_SHA = '939e590b9e0ef829a465e9b3bc620d14cb114c01f7c2f2d591d3d34db3c47c81'
HELPER_BYTES = 12425
TARGET = ('left', 7, 'head_motor_1')


def require(value, reason):
    if not value:
        raise ValueError(reason)


def reference(path):
    path = Path(path).resolve()
    data = path.read_bytes()
    return {'path': str(path), 'sha256': hashlib.sha256(data).hexdigest(), 'bytes': len(data)}


def load_helper(path):
    ref = reference(path)
    require(ref['sha256'] == HELPER_SHA and ref['bytes'] == HELPER_BYTES, '必须原939参数/停止原帧辅助')
    spec = importlib.util.spec_from_file_location('head1_original_readonly_wire_r8', ref['path'])
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module, ref


def witness_position(m, engine, side, motor, target, anchor, stage, evidence, original):
    if stage != 'outward':
        return original(engine, side, motor, target, anchor, stage, evidence)
    require((side, motor) == TARGET[:2], '出程观察只允许left7')
    require(engine.plan['position_wait_ms'] == 800 and engine.plan['return_tolerance_raw'] == 4,
            '原800ms/±4门不允许变化')
    started = engine.io.now()
    deadline = started + engine.plan['position_wait_ms'] * 1_000_000
    row = {'stage': stage, 'status': 'FAIL', 'target_raw': target, 'anchor_raw': anchor,
           'tolerance_raw': engine.plan['return_tolerance_raw'], 'budget_ms': engine.plan['position_wait_ms'],
           'started_ns': started, 'deadline_ns': deadline, 'samples': [], 'final_signed_error_raw': None,
           'first_qualified_sample': None, 'first_qualified_elapsed_ns': None,
           'observation_policy': 'FIRST_QUALIFIED_WITHIN_ORIGINAL_DEADLINE_THEN_OBSERVE_TO_SAME_DEADLINE'}
    evidence[stage] = row
    while engine.io.now() < deadline:
        position = engine.read(side, motor, 'Present_Position')
        stamp = engine.io.now()
        sample = {'stamp_ns': stamp, 'position_raw': position, 'signed_error_raw': position - target}
        row['samples'].append(sample)
        row.update(finished_ns=stamp, elapsed_ns=stamp - started, final_signed_error_raw=position - target)
        if abs(position - anchor) > 32:
            row['failure_code'] = 'POSITION_EXCURSION_LIMIT'
            raise m.PositionAccuracyError(row)
        # 保留原首次合格门；迟返的首个合格样本不能计PASS，之后也不更新原deadline。
        if row['first_qualified_sample'] is None and stamp <= deadline and abs(position - target) <= 4:
            row['first_qualified_sample'] = dict(sample)
            row['first_qualified_elapsed_ns'] = stamp - started
        if stamp >= deadline:
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


def checked_position(m, h, engine, side, motor, name, calibration, original, reader=None):
    require((side, motor, name) == TARGET, '本次仅头1left7')
    reader = h.raw_read if reader is None else reader
    entry = {'status': 'FAIL', 'fixed_p': 16, 'fixed_i': 0, 'raw': {}, 'parameter_writes': 0}
    command = engine.io.command
    before_rows = len(engine.rows)

    def guard(argv, **kwargs):
        if entry.get('uart_blocked') and str(argv[0]) in (engine.io.tool['path'], h.RAW_TOOL['path']):
            raise ValueError('入口停止未确认，粘性拒绝后续UART')
        return command(argv, **kwargs)

    engine.io.command = guard
    try:
        engine.check_gate()
        for register in ('Torque_Enable', 'Present_Velocity'):
            try:
                entry['raw'][register] = reader(m, engine, register)
                require(entry['raw'][register]['raw_unsigned'] == 0, '入口停止未确认：' + register)
            except BaseException:
                entry['uart_blocked'] = True
                engine.manual_cut = True
                print('爸爸，入口停止未确认，请立即人工断电；禁止后续UART。', flush=True)
                raise
        for register in h.META:
            if register not in entry['raw']:
                entry['raw'][register] = reader(m, engine, register)
        values = {key: raw['raw_unsigned'] for key, raw in entry['raw'].items()}
        require(all(values[key] == h.BACKUP[key] for key in h.PARAMETERS), '原P16/I0/固件/模式/其他参数漂移')
        require(values['Min_Position_Limit'] <= values['Present_Position'] <= values['Present_Position'] + 24
                <= values['Max_Position_Limit'], '本轮动态起点/目标超原限位')
        entry['status'] = 'PASS'
        original(engine, side, motor, name, calibration)
    except BaseException as error:
        entry['error'] = repr(error)
        if len(engine.rows) == before_rows:
            cleanup = [] if entry.get('uart_blocked') else engine.safe_off(side, motor, False)
            engine.rows.append({'side': side, 'motor': motor, 'name': name, 'kind': 'position', 'status': 'FAIL',
                                'position_accuracy': {'status': 'NOT_RUN', 'stages': {}},
                                'cleanup_errors': cleanup, 'error': repr(error)})
        raise
    finally:
        if len(engine.rows) > before_rows:
            engine.rows[before_rows]['parameter_entry'] = entry


def install(m, h):
    h.install_stop_wire_capture(m)
    original_witness = m.ProbeEngine.witness_position
    original_position = m.ProbeEngine.position
    m.ProbeEngine.witness_position = lambda engine, side, motor, target, anchor, stage, evidence: witness_position(
        m, engine, side, motor, target, anchor, stage, evidence, original_witness)
    m.ProbeEngine.position = lambda engine, side, motor, name, calibration: checked_position(
        m, h, engine, side, motor, name, calibration, original_position)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--maintenance', type=Path)
    parser.add_argument('--helper', type=Path)
    parser.add_argument('--bindings', type=Path)
    parser.add_argument('--output', type=Path)
    parser.add_argument('--run-id')
    parser.add_argument('--artifact-root', type=Path)
    parser.add_argument('--execute', action='store_true')
    parser.add_argument('--approval', type=Path)
    options = parser.parse_args(argv)
    if not all((options.maintenance, options.helper, options.bindings, options.output,
                options.run_id, options.artifact_root)):
        require(not options.execute, '执行输入不完整')
        print(json.dumps({'status': 'PLAN_ONLY', 'target': TARGET, 'fixed_p': 16, 'fixed_i': 0,
                          'outward_hold_to_original_ms': 800, 'delta_raw': 24, 'tolerance_raw': 4,
                          'actions_started': 0, 'parameter_writes': 0, 'motion_commands': 0}, ensure_ascii=False))
        return 0
    require(not options.execute or options.approval is not None,
            '必须root本次新头1动作GO；R7只读授权不适用')
    h, helper_ref = load_helper(options.helper)
    m = h.load_source(options.maintenance)
    bindings = m.strict_json(options.bindings.read_text())
    own_ref = reference(__file__)
    h.required_references(bindings, own_ref)
    require(any(h.same_reference(row, helper_ref) for row in bindings['refs']), '原939辅助必须加入本轮完整refs')
    plan = m.make_plan(bindings, options.output, options.run_id, options.artifact_root, ['head-1'])
    plan['head1_outward_observation'] = {
        'wrapper': own_ref, 'readonly_stop_wire_helper': helper_ref, 'raw_tool': h.RAW_TOOL, 'target': TARGET,
        'original_parameter_backup': h.BACKUP, 'fixed_p': 16, 'fixed_i': 0, 'parameter_writes': 0,
        'hold_until_original_deadline': True, 'first_qualified_within_original_deadline_required': True,
        'return_witness': 'UNMODIFIED_FROZEN_D97',
        'ordinary_stop_wire': {'tool': h.TOOL, 'strace': h.STRACE, 'flags': h.STRACE_FLAGS,
                               'right_device': h.RIGHT_DEVICE, 'motors': [8, 9], 'additional_uart_reads': 0,
                               'wire_frame_review': 'PENDING_ROOT_REVIEW_OF_COMPLETE_STRACE'}}
    if not options.execute:
        print(json.dumps({'status': 'PLAN_ONLY', 'plan_sha256': m.digest(plan), 'plan': plan}, ensure_ascii=False, indent=2))
        return 0
    go = m.strict_json(options.approval.read_text())
    m.validate_go(go, plan, time.time_ns())
    m.verify_ref(h.STRACE, executable=True)
    install(m, h)
    return m.execute(plan, go, options.approval)


if __name__ == '__main__':
    try:
        sys.exit(main())
    except Exception as error:
        print('爸爸，头1出程观察前置拒绝：' + repr(error), file=sys.stderr)
        sys.exit(2)
