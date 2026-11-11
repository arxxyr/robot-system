"""仅用 Linux ARM64 PTY 验证维护编排、默认 S0 与发送器；不连接物理串口。

显式设置 ROBOT_TEST_S0_BIN、ROBOT_TEST_MIXED_BIN 后运行；普通单测跳过。
"""

import errno
import contextlib
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import pty
import select
import signal
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch


def load_script(name):
    scripts = Path(__file__).resolve().parents[1]
    if str(scripts) not in sys.path:
        sys.path.insert(0, str(scripts))
    spec = importlib.util.spec_from_file_location(f"pty_{name}", scripts / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def full_reference(path, *, executable=False):
    path = Path(path).resolve(strict=True)
    raw, info = path.read_bytes(), path.stat()
    reference = {
        "path": str(path), "sha256": hashlib.sha256(raw).hexdigest(), "bytes": len(raw),
        "uid": info.st_uid, "gid": info.st_gid, "mode": stat.S_IMODE(info.st_mode),
        "device": info.st_dev, "inode": info.st_ino, "mtime_ns": info.st_mtime_ns,
        "ctime_ns": info.st_ctime_ns, "nlink": info.st_nlink, "symlink": False,
    }
    if executable:
        if raw[:6] != b"\x7fELF\x02\x01" or int.from_bytes(raw[18:20], "little") != 183:
            raise AssertionError("PTY 集成必须使用同源 ARM64 ELF，不能伪造产物身份")
        reference["elf_machine"] = 183
    return reference


def lock_is_busy(path):
    # 独立 open description 会与同一测试进程已持有的 flock 发生真实冲突。
    with Path(path).open("rb") as competitor:
        try:
            fcntl.flock(competitor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        fcntl.flock(competitor, fcntl.LOCK_UN)
        return False


def packet(motor, status, payload=b""):
    body = bytes((motor, len(payload) + 2, status)) + payload
    return b"\xff\xff" + body + bytes(((~sum(body)) & 0xff,))


class ServoPty:
    """左八位置关节或右九混合电机协议夹具，只实现本次所用寄存器。"""

    def __init__(self, fail_enable=False, position_only=False):
        self.position_only = position_only
        self.master, self.slave = pty.openpty()
        self.path = os.ttyname(self.slave)
        if not self.path.startswith("/dev/pts/"):
            raise RuntimeError("本测试只允许新建 Linux PTY")
        self.fail_enable = fail_enable
        self.enabled = [0] * 10
        self.goals = [0] * 10
        self.positions = [2047.0] * 10
        self.goal_positions = [2047] * 10
        self.last_update = time.monotonic()
        self.enable_attempts = 0
        self.nonzero_writes = []
        self.position_writes = []
        self.stop_writes = 0
        self.error = None
        self.stop = threading.Event()
        self.worker = threading.Thread(target=self.serve, name="robot-pty-servo")
        self.worker.start()

    def advance(self):
        now = time.monotonic()
        for motor in (() if self.position_only else (7, 8, 9)):
            if self.enabled[motor]:
                raw = self.goals[motor]
                speed = -(raw & 0x7fff) if raw & 0x8000 else raw
                self.positions[motor] = (self.positions[motor] + speed * (now - self.last_update)) % 4096
        self.last_update = now

    def read(self, motor, register, width):
        self.advance()
        values = {
            0: 3, 1: 10, 3: 777, 18: 12, 33: int(motor >= 7 and not self.position_only),
            40: self.enabled[motor], 42: self.goal_positions[motor],
            46: self.goals[motor], 56: round(self.positions[motor]) % 4096,
            58: self.goals[motor] if self.enabled[motor] else 0,
        }
        if register not in values:
            raise AssertionError(f"未预期读取: ID{motor} addr{register}/{width}")
        return int(values[register]).to_bytes(width, "little")

    def write(self, motor, register, payload):
        self.advance()
        value = int.from_bytes(payload, "little")
        if register == 40:
            if value:
                self.enable_attempts += 1
                if self.fail_enable:
                    self.fail_enable = False
                    return 8
            self.enabled[motor] = value
            self.stop_writes += int(value == 0)
        elif register == 42:
            if motor >= 7 and not self.position_only:
                raise AssertionError("轮子不能接收位置目标")
            if value != 2047:
                raise AssertionError("原位目标被改变")
            self.position_writes.append((motor, value, time.monotonic()))
            self.goal_positions[motor] = value
            self.positions[motor] = float(value)
        elif register == 46:
            if motor < 7 or self.position_only:
                raise AssertionError("位置关节不能接收轮速目标")
            self.goals[motor] = value
            if value:
                self.nonzero_writes.append((motor, value, time.monotonic()))
        else:
            raise AssertionError(f"未授权写寄存器: ID{motor} addr{register}")
        return 0

    def respond(self, frame):
        if sum(frame[2:]) & 0xff != 0xff:
            raise AssertionError("请求校验和错误")
        motor, instruction, payload = frame[2], frame[4], frame[5:-1]
        if instruction == 1:
            return packet(motor, 0)
        if instruction == 2:
            return packet(motor, 0, self.read(motor, payload[0], payload[1]))
        if instruction == 3:
            return packet(motor, self.write(motor, payload[0], payload[1:]))
        if instruction == 0x82:
            register, width = payload[:2]
            return b"".join(packet(mid, 0, self.read(mid, register, width)) for mid in payload[2:])
        if instruction == 0x83:
            register, width = payload[:2]
            for offset in range(2, len(payload), width + 1):
                mid = payload[offset]
                status = self.write(mid, register, payload[offset + 1:offset + 1 + width])
                if status:
                    raise AssertionError("测试拒绝点仅用于普通Enable写")
            return b""
        raise AssertionError(f"未预期指令: {instruction:#x}")

    def serve(self):
        pending = bytearray()
        try:
            while not self.stop.is_set():
                readable, _, _ = select.select([self.master], [], [], 0.05)
                if not readable:
                    continue
                try:
                    chunk = os.read(self.master, 4096)
                except OSError as error:
                    if error.errno == errno.EIO:
                        continue
                    raise
                pending.extend(chunk)
                while len(pending) >= 4:
                    if pending[:2] != b"\xff\xff":
                        raise AssertionError(f"请求头错误: {pending[:8].hex()}")
                    size = pending[3] + 4
                    if len(pending) < size:
                        break
                    reply = self.respond(bytes(pending[:size]))
                    del pending[:size]
                    if reply:
                        os.write(self.master, reply)
        except BaseException as error:
            self.error = error

    def close(self):
        self.stop.set()
        self.worker.join(timeout=2)
        if self.worker.is_alive():
            raise AssertionError("PTY夹具未退出")
        os.close(self.master)
        os.close(self.slave)
        if self.error:
            raise self.error


@unittest.skipUnless(sys.platform == "linux" and os.uname().machine == "aarch64" and os.environ.get("ROBOT_TEST_S0_BIN")
                     and os.environ.get("ROBOT_TEST_MIXED_BIN"), "需要显式绑定同源Linux软件测试产物")
class MixedSenderIntegration(unittest.TestCase):
    def run_case(self, fail_enable, position_only=False):
        active = "left" if position_only else "right"
        passive = "right" if position_only else "left"
        daemon_bin = Path(os.environ["ROBOT_TEST_S0_BIN"]).resolve(strict=True)
        sender_bin = Path(os.environ["ROBOT_TEST_MIXED_BIN"]).resolve(strict=True)
        evidence_root = os.environ.get("ROBOT_TEST_EVIDENCE_DIR")
        temporary = None if evidence_root else tempfile.TemporaryDirectory(prefix="robot-mixed-pty-")
        directory = (Path(tempfile.mkdtemp(prefix="mixed-fail-" if fail_enable else "mixed-ok-", dir=evidence_root))
                     if evidence_root else Path(temporary.name)).resolve()
        maintenance = load_script("run_hardware_maintenance")
        proc_helper = load_script("run_post_e_h2")
        servo = ServoPty(fail_enable, position_only)
        executor = monitor = None
        monitoring_done, daemon_locks_seen = threading.Event(), threading.Event()
        lock_checks = {"left": 0, "right": 0, "active_reacquired_after_wait": 0, "errors": []}
        report = {}
        try:
            robot = f"mixed_pty_{os.getpid()}_{time.monotonic_ns()}"
            right_names = list(maintenance.MIXED_ARM_NAMES) + ["base_right_wheel", "base_front_wheel", "base_left_wheel"]
            left_names = [name.replace("right_", "left_", 1) for name in maintenance.MIXED_ARM_NAMES]
            left_names += ["head_motor_1", "head_motor_2"]
            joints = left_names if position_only else right_names
            hardware_id = "left_arm_full_with_head" if position_only else "right_arm_full_with_base"
            calibration = directory / "calibration.json"
            calibration.write_text(json.dumps({name: {"id": index, "drive_mode": 0, "homing_offset": 0,
                                                       "range_min": 0, "range_max": 4095}
                                               for index, name in enumerate(joints, 1)}))
            config = directory / "robot.toml"
            config.write_text(f'''[robot]
id = "{robot}"
config_version = "pty-only"
[[hardware]]
id = "{hardware_id}"
adapter = "feetech_sts3215"
auto_enable = false
feetech_bus2_mixed_raw = {str(not position_only).lower()}
address = "{servo.path}"
joints = {json.dumps(joints)}
motor_ids = {json.dumps(list(range(1, len(joints) + 1)))}
calibration_paths = [{json.dumps(str(calibration))}]
calibration_range_tolerances_raw = {json.dumps([20] * 8 if position_only else [20] * 6 + [0] * 3)}
''')
            passive_device = str(directory / "passive-no-uart")
            passive_config = directory / "passive.toml"
            passive_names = right_names if position_only else left_names
            passive_config.write_text('[[hardware]]\nadapter = "feetech_sts3215"\naddress = '
                                      + json.dumps(passive_device) + '\njoints = ' + json.dumps(passive_names)
                                      + '\nmotor_ids = ' + json.dumps(list(range(1, len(passive_names) + 1))) + '\n')
            devices = {active: servo.path, passive: passive_device}
            environment = {key: value for key, value in os.environ.items()
                           if not key.startswith("ROBOT_TEST_") and key not in
                           ("NOTIFY_SOCKET", "INVOCATION_ID", "WATCHDOG_USEC", "WATCHDOG_PID")}
            environment.update(ROBOT_LOG_FORMAT="json", RUST_LOG="info,s0_wheel_observation=debug", NO_COLOR="1")
            protection = {}
            # 两侧Dirty marker由真实revoke建立；本测试不手写Clean marker。
            for side in maintenance.SERIALS:
                trust = directory / side
                trust.mkdir(mode=0o750)
                latch = trust / "estop_latch"
                latch.write_text("1\n")
                Path(str(latch) + ".bak").write_text("1\n")
                for path in (latch, Path(str(latch) + ".bak")):
                    path.chmod(0o600 if side == active else 0o640)
                # rx-trust子命令不创建latch锁；仅为本次新PTY夹具预建稳定锁inode。
                (trust / "estop_latch.lock").touch(mode=0o600)
                command = [str(daemon_bin), "rx-trust", "revoke", "--robot-id", robot,
                           "--rx-trust-dir", str(trust), "--latch-path", str(latch)]
                marker = subprocess.run(command, env=environment, capture_output=True, text=True, timeout=5)
                (directory / f"{side}-revoke.log").write_text(marker.stdout + marker.stderr)
                self.assertEqual(marker.returncode, 0, marker.stdout + marker.stderr)
                protection[side] = {name: full_reference(trust / name) for name in
                                    ("rx_trust.lock", "estop_latch.lock", "rx_trust_marker",
                                     "rx_trust_marker.bak", "estop_latch", "estop_latch.bak")}
            daemon_ref, sender_ref = full_reference(daemon_bin, executable=True), full_reference(sender_bin, executable=True)
            helper_ref = full_reference(Path(proc_helper.__file__))
            bindings = {
                "schema": "MAINTENANCE_BINDINGS_V1", "tool": dict(maintenance.RIGHT_PULSE_WRITE),
                "host": {"hostname": os.uname().nodename, "uid": os.getuid(), "gid": os.getgid(),
                         "boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip()},
                "usb": {side: {"device": devices[side],
                                "serial": maintenance.SERIALS[side]} for side in maintenance.SERIALS},
                "refs": [daemon_ref, sender_ref, full_reference(calibration), helper_ref, full_reference(config)],
                "binary_paths": [str(daemon_bin), str(sender_bin)], "protection": protection,
                "proc_helper": helper_ref, "s0_daemon": daemon_ref, "supervised_demo": sender_ref,
                "s0_hardware_id": hardware_id,
            }
            with contextlib.ExitStack() as stack:
                stack.enter_context(patch.dict(os.environ, environment, clear=True))
                stack.enter_context(patch.object(maintenance, "DEVICES", devices))
                stack.enter_context(patch.object(maintenance, "MAINTENANCE_NAMESPACE", directory.parent))
                maintenance.configure_bus_mapping(passive_config if position_only else config,
                                                  config if position_only else passive_config)
                plan = maintenance.make_plan(bindings, directory / "run", "pty-fail" if fail_enable else "pty-ok",
                                             directory, **({"s0_position_hold": True} if position_only else {"s0_mixed_hold": True}))
                issued = time.time_ns()
                facts = {side: {**dict.fromkeys(maintenance.FACTS, True), "confirmed_at_ns": issued}
                         for side in maintenance.SERIALS}
                facts["right"].update(all_three_wheels_off_ground=True, base_stably_supported=True,
                    continuous_wheel_observation=True, base_confirmation_kind="CURRENT_SETUP_READY",
                    base_confirmation_scope=plan["scope"], base_confirmation_answer="PTY软件夹具；不代表现场事实或物理验收")
                if position_only:
                    facts["left"].update(position_hold_confirmation_scope=plan["scope"],
                        position_hold_confirmation_answer="PTY软件夹具；不是现场确认")
                go = {"schema": "MAINTENANCE_GO_V2", "issued_by": "root", "plan_sha256": maintenance.digest(plan),
                      "artifact_root": str(directory), "host": bindings["host"], "source": "PTY软件夹具",
                      "issued_at_ns": issued, "expires_at_ns": issued + 60_000_000_000, "physical": facts}
                maintenance.validate_go(go, plan, time.time_ns())
                with self.assertRaises(ValueError):
                    maintenance.validate_go({**go, "plan_sha256": "0" * 64}, plan, time.time_ns())
                with self.assertRaises(ValueError):
                    maintenance.validate_go({**go, "expires_at_ns": issued}, plan, time.time_ns())
                maintenance.write_new(directory / "plan.json", plan)
                maintenance.write_new(directory / "go.json", go)
                # 直接调用编排时，复用execute的原独占claim，不覆盖或重放GO。
                consumed = directory / "go.json.consumed"
                claim = {"plan_sha256": maintenance.digest(plan), "consumed_at_ns": time.time_ns()}
                maintenance.write_new(consumed, claim)
                with self.assertRaises(FileExistsError):
                    maintenance.write_new(consumed, claim)
                output = Path(plan["output"])
                output.mkdir(mode=0o750)
                executor = maintenance.RealExecutor(bindings["tool"], output)
                stack.callback(executor.journal.close)
                executor.process_snapshots = []
                engine = maintenance.ProbeEngine(executor, plan,
                    lambda: maintenance.validate_go(go, plan, time.time_ns()))
                locks = {}
                for side in maintenance.SERIALS:
                    for name in ("rx_trust.lock", "estop_latch.lock"):
                        handle = stack.enter_context(Path(protection[side][name]["path"]).open("rb"))
                        fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        locks[(side, name)] = handle
                maintenance.protected_and_refs(bindings)

                def fixture_snapshot():
                    # 只用于外层STOP51分类；没有额外UART连接或第二份协议驱动。
                    result = {}
                    for side in maintenance.SERIALS:
                        for motor in range(1, 9 if side == "left" else 10):
                            if side == active:
                                result[f"{side}:{motor}"] = {register: int.from_bytes(servo.read(motor, address, width), "little")
                                    for register, address, width in (("Operating_Mode", 33, 1), ("Torque_Enable", 40, 1),
                                        ("Present_Position", 56, 2), ("Present_Velocity", 58, 2),
                                        ("Goal_Position", 42, 2), ("Goal_Velocity", 46, 2))}
                            else:
                                result[f"{side}:{motor}"] = dict(Operating_Mode=int(side == "right" and motor >= 7), Torque_Enable=0,
                                    Present_Position=2047, Present_Velocity=0, Goal_Position=2047, Goal_Velocity=0)
                    return result

                report.update(schema="MAINTENANCE_PTY_INTEGRATION_V1", result="FAIL", plan=plan, go=go,
                    before=fixture_snapshot(), fixture_boundaries={"uart": "唯一ServoPty八位置关节" if position_only else "唯一ServoPty九电机，不是物理串口",
                        "passive_uart": "NOT_OPENED", "external_systemd_usb": "NOT_RUN_DIRECT_RUN_FIXTURE",
                        "external_stop51": "PTY_STATE_MODEL_NOT_UART", "physical_motion": "NOT_OBSERVED", "deployable": False})
                original_verify_locks = maintenance.verify_s0_daemon_locks

                def verified_daemon_locks(bound, identity, *args, **kwargs):
                    original_verify_locks(bound, identity, *args, **kwargs)
                    daemon_locks_seen.set()

                def watch_locks():
                    try:
                        while not monitoring_done.is_set():
                            for name in ("rx_trust.lock", "estop_latch.lock"):
                                if not lock_is_busy(protection[passive][name]["path"]):
                                    raise AssertionError("非活动侧保护锁在编排中被释放")
                                lock_checks[passive] += 1
                            process = executor.s0_daemon
                            stopping = report.get("s0_daemon", {}).get("signals_sent")
                            # 协作退出可先析构锁再exit；运行阶段要求活动侧锁持续busy。
                            if daemon_locks_seen.is_set() and not stopping and process is not None and process.poll() is None:
                                for name in ("rx_trust.lock", "estop_latch.lock"):
                                    busy = lock_is_busy(protection[active][name]["path"])
                                    if not busy and process.poll() is None and not report["s0_daemon"]["signals_sent"]:
                                        raise AssertionError("活动侧锁在本轮daemon运行阶段可被重新取得")
                                    lock_checks[active] += 1
                            monitoring_done.wait(0.002)
                    except BaseException as error:
                        lock_checks["errors"].append(repr(error))

                holder_calls = []
                def isolated_pty_holder(io, device, elevated, *, cleanup=False):
                    # 夹具自己一直持有PTY主从FD；这里只替身物理USB/fuser外层门。
                    self.assertIsNone(io.s0_daemon, "取回UART门不得早于daemon真实退出")
                    holder_calls.append((device, elevated))
                    return {"fixture": "PTY_HOLDER_NOT_PHYSICAL_USB", "device": device, "elevated": elevated}

                stack.enter_context(patch.object(maintenance, "verify_s0_daemon_locks", verified_daemon_locks))
                stack.enter_context(patch.object(maintenance, "inspect_holder", isolated_pty_holder))
                original_flock = fcntl.flock
                active_fds = {handle.fileno() for (side, _), handle in locks.items() if side == active}

                def ordered_flock(file, operation):
                    fd = file if isinstance(file, int) else file.fileno()
                    if fd in active_fds and operation & fcntl.LOCK_EX:
                        self.assertIsNone(executor.s0_daemon, "父程序不得在真实wait前取回活动侧锁")
                        self.assertTrue(report["s0_daemon"]["reaped"])
                        lock_checks["active_reacquired_after_wait"] += 1
                    return original_flock(file, operation)

                stack.enter_context(patch.object(maintenance.fcntl, "flock", ordered_flock))
                monitor = threading.Thread(target=watch_locks, name="robot-pty-lock-check")
                monitor.start()
                failure = None
                try:
                    maintenance.run_s0_mixed_hold(engine, go, proc_helper, locks, report)
                except BaseException as error:
                    failure = error
                    report["orchestration_error"] = repr(error)
                finally:
                    monitoring_done.set()
                    monitor.join(timeout=2)
                self.assertFalse(monitor.is_alive(), "锁观测线程未回收")
                self.assertEqual(lock_checks["errors"], [])
                self.assertGreater(lock_checks["left"], 0)
                self.assertTrue(daemon_locks_seen.is_set())
                self.assertGreater(lock_checks["right"], 0)
                self.assertEqual(lock_checks["active_reacquired_after_wait"], 2)
                for key, handle in locks.items():
                    ref = protection[key[0]][key[1]]
                    self.assertTrue(lock_is_busy(ref["path"]))
                    self.assertEqual(os.fstat(handle.fileno()).st_ino, Path(ref["path"]).stat().st_ino)
                self.assertIsNone(executor.child)
                self.assertIsNone(executor.s0_daemon)
                self.assertTrue(report["s0_daemon"]["reaped"])
                self.assertEqual(report["s0_daemon"]["exit_code"], 0)
                self.assertTrue(executor.process_snapshots)
                self.assertTrue(executor.process_snapshots[-1]["clean"])
                if executor.s0_post_ready:
                    self.assertEqual(holder_calls, [(servo.path, False), (servo.path, True)])
                    maintenance.verify_s0_protection(bindings, "final", active)
                else:
                    self.assertTrue(fail_enable, report.get("s0_cleanup_errors"))
                    self.assertTrue(engine.manual_cut)
                for side in maintenance.SERIALS:
                    for name in ("rx_trust_marker", "rx_trust_marker.bak", "estop_latch", "estop_latch.bak"):
                        self.assertEqual(Path(protection[side][name]["path"]).read_bytes(), b"1\n")
                self.assertIn("s0_sender_result", report, f"编排错误：{report.get('orchestration_error')}；清理错误：{report.get('s0_cleanup_errors')}")
                sender_report = report["s0_sender_result"]
                sender_command = next(row for row in executor.commands if row["argv"][0] == str(sender_bin))
                self.assertTrue(sender_command["reaped"])
                self.assertEqual(sender_command["exit_code"], 1 if fail_enable else 0)
                self.assertGreater(servo.enable_attempts, 0)
                self.assertTrue(all(value == 0 for value in servo.enabled[1:]))
                # 原编排确认真实退出及四锁取回后，才允许软件STOP51替身取值。
                post_reads = []
                if executor.s0_post_ready:
                    after = fixture_snapshot()
                    stop_values = {key: {register: row[register] for register in maintenance.STOP_REGS}
                                   for key, row in after.items()}
                    post_reads = [(key, register) for key in stop_values for register in maintenance.STOP_REGS]
                    report["fixture_stop51"] = maintenance.classify_stop_snapshot(stop_values,
                        child_active=executor.child is not None or executor.s0_daemon is not None)
                    self.assertEqual(len(post_reads), 51)
                    self.assertEqual(report["fixture_stop51"]["status"], "PASS")
                else:
                    self.assertEqual(post_reads, [])
                report["fixture_post_reads"] = post_reads
                report["manual_power_cut_required"] = engine.manual_cut
                if fail_enable:
                    self.assertIsNotNone(failure)
                    self.assertFalse(servo.nonzero_writes)
                    self.assertEqual(sender_report["result"], "FAIL")
                    # 保留原sender失败/人工处置事实，不为夹具改成成功。
                    self.assertIsInstance(sender_report["manual_power_cut_required"], bool)
                else:
                    self.assertIsNone(failure, repr(failure))
                    self.assertEqual(sender_report["result"], "SOFTWARE_PATH_PASS")
                    self.assertEqual(len(sender_report["cycles"]), 400)
                    self.assertTrue(sender_report["disable_retired"])
                    self.assertTrue(sender_report["torque_off_confirmed"])
                    self.assertTrue(sender_report["final_estop_published"])
                    self.assertEqual(sender_report["stop51"], "NOT_RUN")
                    self.assertFalse(sender_report["manual_power_cut_required"])
                    if position_only:
                        self.assertTrue(sender_report["hold_position_verified"])
                        self.assertEqual(len(sender_report["hold_samples"]), 400)
                        self.assertFalse(servo.nonzero_writes)
                        self.assertEqual({motor for motor, _, _ in servo.position_writes}, set(range(1, 9)))
                        self.assertGreater(len(servo.position_writes), 8)
                        self.assertTrue(all(raw == 2047 for _, raw, _ in servo.position_writes))
                        self.assertEqual(servo.goal_positions[1:9], [2047] * 8)
                        self.assertEqual(sender_report["hold_end_ns"] - sender_report["hold_start_ns"], 20_000_000_000)
                    else:
                        self.assertEqual(len(sender_report["motion_samples"]), 79)
                        self.assertEqual(len(sender_report["zero_samples"]), 3)
                        self.assertTrue(sender_report["motion_position_verified"])
                        self.assertEqual(sender_report["motion_velocity_raw"], "PASS")
                        self.assertTrue(servo.nonzero_writes)
                        self.assertTrue(all(motor == 8 and raw == 500 for motor, raw, _ in servo.nonzero_writes))
                        elapsed = servo.nonzero_writes[-1][2] - servo.nonzero_writes[0][2]
                        self.assertGreater(elapsed, 19.8)
                        self.assertLess(elapsed, 20.1)
                    report["result"] = "SOFTWARE_ORCHESTRATION_PASS_NOT_PHYSICAL"
                report["lock_checks"] = lock_checks
        finally:
            monitoring_done.set()
            if monitor is not None:
                monitor.join(timeout=2)
            if executor is not None:
                for process in (executor.child, executor.s0_daemon):
                    if process is not None and process.poll() is None:
                        process.send_signal(signal.SIGTERM)
                        try:
                            process.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait(timeout=3)
            try:
                report["lock_checks"] = lock_checks
                (directory / "integration.json").write_text(json.dumps(report, ensure_ascii=False, indent=2))
            finally:
                try:
                    servo.close()
                finally:
                    if temporary:
                        temporary.cleanup()

    def test_full_twenty_seconds_through_default_s0_and_pty(self):
        self.run_case(False)

    def test_enable_failure_stops_without_any_nonzero_command(self):
        self.run_case(True)

    def test_left_eight_positions_hold_twenty_seconds_through_default_s0_and_pty(self):
        self.run_case(False, position_only=True)

    def test_left_enable_failure_stops_without_velocity_command(self):
        self.run_case(True, position_only=True)


if __name__ == "__main__":
    unittest.main()
