"""仅软件自有 child 实验；不调用 H2 执行、服务或 UART。"""

import errno
import hashlib
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import time
from unittest.mock import patch

sys.dont_write_bytecode = True
BASE = Path(__file__).resolve().parent
sys.path.insert(0, str(BASE / "source"))
import run_post_e_h2 as driver

START = time.monotonic_ns()
DEADLINE = START + 120_000_000_000
HISTORICAL_H2_PGID = 568217
CHILD_CODE = "import sys; print('ready', flush=True); sys.stdin.buffer.read(1)"
result = {
    "schema": "PROC_SHORT_CHILD_ONCE_V1",
    "status": "RUNNING",
    "scope": "仅软件；不授权或访问 H2、UART、服务、硬件动作",
    "source_git_head": "35298f57be983e2fdae0c2ef567fa800b9457b69",
    "started_ns": time.time_ns(),
    "owner_pid": os.getpid(),
    "owner_uid": os.getuid(),
    "owner_pgid": os.getpgrp(),
    "owner_identity": driver.process_identity(os.getpid()),
    "boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
    "native_unrelated_source_head": "a96c34ae26b56fc9e8d2deef17c3b829c3d87d0d",
    "source_export_identity": json.loads((BASE / "source_identity.json").read_text()),
    "children": [],
    "natural_trials": [],
    "controlled": None,
    "errors": [],
    "instrumentation_boundary": "自然组不拦截读取；受控组仅在真实 readlink 前同步正常退出、限定枚举自有 child；不伪造 errno/字段，不称自然调度",
}


def save():
    temporary = BASE / "result.json.tmp"
    temporary.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    temporary.replace(BASE / "result.json")


def check_time():
    if time.monotonic_ns() >= DEADLINE:
        raise TimeoutError("120秒实验预算到期；不再启动 child")


def ppid(pid):
    raw = Path(f"/proc/{pid}/stat").read_text()
    return int(raw[raw.rfind(")") + 2:].split()[1])


