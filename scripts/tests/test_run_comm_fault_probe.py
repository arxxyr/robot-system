"""H2 驱动离线自测；仅临时文件、fake 后端与无害 Python 子进程。"""
from __future__ import annotations

import argparse
import copy
from dataclasses import replace
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import comm_fault_report as report
import run_comm_fault_probe as driver


def write_json(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False))


def ack(request_id, dry=False):
    return {"ok": True, "kind": "ack", "request_id": request_id,
            "dry_run": dry, "cleared_at_ns": 10}


def rt_fixture(side="left"):
    profile = report.arm_profile(side)
    processes = []
    for role, pid in (("tool", 100), ("s0", 101), ("gateway", 102)):
        threads = [{"tid": pid, "comm": role, "policy": 0, "priority": 0, "cpus": [0, 1, 2, 3]}]
        if role == "s0":
            threads.append({"tid": 103, "comm": "s0_serial_io_th", "policy": 1, "priority": 70, "cpus": [profile.serial_cpu]})
        tids = sorted(t["tid"] for t in threads)
        processes.append({"role": role, "validation_error": None, "snapshot": {
            "pid": pid, "tids_before": tids, "tids_after": tids, "threads": threads, "errors": []}})
    return {"started_ns": 1, "finished_ns": 2, "passed": True, "processes": processes,
            "profile": {"serial_cpu": profile.serial_cpu, "serial_policy": 1, "serial_priority": 70,
                        "ordinary_policy": 0, "ordinary_priority": 0, "ordinary_cpus": [0, 1, 2, 3]}}


def boundary(seq, mode, now):
    return {"sequence": seq, "mode": mode, "started_ns": now, "finished_ns": now+1, "error": None,
            "snapshot": {"request_seq": seq, "receipt_seq": seq, "request_mode": f"Some({mode})",
                         "mode": f"Some({mode})", "status": "Disarmed" if mode == "Disarm" else "Ready",
                         "armed_seq": 0 if mode == "Disarm" else seq, "ready_ns": now+1,
                         "request_deadline_ns": now+100_000, "failure": "None"}}


