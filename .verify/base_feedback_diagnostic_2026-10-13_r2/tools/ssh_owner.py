"""本次 H2 本地 SSH 持有者：只观察和真实 wait，绝不按超时或信号终止 child。

输出写入脚本旁全新 <label>.owner 目录；status.json 可周期读取。
使用 uv 管理的解释器直接启动；不要在外面再套会自动 kill 的 timeout/gate。
本地 SSH 的 exit/reaped 不证明远端 driver 或控制进程已退出，须独立后验。
"""

import argparse
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import time


def utc():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def file_digest(stream):
    digest = hashlib.sha256()
    size = 0
    while chunk := stream.read(1024 * 1024):
        digest.update(chunk)
        size += len(chunk)
    return {"sha256": digest.hexdigest(), "bytes": size}


def save(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    with temporary.open("w") as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2, allow_nan=False)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    temporary.replace(path)


def run(options):
    command = options.command[1:] if options.command[:1] == ["--"] else options.command
    if not command or not re.fullmatch(r"[A-Za-z0-9_-]+", options.label):
        raise ValueError("要求非空 argv 与简单唯一 label")
    if not math.isfinite(options.observe_seconds) or not 0.05 <= options.observe_seconds <= 60:
        raise ValueError("观察间隔须在 0.05 至 60 秒之间；不是运行超时")
    directory = Path(__file__).resolve().parent / (options.label + ".owner")
    directory.mkdir()
    log = directory / "child.log"
    state = {"schema": "P7_H2_LOCAL_SSH_OWNER_V1", "phase": "PREPARING", "status": "RUNNING",
             "owner_pid": os.getpid(), "command": command, "pid": None,
             "started_utc": utc(), "started_monotonic_ns": time.monotonic_ns(),
             "observe_seconds": options.observe_seconds, "wait_returncode": None,
             "reaped": False, "owner_sent_signals": [], "received_signals": {},
             "errors": [], "observations": 0, "remote_state": "NOT_VERIFIED",
             "helper_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    errors = {}

    def error(kind, exception):
        key = (kind, repr(exception))
        row = errors.setdefault(key, {"kind": kind, "error": repr(exception), "count": 0,
                                      "first_utc": utc()})
        row["count"] += 1
        row["last_utc"] = utc()
        state["errors"] = list(errors.values())

    def persist(name):
        try:
            save(directory / name, state)
        except BaseException as exception:
            # 证据 IO 失败不释放仍存活的 child；最终结果保留失败。
            error("evidence:" + name, exception)

    def receive(signum, frame):
        name = signal.Signals(signum).name
        row = state["received_signals"].setdefault(name, {"count": 0, "first_utc": utc()})
        row["count"] += 1
        row["last_utc"] = utc()
        # 不抛异常、不转发信号；继续等待实际 SSH child 结束。

    def observe():
        state["observations"] += 1
        state["observed_utc"] = utc()
        state["elapsed_ns"] = time.monotonic_ns() - state["started_monotonic_ns"]
        try:
            state["log_bytes_observed"] = log.stat().st_size
        except OSError as exception:
            error("log_stat", exception)
        persist("status.json")

    def wait_owned(child):
        while not state["reaped"]:
            try:
                code = child.wait(timeout=options.observe_seconds)
                # 仅实际 wait 返回后写 reaped；负值仍保留为信号结束。
                state.update(wait_returncode=code, reaped=True, phase="REAPED",
                             child_end_class="EXIT_STATUS" if code >= 0 else "SIGNAL",
                             child_signal=(-code if code < 0 else None), finished_utc=utc())
            except subprocess.TimeoutExpired:
                pass  # 只更新观察回执；无总超时，更无 terminate/kill。
            except BaseException as exception:
                error("wait", exception)
                # 异常也保持所有权，避免失败重试的忙循环。
                try:
                    time.sleep(options.observe_seconds)
                except BaseException as sleep_error:
                    error("wait_backoff", sleep_error)
            observe()

    previous_handlers = {}
    child = None
    input_stream = None
    output_stream = None
    try:
        for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT):
            previous_handlers[signum] = signal.signal(signum, receive)
        output_stream = log.open("xb", buffering=0)
        if options.input is not None:
            path = options.input
            if not path.is_absolute() or path != path.resolve():
                raise ValueError("input 要求规范绝对路径")
            descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
            input_stream = os.fdopen(descriptor, "rb")
            metadata = os.fstat(input_stream.fileno())
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
                raise ValueError("input 不是独立普通文件")
            state["input"] = {"path": str(path), **file_digest(input_stream)}
            input_stream.seek(0)
        # 启动前先要求至少一份可落盘回执，IO 失败时不启动。
        save(directory / "status.json", state)
        if state["received_signals"]:
            raise ValueError("启动前已收到退出信号，拒绝启动 child")
        child = subprocess.Popen(command, stdin=input_stream if input_stream else subprocess.DEVNULL,
                                 stdout=output_stream, stderr=subprocess.STDOUT,
                                 start_new_session=True)
        state.update(pid=child.pid, phase="WAITING", launched_utc=utc(),
                     start_new_session=True, child_session_id=child.pid)
        persist("launch.json")
        wait_owned(child)
    except BaseException as exception:
        error("owner", exception)
    finally:
        if child is not None and not state["reaped"]:
            # 任意可处理的 owner 异常都不能成为丢弃/终止 SSH 的理由。
            wait_owned(child)
        for stream in (input_stream, output_stream):
            if stream is not None:
                try:
                    if stream is output_stream:
                        os.fsync(stream.fileno())
                    stream.close()
                except BaseException as exception:
                    error("close", exception)
        for signum, previous in previous_handlers.items():
            signal.signal(signum, previous)

    try:
        with log.open("rb") as stream:
            state["log"] = {"path": str(log), **file_digest(stream)}
    except BaseException as exception:
        error("log_digest", exception)
    state["phase"] = "REAPED" if state["reaped"] else "NOT_LAUNCHED"
    state["status"] = "PASS" if state["reaped"] and state["wait_returncode"] == 0 and not errors else "FAIL"
    state["finished_utc"] = utc()
    state["elapsed_ns"] = time.monotonic_ns() - state["started_monotonic_ns"]
    persist("status.json")
    if errors:
        state["status"] = "FAIL"
    persist("result.json")
    # 落盘失败不能通过返回码伪装成成功；最终打印只是摘要。
    if errors:
        state["status"] = "FAIL"
    try:
        print(json.dumps(state, ensure_ascii=False), flush=True)
    except (BrokenPipeError, OSError):
        pass
    code = state["wait_returncode"]
    return (code if code is not None and code > 0 else 128 - code if code is not None and code < 0
            else 1 if state["status"] != "PASS" else 0)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--label", required=True)
    parser.add_argument("--input", type=Path)
    parser.add_argument("--observe-seconds", type=float, default=5)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    return run(parser.parse_args(argv))


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError) as exception:
        print(json.dumps({"status": "FAIL", "phase": "PRELAUNCH_REFUSED", "error": repr(exception)},
                         ensure_ascii=False), file=sys.stderr)
        raise SystemExit(2)
