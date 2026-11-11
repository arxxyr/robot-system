#!/usr/bin/env -S uv run --no-project python
"""显式的只读sudo集成验证；仅创建本次无害non-dumpable子进程，不skip。"""
import json
import os
from pathlib import Path
import selectors
import subprocess
import sys
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import run_comm_fault_probe as driver


CHILD = """import ctypes,json,os,sys
sys.path.insert(0,sys.argv[1])
import run_comm_fault_probe as driver
assert ctypes.CDLL(None).prctl(4,0,0,0,0)==0
print(json.dumps(driver.read_process(os.getpid())),flush=True)
sys.stdin.buffer.read(1)
"""


def main():
    receipt = {"schema": "P7_PROC_PROBE_INTEGRATION_V1", "status": "FAIL",
               "scope": "仅本次无害child与固定sudo只读PID探针，无硬件/marker/服务", "errors": []}
    child = None
    try:
        driver.require(os.getuid() == os.geteuid() != 0, "集成验证必须从普通身份运行")
        child = subprocess.Popen([sys.executable, "-c", CHILD, str(Path(driver.__file__).parent)],
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 text=True, start_new_session=True)
        receipt["child_pid"] = child.pid
        with selectors.DefaultSelector() as selector:
            selector.register(child.stdout, selectors.EVENT_READ)
            driver.require(selector.select(timeout=5), "无害child未在5秒内提供就绪回执")
        own = driver.strict_json(child.stdout.readline())
        receipt["child_self_read"] = own
        driver.require(own["status"] == "observed", "child自读身份失败")
        basic = driver.read_process(child.pid, details=False)
        receipt["ordinary_basic"] = basic
        driver.require(basic["status"] == "observed", "普通身份无法取得最小PID身份")
        ordinary = driver.read_process(child.pid, expected=basic["identity"])
        receipt["ordinary_detail"] = ordinary
        driver.require(ordinary["status"] == "permission", "未真实复现non-dumpable拒读")
        # 只限定枚举集合为本次child；身份读取和sudo后端均为真实实现。
        with patch.object(Path, "iterdir", return_value=[Path("/proc") / str(child.pid)]):
            snapshot = driver.process_snapshot(child.pid, Path("/p7-harmless-integration-no-write"), [])
        receipt["snapshot"] = snapshot
        driver.require(snapshot["privileged_probe"]["exit_code"] == 0, "sudo只读探针失败")
        driver.require(snapshot["unreadable"] == [] and len(snapshot["related"]) == 1,
                       "相关child没有得到稳定关联身份")
        actual = snapshot["related"][0]
        driver.require(all(actual[key] == own["identity"][key]
                           for key in ("pid", "start_ticks", "boot_id", "pgid", "uids", "exe", "argv")),
                       "sudo读取与child自读身份不符")
        receipt["probe_validated"] = True
    except Exception as error:
        receipt["errors"].append(repr(error))
    finally:
        if child is not None:
            try:
                # 关闭输入使无害child自然结束，不向被测机器人进程发送信号。
                child.stdin.close()
                receipt["child_exit"] = child.wait(timeout=5)
                receipt["child_naturally_reaped"] = True
                driver.require(child.returncode == 0, "无害child非正常退出")
                try:
                    os.waitpid(child.pid, os.WNOHANG)
                except ChildProcessError:
                    receipt["no_second_waitable_child"] = True
                else:
                    raise ValueError("child未被实际回收")
            except Exception as error:
                receipt["errors"].append(repr(error))
            receipt["child_stderr"] = child.stderr.read() if child.poll() is not None else "未退出，禁止补造收尾"
            child.stdout.close()
            child.stderr.close()
    if receipt.get("probe_validated") and receipt.get("child_naturally_reaped") and not receipt["errors"]:
        receipt["status"] = "PASS"
    print(json.dumps(receipt, ensure_ascii=False, indent=2))
    return 0 if receipt["status"] == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
