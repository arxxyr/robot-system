"""爸爸，仅头1临时D43→0→43，原+24往返与±4/800ms；默认PLAN_ONLY。"""
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
OBSERVATION_SHA = '11b86f6768359bc7f6f0063bd188868b08c8baf164a59d89a62acdb557d7fc5e'
OBSERVATION_BYTES = 9328
TARGET = ('left', 7, 'head_motor_1')
RESTORE_PARAMETERS = ('P_Coefficient', 'D_Coefficient', 'I_Coefficient', 'Lock', 'CW_Dead_Zone', 'CCW_Dead_Zone')


def require(value, reason):
    if not value:
        raise ValueError(reason)


def reference(path):
    path = Path(path).resolve()
    raw = path.read_bytes()
    return {'path': str(path), 'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}


def load_frozen(path, sha, size, name):
    ref = reference(path)
    require(ref['sha256'] == sha and ref['bytes'] == size, '冻结辅助SHA/字节变化：' + name)
    spec = importlib.util.spec_from_file_location(name, ref['path'])
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module, ref


def write_d(m, h, engine, value, cleanup=False):
    require(type(value) is int and value in (0, 43), '仅本轮D0/D43')
    m.verify_ref(h.RAW_TOOL, executable=True)
    text = engine.io.command([h.RAW_TOOL['path'], m.DEVICES['left'], '7', 'write', 'D_Coefficient', str(value)],
                             cleanup=cleanup)
    require(text == f'write D_Coefficient@7 <- {value} ok\n', 'D写ACK stdout不匹配')


def run_trial(m, h, engine, side, motor, name, calibration, original_position, reader=None, writer=None):
    require((side, motor, name) == TARGET, '本轮仅head1(left7)')
    require(engine.plan['position_delta_raw'] == 24 and engine.plan['return_tolerance_raw'] == 4
            and engine.plan['position_wait_ms'] == 800 and engine.plan['minimum_excursion_raw'] == 12,
            '原+24/±4/800ms/最小12门不允许变化')
    require(engine.io.tool['path'] == h.TOOL['path'], '位置动作必须原f201工具')
    reader = h.raw_read if reader is None else reader
    writer = (lambda module, current, value, cleanup=False: write_d(module, h, current, value, cleanup)) if writer is None else writer
    engine.check_gate()
    trial = {'status': 'RUNNING', 'original_backup': h.BACKUP, 'before_raw': {}, 'd0_raw': {}, 'restore_raw': {},
             'restore_status': 'NOT_NEEDED', 'd0_write_attempted': False, 'errors': [],
             'interpretation': '只验当前起点原精度功能；不同姿态/方向不证明D因果'}
    command = engine.io.command
    start_rows = len(engine.rows)
    motion_error = None

    def guard(argv, **kwargs):
        path = str(argv[0])
        if path in (h.TOOL['path'], h.RAW_TOOL['path']):
            require(not trial.get('uart_blocked') and not engine.manual_cut, 'D对照停止/Lock/恢复未确认，粘性拒绝后续UART')
            if len(argv) > 3 and argv[3] == 'write':
                require(len(argv) == 6 and argv[1:3] == [m.DEVICES['left'], '7'], '写只允许left7')
                if path == h.RAW_TOOL['path']:
                    require(argv[4] == 'D_Coefficient' and argv[5] in ('0', '43'), '新参数写仅D0/D43')
                else:
                    require(argv[4] in ('Goal_Position', 'Torque_Enable'), '原位置写仅GP/TE')
            elif path == h.RAW_TOOL['path']:
                require(len(argv) == 5 and argv[1:4] == [m.DEVICES['left'], '7', 'read-raw']
                        and argv[4] in h.META, '原raw工具只读本轮具名参数')
        return command(argv, **kwargs)

    engine.io.command = guard

    def block(error):
        trial['uart_blocked'] = True
        engine.manual_cut = True
        trial['errors'].append(repr(error))
        print('爸爸，D对照停止/Lock/恢复未确认，请立即人工断电；不再UART或动作。', flush=True)

    def confirm_stopped(raw, cleanup=False):
        # 三项任一未知/不符，立即封锁；尤其Lock失败后不尝试参数恢复。
        for register, expected in (('Torque_Enable', 0), ('Present_Velocity', 0), ('Lock', 1)):
            try:
                raw[register] = reader(m, engine, register, cleanup=cleanup)
                require(raw[register]['raw_unsigned'] == expected, '目标停止/Lock未确认：' + register)
            except BaseException as error:
                block(error)
                raise

    try:
        confirm_stopped(trial['before_raw'])
        for register in h.META:
            if register not in trial['before_raw']:
                trial['before_raw'][register] = reader(m, engine, register)
        values = {key: raw['raw_unsigned'] for key, raw in trial['before_raw'].items()}
        require(all(values[key] == h.BACKUP[key] for key in h.PARAMETERS), '原模式/固件/P16/D43/I0/Phase/死区参数漂移')
        require(values['Min_Position_Limit'] <= values['Present_Position'] <= values['Present_Position'] + 24
                <= values['Max_Position_Limit'], '当前起点/目标超内置限位')
        engine.check_gate()
        trial['d0_write_attempted'] = True  # 即使ACK未知，也按可能已生效进入停止确认与恢复。
        writer(m, engine, 0)
        trial['d0_raw']['D_Coefficient'] = reader(m, engine, 'D_Coefficient')
        try:
            trial['d0_raw']['Lock'] = reader(m, engine, 'Lock')
            require(trial['d0_raw']['Lock']['raw_unsigned'] == 1, 'D0后Lock非1/未知')
        except BaseException as error:
            block(error)
            raise
        require(trial['d0_raw']['D_Coefficient']['raw_unsigned'] == 0, 'D0读回不符，不启动运动')
        original_position(engine, side, motor, name, calibration)
    except BaseException as error:
        motion_error = error
        trial['errors'].append(repr(error))
    finally:
        if trial['d0_write_attempted']:
            trial['restore_status'] = 'FAIL'
            try:
                require(not engine.manual_cut, '原安全收尾或Lock未确认，不再UART或恢复写')
                # 复用R6恢复顺序：原position先safe_off，再独立核TE/PV/Lock，随后恢复并逐项原帧读回。
                confirm_stopped(trial['restore_raw'], cleanup=True)
                writer(m, engine, 43, cleanup=True)
                for register in RESTORE_PARAMETERS:
                    trial['restore_raw'][register] = reader(m, engine, register, cleanup=True)
                    require(trial['restore_raw'][register]['raw_unsigned'] == h.BACKUP[register],
                            'P16/D43/I0/Lock1/死区恢复不符：' + register)
                trial['restore_status'] = 'PASS'
            except BaseException as error:
                block(error)
        if len(engine.rows) == start_rows:
            engine.rows.append({'side': side, 'motor': motor, 'name': name, 'kind': 'position', 'status': 'FAIL',
                                'position_accuracy': {'status': 'NOT_RUN', 'stages': {}}, 'cleanup_errors': [],
                                'error': repr(motion_error)})
        row = engine.rows[start_rows]
        row['d_trial'] = trial
        trial['status'] = 'PASS' if motion_error is None and trial['restore_status'] == 'PASS' and not trial['errors'] else 'FAIL'
        if trial.get('uart_blocked'):
            row['status'] = 'FAIL'
            row.setdefault('cleanup_errors', []).append('D对照停止/Lock/参数恢复未确认；UART已封锁')
    if motion_error is not None:
        raise motion_error
    require(trial['status'] == 'PASS', 'D对照失败，原事实保留')


def install(m, h, observation):
    h.install_stop_wire_capture(m)
    original_witness = m.ProbeEngine.witness_position
    original_position = m.ProbeEngine.position
    # 原R8出程观察与原d97回程/位移/断扭正文直接复用，不改deadline或强行回程。
    m.ProbeEngine.witness_position = lambda engine, side, motor, target, anchor, stage, evidence: observation.witness_position(
        m, engine, side, motor, target, anchor, stage, evidence, original_witness)
    m.ProbeEngine.position = lambda engine, side, motor, name, calibration: run_trial(
        m, h, engine, side, motor, name, calibration, original_position)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for option in ('maintenance', 'helper', 'observation-helper', 'bindings', 'output', 'artifact-root'):
        parser.add_argument('--' + option, type=Path)
    parser.add_argument('--run-id')
    parser.add_argument('--execute', action='store_true')
    parser.add_argument('--approval', type=Path)
    options = parser.parse_args(argv)
    if not all((options.maintenance, options.helper, options.observation_helper, options.bindings,
                options.output, options.artifact_root, options.run_id)):
        require(not options.execute, '执行输入不完整')
        print(json.dumps({'status': 'PLAN_ONLY', 'target': TARGET, 'fixed_p': 16, 'fixed_i': 0,
                          'temporary_d': 0, 'restore_d': 43, 'delta_raw': 24, 'tolerance_raw': 4,
                          'position_wait_ms': 800, 'actions_started': 0, 'parameter_writes': 0,
                          'motion_commands': 0, 'load_raw_reads': 0}, ensure_ascii=False))
        return 0
    require(not options.execute or options.approval is not None, '必须root本次新D0功能试验GO，旧GO全部不适用')
    h, helper_ref = load_frozen(options.helper, HELPER_SHA, HELPER_BYTES, 'head1_d0_original_r6')
    observation, observation_ref = load_frozen(options.observation_helper, OBSERVATION_SHA, OBSERVATION_BYTES,
                                             'head1_d0_original_r8')
    m = h.load_source(options.maintenance)
    bindings = m.strict_json(options.bindings.read_text())
    own_ref = reference(__file__)
    h.required_references(bindings, own_ref)
    require(all(any(h.same_reference(row, expected) for row in bindings['refs'])
                for expected in (helper_ref, observation_ref)), 'R6/R8冻结辅助必须加入本轮完整refs')
    require(h.RAW_TOOL['path'] in bindings['binary_paths'], '原13c参数工具必须加入本轮binary_paths')
    plan = m.make_plan(bindings, options.output, options.run_id, options.artifact_root, ['head-1'])
    plan['head1_d0_trial'] = {
        'wrapper': own_ref, 'parameter_stop_wire_helper': helper_ref, 'outward_observation_helper': observation_ref,
        'target': TARGET, 'raw_tool': h.RAW_TOOL, 'original_parameter_backup': h.BACKUP,
        'fixed_p': 16, 'fixed_i': 0, 'temporary_d': 0, 'restore_d': 43, 'maximum_parameter_write_attempts': 2,
        'entry_and_restore_require': {'Torque_Enable': 0, 'Present_Velocity': 0, 'Lock': 1},
        'restored_raw_registers': RESTORE_PARAMETERS, 'no_lock_phase_or_dead_zone_write': True,
        'd_register_address': 22, 'd_register_area': '厂家EEPROM配置区', 'lock_kept': 1,
        'save_command_sent': False, 'power_cycle_persistence': 'NOT_VERIFIED',
        'position_body': 'UNMODIFIED_FROZEN_D97', 'outward_witness': 'UNMODIFIED_FROZEN_R8',
        'hold_until_original_deadline': True, 'first_qualified_within_original_deadline_required': True,
        'return_witness': 'UNMODIFIED_FROZEN_D97', 'load_raw_reads': 0,
        'write_whitelist': {'raw_left7': {'D_Coefficient': [0, 43]}, 'position_left7': ['Goal_Position', 'Torque_Enable']},
        'interpretation': '有限功能试验；起点/方向不同，不证明D因果或长期参数闭环',
        'ordinary_stop_wire': {'tool': h.TOOL, 'strace': h.STRACE, 'flags': h.STRACE_FLAGS,
                               'right_device': h.RIGHT_DEVICE, 'motors': [8, 9], 'additional_uart_reads': 0,
                               'wire_frame_review': 'PENDING_ROOT_REVIEW_OF_COMPLETE_STRACE'}}
    if not options.execute:
        print(json.dumps({'status': 'PLAN_ONLY', 'plan_sha256': m.digest(plan), 'plan': plan}, ensure_ascii=False, indent=2))
        return 0
    go = m.strict_json(options.approval.read_text())
    m.validate_go(go, plan, time.time_ns())
    m.verify_ref(h.STRACE, executable=True)
    install(m, h, observation)
    return m.execute(plan, go, options.approval)


if __name__ == '__main__':
    try:
        sys.exit(main())
    except Exception as error:
        print('爸爸，头1D0功能试验前置拒绝：' + repr(error), file=sys.stderr)
        sys.exit(2)
