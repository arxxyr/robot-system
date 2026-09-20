"""H2 驱动离线自测；仅临时文件、fake 后端与无害 Python 子进程。"""
from __future__ import annotations

import argparse
import copy
import contextlib
import hashlib
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
import post_e_h2_report as report
import run_post_e_h2 as driver


def write_json(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False))


def ack(request_id, dry=False):
    return {"ok": True, "kind": "ack", "request_id": request_id,
            "dry_run": dry, "cleared_at_ns": 10}


def rt_fixture():
    processes = []
    for role, pid in (("tool", 100), ("s0", 101), ("gateway", 102)):
        threads = [{"tid": pid, "comm": role, "policy": 0, "priority": 0, "cpus": [0, 1, 2, 3]}]
        if role == "s0":
            threads.append({"tid": 103, "comm": "s0_serial_io_th", "policy": 1, "priority": 70, "cpus": [4]})
        tids = sorted(t["tid"] for t in threads)
        processes.append({"role": role, "validation_error": None, "snapshot": {
            "pid": pid, "tids_before": tids, "tids_after": tids, "threads": threads, "errors": []}})
    return {"started_ns": 1, "finished_ns": 2, "passed": True, "processes": processes,
            "profile": {"serial_cpu": 4, "serial_policy": 1, "serial_priority": 70,
                        "ordinary_policy": 0, "ordinary_priority": 0, "ordinary_cpus": [0, 1, 2, 3]}}


def boundary(seq, mode, now):
    return {"sequence": seq, "mode": mode, "started_ns": now, "finished_ns": now+1, "error": None,
            "snapshot": {"request_seq": seq, "receipt_seq": seq, "request_mode": f"Some({mode})",
                         "mode": f"Some({mode})", "status": "Disarmed" if mode == "Disarm" else "Ready",
                         "armed_seq": 0 if mode == "Disarm" else seq, "ready_ns": now+1,
                         "request_deadline_ns": now+100_000, "failure": "None"}}


