"""爸爸，独立相机固定20秒连拍草稿；零电机/UART，原帧先留远端。"""
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time
from decimal import Decimal

OUT = Path('/home/loosqk/robot-checks/position-return-20261010-r1/head-reverse-r9/camera-record-r9')
CONFIGS = [
    ('sonix-usb-2_1', '/dev/video0', 'MJPG', 640, 480, 10, 201),
    ('sonix-usb-2_2', '/dev/video2', 'MJPG', 640, 480, 10, 201),
    ('realsense-043323051347-rgb', '/dev/video8', 'YUYV', 320, 180, 30, 601),
]
PATTERN = re.compile(r'cap dqbuf: (\d+) seq:\s*(\d+) bytesused: (\d+) ts: ([0-9.]+)')


def save(name, data):
    with (OUT / name).open('xb') as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())


def publish(name, value):
    save(name, (json.dumps(value, ensure_ascii=False, indent=2) + '\n').encode())


def receipt(name, argv, pid, started, mono, code, signals):
    row = dict(name=name, argv=argv, pid=pid, started_ns=started,
               started_monotonic_ns=mono, finished_ns=time.time_ns(),
               finished_monotonic_ns=time.monotonic_ns(), exit_code=code,
               wait_returncode=code, reaped=True, signals_sent=signals)
    for suffix in ('stdout', 'stderr'):
        path = OUT / (name + '.' + suffix)
        raw = path.read_bytes()
        row[suffix] = dict(path=str(path), bytes=len(raw), sha256=hashlib.sha256(raw).hexdigest())
    return row


def command(name, argv):
    started = time.time_ns()
    mono = time.monotonic_ns()
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    stdout, stderr = child.communicate()
    code = child.wait()
    save(name + '.stdout', stdout)
    save(name + '.stderr', stderr)
    return receipt(name, argv, child.pid, started, mono, code, [])


def buffers(text):
    rows = []
    for line in text.splitlines():
        match = PATTERN.search(line)
        if not match or not line.endswith(')'):
            continue
        error = 'error,' in line
        row = dict(sequence=int(match[2]), bytesused=int(match[3]),
                   timestamp_monotonic_seconds=match[4],
                   timestamp_monotonic_us=int(Decimal(match[4]) * 1_000_000),
                   buffer_error=error, timestamp_flags=line.split('(', 1)[-1].rstrip(')'))
        if row['bytesused'] and not error:
            assert 'ts-monotonic' in line and 'ts-src-soe' in line
        rows.append(row)
    return rows


def format_matches(text, config):
    _, _, fmt, width, height, fps, _ = config
    return (re.search(r'Width/Height\s*:\s*' + str(width) + '/' + str(height) + r'\s', text)
            and "Pixel Format      : '" + fmt + "'" in text
            and re.search(r'Frames per second:\s*' + str(fps) + r'\.000\s', text))


