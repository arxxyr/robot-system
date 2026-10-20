"""爸爸，维护收尾确定性测试；只用假寄存器及自有短命子进程，不访问硬件。"""
import contextlib
import copy
import hashlib
import io
import json
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
import run_hardware_maintenance as m

PLAN = {'position_targets':m.TARGETS, 'position_delta_raw':24,'minimum_excursion_raw':12,'return_tolerance_raw':4,
        'position_wait_ms':800,'base_raw':[20,-20],'pulse_target_ms':150,
        'nonzero_window_fail_ns':1_000_000_000,'observed_velocity_raw_max':40,
        'observed_wheel_delta_raw_min':3,'observed_wheel_delta_raw_max':64}
CALI = {'id':5,'range_min':73,'range_max':3864}


class FakeIO:
    def __init__(self, fail_at=None, *, stuck=False, velocity=None, delta=None, step=None):
        self.t=0; self.normal=0; self.fail_at=fail_at; self.stuck=stuck
        self.velocity=velocity; self.delta=delta; self.calls=[]; self.fail_cleanup=set()
        self.position=1000; self.goal=1000; self.torque=0; self.goal_velocity=0
        self.mode=0; self.drift=0; self.slow=False
        self.step=step

    def now(self): return self.t
    def check(self): pass

    def checkpoint(self, cleanup):
        if not cleanup:
            self.normal+=1
            if self.normal==self.fail_at:
                raise RuntimeError('注入：动作已生效而回执/控制点失败')

    def sleep(self, seconds, cleanup=False):
        self.calls.append(('sleep',seconds,cleanup))
        self.t+=int(seconds*1e9)
        if not cleanup and self.slow and seconds==0.15: self.t+=1_000_000_000
        if not cleanup and seconds==0.05 and self.t==70_000_000: self.position+=self.drift
        if cleanup and 'sleep' in self.fail_cleanup: raise RuntimeError('cleanup sleep失败')
        self.checkpoint(cleanup)

    def poke(self, side, motor, action, register, value=None, cleanup=False):
        self.calls.append((side,motor,action,register,value,cleanup))
        # 先实施真实副作用，再注入ACK/wait错误，验证不确定写也必定停机。
        if action=='write':
            if register=='Torque_Enable': self.torque=value
            elif register=='Goal_Position': self.goal=value
            elif register=='Goal_Velocity': self.goal_velocity=value
            else: raise AssertionError('禁止的维护写')
        if cleanup and register in self.fail_cleanup: raise RuntimeError('cleanup寄存器失败')
        self.checkpoint(cleanup)
        if action=='write': return None
        if register=='Operating_Mode': return self.mode
        if register=='Torque_Enable': return self.torque
        if register=='Goal_Position': return self.goal
        if register=='Goal_Velocity': return self.goal_velocity
        if register=='Present_Velocity':
            return (self.velocity if self.velocity is not None else self.goal_velocity) if self.torque else 0
        if register=='Present_Position':
            if self.torque and not self.stuck:
                if self.mode==0:
                    difference=self.goal-self.position
                    self.position+=difference if self.step is None else max(-self.step,min(self.step,difference))
                elif self.goal_velocity:
                    self.position=(self.position+(self.delta if self.delta is not None else (6 if self.goal_velocity>0 else -6)))%4096
            return self.position
        raise AssertionError(register)


class ProbeTests(unittest.TestCase):
    def quiet(self): return contextlib.redirect_stdout(io.StringIO())

    def test_position_and_wheel_positive(self):
        a=FakeIO(); e=m.ProbeEngine(a,PLAN)
        e.position('left',5,'left_arm_wrist_roll',CALI)
        self.assertEqual(e.rows[0]['status'],'PASS'); self.assertEqual((a.torque,a.position,a.goal),(0,1000,1000))
        a=FakeIO(); a.mode=1; e=m.ProbeEngine(a,PLAN); e.wheel(8)
        self.assertEqual(e.rows[0]['status'],'PASS'); self.assertEqual((a.torque,a.goal_velocity),(0,0))
        self.assertEqual([p['delta_raw'] for p in e.rows[0]['pulses']],[6,-6])
        self.assertTrue(all(p['zero_write_confirmed'] for p in e.rows[0]['pulses']))

    def test_every_position_action_point_stops_after_applied_write(self):
        baseline=FakeIO(step=8); m.ProbeEngine(baseline,PLAN).position('left',5,'wrist',CALI)
        cases=0
        for index in range(1,baseline.normal+1):
            a=FakeIO(index,step=8); e=m.ProbeEngine(a,PLAN)
            with self.subTest(index=index),self.quiet(),self.assertRaises(ValueError):
                e.position('left',5,'wrist',CALI)
            self.assertEqual(a.torque,0)
            self.assertIn(('left',5,'write','Torque_Enable',0,True),a.calls)
            self.assertEqual(e.rows[0]['status'],'FAIL'); cases+=1
        self.assertEqual(cases,baseline.normal)

    def test_normal_multiread_outward_and_return_movement(self):
        a=FakeIO(step=8); e=m.ProbeEngine(a,PLAN)
        e.position('left',5,'wrist',CALI)
        self.assertEqual([s['position_raw'] for s in e.rows[0]['outward']],[1008,1016,1024])
        self.assertEqual([s['position_raw'] for s in e.rows[0]['return']],[1016,1008,1000])
        self.assertEqual(e.rows[0]['status'],'PASS'); self.assertEqual(a.torque,0)

    def test_every_wheel_action_point_zero_then_off_after_applied_write(self):
        baseline=FakeIO(); baseline.mode=1; m.ProbeEngine(baseline,PLAN).wheel(8)
        for index in range(1,baseline.normal+1):
            a=FakeIO(index); a.mode=1; e=m.ProbeEngine(a,PLAN)
            with self.subTest(index=index),self.quiet(),self.assertRaises(ValueError): e.wheel(8)
            self.assertEqual((a.torque,a.goal_velocity),(0,0))
            self.assertIn(('right',8,'write','Goal_Velocity',0,True),a.calls)
            self.assertIn(('right',8,'write','Torque_Enable',0,True),a.calls)
            self.assertEqual(e.rows[0]['status'],'FAIL')

    def test_cleanup_errors_never_skip_independent_torqueoff(self):
        for failure in ('Goal_Velocity','Torque_Enable','Present_Velocity','sleep'):
            a=FakeIO(); a.mode=1; a.fail_cleanup={failure}; e=m.ProbeEngine(a,PLAN)
            with self.subTest(failure=failure),self.quiet(),self.assertRaises(ValueError): e.wheel(8)
            self.assertIn(('right',8,'write','Torque_Enable',0,True),a.calls)
            self.assertTrue(e.manual_cut)
            self.assertEqual(e.rows[0]['status'],'FAIL')

    def test_zero_position_motion_fails(self):
        a=FakeIO(stuck=True); e=m.ProbeEngine(a,PLAN)
        with self.quiet(),self.assertRaises(ValueError): e.position('left',5,'wrist',CALI)
        self.assertEqual(a.torque,0); self.assertEqual(e.rows[0]['status'],'FAIL')

    def test_wheel_observed_velocity_and_delta_limits_fail_and_stop(self):
        for velocity,delta in [(0,None),(1000,None),(-20,None),(None,0),(None,2),(None,-6),(None,65)]:
            a=FakeIO(velocity=velocity,delta=delta); a.mode=1; e=m.ProbeEngine(a,PLAN)
            with self.subTest(velocity=velocity,delta=delta),self.quiet(),self.assertRaises(ValueError): e.wheel(8)
            self.assertEqual((a.torque,a.goal_velocity),(0,0))
            self.assertEqual(len(e.rows[0]['pulses']),1)

    def test_modular_encoder_both_wrap_directions(self):
        for start,sign in [(4093,20),(2,-20)]:
            a=FakeIO(); a.mode=1; a.torque=1; a.position=start; e=m.ProbeEngine(a,PLAN)
            pulse=e.pulse(8,sign)
            self.assertEqual(pulse['status'],'PASS'); self.assertEqual(pulse['delta_raw'],6 if sign>0 else -6)

    def test_long_actual_nonzero_window_fails_not_sleep_claim(self):
        a=FakeIO(); a.mode=1; a.slow=True; e=m.ProbeEngine(a,PLAN)
        with self.quiet(),self.assertRaises(ValueError): e.wheel(8)
        self.assertGreater(e.rows[0]['pulses'][0]['nonzero_window_ns'],1_000_000_000)
        self.assertEqual((a.torque,a.goal_velocity),(0,0))

    def test_mode_calibration_and_anchor_drift_refuse_enable(self):
        for mode,cali,drift in [(1,CALI,0),(0,{**CALI,'id':6},0),(0,{**CALI,'range_max':1010},0),(0,CALI,5)]:
            a=FakeIO(); a.mode=mode; a.drift=drift; e=m.ProbeEngine(a,PLAN)
            with self.subTest(mode=mode,cali=cali,drift=drift),self.quiet(),self.assertRaises(ValueError): e.position('left',5,'wrist',cali)
            self.assertNotIn(('left',5,'write','Torque_Enable',1,False),a.calls)

    def test_failure_prevents_next_motor(self):
        a=FakeIO(fail_at=10); e=m.ProbeEngine(a,PLAN)
        with self.quiet(),self.assertRaises(ValueError):
            e.position('left',5,'wrist',CALI)
            e.wheel(8)
        self.assertEqual(len(e.rows),1)

    def test_final_snapshot_rejects_unexpected_changes_and_unsafe_readback(self):
        rows={}
        for side in m.SERIALS:
            for motor in range(1,9 if side=='left' else 10):
                rows[f'{side}:{motor}']={r:0 for r in m.REGS}
                rows[f'{side}:{motor}']['Operating_Mode']=1 if side=='right' and motor>=7 else 0
                rows[f'{side}:{motor}']['Present_Position']=1000
        m.validate_snapshot(rows)
        for reg,value in [('Torque_Enable',1),('Goal_Velocity',20),('Present_Velocity',2),('Goal_Position',1),('Present_Position',1005),('Operating_Mode',1)]:
            changed=copy.deepcopy(rows); changed['left:1'][reg]=value
            with self.subTest(reg=reg),self.assertRaises(ValueError): m.validate_snapshot(changed,rows)


