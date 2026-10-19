"""爸爸，固定right8/base_front_wheel的SYNC/普通PV只读对照；缺省PLAN_ONLY，未绑定新ELF不得执行。"""
import argparse
import contextlib
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import re
import stat
import sys
import time

sys.dont_write_bytecode = True
SUPPORT_SHA = '5f57211caa487717672e317b9441ae5e4a77983bb28899c1fac3e081ab3f9bd7'
SUPPORT_BYTES = 14217
# 新原生诊断ELF通过软件验证后再绑定；缺省不允许执行。
FROZEN_TOOL = {'path': '/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/quality/frozen/feetech_poke', 'sha256': 'b2bfedb1bc420c9957a276cd69f36f5fd47c66d0b7aaa519cf3375e257eabd94', 'bytes': 1507224}
APPROVED_TOOL = None  # 爸爸，完整当前ELF stat仍待当次绑定；不以旧inode或断电答复补值。
STRACE = {'path': '/usr/bin/strace', 'sha256': '5392f54d889a87341d49f970d91e21ab8c1f70a835ac9ead6ee4d17e532ae7f7', 'bytes': 1583504}
STRACE_FLAGS = ('-ttt', '-xx', '-s', '256', '-yy', '-e', 'trace=openat,close,read,write,ioctl')
FIELD_SCOPE = '仅right8/base_front_wheel一次SYNC_READ TE40/1确认0→SYNC_READ PV58/2原word→普通READ PV58/2并留完整strace；任意TE/PV非零或未知立即封锁后续UART，不为凑pair补读；不Enable/Disable/参数写，不证明M17完整停止或物理轮动'
FACTS = ('motor_supplies_on', 'continuous_watch', 'immediate_manual_power_cut',
         'arms_unloaded_supported', 'areas_clear', 'all_three_wheels_off_ground', 'base_stably_supported')
TTL_NS = 600_000_000_000
META = {'Torque_Enable': (40, 1), 'Present_Velocity': (58, 2)}
READS = ((8, 'read-raw', 'Torque_Enable'), (8, 'read-raw', 'Present_Velocity'),
         (8, 'read', 'Present_Velocity'))
RUN_ID = 'base-front-pv-compare-r1'


def require(value, reason):
    if not value:
        raise ValueError(reason)


def own_reference():
    path = Path(__file__).resolve()
    data = path.read_bytes()
    return {'path': str(path), 'sha256': hashlib.sha256(data).hexdigest(), 'bytes': len(data)}


