#!/usr/bin/env python3
"""固定 Python 启动的单相机 BGR8 采集；stdout 仅发送小帧头和像素。"""

import argparse
import math
import os
from pathlib import Path
import signal
import struct
import sys
import time


HEADER = struct.Struct("<4sIIIQ")
MAX_FRAME_BYTES = 16 * 1024 * 1024 - 4096


class CaptureStopped(BaseException):
    """停止请求会展开采集 finally，释放相机。"""


def request_stop(_signum, _frame):
    raise CaptureStopped


def write_all(fd, data):
    """管道可能分段写入；只在相机子进程中等待下游。"""
    remaining = memoryview(data)
    while remaining:
        written = os.write(fd, remaining)
        if written <= 0:
            raise OSError("相机管道写入未前进")
        remaining = remaining[written:]


def load_opencv():
    """只加载所选 venv 的包目录，不执行 site、.pth 或启动钩子。"""
    # 不能 resolve 解释器符号链接，否则会丢失明确选择的 venv 路径。
    venv = Path(sys.executable).parent.parent
    if not (venv / "pyvenv.cfg").is_file():
        raise RuntimeError(f"相机解释器缺少 venv 配置：{venv / 'pyvenv.cfg'}")
    packages = venv / "lib" / f"python{sys.version_info.major}.{sys.version_info.minor}" / "site-packages"
    if not packages.is_dir():
        raise RuntimeError(f"相机 venv 包目录不存在：{packages}")
    sys.path.insert(0, str(packages))
    import cv2

    return cv2


def capture(args, cv2):
    """保留设备原生输入格式，使用 OpenCV 的 BGR 转换并校验实际帧。"""
    camera = None
    try:
        camera = cv2.VideoCapture(args.device, cv2.CAP_V4L2)
        if not camera.isOpened():
            raise RuntimeError("相机打开失败")
        camera.set(cv2.CAP_PROP_FRAME_WIDTH, args.width)
        camera.set(cv2.CAP_PROP_FRAME_HEIGHT, args.height)
        camera.set(cv2.CAP_PROP_FPS, float(args.fps))
        if camera.get(cv2.CAP_PROP_CONVERT_RGB) != 1:
            raise RuntimeError("相机未启用 OpenCV 的 BGR 转换")
        actual_fps = camera.get(cv2.CAP_PROP_FPS)
        print(f"相机请求 {args.width}x{args.height}@{args.fps}，驱动报告 FPS={actual_fps}", file=sys.stderr)
        if not math.isfinite(actual_fps) or actual_fps <= 0 or abs(actual_fps - args.fps) > max(1.0, args.fps * 0.1):
            raise RuntimeError("相机实际帧率不在请求的 10%/1 FPS 容差内")
        while True:
            ok, frame = camera.read()
            stamp = time.clock_gettime_ns(time.CLOCK_MONOTONIC_RAW)
            if not ok:
                raise RuntimeError("相机读取失败")
            if frame.ndim != 3 or frame.shape != (args.height, args.width, 3) or str(frame.dtype) != "uint8":
                raise RuntimeError("相机实际格式与请求的 BGR8 宽高不符")
            pixels = frame.tobytes(order="C")
            if len(pixels) != args.width * args.height * 3:
                raise RuntimeError("相机像素字节数不符")
            write_all(sys.stdout.fileno(), HEADER.pack(b"RCAM", args.width, args.height, len(pixels), stamp))
            write_all(sys.stdout.fileno(), pixels)
    finally:
        if camera is not None:
            camera.release()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--device", required=True)
    parser.add_argument("--width", type=int, required=True)
    parser.add_argument("--height", type=int, required=True)
    parser.add_argument("--fps", type=int, required=True)
    args = parser.parse_args()
    if sys.platform != "linux":
        parser.error("真实采集仅支持 Linux")
    if args.width <= 0 or args.height <= 0 or args.fps <= 0 or args.width * args.height * 3 > MAX_FRAME_BYTES:
        parser.error("相机尺寸或帧率非法")
    signal.signal(signal.SIGTERM, request_stop)
    signal.signal(signal.SIGINT, request_stop)
    try:
        Path("/proc/self/comm").write_text("robot-camera", encoding="ascii")
        capture(args, load_opencv())
        return 0
    except (CaptureStopped, BrokenPipeError):
        return 0
    except Exception as error:
        print(f"相机采集失败：{error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
