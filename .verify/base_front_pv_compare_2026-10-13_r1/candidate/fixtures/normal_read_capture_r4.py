"""爸爸，固定右8→右9普通READ原件采集；默认只输出计划，不触UART。"""
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
import sys
import time

sys.dont_write_bytecode = True
SOURCE_SHA = 'd97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41'
SOURCE_BYTES = 50168
TOOL = {'path': '/home/loosqk/robot-checks/h2-serial-recovery-20260930-r1/readback-tool-r1/feetech_poke',
        'sha256': 'f2014ee4008311e7a8725235a39f0d1b3913391f17cbd977b7441f120539114b', 'bytes': 1469904}
STRACE = {'path': '/usr/bin/strace',
          'sha256': '5392f54d889a87341d49f970d91e21ab8c1f70a835ac9ead6ee4d17e532ae7f7', 'bytes': 1583504}
DEVICE = '/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C122340-if00'
FLAGS = ('-ttt', '-xx', '-s', '256', '-yy', '-e', 'trace=openat,close,read,write,ioctl')
MOTORS = (8, 9)
FIELD_SCOPE = '只读右8、右9速度原帧各一次；不启用电机、不改参数'
FIELD_FACTS = ('motor_supplies_on', 'continuous_watch', 'immediate_manual_power_cut',
               'arms_unloaded_supported', 'areas_clear')
FIELD_TTL_NS = 600_000_000_000


def require(value, message):
    if not value:
        raise ValueError(message)


