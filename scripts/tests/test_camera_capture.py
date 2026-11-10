"""相机 helper 的纯夹具测试，只加载假的 cv2 模块，不打开硬件。"""

import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location("camera_capture", Path(__file__).resolve().parents[1] / "camera_capture.py")
capture = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(capture)


class Frame:
    ndim = 3
    shape = (1, 2, 3)
    dtype = "uint8"

    def tobytes(self, order):
        assert order == "C"
        return bytes([1, 2, 3, 4, 5, 6])


class Camera:
    def __init__(self, frame=None, fps=10.0, ok=True, convert_rgb=1.0):
        self.frame = frame or Frame()
        self.fps = fps
        self.ok = ok
        self.convert_rgb = convert_rgb
        self.released = False
        self.properties = []

    def isOpened(self):
        return True

    def set(self, key, value):
        self.properties.append((key, value))
        return True

    def get(self, key):
        return self.convert_rgb if key == 6 else self.fps

    def read(self):
        return self.ok, self.frame

    def release(self):
        self.released = True


def cv2_fixture(camera):
    return SimpleNamespace(
        VideoCapture=lambda *_args: camera,
        CAP_V4L2=1,
        CAP_PROP_FOURCC=2,
        CAP_PROP_FRAME_WIDTH=3,
        CAP_PROP_FRAME_HEIGHT=4,
        CAP_PROP_FPS=5,
        CAP_PROP_CONVERT_RGB=6,
    )


class CaptureTests(unittest.TestCase):
    args = SimpleNamespace(device="/fixture/not-a-camera", width=2, height=1, fps=10)

    def run_capture(self, camera, writer):
        with patch.object(capture, "write_all", side_effect=writer), patch.object(
            capture.time, "clock_gettime_ns", return_value=1234, create=True
        ), patch.object(capture.time, "CLOCK_MONOTONIC_RAW", 4, create=True):
            capture.capture(self.args, cv2_fixture(camera))

    def test_bgr_payload_header_and_stop_release(self):
        camera = Camera()
        output = []

        def writer(_fd, data):
            output.append(bytes(data))
            if len(output) == 2:
                raise capture.CaptureStopped

        with self.assertRaises(capture.CaptureStopped):
            self.run_capture(camera, writer)
        self.assertEqual(capture.HEADER.unpack(output[0]), (b"RCAM", 2, 1, 6, 1234))
        self.assertEqual(output[1], bytes([1, 2, 3, 4, 5, 6]))
        self.assertTrue(camera.released)

    def test_read_failure_does_not_reuse_an_old_frame(self):
        camera = Camera(ok=False)
        with self.assertRaisesRegex(RuntimeError, "读取失败"):
            self.run_capture(camera, lambda *_args: self.fail("失败后不得发送帧"))
        self.assertTrue(camera.released)

    def test_native_fourcc_is_preserved(self):
        camera = Camera()
        with self.assertRaises(capture.CaptureStopped):
            self.run_capture(camera, lambda *_args: capture.request_stop(None, None))
        self.assertNotIn(cv2_fixture(camera).CAP_PROP_FOURCC, [key for key, _ in camera.properties])
        self.assertTrue(camera.released)

    def test_disabled_bgr_conversion_is_rejected(self):
        camera = Camera(convert_rgb=0)
        with self.assertRaisesRegex(RuntimeError, "BGR 转换"):
            self.run_capture(camera, lambda *_args: self.fail("禁用 BGR 转换不得发送帧"))
        self.assertTrue(camera.released)

    def test_actual_dimensions_are_not_relabelled(self):
        frame = Frame()
        frame.shape = (2, 1, 3)
        camera = Camera(frame=frame)
        with self.assertRaisesRegex(RuntimeError, "实际格式"):
            self.run_capture(camera, lambda *_args: self.fail("非法格式不得发送帧"))
        self.assertTrue(camera.released)

    def test_actual_fps_is_checked(self):
        for fps in [0, float("nan"), 30]:
            camera = Camera(fps=fps)
            with self.assertRaisesRegex(RuntimeError, "实际帧率"):
                self.run_capture(camera, lambda *_args: self.fail("帧率不符不得发送帧"))
            self.assertTrue(camera.released)

    def test_partial_pipe_writes_complete_bytes(self):
        pieces = []

        def write(_fd, data):
            pieces.append(bytes(data[:2]))
            return min(2, len(data))

        with patch.object(capture.os, "write", side_effect=write):
            capture.write_all(10, b"abcdefg")
        self.assertEqual(b"".join(pieces), b"abcdefg")

    def test_isolated_venv_load_does_not_execute_pth_or_startup_hooks(self):
        with tempfile.TemporaryDirectory() as temporary:
            venv = Path(temporary)
            (venv / "pyvenv.cfg").write_text("", encoding="ascii")
            packages = venv / "lib" / f"python{sys.version_info.major}.{sys.version_info.minor}" / "site-packages"
            packages.mkdir(parents=True)
            (packages / "cv2.py").write_text("__version__ = 'fixture'\n", encoding="ascii")
            marker = venv / "hook-executed"
            hook = f"import pathlib; pathlib.Path({str(marker)!r}).write_text('executed'); print('SDK_VERSION: fixture')\n"
            (packages / "startup.pth").write_text(hook, encoding="utf-8")
            (packages / "sitecustomize.py").write_text(hook, encoding="utf-8")
            program = (
                "import runpy, sys\n"
                "loader = runpy.run_path(sys.argv[1])\n"
                "sys.executable = sys.argv[2]\n"
                "module = loader['load_opencv']()\n"
                "assert 'site' not in sys.modules\n"
                "print(module.__version__)\n"
            )
            result = subprocess.run(
                [sys.executable, "-I", "-S", "-B", "-u", "-c", program, str(Path(capture.__file__).resolve()), str(venv / "bin" / "python")],
                check=True, capture_output=True, text=True, timeout=5,
            )
            self.assertEqual(result.stdout, "fixture\n")
            self.assertEqual(result.stderr, "")
            self.assertFalse(marker.exists())

    def test_missing_venv_package_directory_is_reported(self):
        with tempfile.TemporaryDirectory() as temporary:
            venv = Path(temporary)
            executable = str(venv / "bin" / "python")
            with patch.object(capture.sys, "executable", executable):
                with self.assertRaisesRegex(RuntimeError, "缺少 venv 配置"):
                    capture.load_opencv()
                (venv / "pyvenv.cfg").write_text("", encoding="ascii")
                with self.assertRaisesRegex(RuntimeError, "包目录不存在.*site-packages"):
                    capture.load_opencv()


if __name__ == "__main__":
    unittest.main()
