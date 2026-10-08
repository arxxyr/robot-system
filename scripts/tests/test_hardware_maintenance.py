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


if __name__=='__main__': unittest.main(verbosity=2)