def start_child(label):
    check_time()
    row = {"label": label, "argv": [sys.executable, "-c", CHILD_CODE],
           "started_ns": time.time_ns(), "reaped": False, "signals_sent": []}
    result["children"].append(row)
    save()
    child = subprocess.Popen(row["argv"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, start_new_session=True)
    row["pid"] = child.pid
    row["start_new_session"] = True
    if not select.select([child.stdout], [], [], 5)[0]:
        child.stdin.close()
        finish_child(child, row)
        raise TimeoutError("自有 child ready 超时")
    ready = child.stdout.readline()
    if ready != b"ready\n":
        child.stdin.close()
        finish_child(child, row)
        raise ValueError("自有 child 未给真实 ready 握手")
    row["ready"] = ready.decode()
    row["identity"] = driver.process_identity(child.pid)
    row["ppid"] = ppid(child.pid)
    assert row["ppid"] == os.getpid()
    assert row["identity"]["pgid"] == child.pid != HISTORICAL_H2_PGID
    assert os.getuid() in row["identity"]["uids"]
    save()
    return child, row


def finish_child(child, row):
    if not child.stdin.closed:
        child.stdin.close()
    try:
        row["exit_code"] = child.wait(timeout=5)
    except subprocess.TimeoutExpired:
        row["signals_sent"].append("SIGTERM")
        child.terminate()
        try:
            row["exit_code"] = child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            row["signals_sent"].append("SIGKILL")
            child.kill()
            row["exit_code"] = child.wait(timeout=5)
        result["errors"].append("无害自有child未按正常握手退出，已结束实验")
    row.update(reaped=True, finished_ns=time.time_ns())
    stdout, stderr = child.stdout.read(), child.stderr.read()
    row.update(stdout_tail=stdout.decode(errors="replace"), stderr=stderr.decode(errors="replace"))
    child.stdout.close()
    child.stderr.close()
    try:
        unexpected = os.waitpid(child.pid, os.WNOHANG)
        row["second_wait"] = {"unexpected": unexpected}
        raise ValueError("真实 wait 后仍有可回收 child")
    except ChildProcessError as error:
        row["second_wait"] = {"errno": error.errno, "error": repr(error)}
        assert error.errno == errno.ECHILD
    save()
    assert row["exit_code"] == 0 and row["signals_sent"] == [] and stderr == b""


try:
    assert sys.platform == "linux" and hasattr(os, "WNOWAIT")
    assert os.getuid() == 1000
    for source in result["source_export_identity"]["files"]:
        assert hashlib.sha256((BASE / "source" / source["path"]).read_bytes()).hexdigest() == source["sha256"]
    for number in range(32):
        check_time()
        child, row = start_child(f"natural-{number:02d}")
        trial = {"child_pid": child.pid, "reads": 0, "statuses": {}, "exe_enoent_rows": []}
        result["natural_trials"].append(trial)
        try:
            # 正常EOF退出；此组不拦截任何 proc/syscall。
            child.stdin.close()
            for _ in range(32):
                observed = driver.read_process(child.pid, expected=row["identity"])
                trial["reads"] += 1
                status = observed["status"]
                trial["statuses"][status] = trial["statuses"].get(status, 0) + 1
                if observed.get("error_errno") == errno.ENOENT and observed.get("error_filename") == "exe":
                    trial["exe_enoent_rows"].append(observed)
                done = os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOWAIT | os.WNOHANG)
                if done is not None:
                    trial["nonreaping_exit"] = {"pid": done.si_pid, "code": done.si_code, "status": done.si_status}
                    break
        finally:
            finish_child(child, row)
        if result["errors"]:
            raise RuntimeError("自然组异常，停止实验")
    child, row = start_child("controlled-exe-read-point")
    controlled = {"child_pid": child.pid, "reads": [], "actual_waitid": [], "synchronization_points": 0}
    result["controlled"] = controlled
    real_readlink = os.readlink
    real_iterdir = Path.iterdir

    def at_readlink(name, *, dir_fd=None):
        if name == "exe" and dir_fd is not None and real_readlink(f"/proc/self/fd/{dir_fd}") == f"/proc/{child.pid}":
            controlled["synchronization_points"] += 1
            assert controlled["synchronization_points"] == 1
            child.stdin.close()
            done = os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOWAIT)
            controlled["actual_waitid"].append({"pid": done.si_pid, "code": done.si_code, "status": done.si_status})
            assert (done.si_pid, done.si_code, done.si_status) == (child.pid, os.CLD_EXITED, 0)
        # ENOENT由此真实Linux readlink产生；不raise或返回伪造数据。
        return real_readlink(name, dir_fd=dir_fd)

    def only_owned_child(path):
        return iter([Path("/proc") / str(child.pid)]) if path == Path("/proc") else real_iterdir(path)

    def recording_reader(pid, expected=None, details=True, proc=Path("/proc")):
        observed = driver.read_process(pid, expected=expected, details=details, proc=proc)
        controlled["reads"].append({"details": details, "expected": expected, "result": observed})
        return observed

    try:
        with patch.object(driver.os, "readlink", side_effect=at_readlink), patch.object(Path, "iterdir", only_owned_child):
            controlled["snapshot"] = driver.process_snapshot(HISTORICAL_H2_PGID, BASE / "unused-hardware-output", [],
                                                             reader=recording_reader)
        snapshot = controlled["snapshot"]
        assert snapshot["related"] == [] and len(snapshot["unreadable"]) == 1
        failure = snapshot["unreadable"][0]
        assert (failure["pid"], failure["error_errno"], failure["error_filename"]) == (child.pid, errno.ENOENT, "exe")
        assert driver._PROC_READER["_stable"](row["identity"], failure["before"])
        assert failure["remaining_stat"]["start_ticks"] == row["identity"]["start_ticks"]
        assert failure["remaining_stat"]["pgid"] == child.pid
        assert snapshot["clean"] is False
        controlled["interpretation"] = "同UID、不同于历史H2组的已知自有child正常退出，在真实exe读点得到内核ENOENT且stat仍同代次；受控残余Z，不证明历史R窗口或585588归属"
    finally:
        finish_child(child, row)
    result["natural_exe_enoent_count"] = sum(len(x["exe_enoent_rows"]) for x in result["natural_trials"])
    result["status"] = "PASS"
except BaseException as error:
    result["status"] = "FAIL"
    result["errors"].append(repr(error))
finally:
    result["finished_ns"] = time.time_ns()
    result["elapsed_ns"] = time.monotonic_ns() - START
    result["source_export_unchanged"] = all(hashlib.sha256((BASE / "source" / source["path"]).read_bytes()).hexdigest() == source["sha256"]
                                            for source in result["source_export_identity"]["files"])
    save()
print(json.dumps({"status": result["status"], "children": len(result["children"]),
                  "naturally_reaped": sum(x.get("reaped") and x.get("exit_code") == 0 and not x["signals_sent"] for x in result["children"]),
                  "natural_exe_enoent_count": result.get("natural_exe_enoent_count"),
                  "controlled": None if result["controlled"] is None else result["controlled"].get("snapshot"),
                  "errors": result["errors"], "elapsed_ns": result["elapsed_ns"]}, ensure_ascii=False))
raise SystemExit(0 if result["status"] == "PASS" else 1)