def report_fixture(root, side="left"):
    """合成报告只测试解析/判据，不模拟或宣称真实电机时序。"""
    profile = report.arm_profile(side)
    evidence = root / profile.evidence_directory; evidence.mkdir()
    bin_dir = root / "bin"; bin_dir.mkdir()
    artifacts = {name: {"sha256": "a"*64, "build": {"fixture": name}} for name in
                 ("robot-s0", "robot-gateway", "comm_fault_e2e")}
    expected = {"side": side, "bin_dir": str(bin_dir), "config": str(profile.config), "config_sha256": "b"*64,
                "calibration_sha256": "c"*64, "artifacts": artifacts, "tool_pid": 100}
    env = {"gateway_idempotency_max_entries": 8192, "gateway_idempotency_ttl_secs": 300,
           "background_tracked_limit": 64, "wire_active_limit_unchanged": 4, "s48_cache_observation": False,
           "hardware_id": profile.hardware_id, "address": profile.device, "current_exe": str(bin_dir / "comm_fault_e2e"),
           "config_sha256": expected["config_sha256"], "calibration_sha256": expected["calibration_sha256"],
           "artifacts": {role: artifacts[name]["build"] for role, name in
                         (("tool", "comm_fault_e2e"), ("s0", "robot-s0"), ("gateway", "robot-gateway"))},
           "gateway_arguments": ["--idempotency-max-entries", "8192", "--idempotency-ttl-secs", "300"]}
    attempts, boundaries, recoveries, csv_lines, groups = [], [], [], [], []
    csv_lines.append("attempt,external,sample_seq,stamp_ns,kind,source,normal_seq,generation,a,b")
    for external in (False, True):
        for inflight in (False, True):
            for i in range(1100):
                serial = len(attempts)
                seq = serial*3+1; now = 1_000_000_000 + serial*610_000_000
                attempts.append({"sequence": seq, "attempt": i, "external": external,
                                 "requested_in_flight": inflight, "matched": True, "warmup": i < 100,
                                 "t0_ns": now, "t1_ns": now+100, "all_off_ns": now+1_000_000})
                boundaries.extend([boundary(seq, "ArmInflight" if inflight else "ArmIdle", now-50),
                                   boundary(seq+1, "Disarm", now+1_000_001)])
                for offset, method in enumerate(("ClearEStop", "SetEnabled(true)")):
                    identity = f"recovery-{serial}-{offset}"
                    recoveries.append({"method": method, "ok": True, "request_id": identity,
                                       "idempotency_key": identity, "started_ns": now-100+offset*20,
                                       "finished_ns": now-90+offset*20, "response": ack(identity)})
                events = [("ExternalEnter" if external else "TopicReceived", now, 0, 0),
                          ("PrimitiveCommitted", now+100, 1, int(inflight)),
                          ("SerialClaim", now+200, 1, 1), ("IpcClaim", now+300, 1, 1),
                          ("IpcEStopCommit", now+400, 0, 0), ("SafetyStart", now+600_000, 1, 0),
                          ("SafetyReturn", now+800_000, 1, 1), ("ConfirmReturn", now+1_000_000, 0, 1),
                          ("AllOffTrusted", now+1_000_000, 0, 0)]
                if inflight:
                    events += [("NormalAcquire", now-1_000_000, 1, 1),
                               ("InjectedDelay", now-500_000, 1_000_000, 1_000_000),
                               ("NormalRelease", now+500_000, 1, 1)]
                for kind, stamp, a, b in sorted(events, key=lambda e:e[1]):
                    csv_lines.append(f"{i},{str(external).lower()},{seq},{stamp},{kind},1,7,1,{a},{b}")
            dist = {"count": 1000, "p50": 1, "p99": 1, "max": 1}
            groups.append({"external": external, "in_flight": inflight, "warmup": 100, "samples": 1000,
                           "class_mismatch_retries": 0, "passed": True, "sla_b_target_ns": 75_000_000 if inflight else 20_000_000,
                           "sla_a_ns": report.distribution([100]*1000), "sla_b_ns": report.distribution([1_000_000]*1000),
                           "primitive_cas_retries": dist, "serial_claim_cas_retries": dist,
                           "ipc_claim_cas_retries": dist, "acquire_cas_retries": dist if inflight else None})
    stop = attempts[-1]["all_off_ns"]+1000
    rr=[]
    for i in range((stop-1000)//50_000_000 + 1):
        start = 1000+i*50_000_000
        identity=f"background-{i}"
        rr.append({"sequence": i, "due_ns": start, "started_ns": start, "finished_ns": start+1,
                   "deadline_ns": start+2_000_000_000, "request_id": identity, "idempotency_key": identity,
                   "ok": True, "terminal": True, "response": ack(identity, True)})
    background={"valid_responses": True, "valid_cadence": True, "first_failure": None, "failures": [],
                "admission_rejection": None, "unfinished": [], "rr_target_per_second": 20, "tracked_limit": 64,
                "client_timeout_ns": 2_000_000_000, "drain_budget_ns": 2_500_000_000, "sigma_probe_period_ns": 5_000_000,
                "peak_tracked": 1, "started_ns": 1000, "stop_ns": stop, "drained_ns": stop+1, "finished_ns": stop+2,
                "launched": len(rr), "completed": len(rr), "successful_unique_writes": len(rr), "rr_requests": rr,
                "rr_gap_ns": report.distribution([50_000_000]*(len(rr)-1)), "sigma_raw_ns": [1,2,3],
                "sigma_est_ns": report.distribution([1,2,3])}
    final_seq=50_000
    boundaries.append(boundary(final_seq, "Disarm", stop+100))
    final={"ok": True, "estop": "Ok(49999)",
           "disarmed": f"Ok(({final_seq}, BoundarySnapshot {{ status: Disarmed, armed_seq: 0, failure: None }}))",
           "get_state": {"attempted": True, "ok": True, "safety_state": 3, "state_stamp_ns": stop+102},
           "disable": {**ack("final"), "attempted": True, "started_ns": stop+103, "finished_ns": stop+104,
                       "request": {"request_id": "final", "idempotency_key": "final", "enabled": False, "dry_run": False}}}
    final_trace={"valid": True, "error": None, "children_stopped": True, "read_error": None,
                 "installed": True, "dropped": 0, "boundary": {"request_seq": final_seq,
                 "receipt_seq": final_seq, "request_mode": "Some(Disarm)", "mode": "Some(Disarm)",
                 "status": "Disarmed", "armed_seq": 0, "gate_active": False, "failure_seq": 0, "failure": "None"}}
    rt=rt_fixture(side)
    measurement={"groups": groups, "background": background, "trace_dropped": 0, "hardware_only": True,
                 "runtime_only": False, "in_flight_exercised": True, "mock_lease": False,
                 "external_entry_exercised": True, "topic_entry_exercised": True, "idle_exercised": True,
                 "events_csv": str(evidence / "events.csv")}
    payload={"schema_version": 1, "status": "PASS", "hardware": {"H1":"NEEDS_HUMAN", "H2":"PASS", "H3":"NEEDS_HUMAN", "D15":"OPEN"},
             "invocation": {"phase":"OrinSla", "case_filter":None, "hardware_id":profile.hardware_id, "config":str(profile.config),
                            "config_sha256":"b"*64, "current_exe":str(bin_dir/"comm_fault_e2e"), "current_exe_sha256":"a"*64},
             "artifacts":[{"name":n, "path":str(bin_dir/n), **v} for n,v in artifacts.items()],
             "cases":[{"case":"orin_sla", "ac":"AC-12/H2", "status":"PASS", "evidence":{
                 "environment":env, "measurement":measurement, "final_state":final, "final_trace":final_trace, "rt_preflight":rt}}]}
    write_json(root/"report.json",payload)
    for name,value in (("environment.json",env),("background.json",background),("recovery_writes.json",recoveries),
                       ("rt_preflight.json",rt),("final_stop.json",final),("final_trace.json",final_trace)):
        write_json(evidence/name,value)
    for group in groups:
        write_json(evidence/f"sla_{str(group['external']).lower()}_{str(group['in_flight']).lower()}.json",group)
    for name,rows in (("attempts.jsonl",attempts),("boundary.jsonl",boundaries)):
        (evidence/name).write_text("\n".join(json.dumps(row) for row in rows)+"\n")
    (evidence/"events.csv").write_text("\n".join(csv_lines)+"\n")
    for name in ("s0.stdout.log","s0.stderr.log","gateway.stdout.log","gateway.stderr.log"):
        (evidence/name).write_text("")
    return expected


class ProcessNameTests(unittest.TestCase):
    def test_import_does_not_set_name_and_main_sets_name(self):
        spec=importlib.util.spec_from_file_location("h2_import_fixture",driver.__file__)
        module=importlib.util.module_from_spec(spec)
        with patch.object(Path,"write_text") as write:
            spec.loader.exec_module(module)
            write.assert_not_called()
        with patch.object(driver,"set_process_name") as name,contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(driver.main([]),0)
            name.assert_called_once_with()

    def test_linux_comm_failure_is_nonfatal_and_other_platform_does_not_write(self):
        with patch.object(driver.sys,"platform","linux"),patch.object(Path,"write_text") as write:
            driver.set_process_name()
            write.assert_called_once_with("robot-h2",encoding="ascii")
        with patch.object(driver.sys,"platform","linux"),patch.object(Path,"write_text",side_effect=OSError("拒绝命名")), \
                contextlib.redirect_stderr(io.StringIO()):
            driver.set_process_name()
        with patch.object(driver.sys,"platform","win32"),patch.object(Path,"write_text") as write:
            driver.set_process_name()
            write.assert_not_called()


class ParameterTests(unittest.TestCase):
    def test_default_does_not_read_files_spawn_or_create_directory(self):
        with patch.object(driver.subprocess, "run", side_effect=AssertionError), \
             patch.object(driver.subprocess, "Popen", side_effect=AssertionError), \
             patch.object(Path, "read_text", side_effect=AssertionError), \
             patch.object(Path, "mkdir", side_effect=AssertionError), contextlib.redirect_stdout(io.StringIO()) as out:
            self.assertEqual(driver.main([]),0)
            self.assertEqual(json.loads(out.getvalue())["mode"],"PLAN_ONLY")

    def test_missing_required_arguments_have_no_external_effect(self):
        with patch.object(driver.subprocess,"run",side_effect=AssertionError), \
             patch.object(Path,"mkdir",side_effect=AssertionError), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(driver.main(["--execute"]),2)

    def test_command_keeps_physical_budgets_and_removes_inherited_hooks(self):
        command=driver.tool_command(Path("/fake/bin"),Path("/fake/run"))
        self.assertEqual(command[:3],["taskset","-c","0-3"])
        self.assertNotIn("--h2-software-cache-entries",command)
        self.assertNotIn("chrt",command)
        env=driver.clean_environment({"PATH":"/bin","ROBOT_TEST_ANY":"1","NOTIFY_SOCKET":"bad", "ROBOT_TOKEN":"secret"})
        self.assertEqual(env,{"PATH":"/bin","RUST_LOG":"info"})

    def test_services_require_complete_inactive_not_failed(self):
        text="\n\n".join(f"Id={u}\nLoadState=loaded\nActiveState=inactive\nSubState=dead\nJob=0" for u in driver.UNITS)
        self.assertEqual(len(driver.validate_services(text)),18)
        legacy=text.replace("Job=0","MainPID=123\nJob=0")
        self.assertEqual(set(driver.validate_services(legacy)),set(driver.UNITS))
        for broken in (text.replace("inactive","failed",1), text.replace("Job=0","Job=42 /org/freedesktop/systemd1/job/42",1), text.split("\n\n",1)[1]):
            with self.assertRaises(ValueError): driver.validate_services(broken)

    def test_marker_strict_bytes_permissions_symlink_and_owner(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp)
            for name in ("rx_trust_marker","rx_trust_marker.bak"):
                path=root/name; path.write_bytes(b"1\n"); path.chmod(0o600)
            driver.marker_snapshot(root,b"1\n",os.getuid(),os.getgid())
            (root/"rx_trust_marker").chmod(0o644)
            with self.assertRaises(ValueError): driver.marker_snapshot(root,b"1\n",os.getuid(),os.getgid())
            (root/"rx_trust_marker").unlink(); (root/"rx_trust_marker").symlink_to(root/"rx_trust_marker.bak")
            with self.assertRaises(ValueError): driver.marker_snapshot(root,None,os.getuid(),os.getgid())


class ReportTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp=tempfile.TemporaryDirectory(); cls.root=Path(cls.temp.name)
        cls.expected=report_fixture(cls.root)

    @classmethod
    def tearDownClass(cls): cls.temp.cleanup()

    def test_current_complete_report_and_originals_pass(self):
        value=report.verify_report(self.root,self.expected)
        self.assertEqual((value["groups"],value["samples"],value["warmup"]),(4,4000,400))

    def test_state_three_does_not_hide_each_final_stop_failure(self):
        base=json.loads((self.root/"orin_sla_left/final_stop.json").read_text())
        boundaries=[json.loads(line) for line in (self.root/"orin_sla_left/boundary.jsonl").read_text().splitlines()]
        changes=[("ok",False),("estop","Err(\"timeout\")"),("disarmed","Err(\"failure\")")]
        for key,value in changes:
            bad=copy.deepcopy(base); bad[key]=value
            with self.subTest(key=key), self.assertRaises(ValueError): report.verify_final_stop(bad,boundaries)
        for key,value in (("attempted",False),("ok",False),("kind","status"),("request_id","wrong"),("dry_run",True),("cleared_at_ns",0)):
            bad=copy.deepcopy(base); bad["disable"][key]=value
            with self.subTest(disable=key), self.assertRaises(ValueError): report.verify_final_stop(bad,boundaries)
        bad=copy.deepcopy(base); bad["disarmed"]=bad["disarmed"].replace("failure: None","failure: Some(Timeout)")
        with self.assertRaises(ValueError): report.verify_final_stop(bad,boundaries)
        bad={"safety_state":3,"ok":True}
        with self.assertRaises(ValueError): report.verify_final_stop(bad,boundaries)

    def test_missing_original_rejected(self):
        path=self.root/"orin_sla_left/recovery_writes.json"; saved=path.read_bytes(); path.unlink()
        try:
            with self.assertRaises(ValueError): report.verify_report(self.root,self.expected)
        finally: path.write_bytes(saved)

    def test_raw_failures_or_budget_changes_rejected(self):
        path=self.root/"report.json"; saved=path.read_bytes()
        for mutate in (lambda p:p.update(status="FAIL"),
                       lambda p:p["invocation"].update(current_exe="/old/tool"),
                       lambda p:p["cases"][0]["evidence"]["measurement"].update(trace_dropped=1),
                       lambda p:p["cases"][0]["evidence"]["measurement"]["groups"].pop()):
            try:
                bad=json.loads(saved); mutate(bad); write_json(path,bad)
                with self.assertRaises(ValueError): report.verify_report(self.root,self.expected)
            finally: path.write_bytes(saved)

    def test_background_original_rows_override_aggregate_flags(self):
        base=json.loads((self.root/"orin_sla_left/background.json").read_text())
        for mutate in (lambda p:p["rr_requests"][0].update(ok=False),
                       lambda p:p["rr_requests"][0].update(started_ns=p["started_ns"]+50_000_000),
                       lambda p:p.update(completed=p["completed"]-1),
                       lambda p:p.update(drained_ns=p["stop_ns"]+2_500_000_001),
                       lambda p:p.update(failures=[{"error":"join"}])):
            bad=copy.deepcopy(base); mutate(bad)
            with self.assertRaises(ValueError): report.verify_background(bad)

    def test_rt_checks_actual_threads_not_claimed_pass(self):
        for mutate in (lambda p:p["processes"][1]["snapshot"]["threads"][1].update(priority=69),
                       lambda p:p["processes"][0]["snapshot"]["threads"][0].update(policy=1),
                       lambda p:p["processes"][2]["snapshot"].update(errors=["unreadable"]),
                       lambda p:p["processes"][1]["snapshot"].update(tids_after=[101])):
            bad=rt_fixture(); mutate(bad)
            with self.assertRaises(ValueError): report.verify_rt(bad,100)


class FakeBackend:
    instances=[]
    failure=None

    def __init__(self,options,journal):
        self.options=options; self.journal=journal; self.actions=[]; self.process=None
        self.profile=report.arm_profile(getattr(options,"side","left"))
        type(self).instances.append(self)

    def preflight(self):
        if self.failure=="preflight": raise ValueError("预检失败")

    def audit(self): return {"fixture":True}
    def trust(self,action): self.actions.append(action)
    def markers(self,expected): return {"expected":expected.hex(),"fixture":True}
    def launch(self):
        self.actions.append("Popen")
        if self.failure=="launch": raise OSError("明确Popen失败")
        self.process=subprocess.Popen([sys.executable,"-c","import time; time.sleep(0.08)"],start_new_session=True)
        if self.failure == "born_exception": raise OSError("出生后句柄交接异常")
        if self.failure == "born_signal": os.kill(os.getpid(), signal.SIGINT)
        return self.process
    def report(self,pid):
        if self.failure=="report": raise ValueError("模拟原报告失败")
        return {"fixture":True,"pid":pid}
    def cleanup(self):
        if self.failure=="cleanup": raise ValueError("模拟残余进程")
        require_empty=self.process is None or self.process.poll() is not None
        driver.require(require_empty,"无害子进程未退出")
        return {"fixture":True,"clean":True}
    def close(self): pass


class LifecycleTests(unittest.TestCase):
    def run_fixture(self,failure):
        FakeBackend.failure=failure
        with tempfile.TemporaryDirectory() as temp:
            options=argparse.Namespace(output=Path(temp)/"new",expected_head="a"*40,inputs_sha256="b"*64)
            result=driver.execute(options,FakeBackend)
            state=json.loads((options.output/"run.json").read_text())
            instance=FakeBackend.instances[-1]
            return result,state,instance

    def test_single_authorize_single_launch_and_real_natural_wait(self):
        result,state,backend=self.run_fixture(None)
        self.assertEqual(result,0)
        self.assertEqual(backend.actions,["status","revoke","status","authorize","status","Popen"])
        self.assertTrue(state["tool_naturally_reaped"])
        self.assertEqual(state["tool_exit"],0)
        with self.assertRaises(ChildProcessError): os.waitpid(backend.process.pid,os.WNOHANG)

    def test_preflight_failure_never_authorizes_or_launches(self):
        result,state,backend=self.run_fixture("preflight")
        self.assertEqual(result,1); self.assertEqual(backend.actions,[])

    def test_popen_failure_revokes_unused_authorization(self):
        result,state,backend=self.run_fixture("launch")
        self.assertEqual(result,1); self.assertEqual(backend.actions.count("authorize"),1)
        self.assertEqual(backend.actions.count("Popen"),1); self.assertEqual(backend.actions[-1],"revoke")
        self.assertIn("unused_authorization_revoked",state)

    def test_started_failures_remain_fail_without_second_authorization_or_launch(self):
        for failure in ("report","cleanup","born_exception","born_signal"):
            with self.subTest(failure=failure):
                result,state,backend=self.run_fixture(failure)
                self.assertEqual(result,1); self.assertTrue(state["tool_naturally_reaped"])
                self.assertEqual(backend.actions.count("authorize"),1); self.assertEqual(backend.actions.count("Popen"),1)

    def test_consumed_run_directory_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            options=argparse.Namespace(output=Path(temp),expected_head="a"*40,inputs_sha256="b"*64)
            with self.assertRaises(FileExistsError): driver.execute(options,FakeBackend)


class RealBackendTests(unittest.TestCase):
    def options(self, root):
        return argparse.Namespace(output=root, expected_head="a"*40, inputs_sha256="b"*64)

    def test_execute_artifacts_require_v2_and_one_shared_compiler_window(self):
        # 复用完整离线 ELF/原始 Cargo 回执夹具，实际走 artifact_check 与共享校验器。
        from test_artifact_checks import ArtifactTests
        fixture = ArtifactTests()
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        production = fixture.manifest("production")
        measurement = fixture.manifest("measurement")
        options = self.options(fixture.root)
        backend = driver.RealBackend(options, driver.Journal(fixture.root, options))
        backend.source = fixture.source
        backend.command = lambda argv: self.fail("相同 HEAD 不应启动 git 子进程")

        def check(prod, measured):
            backend.inputs = {"production": fixture.write("production.json", prod),
                              "measurement": fixture.write("measurement.json", measured)}
            return backend.artifact_check()

        # 两套各自合法但没有同一对回执，不能进入本次现场执行。
        with self.assertRaisesRegex(ValueError, "共用同一对"):
            check(production, measurement)
        production["rustc_after"] = measurement["rustc_after"]
        measurement["rustc_before"] = production["rustc_before"]
        self.assertEqual(set(check(production, measurement)), {"production", "measurement"})
        for kind in ("production", "measurement"):
            changed = copy.deepcopy(production if kind == "production" else measurement)
            changed["schema"] = "P7_RELEASE_ARTIFACTS_V1"
            with self.subTest(kind=kind), self.assertRaisesRegex(ValueError, "只接受 V2"):
                check(changed if kind == "production" else production,
                      changed if kind == "measurement" else measurement)

    def test_real_launch_owns_child_before_return_signal_and_exception(self):
        for fault in ("signal", "exception", "disk"):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as temp:
                options = self.options(Path(temp)/"run")
                actual_popen = subprocess.Popen
                class Backend(FakeBackend):
                    launch = driver.RealBackend.launch
                    def __init__(self, options, journal):
                        super().__init__(options, journal)
                        self.bin_dir = Path(temp)
                        self.environment = driver.clean_environment(os.environ)
                        self.log = None
                        self.launching = False
                    def close(self):
                        if self.log: self.log.close()
                def popen(argv, **kwargs):
                    child = actual_popen([sys.executable, "-c", "import time; time.sleep(0.1)"], **kwargs)
                    # SIGINT故意发生在Popen返回给backend的赋值之前。
                    if fault == "signal": os.kill(os.getpid(), signal.SIGINT)
                    return child
                original_launch = Backend.launch
                def launch(backend):
                    value = original_launch(backend)
                    if fault == "exception": raise OSError("已出生、返回调用方之前异常")
                    if fault == "disk":
                        backend.journal._save = lambda: (_ for _ in ()).throw(OSError("磁盘失败"))
                    return value
                Backend.launch = launch
                FakeBackend.failure = None
                with patch.object(driver.subprocess, "Popen", side_effect=popen):
                    result = driver.execute(options, Backend)
                backend = Backend.instances[-1]
                self.assertEqual(result, 1)
                self.assertTrue(backend.journal.state["tool_naturally_reaped"])
                self.assertEqual(backend.process.returncode, 0)
                self.assertEqual(backend.journal.state["tool_pid"], backend.process.pid)
                self.assertEqual(backend.actions.count("authorize"), 1)
                self.assertEqual(backend.actions.count("revoke"), 1)
                with self.assertRaises(ChildProcessError): os.waitpid(backend.process.pid, os.WNOHANG)

    def test_disk_failure_before_authorization_does_not_launch(self):
        FakeBackend.failure = None
        with tempfile.TemporaryDirectory() as temp:
            options = self.options(Path(temp)/"run")
            with patch.object(driver.Journal, "_save", side_effect=OSError("磁盘失败")):
                self.assertEqual(driver.execute(options, FakeBackend), 1)
            backend = FakeBackend.instances[-1]
            self.assertNotIn("authorize", backend.actions)
            self.assertIsNone(backend.process)

    def test_physical_facts_are_pinned_to_new_run_and_artifacts(self):
        options = self.options(Path("/new/run"))
        inputs = {"schema":"P7_H2_INPUTS_V1", "expected_head":options.expected_head,
                  "output":str(options.output), "source_manifest_sha256":"s"*64,
                  "alignment":{"sha256":"c"*64}, "measurement":{"sha256":"d"*64}}
        alignment = {"schema":"P7_H2_ALIGNMENT_V1", "expected_head":options.expected_head,
                     "recorded_at_ns":10, "hosts":[{"role":role, "host":role, "boot_id":role,
                     "head":options.expected_head, "status_porcelain":"", "source_manifest_sha256":"s"*64}
                     for role in ("local","orin")]}
        physical = {"schema":"P7_H2_PHYSICAL_CONFIRMATION_V1", "side":"left", "issued_by":"root",
                    "expected_head":options.expected_head, "output":str(options.output), "confirmed_at_ns":11,
                    "source":"本次用户现场事实", "measurement_manifest_sha256":"d"*64, "alignment_sha256":"c"*64}
        facts = ("supported", "unloaded", "area_clear", "motor_power_cycle", "usb_replug",
                 "continuous_presence_until_cleanup", "immediate_power_cut")
        physical.update({key:True for key in facts})
        driver.validate_facts(inputs,alignment,physical,options,12)
        for field in facts:
            bad = dict(physical); bad[field] = False
            with self.subTest(field=field), self.assertRaises(ValueError):
                driver.validate_facts(inputs,alignment,bad,options,12)
        for field, value in (("output","/old/consumed"),("expected_head","f"*40),
                             ("confirmed_at_ns",9),("measurement_manifest_sha256","old"),("source","")):
            bad = dict(physical); bad[field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                driver.validate_facts(inputs,alignment,bad,options,12)
        for field, value in (("head","f"*40),("status_porcelain"," M scripts/run_comm_fault_probe.py")):
            bad = copy.deepcopy(alignment); bad["hosts"][1][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                driver.validate_facts(inputs,bad,physical,options,12)

    def test_real_backend_command_records_original_failure_and_rejects_partial_authorize(self):
        with tempfile.TemporaryDirectory() as temp:
            options = self.options(Path(temp))
            backend = driver.RealBackend(options, driver.Journal(options.output,options))
            backend.production = {"artifacts":{"robot-s0":{"path":"/fixture/robot-s0"}}}
            with patch.object(driver.subprocess,"run",return_value=subprocess.CompletedProcess([],3,"partial","error")):
                with self.assertRaises(ValueError): backend.trust("authorize")
            row = backend.journal.state["steps"][-1]
            self.assertEqual((row["exit_code"],row["stdout"],row["stderr"]),(3,"partial","error"))
            self.assertIn("--rx-trust-dir",row["argv"])

    def test_pinned_json_rejects_mutation_and_duplicate_keys(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp)/"input.json"; path.write_text('{"x":1}')
            reference = {"path":str(path),"sha256":driver.file_hash(path)}
            self.assertEqual(driver.pinned_json(reference),{"x":1})
            path.write_text('{"x":2}')
            with self.assertRaises(ValueError): driver.pinned_json(reference)
            path.write_text('{"x":1,"x":2}'); reference["sha256"] = driver.file_hash(path)
            with self.assertRaises(ValueError): driver.pinned_json(reference)

    def test_fresh_source_head_dirty_content_and_executable_gate(self):
        import artifact_checks as artifacts
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp); (root/"Cargo.toml").write_text("[workspace]\n")
            source=artifacts.snapshot_source(root,["Cargo.toml"])
            manifest=root/"source.json"; write_json(manifest,source)
            options=self.options(root)
            backend=driver.RealBackend(options,driver.Journal(root,options))
            backend.inputs={"repository":str(root),"source_manifest_sha256":source["source_manifest_sha256"],
                            "source":{"path":str(manifest),"sha256":driver.file_hash(manifest)}}
            backend.alignment={"hosts":[{"role":"orin","host":os.uname().nodename,
                "boot_id":Path("/proc/sys/kernel/random/boot_id").read_text().strip()}]}
            replies={"HEAD":options.expected_head+"\n","--untracked-files=all":"","--exclude-standard":"Cargo.toml\0"}
            backend.command=lambda argv:replies[argv[-1]]
            self.assertEqual(backend.source_check()["head"],options.expected_head)
            for key,value in (("HEAD","f"*40+"\n"),("--untracked-files=all"," M Cargo.toml\n")):
                saved=replies[key]; replies[key]=value
                with self.subTest(key=key),self.assertRaises(ValueError):backend.source_check()
                replies[key]=saved
            path=root/"Cargo.toml"
            path.write_text("[workspace]\nmembers=[]\n")
            with self.assertRaises(ValueError):backend.source_check()
            path.write_text("[workspace]\n");path.chmod(0o755)
            with self.assertRaises(ValueError):backend.source_check()
            path.chmod(0o644);(root/"Cargo.lock").write_text("new")
            replies["--exclude-standard"]="Cargo.toml\0Cargo.lock\0"
            with self.assertRaises(ValueError):backend.source_check()

    def test_fuser_error_cannot_be_interpreted_as_no_holder(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp); port=root/"port";port.write_text("fixture")
            options=self.options(root);backend=driver.RealBackend(options,driver.Journal(root,options))
            properties="ID_VENDOR_ID=1a86\nID_MODEL_ID=55d3\nID_SERIAL_SHORT=5A7C121427\nID_USB_DRIVER=cdc_acm\n"
            for code,stdout,stderr,valid in ((1,"","",True),(0,"123","",False),(2,"","error",False),(1,"","error",False)):
                replies=[subprocess.CompletedProcess([],0,properties,""),subprocess.CompletedProcess([],code,stdout,stderr)]
                with self.subTest(code=code,stderr=stderr),patch.object(backend,"profile",replace(backend.profile,device=str(port))), \
                     patch.object(driver.stat,"S_ISCHR",return_value=True),patch.object(driver.subprocess,"run",side_effect=replies):
                    if valid:self.assertEqual(backend.device_check()["path"],str(port))
                    else:
                        with self.assertRaises(ValueError):backend.device_check()

    def test_unreadable_process_is_not_clean(self):
        with patch.object(Path,"iterdir",return_value=[Path("/fake/123")]):
            value = driver.process_snapshot(123,Path("/fake/run"),[],Path("/fake"),
                reader=lambda *args,**kwargs:{"pid":123,"status":"unreadable","error":"不可读"})
        self.assertFalse(value["clean"])
        self.assertEqual(value["unreadable"][0]["pid"],123)


class ProcProbeTests(unittest.TestCase):
    boot = "12345678-1234-1234-1234-123456789abc"

    def make_proc(self, root, pid=123):
        directory = root/str(pid); directory.mkdir()
        fields = ["S", "1", "12"] + ["0"]*16 + ["42"]
        (directory/"stat").write_text(f"{pid} (名字 ) 含括号) " + " ".join(fields))
        (directory/"status").write_text("Name:\tfixture\nUid:\t1000\t1000\t1000\t1000\n")
        (directory/"cmdline").write_bytes(b"/unrelated/tool\0")
        (directory/"exe").symlink_to("/unrelated/tool")
        (root/"sys/kernel/random").mkdir(parents=True,exist_ok=True)
        (root/"sys/kernel/random/boot_id").write_text(self.boot+"\n")
        return directory

    def row(self, *, exe="/unrelated/tool", pgid=12, argv=None, state="S"):
        identity = {"pid":123,"start_ticks":42,"pgid":pgid,"state":state,"boot_id":self.boot,
                    "uids":[1000]*4}
        return {"pid":123,"status":"observed","before":dict(identity),"after":dict(identity),
                "identity":dict(identity,exe=exe,argv=argv or ["/unrelated/tool"],zombie=state=="Z")}

    def receipt(self, rows):
        return {"exit_code":0,"response":{"schema":"P7_PROC_PROBE_V1","reader_euid":0,
                                           "boot_id":self.boot,"rows":rows}}

    def scan_denied(self, receipt, expected=None):
        base=expected or self.row()
        def reader(pid,expected=None,details=True,proc=None):
            if not details:
                basic=copy.deepcopy(base); basic["identity"]={k:v for k,v in basic["identity"].items()
                    if k not in ("exe","argv","zombie")}
                return basic
            return {"pid":pid,"status":"permission","before":expected,"error":"PermissionError"}
        with patch.object(driver.os,"getuid",return_value=1000), \
             patch.object(Path,"iterdir",return_value=[Path("/fixture/123")]):
            calls=[]
            def probe(rows):
                calls.append(rows)
                return receipt
            result=driver.process_snapshot(99,Path("/run/new"),[Path("/bin/measurement")],
                Path("/fixture"),reader=reader,probe_runner=probe)
        return result,calls

    def test_dirfd_read_uses_status_uid_and_handles_stat_parentheses(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);self.make_proc(root)
            row=driver.read_process(123,proc=root)
            self.assertEqual(row["status"],"observed")
            self.assertEqual(row["identity"]["start_ticks"],42)
            self.assertEqual(row["identity"]["uids"],[1000]*4)
            with patch.object(driver.os,"getuid",return_value=1000), \
                 patch.object(Path,"stat",side_effect=AssertionError("不能依赖proc目录属主")):
                value=driver.process_snapshot(99,Path("/run/new"),[Path("/unrelated/tool")],root)
            self.assertEqual(value["related"][0]["pid"],123)
            self.assertFalse(value["clean"])

    def check_probe_request(self, candidate):
        request={"schema":"P7_PROC_PROBE_V1","candidates":[candidate]}
        with patch.object(sys,"stdin",io.StringIO(json.dumps(request))), \
             patch.dict(driver._PROC_READER,{"_boot":lambda root:self.boot,
                 "_proc_probe":lambda pid,expected:{"pid":pid,"status":"vanished"}}), \
             contextlib.redirect_stdout(io.StringIO()) as out:
            driver._PROC_READER["_probe_main"]()
            return json.loads(out.getvalue())

    def test_zero_start_ticks_is_valid_and_still_fully_identity_bound(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);directory=self.make_proc(root)
            path=directory/"stat";path.write_text(path.read_text().rsplit(" ",1)[0]+" 0")
            row=driver.read_process(123,details=False,proc=root)
            self.assertEqual(row["status"],"observed")
            identity=row["identity"]
            self.assertEqual(identity["start_ticks"],0)
            self.assertEqual(driver.read_process(123,expected=identity,proc=root)["status"],"observed")
            for field,value in (("start_ticks",1),("boot_id","other"),("pgid",99),("uids",[0]*4)):
                with self.subTest(field=field):
                    self.assertEqual(driver.read_process(123,expected=dict(identity,**{field:value}),proc=root)["status"],"replaced")
            response=self.check_probe_request(identity)
            self.assertEqual(response["rows"],[{"pid":123,"status":"vanished"}])

    def test_negative_missing_or_malformed_start_ticks_is_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);directory=self.make_proc(root)
            path=directory/"stat";prefix=path.read_text().rsplit(" ",1)[0]
            for suffix in (" -1"," invalid",""):
                path.write_text(prefix+suffix)
                with self.subTest(stat_suffix=suffix):
                    self.assertEqual(driver.read_process(123,details=False,proc=root)["status"],"unreadable")
        identity=self.row()["before"]
        for value in (-1,"0",None):
            with self.subTest(probe_value=value),self.assertRaises(ValueError):
                self.check_probe_request(dict(identity,start_ticks=value))
        missing=dict(identity);missing.pop("start_ticks")
        with self.assertRaises(ValueError):self.check_probe_request(missing)

    def test_only_stable_privileged_unrelated_identity_resolves_eacces(self):
        value,calls=self.scan_denied(self.receipt([self.row()]))
        self.assertTrue(value["clean"])
        self.assertEqual(len(value["ordinary_denied"]),1)
        self.assertEqual(len(calls),1)
        self.assertEqual(calls[0][0]["start_ticks"],42)
        self.assertEqual(calls[0][0]["uids"],[1000]*4)

    def test_related_pgid_argv_elf_and_zombie_remain_residual(self):
        for row in (self.row(pgid=99),self.row(exe="/bin/measurement"),
                    self.row(argv=["tool","--output","/run/new"]),self.row(exe=None,pgid=99,state="Z")):
            with self.subTest(row=row):
                value,_=self.scan_denied(self.receipt([row]),expected=row)
                self.assertFalse(value["clean"])
                self.assertEqual(len(value["related"]),1)

    def test_replaced_unknown_failed_or_malformed_probe_stays_unreadable(self):
        good=self.receipt([self.row()])
        broken=[]
        for field,value in (("start_ticks",43),("boot_id","87654321-1234-1234-1234-123456789abc"),("pgid",13),("uids",[0]*4)):
            bad=copy.deepcopy(good);bad["response"]["rows"][0]["after"][field]=value;broken.append(bad)
        for status in ("replaced","unreadable","permission","unknown"):
            bad=copy.deepcopy(good);bad["response"]["rows"][0]["status"]=status;broken.append(bad)
        for rows in ([],[self.row(),self.row()],[dict(self.row(),pid=456)]):
            bad=copy.deepcopy(good);bad["response"]["rows"]=rows;broken.append(bad)
        broken += [{"exit_code":1,"error":"sudo denied"},{"exit_code":0,"response":{}},
                   {"exit_code":0,"response":dict(good["response"],reader_euid=1000)},
                   {"exit_code":0,"response":dict(good["response"],boot_id="other")}]
        for receipt in broken:
            with self.subTest(receipt=receipt):
                value,_=self.scan_denied(receipt)
                self.assertFalse(value["clean"])
                self.assertTrue(value["unreadable"])

    def test_vanished_is_explicit_current_absence_not_reap_claim(self):
        value,_=self.scan_denied(self.receipt([{"pid":123,"status":"vanished"}]))
        self.assertTrue(value["clean"])
        self.assertEqual(value["vanished"][0]["expected"]["start_ticks"],42)
        self.assertNotIn("reaped",value)

    def test_fixed_probe_command_bounds_and_failures(self):
        candidate={k:v for k,v in self.row()["identity"].items() if k not in ("exe","argv","zombie")}
        for result in (subprocess.CompletedProcess([],1,"","sudo denied"),
                       subprocess.CompletedProcess([],0,"{truncated",""),
                       subprocess.CompletedProcess([],0,'{"x":1,"x":2}',"")):
            with patch.object(driver.subprocess,"run",return_value=result) as run:
                value=driver.privileged_process_probe([candidate])
                self.assertIn("error",value)
                argv=run.call_args.args[0]
                self.assertEqual(argv[:6],["sudo","-n","/usr/bin/python3","-I","-S","-c"])
                self.assertEqual(argv[6],driver.PROC_PROBE_CODE)
                self.assertEqual(run.call_args.kwargs["timeout"],10)
                self.assertNotIn("shell",run.call_args.kwargs)
        with patch.object(driver.subprocess,"run",side_effect=subprocess.TimeoutExpired("probe",10)):
            self.assertIn("error",driver.privileged_process_probe([candidate]))

    def test_fixed_probe_script_rejects_extra_path_and_duplicate_pid_input(self):
        candidate={k:v for k,v in self.row()["identity"].items() if k not in ("exe","argv","zombie")}
        for candidates in ([dict(candidate,root="/tmp/forbidden")],[candidate,candidate],[]):
            result=subprocess.run([sys.executable,"-I","-S","-c",driver.PROC_PROBE_CODE],
                input=json.dumps({"schema":"P7_PROC_PROBE_V1","candidates":candidates}),
                capture_output=True,text=True,timeout=5,check=False)
            self.assertNotEqual(result.returncode,0)
            self.assertEqual(result.stdout,"")

    def test_oversized_permission_batch_never_invokes_sudo(self):
        def reader(pid,expected=None,details=True,proc=None):
            row=self.row();row["pid"]=pid
            for key in ("identity","before","after"):row[key]["pid"]=pid
            if details:return {"pid":pid,"status":"permission","before":expected}
            row["identity"]={k:v for k,v in row["identity"].items() if k not in ("exe","argv","zombie")}
            return row
        with patch.object(driver.os,"getuid",return_value=1000), \
             patch.object(Path,"iterdir",return_value=[Path("/fixture")/str(pid) for pid in range(1000000,1000257)]):
            def probe(rows):raise AssertionError("不应调用sudo")
            value=driver.process_snapshot(99,Path("/run/new"),[],Path("/fixture"),reader=reader,probe_runner=probe)
        self.assertFalse(value["clean"])
        self.assertEqual(len(value["unreadable"]),257)

    def test_dirfd_replacement_missing_exe_and_exit_are_distinguished(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);directory=self.make_proc(root)
            expected=driver.read_process(123,details=False,proc=root)["identity"]
            changed=dict(expected,start_ticks=41)
            self.assertEqual(driver.read_process(123,expected=changed,proc=root)["status"],"replaced")
            (directory/"exe").unlink()
            missing = driver.read_process(123,proc=root)
            self.assertEqual(missing["status"],"unreadable")
            self.assertEqual((missing["error_errno"], missing["error_filename"]), (2, "exe"))
            self.assertEqual(driver.read_process(456,proc=root)["status"],"vanished")
            actual_stat=driver._PROC_READER["_stat_at"]
            calls=0
            def changing(fd,pid):
                nonlocal calls
                calls+=1;value=actual_stat(fd,pid)
                if calls==2:value["start_ticks"]+=1
                return value
            with patch.dict(driver._PROC_READER,{"_stat_at":changing}):
                self.assertEqual(driver.read_process(123,details=False,proc=root)["status"],"replaced")

    def test_real_non_dumpable_denial_with_fixture_probe_and_natural_reap(self):
        code = """import ctypes,json,os,sys
sys.path.insert(0,sys.argv[1])
import run_comm_fault_probe as driver
assert ctypes.CDLL(None).prctl(4,0,0,0,0)==0
print(json.dumps(driver.read_process(os.getpid())),flush=True)
sys.stdin.buffer.read(1)
"""
        child=subprocess.Popen([sys.executable,"-c",code,str(Path(driver.__file__).parent)],
                               stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True,start_new_session=True)
        try:
            own=json.loads(child.stdout.readline())
            self.assertEqual(own["status"],"observed")
            basic=driver.read_process(child.pid,details=False)
            self.assertEqual(basic["status"],"observed")
            ordinary=driver.read_process(child.pid,expected=basic["identity"])
            self.assertEqual(ordinary["status"],"permission")
            # 真实child产生拒读；仅probe回执为夹具，不能当sudo路径实证。
            receipt=self.receipt([own]);receipt["response"]["boot_id"]=own["identity"]["boot_id"]
            with patch.object(Path,"iterdir",return_value=[Path("/proc")/str(child.pid)]):
                value=driver.process_snapshot(child.pid,Path("/fixture/run"),[],probe_runner=lambda rows:receipt)
            self.assertFalse(value["clean"])
            self.assertEqual(value["related"][0]["pid"],child.pid)
            self.assertEqual(value["unreadable"],[])
        finally:
            child.stdin.write("x");child.stdin.flush();child.stdin.close()
            self.assertEqual(child.wait(timeout=5),0)
            child.stdout.close()
        with self.assertRaises(ChildProcessError):os.waitpid(child.pid,os.WNOHANG)


class OwnedProcessGroupTests(unittest.TestCase):
    def test_only_group_absence_proves_cleanup(self):
        with tempfile.TemporaryDirectory() as temp:
            options = argparse.Namespace(output=Path(temp),expected_head="a"*40,inputs_sha256="b"*64)
            backend = driver.RealBackend(options,driver.Journal(options.output,options))
            with patch.object(driver.os,"killpg") as probe:
                self.assertEqual(backend.cleanup(), {"status":"NOT_STARTED"})
                probe.assert_not_called()
                backend.process = argparse.Namespace(pid=123,returncode=None)
                with self.assertRaisesRegex(ValueError,"尚未回收"):
                    backend.cleanup()
                probe.assert_not_called()
                backend.process.returncode = 0
                with self.assertRaisesRegex(ValueError,"仍有成员"):
                    backend.cleanup()
                probe.assert_called_once_with(123,0)
                probe.side_effect = PermissionError()
                with self.assertRaises(PermissionError):
                    backend.cleanup()
                probe.side_effect = ProcessLookupError()
                self.assertEqual(backend.cleanup(), {"status":"GROUP_EXITED","pgid":123})

    def test_group_member_surviving_reaped_leader_blocks_cleanup(self):
        argv = [sys.executable, "-c", "import sys; sys.stdin.buffer.read(1)"]
        with tempfile.TemporaryDirectory() as temp:
            options = argparse.Namespace(output=Path(temp),expected_head="a"*40,inputs_sha256="b"*64)
            backend = driver.RealBackend(options,driver.Journal(options.output,options))
            with subprocess.Popen(argv,stdin=subprocess.PIPE,process_group=0) as leader:
                backend.process = leader
                with subprocess.Popen(argv,stdin=subprocess.PIPE,process_group=leader.pid) as member:
                    try:
                        leader.stdin.close()
                        self.assertEqual(leader.wait(timeout=5),0)
                        with self.assertRaisesRegex(ValueError,"仍有成员"):
                            backend.cleanup()
                        self.assertIsNone(member.poll())
                    finally:
                        member.stdin.close()
                self.assertEqual(backend.cleanup(),{"status":"GROUP_EXITED","pgid":leader.pid})

    def test_nonzero_child_exit_and_unrelated_same_program(self):
        class Backend(FakeBackend):
            cleanup = driver.RealBackend.cleanup
            def launch(self):
                self.actions.append("Popen")
                self.process=subprocess.Popen([sys.executable,"-c","raise SystemExit(7)"],start_new_session=True)
                return self.process
        FakeBackend.failure=None
        with subprocess.Popen([sys.executable,"-c","import sys; sys.stdin.read()"],
                              stdin=subprocess.PIPE, start_new_session=True) as unrelated:
            try:
                with tempfile.TemporaryDirectory() as temp:
                    options=argparse.Namespace(output=Path(temp)/"run",expected_head="a"*40,inputs_sha256="b"*64)
                    with patch.object(driver,"process_snapshot",side_effect=AssertionError("不得枚举全系统")):
                        self.assertEqual(driver.execute(options,Backend),1)
                    state=json.loads((options.output/"run.json").read_text())
                    self.assertEqual(state["tool_exit"],7)
                    self.assertTrue(state["tool_naturally_reaped"])
                    self.assertEqual(state["cleanup"],{"status":"GROUP_EXITED","pgid":state["tool_pid"]})
                    self.assertTrue(any("工具原退出码 7" in failure["error"] for failure in state["failures"]))
                    self.assertIsNone(unrelated.poll())
            finally:
                unrelated.stdin.close()


class RuntimePreconditionTests(unittest.TestCase):
    def test_readonly_inspection_does_not_create_or_spawn_and_validator_rejects_bad_trust(self):
        with tempfile.TemporaryDirectory() as temp:
            trust=Path(temp)/"trust";trust.mkdir(mode=0o750)
            with patch.object(driver.subprocess,"run",side_effect=AssertionError), \
                 patch.object(driver.subprocess,"Popen",side_effect=AssertionError), \
                 patch.object(Path,"mkdir",side_effect=AssertionError):
                value=driver.inspect_runtime_preconditions(trust)
            self.assertEqual(value["trust"]["mode"],0o750)
            self.assertEqual(value["trust"]["inode"],trust.stat().st_ino)
            good=copy.deepcopy(value)
            good.update(uid=1000,euid=1000,gid=1000,egid=1000,account_uid=1000,account_gid=1000,
                        machine="aarch64",python_version=[3,12,0],rtprio=[99,99],memlock=[-1,-1])
            good["tools"]={name:"/usr/bin/"+name for name in good["tools"]}
            good["trust"].update(uid=1000,gid=1000)
            self.assertEqual(driver.validate_runtime_preconditions(good,trust),good)
            for field,bad in (("mode",0o755),("uid",0),("gid",0),("is_directory",False),("is_symlink",True)):
                value=copy.deepcopy(good);value["trust"][field]=bad
                with self.subTest(field=field),self.assertRaises(ValueError):driver.validate_runtime_preconditions(value,trust)
            for field,bad in (("uid",0),("euid",0),("egid",0),("rtprio",[69,99]),("memlock",[65536,65536]),("machine","x86_64")):
                value=copy.deepcopy(good);value[field]=bad
                with self.subTest(field=field),self.assertRaises(ValueError):driver.validate_runtime_preconditions(value,trust)
            trust.chmod(0o755)
            self.assertEqual(driver.inspect_runtime_preconditions(trust)["trust"]["mode"],0o755)
            link=Path(temp)/"link";link.symlink_to(trust)
            self.assertTrue(driver.inspect_runtime_preconditions(link)["trust"]["is_symlink"])


def side_facts(root, side):
    options=argparse.Namespace(side=side,output=root,expected_head="a"*40,inputs_sha256="b"*64,inputs=root/"inputs.json")
    inputs={"schema":"P7_H2_INPUTS_V1","side":side,"expected_head":options.expected_head,
            "output":str(root),"source_manifest_sha256":"s"*64,"alignment":{"sha256":"c"*64},
            "measurement":{"sha256":"d"*64},"physical":{"sha256":"e"*64}}
    alignment={"schema":"P7_H2_ALIGNMENT_V1","expected_head":options.expected_head,"recorded_at_ns":10,
               "hosts":[{"role":role,"host":role,"boot_id":role,"head":options.expected_head,
                         "status_porcelain":"","source_manifest_sha256":"s"*64} for role in ("local","orin")]}
    physical={"schema":"P7_H2_PHYSICAL_CONFIRMATION_V1","side":side,"issued_by":"root",
              "expected_head":options.expected_head,"output":str(root),"confirmed_at_ns":11,
              "source":"本次现场事实夹具","measurement_manifest_sha256":"d"*64,"alignment_sha256":"c"*64}
    physical.update({key:True for key in ("supported","unloaded","area_clear","motor_power_cycle","usb_replug",
                                        "continuous_presence_until_cleanup","immediate_power_cut")})
    return options,inputs,alignment,physical


class SideTests(unittest.TestCase):
    def test_profiles_exact_d2_and_immutable(self):
        for side,serial,cpu in (("left","5A7C121427",4),("right","5A7C122340",5)):
            p=report.arm_profile(side)
            self.assertEqual((p.robot_id,p.hardware_id,p.config,p.repository_config,p.trust,p.device,
                              p.usb_serial,p.calibration_name,p.evidence_directory,p.serial_cpu),
                             (f"xlerobot_{side}",f"{side}_arm",Path(f"/etc/robot/xlerobot-{side}-arm.toml"),
                              Path(f"configs/xlerobot-{side}-arm.toml"),Path(f"/var/lib/robot/xlerobot_{side}"),
                              f"/dev/serial/by-id/usb-1a86_USB_Single_Serial_{serial}-if00",serial,
                              f"{side}_arm.json",f"orin_sla_{side}",cpu))
            with self.assertRaises(AttributeError): p.side="other"
        for side in (None,"LEFT","other",1):
            with self.assertRaises(ValueError):report.arm_profile(side)

    def test_both_side_plan_only_has_no_effect_and_default_is_left(self):
        for argv,side in (([],"left"),(["--side","left"],"left"),(["--side","right"],"right")):
            with self.subTest(side=side),patch.object(driver.subprocess,"run",side_effect=AssertionError), \
                 patch.object(driver.subprocess,"Popen",side_effect=AssertionError), \
                 patch.object(Path,"read_text",side_effect=AssertionError),patch.object(Path,"mkdir",side_effect=AssertionError), \
                 contextlib.redirect_stdout(io.StringIO()) as output:
                self.assertEqual(driver.main(argv),0)
                self.assertEqual(json.loads(output.getvalue())["side"],side)
        with contextlib.redirect_stderr(io.StringIO()),self.assertRaises(SystemExit):driver.main(["--side","other"])
        self.assertEqual(driver.tool_command(Path("/bin"),Path("/run")),driver.tool_command(Path("/bin"),Path("/run"),"left"))

    def test_facts_require_three_way_side_and_keep_legacy_left(self):
        for side in ("left","right"):
            options,inputs,alignment,physical=side_facts(Path("/new/run"),side)
            driver.validate_facts(inputs,alignment,physical,options,12)
            for candidate in ("right" if side=="left" else "left","other",None,True):
                for part in (inputs,physical):
                    saved=part["side"];part["side"]=candidate
                    with self.subTest(side=side,candidate=candidate),self.assertRaises(ValueError):
                        driver.validate_facts(inputs,alignment,physical,options,12)
                    part["side"]=saved
            del inputs["side"]
            if side=="left":driver.validate_facts(inputs,alignment,physical,options,12)
            else:
                with self.assertRaises(ValueError):driver.validate_facts(inputs,alignment,physical,options,12)

    def test_backend_preflight_wrong_returned_trust_rejects_before_commands(self):
        with tempfile.TemporaryDirectory() as temp:
            options,inputs,alignment,physical=side_facts(Path(temp),"right")
            runtime={"schema":"P7_RUNTIME_PRECONDITIONS_V1","uid":1000,"euid":1000,"gid":1000,"egid":1000,
                     "account_uid":1000,"account_gid":1000,"machine":"aarch64","python_version":[3,12,0],
                     "rtprio":[99,99],"memlock":[-1,-1],"tools":{name:"/usr/bin/"+name for name in
                     ("taskset","git","systemctl","udevadm","fuser","sha256sum","sudo")},
                     "trust":{"path":str(driver.TRUST),"is_directory":True,"is_symlink":False,"mode":0o750,"uid":1000,"gid":1000}}
            backend=driver.RealBackend(options,driver.Journal(options.output,options))
            with patch.object(driver,"pinned_json",side_effect=[inputs,alignment,physical]), \
                 patch.object(driver,"inspect_runtime_preconditions",return_value=runtime) as inspect, \
                 patch.object(backend,"command",side_effect=AssertionError):
                with self.assertRaisesRegex(ValueError,"TRUST路径"):backend.preflight()
                inspect.assert_called_once_with(report.arm_profile("right").trust)
            self.assertEqual(backend.journal.state["authorization_attempts"],0)
            runtime["trust"]["path"]=str(backend.profile.trust)
            driver.validate_runtime_preconditions(runtime,backend.profile.trust)
            with self.assertRaises(ValueError):driver.validate_runtime_preconditions(runtime)

    def test_backend_maintenance_markers_and_launch_use_selected_side(self):
        for side in ("left","right"):
            with tempfile.TemporaryDirectory() as temp:
                options,_,_,_=side_facts(Path(temp),side)
                backend=driver.RealBackend(options,driver.Journal(options.output,options));p=backend.profile
                backend.production={"artifacts":{"robot-s0":{"path":"/frozen/s0"}}}
                backend.uid=1000;backend.gid=1000;backend.bin_dir=Path("/frozen/measurement")
                with patch.object(backend,"command",return_value="ok") as command:
                    for action in ("status","revoke","authorize"):
                        backend.trust(action)
                        self.assertEqual(command.call_args.args[0],["/frozen/s0","rx-trust",action,"--robot-id",p.robot_id,"--rx-trust-dir",str(p.trust)])
                with patch.object(driver,"marker_snapshot",return_value={}) as marker:
                    backend.markers(b"1\n");marker.assert_called_once_with(p.trust,b"1\n",1000,1000)
                with patch.object(driver.subprocess,"Popen",return_value=object()) as popen:
                    backend.launch()
                    argv=popen.call_args.args[0]
                    self.assertEqual(argv[argv.index("--hardware-id")+1],p.hardware_id)
                    self.assertEqual(argv[argv.index("--config")+1],str(p.config))
                backend.close()

    def test_backend_report_passes_explicit_side_and_approved_config(self):
        for side in ("left","right"):
            options,_,_,_=side_facts(Path("/fixture/output"),side)
            backend=driver.RealBackend(options,driver.Journal(options.output,options))
            backend.bin_dir=Path("/fixture/bin")
            backend.inputs={"config_sha256":"a"*64,"calibration":{"sha256":"b"*64}}
            backend.measurement={"artifacts":{"comm_fault_e2e":{"sha256":"c"*64,"sidecar":{}}}}
            with patch.object(driver,"pinned_json",return_value={"fixture":True}), \
                 patch.object(driver,"verify_report",return_value={}) as verify:
                backend.report(100)
                directory,expected=verify.call_args.args
                self.assertEqual(directory,options.output)
                self.assertEqual((expected["side"],expected["config"]),(side,str(report.arm_profile(side).config)))

    def test_config_check_rejects_each_cross_side_identity(self):
        for side in ("left","right"):
            with tempfile.TemporaryDirectory() as temp:
                root=Path(temp);options,_,_,_=side_facts(root,side)
                backend=driver.RealBackend(options,driver.Journal(root,options));p=backend.profile
                calibration=root/p.calibration_name;calibration.write_text("{}")
                installed=root/"installed.toml";tracked=root/"tracked.toml"
                backend.profile=replace(p,config=installed,repository_config=Path("tracked.toml"))
                config={"robot":{"id":p.robot_id},"hardware":[{"id":p.hardware_id,"address":p.device,
                        "adapter":"feetech_sts3215","motor_ids":list(range(1,7)),"auto_enable":False,
                        "calibration_paths":[str(calibration)]}]}
                installed.write_text("fixture");tracked.write_text("fixture")
                backend.inputs={"repository":str(root),"config_sha256":driver.file_hash(installed),
                                "calibration":{"path":str(calibration),"sha256":driver.file_hash(calibration)}}
                with patch.object(driver.tomllib,"loads",return_value=config):backend.config_check()
                other=report.arm_profile("right" if side=="left" else "left")
                for key,bad in (("id",other.hardware_id),("address",other.device),("auto_enable",True),
                                ("calibration_paths",[str(root/other.calibration_name)])):
                    broken=copy.deepcopy(config);broken["hardware"][0][key]=bad
                    with self.subTest(side=side,key=key),patch.object(driver.tomllib,"loads",return_value=broken),self.assertRaises(ValueError):backend.config_check()
                config["robot"]["id"]=other.robot_id
                with patch.object(driver.tomllib,"loads",return_value=config),self.assertRaises(ValueError):backend.config_check()

    def test_right_device_requires_right_usb_serial(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);port=root/"port";port.write_text("fixture")
            options,_,_,_=side_facts(root,"right")
            backend=driver.RealBackend(options,driver.Journal(root,options));backend.profile=replace(backend.profile,device=str(port))
            for serial,valid in (("5A7C122340",True),("5A7C121427",False)):
                properties=f"ID_VENDOR_ID=1a86\nID_MODEL_ID=55d3\nID_SERIAL_SHORT={serial}\nID_USB_DRIVER=cdc_acm\n"
                replies=[subprocess.CompletedProcess([],0,properties,""),subprocess.CompletedProcess([],1,"","")]
                with patch.object(driver.stat,"S_ISCHR",return_value=True),patch.object(driver.subprocess,"run",side_effect=replies) as run:
                    if valid:self.assertEqual(backend.device_check()["path"],str(port))
                    else:
                        with self.assertRaises(ValueError):backend.device_check()
                    self.assertEqual(run.call_args_list[0].args[0][-1],str(port))

    def test_selected_side_rejects_running_units_on_either_bus(self):
        text="\n\n".join(f"Id={u}\nLoadState=loaded\nActiveState=inactive\nSubState=dead\nJob=0" for u in driver.UNITS)
        for unit in driver.UNITS:
            blocks=text.split("\n\n");index=next(i for i,b in enumerate(blocks) if b.startswith("Id="+unit))
            blocks[index]=blocks[index].replace("ActiveState=inactive","ActiveState=active").replace("SubState=dead","SubState=running")
            with self.subTest(unit=unit),self.assertRaises(ValueError):driver.validate_services("\n\n".join(blocks))


class RightReportTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp=tempfile.TemporaryDirectory();cls.root=Path(cls.temp.name)
        cls.expected=report_fixture(cls.root,"right")
    @classmethod
    def tearDownClass(cls):cls.temp.cleanup()

    def test_complete_right_report_passes_and_hashes_final_trace(self):
        value=report.verify_report(self.root,self.expected)
        self.assertEqual(value["samples"],4000)
        self.assertIn("orin_sla_right/final_trace.json",value["original_hashes"])

    def test_side_cpu_profile_and_actual_thread_cannot_cross(self):
        for side,wrong in (("left","right"),("right","left")):
            report.verify_rt(rt_fixture(side),100,side)
            with self.assertRaises(ValueError):report.verify_rt(rt_fixture(side),100,wrong)
            rt=rt_fixture(side);rt["processes"][1]["snapshot"]["threads"][1]["cpus"]=[report.arm_profile(wrong).serial_cpu]
            with self.assertRaises(ValueError):report.verify_rt(rt,100,side)

    def test_wrong_side_report_identity_and_directory_rejected(self):
        path=self.root/"report.json";saved=path.read_bytes()
        for key,bad in (("hardware_id","left_arm"),("config",str(driver.CONFIG))):
            try:
                value=json.loads(saved);value["invocation"][key]=bad;write_json(path,value)
                with self.assertRaises(ValueError):report.verify_report(self.root,self.expected)
            finally:path.write_bytes(saved)
        env_path=self.root/"orin_sla_right/environment.json";env_saved=env_path.read_bytes()
        try:
            value=json.loads(saved);env=json.loads(env_saved);env["address"]=driver.DEVICE
            value["cases"][0]["evidence"]["environment"]=env;write_json(path,value);write_json(env_path,env)
            with self.assertRaises(ValueError):report.verify_report(self.root,self.expected)
        finally:path.write_bytes(saved);env_path.write_bytes(env_saved)
        with self.assertRaises(ValueError):report.verify_report(self.root,{**self.expected,"side":"left"})
        selected=self.root/"orin_sla_right";other=self.root/"orin_sla_left";selected.rename(other)
        try:
            with self.assertRaises(ValueError):report.verify_report(self.root,self.expected)
        finally:other.rename(selected)

    def test_final_trace_requires_original_and_strict_report_equality(self):
        path=self.root/"orin_sla_right/final_trace.json";saved=path.read_bytes();path.unlink()
        try:
            with self.assertRaises(ValueError):report.verify_report(self.root,self.expected)
        finally:path.write_bytes(saved)
        report_path=self.root/"report.json";payload=report_path.read_bytes()
        for key,wrong in (("valid",1),("dropped",False),("dropped",0.0)):
            try:
                value=json.loads(payload);value["cases"][0]["evidence"]["final_trace"][key]=wrong;write_json(report_path,value)
                with self.subTest(key=key,wrong=wrong),self.assertRaises(ValueError):report.verify_report(self.root,self.expected)
            finally:report_path.write_bytes(payload)

    def test_final_trace_rejects_unknown_sticky_unstopped_and_boolean_numbers(self):
        base=json.loads((self.root/"orin_sla_right/final_trace.json").read_text())
        boundaries=[json.loads(line) for line in (self.root/"orin_sla_right/boundary.jsonl").read_text().splitlines()]
        report.verify_final_trace(base,boundaries)
        changes=[("valid",False),("valid",1),("children_stopped",False),("installed",False),
                 ("dropped",1),("dropped",False),("error","error"),("read_error","read"),("boundary",None)]
        for key,value in changes:
            bad=copy.deepcopy(base);bad[key]=value
            with self.subTest(key=key,value=value),self.assertRaises(ValueError):report.verify_final_trace(bad,boundaries)
        for key,value in (("request_seq",0),("receipt_seq",1),("request_seq",True),("mode","None"),
                          ("request_mode","Some(ArmIdle)"),("status","Ready"),("armed_seq",1),
                          ("armed_seq",False),("failure_seq",1),("failure_seq",False),("gate_active",True),
                          ("gate_active",0),("failure","Some(DelayInvalid)")):
            bad=copy.deepcopy(base);bad["boundary"][key]=value
            with self.subTest(key=key,value=value),self.assertRaises(ValueError):report.verify_final_trace(bad,boundaries)
        bad=copy.deepcopy(base);bad["boundary"].update(request_seq=1,receipt_seq=1)
        with self.assertRaises(ValueError):report.verify_final_trace(bad,boundaries)
        for key in base:
            bad=copy.deepcopy(base);del bad[key]
            with self.subTest(missing=key),self.assertRaises((ValueError,KeyError)):report.verify_final_trace(bad,boundaries)

if __name__=="__main__": unittest.main()