class OwnerTests(unittest.TestCase):
    def make_io(self,d): return m.RealExecutor({'path':'/not-hardware/fake'},Path(d))

    def test_actual_child_signal_request_wait_and_reap_without_kill(self):
        with tempfile.TemporaryDirectory() as d:
            e=self.make_io(d); timer=threading.Timer(.02,e.mark_signal,args=(signal.SIGHUP,None)); timer.start()
            with self.assertRaises(ValueError): e.command([sys.executable,'-c','import time;time.sleep(.08);print("ok")'])
            timer.join(); self.assertIsNone(e.child)
            self.assertTrue(e.commands[0]['reaped']); self.assertEqual(e.commands[0]['signals_sent'],[])
            self.assertEqual(e.command([sys.executable,'-c','print("stop")'],cleanup=True),'stop\n')
            e.journal.close()

    def test_wait_exception_keeps_actual_child_then_allows_cleanup(self):
        real_popen=subprocess.Popen
        class WaitOnce:
            def __init__(self,*args,**kwargs): self.raw=real_popen(*args,**kwargs); self.pid=self.raw.pid; self.fail=True
            def wait(self,timeout):
                if self.fail: self.fail=False; raise OSError('真实child已启动后wait注入错误')
                return self.raw.wait(timeout=timeout)
        with tempfile.TemporaryDirectory() as d,patch.object(m.subprocess,'Popen',WaitOnce):
            e=self.make_io(d)
            with self.assertRaises(ValueError): e.command([sys.executable,'-c','print("ok")'])
            self.assertIsNone(e.child); self.assertTrue(e.commands[0]['reaped'])
            self.assertTrue(e.commands[0]['owner_errors'])
            with self.assertRaises(ValueError): e.command([sys.executable,'-c','print("cleanup")'],cleanup=True)
            self.assertTrue(e.commands[1]['reaped']); e.journal.close()

    def test_stderr_and_evidence_failure_are_sticky_after_reap(self):
        with tempfile.TemporaryDirectory() as d:
            e=self.make_io(d)
            with self.assertRaises(ValueError): e.command([sys.executable,'-c','import sys;print("error",file=sys.stderr)'])
            self.assertIsNone(e.child); self.assertTrue(e.commands[0]['reaped'])
            e.journal.close()
            with self.assertRaises(ValueError): e.command([sys.executable,'-c','print("stopped")'],cleanup=True)
            self.assertTrue(e.commands[1]['reaped']); self.assertIsNone(e.child)

    def test_default_plan_only_never_spawns(self):
        with patch.object(m.subprocess,'Popen',side_effect=AssertionError('PLAN_ONLY不许启动child')),contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(m.main([]),0)
        self.assertEqual(json.loads(output.getvalue())['actions_started'],0)

    def test_actual_cli_default_plan_only_exit_zero(self):
        for args in ([],['--help']):
            child=subprocess.Popen([sys.executable,str(Path(m.__file__)),*args],stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            out,err=child.communicate()
            self.assertEqual(child.wait(),0); self.assertEqual(err,b'')
            if not args:self.assertEqual(json.loads(out)['actions_started'],0)

    def test_go_rejects_wrong_plan_missing_facts_and_expiration(self):
        plan={'schema':'MAINTENANCE_PLAN_V2','artifact_root':'/home/loosqk/robot-checks/test',
              'position_targets':m.TARGETS, 'bindings':{'host':{'boot_id':'test'}}}
        go={'schema':'MAINTENANCE_GO_V2','artifact_root':plan['artifact_root'],'issued_by':'root','plan_sha256':m.digest(plan),
            'issued_at_ns':100,'expires_at_ns':1000,'host':plan['bindings']['host'],'source':'当次用户现场答复',
            'physical':{s:{**{f:True for f in m.FACTS},'confirmed_at_ns':100} for s in m.SERIALS}}
        m.validate_go(go,plan,200)
        for change in ('digest','facts','expire'):
            bad=copy.deepcopy(go)
            if change=='digest':bad['plan_sha256']='0'*64
            if change=='facts':bad['physical']['left']['supported']=False
            if change=='expire':bad['expires_at_ns']=150
            with self.subTest(change=change),self.assertRaises(ValueError):m.validate_go(bad,plan,200)

    def test_go_accepts_current_five_facts_without_power_cycle_or_usb_replug(self):
        facts={'supported':True,'unloaded':True,'area_clear':True,
               'continuous_presence_until_cleanup':True,'immediate_power_cut':True}
        self.assertEqual(set(m.FACTS),set(facts))
        plan={'schema':'MAINTENANCE_PLAN_V2','artifact_root':'/home/loosqk/robot-checks/test',
              'position_targets':m.TARGETS,'bindings':{'host':{'boot_id':'test'}}}
        go={'schema':'MAINTENANCE_GO_V2','artifact_root':plan['artifact_root'],'issued_by':'root',
            'plan_sha256':m.digest(plan),'issued_at_ns':100,'expires_at_ns':1000,
            'host':plan['bindings']['host'],'source':'当次用户现场答复',
            'physical':{side:{**facts,'confirmed_at_ns':100} for side in m.SERIALS}}
        m.validate_go(go,plan,200)

    def test_go_rejects_missing_or_false_watch_and_immediate_power_cut(self):
        facts={'supported':True,'unloaded':True,'area_clear':True,
               'continuous_presence_until_cleanup':True,'immediate_power_cut':True}
        plan={'schema':'MAINTENANCE_PLAN_V2','artifact_root':'/home/loosqk/robot-checks/test',
              'position_targets':m.TARGETS,'bindings':{'host':{'boot_id':'test'}}}
        go={'schema':'MAINTENANCE_GO_V2','artifact_root':plan['artifact_root'],'issued_by':'root',
            'plan_sha256':m.digest(plan),'issued_at_ns':100,'expires_at_ns':1000,
            'host':plan['bindings']['host'],'source':'当次用户现场答复',
            'physical':{side:{**facts,'confirmed_at_ns':100} for side in m.SERIALS}}
        for side in m.SERIALS:
            for key in ('continuous_presence_until_cleanup','immediate_power_cut'):
                for missing in (False,True):
                    bad=copy.deepcopy(go)
                    if missing:del bad['physical'][side][key]
                    else:bad['physical'][side][key]=False
                    with self.subTest(side=side,key=key,missing=missing),self.assertRaises(ValueError):
                        m.validate_go(bad,plan,200)

    def test_strict_poke_stdout_rejects_extra_lines(self):
        with tempfile.TemporaryDirectory() as d:
            e=self.make_io(d)
            with patch.object(m,'verify_ref'),patch.object(e,'command',return_value='extra\nread Goal_Velocity@8 = -20\n'):
                with self.assertRaises(ValueError):e.poke('right',8,'read','Goal_Velocity')
            with patch.object(m,'verify_ref'),patch.object(e,'command',return_value='read Goal_Velocity@8 = -20\n'):
                self.assertEqual(e.poke('right',8,'read','Goal_Velocity'),-20)
            e.journal.close()


def idle_rows():
    return {key:{'Operating_Mode':1 if key.startswith('right:') and int(key.split(':')[1])>=7 else 0,
                 'Torque_Enable':0,'Present_Position':1000,'Present_Velocity':0,'Goal_Position':1000,'Goal_Velocity':0}
            for key in m.MOTOR_KEYS}


class SnapshotIO:
    def __init__(self,rows,fail=None):
        self.rows=rows; self.fail=fail; self.reads=[]; self.child=None; self.evidence_errors=[]
    def poke(self,side,motor,action,register,cleanup=False):
        key=f'{side}:{motor}'; self.reads.append((key,register,cleanup))
        if (key,register)==self.fail: raise OSError('原始READ不可得')
        return self.rows[key][register]


class ClassificationTests(unittest.TestCase):
    def identity_calls(self):
        names=['rx_trust_marker','rx_trust_marker.bak','estop_latch','estop_latch.bak','rx_trust.lock','estop_latch.lock']
        return (['ref:/first','ref:/next']+[f'protection:{side}/{name}' for side in m.SERIALS for name in names]
                +[f'service:{unit}' for unit in m.UNITS+m.OLD_UNITS+['xlerobot-handoff.service']]
                +['process_snapshot']+[f"holder:{side}:{'sudo' if elevated else 'ordinary'}" for side in m.SERIALS for elevated in (False,True)]
                +['usb:left','usb:right'])

    def fixture(self):
        path=Path(__file__).with_name('fixtures')/'hardware_maintenance_run02_return_error.json'
        return json.loads(path.read_text())

    def finish(self,before,after,probes,*,read_fail=None,manual=False,identity_fail=None):
        executor=SnapshotIO(after,read_fail); engine=m.ProbeEngine(executor,PLAN)
        engine.rows=copy.deepcopy(probes); engine.manual_cut=manual
        report={'before':before,'error':'原精度FAIL','received_signal':None,'child_active':False,'probes':engine.rows}
        called=[]
        def verifier(name):
            called.append(name)
            if name==identity_fail: raise OSError('后验不可得')
        names=['rx_trust_marker','rx_trust_marker.bak','estop_latch','estop_latch.bak','rx_trust.lock','estop_latch.lock']
        bindings={'usb':{'left':{},'right':{}},'refs':[{'path':'/first'},{'path':'/next'}],
                  'protection':{side:{name:{'path':f'/fake/{side}/{name}'} for name in names} for side in m.SERIALS}}
        with (patch.object(m,'verify_ref',side_effect=lambda r:verifier('ref:'+r['path'])),
                patch.object(m,'verify_protection_file',side_effect=lambda side,name,r:verifier(f'protection:{side}/{name}')),
                patch.object(m,'inspect_unit',side_effect=lambda io,unit,**k:verifier('service:'+unit)),
                patch.object(m,'inspect_processes',side_effect=lambda *a,**k:verifier('process_snapshot')),
                patch.object(m,'inspect_holder',side_effect=lambda io,device,elevated,**k:verifier('holder:'+next(s for s,d in m.DEVICES.items() if d==device)+(':sudo' if elevated else ':ordinary'))),
                patch.object(m,'usb_identity',side_effect=lambda io,side,b,**k:verifier('usb:'+side)),
                contextlib.redirect_stdout(io.StringIO())):
            m.finish_verification(report,executor,engine,bindings,None)
        report['result']=m.classify_run_result(report)
        return report,called,executor

    def test_actual_run02_plus_six_precision_fail_stop_pass_no_rewrite(self):
        fixture=self.fixture(); self.assertEqual(fixture['original_result'],'NEEDS_MANUAL_POWER_CUT')
        report,called,executor=self.finish(fixture['before'],fixture['after'],[fixture['probe']])
        self.assertEqual(report['position_verification']['status'],'FAIL')
        error=next(e for e in report['position_verification']['issues'] if e['code']=='POSITION_FINAL_ANCHOR_ERROR')
        self.assertEqual(error['signed_error_raw'],6); self.assertEqual(error['tolerance_raw'],4)
        self.assertEqual(report['stop_verification']['status'],'PASS')
        self.assertEqual(report['identity_verification']['status'],'PASS')
        self.assertEqual(report['result'],'FAIL'); self.assertEqual(called,self.identity_calls())
        self.assertEqual(len([r for r in executor.reads if r[1] in m.STOP_REGS]),51)
        self.assertEqual(self.fixture()['original_result'],'NEEDS_MANUAL_POWER_CUT')

    def test_each_of_51_stop_registers_nonzero_or_missing_requires_cut(self):
        for key in m.MOTOR_KEYS:
            for register in m.STOP_REGS:
                for value,expected in [(1,'FAIL'),(None,'UNKNOWN'),(False,'UNKNOWN')]:
                    rows=idle_rows(); rows[key][register]=value
                    with self.subTest(key=key,register=register,value=value):
                        stop=m.classify_stop_snapshot(rows)
                        self.assertEqual(stop['status'],expected)
                        report={'stop_verification':stop,'identity_verification':{'status':'PASS'}}
                        self.assertEqual(m.classify_run_result(report),'NEEDS_MANUAL_POWER_CUT')

    def test_missing_motor_stop_unknown_even_remaining_values_zero(self):
        rows=idle_rows(); del rows['right:9']
        self.assertEqual(m.classify_stop_snapshot(rows)['status'],'UNKNOWN')

    def test_stop_read_failure_keeps_cut_and_all_identity_checks_attempted(self):
        fixture=self.fixture()
        report,called,_=self.finish(fixture['before'],fixture['after'],[fixture['probe']],read_fail=('right:9','Goal_Velocity'))
        self.assertEqual(report['stop_verification']['status'],'UNKNOWN')
        self.assertEqual(report['result'],'NEEDS_MANUAL_POWER_CUT')
        self.assertEqual(called,self.identity_calls())

    def test_position_read_failure_does_not_hide_complete_stop_evidence(self):
        fixture=self.fixture()
        report,called,_=self.finish(fixture['before'],fixture['after'],[fixture['probe']],read_fail=('left:5','Present_Position'))
        self.assertEqual(report['stop_verification']['status'],'PASS')
        self.assertEqual(report['position_verification']['status'],'UNKNOWN')
        self.assertEqual(report['identity_verification']['status'],'PASS')
        self.assertEqual(report['result'],'FAIL'); self.assertEqual(called,self.identity_calls())

    def test_cleanup_ack_failure_sticky_even_51_final_zero(self):
        fixture=self.fixture(); probe={**fixture['probe'],'cleanup_errors':['Torque0 ACK失败']}
        report,_,_=self.finish(fixture['before'],fixture['after'],[probe],manual=True)
        self.assertEqual(report['stop_verification']['status'],'FAIL')
        self.assertEqual(report['result'],'NEEDS_MANUAL_POWER_CUT')
        self.assertTrue(any(e['code']=='STOP_CLEANUP_FAILED' for e in report['stop_verification']['issues']))

    def test_each_identity_failure_does_not_skip_remaining_checks(self):
        fixture=self.fixture()
        for name in self.identity_calls():
            with self.subTest(name=name):
                report,called,_=self.finish(fixture['before'],fixture['after'],[fixture['probe']],identity_fail=name)
                self.assertEqual(report['stop_verification']['status'],'PASS')
                self.assertEqual(report['identity_verification']['status'],'UNKNOWN')
                self.assertEqual(report['result'],'NEEDS_MANUAL_POWER_CUT')
                self.assertEqual(called,self.identity_calls())

    def test_return_timeout_keeps_structured_samples_and_signed_error(self):
        class ReturnPlusSix(FakeIO):
            def poke(self,*args,**kwargs):
                result=super().poke(*args,**kwargs)
                side,motor,action,register=args[:4]
                if action=='read' and register=='Present_Position' and self.torque and self.goal==1000:
                    self.position=1006; return 1006
                return result
        executor=ReturnPlusSix(); engine=m.ProbeEngine(executor,PLAN)
        with self.assertRaises(ValueError):engine.position('left',5,'wrist',CALI)
        probe=engine.rows[0]; failure=probe['accuracy_failure']
        self.assertEqual(probe['position_accuracy']['status'],'FAIL')
        self.assertEqual(probe['error_class'],'POSITION_ACCURACY_FAILED')
        self.assertEqual(failure['stage'],'return');self.assertEqual(failure['target_raw'],1000)
        self.assertEqual(failure['final_signed_error_raw'],6);self.assertTrue(failure['samples'])
        self.assertTrue(all(s['signed_error_raw']==6 for s in failure['samples']))
        self.assertEqual(executor.torque,0);self.assertFalse(engine.manual_cut)

    def test_read_deadline_boundary_rejects_only_after_800ms(self):
        class LateRead(FakeIO):
            def poke(self,*args,**kwargs):
                result=super().poke(*args,**kwargs)
                if args[2:4]==('read','Present_Position') and self.torque:self.t+=self.read_delay_ms*1_000_000
                return result
        for delay in (799,800,801):
            executor=LateRead();executor.read_delay_ms=delay;engine=m.ProbeEngine(executor,PLAN)
            with self.subTest(delay=delay):
                if delay<=800:
                    engine.position('left',5,'wrist',CALI)
                    self.assertEqual(engine.rows[0]['status'],'PASS')
                else:
                    with self.assertRaises(ValueError):engine.position('left',5,'wrist',CALI)
                    failure=engine.rows[0]['accuracy_failure']
                    self.assertEqual(failure['final_signed_error_raw'],0)
                    self.assertEqual(failure['status'],'FAIL');self.assertGreater(failure['elapsed_ns'],800_000_000)

    def test_complete_success_only_passes_all_three_evidence_types(self):
        report={'stop_verification':{'status':'PASS'},'identity_verification':{'status':'PASS'},
                'position_verification':{'status':'PASS'},'probes':[{'status':'PASS'}]*7,
                'received_signal':None,'child_active':False,'evidence_errors':[]}
        self.assertEqual(m.classify_run_result(report),'PASS')
        for key in ('stop_verification','identity_verification'):
            failed=copy.deepcopy(report);failed[key]['status']='UNKNOWN'
            self.assertEqual(m.classify_run_result(failed),'NEEDS_MANUAL_POWER_CUT')
        failed=copy.deepcopy(report);failed['position_verification']['status']='FAIL'
        self.assertEqual(m.classify_run_result(failed),'FAIL')

    def test_live_four_positions_pass_base_deferred_without_any_wheel_operation(self):
        executor=FakeIO();engine=m.ProbeEngine(executor,PLAN)
        bindings={'calibrations':{name:{'id':motor,'range_min':73,'range_max':3864} for side,motor,name in m.TARGETS},
                  'base_contract_verified':True,'firmware_contract_verified':True}
        with (patch.object(engine,'wheel',side_effect=AssertionError('live不允许wheel')) as wheel,
                self.assertRaisesRegex(ValueError,'BASE_FIRMWARE_CONTRACT_UNVERIFIED')):
            m.run_motion_probes(engine,bindings,PLAN)
        wheel.assert_not_called()
        self.assertEqual([r['status'] for r in engine.rows],['PASS']*4+['NOT_RUN']*3)
        self.assertTrue(all(r['decision']=='DEFERRED' for r in engine.rows[4:]))
        self.assertFalse(any(call[0]=='right' and call[1] in (7,8,9) for call in executor.calls if len(call)==6))
        self.assertFalse(any(call[3]=='Goal_Velocity' for call in executor.calls if len(call)==6))
        rows=idle_rows();report,called,_=self.finish(rows,rows,engine.rows)
        self.assertEqual(report['position_verification']['status'],'PASS')
        self.assertEqual(report['stop_verification']['status'],'PASS')
        self.assertEqual(report['identity_verification']['status'],'PASS')
        self.assertEqual(report['result'],'FAIL');self.assertEqual(called,self.identity_calls())

    def test_deferred_wheel_remains_nontarget_with_four_raw_boundary(self):
        for flags in ({'status':'NOT_RUN'}, {'status':'PASS','decision':'DEFERRED'},
                      {'status':'NOT_RUN','decision':'DEFERRED'}):
            for delta,expected in ((4,'PASS'),(5,'FAIL')):
                with self.subTest(flags=flags,delta=delta):
                    before=idle_rows();after=copy.deepcopy(before)
                    after['right:8']['Present_Position']+=delta
                    probes=[{'side':side,'motor':motor,'kind':'position','status':'PASS','anchor_raw':1000}
                            for side,motor,name in m.TARGETS]
                    probes += [{'side':side,'motor':motor,'kind':'velocity','status':'NOT_RUN','decision':'DEFERRED'}
                               for side,motor,name in m.BASE_TARGETS]
                    deferred=next(row for row in probes if row['side']=='right' and row['motor']==8)
                    deferred.pop('decision');deferred.update(flags)
                    report,called,_=self.finish(before,after,probes)
                    report.pop('error')
                    self.assertEqual(report['position_verification']['status'],expected)
                    self.assertEqual(report['stop_verification']['status'],'PASS')
                    self.assertEqual(report['identity_verification']['status'],'PASS')
                    self.assertEqual(m.classify_run_result(report),'FAIL')
                    self.assertEqual(called,self.identity_calls())
                    if delta==5:
                        issue=next(e for e in report['position_verification']['issues'] if e['code']=='NONTARGET_POSITION_CHANGED')
                        self.assertEqual(issue['motor'],'right:8');self.assertEqual(issue['signed_error_raw'],5)
                        with self.assertRaisesRegex(ValueError,'非目标位置变化：right:8'):
                            m.validate_snapshot(after,before,probes)
                    else:
                        m.validate_snapshot(after,before,probes)
        # 已实际尝试的单轮仍保留原目标语义；本例只验证软件分类。
        before=idle_rows();after=copy.deepcopy(before);after['right:8']['Present_Position']+=5
        attempted=[{'side':'right','motor':8,'kind':'velocity','status':'PASS'}]
        self.assertEqual(m.classify_position_snapshot(after,before,attempted)['status'],'PASS')
        m.validate_snapshot(after,before,attempted)

    def test_plan_always_defers_base_even_supplied_verified_flags(self):
        bindings={'schema':'MAINTENANCE_BINDINGS_V1','tool':{'sha256':m.TOOL_SHA,'bytes':m.TOOL_BYTES},
                  'usb':{side:{'device':m.DEVICES[side],'serial':m.SERIALS[side]} for side in m.SERIALS},
                  'base_contract_verified':True,'firmware_contract_verified':True}
        plan=m.make_plan(bindings,'/home/loosqk/robot-checks/approved/run','run','/home/loosqk/robot-checks/approved')
        self.assertEqual([t['motor'] for t in plan['base_targets']],[8,7,9])
        self.assertTrue(all(t['decision']=='DEFERRED' and t['reason_code']=='BASE_FIRMWARE_CONTRACT_UNVERIFIED' for t in plan['base_targets']))
        report={'plan':plan,'stop_verification':{'status':'PASS'},'identity_verification':{'status':'PASS'},
                'position_verification':{'status':'PASS'},'probes':[{'status':'PASS'}]*7}
        self.assertEqual(m.classify_run_result(report),'FAIL')

    def test_selected_plan_defaults_order_and_go_digest_bind_exact_targets(self):
        bindings={'schema':'MAINTENANCE_BINDINGS_V1','tool':{'sha256':m.TOOL_SHA,'bytes':m.TOOL_BYTES},
                  'usb':{side:{'device':m.DEVICES[side],'serial':m.SERIALS[side]} for side in m.SERIALS},
                  'host':{'boot_id':'selection-fixture'}}
        root='/home/loosqk/robot-checks/selected-test'
        default=m.make_plan(bindings,root+'/run','run',root)
        explicit_default=m.make_plan(bindings,root+'/run','run',root,list(m.POSITION_TARGETS))
        selected=m.make_plan(bindings,root+'/run','run',root,['head-2','right-wrist'])
        self.assertEqual(default['position_targets'],m.TARGETS)
        self.assertEqual(m.digest(default),m.digest(explicit_default))
        self.assertEqual(selected['position_targets'],[('left',8,'head_motor_2'),('right',5,'right_arm_wrist_roll')])
        self.assertNotEqual(m.digest(default),m.digest(selected))
        reversed_plan=m.make_plan(bindings,root+'/run','run',root,['right-wrist','head-2'])
        self.assertNotEqual(m.digest(selected),m.digest(reversed_plan))
        go={'schema':'MAINTENANCE_GO_V2','issued_by':'root','artifact_root':root,'plan_sha256':m.digest(default),
            'issued_at_ns':100,'expires_at_ns':1000,'host':bindings['host'],'source':'仅本地夹具，无硬件授权',
            'physical':{side:{**{f:True for f in m.FACTS},'confirmed_at_ns':100} for side in m.SERIALS}}
        m.validate_go(go,default,200)
        with self.assertRaises(ValueError):m.validate_go(go,selected,200)

    def test_invalid_target_selection_and_plan_refuse_zero_launch(self):
        with patch.object(m.subprocess,'Popen',side_effect=AssertionError('错误目标不许启动child')) as launch:
            for names in ([],['left-wrist','left-wrist'],['unknown'],[''],'head-1'):
                with self.subTest(names=names),self.assertRaises(ValueError):m.select_position_targets(names)
            for args in (['--position-target','left-wrist','--position-target','left-wrist'],
                         ['--position-target','unknown'],['--position-target',''],['--position-target']):
                with self.subTest(args=args),contextlib.redirect_stderr(io.StringIO()):
                    with self.assertRaises((ValueError,SystemExit)):m.main(args)
            for targets in ([],[m.TARGETS[0],m.TARGETS[0]],[('right',9,'base_right_wheel')],
                            [('left',5,'head_motor_1')],[('left',False,'left_arm_wrist_roll')]):
                plan={'schema':'MAINTENANCE_PLAN_V2','position_targets':targets}
                called=[];engine=SimpleNamespace(rows=[],position=lambda *a:called.append(a))
                with self.subTest(targets=targets),self.assertRaises(ValueError):m.run_motion_probes(engine,{},plan)
                self.assertEqual(called,[])
                with self.assertRaises(ValueError):m.execute(plan,{},Path('/never-write-go'))
            launch.assert_not_called()

    def test_selected_dispatch_uses_only_plan_order_and_preserves_base_deferred(self):
        selected=[('left',8,'head_motor_2'),('right',5,'right_arm_wrist_roll')]
        plan={**PLAN,'position_targets':selected}
        executor=FakeIO();engine=m.ProbeEngine(executor,plan)
        bindings={'calibrations':{name:{'id':motor,'range_min':73,'range_max':3864} for side,motor,name in selected}}
        with (patch.object(m,'TARGETS',[('right',9,'forbidden-global-target')]),
              patch.object(engine,'wheel',side_effect=AssertionError('base不许运动')) as wheel,
              self.assertRaisesRegex(ValueError,'BASE_FIRMWARE_CONTRACT_UNVERIFIED')):
            m.run_motion_probes(engine,bindings,plan)
        wheel.assert_not_called()
        self.assertEqual([(r['side'],r['motor'],r['name']) for r in engine.rows[:2]],selected)
        self.assertEqual([r['status'] for r in engine.rows],['PASS','PASS','NOT_RUN','NOT_RUN','NOT_RUN'])
        self.assertTrue(all(r['decision']=='DEFERRED' for r in engine.rows[2:]))
        self.assertEqual({(c[0],c[1]) for c in executor.calls if len(c)==6}, {('left',8),('right',5)})

    def test_selected_target_keeps_all_17_stop_and_nontarget_position_checks(self):
        before=idle_rows();after=copy.deepcopy(before)
        after['left:5']['Present_Position']+=5
        probes=[{'side':'left','motor':7,'name':'head_motor_1','kind':'position','status':'PASS',
                 'anchor_raw':1000,'position_accuracy':{'status':'PASS'}}]
        probes += [{'side':side,'motor':motor,'name':name,'kind':'velocity','status':'NOT_RUN','decision':'DEFERRED'}
                   for side,motor,name in m.BASE_TARGETS]
        report,called,executor=self.finish(before,after,probes)
        self.assertEqual(report['stop_verification']['status'],'PASS')
        self.assertEqual(len(executor.reads),17*len(m.REGS))
        self.assertEqual(len([r for r in executor.reads if r[1] in m.STOP_REGS]),17*3)
        self.assertEqual(called,self.identity_calls())
        issue=next(e for e in report['position_verification']['issues'] if e['code']=='NONTARGET_POSITION_CHANGED')
        self.assertEqual((issue['motor'],issue['signed_error_raw']),('left:5',5))
        self.assertEqual(report['result'],'FAIL')
        self.assertEqual(self.fixture()['original_result'],'NEEDS_MANUAL_POWER_CUT')

    def test_cli_selected_plan_only_lists_requested_order_without_child(self):
        with (patch.object(m.subprocess,'Popen',side_effect=AssertionError('PLAN_ONLY不許child')) as launch,
              contextlib.redirect_stdout(io.StringIO()) as output):
            self.assertEqual(m.main(['--position-target','head-2','--position-target','right-wrist']),0)
        reply=json.loads(output.getvalue())
        self.assertEqual(reply['actions_started'],0)
        self.assertEqual(reply['position_targets'],[['left',8,'head_motor_2'],['right',5,'right_arm_wrist_roll']])
        launch.assert_not_called()

    def test_execute_route_four_positions_pass_base_blocked_still_all_postchecks(self):
        class IntegratedIO:
            def __init__(self,tool,output):
                self.regs=idle_rows();self.calls=[];self.child=None;self.interrupted=None;self.commands=[]
                self.process_snapshots=[];self.evidence_errors=[];self.journal=io.StringIO();self.t=0;self.output=Path(output)
            def mark_signal(self,*args):raise AssertionError('本例不发送信号')
            def check(self):pass
            def now(self):return self.t
            def sleep(self,seconds,cleanup=False):self.t+=int(seconds*1e9)
            def command(self,*args,**kwargs):return ''
            def poke(self,side,motor,action,register,value=None,cleanup=False):
                key=f'{side}:{motor}';self.calls.append((key,action,register,value,cleanup));row=self.regs[key]
                if action=='write':row[register]=value;return None
                if register=='Present_Position' and row['Torque_Enable'] and row['Operating_Mode']==0:
                    row['Present_Position']=row['Goal_Position']
                return row[register]
        import os
        actual_uid,actual_gid=os.getuid(),os.getgid()
        read_text=Path.read_text
        def boot_or_text(path,*args,**kwargs):
            return 'unit-boot' if path==Path('/proc/sys/kernel/random/boot_id') else read_text(path,*args,**kwargs)
        with tempfile.TemporaryDirectory() as temp:
            namespace=Path(temp).resolve();root=namespace/'approved';root.mkdir();helper=root/'helper.py';helper.write_text('# 纯本机测试helper\n')
            helper_data=helper.read_bytes();protection={}
            for side in m.SERIALS:
                protection[side]={}
                for name in ('rx_trust.lock','estop_latch.lock'):
                    path=root/f'{side}-{name}';path.write_bytes(b'unchanged\n');protection[side][name]={'path':str(path)}
            bindings={'schema':'MAINTENANCE_BINDINGS_V1','host':{'hostname':'unit-host','uid':actual_uid,'gid':actual_gid,'boot_id':'unit-boot'},
                      'tool':{'path':'/never-access-uart','sha256':m.TOOL_SHA,'bytes':m.TOOL_BYTES},
                      'usb':{side:{'device':m.DEVICES[side],'serial':m.SERIALS[side]} for side in m.SERIALS},
                      'refs':[],'protection':protection,'binary_paths':[],'proc_helper':{'path':str(helper),'bytes':len(helper_data),'sha256':hashlib.sha256(helper_data).hexdigest()},
                      'calibrations':{name:{'id':motor,'range_min':73,'range_max':3864} for side,motor,name in m.TARGETS}}
            holder={};post=[]
            def factory(tool,output):holder['io']=IntegratedIO(tool,output);return holder['io']
            with (patch.object(m,'MAINTENANCE_NAMESPACE',namespace),patch.object(m.sys,'platform','linux'),
                  patch.object(m.os,'uname',return_value=SimpleNamespace(machine='aarch64',nodename='unit-host')),
                  patch.object(Path,'read_text',boot_or_text),patch.object(m,'RealExecutor',side_effect=factory),
                  patch.object(m,'verify_ref'),patch.object(m,'protected_and_refs'),patch.object(m,'verify_protection_file',return_value=None),
                  patch.object(m,'idle'),patch.object(m,'usb_identity',side_effect=lambda *a,**k:post.append('usb')),
                  patch.object(m,'inspect_unit',side_effect=lambda *a,**k:post.append('unit')),
                  patch.object(m,'inspect_processes',side_effect=lambda *a,**k:post.append('proc')),
                  patch.object(m,'inspect_holder',side_effect=lambda *a,**k:post.append('holder')),
                  patch.object(m.signal,'signal'),contextlib.redirect_stdout(io.StringIO())):
                plan=m.make_plan(bindings,root/'run','run',root)
                now=m.time.time_ns()
                go={'schema':'MAINTENANCE_GO_V2','issued_by':'root','artifact_root':str(root),'plan_sha256':m.digest(plan),
                    'issued_at_ns':now,'expires_at_ns':now+60_000_000_000,'host':bindings['host'],'source':'纯假执行器测试，无真实硬件授权',
                    'physical':{side:{**{f:True for f in m.FACTS},'confirmed_at_ns':now} for side in m.SERIALS}}
                approval=root/'go.json';m.write_new(approval,go)
                self.assertEqual(m.execute(plan,go,approval),1)
            report=json.loads((root/'run/run.json').read_text());executor=holder['io']
            self.assertEqual([p['status'] for p in report['probes']],['PASS']*4+['NOT_RUN']*3)
            self.assertEqual(report['result'],'FAIL');self.assertFalse(report['manual_power_cut_required'])
            self.assertEqual(report['stop_verification']['status'],'PASS');self.assertEqual(report['identity_verification']['status'],'PASS')
            self.assertEqual(sum(len(row) for phase in ('before','after') for row in report[phase].values()),204)
            for motor in (7,8,9):
                calls=[c for c in executor.calls if c[0]==f'right:{motor}']
                self.assertEqual(len(calls),12)
                self.assertTrue(all(c[1]=='read' for c in calls))
            self.assertEqual(post.count('unit'),11);self.assertEqual(post.count('proc'),1);self.assertEqual(post.count('holder'),4)
            self.assertTrue(Path(str(approval)+'.consumed').is_file())
            self.assertTrue(all(Path(r['path']).read_bytes()==b'unchanged\n' for files in protection.values() for r in files.values()))

    def test_v1_go_and_changed_artifact_root_rejected(self):
        plan={'schema':'MAINTENANCE_PLAN_V2','artifact_root':'/home/loosqk/robot-checks/new-run','position_targets':m.TARGETS,
              'bindings':{'host':{'boot_id':'same'}}}
        go={'schema':'MAINTENANCE_GO_V2','artifact_root':plan['artifact_root'],'issued_by':'root','plan_sha256':m.digest(plan),
            'issued_at_ns':100,'expires_at_ns':1000,'host':plan['bindings']['host'],'source':'当前现场确认',
            'physical':{side:{**{f:True for f in m.FACTS},'confirmed_at_ns':100} for side in m.SERIALS}}
        m.validate_go(go,plan,200)
        for field,value in [('schema','MAINTENANCE_GO_V1'),('artifact_root','/home/loosqk/robot-checks/other')]:
            bad={**go,field:value}
            with self.subTest(field=field),self.assertRaises(ValueError):m.validate_go(bad,plan,200)
        old_plan={**plan,'schema':'MAINTENANCE_PLAN_V1'}
        with self.assertRaises(ValueError):m.validate_go(go,old_plan,200)

    def test_artifact_root_no_date_dependency_and_once_claim(self):
        m.validate_artifact_root('/home/loosqk/robot-checks/operator-approved-session')
        for root in ('/etc/robot/maintenance','/var/lib/robot/maintenance','/home/loosqk/repo/maintenance',
                     '/home/loosqk/robot-checks','/home/loosqk/robot-checks/session/../other'):
            with self.subTest(root=root),self.assertRaises(ValueError):m.validate_artifact_root(root)
        with tempfile.TemporaryDirectory() as directory,patch.object(m,'MAINTENANCE_NAMESPACE',Path(directory).resolve()):
            root=Path(directory).resolve()/'approved';root.mkdir()
            approval=root/'go.json';m.write_new(approval,{'test':True})
            consumed=m.artifact_path(str(approval)+'.consumed',root);m.write_new(consumed,{'once':True})
            with self.assertRaises(ValueError):m.artifact_path(consumed,root)
            with self.assertRaises(FileExistsError):m.write_new(consumed,{'once':False})
            elsewhere=Path(directory).resolve()/'elsewhere';elsewhere.mkdir()
            with self.assertRaises(ValueError):m.artifact_path(elsewhere/'run',root)
            link=root/'alias';link.symlink_to(elsewhere,target_is_directory=True)
            with self.assertRaises(ValueError):m.artifact_path(link/'run',root)


class RecordingClockIO:
    def __init__(self, reads):
        self.t = 0
        self.reads = iter(reads)
        self.read_count = 0
        self.sleeps = []

    def now(self):
        return self.t

    def poke(self, side, motor, action, register, *, cleanup=False):
        assert (side, motor, action, register, cleanup) == (
            'left', 7, 'read', 'Present_Position', False
        )
        delay_ms, position = next(self.reads)
        self.t += delay_ms * 1_000_000
        self.read_count += 1
        return position

    def sleep(self, seconds):
        self.sleeps.append(seconds)
        self.t += int(seconds * 1_000_000_000)


class DeadlineExhaustionTests(unittest.TestCase):
    def engine(self, reads):
        io = RecordingClockIO(reads)
        plan = {'position_wait_ms': 800, 'return_tolerance_raw': 4}
        return io, m.ProbeEngine(io, plan)

    def test_plus_eight_at_or_after_deadline_fails_without_extra_sleep(self):
        for delay_ms in (800, 801):
            with self.subTest(delay_ms=delay_ms):
                io, engine = self.engine([(delay_ms, 2642)])
                evidence = {}
                with self.assertRaises(m.PositionAccuracyError):
                    engine.witness_position('left', 7, 2634, 2634, 'return', evidence)
                row = evidence['return']
                self.assertEqual(row['status'], 'FAIL')
                self.assertEqual(row['failure_code'], 'POSITION_ACCURACY_TIMEOUT')
                self.assertEqual(row['final_signed_error_raw'], 8)
                self.assertEqual(row['samples'][0]['position_raw'], 2642)
                self.assertEqual(io.read_count, 1)
                self.assertEqual(io.sleeps, [])
                self.assertEqual(row['elapsed_ns'], delay_ms * 1_000_000)

    def test_late_in_tolerance_sample_still_fails_without_extra_sleep(self):
        io, engine = self.engine([(801, 2634)])
        evidence = {}
        with self.assertRaises(m.PositionAccuracyError):
            engine.witness_position('left', 7, 2634, 2634, 'return', evidence)
        row = evidence['return']
        self.assertEqual(row['status'], 'FAIL')
        self.assertEqual(row['final_signed_error_raw'], 0)
        self.assertEqual(row['elapsed_ns'], 801_000_000)
        self.assertEqual(io.read_count, 1)
        self.assertEqual(io.sleeps, [])

    def test_on_time_success_keeps_existing_sample_and_sleep_path(self):
        io, engine = self.engine([(25, 2642), (10, 2638)])
        evidence = {}
        samples = engine.witness_position('left', 7, 2634, 2634, 'return', evidence)
        self.assertEqual([s['position_raw'] for s in samples], [2642, 2638])
        self.assertEqual(evidence['return']['status'], 'PASS')
        self.assertEqual(evidence['return']['final_signed_error_raw'], 4)
        self.assertEqual(evidence['return']['elapsed_ns'], 85_000_000)
        self.assertEqual(io.read_count, 2)
        self.assertEqual(io.sleeps, [0.05])

    def test_exact_deadline_in_tolerance_remains_pass(self):
        io, engine = self.engine([(800, 2638)])
        evidence = {}
        engine.witness_position('left', 7, 2634, 2634, 'return', evidence)
        self.assertEqual(evidence['return']['status'], 'PASS')
        self.assertEqual(evidence['return']['elapsed_ns'], 800_000_000)
        self.assertEqual(io.read_count, 1)
        self.assertEqual(io.sleeps, [])



class RightPulseIO:
    """内存wire端点；输出真实格式的校验帧，错误可发生在写生效之后。"""
    def __init__(self):
        self.t=0; self.torque=0; self.goal=0; self.position=1000
        self.velocity=500; self.delta=80; self.calls=[]; self.tool=m.RIGHT_PULSE_WRITE
        self.fail_write=None; self.bad_raw=None; self.signal_on_enable=False
        self.slow_active_ns=0; self.stop_velocity=0; self.child=None; self.evidence_errors=[]
        self.sequence=0; self.commands=[]; self.interrupted=None; self.trace_calls=[]
        self.zero_count=0; self.fail_zero_number=None; self.goal_write_enables=False
        self.fail_read=None; self.read_counts={}

    def now(self): return self.t
    def check(self):
        if self.interrupted: raise ValueError('注入现场信号')
    def sleep(self,seconds,cleanup=False):
        if not cleanup:self.check()
        self.t+=int(seconds*1e9)
    def poke(self,*args,**kwargs): return m.RealExecutor.poke(self,*args,**kwargs)
    def value(self,register):
        if register in m.RIGHT_PULSE_METADATA:return m.RIGHT_PULSE_METADATA[register]
        if register=='Torque_Enable':return self.torque
        if register=='Goal_Velocity':return self.goal
        if register=='Present_Velocity':return self.velocity if self.torque and self.goal else self.stop_velocity
        if register=='Present_Position':
            if self.torque and self.goal:self.position=(self.position+self.delta)%4096
            return self.position
        if register=='Goal_Position':return 1000
        raise AssertionError(register)
    def command(self,argv,**kwargs):
        cleanup=kwargs.get('cleanup',False)
        if not cleanup:self.check()
        if argv[0]==m.RIGHT_PULSE_TRACE['path']:
            self.trace_calls.append(list(argv))
            index=argv.index('-o')
            Path(argv[index+1]).write_bytes(b'fixture wire capture\n+++ exited with 0 +++\n')
            argv=argv[index+2:]
        self.calls.append((list(argv),cleanup));self.sequence+=1
        self.commands.append({'argv':list(argv),'cleanup':cleanup})
        self.t+=10_000_000
        action=argv[3]
        if action=='write':
            register,value=argv[4],int(argv[5])
            if register=='Torque_Enable':self.torque=value
            elif register=='Goal_Velocity':self.goal=value
            else:raise AssertionError(register)
            if self.fail_write==(register,value):
                self.fail_write=None
                raise OSError('注入：已写入但回执失败')
            if register=='Torque_Enable' and value==1 and self.signal_on_enable:
                self.interrupted=signal.SIGTERM
                self.check()
            return f'write {register}@9 <- {value} ok\n'
        if action in ('sync-zero','sync-velocity'):
            self.goal=0 if action=='sync-zero' else int(argv[4])
            if self.goal_write_enables:self.torque=1
            if action=='sync-zero':self.zero_count+=1
            if self.fail_write==('Goal_Velocity',self.goal) or (
                    action=='sync-zero' and self.zero_count==self.fail_zero_number):
                self.fail_write=None
                raise OSError('注入：SYNC已发送但结果未知')
            return f'{action} Goal_Velocity@9 <- {self.goal}: SYNC_WRITE已发送，无ACK；未确认电机接收或实际速度\n'
        register=argv[4]
        self.read_counts[register]=self.read_counts.get(register,0)+1
        if self.fail_read==(register,self.read_counts[register]):
            raise OSError('注入：读取未知')
        value=self.value(register)
        if action=='read':return f'read {register}@{argv[2]} = {value}\n'
        self.t+=self.slow_active_ns if self.torque else 0
        address,width=self.meta[register]
        raw=abs(value)|(0x8000 if value<0 else 0)
        tx=[255,255,254,5,130,address,width,9];tx.append((~sum(tx[2:]))&255)
        rx=[255,255,9,width+2,0,*raw.to_bytes(width,'little')];rx.append((~sum(rx[2:]))&255)
        if self.torque and register==self.bad_raw:rx[-1]^=1
        return json.dumps({'operation':'sync_read','serial_device':m.DEVICES['right'],'motor_id':9,
                           'register':register,'requested_register_address':address,'width':width,
                           'raw_unsigned':raw,'tx':tx,'rx':rx})+'\n'


class RightPulseTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # 复用已入库的冻结校验器，不另写一份协议解析。
        path=Path(m.__file__).parents[1]/'.verify/base_zero_sync_diagnostic_2026-10-13_r2_hardware/originals/helpers/raw_right9_from_r14_r2.py'
        spec=m.importlib.util.spec_from_file_location('right_pulse_test_raw',path)
        cls.raw=m.importlib.util.module_from_spec(spec);spec.loader.exec_module(cls.raw)
        cls.raw.RAW_TOOL=dict(m.RIGHT_PULSE_RAW)
        cls.raw.META={**cls.raw.META,'Present_Position':(56,2)}

    def setUp(self):
        self.verify=patch.object(m,'verify_ref')
        self.verify.start();self.addCleanup(self.verify.stop)
        self.sync_binding=patch.dict(m.RIGHT_PULSE_SYNC,sha256='a'*64,bytes=123456)
        self.sync_binding.start();self.addCleanup(self.sync_binding.stop)

    def bindings(self):
        return {'schema':'MAINTENANCE_BINDINGS_V1','tool':dict(m.RIGHT_PULSE_WRITE),
                'usb':{side:{'device':m.DEVICES[side],'serial':m.SERIALS[side]} for side in m.SERIALS},
                'host':{'boot_id':'pulse-fixture'},
                'refs':[dict(r) for r in (m.RIGHT_PULSE_WRITE,m.RIGHT_PULSE_RAW,m.RIGHT_PULSE_SYNC,m.RIGHT_PULSE_HELPER,m.RIGHT_PULSE_TRACE)],
                'binary_paths':[r['path'] for r in (m.RIGHT_PULSE_WRITE,m.RIGHT_PULSE_RAW,m.RIGHT_PULSE_SYNC,m.RIGHT_PULSE_TRACE)]}

    def plan(self):
        root='/home/loosqk/robot-checks/pulse-fixture'
        return m.make_plan(self.bindings(),root+'/run','pulse-fixture',root,right_wheel_pulse=True)

    def engine(self):
        executor=RightPulseIO();executor.meta=self.raw.META
        directory=tempfile.TemporaryDirectory();self.addCleanup(directory.cleanup)
        executor.output=Path(directory.name)
        helper=SimpleNamespace(META=self.raw.META,raw_read=self.raw.raw_read,
                               install_stop_wire_capture=lambda module:None)
        engine=m.ProbeEngine(executor,self.plan())
        m.install_right_pulse_guard(engine,helper)
        return executor,engine

    def perform(self,executor,engine,success=True):
        with contextlib.redirect_stdout(io.StringIO()):
            if success:m.run_right_wheel_pulse(engine)
            else:
                with self.assertRaises(ValueError):m.run_right_wheel_pulse(engine)
        return engine.right_pulse

    def writes(self,executor):
        return [argv[3:] for argv,cleanup in executor.calls if argv[3] in ('write','sync-zero','sync-velocity')]

    def test_normal_order_and_quantized_fifty_are_motion_not_stop_errors(self):
        for velocity in (50,500,1000):
            executor,engine=self.engine();executor.velocity=velocity
            row=self.perform(executor,engine)
            self.assertEqual(row['status'],'PASS')
            self.assertEqual(self.writes(executor),[['sync-zero'],['write','Torque_Enable','1'],
                                                   ['sync-velocity','500'],['sync-zero'],['write','Torque_Enable','0']])
            self.assertEqual(len(row['samples']),2)
            self.assertTrue(all(s['raw'][-1]['finished_ns']<=row['observation_deadline_ns'] for s in row['samples']))
            self.assertLessEqual(row['enabled_attempt_window_ns'],500_000_000)
            self.assertFalse(engine.manual_cut)
            self.assertEqual((executor.torque,executor.goal),(0,0))
            self.assertTrue(all(argv[1:3]==[m.DEVICES['right'],'9'] for argv,_ in executor.calls))
            self.assertEqual(len(executor.trace_calls),5)
            self.assertEqual([attempt['wire_trace']['instruction'] for attempt in row['writes']],[131,3,131,131,3])
            self.assertEqual([attempt['wire_trace']['requested_register_address'] for attempt in row['writes']],[46,40,46,46,40])
            initial_goal=row['before_raw'][10]
            self.assertEqual(initial_goal['register'],'Goal_Velocity')
            self.assertGreaterEqual(initial_goal['started_ns']-row['initial_zero_finished_ns'],50_000_000)
            self.assertEqual([sample['register'] for sample in row['cleanup_raw']],
                             ['Torque_Enable','Present_Velocity','Goal_Velocity'])
            for attempt in row['writes']:
                trace=attempt['wire_trace'];raw=Path(trace['path']).read_bytes()
                self.assertEqual(hashlib.sha256(raw).hexdigest(),trace['sha256'])

    def test_motion_direction_magnitude_and_no_motion_fail_but_disable(self):
        for velocity,delta in ((-50,80),(1001,80),(0,0),(500,-1),(500,513)):
            executor,engine=self.engine();executor.velocity=velocity;executor.delta=delta
            row=self.perform(executor,engine,False)
            self.assertEqual(row['status'],'FAIL');self.assertFalse(engine.manual_cut)
            self.assertIn(['write','Torque_Enable','0'],self.writes(executor))
            self.assertEqual((executor.torque,executor.goal),(0,0))

    def test_bad_raw_signal_and_late_read_preserve_failure_and_attempt_disable(self):
        for fault in ('frame','signal','late','over_one_second'):
            executor,engine=self.engine()
            if fault=='frame':executor.bad_raw='Present_Velocity'
            if fault=='signal':executor.signal_on_enable=True
            if fault=='late':executor.slow_active_ns=450_000_000
            if fault=='over_one_second':executor.slow_active_ns=1_100_000_000
            row=self.perform(executor,engine,False)
            self.assertIn(['write','Torque_Enable','0'],self.writes(executor))
            self.assertEqual((executor.torque,executor.goal),(0,0));self.assertFalse(engine.manual_cut)
            self.assertTrue(row['errors'])
            if fault=='over_one_second':self.assertGreater(row['enabled_attempt_window_ns'],1_000_000_000)

    def test_each_uncertain_motion_write_still_disables_once_without_retry(self):
        for failed in (('Goal_Velocity',500),('Torque_Enable',1)):
            executor,engine=self.engine();executor.fail_write=failed
            row=self.perform(executor,engine,False)
            writes=self.writes(executor)
            attempted=['sync-velocity','500'] if failed[0]=='Goal_Velocity' else ['write',failed[0],str(failed[1])]
            self.assertEqual(writes.count(attempted),1)
            self.assertEqual(writes.count(['write','Torque_Enable','0']),1)
            self.assertEqual(row['status'],'FAIL');self.assertFalse(engine.manual_cut)

    def test_velocity_write_torque_side_effect_cannot_reenable_after_final_disable(self):
        executor,engine=self.engine();executor.goal_write_enables=True
        row=self.perform(executor,engine)
        self.assertTrue(row['enable_verified'])
        self.assertEqual((executor.torque,executor.goal),(0,0))
        self.assertEqual(self.writes(executor)[-1],['write','Torque_Enable','0'])

    def test_initial_zero_failure_has_no_zero_retry_and_only_independent_disable(self):
        executor,engine=self.engine();executor.fail_zero_number=1
        row=self.perform(executor,engine,False)
        self.assertEqual(self.writes(executor),[['sync-zero'],['write','Torque_Enable','0']])
        self.assertTrue(row['errors']);self.assertFalse(row['enable_attempted'])

    def test_cleanup_zero_failure_still_disables_once_then_blocks_uart(self):
        executor,engine=self.engine();executor.fail_zero_number=2
        row=self.perform(executor,engine,False)
        self.assertEqual(self.writes(executor)[-2:],[['sync-zero'],['write','Torque_Enable','0']])
        self.assertEqual(self.writes(executor).count(['sync-zero']),2)
        self.assertEqual(self.writes(executor).count(['write','Torque_Enable','0']),1)
        self.assertEqual(row['cleanup_raw'],[])
        self.assertTrue(row['uart_blocked']);self.assertTrue(engine.manual_cut)
        count=len(executor.calls)
        with self.assertRaises(ValueError):executor.poke('right',9,'read','Torque_Enable',cleanup=True)
        self.assertEqual(len(executor.calls),count)

    def test_preparation_and_sample_read_failures_keep_single_disable(self):
        for register,number,nonzero_sent in (('Goal_Velocity',2,False),('Torque_Enable',2,False),
                ('Goal_Velocity',3,True),('Present_Velocity',2,True),('Present_Position',2,True)):
            executor,engine=self.engine();executor.fail_read=(register,number)
            row=self.perform(executor,engine,False)
            self.assertEqual(self.writes(executor).count(['write','Torque_Enable','0']),1)
            self.assertEqual(row['velocity_attempted'],nonzero_sent)
            self.assertEqual(self.writes(executor).count(['sync-zero']),2 if nonzero_sent else 1)
            self.assertEqual(self.writes(executor)[-1],['write','Torque_Enable','0'])
            self.assertTrue(row['errors'])

    def test_enable_must_read_one_before_nonzero_and_expiry_does_not_move_start(self):
        for fault in ('readback','deadline'):
            executor,engine=self.engine()
            if fault=='readback':
                original=executor.value
                executor.value=lambda register:0 if register=='Torque_Enable' else original(register)
            else:executor.slow_active_ns=500_000_000
            row=self.perform(executor,engine,False)
            self.assertFalse(row['velocity_attempted'])
            self.assertEqual(row['observation_deadline_ns']-row['enable_attempt_started_ns'],500_000_000)
            self.assertEqual(self.writes(executor)[-1],['write','Torque_Enable','0'])

    def test_right9_ordinary_torque_captures_same_command_even_if_stop_nonzero(self):
        for nonzero in (False,True):
            executor,engine=self.engine();executor.torque=int(nonzero)
            if nonzero:
                with contextlib.redirect_stdout(io.StringIO()),self.assertRaises(ValueError):
                    executor.poke('right',9,'read','Torque_Enable',cleanup=True)
            else:self.assertEqual(executor.poke('right',9,'read','Torque_Enable'),0)
            self.assertEqual(len(executor.calls),1)
            trace=executor.commands[-1]['ordinary_read_trace']
            self.assertEqual(trace['instruction'],2)
            self.assertEqual(trace['requested_register_address'],40)
            self.assertEqual(trace['width'],1)
            self.assertEqual(hashlib.sha256(Path(trace['path']).read_bytes()).hexdigest(),trace['sha256'])
            self.assertEqual(engine.manual_cut,nonzero)

    def test_unfrozen_sync_tool_and_wrong_sync_command_reject_before_child(self):
        for key,value in (('sha256',None),('bytes',None),('sha256','bad'),('bytes',0)):
            with patch.dict(m.RIGHT_PULSE_SYNC,{key:value}),self.assertRaises(ValueError):
                self.plan()
        for tool,action in ((m.RIGHT_PULSE_RAW,['sync-velocity','500']),
                            (m.RIGHT_PULSE_SYNC,['sync-velocity','501']),
                            (m.RIGHT_PULSE_SYNC,['sync-zero'])):
            executor,engine=self.engine()
            with self.assertRaises(ValueError):
                executor.command([tool['path'],m.DEVICES['right'],'9',*action])
            self.assertEqual(executor.calls,[])

    def test_last_position_read_crossing_go_keeps_frame_fails_and_disables(self):
        executor,engine=self.engine()
        expiry=lambda:engine.right_pulse.get('enable_attempt_started_ns',10**12)+415_000_000
        engine.right_pulse_go_check=lambda:m.require(executor.now()<expiry(),'夹具GO到期')
        row=self.perform(executor,engine,False)
        self.assertIn('GO到期',row['errors'][-1])
        self.assertEqual(row['samples'][-1]['raw'][-1]['status'],'READ_VALID')
        self.assertGreaterEqual(row['samples'][-1]['raw'][-1]['finished_ns'],expiry())
        self.assertEqual(self.writes(executor).count(['write','Torque_Enable','0']),1)
        self.assertFalse(engine.manual_cut)

    def test_write_and_ordinary_read_return_recheck_go(self):
        for kind in ('write','ordinary'):
            executor,engine=self.engine()
            engine.right_pulse_go_check=lambda:m.require(executor.now()<5_000_000,'返回时GO到期')
            if kind=='write':
                engine.right_pulse['metadata_verified']=True
                argv=[m.RIGHT_PULSE_RAW['path'],m.DEVICES['right'],'9','sync-zero']
            else:argv=[m.RIGHT_PULSE_WRITE['path'],m.DEVICES['right'],'9','read','Torque_Enable']
            with contextlib.redirect_stdout(io.StringIO()),self.assertRaisesRegex(ValueError,'GO到期'):
                executor.command(argv)
            self.assertEqual(len(executor.calls),1)
            if kind=='write':
                self.assertEqual(engine.right_pulse['writes'][0]['status'],'SENT_NO_ACK')
                self.assertIn('wire_trace',engine.right_pulse['writes'][0])

    def test_trace_prepare_failure_never_prevents_single_disable(self):
        for when in ('enable','zero','disable'):
            executor,engine=self.engine()
            def verify(reference,**kwargs):
                if reference==m.RIGHT_PULSE_TRACE and (
                        engine.right_pulse['off_attempted'] if when=='disable'
                        else len(engine.right_pulse['writes'])==(4 if when=='zero' else 2)):
                    raise OSError('夹具strace准备失败')
            with patch.object(m,'verify_ref',side_effect=verify):
                row=self.perform(executor,engine,False)
            self.assertEqual(self.writes(executor).count(['write','Torque_Enable','0']),1)
            self.assertEqual(executor.torque,0)
            self.assertTrue(executor.evidence_errors)
            if when in ('zero','disable'):
                self.assertTrue(row['uart_blocked']);self.assertTrue(engine.manual_cut)
                self.assertEqual(self.writes(executor)[-1],['write','Torque_Enable','0'])
            else:self.assertFalse(engine.manual_cut)

    def test_trace_save_failure_after_write_still_attempts_disable_and_preserves_failure(self):
        original=Path.read_bytes
        for when in ('enable','zero','disable'):
            executor,engine=self.engine()
            def read_bytes(path):
                target=(engine.right_pulse['off_attempted'] if when=='disable'
                        else len(engine.right_pulse['writes'])==(4 if when=='zero' else 2))
                if path.name.startswith('right9-pulse-write-') and target:
                    raise OSError('夹具trace落盘读取失败')
                return original(path)
            with patch.object(Path,'read_bytes',read_bytes):
                row=self.perform(executor,engine,False)
            self.assertEqual(self.writes(executor).count(['write','Torque_Enable','0']),1)
            self.assertEqual(executor.torque,0)
            self.assertEqual(row['status'],'FAIL');self.assertTrue(executor.evidence_errors)
            self.assertEqual(engine.manual_cut,when in ('zero','disable'))

    def test_disable_failure_or_nonzero_stop_blocks_all_subsequent_uart(self):
        for fault in ('ack','speed'):
            executor,engine=self.engine()
            if fault=='ack':executor.fail_write=('Torque_Enable',0)
            else:executor.stop_velocity=50
            # 停止异常只在实际断扭后注入，不污染初始静止门。
            if fault=='speed':
                executor.stop_velocity=0
                original=executor.value
                executor.value=lambda register:50 if register=='Present_Velocity' and engine.right_pulse['off_attempted'] else original(register)
            row=self.perform(executor,engine,False)
            self.assertTrue(engine.manual_cut);self.assertTrue(row['uart_blocked'])
            self.assertEqual(self.writes(executor)[-1],['write','Torque_Enable','0'])
            count=len(executor.calls)
            with self.assertRaises(ValueError):executor.poke('right',9,'read','Present_Velocity',cleanup=True)
            self.assertEqual(len(executor.calls),count)

    def test_initial_and_final_ordinary_stop_failure_is_sticky(self):
        for after_motion in (False,True):
            executor,engine=self.engine()
            if after_motion:self.perform(executor,engine)
            executor.stop_velocity=50
            with contextlib.redirect_stdout(io.StringIO()),self.assertRaises(ValueError):
                executor.poke('right',8,'read','Present_Velocity',cleanup=after_motion)
            count=len(executor.calls)
            with self.assertRaises(ValueError):executor.poke('left',1,'read','Torque_Enable',cleanup=True)
            self.assertEqual(len(executor.calls),count);self.assertTrue(engine.manual_cut)

    def test_illegal_writes_are_refused_before_executor(self):
        for side,motor,register,value in (('left',1,'Torque_Enable',1),('right',8,'Torque_Enable',1),
                ('right',9,'Goal_Velocity',501),('right',9,'Goal_Position',1234),('right',9,'Phase',4)):
            executor,engine=self.engine()
            argv=[m.RIGHT_PULSE_WRITE['path'],m.DEVICES[side],str(motor),'write',register,str(value)]
            with self.assertRaises(ValueError):executor.command(argv)
            self.assertEqual(executor.calls,[])

    def test_bad_actual_metadata_never_writes_or_continues_uart(self):
        executor,engine=self.engine();original=executor.value
        executor.value=lambda register:11 if register=='Firmware_Minor_Version' else original(register)
        self.perform(executor,engine,False)
        self.assertEqual(self.writes(executor),[]);self.assertTrue(engine.right_pulse['uart_blocked'])

    def test_plan_flags_binding_and_missing_go_reject_without_child(self):
        with patch.object(m.subprocess,'Popen',side_effect=AssertionError('前置不许启动child')) as child:
            for args in (['--right-wheel-pulse','--position-target','head-1'],
                         ['--right-wheel-pulse','--execute'],['--right-wheel-pulse','500']):
                with contextlib.redirect_stderr(io.StringIO()),self.assertRaises((ValueError,SystemExit)):
                    m.main(args)
            for mutate in ('metadata','tool','refs','targets','sync_sha','sync_path','sync_bytes','sync_binary'):
                plan=self.plan()
                if mutate=='metadata':plan['right_wheel_pulse']['metadata']['Phase']=4
                if mutate=='tool':plan['bindings']['tool']['sha256']='0'*64
                if mutate=='refs':plan['bindings']['refs'].pop()
                if mutate=='targets':plan['position_targets']=m.TARGETS
                if mutate.startswith('sync_'):
                    reference=next(ref for ref in plan['bindings']['refs'] if ref['path']==m.RIGHT_PULSE_SYNC['path'])
                    if mutate=='sync_sha':reference['sha256']='b'*64
                    if mutate=='sync_path':reference['path']='/tmp/wrong-tool'
                    if mutate=='sync_bytes':reference['bytes']+=1
                    if mutate=='sync_binary':plan['bindings']['binary_paths'].remove(reference['path'])
                with self.assertRaises(ValueError):m.position_targets_from_plan(plan)
            with self.assertRaises((ValueError,KeyError)):m.validate_go({},self.plan(),200)
            child.assert_not_called()

    def test_existing_r4_extended_identity_metadata_is_preserved(self):
        path=Path(m.__file__).parents[1]/'.verify/base_zero_sync_diagnostic_2026-10-20_r4/plan.json'
        document=json.loads(path.read_text())
        bindings=document.get('plan',document)['bindings']
        bindings['refs'].append(dict(m.RIGHT_PULSE_SYNC))
        bindings['binary_paths'].append(m.RIGHT_PULSE_SYNC['path'])
        original=copy.deepcopy(bindings)
        root='/home/loosqk/robot-checks/pulse-fixture'
        plan=m.make_plan(bindings,root+'/run','pulse-fixture',root,right_wheel_pulse=True)
        self.assertEqual(plan['bindings'],original)
        self.assertIn('inode',plan['bindings']['tool'])

    def test_go_binds_current_scope_and_600_seconds(self):
        plan=self.plan()
        facts={**{key:True for key in m.FACTS},'confirmed_at_ns':100}
        go={'schema':'MAINTENANCE_GO_V2','issued_by':'root','artifact_root':plan['artifact_root'],
            'plan_sha256':m.digest(plan),'issued_at_ns':100,'expires_at_ns':1000,'host':plan['bindings']['host'],
            'source':'仅测试夹具','physical':{side:dict(facts) for side in m.SERIALS}}
        go['physical']['right'].update(all_three_wheels_off_ground=True,base_stably_supported=True,
            continuous_wheel_observation=True,base_confirmation_kind='CURRENT_SETUP_READY',
            base_confirmation_scope=m.RIGHT_PULSE_SCOPE,base_confirmation_answer='夹具就绪')
        m.validate_go(go,plan,200)
        for fault in ('scope','observation','ttl','stale'):
            bad=copy.deepcopy(go)
            if fault=='scope':bad['physical']['right']['base_confirmation_scope']='旧零速'
            if fault=='observation':bad['physical']['right']['continuous_wheel_observation']=False
            if fault=='ttl':bad['expires_at_ns']=601_000_000_101
            if fault=='stale':
                bad['issued_at_ns']=601_000_000_100;bad['expires_at_ns']=601_000_000_900
            with self.assertRaises(ValueError):m.validate_go(bad,plan,601_000_000_200 if fault=='stale' else 200)

    def test_diagnostic_pass_keeps_firmware_deferred_and_other_wheels_untouched(self):
        executor,engine=self.engine()
        with self.assertRaisesRegex(ValueError,'BASE_FIRMWARE_CONTRACT_UNVERIFIED'):
            m.run_motion_probes(engine,engine.plan['bindings'],engine.plan)
        self.assertEqual(engine.rows[0]['status'],'PASS')
        self.assertTrue(all(row['decision']=='DEFERRED' for row in engine.rows[1:]))
        before=idle_rows();after=copy.deepcopy(before);after['right:9']['Present_Position']=executor.position
        self.assertEqual(m.classify_position_snapshot(after,before,engine.rows)['status'],'PASS')
        after['right:8']['Present_Position']+=5
        self.assertEqual(m.classify_position_snapshot(after,before,engine.rows)['status'],'FAIL')
        self.assertEqual(m.classify_stop_snapshot(after)['status'],'PASS')


class FeedbackIO:
    """右7只读内存端点；计时、故障及原帧可独立注入。"""
    def __init__(self):
        self.t=0;self.sequence=0;self.commands=[];self.calls=[]
        self.child=None;self.interrupted=None;self.evidence_errors=[]
        self.process_snapshots=[];self.journal=io.StringIO()
        self.fail_at=None;self.torque_at=None;self.signal_at=None;self.bad_at=None
        self.slow_at=None;self.slow_ns=0;self.velocity_index=0
    def now(self):return self.t
    def check(self):
        if self.interrupted:raise ValueError('只读夹具信号')
    def mark_signal(self,signum,_frame):self.interrupted=signum
    def sleep(self,seconds,cleanup=False):
        if not cleanup:self.check()
        self.t+=round(seconds*1e9)
    def command(self,argv,**kwargs):
        if argv[0]!=m.RIGHT_PULSE_RAW['path']:
            if 'fuser' in argv:return ''
            raise AssertionError(argv)
        self.check()
        self.sequence+=1;self.calls.append(list(argv))
        self.commands.append({'argv':list(argv),'started_ns':self.t})
        self.t+=10_000_000+(self.slow_ns if self.sequence==self.slow_at else 0)
        self.commands[-1]['finished_ns']=self.t
        if self.sequence==self.fail_at:raise OSError('夹具通信失败')
        if self.sequence==self.signal_at:self.interrupted=signal.SIGTERM
        register=argv[4];address,width=m.LEFT_FEEDBACK_META[register]
        value=0
        if register=='Torque_Enable':value=int(self.sequence==self.torque_at)
        if register=='Present_Position':value=1212
        if register=='Present_Velocity':
            value=(0,50,-50,50,0)[self.velocity_index];self.velocity_index+=1
        value=abs(value)|(0x8000 if value<0 else 0)
        tx=[255,255,254,5,130,address,width,7];tx.append((~sum(tx[2:]))&255)
        rx=[255,255,7,width+2,0,*value.to_bytes(width,'little')];rx.append((~sum(rx[2:]))&255)
        if self.sequence==self.bad_at:rx[-1]^=1
        return json.dumps({'operation':'sync_read','serial_device':m.DEVICES['right'],'motor_id':7,
                           'register':register,'requested_register_address':address,'width':width,
                           'raw_unsigned':value,'tx':tx,'rx':rx})+'\n'


class LeftFeedbackTests(unittest.TestCase):
    def setUp(self):
        directory=tempfile.TemporaryDirectory();self.addCleanup(directory.cleanup)
        self.root=Path(directory.name).resolve()
        path=self.root/'fake-arm64'
        elf=bytearray(64);elf[:6]=b'\x7fELF\x02\x01';elf[18:20]=(183).to_bytes(2,'little')
        path.write_bytes(elf);path.chmod(0o755)
        fixed={'path':str(path),'sha256':hashlib.sha256(elf).hexdigest(),'bytes':len(elf)}
        replacement=patch.dict(m.RIGHT_PULSE_RAW,fixed,clear=True)
        replacement.start();self.addCleanup(replacement.stop)
        info=path.stat()
        self.reference={**fixed,'uid':info.st_uid,'gid':info.st_gid,'mode':0o755,'device':info.st_dev,
                        'inode':info.st_ino,'mtime_ns':info.st_mtime_ns,'ctime_ns':info.st_ctime_ns,
                        'nlink':1,'symlink':False,'elf_machine':183}

    def bindings(self):
        return {'schema':'MAINTENANCE_BINDINGS_V1','tool':dict(m.RIGHT_PULSE_WRITE),
                'host':{'hostname':'unit-host','uid':m.os.getuid(),'gid':m.os.getgid(),'boot_id':'unit-boot'},
                'usb':{side:{'device':m.DEVICES[side],'serial':m.SERIALS[side]} for side in m.SERIALS},
                'refs':[dict(self.reference)],'binary_paths':[self.reference['path']]}

    def plan(self):
        root='/home/loosqk/robot-checks/feedback'
        return m.make_plan(self.bindings(),root+'/run','feedback',root,left_wheel_feedback_only=True)

    def go(self,plan,now):
        facts={**{k:True for k in m.FACTS},'confirmed_at_ns':now}
        return {'schema':'MAINTENANCE_GO_V2','issued_by':'root','artifact_root':plan['artifact_root'],
                'plan_sha256':m.digest(plan),'issued_at_ns':now,'expires_at_ns':now+60_000_000_000,
                'host':plan['bindings']['host'],'source':'内存夹具，无硬件授权',
                'physical':{'left':dict(facts),'right':{**facts,'all_three_wheels_off_ground':True,
                'base_stably_supported':True,'continuous_wheel_observation':True,
                'base_confirmation_kind':'CURRENT_SETUP_READY','base_confirmation_scope':m.LEFT_FEEDBACK_SCOPE,
                'base_confirmation_answer':'夹具只读'}}}

    def engine(self):
        return m.ProbeEngine(FeedbackIO(),self.plan())

    def test_fixed_twelve_reads_record_fifty_without_stop_pass(self):
        engine=self.engine();m.run_left_wheel_feedback(engine,lambda:None)
        row=engine.rows[0]
        self.assertEqual(row['status'],'DIAGNOSTIC_CAPTURED')
        self.assertFalse(row['stop_verified']);self.assertFalse(row['same_frame'])
        self.assertEqual([c[4] for c in engine.io.calls],
                         ['Torque_Enable']+['Present_Position','Present_Velocity']*5+['Torque_Enable'])
        self.assertTrue(all(c[:4]==[self.reference['path'],m.DEVICES['right'],'7','read-raw']
                            for c in engine.io.calls))
        pv=[s['raw']['raw_unsigned'] for s in row['samples'] if s['register']=='Present_Velocity']
        self.assertEqual(pv,[0,50,0x8032,50,0])
        self.assertEqual([s['started_ns']-row['observation_started_ns'] for s in row['samples']
                          if s['register']=='Present_Position'],[0,250_000_000,500_000_000,750_000_000,1_000_000_000])
        self.assertTrue(all(s['finished_ns']>s['started_ns'] for s in row['samples']))
        self.assertFalse(engine.manual_cut)
        with self.assertRaises(ValueError):m.run_left_wheel_feedback(engine,lambda:None)
        self.assertEqual(len(engine.io.calls),12)

    def test_every_read_fault_stops_without_retry_or_final_torque_read(self):
        for fault in ('fail_at','bad_at','signal_at'):
            for index in range(1,13):
                engine=self.engine();setattr(engine.io,fault,index)
                with self.subTest(fault=fault,index=index),self.assertRaises((ValueError,OSError)):
                    m.run_left_wheel_feedback(engine,lambda:None)
                self.assertEqual(len(engine.io.calls),index)
                self.assertTrue(engine.rows[0]['uart_blocked']);self.assertTrue(engine.manual_cut)
                self.assertEqual(engine.rows[0]['status'],'FAIL')
        for index in (1,12):
            engine=self.engine();engine.io.torque_at=index
            with self.assertRaises(ValueError):m.run_left_wheel_feedback(engine,lambda:None)
            self.assertEqual(len(engine.io.calls),index)
            self.assertEqual(engine.rows[0]['samples'][-1]['raw']['raw_unsigned'],1)

    def test_deadline_and_go_expiry_keep_last_frame_then_stop(self):
        for index in (1,5,12):
            engine=self.engine();engine.io.slow_at=index;engine.io.slow_ns=1_500_000_000
            with self.assertRaisesRegex(ValueError,'窗口'):m.run_left_wheel_feedback(engine,lambda:None)
            self.assertEqual(len(engine.io.calls),index)
            self.assertIn('raw',engine.rows[0]['samples'][-1])
        engine=self.engine()
        with self.assertRaisesRegex(ValueError,'GO'):
            m.run_left_wheel_feedback(engine,lambda:m.require(engine.io.now()<1_035_000_000,'夹具GO到期'))
        self.assertEqual(len(engine.io.calls),12)
        self.assertIn('raw',engine.rows[0]['samples'][-1])

    def test_tool_identity_and_prelaunch_expiry_reject_before_uart(self):
        for key in ('sha256','inode','mode','mtime_ns'):
            engine=self.engine();ref=m.left_feedback_tool_ref(engine.plan)
            ref[key]='0'*64 if key=='sha256' else ref[key]+1
            with self.subTest(key=key),self.assertRaises(ValueError):
                m.run_left_wheel_feedback(engine,lambda:None)
            self.assertEqual(engine.io.calls,[])
        engine=self.engine()
        with self.assertRaises(ValueError):
            m.run_left_wheel_feedback(engine,lambda:(_ for _ in ()).throw(ValueError('GO已失效')))
        self.assertEqual(engine.io.calls,[])
        with self.assertRaises(ValueError):
            m.left_feedback_raw(engine.io,self.reference,'Goal_Velocity',lambda:None)
        self.assertEqual(engine.io.calls,[])
        engine=self.engine()
        with patch.object(m,'verify_ref',side_effect=lambda *a,**k:setattr(engine.io,'t',1_500_000_000)):
            with self.assertRaises(ValueError):m.run_left_wheel_feedback(engine,lambda:None)
        self.assertEqual(engine.io.calls,[])

    def test_raw_target_and_frame_metadata_mismatch_stop_after_one_read(self):
        for key,value in (('motor_id',9),('serial_device',m.DEVICES['left']),('register','Present_Position'),
                          ('width',True),('requested_register_address',58),('raw_unsigned',1),
                          ('tx',[255]),('rx',[255])):
            engine=self.engine();command=engine.io.command
            def changed(argv,**kwargs):
                raw=json.loads(command(argv,**kwargs));raw[key]=value
                return json.dumps(raw)+'\n'
            engine.io.command=changed
            with self.subTest(key=key),self.assertRaises(ValueError):
                m.run_left_wheel_feedback(engine,lambda:None)
            self.assertEqual(len(engine.io.calls),1)
            self.assertTrue(engine.rows[0]['uart_blocked'])

    def test_flag_contract_binding_and_missing_go_have_zero_child(self):
        with patch.object(m.subprocess,'Popen',side_effect=AssertionError('禁止子进程')) as child:
            for args in (['--left-wheel-feedback-only','--right-wheel-pulse'],
                         ['--left-wheel-feedback-only','--position-target','head-1'],
                         ['--left-wheel-feedback-only','--execute']):
                with self.assertRaises(ValueError):m.main(args)
            for key in ('target','reads','capture_budget_ns'):
                plan=self.plan();plan['left_wheel_feedback_only'][key]=0
                with self.assertRaises(ValueError):m.position_targets_from_plan(plan)
            for key in ('refs','binary_paths'):
                plan=self.plan();plan['bindings'][key]=[]
                with self.assertRaises(ValueError):m.position_targets_from_plan(plan)
            with self.assertRaises((ValueError,KeyError)):m.validate_go({},self.plan(),0)
            with contextlib.redirect_stdout(io.StringIO()) as out:
                self.assertEqual(m.main(['--left-wheel-feedback-only']),0)
            self.assertEqual(json.loads(out.getvalue())['actions_started'],0)
            child.assert_not_called()

    def test_go_binds_readonly_scope_and_ten_minute_field_state(self):
        plan=self.plan();go=self.go(plan,100);m.validate_go(go,plan,200)
        for field in ('base_confirmation_scope','continuous_wheel_observation','confirmed_at_ns'):
            bad=copy.deepcopy(go)
            bad['physical']['right'][field]=m.RIGHT_PULSE_SCOPE if field=='base_confirmation_scope' else False
            with self.assertRaises(ValueError):m.validate_go(bad,plan,200)
        go['expires_at_ns']=601_000_000_100
        with self.assertRaises(ValueError):m.validate_go(go,plan,200)

    def test_execute_reuses_locks_postchecks_but_never_snapshots_or_writes(self):
        for fault in (None,3,'post'):
            root=self.root/str(fault);root.mkdir()
            helper=root/'helper.py';helper.write_text('# 本机空helper\n')
            bindings=self.bindings();bindings['proc_helper']={'path':str(helper)}
            bindings['protection']={}
            for side in m.SERIALS:
                bindings['protection'][side]={}
                for name in ('rx_trust.lock','estop_latch.lock'):
                    path=root/f'{side}-{name}';path.write_bytes(b'unchanged\n')
                    bindings['protection'][side][name]={'path':str(path)}
            executor=FeedbackIO();executor.fail_at=fault
            read_text=Path.read_text
            def read_boot(path,*args,**kwargs):
                return 'unit-boot' if str(path)=='/proc/sys/kernel/random/boot_id' else read_text(path,*args,**kwargs)
            with (patch.object(m,'MAINTENANCE_NAMESPACE',self.root),
                  patch.object(m.sys,'platform','linux'),patch.object(m.os,'uname',return_value=SimpleNamespace(machine='aarch64',nodename='unit-host')),
                  patch.object(Path,'read_text',read_boot),patch.object(m,'RealExecutor',return_value=executor),
                  patch.object(m,'verify_ref',return_value=None),patch.object(m,'protected_and_refs'),patch.object(m,'verify_protection_file',return_value=None),
                  patch.object(m,'idle'),patch.object(m,'usb_identity',return_value=None),patch.object(m,'inspect_unit',return_value=None),
                  patch.object(m,'inspect_processes',return_value=None,
                               side_effect=ValueError('后验身份未知') if fault=='post' else None) as process,
                  patch.object(m,'inspect_holder',return_value=None),
                  patch.object(m,'snapshot',side_effect=AssertionError('只读不可整机snapshot')) as snapshot,
                  patch.object(m,'run_motion_probes',side_effect=AssertionError('不可动作')) as motion,
                  patch.object(m,'finish_verification',side_effect=AssertionError('不可STOP收尾')) as finish,
                  patch.object(m.signal,'signal'),contextlib.redirect_stdout(io.StringIO())):
                plan=m.make_plan(bindings,root/'run','feedback',root,left_wheel_feedback_only=True)
                go=self.go(plan,m.time.time_ns());approval=root/'go.json';m.write_new(approval,go)
                self.assertEqual(m.execute(plan,go,approval),0 if fault is None else 1)
                snapshot.assert_not_called();motion.assert_not_called();finish.assert_not_called()
                self.assertEqual(process.call_count,1)
            report=json.loads((root/'run/run.json').read_text())
            self.assertEqual(report['result'],'DIAGNOSTIC_CAPTURED' if fault is None else 'NEEDS_MANUAL_POWER_CUT')
            self.assertEqual(report['stop_verification']['status'],'NOT_EVALUATED_DIAGNOSTIC_ONLY')
            self.assertEqual(report['identity_verification']['status'],'FAIL' if fault=='post' else 'PASS')
            self.assertEqual(len(report['owned_locks']),4)
            self.assertEqual(len(executor.calls),3 if fault==3 else 12)
            self.assertTrue(Path(str(approval)+'.consumed').exists())
            self.assertFalse(report['deployable'])


if __name__=='__main__': unittest.main(verbosity=2)
