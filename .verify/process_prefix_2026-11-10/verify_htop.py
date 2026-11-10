"""仅启动等待中的本进程夹具，核对 Orin htop 实际筛选；不访问硬件。"""
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import struct
import subprocess
import sys
import tempfile
import termios
import time


def capture(config, output):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 240, 0, 0))
    process = subprocess.Popen(['htop', '-F', 'robot-namecheck'], stdin=slave,
        stdout=slave, stderr=slave,
        env={**os.environ, 'TERM': 'xterm', 'HTOPRC': str(config), 'LC_ALL': 'C.UTF-8'})
    os.close(slave)
    chunks = []
    try:
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            if select.select([master], [], [], 0.1)[0]:
                chunks.append(os.read(master, 65536))
        os.write(master, b'q')
        process.wait(timeout=3)
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=3)
        os.close(master)
    raw = b''.join(chunks)
    output.write_bytes(raw)
    return re.sub(r'\x1b\[[0-?]*[ -/]*[@-~]', '', raw.decode(errors='replace'))


output = Path(sys.argv[1])
output.mkdir(exist_ok=True)
with tempfile.TemporaryDirectory(prefix='namecheck-') as temporary:
    root = Path(temporary)
    fixture = root / 'child.py'
    fixture.write_text('from pathlib import Path\nimport sys\n'
        'Path("/proc/self/comm").write_text("robot-namecheck")\n'
        'print("READY",flush=True)\nsys.stdin.read()\n')
    child = subprocess.Popen([sys.executable, '-I', '-S', str(fixture)],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    try:
        assert child.stdout.readline().strip() == 'READY'
        base = Path('/home/loosqk/.config/htop/htoprc').read_text()
        results = {}
        for merged in (0, 1):
            config = root / f'htop-{merged}.rc'
            config.write_text(re.sub(r'^show_merged_command=.*$',
                f'show_merged_command={merged}', base, flags=re.MULTILINE))
            text = capture(config, output / f'htop-{merged}.terminal')
            visible = bool(re.search(rf'\b{child.pid}\b', text))
            assert visible == bool(merged), (merged, visible, text)
            results[str(merged)] = {'fixture_pid': child.pid, 'pid_visible': visible}
        proc = Path('/proc') / str(child.pid)
        results.update(comm=proc.joinpath('comm').read_text().strip(),
            argv=proc.joinpath('cmdline').read_bytes().decode().split('\0')[:-1],
            exe=os.readlink(proc / 'exe'), hardware_access=False)
        print(json.dumps(results, ensure_ascii=False, indent=2))
    finally:
        child.stdin.close()
        child.wait(timeout=3)