def load_source(reference):
    require(reference['sha256'] == SOURCE_SHA and reference['bytes'] == SOURCE_BYTES,
            '必须复用冻结d97维护源码')
    path = Path(reference['path'])
    require(path.is_absolute() and path == path.resolve() and stat.S_ISREG(path.lstat().st_mode),
            '维护源码路径身份错误')
    raw = path.read_bytes()
    require(len(raw) == SOURCE_BYTES and hashlib.sha256(raw).hexdigest() == SOURCE_SHA,
            '维护源码SHA/bytes变化')
    spec = importlib.util.spec_from_file_location('normal_read_existing_maintenance', path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def confirmation(m, reference, now_ns):
    require(isinstance(reference, dict), '本轮现场回执缺失，零启动拒绝')
    raw = m.verify_ref(reference)
    receipt = m.strict_json(raw.decode())
    stamp = receipt['recorded_at_ns']
    require(type(stamp) is int and 0 <= now_ns - stamp <= FIELD_TTL_NS,
            '本轮现场回执过期或时间错误，零启动拒绝')
    require(receipt.get('scope') == FIELD_SCOPE and isinstance(receipt.get('answer'), str)
            and receipt['answer'].strip(), '不是本轮普通READ现场回执')
    require(isinstance(receipt.get('physical'), dict)
            and all(receipt['physical'].get(key) is True for key in FIELD_FACTS),
            '本轮供电/看护/断电能力/支撑空载/清场事实不完整')
    return receipt


def same_reference(reference, expected):
    return all(reference.get(key) == value for key, value in expected.items())


def host_identity(bindings):
    host = bindings['host']
    require(sys.platform == 'linux' and os.uname().machine == 'aarch64'
            and os.uname().nodename == host['hostname']
            and os.getuid() == os.geteuid() == host['uid']
            and os.getgid() == os.getegid() == host['gid']
            and Path('/proc/sys/kernel/random/boot_id').read_text().strip() == host['boot_id'],
            '当前主机/boot身份不符')


def usb_check(m, io, bindings, *, cleanup=False):
    for side in m.SERIALS:
        expected = bindings['usb'][side]
        m.usb_identity(io, side, expected, cleanup=cleanup)
        path = Path(m.DEVICES[side])
        require(path.stat().st_ino == expected['device_inode']
                and path.lstat().st_ino == expected['link_inode'], 'USB inode变化')


def run_reads(m, io, output, gate, readouts):
    # 此循环之外没有UART；任何异常直接离开，不补读第二项，也不重试。
    for motor in MOTORS:
        trace = output / f'right{motor}-normal-read.strace'
        require(not trace.exists() and not trace.is_symlink(), 'strace原件已存在')
        argv = [STRACE['path'], *FLAGS, '-o', str(trace), TOOL['path'], DEVICE,
                str(motor), 'read', 'Present_Velocity']
        gate()
        text = io.command(argv)
        io.check()
        require(io.child is None and not io.evidence_errors, 'child/证据未闭合')
        match = re.fullmatch(rf'read Present_Velocity@{motor} = (-?(?:0|[1-9][0-9]*))\n', text)
        require(match is not None, '普通READ stdout必须完整匹配固定目标PV整数')
        value = int(match.group(1))
        require(-32767 <= value <= 32767, 'PV解码整数越界')
        info = trace.lstat()
        require(stat.S_ISREG(info.st_mode) and info.st_nlink == 1, 'strace原件不是独立普通文件')
        raw = trace.read_bytes()
        require(raw and re.search(rb'(?:^|\n)[^\n]*\+\+\+ exited with 0 \+\+\+\n?\Z', raw),
                'strace原件缺失或未记录成功终态')
        readouts.append({'side': 'right', 'motor_id': motor, 'register': 'Present_Velocity',
                         'decoded_integer': value, 'command_sequence': io.sequence,
                         'trace': {'path': str(trace), 'bytes': len(raw),
                                   'sha256': hashlib.sha256(raw).hexdigest()},
                         'wire_frame_review': 'PENDING_ROOT_REVIEW_OF_COMPLETE_STRACE'})
        require(value == 0, 'PV非零，停止后续UART，请现场立即人工断电')


def execute(config, m, config_ref, own_ref):
    # 现场门必须先于OUT创建、Executor构造和任何child/UART。
    receipt = confirmation(m, config.get('field_confirmation'), time.time_ns())
    require(config['schema'] == 'NORMAL_READ_CAPTURE_CONFIG_R4', '采集配置schema错误')
    binding_ref = config['bindings']
    bindings = m.strict_json(m.verify_ref(binding_ref).decode())
    require(bindings['schema'] == 'MAINTENANCE_BINDINGS_V1'
            and same_reference(bindings['tool'], TOOL), '不是原维护绑定/冻结f201')
    require(config['strace'] == STRACE and set(bindings['usb']) == set(m.SERIALS), 'strace/双USB绑定错误')
    for side in m.SERIALS:
        require(bindings['usb'][side]['device'] == m.DEVICES[side]
                and bindings['usb'][side]['serial'] == m.SERIALS[side], 'USB固定目标身份错误')
    for reference in (own_ref, config['maintenance'], TOOL, STRACE):
        require(any(same_reference(row, reference) for row in bindings['refs']), '本轮必要引用未加入refs')
    require(TOOL['path'] in bindings['binary_paths'] and STRACE['path'] in bindings['binary_paths'],
            'proc门必须覆盖f201和strace')
    host_identity(bindings)
    output = m.artifact_path(config['output'], config['artifact_root'])
    output.mkdir(mode=0o750, parents=False, exist_ok=False)
    io = m.RealExecutor(bindings['tool'], output)
    io.process_snapshots = []
    report = {'status': 'FAIL', 'pid': os.getpid(), 'started_ns': time.time_ns(),
              'config': config_ref, 'source': own_ref, 'maintenance': config['maintenance'],
              'field_confirmation': config['field_confirmation'], 'field_receipt': receipt,
              'readouts': [], 'errors': [], 'register_writes': 0, 'motion_commands': 0,
              'stop_verification': 'NOT_FULL_17_MOTOR_STOP_VERIFICATION',
              'old_failures_unchanged': True,
              'child_scope': 'RealExecutor自然回收直属strace；f201终态存于strace原件，未导出nested PID/wait',
              'syscall_write_note': 'strace记录的write系统调用不等同于寄存器写，须按TTY FD和协议区分'}
    previous = {}
    try:
        for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT):
            previous[sig] = signal.signal(sig, io.mark_signal)
        m.verify_ref(bindings['proc_helper'])
        spec = importlib.util.spec_from_file_location('normal_read_existing_proc', bindings['proc_helper']['path'])
        proc_helper = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(proc_helper)
        with contextlib.ExitStack() as stack:
            try:
                m.protected_and_refs(bindings)
                m.idle(io, bindings, proc_helper)
                for side in m.SERIALS:
                    for name in ('rx_trust.lock', 'estop_latch.lock'):
                        reference = bindings['protection'][side][name]
                        for prefix in ([], ['sudo', '-n']):
                            require(io.command(prefix + ['fuser', reference['path']], allowed=(1,)) == '',
                                    '合作锁已有持有者')
                        lock = stack.enter_context(Path(reference['path']).open('rb'))
                        fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                        require(os.fstat(lock.fileno()).st_ino == reference['inode'], '合作锁inode变化')

                def gate():
                    io.check()
                    require(not io.evidence_errors and io.child is None, '前项child/证据失败')
                    confirmation(m, config['field_confirmation'], time.time_ns())
                    host_identity(bindings)
                    for reference in (config_ref, binding_ref, own_ref, config['maintenance']):
                        m.verify_ref(reference)
                    m.protected_and_refs(bindings)
                    m.verify_ref(bindings['tool'], executable=True)
                    m.verify_ref(STRACE, executable=True)
                    m.idle(io, bindings, proc_helper)
                    usb_check(m, io, bindings)
                    io.check()
                    # 非UART准备可能耗尽时效；最终重验紧邻此次UART启动。
                    confirmation(m, config['field_confirmation'], time.time_ns())

                run_reads(m, io, output, gate, report['readouts'])
            except BaseException as error:
                report['errors'].append(repr(error))
                print('爸爸，普通READ采集失败，停止后续UART，请现场立即断电；不重试：' + repr(error), flush=True)
            finally:
                # 无论采集成败，都只做非UART后验；不调用原17电机snapshot/finish。
                report['nonuart_post'] = []
                tasks = [('host', lambda: host_identity(bindings)),
                         ('refs_and_protection', lambda: m.protected_and_refs(bindings)),
                         ('tool', lambda: m.verify_ref(bindings['tool'], executable=True)),
                         ('strace', lambda: m.verify_ref(STRACE, executable=True)),
                         ('idle', lambda: m.idle(io, bindings, proc_helper, cleanup=True)),
                         ('usb', lambda: usb_check(m, io, bindings, cleanup=True))]
                for name, check in tasks:
                    try:
                        check()
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
            report['errors'].append('信号/child/证据失败')
        try:
            io.journal.close()
        except BaseException as error:
            report['errors'].append('journal关闭:' + repr(error))
        report['status'] = 'PASS_NORMAL_READ_CAPTURE_2' if not report['errors'] and len(report['readouts']) == 2 else 'FAIL'
        report['manual_power_cut_requested'] = report['status'] == 'FAIL'
        try:
            m.write_new(output / 'run.json', report)
        except BaseException as error:
            report['status'] = 'FAIL'
            report['errors'].append('run保存:' + repr(error))
            print(json.dumps(report, ensure_ascii=False), flush=True)
    return 0 if report['status'] == 'PASS_NORMAL_READ_CAPTURE_2' else 1


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', type=Path)
    parser.add_argument('--execute', action='store_true')
    options = parser.parse_args(argv)
    if not options.execute:
        print(json.dumps({'status': 'PLAN_ONLY', 'actions_started': 0, 'output_created': False,
                          'field_confirmation': None, 'fixed_motors': MOTORS, 'tool': TOOL,
                          'strace': STRACE, 'register': 'Present_Velocity', 'opcode': 2,
                          'cli_requests_per_successful_read': ['PING', 'READ Model_Number', 'READ Present_Velocity'],
                          'register_writes': 0, 'motion_commands': 0}, ensure_ascii=False))
        return 0
    require(options.config is not None, '执行必须指定本轮config')
    raw = options.config.read_bytes()
    # 先拒缺现场引用；不导入旧脚本，也不创建OUT或Executor。
    draft = json.loads(raw)
    require(isinstance(draft.get('field_confirmation'), dict), '本轮现场回执缺失，零启动拒绝')
    m = load_source(draft['maintenance'])
    config = m.strict_json(raw.decode())
    config_ref = {'path': str(options.config.resolve()), 'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}
    own_raw = Path(__file__).read_bytes()
    own_ref = {'path': str(Path(__file__).resolve()), 'sha256': hashlib.sha256(own_raw).hexdigest(), 'bytes': len(own_raw)}
    return execute(config, m, config_ref, own_ref)


if __name__ == '__main__':
    try:
        sys.exit(main())
    except Exception as error:
        print('爸爸，普通READ前置拒绝：' + repr(error), file=sys.stderr)
        sys.exit(2)
