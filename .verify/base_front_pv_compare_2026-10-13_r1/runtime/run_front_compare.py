"""爸爸，只绑定已核实的 b2 stat，原只读候选与安全门保持原字节。"""
import hashlib
import importlib.util
from pathlib import Path
import stat
import sys

sys.dont_write_bytecode = True
CANDIDATE = Path('/home/loosqk/robot-checks/base-front-pv-compare-r1/base_front_pv_compare_r1.py')
EXPECTED_SHA = 'd6b74da1ec4a587a02366c4c6fa9647e0ae5abba9622b3021e459a01f8cb3886'
EXPECTED_BYTES = 21849
APPROVED_TOOL = {'path': '/home/loosqk/robot-checks/xlerobot-sync-zero-software-20261013/quality/frozen/feetech_poke', 'sha256': 'b2bfedb1bc420c9957a276cd69f36f5fd47c66d0b7aaa519cf3375e257eabd94', 'bytes': 1507224, 'uid': 1000, 'gid': 1000, 'mode': 509, 'device': 66305, 'inode': 17173933, 'mtime_ns': 1791872617842665275, 'ctime_ns': 1791872617842665275, 'nlink': 1, 'symlink': False, 'elf_machine': 183}


def main():
    raw = CANDIDATE.read_bytes()
    if (CANDIDATE != CANDIDATE.resolve() or not stat.S_ISREG(CANDIDATE.lstat().st_mode)
            or len(raw) != EXPECTED_BYTES or hashlib.sha256(raw).hexdigest() != EXPECTED_SHA):
        raise ValueError('固定只读候选身份改变，拒绝启动')
    spec = importlib.util.spec_from_file_location('front_compare_fixed_candidate', CANDIDATE)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.APPROVED_TOOL = APPROVED_TOOL
    return module.main()


if __name__ == '__main__':
    try:
        sys.exit(main())
    except Exception as error:
        print('爸爸，固定入口前置拒绝：' + repr(error), file=sys.stderr)
        sys.exit(2)