def load_support(reference):
    require(isinstance(reference, dict) and reference.get('sha256') == SUPPORT_SHA
            and reference.get('bytes') == SUPPORT_BYTES, '必须原R4只读身份辅助源码')
    path = Path(reference['path'])
    require(path.is_absolute() and path == path.resolve()
            and stat.S_ISREG(path.lstat().st_mode), 'R4辅助路径身份错误')
    data = path.read_bytes()
    require(len(data) == SUPPORT_BYTES and hashlib.sha256(data).hexdigest() == SUPPORT_SHA,
            'R4辅助SHA/bytes变化')
    spec = importlib.util.spec_from_file_location('readonly12_existing_r4', path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def confirmation(m, reference, now):
    require(isinstance(reference, dict), '缺本轮新现场回执，零启动拒绝')
    receipt = m.strict_json(m.verify_ref(reference).decode())
    stamp = receipt['recorded_at_ns']
    require(type(stamp) is int and 0 <= now - stamp <= TTL_NS, '现场时效过期或错误')
    require(receipt.get('scope') == FIELD_SCOPE and isinstance(receipt.get('answer'), str)
            and receipt['answer'].strip(), '不是本次right8三读现场答复')
    require(isinstance(receipt.get('physical'), dict)
            and all(receipt['physical'].get(name) is True for name in FACTS), '现场供电/看护/支撑/架空条件缺失')
    return receipt


def tool_identity(m, ref):
    require(isinstance(APPROVED_TOOL, dict), '本轮新ELF未绑定，零启动拒绝')
    require(all(APPROVED_TOOL.get(key)==value for key,value in FROZEN_TOOL.items()), '只能本次冻结b2 ELF')
    require(APPROVED_TOOL.get('sha256') not in
            ('f2014ee4008311e7a8725235a39f0d1b3913391f17cbd977b7441f120539114b',
             '13c97115b89d9f8780ea8b9cac80a091ddcc2b73549d34b2d5cbad83c718ffe1'),
            '禁止旧f201/13c替代本次固定b2对照工具')
    require(isinstance(ref, dict), '缺本轮新诊断ELF完整引用')
    require(all(ref.get(key) == value for key, value in APPROVED_TOOL.items()), '不是本轮新ELF')
    raw = m.verify_ref(ref, executable=True)
    info = Path(ref['path']).lstat()
    actual = {'uid': info.st_uid, 'gid': info.st_gid, 'mode': stat.S_IMODE(info.st_mode),
              'device': info.st_dev, 'inode': info.st_ino, 'mtime_ns': info.st_mtime_ns,
              'ctime_ns': info.st_ctime_ns, 'nlink': info.st_nlink, 'symlink': Path(ref['path']).is_symlink(), 'elf_machine': int.from_bytes(raw[18:20], 'little')}
    require(all(ref.get(key) == value for key, value in actual.items()), '新ELF完整元数据变化')


def make_plan(m, config, config_ref, own_ref, bindings):
    require(config['run_id'] == RUN_ID, '只允许本次固定run-id')
    original = m.make_plan(bindings, config['output'], config['run_id'], config['artifact_root'],
                           ['left-wrist', 'head-1', 'head-2'])
    # 保留原f201绑定与三目标身份门，仅取非动作字段，不调用原execute或任何motion方法。
    plan = {key: original[key] for key in ('schema', 'artifact_root', 'output', 'run_id',
                                         'bindings', 'position_targets', 'script_sha256', 'script_bytes')}
    # GO引用不进授权摘要，避免GO SHA与计划摘要互相依赖；完整config仍逐UART核原件。
    plan.update(configuration_path=config_ref['path'],
                configuration_without_go_sha256=m.digest({key: value for key, value in config.items()
                                                           if key != 'approval'}),
                script=own_ref, maintenance=config['maintenance'],
                support=config['support'], field_confirmation=config['field_confirmation'],
                diagnostic_tool=config['diagnostic_tool'], scope=FIELD_SCOPE,
                fixed_reads=READS, register_metadata=META, strace=STRACE, register_writes=0, motion_commands=0,
                position_targets_use='FROZEN_GO_IDENTITY_METADATA_ONLY_NO_POSITION_OPERATION',
                physical_motion='UNKNOWN', full_M17_stop_verification=False)
    return plan


def parse_raw(m, text, device, motor, register):
    require(len(text.splitlines()) == 1, '原始JSON不是完整单行')
    row = m.strict_json(text)
    keys = {'operation', 'serial_device', 'motor_id', 'register',
            'requested_register_address', 'width', 'raw_unsigned', 'tx', 'rx'}
    address, width = META[register]
    require(isinstance(row, dict) and set(row) == keys
            and type(row['motor_id']) is int and type(row['requested_register_address']) is int
            and type(row['width']) is int
            and (row['operation'], row['serial_device'], row['motor_id'], row['register'],
                 row['requested_register_address'], row['width'])
            == ('sync_read', device, motor, register, address, width), '原帧JSON目标/地址/宽度错误')
    tx = [255, 255, 254, 5, 130, address, width, motor]
    tx.append((~sum(tx[2:])) & 255)
    rx, raw = row['rx'], row['raw_unsigned']
    require(type(row['tx']) is list and all(type(v) is int and 0 <= v <= 255 for v in row['tx'])
            and row['tx'] == tx and sum(tx[2:]) & 255 == 255, '请求帧不是固定SYNC_READ')
    require(type(rx) is list and all(type(v) is int and 0 <= v <= 255 for v in rx)
            and len(rx) == 6 + width and rx[:5] == [255, 255, motor, width + 2, 0]
            and sum(rx[2:]) & 255 == 255 and type(raw) is int
            and 0 <= raw < 1 << (8 * width)
            and int.from_bytes(bytes(rx[5:5 + width]), 'little') == raw,
            '响应ID/LEN/error/checksum/原word不符')
    return row


def comparison_from_readouts(readouts):
    sync = next((row for row in readouts if row['operation']=='read-raw' and row['register']=='Present_Velocity'), None)
    ordinary = next((row for row in readouts if row['operation']=='read' and row['register']=='Present_Velocity'), None)
    if sync is None or ordinary is None:
        return {'status': 'NOT_AVAILABLE_MISSING_PV_OUTPUT', 'reason': '原非零/未知封锁优先；不为形成pair追加UART'}
    raw, decoded = sync['raw']['raw_unsigned'], ordinary['decoded_integer']
    expected = -(raw & 0x7fff) if raw & 0x8000 else raw
    return {'status': 'RECORDED_OUTPUTS_COMPARISON', 'sync_read_instruction': 0x82,
            'ordinary_read_instruction': 0x02, 'sync_raw_unsigned': raw,
            'ordinary_decoded_integer': decoded, 'ordinary_sign_magnitude_representation': expected,
            'representation_matches': decoded==expected,
            'sampling_same_time': False, 'physical_motion': 'UNKNOWN',
            'ordinary_wire_word': 'PENDING_ROOT_REVIEW_OF_COMPLETE_STRACE; NOT_INFERRED_FROM_STDOUT'}


def run_reads(m, io, tool, gate, report, go_check=lambda: None):
    require(not report.get('uart_locked_out'), '本批已粘性封锁UART')
    require(not report.get('read_sequence_started'), '固定三读只允许一次，不重跑或重试')
    report['read_sequence_started'] = True
    for motor, operation, register in READS:
        try:
            gate()
            trace = io.output / f'right8-{operation}-{register}-command_{io.sequence+1:04d}.strace'
            with trace.open('xb'):
                pass
            original_argv = [tool['path'], m.DEVICES['right'], str(motor), operation, register]
            argv = [STRACE['path'], *STRACE_FLAGS, '-o', str(trace), *original_argv]
            text = io.command(argv)
            io.check()
            require(io.child is None and not io.evidence_errors, 'child/原输出证据未闭合')
            info = trace.lstat()
            require(stat.S_ISREG(info.st_mode) and info.st_nlink==1, 'strace原件不是独立普通文件')
            raw_trace = trace.read_bytes()
            require(raw_trace and re.search(rb'(?:^|\n)[^\n]*\+\+\+ exited with 0 \+\+\+\n?\Z', raw_trace),
                    'strace原件未知或缺成功终态')
            row = {'side': 'right', 'motor_id': motor, 'name': 'base_front_wheel', 'operation': operation,
                   'register': register, 'command_sequence': io.sequence, 'original_argv': original_argv,
                   'trace': {'path': str(trace), 'sha256': hashlib.sha256(raw_trace).hexdigest(), 'bytes': len(raw_trace)},
                   'wire_frame_review': 'PENDING_ROOT_REVIEW_OF_COMPLETE_STRACE'}
            if operation=='read-raw':
                row['raw'] = parse_raw(m, text, m.DEVICES['right'], motor, register)
                value = row['raw']['raw_unsigned']
            else:
                match = re.fullmatch(r'read Present_Velocity@8 = (-?(?:0|[1-9][0-9]*))\n', text)
                require(match is not None, '普通READ stdout必须完整匹配right8 PV整数')
                value = int(match[1])
                require(-32767<=value<=32767, '普通PV解码整数越界')
                row['decoded_integer'] = value
            report['readouts'].append(row)
            report['comparison'] = comparison_from_readouts(report['readouts'])
            require(value==0, 'TE/PV非零，停止全部后续UART，请现场立即人工断电')
            go_check()
        except BaseException:
            report['uart_locked_out'] = True
            report['manual_power_cut_requested'] = True
            raise


def execute(config, m, h, config_ref, own_ref):
    require(isinstance(APPROVED_TOOL, dict), '新ELF未绑定，零启动拒绝')
    receipt = confirmation(m, config.get('field_confirmation'), time.time_ns())
    require(config['schema'] == 'BASE_FRONT_PV_COMPARE_CONFIG_R1', '只读配置schema错误')
    bindings = m.strict_json(m.verify_ref(config['bindings']).decode())
    require(bindings['schema'] == 'MAINTENANCE_BINDINGS_V1'
            and set(bindings['usb']) == set(m.SERIALS), '完整双端维护绑定缺失')
    for side in m.SERIALS:
        require(bindings['usb'][side]['device'] == m.DEVICES[side]
                and bindings['usb'][side]['serial'] == m.SERIALS[side], '固定USB目标不符')
    require(config['current_usb']==bindings['usb'], '本次USB stat/枚举未固定或与binding不符')
    diag = config['diagnostic_tool']
    require(isinstance(config.get('current_tool_stat'), dict) and config['current_tool_stat']==APPROVED_TOOL, '本次ELF完整stat未固定')
    require(config['strace']==STRACE, 'strace冻结引用错误')
    for ref in (own_ref, config['maintenance'], config['support'], bindings['tool'], diag, STRACE):
        require(isinstance(ref, dict), '必要工具/源码引用未绑定')
        require(any(h.same_reference(row, ref) for row in bindings['refs']), '必要完整引用未列入refs')
    require(bindings['tool']['path'] in bindings['binary_paths']
            and diag['path'] in bindings['binary_paths'] and STRACE['path'] in bindings['binary_paths'], 'proc门缺冻结f201或新工具')
    plan = make_plan(m, config, config_ref, own_ref, bindings)
    go = m.strict_json(m.verify_ref(config['approval']).decode())
    m.validate_go(go, plan, time.time_ns())
    h.host_identity(bindings)
    m.protected_and_refs(bindings)
    tool_identity(m, diag)
    output = m.artifact_path(config['output'], config['artifact_root'])
    output.mkdir(mode=0o750, parents=False, exist_ok=False)
    io = m.RealExecutor(bindings['tool'], output)
    io.process_snapshots = []
    report = {'status': 'FAIL', 'pid': os.getpid(), 'started_ns': time.time_ns(),
              'plan': plan, 'go': go, 'field_receipt': receipt, 'readouts': [], 'comparison': {'status': 'NOT_AVAILABLE_MISSING_PV_OUTPUT'}, 'errors': [],
              'uart_locked_out': False, 'manual_power_cut_requested': False,
              'register_writes': 0, 'motion_commands': 0,
              'stop_verification': 'NOT_FULL_17_MOTOR_STOP_VERIFICATION',
              'old_failures_unchanged': True, 'physical_motion': 'UNKNOWN',
              'child_scope': '仅直属strace真实wait/reap；b2子工具自然终态见strace，不伪称导出nested PID/wait',
              'position_targets_use': 'FROZEN_GO_IDENTITY_METADATA_ONLY_NO_POSITION_OPERATION'}
    previous = {}
    try:
        for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT):
            previous[sig] = signal.signal(sig, io.mark_signal)
        m.verify_ref(bindings['proc_helper'])
        spec = importlib.util.spec_from_file_location('readonly12_existing_proc', bindings['proc_helper']['path'])
        proc = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(proc)
        with contextlib.ExitStack() as stack:
            try:
                m.idle(io, bindings, proc)
                for side in m.SERIALS:
                    for name in ('rx_trust.lock', 'estop_latch.lock'):
                        ref = bindings['protection'][side][name]
                        for prefix in ([], ['sudo', '-n']):
                            require(io.command(prefix + ['fuser', ref['path']], allowed=(1,)) == '', '合作锁被占用')
                        lock = stack.enter_context(Path(ref['path']).open('rb'))
                        fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                        require(os.fstat(lock.fileno()).st_ino == ref['inode'], '合作锁inode变化')

                def gate():
                    io.check()
                    require(io.child is None and not io.evidence_errors, '前项child/证据失败')
                    confirmation(m, config['field_confirmation'], time.time_ns())
                    m.validate_go(go, plan, time.time_ns())
                    h.host_identity(bindings)
                    for ref in (config_ref, config['bindings'], config['approval'], own_ref,
                                config['maintenance'], config['support']):
                        m.verify_ref(ref)
                    m.protected_and_refs(bindings)
                    m.verify_ref(bindings['tool'], executable=True)
                    tool_identity(m, diag)
                    m.verify_ref(STRACE, executable=True)
                    m.idle(io, bindings, proc)
                    h.usb_check(m, io, bindings)
                    io.check()
                    # 所有非UART准备后再重核600秒现场及GO，紧邻唯一此次工具启动。
                    confirmation(m, config['field_confirmation'], time.time_ns())
                    m.validate_go(go, plan, time.time_ns())

                def go_check():
                    io.check()
                    confirmation(m, config['field_confirmation'], time.time_ns())
                    m.validate_go(go, plan, time.time_ns())
                run_reads(m, io, diag, gate, report, go_check)
            except BaseException as error:
                report['errors'].append(repr(error))
                report['uart_locked_out'] = report['manual_power_cut_requested'] = True
                print('爸爸，只读失败，停止后续UART，请现场立即人工断电；不重试：' + repr(error), flush=True)
            finally:
                # 失败后仅非UART后验；不调用原17电机snapshot/finish或任何恢复写。
                report['nonuart_post'] = []
                checks = [('host', lambda: h.host_identity(bindings)),
                          ('refs_protection', lambda: m.protected_and_refs(bindings)),
                          ('tool', lambda: tool_identity(m, diag)),
                          ('strace', lambda: m.verify_ref(STRACE, executable=True)),
                          ('idle', lambda: m.idle(io, bindings, proc, cleanup=True)),
                          ('usb', lambda: h.usb_check(m, io, bindings, cleanup=True))]
                for name, fn in checks:
                    try:
                        fn()
                        report['nonuart_post'].append({'name': name, 'status': 'PASS'})
                    except BaseException as error:
                        report['nonuart_post'].append({'name': name, 'status': 'FAIL', 'error': repr(error)})
                        report['errors'].append(name + ':' + repr(error))
    except BaseException as error:
        report['errors'].append(repr(error))
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)
        report.update(commands=io.commands, process_snapshots=io.process_snapshots,
                      received_signal=io.interrupted, evidence_errors=io.evidence_errors,
                      child_active=io.child is not None, finished_ns=time.time_ns())
        if io.interrupted is not None or io.child is not None or io.evidence_errors:
            report['errors'].append('信号/child/原件失败')
        try:
            io.journal.close()
        except BaseException as error:
            report['errors'].append('journal关闭:' + repr(error))
        report['status'] = 'PASS_BASE_FRONT_THREE_READ_CAPTURE' if not report['errors'] and len(report['readouts']) == 3 else 'FAIL'
        if report['status'] == 'FAIL':
            report['uart_locked_out'] = report['manual_power_cut_requested'] = True
        try:
            m.write_new(output / 'run.json', report)
        except BaseException as error:
            report['status'] = 'FAIL'
            report['uart_locked_out'] = report['manual_power_cut_requested'] = True
            report['errors'].append('run原件保存失败:' + repr(error))
            print('爸爸，原件保存失败，禁止后续UART，请现场人工断电：' +
                  json.dumps(report, ensure_ascii=False), flush=True)
    return 0 if report['status'] == 'PASS_BASE_FRONT_THREE_READ_CAPTURE' else 1


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', type=Path)
    parser.add_argument('--execute', action='store_true')
    args = parser.parse_args(argv)
    if args.config is None and not args.execute:
        print(json.dumps({'status': 'PLAN_ONLY', 'tool': APPROVED_TOOL, 'fixed_reads': READS,
                          'frozen_tool': FROZEN_TOOL, 'metadata': META, 'uart_tool_calls': 3, 'actions_started': 0,
                          'output_created': False, 'register_writes': 0, 'motion_commands': 0}, ensure_ascii=False))
        return 0
    if args.execute:
        require(isinstance(APPROVED_TOOL, dict), '当前ELF完整stat/USB/现场/新GO未绑定，读取config前零I/O拒绝')
    require(args.config is not None, '执行必须指定当次config')
    raw = args.config.read_bytes()
    draft = json.loads(raw)
    require(isinstance(APPROVED_TOOL, dict), '本轮新ELF尚未绑定，零启动拒绝')
    if args.execute:
        require(all(isinstance(draft.get(key), dict) for key in ('field_confirmation', 'approval', 'bindings', 'diagnostic_tool', 'current_tool_stat', 'current_usb')),
                '缺当次现场/GO/绑定引用，零启动拒绝')
    h = load_support(draft['support'])
    m = h.load_source(draft['maintenance'])
    config = m.strict_json(raw.decode())
    config_ref = {'path': str(args.config.resolve()), 'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}
    own_ref = own_reference()
    if not args.execute:
        bindings = m.strict_json(m.verify_ref(config['bindings']).decode())
        plan = make_plan(m, config, config_ref, own_ref, bindings)
        print(json.dumps({'status': 'PLAN_ONLY', 'plan': plan, 'plan_sha256': m.digest(plan),
                          'actions_started': 0, 'output_created': False}, ensure_ascii=False, indent=2))
        return 0
    return execute(config, m, h, config_ref, own_ref)


if __name__ == '__main__':
    try:
        sys.exit(main())
    except Exception as error:
        print('爸爸，right8三读对照前置拒绝：' + repr(error), file=sys.stderr)
        sys.exit(2)