def report_fixture(root):
    """合成报告只测试解析/判据，不模拟或宣称真实电机时序。"""
    evidence = root / "orin_sla_left"; evidence.mkdir()
    bin_dir = root / "bin"; bin_dir.mkdir()
    artifacts = {name: {"sha256": "a"*64, "build": {"fixture": name}} for name in
                 ("s0_daemon", "robot_rpc_gateway", "comm_fault_e2e")}
    expected = {"bin_dir": str(bin_dir), "config": str(driver.CONFIG), "config_sha256": "b"*64,
                "calibration_sha256": "c"*64, "artifacts": artifacts, "tool_pid": 100}
    env = {"gateway_idempotency_max_entries": 8192, "gateway_idempotency_ttl_secs": 300,
           "background_tracked_limit": 64, "wire_active_limit_unchanged": 4, "s48_cache_observation": False,
           "hardware_id": "left_arm", "address": driver.DEVICE, "current_exe": str(bin_dir / "comm_fault_e2e"),
           "config_sha256": expected["config_sha256"], "calibration_sha256": expected["calibration_sha256"],
           "artifacts": {role: artifacts[name]["build"] for role, name in
                         (("tool", "comm_fault_e2e"), ("s0", "s0_daemon"), ("gateway", "robot_rpc_gateway"))},
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
    rt=rt_fixture()
    measurement={"groups": groups, "background": background, "trace_dropped": 0, "hardware_only": True,
                 "runtime_only": False, "in_flight_exercised": True, "mock_lease": False,
                 "external_entry_exercised": True, "topic_entry_exercised": True, "idle_exercised": True,
                 "events_csv": str(evidence / "events.csv")}
    payload={"schema_version": 1, "status": "PASS", "hardware": {"H1":"NEEDS_HUMAN", "H2":"PASS", "H3":"NEEDS_HUMAN", "D15":"OPEN"},
             "invocation": {"phase":"OrinSla", "case_filter":None, "hardware_id":"left_arm", "config":str(driver.CONFIG),
                            "config_sha256":"b"*64, "current_exe":str(bin_dir/"comm_fault_e2e"), "current_exe_sha256":"a"*64},
             "artifacts":[{"name":n, "path":str(bin_dir/n), **v} for n,v in artifacts.items()],
             "cases":[{"case":"orin_sla", "ac":"AC-12/H2", "status":"PASS", "evidence":{
                 "environment":env, "measurement":measurement, "final_state":final, "rt_preflight":rt}}]}
    write_json(root/"report.json",payload)
    for name,value in (("environment.json",env),("background.json",background),("recovery_writes.json",recoveries),
                       ("rt_preflight.json",rt),("final_stop.json",final)):
        write_json(evidence/name,value)
    for group in groups:
        write_json(evidence/f"sla_{str(group['external']).lower()}_{str(group['in_flight']).lower()}.json",group)
    for name,rows in (("attempts.jsonl",attempts),("boundary.jsonl",boundaries)):
        (evidence/name).write_text("\n".join(json.dumps(row) for row in rows)+"\n")
    (evidence/"events.csv").write_text("\n".join(csv_lines)+"\n")
    for name in ("s0.stdout.log","s0.stderr.log","gateway.stdout.log","gateway.stderr.log"):
        (evidence/name).write_text("")
    return expected


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
        text="\n\n".join(f"Id={u}\nLoadState=loaded\nActiveState=inactive\nSubState=dead\nMainPID=0" for u in driver.UNITS)
        self.assertEqual(len(driver.validate_services(text)),6)
        for broken in (text.replace("inactive","failed",1), text.replace("MainPID=0","MainPID=1",1), text.split("\n\n",1)[1]):
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
        type(self).instances.append(self)

    def preflight(self):
        if self.failure=="preflight": raise ValueError("预检失败")

    def audit(self): return {"fixture":True}
    def trust(self,action): self.actions.append(action)
    def markers(self,expected): return {"expected":expected.hex(),"fixture":True}
    def identity(self,pid): return driver.process_identity(pid)
    def launch(self):
        self.actions.append("Popen")
        if self.failure=="launch": raise OSError("明确Popen失败")
        self.process=subprocess.Popen([sys.executable,"-c","import time; time.sleep(0.08)"],start_new_session=True)
        if self.failure == "born_exception": raise OSError("出生后句柄交接异常")
        if self.failure == "born_signal": os.kill(os.getpid(), signal.SIGINT)
        return self.process
    def observe(self,process,journal):
        if self.failure=="observe": raise OSError("模拟不可读进程")
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
        for failure in ("observe","report","cleanup","born_exception","born_signal"):
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
        for field, value in (("head","f"*40),("status_porcelain"," M scripts/run_post_e_h2.py")):
            bad = copy.deepcopy(alignment); bad["hosts"][1][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                driver.validate_facts(inputs,bad,physical,options,12)

    def test_real_backend_command_records_original_failure_and_rejects_partial_authorize(self):
        with tempfile.TemporaryDirectory() as temp:
            options = self.options(Path(temp))
            backend = driver.RealBackend(options, driver.Journal(options.output,options))
            backend.production = {"artifacts":{"s0_daemon":{"path":"/fixture/s0_daemon"}}}
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
        import post_e_h2_artifacts as artifacts
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
                with self.subTest(code=code,stderr=stderr),patch.object(driver,"DEVICE",str(port)), \
                     patch.object(driver.stat,"S_ISCHR",return_value=True),patch.object(driver.subprocess,"run",side_effect=replies):
                    if valid:self.assertEqual(backend.device_check()["path"],str(port))
                    else:
                        with self.assertRaises(ValueError):backend.device_check()

    def test_unreadable_process_is_not_clean(self):
        with patch.object(Path,"iterdir",return_value=[Path("/fake/123")]), \
             patch.object(Path,"read_text",side_effect=PermissionError("不可读")):
            value = driver.process_snapshot(123,Path("/fake/run"),[],Path("/fake"))
        self.assertFalse(value["clean"])
        self.assertEqual(value["unreadable"][0]["pid"],123)


if __name__=="__main__": unittest.main()