def main():
    OUT.mkdir(mode=0o700)
    commands = []
    captures = []
    errors = []
    before = {}
    for config in CONFIGS:
        name, device, *_ = config
        row = command(name + '-before', ['/usr/bin/v4l2-ctl', '-d', device,
                      '--get-fmt-video', '--get-parm', '--list-ctrls'])
        commands.append(row)
        text = (OUT / (name + '-before.stdout')).read_text()
        assert row['exit_code'] == 0 and format_matches(text, config)
        before[name] = text
        row = command(name + '-holders', ['fuser', device])
        commands.append(row)
        assert row['exit_code'] == 1 and not row['stdout']['bytes'] and not row['stderr']['bytes']
    for config in CONFIGS:
        name, device, fmt, width, height, fps, count = config
        argv = ['/usr/bin/v4l2-ctl', '-d', device, '--verbose', '--stream-mmap=3',
                '--stream-count=' + str(count), '--stream-poll', '--stream-to=' + str(OUT / (name + '.raw'))]
        started = time.time_ns()
        mono = time.monotonic_ns()
        with (OUT / (name + '-capture.stdout')).open('xb') as stdout, (OUT / (name + '-capture.stderr')).open('xb') as stderr:
            child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr)
        captures.append(dict(config=config, child=child, argv=argv, started=started, mono=mono))
    # 只在电机尚未启动时观察暖机日志。ready之后只等待相机退出，不在运动判据中工作。
    ready = False
    deadline = min(capture['mono'] for capture in captures) + 6_000_000_000
    try:
        while time.monotonic_ns() < deadline:
            current = []
            for capture in captures:
                name = capture['config'][0]
                valid = [row for row in buffers((OUT / (name + '-capture.stderr')).read_text())
                         if row['bytesused'] and not row['buffer_error']]
                current.append(dict(name=name, pid=capture['child'].pid,
                                    capture_started_monotonic_ns=capture['mono'],
                                    live=capture['child'].poll() is None,
                                    valid_frames=len(valid), first=valid[0] if valid else None,
                                    latest=valid[-1] if valid else None,
                                    span_us=valid[-1]['timestamp_monotonic_us'] - valid[0]['timestamp_monotonic_us'] if valid else 0))
            common_span_us = (min(row['latest']['timestamp_monotonic_us'] for row in current)
                              - max(row['first']['timestamp_monotonic_us'] for row in current)) if all(row['valid_frames'] for row in current) else 0
            if all(row['live'] and row['span_us'] >= 2_000_000 for row in current) and common_span_us >= 2_000_000:
                publish('ready.json', dict(status='THREE_CAMERA_VALID_WARMUP_READY',
                        recorded_monotonic_ns=time.monotonic_ns(), captures=current,
                        common_valid_span_us=common_span_us,
                        camera_control_writes=0, uart_operations=0))
                ready = True
                break
            if not all(row['live'] for row in current):
                break
            time.sleep(0.05)
    except BaseException as error:
        errors.append(dict(warmup_error=repr(error)))
    if not ready:
        errors.append('三capture未同时存活并取得至少2秒有效时间跨度，禁止启动电机')
    framesets = []
    for capture in captures:
        child = capture['child']
        signals = []
        try:
            code = child.wait(timeout=max(0, (capture['mono'] + 30_000_000_000 - time.monotonic_ns()) / 1e9))
        except subprocess.TimeoutExpired:
            child.terminate()
            signals.append('SIGTERM_ONLY_OWN_CAMERA_AFTER_30S')
            code = child.wait()
        name, device, fmt, width, height, fps, count = capture['config']
        commands.append(receipt(name + '-capture', capture['argv'], child.pid,
                        capture['started'], capture['mono'], code, signals))
        try:
            assert code == 0 and not signals
            raw = (OUT / (name + '.raw')).read_bytes()
            rows = buffers((OUT / (name + '-capture.stderr')).read_text())
            offset = 0
            valid = []
            for row in rows:
                # v4l2-ctl跳过ERROR buffer；错误行即使非零也不计入输出raw偏移。
                row['offset'] = offset
                row['raw_written_bytes'] = 0 if row['buffer_error'] else row['bytesused']
                body = raw[offset:offset + row['raw_written_bytes']]
                assert len(body) == row['raw_written_bytes']
                offset += row['raw_written_bytes']
                row['sha256'] = hashlib.sha256(body).hexdigest()
                if row['raw_written_bytes']:
                    if fmt == 'MJPG':
                        end = body.rfind(b'\xff\xd9')
                        assert body.startswith(b'\xff\xd8') and end >= 0 and not any(body[end + 2:])
                    else:
                        assert len(body) == width * height * 2
                    valid.append(row)
            assert offset == len(raw) and len(valid) == count
            assert all(b['sequence'] > a['sequence'] and b['timestamp_monotonic_us'] > a['timestamp_monotonic_us'] for a, b in zip(valid, valid[1:]))
            framesets.append(dict(name=name, device=device, format=fmt, width=width, height=height,
                             fps=fps, buffers=rows, valid_frames=len(valid), raw=dict(path=str(OUT / (name + '.raw')),
                             bytes=len(raw), sha256=hashlib.sha256(raw).hexdigest()),
                             timestamp_precision='V4L2 CLOCK_MONOTONIC SOE，日志微秒精度；非纳秒曝光精度'))
        except BaseException as error:
            errors.append(dict(camera=name, error=repr(error), raw_preserved=str(OUT / (name + '.raw'))))
    after = {}
    for config in CONFIGS:
        name, device, *_ = config
        row = command(name + '-after', ['/usr/bin/v4l2-ctl', '-d', device,
                      '--get-fmt-video', '--get-parm', '--list-ctrls'])
        commands.append(row)
        text = (OUT / (name + '-after.stdout')).read_text()
        after[name] = dict(format_fps_preserved=bool(row['exit_code'] == 0 and format_matches(text, config)),
                           control_read_values_equal=before[name] == text)
    publish('result.json', dict(status='PASS_CAMERA_ONLY' if not errors and all(v['format_fps_preserved'] for v in after.values()) else 'FAIL_CAMERA_ONLY',
            warmup_ready=ready, errors=errors, framesets=framesets, commands=commands,
            format_control_after=after, camera_control_writes=0, uart_operations=0, motor_actions=0))


if __name__ == '__main__':
    main()
