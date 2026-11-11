"""爸爸，维护收尾确定性测试；只用假寄存器及自有短命子进程，不访问硬件。"""
import contextlib
import atexit
import itertools
import copy
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import signal
import stat
import subprocess
import sys
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
import run_hardware_maintenance as m


# 测试显式提供独立TOML，不把工作树默认配置变成隐藏fixture。
BUS_FIXTURE_DIRECTORY=tempfile.TemporaryDirectory()
atexit.register(BUS_FIXTURE_DIRECTORY.cleanup)
BUS_FIXTURE_PATH=Path(BUS_FIXTURE_DIRECTORY.name)/'bus2.toml'
LEFT_BUS_FIXTURE_PATH=Path(BUS_FIXTURE_DIRECTORY.name)/'bus1.toml'


def write_bus_fixture(path,ids=(8,9,7),*,side='right',tolerances=None):
    names=([*m.MIXED_ARM_NAMES,*(f'base_{role}_wheel' for role in m.WHEEL_ROLES)] if side=='right' else
           [name.replace('right_','left_',1) for name in m.MIXED_ARM_NAMES]+['head_motor_1','head_motor_2'])
    motors=[*range(1,7),*ids] if side=='right' else list(range(1,9))
    path.write_text('[[hardware]]\nadapter = "feetech_sts3215"\naddress = '+json.dumps(m.DEVICES[side])+
                    '\njoints = '+json.dumps(names)+'\nmotor_ids = '+json.dumps(motors)+'\n'+
                    ('' if tolerances is None else 'calibration_range_tolerances_raw = '+json.dumps(tolerances)+'\n'))
    return path


m.configure_bus_mapping(write_bus_fixture(BUS_FIXTURE_PATH),write_bus_fixture(LEFT_BUS_FIXTURE_PATH,side='left'))
def run_main(args):
    return m.main(['--bus-config',str(BUS_FIXTURE_PATH),'--left-bus-config',str(LEFT_BUS_FIXTURE_PATH),*args])


def stop_contracts():
    return {'bus_config':copy.deepcopy(m.BUS_CONFIG),'left_bus_config':copy.deepcopy(m.LEFT_BUS_CONFIG),
            **{f'{prefix}{motor}_stop_contract':m.left_stop_contract(motor=motor)
            for motor,prefix in m.WHEEL_STOP_TARGETS.items()}}


PLAN = {'position_targets':m.TARGETS, 'position_delta_raw':24,'minimum_excursion_raw':12,'return_tolerance_raw':4,
        'position_wait_ms':800,'base_raw':[20,-20],'pulse_target_ms':150,
        'nonzero_window_fail_ns':1_000_000_000,'observed_velocity_raw_max':40,
        'observed_wheel_delta_raw_min':3,'observed_wheel_delta_raw_max':64,
        **stop_contracts()}
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

    def release_fixture(self,goal=1600):
        calibration={'id':6,'drive_mode':0,'homing_offset':0,'range_min':1384,'range_max':2847}
        bindings={'schema':'MAINTENANCE_BINDINGS_V1','tool':{'sha256':m.TOOL_SHA,'bytes':m.TOOL_BYTES},
                  'usb':{side:{'device':m.DEVICES[side],'serial':m.SERIALS[side]} for side in m.SERIALS},
                  'calibrations':{'right_arm_gripper':calibration},'refs':[dict(m.RIGHT_PULSE_TRACE)],
                  'binary_paths':[m.RIGHT_PULSE_TRACE['path']]}
        root='/home/loosqk/robot-checks/release-fixture'
        plan=m.make_plan(bindings,root+'/run','release-fixture',root,right_gripper_release=True,gripper_goal=goal)
        executor=FakeIO(step=64);executor.position=executor.goal=1396 if goal==1600 else 1596
        original=executor.poke
        def auto_enable(side,motor,action,register,value=None,cleanup=False):
            result=original(side,motor,action,register,value,cleanup)
            if (side,motor,action,register)==('right',6,'write','Goal_Position'):executor.torque=1
            return result
        executor.poke=auto_enable
        return executor,m.ProbeEngine(executor,plan),calibration

    def test_right_gripper_release_is_one_way_to_fixed_goal_and_preserves_other_targets(self):
        executor,engine,calibration=self.release_fixture()
        m.run_motion_probes(engine,engine.plan['bindings'],engine.plan)
        row=engine.rows[0]
        self.assertEqual((row['status'],row['initial_raw'],row['goal_raw']),('PASS',1396,1600))
        self.assertEqual((executor.position,executor.goal,executor.torque),(1600,1600,0))
        writes=[call for call in executor.calls if len(call)==6 and call[2]=='write']
        self.assertEqual([(call[3],call[4],call[5]) for call in writes],
                         [('Goal_Position',1600,False),('Torque_Enable',0,True)])
        self.assertEqual({call[:2] for call in executor.calls if len(call)==6},{('right',6)})
        witness=row['position_accuracy']['stages']['release']
        self.assertEqual(witness['started_ns'],row['goal_write_attempt_ns'])
        self.assertEqual(witness['deadline_ns']-witness['started_ns'],800_000_000)
        before=idle_rows();after=copy.deepcopy(before)
        before['right:6'].update(Present_Position=1396,Goal_Position=1396)
        after['right:6'].update(Present_Position=1600,Goal_Position=1600)
        self.assertEqual(m.classify_position_snapshot(after,before,engine.rows)['status'],'PASS')
        m.validate_snapshot(after,before,engine.rows)
        self.assertEqual(len(engine.rows),1)
        report={'plan':engine.plan,'probes':engine.rows,'stop_verification':{'status':'PASS'},
                'identity_verification':{'status':'PASS'},'position_verification':{'status':'PASS'},
                'child_active':False,'evidence_errors':[]}
        self.assertEqual(m.classify_run_result(report),'PASS')
        report['stop_verification']['status']='UNKNOWN'
        self.assertEqual(m.classify_run_result(report),'NEEDS_MANUAL_POWER_CUT')
        for key in ('left:6','left:7'):
            after[key]['Present_Position']+=5
            self.assertEqual(m.classify_position_snapshot(after,before,engine.rows)['status'],'FAIL')
            with self.assertRaises(ValueError):m.validate_snapshot(after,before,engine.rows)
            after[key]['Present_Position']-=5

    def test_right_gripper_close_reuses_one_way_action_and_binds_its_target(self):
        executor,engine,_=self.release_fixture(1397)
        m.run_motion_probes(engine,engine.plan['bindings'],engine.plan)
        self.assertEqual((executor.position,executor.goal,executor.torque),(1397,1397,0))
        self.assertEqual([(call[3],call[4],call[5]) for call in executor.calls if len(call)==6 and call[2]=='write'],
                         [('Goal_Position',1397,False),('Torque_Enable',0,True)])
        before=idle_rows();after=copy.deepcopy(before)
        before['right:6'].update(Present_Position=1596,Goal_Position=1596)
        after['right:6'].update(Present_Position=1397,Goal_Position=1397)
        self.assertEqual(m.classify_position_snapshot(after,before,engine.rows)['status'],'PASS')
        m.validate_snapshot(after,before,engine.rows)
        for error in (5,-5,6,-6):
            trial,trial_engine,calibration=self.release_fixture(1397);original=trial.poke;trial.step=None
            def held_error(side,motor,action,register,value=None,cleanup=False):
                result=original(side,motor,action,register,value,cleanup)
                if action=='read' and register=='Present_Position' and trial.torque:
                    trial.position=1397+error
                    return trial.position
                return result
            trial.poke=held_error
            with self.subTest(error=error):
                if abs(error)<=5:trial_engine.gripper_release(calibration)
                else:
                    with self.assertRaises(ValueError):trial_engine.gripper_release(calibration)
                self.assertEqual(trial_engine.rows[0]['position_accuracy']['stages']['release']['tolerance_raw'],5)
                after['right:6']['Present_Position']=1397+error
                classified=m.classify_position_snapshot(after,before,engine.rows)
                self.assertEqual(classified['status'],'PASS' if abs(error)<=5 else 'FAIL')
                if abs(error)<=5:m.validate_snapshot(after,before,engine.rows)
                else:
                    self.assertEqual(classified['issues'][0]['tolerance_raw'],5)
                    with self.assertRaises(ValueError):m.validate_snapshot(after,before,engine.rows)
        report={'plan':engine.plan,'probes':engine.rows,'stop_verification':{'status':'PASS'},
                'identity_verification':{'status':'PASS'},'position_verification':{'status':'PASS'}}
        self.assertEqual(m.classify_run_result(report),'PASS')
        engine.rows[0]['goal_raw']=1600
        self.assertEqual(m.classify_run_result(report),'FAIL')

    def test_right_gripper_close_rejects_wrong_start_or_excess_increment_without_goal_write(self):
        for initial in (1396,1397,1654):
            executor,engine,calibration=self.release_fixture(1397);executor.position=initial
            with self.subTest(initial=initial),self.assertRaises(ValueError):engine.gripper_release(calibration)
            self.assertFalse(any(call[2:4]==('write','Goal_Position') for call in executor.calls if len(call)==6))
        for goal in (True,1398,1397.0):
            with self.subTest(goal=goal),self.assertRaises(ValueError):self.release_fixture(goal)
        for tolerance in (4,6):
            _,engine,_=self.release_fixture(1397);engine.plan['return_tolerance_raw']=tolerance
            with self.subTest(tolerance=tolerance),self.assertRaises(ValueError):m.validate_gripper_release_plan(engine.plan)

    def test_right_gripper_release_rejects_drift_before_action_and_mixed_entry(self):
        executor,engine,calibration=self.release_fixture();original=executor.poke;reads=[]
        def drift(side,motor,action,register,value=None,cleanup=False):
            if action=='read' and register=='Present_Position' and not cleanup:
                reads.append(True)
                if len(reads)==2:executor.position+=1
            return original(side,motor,action,register,value,cleanup)
        executor.poke=drift
        with self.assertRaises(ValueError):engine.gripper_release(calibration)
        self.assertEqual(executor.torque,0)
        self.assertNotIn(('right',6,'write','Goal_Position',1600,False),executor.calls)
        self.assertNotIn(('right',6,'write','Torque_Enable',1,False),executor.calls)
        self.assertIn(('right',6,'write','Torque_Enable',0,True),executor.calls)
        with self.assertRaises(ValueError):engine.gripper_release(calibration)
        with self.assertRaises(ValueError):run_main(['--right-gripper-release','--position-target','head-1'])
        with self.assertRaises(ValueError):run_main(['--right-gripper-close','--position-target','head-1'])
        with contextlib.redirect_stderr(io.StringIO()),self.assertRaises(SystemExit):
            run_main(['--right-gripper-release','--right-gripper-close'])
        for initial in (1383,2848):
            executor,engine,calibration=self.release_fixture();executor.position=initial
            with self.subTest(initial=initial),self.assertRaises(ValueError):engine.gripper_release(calibration)
            self.assertFalse(any(call[2:4]==('write','Goal_Position') for call in executor.calls if len(call)==6))

    def test_right_gripper_unknown_write_ack_still_disables_once_without_retry_or_return(self):
        for goal in (1600,1397):
            executor,engine,calibration=self.release_fixture(goal);original=executor.poke;initial=executor.position
            def unknown(side,motor,action,reg,written=None,cleanup=False):
                result=original(side,motor,action,reg,written,cleanup)
                if action=='write' and reg=='Goal_Position':raise OSError('GP写已生效并使能，ACK未知')
                return result
            executor.poke=unknown
            with self.subTest(goal=goal),self.assertRaises(ValueError):engine.gripper_release(calibration)
            self.assertEqual((executor.goal,executor.torque),(goal,0))
            self.assertEqual(executor.calls.count(('right',6,'write','Goal_Position',goal,False)),1)
            self.assertEqual(executor.calls.count(('right',6,'write','Torque_Enable',0,True)),1)
            self.assertNotIn(('right',6,'write','Torque_Enable',1,False),executor.calls)
            self.assertNotIn(('right',6,'write','Goal_Position',initial,True),executor.calls)

    def test_right_gripper_late_target_read_fails_budget_from_first_goal_write(self):
        executor,engine,calibration=self.release_fixture();original=executor.poke;executor.step=None
        def late(side,motor,action,reg,value=None,cleanup=False):
            result=original(side,motor,action,reg,value,cleanup)
            if action=='read' and reg=='Present_Position' and executor.torque:executor.t+=801_000_000
            return result
        executor.poke=late
        with self.assertRaises(ValueError):engine.gripper_release(calibration)
        row=engine.rows[0];failure=row['accuracy_failure']
        self.assertEqual(failure['started_ns'],row['goal_write_attempt_ns'])
        self.assertEqual(failure['final_signed_error_raw'],0)
        self.assertGreater(failure['elapsed_ns'],800_000_000)
        self.assertEqual((executor.position,executor.torque),(1600,0))

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


class CalibrationRangeToleranceTests(unittest.TestCase):
    def setUp(self):
        directory=tempfile.TemporaryDirectory();self.addCleanup(directory.cleanup);self.root=Path(directory.name)
        self.addCleanup(m.configure_bus_mapping,BUS_FIXTURE_PATH,LEFT_BUS_FIXTURE_PATH)

    def configure(self,right=None,left=None):
        m.configure_bus_mapping(write_bus_fixture(self.root/'right.toml',tolerances=right),
            write_bus_fixture(self.root/'left.toml',side='left',tolerances=left))
        return {**PLAN,**stop_contracts()}

    def test_per_slot_mapping_defaults_other_values_and_hard_raw_bounds(self):
        plan=self.configure()
        self.assertTrue(all(m.motor_range_tolerance(plan,side,motor)==0 for side,count in (('left',8),('right',9))
                            for motor in range(1,count+1)))
        plan=self.configure([0,20,7,4095,20,3,11,12,13],[1,2,3,4,5,6,20,4095])
        self.assertEqual([m.motor_range_tolerance(plan,'right',motor) for motor in (1,2,3,7,8,9)],
                         [0,20,7,13,11,12])
        self.assertEqual((m.motor_range_tolerance(plan,'left',7),m.motor_range_tolerance(plan,'left',8)),(20,4095))
        self.assertEqual(m.effective_calibration_bounds({'range_min':10,'range_max':4080},20),(0,4095))
        self.assertEqual(m.effective_calibration_bounds({'range_min':100,'range_max':200},0),(100,200))

    def test_bad_arrays_and_both_config_or_plan_tampering_are_rejected_before_executor(self):
        for side,count in (('left',8),('right',9)):
            for bad in ([],[0]*(count-1),[0]*(count-1)+[-1],[0]*(count-1)+[True],
                        [0]*(count-1)+[1.5],[0]*(count-1)+[4096],20):
                with self.subTest(side=side,bad=bad),self.assertRaises(ValueError):
                    self.configure(**{side:bad})
            self.configure()
            _,engine,_=ProbeTests.release_fixture(self);plan=engine.plan
            key='bus_config' if side=='right' else 'left_bus_config'
            plan[key]['calibration_range_tolerances_raw'][0]=20
            current=m.BUS_CONFIG if side=='right' else m.LEFT_BUS_CONFIG
            self.assertEqual(current['calibration_range_tolerances_raw'][0],0)
            with patch.object(m,'RealExecutor',side_effect=AssertionError('配置/计划篡改不应构造executor')) as executor:
                with self.assertRaises(ValueError):m.execute(plan,{},self.root/'go.json')
                _,engine,_=ProbeTests.release_fixture(self);plan=engine.plan
                path=Path(plan[key]['path']);path.write_bytes(path.read_bytes()+b'# changed\n')
                with self.assertRaisesRegex(ValueError,'字节身份变化'):m.execute(plan,{},self.root/'go.json')
                executor.assert_not_called()

    def test_all_selected_position_boundaries_use_motor_policy_without_changing_accuracy(self):
        plan=self.configure([0,0,0,0,20,20,0,0,0],[20]*8)
        for side,motor,name in m.TARGETS:
            for initial,accepted in ((80,True),(79,False),(196,True),(197,False)):
                executor=FakeIO();executor.position=executor.goal=initial;engine=m.ProbeEngine(executor,plan)
                calibration={'id':motor,'range_min':100,'range_max':200}
                with self.subTest(side=side,motor=motor,initial=initial):
                    if accepted:
                        engine.position(side,motor,name,calibration)
                        self.assertEqual((engine.rows[0]['anchor_raw'],engine.rows[0]['goal_raw']),(initial,initial+24))
                        self.assertEqual(engine.rows[0]['position_accuracy']['stages']['outward']['tolerance_raw'],4)
                    else:
                        with self.assertRaises(ValueError):engine.position(side,motor,name,calibration)
                        self.assertFalse(any(call[2:4]==('write','Goal_Position') for call in executor.calls if len(call)==6))
        plan=self.configure([0,0,0,0,4095,0,0,0,0])
        for initial in (-1,4072):
            executor=FakeIO();executor.position=executor.goal=initial;engine=m.ProbeEngine(executor,plan)
            with self.subTest(hard_bound=initial),self.assertRaises(ValueError):
                engine.position('right',5,'right_arm_wrist_roll',{'id':5,'range_min':0,'range_max':4095})

    def test_gripper_keeps_original_raw_and_five_tick_arrival_accuracy(self):
        self.configure([0,0,0,0,0,20,0,0,0])
        for initial,accepted in ((1364,True),(1363,False)):
            executor,engine,calibration=ProbeTests.release_fixture(self);executor.position=executor.goal=initial
            with self.subTest(initial=initial):
                if accepted:
                    engine.gripper_release(calibration)
                    self.assertEqual(engine.rows[0]['initial_raw'],1364)
                    self.assertEqual(engine.plan['right_gripper_release']['calibration']['range_min'],1384)
                    self.assertEqual(engine.rows[0]['position_accuracy']['stages']['release']['tolerance_raw'],5)
                else:
                    with self.assertRaises(ValueError):engine.gripper_release(calibration)
                    self.assertNotIn(('right',6,'write','Goal_Position',1600,False),executor.calls)


class ProcessNameTests(unittest.TestCase):
    def test_import_does_not_set_name_and_main_sets_name(self):
        spec=importlib.util.spec_from_file_location('maintenance_import_fixture',m.__file__)
        module=importlib.util.module_from_spec(spec)
        with patch.object(Path,'write_text') as write:
            spec.loader.exec_module(module)
            write.assert_not_called()
        with patch.object(m,'set_process_name') as name,contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(run_main([]),0)
            name.assert_called_once_with()

    def test_linux_comm_failure_is_nonfatal_and_other_platform_does_not_write(self):
        with patch.object(m.sys,'platform','linux'),patch.object(Path,'write_text') as write:
            m.set_process_name()
            write.assert_called_once_with('robot-maint',encoding='ascii')
        with patch.object(m.sys,'platform','linux'),patch.object(Path,'write_text',side_effect=OSError('拒绝命名')), \
                contextlib.redirect_stderr(io.StringIO()):
            m.set_process_name()
        with patch.object(m.sys,'platform','darwin'),patch.object(Path,'write_text') as write:
            m.set_process_name()
            write.assert_not_called()

    def reference(self,path):
        data=b'\x7fELF\x02\x01'+b'\0'*12+(183).to_bytes(2,'little')
        path.write_bytes(data);path.chmod(0o700)
        return {'path':str(path),'sha256':hashlib.sha256(data).hexdigest(),'bytes':len(data)}

    def test_verified_feetech_exec_alias_preserves_identity_and_records_actual_argv(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            refs={name:self.reference(root/name) for name in ('tool','raw','write','sync','mixed')}
            with patch.object(m,'RIGHT_PULSE_RAW',refs['raw']),patch.object(m,'RIGHT_PULSE_WRITE',refs['write']), \
                    patch.object(m,'RIGHT_PULSE_SYNC',refs['sync']),patch.object(m.sys,'platform','linux'):
                for index,(name,trace) in enumerate((('tool',False),('raw',False),('write',False),
                        ('sync',False),('mixed',False),('raw',True),('mixed',True))):
                    with self.subTest(name=name,trace=trace):
                        output=root/f'run{index}';output.mkdir()
                        executor=m.RealExecutor(refs['tool'],output);executor.mixed_feetech_tool=refs['mixed']
                        self.addCleanup(executor.journal.close)
                        target=Path(refs[name]['path']);before=target.stat();spawned=[]
                        argv=[str(target),'/fixture/no-uart','9','read','Torque_Enable']
                        leaf=0
                        if trace:
                            prefix=[m.RIGHT_PULSE_TRACE['path'],*(['-f','-I','3'] if name=='mixed' else []),
                                    '-ttt','-xx','-s','256','-yy','-e','trace=openat,close,read,write,ioctl',
                                    '-o',str(output/'wire.strace')]
                            leaf=len(prefix);argv=[*prefix,*argv]
                        def popen(actual,**kwargs):
                            self.assertNotIn('preexec_fn',kwargs)
                            self.assertEqual(actual[:leaf],argv[:leaf]);self.assertEqual(actual[leaf+1:],argv[leaf+1:])
                            alias=Path(actual[leaf]);spawned.append(list(actual))
                            self.assertEqual(alias.name,'robot-feetech');self.assertEqual(alias.resolve(),target.resolve())
                            self.assertEqual(stat.S_IMODE(alias.parent.stat().st_mode),0o700)
                            def wait(timeout):
                                self.assertTrue(alias.is_symlink())
                                return 0
                            return SimpleNamespace(pid=123,wait=wait)
                        executor.front_feedback_before_spawn=lambda original:self.assertEqual(original,argv)
                        with patch.object(m.subprocess,'Popen',side_effect=popen):
                            self.assertEqual(executor.command(argv),'')
                        row=executor.commands[0]
                        self.assertEqual(row['argv'],spawned[0]);self.assertEqual(row['requested_argv'],argv)
                        self.assertEqual(json.loads((output/'commands.jsonl').read_text())['argv'],spawned[0])
                        self.assertFalse(Path(spawned[0][leaf]).parent.exists())
                        self.assertTrue(row['reaped']);self.assertIsNone(executor.child)
                        after=target.stat()
                        self.assertEqual((before.st_ino,before.st_mtime_ns,before.st_ctime_ns,before.st_nlink),
                                         (after.st_ino,after.st_mtime_ns,after.st_ctime_ns,after.st_nlink))
                        self.assertEqual(hashlib.sha256(target.read_bytes()).hexdigest(),refs[name]['sha256'])

    def test_system_tool_arguments_are_not_renamed_and_original_ref_gate_remains_strict(self):
        with tempfile.TemporaryDirectory() as directory,patch.object(m.sys,'platform','linux'):
            root=Path(directory);reference=self.reference(root/'tool')
            executor=m.RealExecutor(reference,root);self.addCleanup(executor.journal.close)
            for argv in (['sudo','-n',reference['path']],['systemctl','status',reference['path']],
                         [m.RIGHT_PULSE_TRACE['path'],'--',reference['path']]):
                row={'argv':list(argv)}
                with patch.object(m,'verify_ref',side_effect=AssertionError('系统工具不应走命名验证')), \
                        executor.named_feetech_command(argv,row) as actual:
                    self.assertEqual(actual,argv);self.assertNotIn('requested_argv',row)
            alias=root/'external-link';alias.symlink_to(reference['path'])
            for invalid in (dict(reference,sha256='0'*64),dict(reference,path=str(alias))):
                executor.tool=invalid
                with patch.object(m.subprocess,'Popen') as child,self.assertRaises(ValueError):
                    executor.command([invalid['path']])
                child.assert_not_called()

    def test_alias_failure_falls_back_to_original_cleanup_command_without_safety_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);reference=self.reference(root/'tool')
            executor=m.RealExecutor(reference,root);self.addCleanup(executor.journal.close)
            argv=[reference['path'],'/fixture/no-uart','9','write','Torque_Enable','0']
            child=SimpleNamespace(pid=123,wait=lambda timeout:0)
            with patch.object(m.sys,'platform','linux'), \
                    patch.object(m.tempfile,'TemporaryDirectory',side_effect=OSError('临时显示名不可用')), \
                    patch.object(m.subprocess,'Popen',return_value=child) as spawn,contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(executor.command(argv,cleanup=True),'')
            self.assertEqual(spawn.call_args.args[0],argv)
            self.assertEqual(executor.commands[0]['argv'],argv)
            self.assertTrue(executor.commands[0]['process_name_warnings'])
            self.assertEqual(executor.commands[0]['owner_errors'],[])
            self.assertEqual(executor.evidence_errors,[])

    def test_alias_cleanup_failure_does_not_fail_reaped_stop_command(self):
        temporary_directory=m.tempfile.TemporaryDirectory
        with temporary_directory() as directory:
            root=Path(directory);reference=self.reference(root/'tool')
            executor=m.RealExecutor(reference,root);self.addCleanup(executor.journal.close)
            def failing_cleanup(*args,**kwargs):
                temporary=temporary_directory(*args,**kwargs);cleanup=temporary.cleanup
                def fail():
                    cleanup()
                    raise OSError('显示名清理失败')
                temporary.cleanup=fail
                return temporary
            child=SimpleNamespace(pid=123,wait=lambda timeout:0)
            with patch.object(m.sys,'platform','linux'), \
                    patch.object(m.tempfile,'TemporaryDirectory',side_effect=failing_cleanup), \
                    patch.object(m.subprocess,'Popen',return_value=child),contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(executor.command([reference['path']],cleanup=True),'')
            self.assertTrue(executor.commands[0]['reaped'])
            self.assertTrue(executor.commands[0]['process_name_warnings'])
            self.assertFalse(Path(executor.commands[0]['argv'][0]).parent.exists())
            self.assertEqual(executor.commands[0]['owner_errors'],[])
            self.assertEqual(executor.evidence_errors,[])

    def test_alias_survives_launch_exception_until_original_child_is_reaped(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);reference=self.reference(root/'tool')
            executor=m.RealExecutor(reference,root);self.addCleanup(executor.journal.close)
            aliases=[];original_reap=executor.reap;attempts=[]
            def reap(*args,**kwargs):
                attempts.append(True)
                if len(attempts)==1:
                    raise OSError('child启动后的异常')
                self.assertTrue(aliases[0].is_symlink())
                return original_reap(*args,**kwargs)
            executor.reap=reap
            def popen(argv,**kwargs):
                aliases.append(Path(argv[0]))
                return SimpleNamespace(pid=123,wait=lambda timeout:0)
            with patch.object(m.sys,'platform','linux'),patch.object(m.subprocess,'Popen',side_effect=popen):
                with self.assertRaisesRegex(OSError,'child启动后的异常'):
                    executor.command([reference['path']])
            self.assertEqual(len(attempts),2);self.assertIsNone(executor.child)
            self.assertTrue(executor.commands[0]['reaped'])
            self.assertFalse(aliases[0].parent.exists())


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

    def test_mixed_child_requests_one_group_interrupt_then_reaps_without_kill(self):
        for reason in ('go','signal','budget'):
            with self.subTest(reason=reason),tempfile.TemporaryDirectory() as d:
                e=self.make_io(d);ready=Path(d)/'ready';stopped=Path(d)/'stopped'
                code=('import pathlib,signal,time\n'
                      f'ready=pathlib.Path({str(ready)!r});stopped=pathlib.Path({str(stopped)!r})\n'
                      'def stop(_sig,_frame):\n'
                      ' stopped.write_text("stopped");print("stopped",flush=True);raise SystemExit(0)\n'
                      'signal.signal(signal.SIGINT,stop);ready.write_text("ready")\n'
                      'while True:time.sleep(.01)\n')
                actual_now=e.now;shift=[0]
                e.now=lambda:actual_now()+shift[0]
                def check():
                    if not ready.exists():return
                    if reason=='go':raise ValueError('原GO到期')
                    if reason=='signal':e.mark_signal(signal.SIGHUP,None);e.check()
                    shift[0]=m.MIXED_CHILD_BUDGET_NS+1
                with self.assertRaises(ValueError):
                    e.command([sys.executable,'-c',code],mixed_stop_check=check)
                row=e.commands[0]
                self.assertIsNone(e.child);self.assertTrue(row['reaped'])
                self.assertEqual(row['exit_code'],0)
                self.assertEqual(row['budget_ns'],28_000_000_000)
                self.assertEqual(len(row['signals_sent']),1)
                self.assertEqual(row['signals_sent'][0]['signal'],int(signal.SIGINT))
                self.assertEqual(stopped.read_text(),'stopped')
                self.assertEqual(row['over_budget'],reason=='budget')
                self.assertTrue(row['mixed_stop_requested']);e.journal.close()

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
            self.assertEqual(run_main([]),0)
        self.assertEqual(json.loads(output.getvalue())['actions_started'],0)

    def test_actual_cli_default_plan_only_exit_zero(self):
        for args in ([],['--help']):
            child=subprocess.Popen([sys.executable,str(Path(m.__file__)),*args],stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            out,err=child.communicate()
            self.assertEqual(child.wait(),0); self.assertEqual(err,b'')
            if not args:self.assertEqual(json.loads(out)['actions_started'],0)

    def test_go_rejects_wrong_plan_missing_facts_and_expiration(self):
        plan={'schema':'MAINTENANCE_PLAN_V2','artifact_root':'/home/loosqk/robot-checks/test',
              **stop_contracts(),
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
              **stop_contracts(),
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
              **stop_contracts(),
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


def feedback_reference(path):
    elf=bytearray(64);elf[:6]=b'\x7fELF\x02\x01';elf[18:20]=(183).to_bytes(2,'little')
    path.write_bytes(elf);path.chmod(0o755);info=path.stat()
    return {'path':str(path),'sha256':hashlib.sha256(elf).hexdigest(),'bytes':len(elf),
            'uid':info.st_uid,'gid':info.st_gid,'mode':0o755,'device':info.st_dev,
            'inode':info.st_ino,'mtime_ns':info.st_mtime_ns,'ctime_ns':info.st_ctime_ns,
            'nlink':1,'symlink':False,'elf_machine':183}


class FeedbackIO:
    """右7只读内存端点；计时、故障及原帧可独立注入。"""
    def __init__(self,motor=7):
        self.motor=motor
        self.t=0;self.sequence=0;self.commands=[];self.calls=[]
        self.child=None;self.interrupted=None;self.evidence_errors=[]
        self.process_snapshots=[];self.journal=io.StringIO()
        self.fail_at=None;self.torque_at=None;self.signal_at=None;self.bad_at=None
        self.slow_at=None;self.slow_ns=0;self.velocity_index=0;self.position_index=0
        self.positions=[1212]*5;self.velocities=[0,50,-50,50,0];self.goal_at=None
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
        prelaunch=getattr(self,'front_feedback_before_spawn',None)
        if prelaunch is not None:prelaunch(argv)
        self.sequence+=1;self.calls.append(list(argv))
        self.commands.append({'argv':list(argv),'started_ns':self.t})
        self.t+=10_000_000+(self.slow_ns if self.sequence==self.slow_at else 0)
        self.commands[-1]['finished_ns']=self.t
        if self.sequence==self.fail_at:raise OSError('夹具通信失败')
        if self.sequence==self.signal_at:self.interrupted=signal.SIGTERM
        register=argv[4];address,width=m.LEFT_FEEDBACK_META[register]
        value=0
        if register=='Torque_Enable':value=int(self.sequence==self.torque_at)
        if register=='Goal_Velocity':value=int(self.sequence==self.goal_at)
        if register=='Present_Position':
            value=self.positions[self.position_index%len(self.positions)];self.position_index+=1
        if register=='Present_Velocity':
            value=self.velocities[self.velocity_index%len(self.velocities)];self.velocity_index+=1
        value=abs(value)|(0x8000 if value<0 else 0)
        tx=[255,255,254,5,130,address,width,self.motor];tx.append((~sum(tx[2:]))&255)
        rx=[255,255,self.motor,width+2,0,*value.to_bytes(width,'little')];rx.append((~sum(rx[2:]))&255)
        if self.sequence==self.bad_at:rx[-1]^=1
        return json.dumps({'operation':'sync_read','serial_device':m.DEVICES['right'],'motor_id':self.motor,
                           'register':register,'requested_register_address':address,'width':width,
                           'raw_unsigned':value,'tx':tx,'rx':rx})+'\n'


class SnapshotIO(FeedbackIO):
    def __init__(self,rows,fail=None):
        super().__init__()
        self.rows=rows;self.fail=fail;self.reads=[];self.positions=[rows['right:7']['Present_Position']]*5
        self.wheel_samples={motor:{'positions':[rows[f'right:{motor}']['Present_Position']]*5,
            'velocities':[0]*5,'position_index':0,'velocity_index':0} for motor in (8,9)}
    def command(self,argv,**kwargs):
        if argv[0]!=m.RIGHT_PULSE_RAW['path'] or argv[2] not in ('8','9'):return super().command(argv,**kwargs)
        motor=int(argv[2]);samples=self.wheel_samples[motor]
        saved=(self.motor,self.positions,self.velocities,self.position_index,self.velocity_index)
        self.motor=motor;self.positions=samples['positions'];self.velocities=samples['velocities']
        self.position_index=samples['position_index'];self.velocity_index=samples['velocity_index']
        try:return super().command(argv,**kwargs)
        finally:
            samples['position_index'],samples['velocity_index']=self.position_index,self.velocity_index
            self.motor,self.positions,self.velocities,self.position_index,self.velocity_index=saved
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
        directory=tempfile.TemporaryDirectory();self.addCleanup(directory.cleanup)
        reference=feedback_reference(Path(directory.name)/'b2')
        replacement=patch.dict(m.RIGHT_PULSE_RAW,{k:reference[k] for k in ('path','sha256','bytes')},clear=True)
        replacement.start();self.addCleanup(replacement.stop)
        executor=SnapshotIO(after,read_fail)
        engine=m.ProbeEngine(executor,{**PLAN,**stop_contracts(),
                                       'bindings':{'refs':[reference]}})
        engine.left_stop_go_check=lambda:None
        engine.rows=copy.deepcopy(probes); engine.manual_cut=manual
        report={'before':before,'error':'原精度FAIL','received_signal':None,'child_active':False,'probes':engine.rows}
        called=[]
        def verifier(name):
            called.append(name)
            if name==identity_fail: raise OSError('后验不可得')
        names=['rx_trust_marker','rx_trust_marker.bak','estop_latch','estop_latch.bak','rx_trust.lock','estop_latch.lock']
        bindings={'usb':{'left':{},'right':{}},'refs':[{'path':'/first'},{'path':'/next'}],
                  'protection':{side:{name:{'path':f'/fake/{side}/{name}'} for name in names} for side in m.SERIALS}}
        with (patch.object(m,'verify_ref',side_effect=lambda r,**k:None if r['path']==reference['path'] else verifier('ref:'+r['path'])),
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
        self.assertEqual(plan['base_order'],[8,9,7])
        self.assertEqual([t['motor'] for t in plan['base_targets']],[8,9,7])
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
                    with self.assertRaises((ValueError,SystemExit)):run_main(args)
            for targets in ([],[m.TARGETS[0],m.TARGETS[0]],[('right',9,'base_left_wheel')],
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
            self.assertEqual(run_main(['--position-target','head-2','--position-target','right-wrist']),0)
        reply=json.loads(output.getvalue())
        self.assertEqual(reply['actions_started'],0)
        self.assertEqual(reply['position_targets'],[['left',8,'head_motor_2'],['right',5,'right_arm_wrist_roll']])
        launch.assert_not_called()

    def test_execute_route_four_positions_pass_base_blocked_still_all_postchecks(self):
        class IntegratedIO:
            def __init__(self,tool,output):
                self.regs=idle_rows();self.calls=[];self.child=None;self.interrupted=None;self.commands=[]
                self.process_snapshots=[];self.evidence_errors=[];self.journal=io.StringIO();self.t=0;self.output=Path(output)
                self.feedback=SnapshotIO(self.regs);self.sequence=0
            def mark_signal(self,*args):raise AssertionError('本例不发送信号')
            def check(self):pass
            def now(self):return self.t
            def sleep(self,seconds,cleanup=False):self.t+=int(seconds*1e9)
            def command(self,argv,**kwargs):
                if argv[0]!=m.RIGHT_PULSE_RAW['path']:return ''
                self.feedback.t=self.t
                result=self.feedback.command(argv,**kwargs)
                self.t=self.feedback.t;self.sequence=self.feedback.sequence
                return result
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
            reference=feedback_reference(root/'b2')
            for side in m.SERIALS:
                protection[side]={}
                for name in ('rx_trust.lock','estop_latch.lock'):
                    path=root/f'{side}-{name}';path.write_bytes(b'unchanged\n');protection[side][name]={'path':str(path)}
            bindings={'schema':'MAINTENANCE_BINDINGS_V1','host':{'hostname':'unit-host','uid':actual_uid,'gid':actual_gid,'boot_id':'unit-boot'},
                      'tool':{'path':'/never-access-uart','sha256':m.TOOL_SHA,'bytes':m.TOOL_BYTES},
                      'usb':{side:{'device':m.DEVICES[side],'serial':m.SERIALS[side]} for side in m.SERIALS},
                      'refs':[reference],'protection':protection,'binary_paths':[reference['path']],'proc_helper':{'path':str(helper),'bytes':len(helper_data),'sha256':hashlib.sha256(helper_data).hexdigest()},
                      'calibrations':{name:{'id':motor,'range_min':73,'range_max':3864} for side,motor,name in m.TARGETS}}
            holder={};post=[]
            def factory(tool,output):holder['io']=IntegratedIO(tool,output);return holder['io']
            with (patch.dict(m.RIGHT_PULSE_RAW,{k:reference[k] for k in ('path','sha256','bytes')},clear=True),
                  patch.object(m,'MAINTENANCE_NAMESPACE',namespace),patch.object(m.sys,'platform','linux'),
                  patch.object(m.os,'uname',return_value=SimpleNamespace(machine='aarch64',nodename='unit-host')),
                  patch.object(Path,'read_text',boot_or_text),patch.object(m,'RealExecutor',side_effect=factory),
                  patch.object(m,'verify_ref',return_value=None),patch.object(m,'protected_and_refs'),patch.object(m,'verify_protection_file',return_value=None),
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
            self.assertEqual(len(executor.feedback.calls),84)
            self.assertTrue(report['stop_verification']['right7_stability_used'])
            self.assertTrue(report['stop_verification']['left9_stability_used'])
            self.assertTrue(report['stop_verification']['front8_stability_used'])
            self.assertNotEqual(report['before_right7_stability']['started_ns'],report['after_right7_stability']['started_ns'])
            self.assertNotEqual(report['before_left9_stability']['started_ns'],report['after_left9_stability']['started_ns'])
            json.dumps(report)
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
              **stop_contracts(),
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


class SdkMotorSerial:
    """内存中的单轮电机端点；SDK仍自行组包、收发并解码。"""
    def __init__(self,executor):
        self.io=executor
        self.is_open=True
        self.pending=bytearray()
        executor.sdk_serials.append(self)

    @property
    def in_waiting(self):return len(self.pending)
    def flush(self):
        callback=getattr(self.io,'sdk_flush_hook',None)
        if callback is not None:callback()
    def reset_input_buffer(self):self.pending.clear()
    def close(self):
        self.io.sdk_close_calls+=1
        if self.io.sdk_close_fails:raise OSError('串口关闭失败')
        self.is_open=False
    def write(self,data):
        packet=list(data)
        assert packet[:2]==[255,255] and sum(packet[2:])&255==255
        self.io.t+=1_000_000
        self.io.sdk_packets.append(packet)
        sync=packet[2]==254
        instruction=packet[4]
        address=packet[5]
        if sync:
            assert packet[4:8]==[131,46,2,self.io.sdk_target]
            register='Goal_Velocity';value=packet[8]|(packet[9]<<8)
            self.io.goal=value
            if value:self.io.sdk_nonzero_count+=1
            else:self.io.zero_count+=1
            if self.io.goal_write_enables:self.io.torque=1
            result=None
        elif instruction==3:
            assert packet[2]==self.io.sdk_target and address==40
            register='Torque_Enable';value=packet[6]
            self.io.torque=value
            if value and self.io.signal_on_enable:self.io.interrupted=signal.SIGTERM
            result=[]
        else:
            assert instruction==2 and packet[2]==self.io.sdk_target
            register=next(name for name,meta in m.RIGHT_SDK_REGS.items() if meta==(address,packet[6]))
            self.io.sdk_read_counts[register]=self.io.sdk_read_counts.get(register,0)+1
            if register=='Present_Velocity' and self.io.torque and not self.io.goal and self.io.zero_count>=2 and self.io.sdk_zero_coast:
                value=self.io.sdk_zero_coast.pop(0)
            elif register=='Present_Velocity' and not self.io.torque and self.io.zero_count>=2 and self.io.sdk_coast:
                value=self.io.sdk_coast.pop(0)
            elif self.io.sdk_after_off and register in self.io.sdk_after_off and self.io.torque==0 and self.io.zero_count>=2:
                value=self.io.sdk_after_off[register]
            else:value=self.io.value(register)
            word=abs(value)|(0x8000 if value<0 else 0)
            result=list(word.to_bytes(packet[6],'little'))
        key=(instruction,register,value)
        self.io.sdk_actions.append(key)
        fault=self.io.sdk_fault(key)
        if fault=='signal':self.io.interrupted=signal.SIGTERM
        if fault=='late':self.io.t+=22_000_000_000
        if fault=='io':raise OSError('已收发但状态未知')
        if fault=='short':return len(packet)-1
        if fault=='ack_long' and instruction==3:result=[0]
        if result is not None:
            error=4 if fault=='status' else 0
            response=[255,255,getattr(self.io,'sdk_reply_id',self.io.sdk_target),len(result)+2,error,*result]
            response.append((~sum(response[2:]))&255)
            if fault=='checksum':response[-1]^=1
            if fault=='length':response=response[:-1]
            self.pending.extend(response)
        return len(packet)
    def read(self,length):
        self.io.t+=1_000_000
        raw=self.pending[:length]
        del self.pending[:length]
        return bytes(raw)


def fake_sdk(executor):
    class Port:
        def __init__(self,device):
            assert device==m.DEVICES['right']
            self.is_open=False;self.is_using=False;self.ser=None;self.tx_time_per_byte=0.01
        def setBaudRate(self,baud):
            assert baud==1_000_000
            executor.sdk_open_calls+=1
            self.ser=SdkMotorSerial(executor);self.is_open=True
            return True
        def closePort(self):
            if self.ser:self.ser.close()
            self.is_open=False
        def clearPort(self):self.ser.flush()
        def writePort(self,data):return self.ser.write(data)
        def readPort(self,length):return self.ser.read(length)
        def getCurrentTime(self):return executor.now()/1_000_000
    class Packet:
        def exchange(self,port,motor,instruction,parameters,width=0):
            data=[255,255,motor,len(parameters)+2,instruction,*parameters]
            data.append((~sum(data[2:]))&255)
            port.clearPort()
            if port.writePort(data)!=len(data):return [],-1001,0
            if motor==254:return [],0,0
            raw=list(port.readPort(width+6))
            if len(raw)!=width+6 or sum(raw[2:])&255!=255:return [],-3002,0
            return raw[5:-1],0,raw[4]
        def readTxRx(self,port,motor,address,width):return self.exchange(port,motor,2,[address,width],width)
        def write1ByteTxRx(self,port,motor,address,value):
            _,comm,error=self.exchange(port,motor,3,[address,value])
            return comm,error
        def syncWriteTxOnly(self,port,address,width,parameters,length):
            _,comm,_=self.exchange(port,254,131,[address,width,*parameters[:length]])
            return comm
    class Writer:
        def __init__(self,port,packet,address,width):
            assert (address,width)==(46,2)
            self.port=port;self.packet=packet;self.values={}
        def clearParam(self):self.values.clear()
        def addParam(self,motor,data):
            if motor in self.values:return False
            self.values[motor]=list(data);return True
        def txPacket(self):
            assert list(self.values)==[executor.sdk_target]
            return self.packet.syncWriteTxOnly(self.port,46,2,[executor.sdk_target,*self.values[executor.sdk_target]],3)
    return SimpleNamespace(PortHandler=Port,PacketHandler=lambda protocol:Packet(),GroupSyncWrite=Writer,COMM_SUCCESS=0)


class RightPulseTests(unittest.TestCase):
    def setUp(self):
        self.verify=patch.object(m,'verify_ref')
        self.verify.start();self.addCleanup(self.verify.stop)

    def bindings(self):
        references=[dict(r) for r in (m.RIGHT_PULSE_WRITE,m.RIGHT_PULSE_RAW,m.RIGHT_PULSE_SYNC,
                                      m.RIGHT_PULSE_HELPER,m.RIGHT_PULSE_TRACE,*m.RIGHT_PULSE_SDK['refs'])]
        return {'schema':'MAINTENANCE_BINDINGS_V1','tool':dict(m.RIGHT_PULSE_WRITE),
                'usb':{side:{'device':m.DEVICES[side],'serial':m.SERIALS[side]} for side in m.SERIALS},
                'host':{'boot_id':'pulse-fixture'},'refs':references,
                'binary_paths':[r['path'] for r in (m.RIGHT_PULSE_WRITE,m.RIGHT_PULSE_RAW,m.RIGHT_PULSE_SYNC,m.RIGHT_PULSE_TRACE)]}

    def plan(self,wheel='right',*,zero_stop_check=False):
        root='/home/loosqk/robot-checks/pulse-fixture'
        return m.make_plan(self.bindings(),root+'/run','pulse-fixture',root,right_wheel_pulse=wheel=='right',
                           wheel_pulse_target=wheel if wheel!='right' else None,zero_stop_check=zero_stop_check)

    def engine(self,wheel='right',*,zero_stop_check=False):
        executor=RightPulseIO()
        directory=tempfile.TemporaryDirectory();self.addCleanup(directory.cleanup)
        executor.output=Path(directory.name)
        executor.sdk_packets=[];executor.sdk_actions=[];executor.sdk_serials=[];executor.sdk_read_counts={}
        executor.sdk_fault=lambda key:None;executor.sdk_coast=[];executor.sdk_zero_coast=[];executor.sdk_after_off={}
        executor.sdk_nonzero_count=0;executor.sdk_close_calls=0;executor.sdk_open_calls=0
        executor.sdk_close_fails=False
        executor.sdk_target=m.WHEEL_PULSE_TARGETS[wheel][1]
        helper=SimpleNamespace(install_stop_wire_capture=lambda module:None)
        engine=m.ProbeEngine(executor,self.plan(wheel,zero_stop_check=zero_stop_check))
        m.install_right_pulse_guard(engine,helper)
        return executor,engine

    def perform(self,executor,engine,success=True,sdk=None):
        sdk=sdk or fake_sdk(executor)
        with patch.object(m,'load_right_pulse_sdk',return_value=sdk),contextlib.redirect_stdout(io.StringIO()):
            if success:m.run_right_wheel_pulse(engine)
            else:
                with self.assertRaises(ValueError):m.run_right_wheel_pulse(engine)
        return engine.right_pulse

    def test_toml_permutations_alone_move_named_single_wheel_target_and_scope(self):
        with tempfile.TemporaryDirectory() as directory:
            self.addCleanup(m.configure_bus_mapping,BUS_FIXTURE_PATH)
            path=Path(directory)/'bus2.toml'
            for ids in itertools.permutations((7,8,9)):
                m.configure_bus_mapping(write_bus_fixture(path,ids))
                for role,motor in zip(m.WHEEL_ROLES,ids):
                    with self.subTest(ids=ids,role=role):
                        executor,engine=self.engine(role)
                        result=self.perform(executor,engine)
                        self.assertEqual((result['motor'],result['name']),(motor,f'base_{role}_wheel'))
                        self.assertEqual(engine.plan['right_wheel_pulse']['target'],['right',motor,f'base_{role}_wheel'])
                        self.assertIn(f'仅{m.WHEEL_LABELS[role]}{motor}',engine.plan['scope'])
                        nonzero=[packet for packet in executor.sdk_packets
                                 if len(packet)==11 and packet[4:7]==[131,46,2] and packet[8:10]==[244,1]]
                        self.assertTrue(nonzero)
                        self.assertTrue(all(packet[7]==motor for packet in nonzero))
                        self.assertEqual(m.WHEEL_STOP_TARGETS[motor],role)
                        self.assertEqual(engine.plan[f'{role}{motor}_stop_contract']['target'],
                                         ['right',motor,f'base_{role}_wheel'])

    def stability_engine(self,wheel,phase,trigger=50,*,positions=None,velocities=None):
        executor,engine=self.engine(wheel)
        original=executor.value
        positions=[1000,1001,1000,1001,1000] if positions is None else positions
        velocities=[0,50,-50,0,50] if velocities is None else velocities
        def value(register):
            window=next((engine.right_pulse.get(p+'_stability') for p in
                (('initial','post_disable') if phase=='both' else (phase,))
                if engine.right_pulse.get(p+'_stability',{}).get('status')=='FAIL'),None)
            if window and window['status']=='FAIL' and register in ('Present_Position','Present_Velocity'):
                index=sum(s['register']==register for s in window['samples'])-1
                return (positions if register=='Present_Position' else velocities)[index]
            if register=='Present_Velocity' and ((phase in ('initial','both') and executor.zero_count==0) or (
                    phase in ('post_disable','both') and executor.torque==0 and executor.zero_count>=2)):
                return trigger
            return original(register)
        executor.value=value
        return executor,engine

    def test_same_connection_twenty_seconds_repeats_only_right7_fixed_goal(self):
        executor,engine=self.engine()
        row=self.perform(executor,engine)
        self.assertEqual(engine.plan['right_wheel_pulse']['target'],['right',7,'base_right_wheel'])
        self.assertNotIn('zero_stop_check',engine.plan['right_wheel_pulse'])
        self.assertNotIn('zero_stop_check',row)
        self.assertEqual(row['status'],'PASS');self.assertFalse(engine.manual_cut)
        self.assertEqual((executor.sdk_open_calls,executor.sdk_close_calls),(1,1))
        self.assertTrue(row['connection']['closed']);self.assertFalse(engine.right_sdk_active)
        self.assertEqual(executor.calls,[]);self.assertIsNone(executor.child)
        self.assertEqual(len(row['samples']),79)
        self.assertGreater(executor.sdk_nonzero_count,300)
        self.assertEqual(row['observation_deadline_ns']-row['first_velocity_attempt_started_ns'],20_000_000_000)
        self.assertGreaterEqual(row['observation_finished_ns'],row['observation_deadline_ns'])
        self.assertLessEqual(row['enabled_attempt_window_ns'],21_000_000_000)
        self.assertEqual([w['value'] for w in row['writes'][:3]],[0,1,500])
        self.assertEqual([(w['register'],w['value']) for w in row['writes'][-2:]],
                         [('Goal_Velocity',0),('Torque_Enable',0)])
        self.assertTrue(all(w['motor']==7 for w in row['writes']))
        self.assertTrue(row['stopped_confirmed']);self.assertEqual((executor.goal,executor.torque),(0,0))

    def test_zero_stop_option_is_pulse_only_and_binds_target_scope_plan_and_go(self):
        root='/home/loosqk/robot-checks/pulse-fixture'
        for options in ({'zero_stop_check':True},{'zero_stop_check':True,'right_wheel_feedback_only':True},
                        {'zero_stop_check':1,'right_wheel_pulse':True}):
            with self.assertRaises(ValueError):m.make_plan(self.bindings(),root+'/run','pulse-fixture',root,**options)
        with patch.object(m.subprocess,'Popen',side_effect=AssertionError('前置不得child')) as child:
            for args in (['--zero-stop-check'],['--right-wheel-feedback-only','--zero-stop-check'],
                         ['--right-wheel-pulse','--zero-stop-check','--position-target','head-1']):
                with self.assertRaises(ValueError):run_main(args)
            for args in (['--right-wheel-pulse','--zero-stop-check'],['--wheel-pulse','front','--zero-stop-check'],
                         ['--wheel-pulse','left','--zero-stop-check']):
                with contextlib.redirect_stdout(io.StringIO()):self.assertEqual(run_main(args),0)
            child.assert_not_called()
        for wheel in m.WHEEL_PULSE_TARGETS:
            plan=self.plan(wheel,zero_stop_check=True)
            original=self.plan(wheel)
            self.assertNotIn('zero_stop_check',original['right_wheel_pulse'])
            self.assertNotEqual(m.digest(plan),m.digest(original))
            self.assertEqual(m.position_targets_from_plan(plan),[])
            facts={**{key:True for key in m.FACTS},'confirmed_at_ns':100}
            go={'schema':'MAINTENANCE_GO_V2','issued_by':'root','artifact_root':plan['artifact_root'],
                'plan_sha256':m.digest(plan),'issued_at_ns':100,'expires_at_ns':1000,'host':plan['bindings']['host'],
                'source':'零速检查夹具','physical':{side:dict(facts) for side in m.SERIALS}}
            go['physical']['right'].update(all_three_wheels_off_ground=True,base_stably_supported=True,
                continuous_wheel_observation=True,base_confirmation_kind='CURRENT_SETUP_READY',
                base_confirmation_scope=plan['scope'],base_confirmation_answer='夹具就绪')
            m.validate_go(go,plan,200)
            self.assertIn('三轮各按同一获准稳定性判据',plan['scope'])
            old=copy.deepcopy(plan)
            old['scope']=old['scope'].replace('三轮各按同一获准稳定性判据','仅右7和左9各按获准稳定性判据')
            old['right_wheel_pulse']['scope']=old['scope']
            with self.assertRaises(ValueError):m.position_targets_from_plan(old)
            old_go=copy.deepcopy(go);old_go['plan_sha256']=m.digest(old)
            old_go['physical']['right']['base_confirmation_scope']=old['scope']
            with self.assertRaises(ValueError):m.validate_go(old_go,plan,200)
            for value in (original['scope'],m.pulse_scope('front' if wheel!='front' else 'right',zero_stop_check=True)):
                bad=copy.deepcopy(go);bad['physical']['right']['base_confirmation_scope']=value
                with self.assertRaises(ValueError):m.validate_go(bad,plan,200)
            bad=copy.deepcopy(go);bad['plan_sha256']=m.digest(original)
            with self.assertRaises(ValueError):m.validate_go(bad,plan,200)
            for field,value in (('budget_ns',501_000_000),('cleanup_reserve_ns',0),
                                ('sample_offsets_ns',[50_000_000,250_000_000]),('torque_enable',0),
                                ('final_present_velocity_raw_unsigned',[0,50,51]),('offset_origin','ZERO_SYNC_STARTED')):
                bad=copy.deepcopy(plan);bad['right_wheel_pulse']['zero_stop_check'][field]=value
                with self.assertRaises(ValueError):m.validate_right_pulse_plan(bad)
            bad=copy.deepcopy(plan);bad['right_wheel_pulse']['zero_stop_check']=False
            with self.assertRaises(ValueError):m.validate_right_pulse_plan(bad)

    def test_zero_stop_keeps_enable_for_all_three_fixed_samples_before_final_disable(self):
        for wheel in m.WHEEL_PULSE_TARGETS:
            for speeds in ([250,50,0],[0,0,0]):
                executor,engine=self.engine(wheel,zero_stop_check=True);executor.sdk_zero_coast=list(speeds)
                row=self.perform(executor,engine)
                zero=row['zero_stop_check']
                self.assertEqual(zero['status'],'PASS');self.assertEqual(len(zero['samples']),3)
                self.assertEqual([s['velocity_raw'] for s in zero['samples']],speeds)
                self.assertEqual(zero['deadline_ns']-zero['started_ns'],500_000_000)
                self.assertLessEqual(zero['finished_ns'],zero['deadline_ns'])
                for index,(sample,offset) in enumerate(zip(zero['samples'],(50_000_000,250_000_000,450_000_000))):
                    self.assertEqual(sample['offset_ns'],offset)
                    self.assertLessEqual(abs(sample['raw'][0]['started_ns']-zero['sync_finished_ns']-offset),1)
                    self.assertEqual([e['register'] for e in sample['raw']],
                                     ['Torque_Enable','Goal_Velocity','Present_Velocity'])
                    self.assertEqual([e['raw_unsigned'] for e in sample['raw'][:2]],[1,0])
                    expected_deadline=zero['deadline_ns'] if index==2 else min(
                        zero['deadline_ns'],zero['sync_finished_ns']+(250_000_000,450_000_000)[index])
                    self.assertEqual(sample['deadline_ns'],expected_deadline)
                    self.assertTrue(all(not e['cleanup'] and e['deadline_ns']==expected_deadline for e in sample['raw']))
                selected=[e for e in row['writes'] if e['register']=='Goal_Velocity' and e['value']==0]
                self.assertEqual([e['cleanup'] for e in selected],[False,False,True])
                self.assertEqual(selected[1]['started_ns'],zero['started_ns'])
                self.assertEqual(selected[1]['finished_ns'],zero['sync_finished_ns'])
                self.assertEqual([(e['register'],e['value']) for e in row['writes'][-2:]],
                                 [('Goal_Velocity',0),('Torque_Enable',0)])
                self.assertEqual((executor.sdk_open_calls,executor.sdk_close_calls),(1,1))
                self.assertEqual(executor.sdk_nonzero_count,400)
                self.assertLessEqual(row['enabled_attempt_window_ns'],21_000_000_000)
                self.assertTrue(row['stopped_confirmed']);self.assertFalse(engine.manual_cut)
                self.assertFalse(row['firmware_contract_verified'])

    def test_three_wheel_zero_fifties_preserve_words_and_reach_independent_strict_cleanup(self):
        for wheel in m.WHEEL_PULSE_TARGETS:
            for speeds in ([-50,0,0],[0,-50,0],[-50,-50,0],[250,-50,-50],[300,0,50],[51,51,50]):
                executor,engine=self.engine(wheel,zero_stop_check=True);executor.sdk_zero_coast=list(speeds)
                row=self.perform(executor,engine)
                zero=row['zero_stop_check']
                self.assertEqual(zero['status'],'PASS');self.assertEqual(len(zero['samples']),3)
                expected=[0x8032 if value==-50 else value for value in speeds]
                self.assertEqual([sample['velocity_raw'] for sample in zero['samples']],expected)
                wire=[json.loads(line) for line in Path(row['wire_path']).read_text().splitlines()]
                for sample,word in zip(zero['samples'],expected):
                    entry=sample['raw'][-1]
                    self.assertEqual(entry['raw_unsigned'],word);self.assertEqual(entry['status'],'READ_VALID')
                    self.assertEqual(entry['data'],[word&255,word>>8])
                    self.assertEqual(entry['rx_bytes'][5:7],[word&255,word>>8])
                    self.assertTrue(any(event['operation']==entry['sequence'] and event['direction']=='rx'
                                        and event['bytes']==entry['rx_bytes'] for event in wire))
                    self.assertEqual([entry['raw_unsigned'] for entry in sample['raw'][:2]],[1,0])
                self.assertEqual(zero['samples'][-1]['velocity_raw'],expected[-1])
                self.assertTrue(row['stopped_confirmed']);self.assertFalse(engine.manual_cut)
                self.assertEqual(row['settle_samples'][-1]['velocity_raw'],0)
                self.assertEqual([(e['register'],e['value']) for e in row['writes'][-2:]],
                                 [('Goal_Velocity',0),('Torque_Enable',0)])
                self.assertTrue(row['connection']['closed']);self.assertFalse(engine.right_sdk_active)
                self.assertEqual((executor.goal,executor.torque),(0,0));self.assertFalse(row['uart_blocked'])
                self.assertEqual(executor.sdk_nonzero_count,400)
                self.assertEqual(row['observation_deadline_ns']-row['first_velocity_attempt_started_ns'],20_000_000_000)
                self.assertLessEqual(zero['finished_ns']-zero['started_ns'],500_000_000)
                self.assertLessEqual(row['enabled_attempt_window_ns'],21_000_000_000)

    def test_three_wheel_zero_rejects_negative_zero_outside_fifties_and_final_nonzero_words(self):
        cases=[(index,word) for index in (0,1,2) for word in (0x8000,0x8001,0x8031,0x8033,0x8064,0xffff)]
        cases += [(2,word) for word in (1,49,51,250,1000)]
        cases += [(index,1001) for index in (0,1)]
        for wheel in m.WHEEL_PULSE_TARGETS:
            for index,word in cases:
                executor,engine=self.engine(wheel,zero_stop_check=True)
                speeds=[0,0,0];speeds[index]=word;executor.sdk_zero_coast=speeds
                row=self.perform(executor,engine,False)
                zero=row['zero_stop_check']
                self.assertEqual(zero['status'],'FAIL');self.assertEqual(len(zero['samples']),index+1)
                entry=zero['samples'][-1]['raw'][-1]
                self.assertEqual(entry['raw_unsigned'],word);self.assertEqual(entry['status'],'READ_VALID')
                self.assertTrue(row['uart_blocked']);self.assertTrue(engine.manual_cut)
                self.assertEqual([(entry['register'],entry['value']) for entry in row['writes'][-2:]],
                                 [('Goal_Velocity',0),('Torque_Enable',0)])
                self.assertTrue(row['connection']['closed']);self.assertEqual((executor.torque,executor.goal),(0,0))

    def test_front_zero_exception_does_not_relax_initial_motion_goal_post_disable_or_whole_stop(self):
        for phase in ('initial','motion','goal','post_disable','whole_stop'):
            executor,engine=self.engine('front',zero_stop_check=True);executor.sdk_zero_coast=[-50,-50,0]
            if phase=='initial':executor.stop_velocity=-51
            elif phase=='motion':executor.velocity=-50
            elif phase=='goal':
                original=executor.value
                executor.value=lambda register:0x8032 if register=='Goal_Velocity' and executor.torque and executor.zero_count==2 else original(register)
            elif phase=='post_disable':executor.sdk_after_off={'Present_Velocity':-51}
            row=self.perform(executor,engine,phase=='whole_stop')
            if phase=='initial':
                self.assertEqual(row['writes'],[]);self.assertEqual(executor.sdk_nonzero_count,0)
                self.assertEqual(row['before_raw'][-1]['raw_unsigned'],0x8033)
            elif phase=='motion':
                self.assertNotIn('zero_stop_check',row)
                self.assertEqual(row['samples'][0]['raw'][-1]['raw_unsigned'],0x8032)
            elif phase=='goal':
                self.assertEqual(row['zero_stop_check']['samples'][0]['raw'][-1]['register'],'Goal_Velocity')
                self.assertEqual(row['zero_stop_check']['status'],'FAIL')
            elif phase=='post_disable':
                self.assertEqual(row['zero_stop_check']['status'],'PASS');self.assertFalse(row['stopped_confirmed'])
                self.assertEqual(row['cleanup_raw'][-1]['raw_unsigned'],0x8033)
            else:
                executor.stop_velocity=-50
                with contextlib.redirect_stdout(io.StringIO()),self.assertRaises(ValueError):
                    executor.poke('right',8,'read','Present_Velocity',cleanup=True)
                values=idle_rows();values['right:8']['Present_Velocity']=-50
                self.assertEqual(m.classify_stop_snapshot(values)['status'],'FAIL')
            self.assertTrue(engine.manual_cut);self.assertTrue(row['uart_blocked'])
            self.assertTrue(row['connection']['closed'])

    def test_three_wheel_zero_contract_and_scope_reject_tampering_and_old_go(self):
        expected=self.plan(zero_stop_check=True)['right_wheel_pulse']['zero_stop_check']
        for wheel in m.WHEEL_PULSE_TARGETS:
            plan=self.plan(wheel,zero_stop_check=True);check=plan['right_wheel_pulse']['zero_stop_check']
            self.assertEqual(check,expected)
            self.assertEqual(check['intermediate_sample_indices'],[0,1])
            self.assertEqual(check['intermediate_extra_raw_unsigned'],[0x8032])
            self.assertEqual(check['final_present_velocity_raw_unsigned'],[0,50,0x8032])
            self.assertIn('末组原字限0/50/0x8032并保留原值',plan['scope'])
            self.assertNotIn('zero_stop_check',self.plan(wheel)['right_wheel_pulse'])
            for field,value in (('intermediate_sample_indices',[0,1,2]),('intermediate_sample_indices',[0]),
                                ('intermediate_extra_raw_unsigned',[0x8032,0x8000]),('intermediate_extra_raw_unsigned',[0]),
                                ('final_present_velocity_raw_unsigned',[0,50,0x8033]),
                                ('final_present_velocity_raw_unsigned',[0]),('final_present_velocity',0)):
                bad=copy.deepcopy(plan);bad['right_wheel_pulse']['zero_stop_check'][field]=value
                with self.assertRaises(ValueError):m.validate_right_pulse_plan(bad)
            old=copy.deepcopy(plan);old_check=old['right_wheel_pulse']['zero_stop_check']
            del old_check['final_present_velocity_raw_unsigned'];old_check['final_present_velocity']=0
            if wheel!='front':
                for key in ('intermediate_sample_indices','intermediate_extra_raw_unsigned'):del old_check[key]
            old['scope']=m.pulse_scope(wheel)+'；20秒后额外SYNC零速并保持使能，500毫秒内三组TE1/GV0/PV读取，末组PV严格0，再按原路径断扭'
            if wheel=='front':old['scope']+='；仅前轮前两组另允许原字0x8032并保留原值，末组及初始/断扭后停止门不变'
            old['right_wheel_pulse']['scope']=old['scope']
            with self.assertRaises(ValueError):m.validate_right_pulse_plan(old)
            facts={**{key:True for key in m.FACTS},'confirmed_at_ns':100}
            go={'schema':'MAINTENANCE_GO_V2','issued_by':'root','artifact_root':plan['artifact_root'],
                'plan_sha256':m.digest(old),'issued_at_ns':100,'expires_at_ns':1000,'host':plan['bindings']['host'],
                'source':'仅软件测试，无硬件授权','physical':{side:dict(facts) for side in m.SERIALS}}
            go['physical']['right'].update(all_three_wheels_off_ground=True,base_stably_supported=True,
                continuous_wheel_observation=True,base_confirmation_kind='CURRENT_SETUP_READY',
                base_confirmation_scope=old['scope'],base_confirmation_answer='夹具就绪')
            with self.assertRaises(ValueError):m.validate_go(go,plan,200)
            with self.assertRaises(ValueError):m.validate_go(go,old,200)
            go['plan_sha256']=m.digest(plan)
            with self.assertRaises(ValueError):m.validate_go(go,plan,200)
            go['physical']['right']['base_confirmation_scope']=plan['scope']
            m.validate_go(go,plan,200)

    def test_front_intermediate_minus_fifty_does_not_bypass_time_go_signal_or_bad_frame(self):
        for fault in ('deadline','go','signal','checksum'):
            executor,engine=self.engine('front',zero_stop_check=True);executor.sdk_zero_coast=[-50,-50,0]
            expired=False
            def check_go():m.require(not expired,'中途GO到期')
            engine.right_pulse_go_check=check_go
            def effect(key):
                nonlocal expired
                zero=engine.right_pulse.get('zero_stop_check')
                if zero and len(zero['samples'])==2 and key[:2]==(2,'Present_Velocity'):
                    if fault=='deadline':executor.t=zero['deadline_ns']+1
                    elif fault=='go':expired=True
                    else:return fault
                return None
            executor.sdk_fault=effect
            row=self.perform(executor,engine,False)
            self.assertEqual(row['zero_stop_check']['samples'][0]['velocity_raw'],0x8032)
            self.assertEqual(len(row['zero_stop_check']['samples']),2)
            self.assertEqual(row['zero_stop_check']['status'],'FAIL')
            self.assertTrue(engine.manual_cut);self.assertTrue(row['uart_blocked'])
            self.assertEqual([(entry['register'],entry['value']) for entry in row['writes'][-2:]],
                             [('Goal_Velocity',0),('Torque_Enable',0)])
            self.assertEqual((executor.torque,executor.goal),(0,0));self.assertTrue(row['connection']['closed'])

    def test_zero_stop_final_outside_fifties_and_intermediate_raw_boundaries_fail(self):
        for wheel in ('left','front','right'):
            for speeds in ([0,0,51],[0,0,1000],[250,50,-51],[250,50,0x8000],[1001,0,0]):
                executor,engine=self.engine(wheel,zero_stop_check=True);executor.sdk_zero_coast=list(speeds)
                row=self.perform(executor,engine,False)
                self.assertEqual(row['zero_stop_check']['status'],'FAIL')
                last=row['zero_stop_check']['samples'][-1]['raw'][-1]
                self.assertEqual(last['register'],'Present_Velocity');self.assertEqual(last['status'],'READ_VALID')
                self.assertEqual(last['raw_unsigned'],abs(speeds[0 if speeds[0]==1001 else 2])|(
                    0x8000 if speeds[0 if speeds[0]==1001 else 2]<0 else 0))
                self.assertTrue(engine.manual_cut);self.assertTrue(row['uart_blocked'])
                self.assertTrue(row['connection']['closed']);self.assertEqual((executor.torque,executor.goal),(0,0))
                self.assertEqual(row['cleanup_raw'],[])

    def test_zero_stop_rejects_wrong_enable_or_goal_in_each_group(self):
        for index in (1,2,3):
            for register,wrong in (('Torque_Enable',0),('Torque_Enable',2),('Goal_Velocity',500)):
                executor,engine=self.engine(zero_stop_check=True)
                original=executor.value
                def value(name,index=index,register=register,wrong=wrong):
                    if executor.torque and not executor.goal and executor.zero_count==2 and name==register and (
                            len(engine.right_pulse['zero_stop_check']['samples'])==index):return wrong
                    return original(name)
                executor.value=value
                row=self.perform(executor,engine,False)
                zero=row['zero_stop_check']
                self.assertEqual(len(zero['samples']),index);self.assertEqual(zero['status'],'FAIL')
                self.assertEqual(zero['samples'][-1]['raw'][-1]['raw_unsigned'],wrong)
                self.assertEqual(zero['samples'][-1]['raw'][-1]['register'],register)
                self.assertEqual((executor.torque,executor.goal),(0,0));self.assertTrue(engine.manual_cut)
                self.assertEqual([(e['register'],e['value']) for e in row['writes'][-2:]],
                                 [('Goal_Velocity',0),('Torque_Enable',0)])

    def test_zero_stop_absolute_deadlines_cover_flush_return_and_remaining_enable_budget(self):
        for fault in ('zero_flush','read_flush','read_return','remaining','over21'):
            executor,engine=self.engine(zero_stop_check=True)
            def effect(key):
                if fault=='remaining' and key==(3,'Torque_Enable',1):executor.t+=450_000_000
                zero=engine.right_pulse.get('zero_stop_check')
                if zero and key[:2]==(2,'Torque_Enable'):
                    if fault=='read_return':executor.t=zero['deadline_ns']+1
                    elif fault=='over21':executor.t=engine.right_pulse['enable_attempt_started_ns']+21_000_000_001
                return None
            executor.sdk_fault=effect
            def flush():
                zero=engine.right_pulse.get('zero_stop_check')
                if not zero:return
                entry=engine.right_pulse['sdk_operations'][-1]
                if not entry['cleanup'] and ((fault=='zero_flush' and entry['kind']=='sync') or (
                        fault=='read_flush' and entry['kind']=='read')):executor.t=zero['deadline_ns']
            executor.sdk_flush_hook=flush
            row=self.perform(executor,engine,False)
            zero=row['zero_stop_check']
            self.assertEqual(zero['deadline_ns'],min(zero['started_ns']+500_000_000,
                row['enable_attempt_started_ns']+20_900_000_000))
            normal=[e for e in row['sdk_operations'] if not e['cleanup'] and e['started_ns']>=zero['started_ns']]
            if fault in ('zero_flush','read_flush'):
                self.assertEqual(normal[-1]['tx_attempts'],0)
                self.assertEqual(executor.zero_count,2 if fault=='zero_flush' else 3)
                if fault=='read_flush':
                    self.assertEqual([p[4] for p in executor.sdk_packets[-3:]],[131,131,3])
            if fault=='read_return':
                self.assertEqual(len(zero['samples']),1);self.assertEqual(len(zero['samples'][0]['raw']),1)
                self.assertEqual(zero['samples'][0]['raw'][0]['status'],'READ_VALID')
            if fault=='remaining':
                self.assertLess(zero['deadline_ns']-zero['started_ns'],500_000_000)
                self.assertEqual(len(zero['samples']),2)
            if fault=='over21':
                self.assertGreater(row['enabled_attempt_window_ns'],21_000_000_000)
                self.assertTrue(any('21秒' in error for error in row['cleanup_errors']))
            else:self.assertLessEqual(row['enabled_attempt_window_ns'],21_000_000_000)
            self.assertEqual(zero['status'],'FAIL');self.assertEqual((executor.torque,executor.goal),(0,0))
            self.assertTrue(row['connection']['closed']);self.assertTrue(engine.manual_cut)

    def test_zero_stop_first_group_cannot_cross_next_slot_and_compress_later_groups(self):
        executor,engine=self.engine(zero_stop_check=True)
        def effect(key):
            zero=engine.right_pulse.get('zero_stop_check')
            if zero and key[:2]==(2,'Torque_Enable'):
                executor.t=zero['sync_finished_ns']+250_000_001
            return None
        executor.sdk_fault=effect
        row=self.perform(executor,engine,False)
        zero=row['zero_stop_check']
        self.assertEqual(zero['status'],'FAIL');self.assertEqual(len(zero['samples']),1)
        self.assertEqual(len(zero['samples'][0]['raw']),1)
        entry=zero['samples'][0]['raw'][0]
        self.assertEqual((entry['register'],entry['status']),('Torque_Enable','READ_VALID'))
        self.assertGreater(entry['finished_ns'],entry['deadline_ns'])
        self.assertLess(entry['finished_ns'],zero['deadline_ns'])
        self.assertEqual([p[4] for p in executor.sdk_packets[-3:]],[2,131,3])
        self.assertEqual(executor.sdk_packets[-3][5:7],[40,1])
        self.assertEqual((executor.torque,executor.goal),(0,0));self.assertTrue(row['connection']['closed'])
        self.assertTrue(engine.manual_cut);self.assertTrue(row['uart_blocked'])

    def test_zero_stop_go_expiry_signal_and_bad_read_never_continue_normal_uart(self):
        for fault in ('go','signal','io','checksum','status','length','zero_short','off_ack'):
            executor,engine=self.engine(zero_stop_check=True)
            if fault=='go':
                engine.right_pulse_go_check=lambda:m.require(executor.now()<engine.right_pulse.get(
                    'zero_stop_check',{}).get('started_ns',10**12)+75_000_000,'零速阶段GO到期')
                def flush():
                    zero=engine.right_pulse.get('zero_stop_check')
                    entry=engine.right_pulse['sdk_operations'][-1]
                    if zero and not entry['cleanup'] and entry['kind']=='read':executor.t=zero['started_ns']+75_000_000
                executor.sdk_flush_hook=flush
            def effect(key):
                if 'zero_stop_check' not in engine.right_pulse:return None
                if fault=='zero_short' and key==(131,'Goal_Velocity',0) and executor.zero_count==2:return 'short'
                if fault=='off_ack' and key==(3,'Torque_Enable',0):return 'ack_long'
                if key[:2]==(2,'Torque_Enable') and executor.torque:return fault if fault not in ('go','zero_short','off_ack') else None
                return None
            executor.sdk_fault=effect
            row=self.perform(executor,engine,False)
            self.assertTrue(row['normal_failed'] if fault!='off_ack' else row['cleanup_errors'])
            self.assertEqual(sum(e['kind']=='write' and e['value']==0 for e in row['writes']),1)
            self.assertEqual([(e['register'],e['value']) for e in row['writes'][-2:]],
                             [('Goal_Velocity',0),('Torque_Enable',0)])
            self.assertEqual((executor.torque,executor.goal),(0,0));self.assertTrue(row['connection']['closed'])
            self.assertTrue(engine.manual_cut);self.assertTrue(row['uart_blocked'])
            self.assertEqual(row['cleanup_raw'],[])
            count=len(executor.sdk_packets)
            with self.assertRaises(ValueError):executor.poke('right',9,'read','Present_Velocity',cleanup=True)
            self.assertEqual(len(executor.sdk_packets),count)
            if fault=='go':
                entry=row['zero_stop_check']['samples'][0]['raw'][0]
                self.assertEqual(entry['tx_attempts'],0)

    def test_zero_stop_wire_log_failure_still_independently_clears_goal_and_disables(self):
        executor,engine=self.engine(zero_stop_check=True);original=Path.open
        def opening(path,*args,**kwargs):
            stream=original(path,*args,**kwargs)
            if path.name!='right7-sdk-wire.jsonl' or not args or args[0]!='x':return stream
            def write(value):
                if 'zero_stop_check' in engine.right_pulse:raise OSError('零速阶段原帧日志失败')
                return stream.write(value)
            return SimpleNamespace(write=write,flush=stream.flush,close=stream.close)
        with patch.object(Path,'open',opening):row=self.perform(executor,engine,False)
        self.assertEqual(row['zero_stop_check']['status'],'FAIL');self.assertTrue(executor.evidence_errors)
        self.assertEqual(executor.zero_count,3);self.assertEqual((executor.torque,executor.goal),(0,0))
        self.assertEqual([(e['register'],e['value']) for e in row['writes'][-2:]],
                         [('Goal_Velocity',0),('Torque_Enable',0)])
        self.assertTrue(row['connection']['closed']);self.assertTrue(engine.manual_cut)

    def test_front_and_left_select_only_one_fixed_motor_in_all_sdk_packets(self):
        for wheel,motor in (('front',8),('left',9)):
            executor,engine=self.engine(wheel)
            row=self.perform(executor,engine)
            self.assertEqual((row['side'],row['motor'],row['name']),m.WHEEL_PULSE_TARGETS[wheel])
            self.assertTrue(all(op['motor']==motor for op in row['sdk_operations']))
            self.assertTrue(all(packet[7]==motor if packet[2]==254 else packet[2]==motor
                                for packet in executor.sdk_packets))
            self.assertEqual((executor.sdk_open_calls,executor.sdk_close_calls),(1,1))
            self.assertEqual(len(row['samples']),79)
            self.assertGreater(executor.sdk_nonzero_count,300)
            self.assertTrue(Path(row['wire_path']).name.startswith(f'right{motor}-'))
            self.assertTrue(row['stopped_confirmed']);self.assertEqual((executor.goal,executor.torque),(0,0))
            before=idle_rows();after=copy.deepcopy(before)
            after[f'right:{motor}']['Present_Position']+=200
            self.assertEqual(m.classify_position_snapshot(after,before,engine.rows)['status'],'PASS')
            other=9 if motor!=9 else 8
            after[f'right:{other}']['Present_Position']+=5
            self.assertEqual(m.classify_position_snapshot(after,before,engine.rows)['status'],'FAIL')

    def test_sdk_initial_and_post_disable_signed_fifties_need_independent_fresh_windows(self):
        for wheel in m.WHEEL_PULSE_TARGETS:
            for phase in ('initial','post_disable'):
                for trigger in (50,-50):
                    executor,engine=self.stability_engine(wheel,phase,trigger)
                    row=self.perform(executor,engine);window=row[phase+'_stability']
                    self.assertEqual(window['status'],'PASS');self.assertEqual(len(window['samples']),14)
                    self.assertEqual(window['position_span_raw'],1)
                    self.assertEqual(window['transport'],'SDK_ORDINARY_READ')
                    self.assertEqual(window['anchor_raw'],1000 if phase=='initial' else None)
                    self.assertEqual([s['raw']['raw_unsigned'] for s in window['samples'] if s['register']=='Present_Velocity'],
                                     [0,50,0x8032,0,50])
                    self.assertTrue(all(s['raw']['kind']=='read' and s['raw']['motor']==executor.sdk_target for s in window['samples']))
                    self.assertTrue(all('operation' not in s['raw'] and 'expected_tx' in s['raw'] and 'rx_bytes' in s['raw']
                                        for s in window['samples']))
                    self.assertTrue(all(not s['raw']['cleanup'] for s in window['samples']))
                    self.assertTrue(row['stopped_confirmed']);self.assertFalse(row['uart_blocked'])
                    self.assertTrue(row['connection']['closed']);self.assertEqual(executor.calls,[])
                    self.assertLessEqual(row['enabled_attempt_window_ns'],21_000_000_000)
                    if phase=='initial':
                        self.assertLessEqual(window['finished_ns'],row['writes'][0]['started_ns'])
                        self.assertEqual(next(e['raw_unsigned'] for e in row['before_raw'] if e['register']=='Present_Velocity'),
                                         50 if trigger==50 else 0x8032)
                        self.assertNotIn('post_disable_stability',row)
                    else:
                        self.assertGreater(window['started_ns'],row['disable_finished_ns'])
                        self.assertEqual(row['settle_finished_ns'],window['started_ns'])
                        self.assertEqual(row['cleanup_raw'][-1]['raw_unsigned'],50 if trigger==50 else 0x8032)
                        self.assertNotIn('initial_stability',row)

    def test_sdk_zero_fast_path_and_both_fifty_windows_do_not_reuse_samples(self):
        for wheel in m.WHEEL_PULSE_TARGETS:
            executor,engine=self.engine(wheel);row=self.perform(executor,engine)
            self.assertNotIn('initial_stability',row);self.assertNotIn('post_disable_stability',row)
            executor,engine=self.stability_engine(wheel,'both',-50);row=self.perform(executor,engine)
            first,last=row['initial_stability'],row['post_disable_stability']
            self.assertEqual((first['status'],last['status']),('PASS','PASS'))
            self.assertIsNot(first,last);self.assertIsNot(first['samples'],last['samples'])
            self.assertLess(first['samples'][-1]['command_sequence'],last['samples'][0]['command_sequence'])
            self.assertEqual((executor.sdk_open_calls,executor.sdk_close_calls),(1,1))
            self.assertEqual(first['anchor_raw'],1000);self.assertIsNone(last['anchor_raw'])

    def test_sdk_stability_span_anchor_and_raw_values_fail_without_retry_or_later_uart(self):
        for wheel in m.WHEEL_PULSE_TARGETS:
            for phase in ('initial','post_disable'):
                for fault in ('span','wrap','anchor','velocity51','velocity_minus51','negative_zero','torque','goal'):
                    positions=[1000,1001,1002,1001,1000] if fault=='span' else [4095,0,0,0,0] if fault=='wrap' else [1000]*5
                    if fault=='anchor' and phase=='post_disable':positions=[1000,1000,1000,1000,1002]
                    executor,engine=self.stability_engine(wheel,phase,positions=positions,
                        velocities=[51]*5 if fault=='velocity51' else [-51]*5 if fault=='velocity_minus51'
                                   else [0x8000]*5 if fault=='negative_zero' else [50,-50,50,-50,0])
                    if fault=='anchor' and phase=='initial':
                        original=executor.value
                        executor.value=lambda r:1002 if r=='Present_Position' and phase+'_stability' not in engine.right_pulse else original(r)
                    if fault in ('torque','goal'):
                        original=executor.value;register='Torque_Enable' if fault=='torque' else 'Goal_Velocity'
                        executor.value=lambda r:1 if r==register and phase+'_stability' in engine.right_pulse else original(r)
                    row=self.perform(executor,engine,False);window=row[phase+'_stability']
                    self.assertEqual(window['status'],'FAIL');self.assertTrue(window['errors'])
                    self.assertLess(len(window['samples']),14);self.assertTrue(row['uart_blocked'])
                    self.assertTrue(row['connection']['closed']);self.assertTrue(engine.manual_cut)
                    if phase=='initial':self.assertEqual(row['writes'],[])
                    else:
                        self.assertEqual(sum(w['register']=='Torque_Enable' and w['value']==0 for w in row['writes']),1)
                    last=window['samples'][-1]['raw']
                    self.assertIs(last,row['sdk_operations'][-1])
                    count=len(executor.sdk_packets)
                    with self.assertRaises(ValueError):executor.poke('right',executor.sdk_target,'read','Torque_Enable',cleanup=True)
                    self.assertEqual(len(executor.sdk_packets),count)

    def test_sdk_stability_bad_ordinary_read_frames_never_become_sync_or_zero(self):
        for wheel in m.WHEEL_PULSE_TARGETS:
            for phase in ('initial','post_disable'):
                for fault in ('checksum','length','status','io','wrong_id'):
                    executor,engine=self.stability_engine(wheel,phase)
                    def effect(key):
                        if phase+'_stability' not in engine.right_pulse:return None
                        if fault=='wrong_id':executor.sdk_reply_id=8 if executor.sdk_target!=8 else 9;return None
                        return fault
                    executor.sdk_fault=effect
                    row=self.perform(executor,engine,False);window=row[phase+'_stability']
                    self.assertEqual(window['status'],'FAIL');self.assertEqual(len(window['samples']),1)
                    entry=window['samples'][0]['raw']
                    self.assertEqual(entry['kind'],'read');self.assertEqual(entry['status'],'UNKNOWN')
                    self.assertNotIn('operation',entry);self.assertIn('rx_bytes',entry)
                    self.assertIs(entry,row['sdk_operations'][-1]);self.assertTrue(row['connection']['closed'])
                    if phase=='initial':self.assertEqual(row['writes'],[])

    def test_sdk_stability_deadline_go_and_signal_cover_actual_tx_and_preserve_late_frame(self):
        for wheel in m.WHEEL_PULSE_TARGETS:
            for phase in ('initial','post_disable'):
                for fault in ('flush_deadline','return_deadline','go','signal'):
                    executor,engine=self.stability_engine(wheel,phase)
                    expired=False
                    engine.right_pulse_go_check=lambda:m.require(not expired,'稳定性GO过期')
                    def flush():
                        window=engine.right_pulse.get(phase+'_stability')
                        if window and fault=='flush_deadline':executor.t=window['deadline_ns']
                    executor.sdk_flush_hook=flush
                    def effect(key):
                        nonlocal expired
                        window=engine.right_pulse.get(phase+'_stability')
                        if not window:return None
                        if fault=='return_deadline':executor.t=window['deadline_ns']+1
                        if fault=='go':expired=True
                        if fault=='signal':return 'signal'
                        return None
                    executor.sdk_fault=effect
                    row=self.perform(executor,engine,False);window=row[phase+'_stability']
                    self.assertEqual(window['status'],'FAIL');self.assertEqual(len(window['samples']),1)
                    entry=window['samples'][0]['raw']
                    self.assertEqual(entry['tx_attempts'],0 if fault=='flush_deadline' else 1)
                    if fault!='flush_deadline':self.assertEqual(entry['status'],'READ_VALID');self.assertIn('raw_unsigned',entry)
                    self.assertIs(entry,row['sdk_operations'][-1]);self.assertTrue(row['uart_blocked'])
                    self.assertTrue(row['connection']['closed'])
                    if phase=='initial':self.assertEqual(row['writes'],[])

    def test_sdk_stability_contract_and_old_plan_go_cannot_release_new_path(self):
        for wheel in m.WHEEL_PULSE_TARGETS:
            plan=self.plan(wheel);check=plan['right_wheel_pulse']['sdk_stop_stability']
            self.assertEqual(check['contract'],m.left_stop_contract(motor=m.WHEEL_PULSE_TARGETS[wheel][1]))
            self.assertEqual(check['trigger_raw_unsigned'],[50,0x8032])
            self.assertIn('独立追加最多1.5秒',plan['scope'])
            for fault in ('missing','trigger','span','budget','target'):
                bad=copy.deepcopy(plan)
                if fault=='missing':del bad['right_wheel_pulse']['sdk_stop_stability']
                if fault=='trigger':bad['right_wheel_pulse']['sdk_stop_stability']['trigger_raw_unsigned'].append(51)
                if fault=='span':bad['right_wheel_pulse']['sdk_stop_stability']['contract']['position_span_max']=2
                if fault=='budget':bad['right_wheel_pulse']['sdk_stop_stability']['contract']['capture_budget_ns']+=1
                if fault=='target':bad['right_wheel_pulse']['sdk_stop_stability']['contract']['target'][1]=6
                with self.assertRaises(ValueError):m.validate_right_pulse_plan(bad)
                facts={**{k:True for k in m.FACTS},'confirmed_at_ns':100}
                go={'schema':'MAINTENANCE_GO_V2','issued_by':'root','artifact_root':plan['artifact_root'],
                    'plan_sha256':m.digest(bad),'issued_at_ns':100,'expires_at_ns':1000,'host':plan['bindings']['host'],
                    'source':'纯软件夹具','physical':{side:dict(facts) for side in m.SERIALS}}
                go['physical']['right'].update(all_three_wheels_off_ground=True,base_stably_supported=True,
                    continuous_wheel_observation=True,base_confirmation_kind='CURRENT_SETUP_READY',
                    base_confirmation_scope=plan['scope'],base_confirmation_answer='软件')
                with self.assertRaises(ValueError):m.validate_go(go,bad,200)
                with self.assertRaises(ValueError):m.validate_go(go,plan,200)

    def test_named_target_cli_and_plan_reject_arbitrary_ids_mixed_or_multiple_selections(self):
        with patch.object(m.subprocess,'Popen',side_effect=AssertionError('拒绝前置不得child')) as child:
            for args in (['--wheel-pulse','7'],['--wheel-pulse','right'],
                         ['--wheel-pulse','front','--right-wheel-pulse'],
                         ['--wheel-pulse','front','--wheel-pulse','left'],
                         ['--wheel-pulse','front','--position-target','head-1'],
                         ['--wheel-pulse','left','--right-wheel-feedback-only']):
                with contextlib.redirect_stderr(io.StringIO()),self.assertRaises((ValueError,SystemExit)):run_main(args)
            child.assert_not_called()
        root='/home/loosqk/robot-checks/pulse-fixture'
        for target in (7,8,9,'right','head-1',True):
            with self.assertRaises(ValueError):m.make_plan(self.bindings(),root+'/run','fixture',root,wheel_pulse_target=target)
        with self.assertRaises(ValueError):m.make_plan(self.bindings(),root+'/run','fixture',root,
                                                     right_wheel_pulse=True,wheel_pulse_target='front')
        for wheel in ('right','front','left'):
            plan=self.plan(wheel)
            self.assertEqual(m.position_targets_from_plan(json.loads(json.dumps(plan))),[])
            for target in (['right',6,'right_arm_gripper'],['left',7,'head_motor_1'],
                           ['right',8,'base_right_wheel'],['right',7,'base_front_wheel']):
                bad=copy.deepcopy(plan);bad['right_wheel_pulse']['target']=target
                with self.assertRaises(ValueError):m.position_targets_from_plan(bad)

    def test_target_scope_and_digest_prevent_cross_wheel_go_reuse(self):
        plans={wheel:self.plan(wheel) for wheel in ('right','front','left')}
        self.assertEqual(len({m.digest(plan) for plan in plans.values()}),3)
        facts={**{key:True for key in m.FACTS},'confirmed_at_ns':100}
        for wheel,plan in plans.items():
            go={'schema':'MAINTENANCE_GO_V2','issued_by':'root','artifact_root':plan['artifact_root'],
                'plan_sha256':m.digest(plan),'issued_at_ns':100,'expires_at_ns':1000,'host':plan['bindings']['host'],
                'source':'仅目标绑定夹具','physical':{side:dict(facts) for side in m.SERIALS}}
            go['physical']['right'].update(all_three_wheels_off_ground=True,base_stably_supported=True,
                continuous_wheel_observation=True,base_confirmation_kind='CURRENT_SETUP_READY',
                base_confirmation_scope=m.pulse_scope(wheel),base_confirmation_answer='夹具就绪')
            m.validate_go(go,plan,200)
            for other in plans:
                if other==wheel:continue
                with self.assertRaises(ValueError):m.validate_go(go,plans[other],200)
                bad=copy.deepcopy(go);bad['plan_sha256']=m.digest(plans[other])
                with self.assertRaises(ValueError):m.validate_go(bad,plans[other],200)

    def test_selected_torque_read_trace_and_sdk_status_reject_other_wheel_ids(self):
        for wheel,motor in (('front',8),('left',9)):
            executor,engine=self.engine(wheel)
            self.assertEqual(executor.poke('right',motor,'read','Torque_Enable'),0)
            trace=executor.commands[-1]['ordinary_read_trace']
            self.assertEqual(trace['motor_ids'],[motor])
            self.assertTrue(Path(trace['path']).name.startswith(f'right{motor}-pulse-read-'))
            executor,engine=self.engine(wheel);executor.sdk_reply_id=7
            row=self.perform(executor,engine,False)
            self.assertEqual(row['writes'],[]);self.assertTrue(engine.manual_cut)

    def test_wire_chunks_are_actual_sdk_frames_and_comm_results_not_fake_child(self):
        executor,engine=self.engine();row=self.perform(executor,engine)
        path=Path(row['wire_evidence']['path']);raw=path.read_bytes()
        self.assertEqual(hashlib.sha256(raw).hexdigest(),row['wire_evidence']['sha256'])
        events=[json.loads(line) for line in raw.splitlines()]
        actual=[event['bytes'] for event in events if event['direction']=='tx']
        self.assertEqual(actual,executor.sdk_packets)
        self.assertIn([255,255,254,7,131,46,2,7,244,1,75],actual)
        self.assertTrue(all(entry['comm']==0 and entry['tx_attempts']==1 for entry in row['sdk_operations']))
        self.assertFalse(any('argv' in entry or 'pid' in entry for entry in row['sdk_operations']))
        self.assertTrue(any(event['direction']=='rx' for event in events))
        self.assertTrue(all(entry['ack']=='NONE_SYNC_WRITE' for entry in row['writes'] if entry['kind']=='sync'))

    def test_adjacent_modular_position_steps_support_multiple_revolutions(self):
        executor,engine=self.engine();executor.position=4000;executor.delta=120
        row=self.perform(executor,engine)
        self.assertEqual(row['samples'][0]['position_raw'],24)
        self.assertEqual([s['delta_raw'] for s in row['samples']],[120]*79)
        self.assertEqual(row['cumulative_delta_raw'],9480)
        self.assertEqual(row['samples'][-1]['position_raw'],(4000+9480)%4096)

    def test_motion_reverse_excessive_velocity_bad_step_or_no_motion_always_cleans_once(self):
        for velocity,delta in ((-50,80),(1001,80),(0,0),(500,-1),(500,513),(0x8000,80)):
            executor,engine=self.engine();executor.velocity=velocity;executor.delta=delta
            row=self.perform(executor,engine,False)
            self.assertTrue(engine.manual_cut);self.assertTrue(row['uart_blocked'])
            self.assertTrue(row['connection']['closed'])
            self.assertEqual([(w['register'],w['value']) for w in row['writes'][-2:]],
                             [('Goal_Velocity',0),('Torque_Enable',0)])
            self.assertEqual(sum(w['register']=='Torque_Enable' and w['value']==0 for w in row['writes']),1)

    def test_sdk_all_actual_metadata_and_initial_stop_values_are_required_before_any_write(self):
        for register in (*m.RIGHT_PULSE_METADATA,'Torque_Enable','Goal_Velocity','Present_Velocity'):
            executor,engine=self.engine();original=executor.value
            executor.value=lambda name,r=register:original(name)+1 if name==r else original(name)
            row=self.perform(executor,engine,False)
            self.assertEqual(row['writes'],[]);self.assertTrue(engine.manual_cut)
            self.assertTrue(row['connection']['closed'])

    def test_initial_zero_unknown_never_enables_or_retries_zero_but_attempts_disable(self):
        executor,engine=self.engine()
        executor.sdk_fault=lambda key:'io' if key==(131,'Goal_Velocity',0) else None
        row=self.perform(executor,engine,False)
        self.assertEqual([(w['register'],w['value']) for w in row['writes']],
                         [('Goal_Velocity',0),('Torque_Enable',0)])
        self.assertFalse(row['enable_attempted']);self.assertTrue(row['off_attempted'])

    def test_enable_and_nonzero_uncertain_writes_keep_unique_independent_disable(self):
        for kind,register,value in ((3,'Torque_Enable',1),(131,'Goal_Velocity',500)):
            for fault in ('io','short','status'):
                if kind==131 and fault=='status':continue
                executor,engine=self.engine()
                executor.sdk_fault=lambda key,k=(kind,register,value),f=fault:f if key==k else None
                row=self.perform(executor,engine,False)
                self.assertEqual(sum(w['register']=='Torque_Enable' and w['value']==0 for w in row['writes']),1)
                self.assertTrue(engine.manual_cut);self.assertTrue(row['connection']['closed'])
                self.assertEqual(executor.torque,0)

    def test_repeated_nonzero_failure_does_not_retry_loop_and_disables_once(self):
        executor,engine=self.engine()
        executor.sdk_fault=lambda key:'io' if key==(131,'Goal_Velocity',500) and executor.sdk_nonzero_count==4 else None
        row=self.perform(executor,engine,False)
        self.assertEqual(executor.sdk_nonzero_count,4)
        self.assertEqual([w['value'] for w in row['writes'][-2:]],[0,0])
        self.assertTrue(engine.manual_cut);self.assertEqual(row['cleanup_raw'],[])

    def test_cleanup_zero_failure_still_attempts_torque_off_and_then_closes(self):
        executor,engine=self.engine()
        executor.sdk_fault=lambda key:'io' if key==(131,'Goal_Velocity',0) and executor.zero_count==2 else None
        row=self.perform(executor,engine,False)
        self.assertTrue(row['zero_attempted']);self.assertTrue(row['off_attempted'])
        self.assertEqual(executor.torque,0);self.assertEqual(row['cleanup_raw'],[])
        self.assertEqual(executor.sdk_close_calls,1);self.assertTrue(row['uart_blocked'])

    def test_torque_off_unknown_never_retries_or_reads_after_the_uncertain_ack(self):
        executor,engine=self.engine()
        executor.sdk_fault=lambda key:'status' if key==(3,'Torque_Enable',0) else None
        row=self.perform(executor,engine,False)
        self.assertEqual(row['sdk_operations'][-1]['register'],'Torque_Enable')
        self.assertEqual(row['sdk_operations'][-1]['kind'],'write')
        self.assertEqual(executor.sdk_close_calls,1);self.assertTrue(engine.manual_cut)

    def test_signal_go_expiry_and_overrun_do_not_reset_fixed_deadline_or_repeat_motion(self):
        for fault in ('signal','go','late'):
            executor,engine=self.engine()
            if fault=='go':engine.right_pulse_go_check=lambda:m.require(executor.now()<1_000_000_000,'GO到期')
            else:executor.sdk_fault=lambda key,f=fault:f if key==(131,'Goal_Velocity',500) and executor.sdk_nonzero_count==4 else None
            row=self.perform(executor,engine,False)
            self.assertTrue(row['off_attempted']);self.assertTrue(engine.manual_cut)
            self.assertEqual(row['observation_deadline_ns']-row['first_velocity_attempt_started_ns'],20_000_000_000)
            self.assertTrue(row['connection']['closed'])
            if fault=='late':self.assertGreater(row['enabled_attempt_window_ns'],21_000_000_000)

    def test_sdk_flush_crossing_deadline_or_go_cannot_send_the_next_nonzero_packet(self):
        for fault in ('deadline','go'):
            executor,engine=self.engine()
            if fault=='go':engine.right_pulse_go_check=lambda:m.require(executor.now()<1_000_000_000,'flush后GO到期')
            def flush():
                row=engine.right_pulse
                if row['sdk_operations'] and row['sdk_operations'][-1]['kind']=='sync' and (
                        row['sdk_operations'][-1]['value']==500 and executor.sdk_nonzero_count==1):
                    executor.t=row['observation_deadline_ns']+1 if fault=='deadline' else 1_000_000_001
            executor.sdk_flush_hook=flush
            row=self.perform(executor,engine,False)
            self.assertEqual(executor.sdk_nonzero_count,1)
            denied=[w for w in row['writes'] if w['value']==500 and w['status']=='UNKNOWN']
            self.assertEqual(len(denied),1);self.assertEqual(denied[0]['tx_attempts'],0)
            self.assertEqual((executor.torque,executor.goal),(0,0));self.assertTrue(row['off_attempted'])

    def test_sdk_success_tuple_cannot_hide_an_overlong_write_ack(self):
        executor,engine=self.engine();sdk=fake_sdk(executor);original=sdk.PacketHandler
        executor.sdk_fault=lambda key:'ack_long' if key==(3,'Torque_Enable',1) else None
        def packet(protocol):
            value=original(protocol);write=value.write1ByteTxRx
            def accepting(port,motor,address,word):
                comm,error=write(port,motor,address,word)
                if word==1:
                    port.readPort(1)
                    return 0,0
                return comm,error
            value.write1ByteTxRx=accepting
            return value
        sdk.PacketHandler=packet
        row=self.perform(executor,engine,False,sdk=sdk)
        attempt=next(w for w in row['writes'] if w['register']=='Torque_Enable' and w['value']==1)
        self.assertEqual((attempt['comm'],attempt['error_code']),(0,0))
        self.assertEqual(len(attempt['rx_bytes']),7)
        self.assertEqual(attempt['status'],'UNKNOWN');self.assertEqual(executor.sdk_nonzero_count,0)
        self.assertTrue(row['off_attempted']);self.assertTrue(engine.manual_cut)

    def test_enable_readback_and_budget_fail_before_first_nonzero(self):
        for fault in ('readback','late'):
            executor,engine=self.engine()
            if fault=='readback':
                original=executor.value
                executor.value=lambda register:0 if register=='Torque_Enable' else original(register)
            else:executor.sdk_fault=lambda key:'late' if key==(3,'Torque_Enable',1) else None
            row=self.perform(executor,engine,False)
            self.assertEqual(executor.sdk_nonzero_count,0);self.assertTrue(row['off_attempted'])

    def test_missing_status_corrupt_frame_and_communication_failure_never_become_zero(self):
        for fault in ('status','checksum','length','io'):
            executor,engine=self.engine()
            executor.sdk_fault=lambda key,f=fault:f if key[0:2]==(2,'Present_Velocity') else None
            row=self.perform(executor,engine,False)
            self.assertEqual(row['writes'],[]);self.assertTrue(engine.manual_cut)
            self.assertEqual(row['before_raw'][-1]['status'],'UNKNOWN')

    def test_settle_positive_deceleration_then_fifty_starts_one_separate_window(self):
        for wheel in m.WHEEL_PULSE_TARGETS:
            executor,engine=self.stability_engine(wheel,'post_disable')
            executor.sdk_coast=[250,250,250,250,50]
            row=self.perform(executor,engine);window=row['post_disable_stability']
            self.assertEqual([s['velocity_raw'] for s in row['settle_samples']],[250]*4+[50])
            self.assertGreaterEqual(window['started_ns']-row['disable_finished_ns'],950_000_000)
            self.assertLessEqual(window['started_ns'],row['settle_deadline_ns'])
            self.assertEqual(window['deadline_ns']-window['started_ns'],1_500_000_000)
            self.assertGreater(window['finished_ns'],row['settle_deadline_ns'])
            self.assertEqual(row['settle_finished_ns'],window['started_ns'])
            self.assertTrue(row['stopped_confirmed']);self.assertFalse(engine.manual_cut)
            self.assertEqual(window['status'],'PASS');self.assertEqual(len(window['samples']),14)
            self.assertTrue(all(s['raw_unsigned']==0 for s in row['cleanup_raw'] if s['register']!='Present_Velocity'))

    def test_settle_never_accepts_persistent_positive_above_fifty_or_retries_failed_window(self):
        executor,engine=self.engine();executor.sdk_coast=[250]*5
        row=self.perform(executor,engine,False)
        self.assertFalse(row['stopped_confirmed']);self.assertTrue(engine.manual_cut)
        self.assertEqual([s['velocity_raw'] for s in row['settle_samples']],[250]*5)
        self.assertNotIn('post_disable_stability',row);self.assertTrue(row['connection']['closed'])
        count=len(executor.calls)
        with self.assertRaises(ValueError):executor.poke('right',9,'read','Present_Velocity',cleanup=True)
        self.assertEqual(len(executor.calls),count)

    def test_settle_torque_goal_reverse_and_out_of_range_fail_immediately(self):
        for register,value in (('Torque_Enable',1),('Goal_Velocity',500),('Present_Velocity',-51),
                               ('Present_Velocity',1001),('Present_Velocity',0x8000)):
            executor,engine=self.engine();executor.sdk_after_off={register:value}
            row=self.perform(executor,engine,False)
            self.assertTrue(engine.manual_cut);self.assertFalse(row['stopped_confirmed'])
            self.assertEqual(len(row['settle_samples']),1)

    def test_settle_timeout_or_go_crossing_keeps_last_returned_frame_and_fails(self):
        for fault in ('timeout','go'):
            executor,engine=self.engine()
            if fault=='timeout':
                executor.sdk_fault=lambda key:'late' if key[0:2]==(2,'Goal_Velocity') and executor.zero_count==2 else None
            else:
                engine.right_pulse_go_check=lambda:m.require(executor.now()<
                    engine.right_pulse.get('disable_finished_ns',10**12)+51_000_000,'停稳GO到期')
            row=self.perform(executor,engine,False)
            self.assertTrue(engine.manual_cut);self.assertFalse(row['stopped_confirmed'])
            self.assertTrue(row['cleanup_raw'])
            self.assertEqual(row['cleanup_raw'][-1]['status'],'READ_VALID')

    def test_close_failure_keeps_cli_blocked_even_after_a_zero_stop_readback(self):
        executor,engine=self.engine();executor.sdk_close_fails=True
        row=self.perform(executor,engine,False)
        self.assertTrue(row['stopped_confirmed']);self.assertTrue(engine.right_sdk_active)
        self.assertTrue(engine.manual_cut)
        with self.assertRaises(ValueError):executor.poke('right',8,'read','Torque_Enable',cleanup=True)
        self.assertEqual(executor.calls,[])

    def test_fixed_wire_whitelist_rejects_hidden_other_motor_or_register_writes(self):
        executor,engine=self.engine();sdk=fake_sdk(executor);original=sdk.GroupSyncWrite
        class BadWriter:
            def __init__(self,*args):self.inner=original(*args);self.port=args[0]
            def clearParam(self):self.inner.clearParam()
            def addParam(self,*args):return self.inner.addParam(*args)
            def txPacket(self):return self.port.writePort([255,255,9,4,3,33,1,205])
        sdk.GroupSyncWrite=BadWriter
        row=self.perform(executor,engine,False,sdk)
        self.assertFalse(any(p[5]==33 and p[4]==3 for p in executor.sdk_packets))
        self.assertTrue(row['off_attempted']);self.assertTrue(engine.manual_cut)

    def test_cli_cannot_access_any_motor_while_the_sdk_connection_is_active(self):
        executor,engine=self.engine();engine.right_sdk_active=True
        with self.assertRaises(ValueError):executor.poke('left',1,'read','Torque_Enable')
        self.assertEqual(executor.calls,[])

    def test_initial_and_final_other_motor_stop_values_remain_strict_and_sticky(self):
        for after in (False,True):
            executor,engine=self.engine()
            if after:self.perform(executor,engine)
            executor.stop_velocity=50
            with contextlib.redirect_stdout(io.StringIO()),self.assertRaises(ValueError):
                executor.poke('right',8,'read','Present_Velocity',cleanup=after)
            count=len(executor.calls)
            with self.assertRaises(ValueError):executor.poke('left',1,'read','Torque_Enable',cleanup=True)
            self.assertEqual(len(executor.calls),count);self.assertTrue(engine.manual_cut)

    def test_plan_contract_sdk_refs_old_timing_and_missing_go_are_rejected(self):
        self.assertEqual(m.position_targets_from_plan(json.loads(json.dumps(self.plan()))),[])
        for fault in ('sdk','timing','velocity','targets','scope'):
            plan=self.plan()
            if fault=='sdk':plan['bindings']['refs'].pop()
            if fault=='timing':plan['right_wheel_pulse']['observation_ns']=500_000_000
            if fault=='velocity':plan['right_wheel_pulse']['raw_velocity']=501
            if fault=='targets':plan['position_targets']=m.TARGETS
            if fault=='scope':plan['scope']='旧500ms'
            with self.assertRaises((ValueError,KeyError)):
                if fault=='scope':m.validate_go({},plan,0)
                else:m.position_targets_from_plan(plan)
        with self.assertRaises((ValueError,KeyError)):m.validate_go({},self.plan(),200)
        with patch.object(m.subprocess,'Popen',side_effect=AssertionError('前置不得child')) as child:
            for args in (['--right-wheel-pulse','--position-target','head-1'],['--right-wheel-pulse','--execute']):
                with contextlib.redirect_stderr(io.StringIO()),self.assertRaises((ValueError,SystemExit)):run_main(args)
            child.assert_not_called()

    def test_sdk_loader_rejects_ordinary_startup_or_preexisting_package_before_import(self):
        for flags,modules in ((SimpleNamespace(isolated=False,no_site=False),{}),
                              (SimpleNamespace(isolated=True,no_site=True),{'serial':SimpleNamespace()})):
            with patch.object(m.sys,'flags',flags),patch.dict(m.sys.modules,modules),patch.object(
                    m.importlib,'import_module',side_effect=AssertionError('不得导入')) as imported:
                with self.assertRaises(ValueError):m.load_right_pulse_sdk()
                imported.assert_not_called()

    def test_wire_journal_failure_after_nonzero_still_sends_final_zero_and_disable(self):
        executor,engine=self.engine();original=Path.open
        def opening(path,*args,**kwargs):
            stream=original(path,*args,**kwargs)
            if path.name!='right7-sdk-wire.jsonl' or not args or args[0]!='x':return stream
            def write(value):
                if engine.right_pulse['velocity_attempted']:raise OSError('原帧日志写失败')
                return stream.write(value)
            return SimpleNamespace(write=write,flush=stream.flush,close=stream.close)
        with patch.object(Path,'open',opening):row=self.perform(executor,engine,False)
        self.assertTrue(executor.evidence_errors);self.assertTrue(engine.manual_cut)
        self.assertEqual(executor.torque,0);self.assertTrue(row['connection']['closed'])
        self.assertEqual([(w['register'],w['value']) for w in row['writes'][-2:]],
                         [('Goal_Velocity',0),('Torque_Enable',0)])

    def test_settle_communication_error_does_not_get_a_second_sampling_attempt(self):
        executor,engine=self.engine()
        executor.sdk_fault=lambda key:'status' if key[0:2]==(2,'Present_Velocity') and executor.zero_count==2 else None
        row=self.perform(executor,engine,False)
        self.assertFalse(row['stopped_confirmed']);self.assertTrue(engine.manual_cut)
        self.assertEqual(len(row['settle_samples']),1)
        self.assertEqual(row['sdk_operations'][-1]['error_code'],4)
        self.assertTrue(row['connection']['closed'])

    def test_go_binds_the_new_scope_and_current_ten_minute_field_state(self):
        plan=self.plan();facts={**{key:True for key in m.FACTS},'confirmed_at_ns':100}
        go={'schema':'MAINTENANCE_GO_V2','issued_by':'root','artifact_root':plan['artifact_root'],
            'plan_sha256':m.digest(plan),'issued_at_ns':100,'expires_at_ns':1000,'host':plan['bindings']['host'],
            'source':'仅测试夹具','physical':{side:dict(facts) for side in m.SERIALS}}
        go['physical']['right'].update(all_three_wheels_off_ground=True,base_stably_supported=True,
            continuous_wheel_observation=True,base_confirmation_kind='CURRENT_SETUP_READY',
            base_confirmation_scope=m.pulse_scope('right'),base_confirmation_answer='夹具就绪')
        m.validate_go(go,plan,200)
        for field,value in (('base_confirmation_scope','旧500ms'),('continuous_wheel_observation',False)):
            bad=copy.deepcopy(go);bad['physical']['right'][field]=value
            with self.assertRaises(ValueError):m.validate_go(bad,plan,200)

    def test_diagnostic_success_does_not_clear_firmware_deferred_or_other_wheel_position(self):
        executor,engine=self.engine()
        with patch.object(m,'load_right_pulse_sdk',return_value=fake_sdk(executor)),self.assertRaisesRegex(
                ValueError,'BASE_FIRMWARE_CONTRACT_UNVERIFIED'):
            m.run_motion_probes(engine,engine.plan['bindings'],engine.plan)
        self.assertEqual(engine.rows[0]['status'],'PASS')
        before=idle_rows();after=copy.deepcopy(before);after['right:7']['Present_Position']=executor.position
        self.assertEqual(m.classify_position_snapshot(after,before,engine.rows)['status'],'PASS')
        after['right:8']['Present_Position']+=5
        self.assertEqual(m.classify_position_snapshot(after,before,engine.rows)['status'],'FAIL')
        self.assertTrue(all(row['decision']=='DEFERRED' for row in engine.rows[1:]))

    def test_actual_sdk_golden_when_explicit_same_version_source_is_available(self):
        site=os.environ.get('SDK_GOLDEN_SITE')
        if not site:
            self.skipTest('未提供离线SDK源码目录；内存端点验证独立执行')
        source=Path(site)
        for reference in m.RIGHT_PULSE_SDK['refs']:
            relative=Path(reference['path']).relative_to(m.RIGHT_PULSE_SDK['site'])
            if relative.suffix=='.py':
                self.assertEqual(hashlib.sha256((source/relative).read_bytes()).hexdigest(),reference['sha256'])
        prefix=lambda name:name=='serial' or name.startswith('serial.') or name=='scservo_sdk' or name.startswith('scservo_sdk.')
        saved={name:module for name,module in sys.modules.items() if prefix(name)}
        for name in saved:del sys.modules[name]
        sys.path.insert(0,str(source))
        try:
            sdk=m.importlib.import_module('scservo_sdk');serial=m.importlib.import_module('serial')
            self.assertEqual(serial.VERSION,'3.5')
            original=sdk.PortHandler
            for wheel,motor in (('right',7),('front',8),('left',9)):
                cases=[(False,[250,50,0],None,0),(True,[250,-50,-50],None,0),(True,[300,0,50],None,0)]
                cases += [(False,[250,50,0],phase,trigger) for phase in ('initial','post_disable') for trigger in (50,-50)]
                for zero_stop,speeds,phase,trigger in cases:
                    executor,engine=(self.stability_engine(wheel,phase,trigger) if phase else
                                     self.engine(wheel,zero_stop_check=zero_stop))
                    executor.sdk_zero_coast=list(speeds)
                    def port(device):
                        instance=original(device)
                        instance.getCurrentTime=lambda:executor.now()/1_000_000
                        return instance
                    with patch.object(serial,'Serial',side_effect=lambda **kwargs:SdkMotorSerial(executor)),patch.object(sdk,'PortHandler',side_effect=port):
                        row=self.perform(executor,engine,sdk=sdk)
                    self.assertEqual(row['status'],'PASS');self.assertEqual(executor.sdk_close_calls,1)
                    self.assertGreater(executor.sdk_nonzero_count,300)
                    expected=[255,255,254,7,131,46,2,motor,244,1]
                    expected.append((~sum(expected[2:]))&255)
                    self.assertIn(expected,executor.sdk_packets)
                    self.assertTrue(all(op['motor']==motor for op in row['sdk_operations']))
                    if phase:
                        window=row[phase+'_stability']
                        self.assertEqual(window['status'],'PASS');self.assertEqual(window['position_span_raw'],1)
                        self.assertEqual(len(window['samples']),14)
                        self.assertTrue(all(s['raw']['rx_bytes'][2]==motor for s in window['samples']))
                        self.assertIn(0x8032,[s['raw']['raw_unsigned'] for s in window['samples']
                                            if s['register']=='Present_Velocity'])
                    if zero_stop:
                        zero=row['zero_stop_check']
                        self.assertEqual([s['velocity_raw'] for s in zero['samples']],
                                         [0x8032 if value==-50 else value for value in speeds])
                        entry=zero['samples'][-1]['raw'][-1]
                        expected_word=0x8032 if speeds[-1]==-50 else speeds[-1]
                        self.assertEqual(entry['data'],[expected_word&255,expected_word>>8])
                        self.assertEqual(entry['rx_bytes'][5:7],[expected_word&255,expected_word>>8])
                        self.assertTrue(all([e['raw_unsigned'] for e in s['raw'][:2]]==[1,0] for s in zero['samples']))
                        self.assertEqual(zero['status'],'PASS');self.assertTrue(row['stopped_confirmed'])
        finally:
            sys.path.remove(str(source))
            for name in tuple(sys.modules):
                if prefix(name):del sys.modules[name]
            sys.modules.update(saved)


class WheelFeedbackTests(unittest.TestCase):
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

    def plan(self,*,front=False,left=False):
        root='/home/loosqk/robot-checks/feedback'
        return m.make_plan(self.bindings(),root+'/run','feedback',root,
                           right_wheel_feedback_only=not (front or left),front_wheel_feedback_only=front,
                           left_wheel_feedback_only=left)

    def go(self,plan,now):
        facts={**{k:True for k in m.FACTS},'confirmed_at_ns':now}
        return {'schema':'MAINTENANCE_GO_V2','issued_by':'root','artifact_root':plan['artifact_root'],
                'plan_sha256':m.digest(plan),'issued_at_ns':now,'expires_at_ns':now+60_000_000_000,
                'host':plan['bindings']['host'],'source':'内存夹具，无硬件授权',
                'physical':{'left':dict(facts),'right':{**facts,'all_three_wheels_off_ground':True,
                'base_stably_supported':True,'continuous_wheel_observation':True,
                'base_confirmation_kind':'CURRENT_SETUP_READY','base_confirmation_scope':plan['scope'],
                'base_confirmation_answer':'夹具只读'}}}

    def engine(self,*,front=False,left=False):
        return m.ProbeEngine(FeedbackIO(motor=9 if left else 8 if front else 7),self.plan(front=front,left=left))

    def test_toml_permutations_follow_readonly_names_and_keep_physical_id_policy(self):
        self.addCleanup(m.configure_bus_mapping,BUS_FIXTURE_PATH)
        path=self.root/'bus2.toml'
        for ids in itertools.permutations((7,8,9)):
            m.configure_bus_mapping(write_bus_fixture(path,ids))
            for role,motor in zip(m.WHEEL_ROLES,ids):
                with self.subTest(ids=ids,role=role):
                    plan=self.plan(front=role=='front',left=role=='left')
                    self.assertEqual(m.feedback_entry(plan),(f'{role}_wheel_feedback_only',motor))
                    io=FeedbackIO(motor=motor);engine=m.ProbeEngine(io,plan)
                    if motor==7:m.run_right_wheel_feedback(engine,lambda:None)
                    else:m.run_front_wheel_feedback(engine,lambda:None,motor=motor)
                    row=engine.rows[0]
                    self.assertEqual((row['motor'],row['name']),(motor,f'base_{role}_wheel'))
                    self.assertTrue(row['kind'].startswith(f'{role}{motor}_'))
                    self.assertEqual(row['status'],'PASS' if motor==7 else 'DIAGNOSTIC_CAPTURED')
                    io=FeedbackIO(motor=motor);io.velocities=[500]*5
                    engine=m.ProbeEngine(io,plan)
                    if motor==7:
                        with self.assertRaises(ValueError):m.run_right_wheel_feedback(engine,lambda:None)
                    else:
                        m.run_front_wheel_feedback(engine,lambda:None,motor=motor)
                        self.assertEqual(engine.rows[0]['status'],'DIAGNOSTIC_CAPTURED')

    def snapshot_engine(self,pv=0,guard=False,*,wheel_pv=None):
        rows=idle_rows();rows['right:7']['Present_Velocity']=pv
        for motor,value in (wheel_pv or {}).items():rows[f'right:{motor}']['Present_Velocity']=value
        executor=SnapshotIO(rows)
        engine=m.ProbeEngine(executor,self.plan());engine.left_stop_go_check=lambda:None
        if guard:
            raw_command=executor.command
            def command(argv,**kwargs):
                if argv[0]==m.RIGHT_PULSE_RAW['path']:
                    return raw_command(argv,**kwargs)
                self.assertEqual(argv[0],m.RIGHT_PULSE_WRITE['path'])
                self.assertEqual(argv[3],'read')
                side=next(side for side in m.SERIALS if m.DEVICES[side]==argv[1])
                key=f'{side}:{argv[2]}';register=argv[4]
                executor.reads.append((key,register,kwargs.get('cleanup',False)))
                executor.sequence+=1;executor.t+=1_000_000
                return f'read {register}@{argv[2]} = {rows[key][register]}\n'
            executor.command=command
            executor.tool=m.RIGHT_PULSE_WRITE
            executor.poke=lambda *args,**kwargs:m.RealExecutor.poke(executor,*args,**kwargs)
            helper=SimpleNamespace(META=m.LEFT_FEEDBACK_META,install_stop_wire_capture=lambda module:None)
            m.install_right_pulse_guard(engine,helper)
        return engine

    def wheel_samples(self,engine,motor,**changes):
        if motor==7:
            for name,value in changes.items():setattr(engine.io,name,value)
        else:engine.io.wheel_samples[motor].update(changes)

    def guard_patches(self):
        stack=contextlib.ExitStack()
        verify=m.verify_ref
        stack.enter_context(patch.object(m,'verify_ref',side_effect=
            lambda ref,**kwargs:verify(ref,**kwargs) if ref['path']==self.reference['path'] else None))
        stack.enter_context(patch.object(m,'capture_right_pulse_wire',
            side_effect=lambda executor,argv,original,attempt,**kwargs:original(argv,**kwargs)))
        stack.enter_context(contextlib.redirect_stdout(io.StringIO()))
        return stack

    def test_fixed_fourteen_reads_record_fifty_without_whole_stop_pass(self):
        engine=self.engine();m.run_right_wheel_feedback(engine,lambda:None)
        row=engine.rows[0]
        self.assertEqual(row['status'],'PASS')
        self.assertFalse(row['stop_verified']);self.assertFalse(row['same_frame'])
        self.assertEqual([c[4] for c in engine.io.calls],
                         ['Torque_Enable','Goal_Velocity']+['Present_Position','Present_Velocity']*5+['Goal_Velocity','Torque_Enable'])
        self.assertTrue(all(c[:4]==[self.reference['path'],m.DEVICES['right'],'7','read-raw']
                            for c in engine.io.calls))
        pv=[s['raw']['raw_unsigned'] for s in row['samples'] if s['register']=='Present_Velocity']
        self.assertEqual(pv,[0,50,0x8032,50,0])
        self.assertEqual([s['started_ns']-row['observation_started_ns'] for s in row['samples']
                          if s['register']=='Present_Position'],[0,250_000_000,500_000_000,750_000_000,1_000_000_000])
        self.assertTrue(all(s['finished_ns']>s['started_ns'] for s in row['samples']))
        self.assertFalse(engine.manual_cut)
        with self.assertRaises(ValueError):m.run_right_wheel_feedback(engine,lambda:None)
        self.assertEqual(len(engine.io.calls),14)

    def test_every_read_fault_stops_without_retry_or_final_torque_read(self):
        for fault in ('fail_at','bad_at','signal_at'):
            for index in range(1,15):
                engine=self.engine();setattr(engine.io,fault,index)
                with self.subTest(fault=fault,index=index),self.assertRaises((ValueError,OSError)):
                    m.run_right_wheel_feedback(engine,lambda:None)
                self.assertEqual(len(engine.io.calls),index)
                self.assertTrue(engine.rows[0]['uart_blocked']);self.assertTrue(engine.manual_cut)
                self.assertEqual(engine.rows[0]['status'],'FAIL')
        for index in (1,14):
            engine=self.engine();engine.io.torque_at=index
            with self.assertRaises(ValueError):m.run_right_wheel_feedback(engine,lambda:None)
            self.assertEqual(len(engine.io.calls),index)
            self.assertEqual(engine.rows[0]['samples'][-1]['raw']['raw_unsigned'],1)
        for index in (2,13):
            engine=self.engine();engine.io.goal_at=index
            with self.assertRaises(ValueError):m.run_right_wheel_feedback(engine,lambda:None)
            self.assertEqual(len(engine.io.calls),index)
            self.assertTrue(engine.manual_cut)

    def test_stability_rejects_motion_span_two_bad_speed_and_negative_zero(self):
        for positions in ([1212,1213,1214,1215,1216],[1212,1212,1212,1212,1214],
                          [4095,0,0,0,0],[-1]*5,[4096]*5):
            engine=self.engine();engine.io.positions=positions
            with self.subTest(positions=positions),self.assertRaises((ValueError,OverflowError)):
                m.run_right_wheel_feedback(engine,lambda:None)
            self.assertTrue(engine.manual_cut)
        for velocity in (49,51,-49,-51,0x8000):
            for index in range(5):
                engine=self.engine();engine.io.velocities=[0]*5;engine.io.velocities[index]=velocity
                with self.subTest(velocity=velocity,index=index),self.assertRaises(ValueError):
                    m.run_right_wheel_feedback(engine,lambda:None)
                self.assertEqual(len(engine.io.calls),4+index*2)
                self.assertTrue(engine.manual_cut)

    def test_snapshot_fresh_window_for_zero_and_fifty_and_original_pv_preserved(self):
        for pv in (0,50,-50):
            engine=self.snapshot_engine(pv)
            values,errors=m.snapshot(engine.io,engine=engine)
            self.assertEqual(errors,[]);self.assertEqual(len(engine.io.calls),42)
            self.assertEqual(len(engine.io.reads),102)
            m.validate_snapshot(values,engine=engine)
            result=m.classify_stop_snapshot(values,engine=engine)
            self.assertEqual(result['status'],'PASS')
            self.assertTrue(result['right7_stability_used'])
            self.assertEqual(result['observed']['right:7']['Present_Velocity'],pv)
            if pv:
                with self.assertRaises(ValueError):m.validate_snapshot(values)
                self.assertEqual(m.classify_stop_snapshot(values)['status'],'FAIL')
            original_evidence=engine.right_stop_evidence
            next_values,next_errors=m.snapshot(engine.io,engine=engine)
            self.assertEqual(next_errors,[]);self.assertEqual(len(engine.io.calls),84)
            self.assertIsNot(original_evidence,engine.right_stop_evidence)
            with self.assertRaises(ValueError):m.validate_snapshot(values,engine=engine)
            self.assertEqual(m.classify_stop_snapshot(values,engine=engine)['status'],'UNKNOWN')
            m.validate_snapshot(next_values,engine=engine)
            json.dumps({'before':values,'before_window':original_evidence,
                        'after':next_values,'after_window':engine.right_stop_evidence})

    def test_missing_old_json_or_failed_next_window_cannot_supply_receipt(self):
        for pv in (0,50,-50):
            engine=self.snapshot_engine(pv);values,errors=m.snapshot(engine.io,engine=engine)
            copied=json.loads(json.dumps(values))
            with self.assertRaises(ValueError):m.validate_snapshot(copied,engine=engine)
            self.assertEqual(m.classify_stop_snapshot(copied,engine=engine)['status'],'UNKNOWN')
            receipt=engine.left_stop_receipt
            for invalid in (None,{'status':'PASS','values':values,'evidence':engine.right_stop_evidence}):
                engine.left_stop_receipt=invalid
                with self.assertRaises(ValueError):m.validate_snapshot(values,engine=engine)
                self.assertEqual(m.classify_stop_snapshot(values,engine=engine)['status'],'UNKNOWN')
            engine.left_stop_receipt=receipt
            engine.io.fail_at=engine.io.sequence+1
            after,errors=m.snapshot(engine.io,engine=engine)
            self.assertTrue(errors);self.assertIsNone(engine.left_stop_receipt)
            self.assertEqual(m.classify_stop_snapshot(after,errors,engine=engine)['status'],'UNKNOWN')
            count=len(engine.io.calls);m.snapshot(engine.io,cleanup=True,engine=engine)
            self.assertEqual(len(engine.io.calls),count)

    def test_snapshot_anchor_and_mode_are_not_lost_before_window(self):
        engine=self.snapshot_engine(50);engine.io.positions=[1002]*5
        values,errors=m.snapshot(engine.io,engine=engine)
        self.assertTrue(errors);self.assertEqual(len(engine.io.calls),3)
        self.assertEqual(engine.right_stop_evidence['anchor_raw'],1000)
        self.assertEqual(m.classify_stop_snapshot(values,errors,engine=engine)['status'],'UNKNOWN')
        for register,value in (('Operating_Mode',0),('Torque_Enable',1),('Goal_Velocity',1),
                               ('Present_Velocity',49),('Present_Position',4096)):
            engine=self.snapshot_engine();engine.io.rows['right:7'][register]=value
            values,errors=m.snapshot(engine.io,engine=engine)
            self.assertTrue(errors);self.assertTrue(engine.manual_cut)
            if register!='Present_Position':self.assertEqual(engine.io.calls,[])

    def test_recompute_raw_evidence_instead_of_status_or_pass_boolean(self):
        for change in ('missing','bad_frame','bad_time','bad_offset','wrong_plan','wrong_anchor','error'):
            engine=self.snapshot_engine(50);values,_=m.snapshot(engine.io,engine=engine)
            evidence=engine.right_stop_evidence
            evidence['status']='PASS';evidence['pass']=True
            if change=='missing':evidence['samples'].pop()
            if change=='bad_frame':evidence['samples'][3]['raw']['rx'][-1]^=1
            if change=='bad_time':evidence['finished_ns']=evidence['deadline_ns']+1
            if change=='bad_offset':evidence['samples'][4]['scheduled_offset_ns']=0
            if change=='wrong_plan':evidence['plan_sha256']='0'*64
            if change=='wrong_anchor':evidence['anchor_raw']+=1
            if change=='error':evidence['samples'][-1]['error']='已失败'
            with self.subTest(change=change),self.assertRaises(ValueError):
                m.validate_snapshot(values,engine=engine)
            self.assertEqual(m.classify_stop_snapshot(values,engine=engine)['status'],'UNKNOWN')

    def test_other_motor_nonzero_never_uses_left_wheel_exception(self):
        for key in ('left:7','left:1','right:5'):
            for register in m.STOP_REGS:
                engine=self.snapshot_engine(50);values,_=m.snapshot(engine.io,engine=engine)
                values[key][register]=50
                with self.subTest(key=key,register=register),self.assertRaises(ValueError):
                    m.validate_snapshot(values,engine=engine)
                self.assertEqual(m.classify_stop_snapshot(values,engine=engine)['status'],'FAIL')
        for key in ('right:7','right:8','right:9'):
            for register in ('Torque_Enable','Goal_Velocity'):
                engine=self.snapshot_engine(50);values,_=m.snapshot(engine.io,engine=engine)
                values[key][register]=1
                with self.assertRaises(ValueError):m.validate_snapshot(values,engine=engine)
                self.assertEqual(m.classify_stop_snapshot(values,engine=engine)['status'],'FAIL')

    def test_guard_snapshot_and_final_classifier_agree_and_cleanup_window_is_independent(self):
        with self.guard_patches():
            for pv in (0,50,-50):
                engine=self.snapshot_engine(pv,guard=True)
                before,errors=m.snapshot(engine.io,engine=engine)
                self.assertEqual(errors,[]);m.validate_snapshot(before,engine=engine)
                self.assertEqual(m.classify_stop_snapshot(before,engine=engine)['status'],'PASS')
                engine.right_pulse['observation_deadline_ns']=0
                engine.right_pulse['normal_failed']=True
                after,errors=m.snapshot(engine.io,cleanup=True,engine=engine)
                self.assertEqual(errors,[]);self.assertEqual(len(engine.io.calls),84)
                self.assertEqual(m.classify_stop_snapshot(after,engine=engine)['status'],'PASS')
                self.assertTrue(all(argv[3]=='read-raw' for argv in engine.io.calls))

    def test_guard_only_buffers_exact_left_wheel_values_and_blocks_after_window_failure(self):
        with self.guard_patches():
            for side,motor,register,value in (
                ('right',7,'Present_Velocity',49),('right',7,'Present_Velocity',51),
                ('right',8,'Present_Velocity',50),('right',9,'Present_Velocity',-50),
                ('left',7,'Present_Velocity',50),('left',1,'Present_Velocity',50),
                ('right',7,'Torque_Enable',1),('right',7,'Goal_Velocity',1)):
                engine=self.snapshot_engine(guard=True)
                engine.io.rows[f'{side}:{motor}'][register]=value
                with self.assertRaises(ValueError):engine.io.poke(side,motor,'read',register)
                count=len(engine.io.reads)
                with self.assertRaises(ValueError):engine.io.poke('right',7,'read','Torque_Enable',cleanup=True)
                self.assertEqual(len(engine.io.reads),count);self.assertTrue(engine.manual_cut)
            for fault in ('bad_at','signal_at','fail_at','slow_at'):
                engine=self.snapshot_engine(guard=True)
                setattr(engine.io,fault,91);engine.io.slow_ns=1_500_000_000
                values,errors=m.snapshot(engine.io,engine=engine)
                self.assertTrue(errors);self.assertTrue(engine.right_pulse['uart_blocked'])
                self.assertTrue(engine.manual_cut)
                count=len(engine.io.calls)+len(engine.io.reads)
                with self.assertRaises(ValueError):
                    engine.io.command([m.RIGHT_PULSE_RAW['path'],m.DEVICES['right'],'9','read-raw','Torque_Enable'])
                self.assertEqual(len(engine.io.calls)+len(engine.io.reads),count)

    def test_window_only_allows_four_raw_reads_and_never_writes(self):
        with self.guard_patches():
            for argv in (
                [m.RIGHT_PULSE_WRITE['path'],m.DEVICES['right'],'9','write','Torque_Enable','0'],
                [m.RIGHT_PULSE_RAW['path'],m.DEVICES['right'],'9','sync-zero'],
                [m.RIGHT_PULSE_RAW['path'],m.DEVICES['right'],'8','read-raw','Present_Position'],
                [m.RIGHT_PULSE_RAW['path'],m.DEVICES['right'],'7','read-raw','Goal_Position']):
                engine=self.snapshot_engine(guard=True)
                with self.assertRaises(ValueError):
                    m.collect_left_stop_window(engine,lambda:engine.io.command(argv))
                self.assertEqual(engine.io.calls,[]);self.assertTrue(engine.right_pulse['uart_blocked'])
            engine=self.snapshot_engine(guard=True)
            with self.assertRaises(ValueError):
                engine.io.command([m.RIGHT_PULSE_RAW['path'],m.DEVICES['right'],'7','read-raw','Torque_Enable'])
            self.assertEqual(engine.io.calls,[])

    def test_new_plan_contract_required_and_old_plan_go_cannot_release(self):
        plan=self.plan();go=self.go(plan,100)
        for mutation in ('missing','old12','span','velocity','budget'):
            bad=copy.deepcopy(plan)
            if mutation=='missing':del bad['right7_stop_contract']
            if mutation=='old12':bad['right7_stop_contract']['reads']=12
            if mutation=='span':bad['right7_stop_contract']['position_span_max']=2
            if mutation=='velocity':bad['right7_stop_contract']['present_velocity_raw_unsigned'].append(51)
            if mutation=='budget':bad['right7_stop_contract']['capture_budget_ns']+=1
            with self.subTest(mutation=mutation),self.assertRaises(ValueError):
                m.validate_go(go,bad,200)
        old=copy.deepcopy(plan);del old['right7_stop_contract']
        old_go=self.go(old,100)
        with self.assertRaises(ValueError):m.validate_go(old_go,plan,200)

    def test_wheel_stop_fixed_contract_and_old_or_tampered_plan_go_rejected(self):
        plan=self.plan();go=self.go(plan,100)
        for motor,prefix in ((7,'right'),(8,'front'),(9,'left')):
            key=f'{prefix}{motor}_stop_contract'
            expected=m.left_stop_contract();expected.update(version=f'RIGHT{motor}_STABILITY_V1',
                target=['right',motor,f'base_{prefix}_wheel'])
            self.assertEqual(plan[key],expected)
            for field,value in (('target',['right',6,'other']),('reads',12),
                                ('capture_budget_ns',1_500_000_001),('position_span_max',2),
                                ('present_velocity_raw_unsigned',[0,50,0x8032,51])):
                bad=copy.deepcopy(plan);bad[key][field]=value
                for supplied in (go,self.go(bad,100)):
                    with self.subTest(motor=motor,field=field),self.assertRaises(ValueError):m.validate_go(supplied,bad,200)
            old=copy.deepcopy(plan);del old[key]
            with self.assertRaises(ValueError):m.validate_go(self.go(old,100),old,200)
            with self.assertRaises(ValueError):m.validate_go(self.go(old,100),plan,200)
        for motor in (6,True,'9',10):
            with self.assertRaises(ValueError):m.left_stop_contract(motor=motor)

    def test_wheel_stop_fresh_windows_preserve_zero_and_signed_fifties(self):
        for motor,prefix in m.WHEEL_STOP_TARGETS.items():
            for pv in (0,50,-50):
                engine=self.snapshot_engine(wheel_pv={motor:pv})
                self.wheel_samples(engine,motor,positions=[1000,1001,1000,1001,1000],velocities=[-50,0,50,-50,0])
                before,errors=m.snapshot(engine.io,engine=engine)
                self.assertEqual(errors,[]);m.validate_snapshot(before,engine=engine)
                result=m.classify_stop_snapshot(before,engine=engine)
                self.assertEqual(result['status'],'PASS');self.assertTrue(result[f'{prefix}{motor}_stability_used'])
                self.assertEqual(result['observed'][f'right:{motor}']['Present_Velocity'],pv)
                window=getattr(engine,prefix+'_stop_evidence')
                self.assertEqual(window['anchor_raw'],1000)
                self.assertEqual(window['kind'],f'{prefix}{motor}_stop_stability')
                self.assertEqual([r['register'] for r in window['samples']],
                    ['Torque_Enable','Goal_Velocity']+['Present_Position','Present_Velocity']*5+['Goal_Velocity','Torque_Enable'])
                self.assertEqual([r['raw']['raw_unsigned'] for r in window['samples'] if r['register']=='Present_Velocity'],
                                 [0x8032,0,50,0x8032,0])
                self.assertTrue(all(r['raw']['motor_id']==motor for r in window['samples']))
                self.assertEqual(window['deadline_ns']-window['started_ns'],1_500_000_000)
                receipt=getattr(engine,prefix+'_stop_receipt')
                after,errors=m.snapshot(engine.io,cleanup=True,engine=engine)
                self.assertEqual(errors,[]);self.assertEqual(len(engine.io.calls),84)
                self.assertIsNot(window,getattr(engine,prefix+'_stop_evidence'))
                self.assertIsNot(receipt,getattr(engine,prefix+'_stop_receipt'))
                m.validate_snapshot(after,engine=engine)
                self.assertEqual(m.classify_stop_snapshot(before,engine=engine)['status'],'UNKNOWN')

    def test_wheel_stop_receipt_rejects_copy_cross_motor_and_forged_evidence(self):
        for motor,prefix in m.WHEEL_STOP_TARGETS.items():
            for mutation in ('json_values','json_receipt','cross_receipt','cross_evidence','missing_read','bad_id','wrong_anchor',
                             'wrong_plan','late','wrong_kind'):
                engine=self.snapshot_engine(wheel_pv={motor:50});values,errors=m.snapshot(engine.io,engine=engine)
                self.assertEqual(errors,[])
                evidence=getattr(engine,prefix+'_stop_evidence');other='front' if motor==7 else 'right'
                if mutation=='json_values':values=json.loads(json.dumps(values))
                if mutation=='json_receipt':setattr(engine,prefix+'_stop_receipt',{'values':values,'evidence':evidence,'status':'PASS'})
                if mutation=='cross_receipt':setattr(engine,prefix+'_stop_receipt',getattr(engine,other+'_stop_receipt'))
                if mutation=='cross_evidence':
                    setattr(engine,prefix+'_stop_evidence',getattr(engine,other+'_stop_evidence'))
                    getattr(engine,prefix+'_stop_receipt').evidence=getattr(engine,other+'_stop_evidence')
                if mutation=='missing_read':evidence['samples'].pop()
                if mutation=='bad_id':evidence['samples'][3]['raw']['motor_id']=8 if motor==7 else 7
                if mutation=='wrong_anchor':evidence['anchor_raw']+=1
                if mutation=='wrong_plan':evidence['plan_sha256']='0'*64
                if mutation=='late':evidence['finished_ns']=evidence['deadline_ns']+1
                if mutation=='wrong_kind':evidence['kind']='left9_feedback_diagnostic'
                with self.subTest(motor=motor,mutation=mutation),self.assertRaises(ValueError):m.validate_snapshot(values,engine=engine)
                self.assertEqual(m.classify_stop_snapshot(values,engine=engine)['status'],'UNKNOWN')

    def test_wheel_stop_rejects_span_anchor_wrap_bad_raw_and_nonzero_te_gv(self):
        for motor,prefix in m.WHEEL_STOP_TARGETS.items():
            offset=14*(motor-7)
            for fault,index in (('positions',None),('anchor',None),('wrap',None),('velocity',None),('negative_zero',None),
                                ('wrong_id',None),('fail_at',1),('bad_at',4),('torque_at',1),('goal_at',2),('goal_at',13),('torque_at',14)):
                engine=self.snapshot_engine(wheel_pv={motor:-50})
                if fault=='positions':
                    self.wheel_samples(engine,motor,positions=[1000,1001,1002,1001,1000],velocities=[50,-50,50,-50,0])
                elif fault=='anchor':self.wheel_samples(engine,motor,positions=[1002]*5)
                elif fault=='wrap':
                    engine.io.rows[f'right:{motor}']['Present_Position']=4095
                    self.wheel_samples(engine,motor,positions=[4095,0,0,0,0])
                elif fault=='velocity':self.wheel_samples(engine,motor,velocities=[0,51,0,0,0])
                elif fault=='negative_zero':self.wheel_samples(engine,motor,velocities=[0x8000]*5)
                elif fault=='wrong_id':
                    original=engine.io.command
                    def wrong_id(argv,**kwargs):
                        text=original(argv,**kwargs)
                        if argv[2]==str(motor):
                            raw=json.loads(text);other=8 if motor==7 else 7
                            raw['motor_id']=other;raw['tx'][-2]=other;raw['rx'][2]=other
                            raw['tx'][-1]=(~sum(raw['tx'][2:-1]))&255;raw['rx'][-1]=(~sum(raw['rx'][2:-1]))&255
                            return json.dumps(raw)+'\n'
                        return text
                    engine.io.command=wrong_id
                else:setattr(engine.io,fault,offset+index)
                values,errors=m.snapshot(engine.io,engine=engine)
                self.assertTrue(errors);self.assertTrue(getattr(engine,prefix+'_stop_blocked'));self.assertTrue(engine.manual_cut)
                self.assertIsNone(getattr(engine,prefix+'_stop_receipt'))
                self.assertEqual(m.classify_stop_snapshot(values,errors,engine=engine)['status'],'UNKNOWN')
                reads=len(engine.io.calls)+len(engine.io.reads)
                m.snapshot(engine.io,cleanup=True,engine=engine)
                self.assertEqual(len(engine.io.calls)+len(engine.io.reads),reads)
            for register,value in (('Operating_Mode',0),('Torque_Enable',1),('Goal_Velocity',1),
                                   ('Present_Velocity',49),('Present_Velocity',-51),('Present_Position',4096)):
                engine=self.snapshot_engine();engine.io.rows[f'right:{motor}'][register]=value
                _,errors=m.snapshot(engine.io,engine=engine)
                self.assertTrue(errors);self.assertEqual(len(engine.io.calls),offset)

    def test_wheel_stop_budget_and_go_expiry_block_without_retry_or_tail_reads(self):
        for motor,prefix in m.WHEEL_STOP_TARGETS.items():
            offset=14*(motor-7)
            for index in (offset+1,offset+6,offset+14):
                engine=self.snapshot_engine(wheel_pv={motor:50})
                engine.io.slow_at=index;engine.io.slow_ns=1_500_000_000
                _,errors=m.snapshot(engine.io,engine=engine)
                self.assertTrue(errors);self.assertTrue(getattr(engine,prefix+'_stop_blocked'))
                self.assertEqual(len(engine.io.calls),index)
                self.assertIn('raw',getattr(engine,prefix+'_stop_evidence')['samples'][-1])
            engine=self.snapshot_engine(wheel_pv={motor:-50})
            expiry=(motor-7)*1_060_000_000+10_000_000
            engine.left_stop_go_check=lambda:m.require(engine.io.now()<expiry,'本轮GO到期')
            _,errors=m.snapshot(engine.io,engine=engine)
            self.assertTrue(errors);self.assertEqual(len(engine.io.calls),offset+1)
            self.assertTrue(getattr(engine,prefix+'_stop_blocked'));self.assertTrue(engine.manual_cut)

    def test_wheel_stop_guard_binds_snapshot_and_raw_target_without_widening_sdk(self):
        with self.guard_patches():
            for motor,prefix in m.WHEEL_STOP_TARGETS.items():
                for pv in (50,-50):
                    engine=self.snapshot_engine(guard=True,wheel_pv={motor:pv})
                    values,errors=m.snapshot(engine.io,engine=engine)
                    self.assertEqual(errors,[]);m.validate_snapshot(values,engine=engine)
                    self.assertIsNone(engine.stop_snapshot_motor);self.assertIsNone(engine.stop_window_motor)
                    with self.assertRaises(ValueError):engine.io.poke('right',motor,'read','Present_Velocity',cleanup=True)
                    self.assertTrue(engine.right_pulse['uart_blocked'])
                engine=self.snapshot_engine(guard=True)
                other=8 if motor==7 else 7
                wrong=[m.RIGHT_PULSE_RAW['path'],m.DEVICES['right'],str(other),'read-raw','Torque_Enable']
                with self.assertRaises(ValueError):
                    m.collect_left_stop_window(engine,lambda:engine.io.command(wrong),1000,motor=motor,stop_recheck=True)
                self.assertEqual(engine.io.calls,[]);self.assertTrue(getattr(engine,prefix+'_stop_blocked'))
                self.assertTrue(engine.right_pulse['uart_blocked'])

    def test_front_left_diagnostic_remains_record_only_and_cannot_supply_stop_receipt(self):
        for motor in (8,9):
            engine=self.engine(front=motor==8,left=motor==9);engine.io.positions=[1000,1004,1008,1012,1016]
            engine.io.velocities=[50,-50,51,-51,0]
            key='front_wheel_feedback_only' if motor==8 else 'left_wheel_feedback_only'
            contract=copy.deepcopy(engine.plan[key]);scope=engine.plan['scope']
            m.run_front_wheel_feedback(engine,lambda:None,motor=motor)
            row=engine.rows[0]
            self.assertEqual(row['status'],'DIAGNOSTIC_CAPTURED');self.assertFalse(row['stop_verified'])
            self.assertEqual(row['position_span_raw'],16)
            self.assertEqual(engine.plan[key],contract);self.assertEqual(engine.plan['scope'],scope)
            self.assertFalse(any(hasattr(engine,prefix+'_stop_receipt') for prefix in m.WHEEL_STOP_TARGETS.values()))
            self.assertEqual(m.classify_stop_snapshot(idle_rows(),engine=engine)['status'],'UNKNOWN')
            with self.assertRaises(ValueError):m.validate_left_stop_window(row,engine.plan,None,motor=motor)

    def test_deadline_and_go_expiry_keep_last_frame_then_stop(self):
        for index in (1,5,14):
            engine=self.engine();engine.io.slow_at=index;engine.io.slow_ns=1_500_000_000
            with self.assertRaisesRegex(ValueError,'超窗'):m.run_right_wheel_feedback(engine,lambda:None)
            self.assertEqual(len(engine.io.calls),index)
            self.assertIn('raw',engine.rows[0]['samples'][-1])
        engine=self.engine()
        with self.assertRaisesRegex(ValueError,'GO'):
            m.run_right_wheel_feedback(engine,lambda:m.require(engine.io.now()<1_055_000_000,'夹具GO到期'))
        self.assertEqual(len(engine.io.calls),14)
        self.assertIn('raw',engine.rows[0]['samples'][-1])

    def test_tool_identity_and_prelaunch_expiry_reject_before_uart(self):
        for key in ('sha256','inode','mode','mtime_ns'):
            engine=self.engine();ref=m.left_feedback_tool_ref(engine.plan)
            ref[key]='0'*64 if key=='sha256' else ref[key]+1
            with self.subTest(key=key),self.assertRaises(ValueError):
                m.run_right_wheel_feedback(engine,lambda:None)
            self.assertEqual(engine.io.calls,[])
        engine=self.engine()
        with self.assertRaises(ValueError):
            m.run_right_wheel_feedback(engine,lambda:(_ for _ in ()).throw(ValueError('GO已失效')))
        self.assertEqual(engine.io.calls,[])
        with self.assertRaises(ValueError):
            m.left_feedback_raw(engine.io,self.reference,'Goal_Position',lambda:None)
        self.assertEqual(engine.io.calls,[])
        engine=self.engine()
        with patch.object(m,'verify_ref',side_effect=lambda *a,**k:setattr(engine.io,'t',1_500_000_000)):
            with self.assertRaises(ValueError):m.run_right_wheel_feedback(engine,lambda:None)
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
                m.run_right_wheel_feedback(engine,lambda:None)
            self.assertEqual(len(engine.io.calls),1)
            self.assertTrue(engine.rows[0]['uart_blocked'])

    def test_flag_contract_binding_and_missing_go_have_zero_child(self):
        with patch.object(m.subprocess,'Popen',side_effect=AssertionError('禁止子进程')) as child:
            for args in (['--right-wheel-feedback-only','--right-wheel-pulse'],
                         ['--right-wheel-feedback-only','--position-target','head-1'],
                         ['--right-wheel-feedback-only','--execute']):
                with self.assertRaises(ValueError):run_main(args)
            for key in ('target','reads','capture_budget_ns'):
                plan=self.plan();plan['right_wheel_feedback_only'][key]=0
                with self.assertRaises(ValueError):m.position_targets_from_plan(plan)
            for key in ('refs','binary_paths'):
                plan=self.plan();plan['bindings'][key]=[]
                with self.assertRaises(ValueError):m.position_targets_from_plan(plan)
            with self.assertRaises((ValueError,KeyError)):m.validate_go({},self.plan(),0)
            with contextlib.redirect_stdout(io.StringIO()) as out:
                self.assertEqual(run_main(['--right-wheel-feedback-only']),0)
            self.assertEqual(json.loads(out.getvalue())['actions_started'],0)
            child.assert_not_called()

    def test_go_binds_readonly_scope_and_ten_minute_field_state(self):
        plan=self.plan();go=self.go(plan,100);m.validate_go(go,plan,200)
        for field in ('base_confirmation_scope','continuous_wheel_observation','confirmed_at_ns'):
            bad=copy.deepcopy(go)
            bad['physical']['right'][field]=m.pulse_scope('right') if field=='base_confirmation_scope' else False
            with self.assertRaises(ValueError):m.validate_go(bad,plan,200)
        go['expires_at_ns']=601_000_000_100
        with self.assertRaises(ValueError):m.validate_go(go,plan,200)

    def test_execute_reuses_locks_postchecks_but_never_snapshots_or_writes(self):
        self.check_execute_cases((None,3,'post'))

    def test_final_journal_or_report_failure_revokes_candidate_pass(self):
        self.check_execute_cases(('journal','report'))

    def check_execute_cases(self,faults,*,front=False,left=False):
        for fault in faults:
            root=self.root/str(fault);root.mkdir()
            helper=root/'helper.py';helper.write_text('# 本机空helper\n')
            bindings=self.bindings();bindings['proc_helper']={'path':str(helper)}
            bindings['protection']={}
            for side in m.SERIALS:
                bindings['protection'][side]={}
                for name in ('rx_trust.lock','estop_latch.lock'):
                    path=root/f'{side}-{name}';path.write_bytes(b'unchanged\n')
                    bindings['protection'][side][name]={'path':str(path)}
            executor=FeedbackIO(motor=9 if left else 8 if front else 7);executor.fail_at=fault
            if front or left:executor.positions=[1200,1210,1220,1230,1240];executor.velocities=[100,-250,50,-50,0x8000]
            if fault=='journal':
                class FailedClose(io.StringIO):
                    def close(self):raise OSError('注入日志关闭失败')
                executor.journal=FailedClose()
            write_new=m.write_new
            def write_report(path,value):
                if fault=='report' and Path(path).name=='run.json':
                    raise OSError('注入最终报告保存失败')
                return write_new(path,value)
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
                  patch.object(m,'write_new',side_effect=write_report),
                  patch.object(m.signal,'signal'),contextlib.redirect_stdout(io.StringIO()) as messages):
                plan=m.make_plan(bindings,root/'run','feedback',root,
                                 right_wheel_feedback_only=not (front or left),front_wheel_feedback_only=front,
                                 left_wheel_feedback_only=left)
                go=self.go(plan,m.time.time_ns());approval=root/'go.json';m.write_new(approval,go)
                self.assertEqual(m.execute(plan,go,approval),0 if fault is None else 1)
                snapshot.assert_not_called();motion.assert_not_called();finish.assert_not_called()
                self.assertEqual(process.call_count,1)
            if fault=='report':
                self.assertFalse((root/'run/run.json').exists())
                report=json.loads(messages.getvalue().splitlines()[-1])
                self.assertIn('report_write_error',report)
            else:
                report=json.loads((root/'run/run.json').read_text())
            if fault=='journal':self.assertIn('journal_close_error',report)
            self.assertEqual(report['result'],'DIAGNOSTIC_CAPTURED' if fault is None else 'NEEDS_MANUAL_POWER_CUT')
            verification='left9_feedback_capture' if left else 'front8_feedback_capture' if front else 'right7_candidate_verification'
            self.assertEqual(report[verification]['status'],('DIAGNOSTIC_CAPTURED' if front or left else 'PASS') if fault is None else 'FAIL')
            self.assertEqual(report['stop_verification']['status'],'NOT_EVALUATED_DIAGNOSTIC_ONLY')
            self.assertEqual(report['position_verification']['status'],'NOT_EVALUATED_DIAGNOSTIC_ONLY')
            expected_identity={'post':'FAIL','journal':'UNKNOWN'}.get(fault,'PASS')
            self.assertEqual(report['identity_verification']['status'],expected_identity)
            self.assertEqual(len(report['owned_locks']),4)
            self.assertEqual(len(executor.calls),3 if fault==3 else 14)
            self.assertTrue(Path(str(approval)+'.consumed').exists())
            self.assertFalse(report['deployable'])

    def test_front_fourteen_reads_preserve_all_velocity_words_and_position_span_without_stop_claim(self):
        engine=self.engine(front=True)
        engine.io.positions=[4093,4095,0,100,65535]
        engine.io.velocities=[50,-50,250,0x8000,0xffff]
        m.run_front_wheel_feedback(engine,lambda:None)
        row=engine.rows[0]
        self.assertEqual(row['status'],'DIAGNOSTIC_CAPTURED');self.assertFalse(row['stop_verified'])
        self.assertFalse(row['same_frame']);self.assertFalse(engine.manual_cut)
        self.assertEqual([c[4] for c in engine.io.calls],
                         ['Torque_Enable','Goal_Velocity']+['Present_Position','Present_Velocity']*5+['Goal_Velocity','Torque_Enable'])
        self.assertTrue(all(c[:4]==[self.reference['path'],m.DEVICES['right'],'8','read-raw'] for c in engine.io.calls))
        self.assertEqual([s['raw']['raw_unsigned'] for s in row['samples'] if s['register']=='Present_Velocity'],
                         [50,0x8032,250,0x8000,0xffff])
        self.assertEqual(row['position_span_raw'],65535)
        self.assertEqual([s['started_ns']-row['observation_started_ns'] for s in row['samples']
                          if s['register']=='Present_Position'],[0,250_000_000,500_000_000,750_000_000,1_000_000_000])
        self.assertLessEqual(row['finished_ns']-row['started_ns'],1_500_000_000)
        self.assertFalse(hasattr(engine.io,'front_feedback_before_spawn'))
        m.validate_front_feedback_window(row,engine.plan)

    def test_front_fixed_plan_go_scope_and_final_metadata_cannot_be_forged_or_mixed(self):
        root='/home/loosqk/robot-checks/feedback'
        for options in ({'right_wheel_feedback_only':True},{'right_wheel_pulse':True},
                        {'wheel_pulse_target':'front'},{'zero_stop_check':True}):
            with self.assertRaises(ValueError):m.make_plan(self.bindings(),root+'/run','feedback',root,
                                                         front_wheel_feedback_only=True,**options)
        with self.assertRaises(ValueError):m.make_plan(self.bindings(),root+'/run','feedback',root,['head-1'],front_wheel_feedback_only=True)
        with self.assertRaises(ValueError):m.make_plan(self.bindings(),root+'/run','feedback',root,front_wheel_feedback_only=1)
        plan=self.plan(front=True);go=self.go(plan,100);m.validate_go(go,plan,200)
        for key,value in (('target',['right',7,'base_right_wheel']),('reads',12),('capture_budget_ns',2_000_000_000),
                          ('present_velocity_policy','ZERO_OR_FIFTY'),('position_span_policy','AT_MOST_ONE')):
            bad=copy.deepcopy(plan);bad['front_wheel_feedback_only'][key]=value
            with self.assertRaises(ValueError):m.position_targets_from_plan(bad)
        for key in ('scope','target'):
            bad=copy.deepcopy(go)
            if key=='scope':bad['physical']['right']['base_confirmation_scope']=m.RIGHT_FEEDBACK_SCOPE
            else:bad['plan_sha256']=m.digest(self.plan())
            with self.assertRaises(ValueError):m.validate_go(bad,plan,200)
        with self.assertRaises(ValueError):m.validate_go(self.go(plan,100),plan,600_000_000_101)
        for extra in ('right_wheel_pulse','right_wheel_feedback_only'):
            bad=copy.deepcopy(plan);bad[extra]={}
            with self.assertRaises(ValueError):m.position_targets_from_plan(bad)
        with patch.object(m.subprocess,'Popen',side_effect=AssertionError('不可创建child')) as child:
            for args in (['--front-wheel-feedback-only','--right-wheel-feedback-only'],
                         ['--front-wheel-feedback-only','--right-wheel-pulse'],
                         ['--front-wheel-feedback-only','--wheel-pulse','left'],
                         ['--front-wheel-feedback-only','--position-target','head-1'],
                         ['--front-wheel-feedback-only','--zero-stop-check'],['--front-wheel-feedback-only','--execute']):
                with self.assertRaises(ValueError):run_main(args)
            with contextlib.redirect_stdout(io.StringIO()) as output:self.assertEqual(run_main(['--front-wheel-feedback-only']),0)
            self.assertEqual(json.loads(output.getvalue())['actions_started'],0);child.assert_not_called()
        engine=self.engine(front=True);m.run_front_wheel_feedback(engine,lambda:None)
        for key,value in (('kind','right7_stop_stability'),('same_frame',True),('anchor_raw',100),
                          ('position_span_raw',500),('stop_verified',True),('observation_started_ns',0)):
            bad=copy.deepcopy(engine.rows[0]);bad[key]=value
            with self.assertRaises(ValueError):m.validate_front_feedback_window(bad,engine.plan)

    def test_front_wrong_target_frame_and_tool_identity_fail_without_followup_reads(self):
        for key,value in (('motor_id',7),('motor_id',9),('serial_device',m.DEVICES['left']),('width',True),
                          ('register','Present_Velocity'),('requested_register_address',58),
                          ('tx',[255]),('rx',[255]),('raw_unsigned',True)):
            engine=self.engine(front=True);command=engine.io.command
            def changed(argv,**kwargs):
                raw=json.loads(command(argv,**kwargs));raw[key]=value
                return json.dumps(raw)+'\n'
            engine.io.command=changed
            with self.subTest(key=key,value=value),self.assertRaises(ValueError):m.run_front_wheel_feedback(engine,lambda:None)
            self.assertEqual(len(engine.io.calls),1);self.assertTrue(engine.front_feedback_blocked)
            self.assertTrue(engine.manual_cut);self.assertTrue(engine.rows[0]['uart_blocked'])
            self.assertFalse(hasattr(engine.io,'front_feedback_before_spawn'))
        for key in ('sha256','inode','mode','mtime_ns'):
            engine=self.engine(front=True);ref=m.left_feedback_tool_ref(engine.plan)
            ref[key]='0'*64 if key=='sha256' else ref[key]+1
            with self.assertRaises(ValueError):m.run_front_wheel_feedback(engine,lambda:None)
            self.assertEqual(engine.io.calls,[])

    def test_front_nonzero_te_gv_communication_signal_and_evidence_fail_stickily(self):
        for fault,index in (('torque_at',1),('goal_at',2),('goal_at',13),('torque_at',14),
                            ('fail_at',4),('bad_at',5),('signal_at',6),('evidence',7)):
            engine=self.engine(front=True)
            if fault=='evidence':
                command=engine.io.command
                def broken(argv,**kwargs):
                    text=command(argv,**kwargs)
                    if engine.io.sequence==index:engine.io.evidence_errors.append('原件失败')
                    return text
                engine.io.command=broken
            else:setattr(engine.io,fault,index)
            with self.subTest(fault=fault,index=index),self.assertRaises((ValueError,OSError)):
                m.run_front_wheel_feedback(engine,lambda:None)
            self.assertEqual(len(engine.io.calls),index);self.assertTrue(engine.front_feedback_blocked)
            self.assertTrue(engine.manual_cut);self.assertFalse(engine.rows[0]['stop_verified'])
            count=len(engine.io.calls)
            with self.assertRaises(ValueError):m.run_front_wheel_feedback(engine,lambda:None)
            self.assertEqual(len(engine.io.calls),count)

    def test_front_next_slot_total_deadline_and_go_expiry_never_compress_or_fill_reads(self):
        for fault,index,delay in (('slot',4,250_000_000),('total',14,1_500_000_000),('go',5,0)):
            engine=self.engine(front=True)
            if fault!='go':engine.io.slow_at=index;engine.io.slow_ns=delay
            def gate():
                if fault=='go':m.require(engine.io.sequence<index,'GO已到期')
            with self.assertRaises(ValueError):m.run_front_wheel_feedback(engine,gate)
            self.assertEqual(len(engine.io.calls),index)
            row=engine.rows[0];self.assertIn('raw',row['samples'][-1])
            self.assertEqual(row['samples'][-1]['status'],'READ_VALID')
            if fault=='slot':self.assertLess(row['finished_ns'],row['deadline_ns'])
            self.assertTrue(row['uart_blocked']);self.assertTrue(engine.manual_cut)
        engine=self.engine(front=True)
        with self.assertRaises(ValueError):m.run_front_wheel_feedback(engine,lambda:(_ for _ in ()).throw(ValueError('GO过期')))
        self.assertEqual(engine.io.calls,[])

    def test_front_real_prelaunch_rechecks_after_tempfile_setup_and_preserves_foreign_hook(self):
        engine=self.engine(front=True)
        original=m.tempfile.TemporaryFile
        real=m.RealExecutor(m.RIGHT_PULSE_WRITE,self.root)
        real.now=engine.io.now;real.check=engine.io.check
        self.addCleanup(real.journal.close)
        def command(argv,**kwargs):
            real.front_feedback_before_spawn=engine.io.front_feedback_before_spawn
            return real.command(argv,**kwargs)
        engine.io.command=command
        def delayed(*args,**kwargs):
            stream=original(*args,**kwargs);engine.io.t=1_500_000_000
            return stream
        with patch.object(m.tempfile,'TemporaryFile',delayed),patch.object(m.subprocess,'Popen',side_effect=AssertionError('不应启动UART')) as child:
            with self.assertRaisesRegex(ValueError,'超窗'):m.run_front_wheel_feedback(engine,lambda:None)
            child.assert_not_called()
        self.assertEqual(len(real.commands),1);self.assertIsNone(real.commands[0]['pid'])
        self.assertIsNone(real.child);self.assertEqual(engine.rows[0]['samples'][0]['status'],'UNKNOWN')
        self.assertNotIn('raw',engine.rows[0]['samples'][0]);self.assertFalse(hasattr(engine.io,'front_feedback_before_spawn'))
        engine=self.engine(front=True);foreign=lambda argv:None
        engine.io.front_feedback_before_spawn=foreign
        with self.assertRaises(ValueError):m.run_front_wheel_feedback(engine,lambda:None)
        self.assertIs(engine.io.front_feedback_before_spawn,foreign);self.assertEqual(engine.io.calls,[])

    def test_front_raw_support_does_not_relax_default_right7_target_or_contract(self):
        engine=self.engine(front=True);m.run_front_wheel_feedback(engine,lambda:None)
        raw=engine.rows[0]['samples'][0]['raw']
        self.assertEqual(m.validate_left_feedback_raw(raw,'Torque_Enable',motor=8),0)
        with self.assertRaises(ValueError):m.validate_left_feedback_raw(raw,'Torque_Enable')
        for motor in (6,9,True):
            with self.assertRaises(ValueError):m.validate_left_feedback_raw(raw,'Torque_Enable',motor=motor)
        left=self.engine();left.io.positions=[1212,1212,1214,1212,1212]
        with self.assertRaises(ValueError):m.run_right_wheel_feedback(left,lambda:None)
        self.assertEqual(m.left_stop_contract()['position_span_max'],1)
        self.assertEqual(m.left_stop_contract()['present_velocity_raw_unsigned'],[0,50,0x8032])
        self.assertNotIn('front_wheel_feedback_only',self.plan())

    def test_front_execute_only_fourteen_reads_and_identity_no_stop_in_success_or_failure(self):
        self.check_execute_cases((None,3,'post','journal','report'),front=True)

    def test_left_feedback_fourteen_fixed_reads_keep_signed_fifty_words_and_position_span(self):
        engine=self.engine(left=True)
        engine.io.positions=[948,949,950,951,960];engine.io.velocities=[0,50,-50,300,-250]
        m.run_front_wheel_feedback(engine,lambda:None,motor=9)
        row=engine.rows[0]
        self.assertEqual((row['side'],row['motor'],row['name'],row['kind']),
                         ('right',9,'base_left_wheel','left9_feedback_diagnostic'))
        self.assertEqual(row['status'],'DIAGNOSTIC_CAPTURED');self.assertFalse(row['stop_verified'])
        self.assertEqual([call[4] for call in engine.io.calls],
                         ['Torque_Enable','Goal_Velocity']+['Present_Position','Present_Velocity']*5+['Goal_Velocity','Torque_Enable'])
        self.assertTrue(all(call[:4]==[self.reference['path'],m.DEVICES['right'],'9','read-raw'] for call in engine.io.calls))
        self.assertEqual([sample['raw']['raw_unsigned'] for sample in row['samples'] if sample['register']=='Present_Velocity'],
                         [0,50,0x8032,300,0x80fa])
        self.assertTrue(all(sample['raw']['motor_id']==9 and sample['raw']['rx'][2]==9 for sample in row['samples']))
        self.assertEqual(row['position_span_raw'],12)
        self.assertEqual([sample['started_ns']-row['observation_started_ns'] for sample in row['samples']
                          if sample['register']=='Present_Position'],[0,250_000_000,500_000_000,750_000_000,1_000_000_000])
        self.assertLessEqual(row['finished_ns']-row['started_ns'],1_500_000_000)
        self.assertFalse(engine.manual_cut);self.assertFalse(hasattr(engine.io,'front_feedback_before_spawn'))
        m.validate_front_feedback_window(row,engine.plan,motor=9)
        for motor in (7,8):
            with self.assertRaises(ValueError):m.validate_left_feedback_raw(row['samples'][0]['raw'],'Torque_Enable',motor=motor)

    def test_left_feedback_plan_scope_go_and_fixed_target_reject_mixups_and_missing_authority(self):
        plan=self.plan(left=True);go=self.go(plan,100);m.validate_go(go,plan,200)
        root='/home/loosqk/robot-checks/feedback'
        for options in ({'right_wheel_feedback_only':True},{'front_wheel_feedback_only':True},
                        {'right_wheel_pulse':True},{'wheel_pulse_target':'front'},{'zero_stop_check':True}):
            with self.assertRaises(ValueError):m.make_plan(self.bindings(),root+'/run','feedback',root,left_wheel_feedback_only=True,**options)
        with self.assertRaises(ValueError):m.make_plan(self.bindings(),root+'/run','feedback',root,['head-1'],left_wheel_feedback_only=True)
        with self.assertRaises(ValueError):m.make_plan(self.bindings(),root+'/run','feedback',root,left_wheel_feedback_only=1)
        for field,value in (('target',['right',8,'base_front_wheel']),('reads',12),('capture_budget_ns',2_000_000_000),
                            ('present_velocity_policy','ZERO_OR_FIFTY'),('position_span_policy','AT_MOST_ONE')):
            bad=copy.deepcopy(plan);bad['left_wheel_feedback_only'][field]=value
            with self.assertRaises(ValueError):m.position_targets_from_plan(bad)
        for old in (self.plan(),self.plan(front=True)):
            with self.assertRaises(ValueError):m.validate_go(self.go(old,100),plan,200)
            bad=copy.deepcopy(go);bad['physical']['right']['base_confirmation_scope']=old['scope']
            with self.assertRaises(ValueError):m.validate_go(bad,plan,200)
        for extra in ('right_wheel_feedback_only','front_wheel_feedback_only','right_wheel_pulse'):
            bad=copy.deepcopy(plan);bad[extra]={}
            with self.assertRaises(ValueError):m.position_targets_from_plan(bad)
        with self.assertRaises((ValueError,KeyError)):m.validate_go({},plan,200)
        with patch.object(m.subprocess,'Popen',side_effect=AssertionError('前置不得child')) as child:
            for extra in (['--front-wheel-feedback-only'],['--right-wheel-feedback-only'],['--right-wheel-pulse'],
                          ['--wheel-pulse','front'],['--position-target','head-1'],['--zero-stop-check'],['--execute']):
                with self.assertRaises(ValueError):run_main(['--left-wheel-feedback-only',*extra])
            with contextlib.redirect_stdout(io.StringIO()) as output:self.assertEqual(run_main(['--left-wheel-feedback-only']),0)
            self.assertEqual(json.loads(output.getvalue())['actions_started'],0);child.assert_not_called()
        engine=self.engine(left=True)
        with self.assertRaises(ValueError):m.run_front_wheel_feedback(engine,lambda:None)
        self.assertEqual(engine.io.calls,[])
        front=self.engine(front=True)
        with self.assertRaises(ValueError):m.run_front_wheel_feedback(front,lambda:None,motor=9)
        self.assertEqual(front.io.calls,[])

    def test_left_feedback_wrong_id_bad_frame_te_goal_and_io_errors_block_further_reads(self):
        for fault,index in (('argv',0),('response',1),('bad_at',4),('torque_at',1),('goal_at',2),
                            ('goal_at',13),('torque_at',14),('fail_at',6),('signal_at',9)):
            engine=self.engine(left=True)
            if fault in ('argv','response'):
                command=engine.io.command
                def changed(argv,**kwargs):
                    if fault=='argv':
                        args=list(argv);args[2]='8';return command(args,**kwargs)
                    raw=json.loads(command(argv,**kwargs));raw['motor_id']=8
                    return json.dumps(raw)+'\n'
                engine.io.command=changed
            else:setattr(engine.io,fault,index)
            with self.subTest(fault=fault),self.assertRaises((ValueError,OSError)):
                m.run_front_wheel_feedback(engine,lambda:None,motor=9)
            self.assertEqual(len(engine.io.calls),index);self.assertTrue(engine.front_feedback_blocked)
            self.assertTrue(engine.manual_cut);self.assertFalse(engine.rows[0]['stop_verified'])
            count=len(engine.io.calls)
            with self.assertRaises(ValueError):m.run_front_wheel_feedback(engine,lambda:None,motor=9)
            self.assertEqual(len(engine.io.calls),count);self.assertFalse(hasattr(engine.io,'front_feedback_before_spawn'))

    def test_left_feedback_next_slot_total_budget_and_go_expiry_never_fill_missing_reads(self):
        for fault,index,delay in (('slot',4,250_000_000),('total',14,1_500_000_000),('go',5,0)):
            engine=self.engine(left=True)
            if fault!='go':engine.io.slow_at=index;engine.io.slow_ns=delay
            def gate():
                if fault=='go':m.require(engine.io.sequence<index,'GO已到期')
            with self.assertRaises(ValueError):m.run_front_wheel_feedback(engine,gate,motor=9)
            self.assertEqual(len(engine.io.calls),index)
            row=engine.rows[0];self.assertEqual(row['samples'][-1]['status'],'READ_VALID')
            self.assertIn('raw',row['samples'][-1]);self.assertTrue(row['uart_blocked'])
            if fault=='slot':self.assertLess(row['finished_ns'],row['deadline_ns'])
            self.assertTrue(engine.manual_cut)
        engine=self.engine(left=True)
        with self.assertRaises(ValueError):m.run_front_wheel_feedback(engine,lambda:(_ for _ in ()).throw(ValueError('GO过期')),motor=9)
        self.assertEqual(engine.io.calls,[])

    def test_left_feedback_execute_has_only_fourteen_reads_locks_and_nonuart_finish(self):
        self.check_execute_cases((None,3,'post','journal','report'),left=True)


class ArmRangeIO(FeedbackIO):
    """复用轮诊断假端点，只为固定六臂提供逐ID原帧。"""
    def __init__(self):
        super().__init__()
        self.arm_positions=[1000,2000,2992,2000,2000,1371]
        self.velocities=[0];self.value_at={}
    def command(self,argv,**kwargs):
        if getattr(self,'arm_range_read_only',False):m.RealExecutor.check_arm_range_command(self,argv)
        if argv[0]==m.RIGHT_PULSE_RAW['path']:
            before=getattr(self,'arm_range_before_spawn',None)
            if before is not None:before(argv)
            self.motor=int(argv[2]);self.positions=[self.arm_positions[self.motor-1]]
        text=super().command(argv,**kwargs)
        if self.sequence in self.value_at and argv[0]==m.RIGHT_PULSE_RAW['path']:
            row=json.loads(text);value=self.value_at[self.sequence];width=row['width']
            row['raw_unsigned']=value
            row['rx']=[255,255,self.motor,width+2,0,*value.to_bytes(width,'little')]
            row['rx'].append((~sum(row['rx'][2:]))&255)
            return json.dumps(row)+'\n'
        return text


class ArmRangeTests(unittest.TestCase):
    def setUp(self):
        WheelFeedbackTests.setUp(self)
        self.calibration={name:{'id':motor,'drive_mode':0,'homing_offset':0,'range_min':0,'range_max':4095}
                          for motor,name in enumerate(m.MIXED_ARM_NAMES,1)}
        self.calibration[m.MIXED_ARM_NAMES[2]]['range_max']=2991
        self.calibration[m.MIXED_ARM_NAMES[5]]['range_min']=1384
        self.calibration_path=self.root/'right_arm.json'
        self.calibration_path.write_text(json.dumps(self.calibration));info=self.calibration_path.stat()
        self.calibration_ref={'path':str(self.calibration_path),
            'sha256':hashlib.sha256(self.calibration_path.read_bytes()).hexdigest(),'bytes':info.st_size,
            'uid':info.st_uid,'gid':info.st_gid,'mode':stat.S_IMODE(info.st_mode),'device':info.st_dev,
            'inode':info.st_ino,'mtime_ns':info.st_mtime_ns,'ctime_ns':info.st_ctime_ns,'nlink':1,'symlink':False}

    def bindings(self):
        bindings=WheelFeedbackTests.bindings(self)
        bindings['refs'].append(dict(self.calibration_ref))
        bindings['bus2_mixed_raw_hold']={'arm_calibration':dict(self.calibration_ref)}
        return bindings

    def plan(self,bindings=None,root=None,arm_range_id=None):
        root=root or '/home/loosqk/robot-checks/arm-range'
        return m.make_plan(bindings or self.bindings(),str(root)+'/run','arm-range',root,
                           bus2_arm_range_only=True,arm_range_id=arm_range_id)

    def go(self,plan,now):
        go=WheelFeedbackTests.go(self,plan,now)
        go['physical']['right']={**{key:True for key in m.FACTS},'confirmed_at_ns':now,
            'arm_range_confirmation_scope':plan['bus2_arm_range_only']['scope'],'arm_range_confirmation_answer':'夹具范围只读'}
        return go

    def engine(self,arm_range_id=None):return m.ProbeEngine(ArmRangeIO(),self.plan(arm_range_id=arm_range_id))

    def test_single_elbow_reads_only_three_fields_with_physical_three_calibration_and_bound_go(self):
        engine=self.engine(3);plan=engine.plan;contract=plan['bus2_arm_range_only']
        self.assertEqual(contract['targets'],[['right',3,m.MIXED_ARM_NAMES[2]]])
        self.assertEqual((contract['reads'],contract['scope']),(3,m.ARM_ELBOW_RANGE_SCOPE))
        go=self.go(plan,100);m.validate_go(go,plan,200)
        wrong=copy.deepcopy(go);wrong['physical']['right']['arm_range_confirmation_scope']=m.ARM_RANGE_SCOPE
        with self.assertRaises(ValueError):m.validate_go(wrong,plan,200)
        with self.assertRaises(ValueError):m.validate_go(self.go(self.plan(),100),plan,200)
        m.run_bus2_arm_range_only(engine,lambda:None);row=engine.rows[0]
        self.assertEqual([(call[2],call[3],call[4]) for call in engine.io.calls],
                         [('3','read-raw',register) for register in m.ARM_RANGE_REGS])
        self.assertEqual(row['status'],'DIAGNOSTIC_CAPTURED')
        self.assertEqual((row['ranges'][0]['motor'],row['ranges'][0]['position_raw'],row['ranges'][0]['range_max'],
                          row['ranges'][0]['status']),(3,2992,2991,'OUTSIDE'))
        self.assertEqual(row['register_writes'],0);self.assertFalse(row['mixed_anchor_permitted'])
        m.validate_arm_range_capture(row,plan)
        with self.assertRaises(ValueError):m.validate_arm_range_capture(row,self.plan())

    def test_single_elbow_selection_is_only_three_and_requires_existing_readonly_mode(self):
        with patch.object(m.subprocess,'Popen',side_effect=AssertionError('禁止子进程')) as child:
            for args in (['--arm-range-id','3'],['--bus2-arm-range-only','--arm-range-id','2'],
                         ['--bus2-arm-range-only','--arm-range-id','3','--right-gripper-close']):
                with self.subTest(args=args),contextlib.redirect_stderr(io.StringIO()),self.assertRaises((ValueError,SystemExit)):
                    run_main(args)
            with contextlib.redirect_stdout(io.StringIO()) as output:
                self.assertEqual(run_main(['--bus2-arm-range-only','--arm-range-id','3']),0)
            self.assertEqual(json.loads(output.getvalue())['scope'],m.ARM_ELBOW_RANGE_SCOPE)
            child.assert_not_called()
        for motor in (True,1,2,4,3.0):
            with self.subTest(motor=motor),self.assertRaises(ValueError):self.plan(arm_range_id=motor)

    def test_range_diagnostic_distinguishes_original_and_tolerated_boundaries_without_changing_raw(self):
        self.addCleanup(m.configure_bus_mapping,BUS_FIXTURE_PATH,LEFT_BUS_FIXTURE_PATH)
        path=write_bus_fixture(self.root/'tolerant-bus2.toml',tolerances=[20]*6+[0]*3)
        m.configure_bus_mapping(path,LEFT_BUS_FIXTURE_PATH)
        for motor,value,status in ((3,3011,'WITHIN_TOLERANCE'),(3,3012,'OUTSIDE'),
                                   (6,1364,'WITHIN_TOLERANCE'),(6,1363,'OUTSIDE')):
            engine=self.engine(3 if motor==3 else None);engine.io.arm_positions[motor-1]=value
            with self.subTest(motor=motor,value=value):
                row=m.run_bus2_arm_range_only(engine,lambda:None)
                observed=next(item for item in row['ranges'] if item['motor']==motor)
                self.assertEqual((observed['position_raw'],observed['status']),(value,status))
                self.assertEqual((observed['range_min'],observed['range_max']),
                                 (self.calibration[m.MIXED_ARM_NAMES[motor-1]]['range_min'],
                                  self.calibration[m.MIXED_ARM_NAMES[motor-1]]['range_max']))
                m.validate_arm_range_capture(row,engine.plan)

    def test_eighteen_sync_reads_preserve_frames_and_outside_three_six_without_clamp(self):
        engine=self.engine();m.run_bus2_arm_range_only(engine,lambda:None)
        row=engine.rows[0]
        self.assertEqual(row['status'],'DIAGNOSTIC_CAPTURED');self.assertEqual(row['range_status'],'OUTSIDE')
        self.assertEqual([value['status'] for value in row['ranges']],['INSIDE','INSIDE','OUTSIDE','INSIDE','INSIDE','OUTSIDE'])
        self.assertEqual([value['position_raw'] for value in row['ranges']],[1000,2000,2992,2000,2000,1371])
        self.assertEqual([(call[2],call[4]) for call in engine.io.calls],
                         [(str(motor),register) for motor in range(1,7) for register in m.ARM_RANGE_REGS])
        self.assertTrue(all(call[:2]==[self.reference['path'],m.DEVICES['right']] and call[3]=='read-raw'
                            for call in engine.io.calls))
        self.assertTrue(all(sample['raw']['operation']=='sync_read' and sample['raw']['tx'][4]==0x82
                            and sample['finished_ns']>sample['started_ns'] for sample in row['samples']))
        self.assertFalse(row['stop_verified']);self.assertFalse(row['mixed_anchor_permitted'])
        self.assertFalse(engine.manual_cut);m.validate_arm_range_capture(row,engine.plan)
        for motor in range(1,7):
            with self.assertRaises(ValueError):m.left_feedback_raw(engine.io,self.reference,'Present_Position',lambda:None,motor=motor)
        self.assertEqual(len(engine.io.calls),18)

    def test_each_communication_frame_or_signal_failure_stops_without_tail_reads_or_writes(self):
        for fault in ('fail_at','bad_at','signal_at'):
            for index in range(1,19):
                engine=self.engine();setattr(engine.io,fault,index)
                with self.subTest(fault=fault,index=index),self.assertRaises((ValueError,OSError)):
                    m.run_bus2_arm_range_only(engine,lambda:None)
                self.assertEqual(len(engine.io.calls),index)
                self.assertTrue(engine.io.arm_range_uart_blocked);self.assertTrue(engine.manual_cut)
                self.assertEqual(engine.rows[0]['status'],'FAIL')
                with self.assertRaises(ValueError):m.run_bus2_arm_range_only(engine,lambda:None)
                self.assertEqual(len(engine.io.calls),index)
                self.assertTrue(all(call[3]=='read-raw' for call in engine.io.calls))

    def test_nonzero_te_pv_and_invalid_single_turn_pp_keep_raw_and_stop_immediately(self):
        for index in range(1,19):
            if index%3==0:values=(4096,65535)
            elif index%3==1:values=(1,)
            else:values=(50,0x8032,0x8000)
            for value in values:
                engine=self.engine();engine.io.value_at[index]=value
                with self.subTest(index=index,value=value),self.assertRaises(ValueError):
                    m.run_bus2_arm_range_only(engine,lambda:None)
                self.assertEqual(len(engine.io.calls),index)
                self.assertEqual(engine.rows[0]['samples'][-1]['raw']['raw_unsigned'],value)

    def test_go_scope_ten_minute_expiry_and_midcapture_expiry_refuse_later_uart(self):
        plan=self.plan();go=self.go(plan,100);m.validate_go(go,plan,200)
        for mutation in ('scope','answer','ttl','expired','field_age'):
            bad=copy.deepcopy(go);now=200
            if mutation=='scope':bad['physical']['right']['arm_range_confirmation_scope']=m.MIXED_RAW_SCOPE
            if mutation=='answer':bad['physical']['right']['arm_range_confirmation_answer']=''
            if mutation=='ttl':bad['expires_at_ns']=600_000_000_101
            if mutation=='expired':now=bad['expires_at_ns']
            if mutation=='field_age':
                bad['issued_at_ns']=600_000_000_101;bad['expires_at_ns']=bad['issued_at_ns']+100;now=bad['issued_at_ns']
            with self.subTest(mutation=mutation),self.assertRaises(ValueError):m.validate_go(bad,plan,now)
        for index in (0,1,8,18):
            engine=self.engine()
            with self.subTest(index=index),self.assertRaisesRegex(ValueError,'GO'):
                m.run_bus2_arm_range_only(engine,lambda:m.require(engine.io.sequence<index,'GO过期'))
            self.assertEqual(len(engine.io.calls),index)
            if index:self.assertIn('raw',engine.rows[0]['samples'][-1])

    def test_calibration_content_and_metadata_change_invalidate_plan_go_before_uart(self):
        for mutation in ('content','mode','inode'):
            info=self.calibration_path.stat()
            self.calibration_ref.update(mode=stat.S_IMODE(info.st_mode),inode=info.st_ino,
                mtime_ns=info.st_mtime_ns,ctime_ns=info.st_ctime_ns)
            plan=self.plan();go=self.go(plan,100)
            raw=self.calibration_path.read_bytes();mode=self.calibration_path.stat().st_mode
            if mutation=='content':self.calibration_path.write_bytes(raw+b' ')
            if mutation=='mode':self.calibration_path.chmod(0o600)
            if mutation=='inode':
                replacement=self.root/'replacement.json';replacement.write_bytes(raw);replacement.replace(self.calibration_path)
            with self.subTest(mutation=mutation),self.assertRaises(ValueError):m.validate_go(go,plan,200)
            engine=m.ProbeEngine(ArmRangeIO(),plan)
            with self.assertRaises(ValueError):m.run_bus2_arm_range_only(engine,lambda:None)
            self.assertEqual(engine.io.calls,[])
            self.calibration_path.write_bytes(raw);self.calibration_path.chmod(stat.S_IMODE(mode))

    def test_calibration_refs_allow_existing_valid_metadata_but_require_each_bound_field(self):
        bindings=self.bindings();bindings['refs'][-1]['valid']=True
        plan=self.plan(bindings)
        self.assertEqual(m.arm_calibration_ref(bindings),self.calibration_ref)
        self.assertEqual(m.verify_arm_calibration(plan),m.mixed_calibration_rows(self.calibration))
        for field,value in self.calibration_ref.items():
            bad=copy.deepcopy(bindings)
            bad['refs'][-1][field]=(not value if type(value) is bool else
                                     value+1 if type(value) is int else value+'x')
            with self.subTest(field=field),self.assertRaisesRegex(ValueError,'未进入完整refs'):
                self.plan(bad)

    def test_all_action_and_wheel_modes_and_contract_mutation_are_rejected(self):
        root='/home/loosqk/robot-checks/arm-range'
        for options in ({'right_wheel_pulse':True},{'wheel_pulse_target':'front'},{'zero_stop_check':True},
                        {'right_wheel_feedback_only':True},{'front_wheel_feedback_only':True},
                        {'left_wheel_feedback_only':True},{'bus2_mixed_raw_hold':True}):
            with self.subTest(options=options),self.assertRaises(ValueError):
                m.make_plan(self.bindings(),root+'/run','arm-range',root,bus2_arm_range_only=True,**options)
        with self.assertRaises(ValueError):
            m.make_plan(self.bindings(),root+'/run','arm-range',root,['head-1'],bus2_arm_range_only=True)
        with patch.object(m.subprocess,'Popen',side_effect=AssertionError('禁止子进程')) as child:
            for flag in ('--right-wheel-pulse','--right-wheel-feedback-only','--front-wheel-feedback-only',
                         '--left-wheel-feedback-only','--bus2-mixed-raw-hold','--zero-stop-check','--execute'):
                with self.assertRaises(ValueError):run_main(['--bus2-arm-range-only',flag])
            with self.assertRaises(ValueError):run_main(['--bus2-arm-range-only','--position-target','head-1'])
            with contextlib.redirect_stdout(io.StringIO()) as output:self.assertEqual(run_main(['--bus2-arm-range-only']),0)
            self.assertEqual(json.loads(output.getvalue())['actions_started'],0);child.assert_not_called()
        for field,value in (('reads',17),('register_writes',1),('mixed_anchor_permitted',True),('operation','read')):
            plan=self.plan();plan['bus2_arm_range_only'][field]=value
            with self.assertRaises(ValueError):m.position_targets_from_plan(plan)
        for key in ('right_wheel_pulse','right_wheel_feedback_only','front_wheel_feedback_only',
                    'left_wheel_feedback_only','bus2_mixed_raw_hold'):
            plan=self.plan();plan[key]={}
            with self.assertRaises(ValueError):m.position_targets_from_plan(plan)

    def test_real_executor_readonly_boundary_blocks_write_enable_and_outside_targets_before_child(self):
        directory=self.root/'executor';directory.mkdir()
        executor=m.RealExecutor(dict(m.RIGHT_PULSE_WRITE),directory);self.addCleanup(executor.journal.close)
        executor.arm_range_read_only=True
        with patch.object(m.subprocess,'Popen',side_effect=AssertionError('禁止子进程')) as child:
            for side,motor,register,value in (('right',1,'Torque_Enable',1),('right',1,'Goal_Position',1000),
                                              ('right',9,'Goal_Velocity',0),('left',1,'Torque_Enable',0)):
                with self.assertRaises(ValueError):executor.poke(side,motor,'write',register,value,cleanup=True)
            for argv in ([self.reference['path'],m.DEVICES['right'],'1','write','Torque_Enable','1'],
                         [self.reference['path'],m.DEVICES['right'],'7','read-raw','Torque_Enable'],
                         [self.reference['path'],m.DEVICES['right'],'1','read-raw','Goal_Velocity'],
                         [self.reference['path'],m.DEVICES['left'],'1','read-raw','Torque_Enable']):
                with self.assertRaises(ValueError):executor.command(argv,cleanup=True)
            executor.arm_range_motor_ids=(3,)
            executor.check_arm_range_command([self.reference['path'],m.DEVICES['right'],'3','read-raw','Torque_Enable'])
            for motor in (1,2,4,6):
                with self.subTest(motor=motor),self.assertRaises(ValueError):
                    executor.command([self.reference['path'],m.DEVICES['right'],str(motor),'read-raw','Torque_Enable'])
            child.assert_not_called()
        self.assertEqual(executor.sequence,0)

    def test_execute_dispatch_locks_one_shot_go_and_nonuart_finish_in_success_and_failure(self):
        for fault,arm_range_id in [(fault,None) for fault in (None,3,'post','journal','report','expired')]+[(None,3),(3,3)]:
            root=self.root/f'{fault}-{arm_range_id}';root.mkdir();helper=root/'helper.py';helper.write_text('# 本机空helper\n')
            bindings=self.bindings();bindings['proc_helper']={'path':str(helper),
                'sha256':hashlib.sha256(helper.read_bytes()).hexdigest(),'bytes':helper.stat().st_size}
            bindings['protection']={}
            for side in m.SERIALS:
                bindings['protection'][side]={}
                for name in ('rx_trust.lock','estop_latch.lock'):
                    path=root/f'{side}-{name}';path.write_bytes(b'unchanged\n')
                    bindings['protection'][side][name]={'path':str(path)}
            executor=ArmRangeIO();executor.fail_at=fault
            if fault=='journal':
                class FailedClose(io.StringIO):
                    def close(self):raise OSError('注入日志关闭失败')
                executor.journal=FailedClose()
            write_new=m.write_new
            def write_report(path,value):
                if fault=='report' and Path(path).name=='run.json':raise OSError('注入最终报告保存失败')
                return write_new(path,value)
            read_text=Path.read_text
            def read_boot(path,*args,**kwargs):
                return 'unit-boot' if str(path)=='/proc/sys/kernel/random/boot_id' else read_text(path,*args,**kwargs)
            with contextlib.ExitStack() as stack,contextlib.redirect_stdout(io.StringIO()) as messages:
                for target,name,kwargs in (
                        (m,'MAINTENANCE_NAMESPACE',{'new':self.root}),(m.sys,'platform',{'new':'linux'}),
                        (m.os,'uname',{'return_value':SimpleNamespace(machine='aarch64',nodename='unit-host')}),
                        (Path,'read_text',{'new':read_boot}),(m,'RealExecutor',{'return_value':executor}),
                        (m,'protected_and_refs',{'return_value':None}),(m,'verify_protection_file',{'return_value':None}),
                        (m,'idle',{'return_value':None}),(m,'usb_identity',{'return_value':None}),
                        (m,'inspect_unit',{'return_value':None}),(m,'inspect_holder',{'return_value':None}),
                        (m,'write_new',{'side_effect':write_report}),(m.signal,'signal',{})):
                    stack.enter_context(patch.object(target,name,**kwargs))
                process=stack.enter_context(patch.object(m,'inspect_processes',
                    return_value=None,side_effect=ValueError('后验身份未知') if fault=='post' else None))
                pulse=stack.enter_context(patch.object(m,'load_right_pulse_helper',side_effect=AssertionError('不可依赖mixed新ELF/strace')))
                snapshot=stack.enter_context(patch.object(m,'snapshot',side_effect=AssertionError('不可整机snapshot')))
                motion=stack.enter_context(patch.object(m,'run_motion_probes',side_effect=AssertionError('不可动作')))
                finish=stack.enter_context(patch.object(m,'finish_verification',side_effect=AssertionError('不可STOP收尾')))
                safe_off=stack.enter_context(patch.object(m.ProbeEngine,'safe_off',side_effect=AssertionError('不可safe_off')))
                plan=self.plan(bindings,root,arm_range_id);go=self.go(plan,m.time.time_ns());approval=root/'go.json'
                if fault=='expired':go['expires_at_ns']=go['issued_at_ns']
                m.write_new(approval,go)
                if fault=='expired':
                    with self.assertRaises(ValueError):m.execute(plan,go,approval)
                    self.assertEqual(executor.calls,[]);self.assertFalse(Path(str(approval)+'.consumed').exists());continue
                self.assertEqual(m.execute(plan,go,approval),0 if fault is None else 1)
                with self.assertRaises(ValueError):m.execute(plan,go,approval)
                snapshot.assert_not_called();motion.assert_not_called();finish.assert_not_called();safe_off.assert_not_called();pulse.assert_not_called()
                self.assertEqual(process.call_count,1)
            report=json.loads(messages.getvalue().splitlines()[-1]) if fault=='report' else json.loads((root/'run/run.json').read_text())
            self.assertEqual(report['result'],'DIAGNOSTIC_CAPTURED' if fault is None else 'NEEDS_MANUAL_POWER_CUT')
            self.assertEqual(report['arm_range_capture']['status'],'DIAGNOSTIC_CAPTURED' if fault is None else 'FAIL')
            if fault is None:self.assertEqual(report['arm_range_capture']['range_status'],'OUTSIDE')
            self.assertEqual(report['stop_verification']['status'],'NOT_EVALUATED_DIAGNOSTIC_ONLY')
            self.assertEqual(report['position_verification']['status'],'NOT_EVALUATED_DIAGNOSTIC_ONLY')
            self.assertEqual(report['identity_verification']['status'],{'post':'FAIL','journal':'UNKNOWN'}.get(fault,'PASS'))
            self.assertEqual(len(report['owned_locks']),4);self.assertEqual(len(executor.calls),3 if fault==3 or arm_range_id==3 else 18)
            if arm_range_id==3:
                self.assertEqual([(call[2],call[3],call[4]) for call in executor.calls],
                                 [('3','read-raw',register) for register in m.ARM_RANGE_REGS])
            self.assertTrue(Path(str(approval)+'.consumed').exists());self.assertFalse(report['deployable'])


class MixedRawTests(unittest.TestCase):
    def setUp(self):
        directory=tempfile.TemporaryDirectory();self.addCleanup(directory.cleanup)
        self.directory=Path(directory.name);tool=feedback_reference(self.directory/'mixed-poke')
        self.calibration={name:{'id':motor,'drive_mode':0,'homing_offset':0,'range_min':0,'range_max':4095}
                          for motor,name in enumerate(m.MIXED_ARM_NAMES,1)}
        path=self.directory/'right_arm.json';path.write_text(json.dumps(self.calibration));info=path.stat()
        calibration={'path':str(path),'sha256':hashlib.sha256(path.read_bytes()).hexdigest(),'bytes':path.stat().st_size,
            'uid':info.st_uid,'gid':info.st_gid,'mode':info.st_mode&0o7777,'device':info.st_dev,
            'inode':info.st_ino,'mtime_ns':info.st_mtime_ns,'ctime_ns':info.st_ctime_ns,'nlink':1,'symlink':False}
        self.bindings=RightPulseTests.bindings(self)
        self.bindings['refs'] += [tool,calibration];self.bindings['binary_paths'].append(tool['path'])
        self.bindings['bus2_mixed_raw_hold']={'tool':tool,'arm_calibration':calibration}
        self.root='/home/loosqk/robot-checks/mixed-fixture';self.expiry=600_000_000_000
        self.plan=m.make_plan(self.bindings,self.root+'/run','mixed-fixture',self.root,bus2_mixed_raw_hold=True)

    def receipt(self):
        selected=self.plan['bus2_mixed_raw_hold']['selected_motor_id']
        first=1_100_000_000;zero=first+20_000_000_000;zero_done=zero+1_000_000
        cycles=[{'started_ns':first+index*50_000_000+1_000_000,
                 'finished_ns':first+index*50_000_000+4_000_000,
                 'source_deadline_ns':first+(index+1)*50_000_000} for index in range(400)]
        cycles[0]={'started_ns':first-1_000_000,'finished_ns':first+2_000_000,
                   'source_deadline_ns':first+49_000_000}
        return {**({'schema':'FEETECH_MIXED_WHEEL9_V1','operation':'mixed-hold-wheel9'} if selected==9 else
                   {'schema':'FEETECH_MIXED_WHEEL_V1','operation':'mixed-hold-wheel','selected_motor_id':selected}),
            'result':'PASS','error':None,
            'calibration_range_tolerances_raw':list(self.plan['bus2_mixed_raw_hold']['calibration_range_tolerances_raw']),
            'serial_device':m.DEVICES['right'],
            'arm_calibration_path':self.bindings['bus2_mixed_raw_hold']['arm_calibration']['path'],
            'go_expires_at_unix_ns':self.expiry,'motor_ids':self.plan['bus2_mixed_raw_hold']['motor_ids'],
            'wheel_ids':m.wheel_ids(),'wheel_raw_goal':[500 if motor==selected else 0 for motor in m.wheel_ids()],'pulse_duration_ns':20_000_000_000,
            'zero_window_ns':500_000_000,'enable_budget_ns':21_000_000_000,'cycle_count':400,
            'loaded_calibration':list(self.calibration.values()),
            'arm_anchors':[{'motor_id':motor,'raw':1000,'goal_raw':1000,'calibration_min':0,
                'calibration_max':4095,'sampled_ns':first-50_000_000} for motor in range(1,7)],
            'wheel_anchors':[{'motor_id':motor,'raw':1000,'sampled_ns':first-20_000_000} for motor in m.wheel_ids() if motor!=selected],
            'arm_final':[{'motor_id':motor,'raw':1000} for motor in range(1,7)],'cycles':cycles,
            'wheel_samples':[{'started_ns':cycles[index*5]['started_ns']+1_000_000,
                'finished_ns':cycles[index*5]['started_ns']+2_000_000,
                'position_raw':[(4000+index*5)%4096 if motor==selected else 1000 for motor in m.wheel_ids()],
                'velocity_word':[500 if motor==selected else 0 for motor in m.wheel_ids()],
                'arm_position_raw':[1000]*6} for index in range(1,80)],
            'zero_samples':[{'started_ns':zero_done+offset,'finished_ns':zero_done+offset+1_000_000,
                'torque_enable':1,'goal_velocity_word':0,'present_velocity_word':word}
                for offset,word in zip((50_000_000,250_000_000,450_000_000),(400,50,0))],
            'timing':{'enable_attempt_ns':1_000_000_000,'first_nonzero_ns':first,'zero_started_ns':zero,
                'zero_finished_ns':zero_done,'disable_attempt_ns':zero_done+452_000_000,
                'disable_finished_ns':zero_done+462_000_000,'finished_ns':zero_done+472_000_000},
            'cleanup':{'zero_attempted':True,'zero_ok':True,'disable_attempted':True,'disable_ok':True,
                       'torque_enable_after':[0]*9}}

    def parse(self,row):
        return m.parse_mixed_raw_result(json.dumps(row)+'\n',self.plan,self.expiry,self.calibration)

    def failed_receipt(self):
        # 对应启用前校准拒绝的真实回执形状；没有成功周期或启用时间戳。
        row=self.receipt();row.update(result='FAIL',error='invalid parameter: feetech calibration',cycle_count=0)
        for key in ('arm_anchors','arm_final','wheel_anchors','cycles','wheel_samples','zero_samples'):
            row[key]=[]
        row['timing']={'enable_attempt_ns':None,'first_nonzero_ns':None,'zero_started_ns':None,
            'zero_finished_ns':None,'disable_attempt_ns':2_000_000_000,
            'disable_finished_ns':2_001_000_000,'finished_ns':2_002_000_000}
        row['cleanup']={'zero_attempted':False,'zero_ok':False,'disable_attempted':True,
                        'disable_ok':True,'torque_enable_after':None}
        return row

    def perform(self,row=None,*,fail=False):
        executor=RightPulseIO();executor.output=self.directory
        data=row if row is not None else self.receipt()
        def command(argv,**kwargs):
            kwargs['mixed_stop_check']();executor.sequence+=1
            exit_code=0 if data['result']=='PASS' else 1
            executor.commands.append({'argv':list(argv),'reaped':True,'exit_code':exit_code})
            executor.calls.append(list(argv))
            Path(argv[argv.index('-o')+1]).write_text(f'fixture only\n+++ exited with {exit_code} +++\n')
            return json.dumps(data)+'\n'
        executor.command=command
        engine=m.ProbeEngine(executor,self.plan)
        m.install_right_pulse_guard(engine,SimpleNamespace(install_stop_wire_capture=lambda _module:None))
        engine.right_pulse_go_check=lambda:None;engine.go_expires_at_unix_ns=self.expiry
        verify=m.verify_ref
        def reference(ref,**kwargs):
            return b'' if ref['path']==m.RIGHT_PULSE_TRACE['path'] else verify(ref,**kwargs)
        with patch.object(m,'verify_ref',side_effect=reference),patch.object(m.time,'time_ns',return_value=0),contextlib.redirect_stdout(io.StringIO()):
            if fail:
                with self.assertRaises(ValueError):m.run_motion_probes(engine,self.bindings,self.plan)
            else:m.run_motion_probes(engine,self.bindings,self.plan)
        return engine

    def test_mixed_front_plan_command_and_final_result_bind_physical_8(self):
        self.plan=m.make_plan(self.bindings,self.root+'/run','mixed-fixture',self.root,
                              bus2_mixed_raw_hold=True,mixed_wheel_id=8)
        contract=self.plan['bus2_mixed_raw_hold']
        self.assertEqual(contract['selected_motor_id'],8)
        self.assertEqual(contract['wheel_ids'],[8,9,7])
        self.assertEqual(contract['wheel_raw_goal'],[500,0,0])
        self.assertIn('前8固定raw+500持续20秒',self.plan['scope'])
        self.assertIn('左9/右7保持raw0',self.plan['scope'])
        self.assertEqual([row['motor'] for row in self.plan['base_targets']
                          if row['decision']=='RAW_DIAGNOSTIC'],[8])
        engine=self.perform()
        self.assertEqual(engine.mixed_wire_evidence['original_argv'][2:4],['8','mixed-hold-wheel'])
        self.assertEqual((engine.right_pulse['motor'],engine.right_pulse['name']),(8,'base_front_wheel'))
        self.assertEqual((engine.rows[-1]['motor'],engine.rows[-1]['name']),(8,'base_front_wheel'))
        before=idle_rows();after=idle_rows()
        for motor in range(1,7):before[f'right:{motor}']['Goal_Position']=0
        after['right:8']['Present_Position']=1400
        position=m.classify_position_snapshot(after,before,engine.rows)
        self.assertEqual(position['status'],'PASS')
        report={'plan':self.plan,'probes':engine.rows,'mixed_raw_result':engine.mixed_raw_result,
                'stop_verification':{'status':'PASS'},'identity_verification':{'status':'PASS'},
                'position_verification':position,'child_active':False}
        self.assertEqual(m.classify_run_result(report),'PASS')
        bad=copy.deepcopy(report);bad['probes'][-1]['motor']=9
        self.assertEqual(m.classify_run_result(bad),'FAIL')

    def test_mixed_selection_rejects_wrong_id_held_motion_and_cross_wheel_go(self):
        legacy=self.receipt();old_plan=copy.deepcopy(self.plan)
        old_go=WheelFeedbackTests.go(self,old_plan,1000)
        for selected in (8,7):
            with self.subTest(selected=selected):
                self.plan=m.make_plan(self.bindings,self.root+'/run','mixed-fixture',self.root,
                                      bus2_mixed_raw_hold=True,mixed_wheel_id=selected)
                row=self.receipt();self.assertEqual(self.parse(row),row)
                self.assertEqual([anchor['motor_id'] for anchor in row['wheel_anchors']],
                                 [motor for motor in m.wheel_ids() if motor!=selected])
                for value in (9,True,str(selected),None):
                    bad=copy.deepcopy(row);bad['selected_motor_id']=value
                    with self.subTest(value=value),self.assertRaises(ValueError):self.parse(bad)
                bad=copy.deepcopy(row);bad.pop('selected_motor_id')
                with self.assertRaises(ValueError):self.parse(bad)
                with self.assertRaises(ValueError):self.parse(legacy)
                for slot,motor in enumerate(m.wheel_ids()):
                    if motor==selected:continue
                    bad=copy.deepcopy(row);bad['wheel_samples'][0]['position_raw'][slot]=1005
                    with self.subTest(held_motor=motor),self.assertRaises(ValueError):self.parse(bad)
                with self.assertRaises(ValueError):m.validate_go(old_go,self.plan,2000)
                bad_go=copy.deepcopy(old_go);bad_go['plan_sha256']=m.digest(self.plan)
                with self.assertRaises(ValueError):m.validate_go(bad_go,self.plan,2000)
                m.validate_go(WheelFeedbackTests.go(self,self.plan,1000),self.plan,2000)
        self.plan=old_plan
        self.assertEqual(self.parse(legacy),legacy)
        for value in (8,9,True):
            legacy['selected_motor_id']=value
            with self.subTest(legacy_selected=value),self.assertRaises(ValueError):self.parse(legacy)

    def test_mixed_wheel_cli_requires_mixed_mode_and_keeps_legacy_default(self):
        self.assertEqual(self.plan['bus2_mixed_raw_hold']['selected_motor_id'],9)
        self.assertEqual(self.plan['bus2_mixed_raw_hold']['operation'],'mixed-hold-wheel9')
        self.assertEqual(self.plan['scope'],m.MIXED_RAW_SCOPE)
        for selected in (7,8,9):
            with self.subTest(selected=selected),self.assertRaises(ValueError):
                m.make_plan(self.bindings,self.root+'/run','mixed-fixture',self.root,mixed_wheel_id=selected)
            with self.assertRaises(ValueError):run_main(['--mixed-wheel-id',str(selected)])
        for selected in (6,10,True,'8'):
            with self.subTest(selected=selected),self.assertRaises(ValueError):
                m.make_plan(self.bindings,self.root+'/run','mixed-fixture',self.root,
                            bus2_mixed_raw_hold=True,mixed_wheel_id=selected)
        with patch.object(m.subprocess,'Popen',side_effect=AssertionError('PLAN_ONLY不得启动child')):
            out=io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(run_main(['--bus2-mixed-raw-hold','--mixed-wheel-id','8']),0)
            row=json.loads(out.getvalue())
            self.assertEqual(row['actions_started'],0)
            self.assertIn('前8固定raw+500持续20秒',row['scope'])

    @unittest.skipUnless(os.environ.get('ROBOT_MIXED_FRONT_SOFTWARE_FIXTURE'),'未提供前轮同源Rust内存串口成功回执')
    def test_actual_rust_front_memory_serial_receipt_passes_strict_python_parser(self):
        self.plan=m.make_plan(self.bindings,self.root+'/run','mixed-fixture',self.root,
                              bus2_mixed_raw_hold=True,mixed_wheel_id=8)
        row=m.strict_json(Path(os.environ['ROBOT_MIXED_FRONT_SOFTWARE_FIXTURE']).read_text())
        self.assertEqual(row['schema'],'FEETECH_MIXED_WHEEL_V1')
        self.assertEqual(row['selected_motor_id'],8)
        self.assertEqual(row['arm_calibration_path'],'/tmp/mixed-software-arm-calibration.json')
        row['arm_calibration_path']=self.bindings['bus2_mixed_raw_hold']['arm_calibration']['path']
        self.expiry=row['go_expires_at_unix_ns']
        self.assertEqual(self.parse(row),row)

    @unittest.skipUnless(os.environ.get('ROBOT_MIXED_SOFTWARE_FIXTURE'),'未提供同源Rust内存串口成功回执')
    def test_actual_rust_memory_serial_receipt_passes_strict_python_parser(self):
        row=m.strict_json(Path(os.environ['ROBOT_MIXED_SOFTWARE_FIXTURE']).read_text())
        self.assertEqual(row['arm_calibration_path'],'/tmp/mixed-software-arm-calibration.json')
        self.assertEqual(row['serial_device'],m.DEVICES['right'])
        # Rust测试以虚拟校准路径输出；只将该身份接到本测试已绑定的相同六臂校准。
        row['arm_calibration_path']=self.bindings['bus2_mixed_raw_hold']['arm_calibration']['path']
        self.expiry=row['go_expires_at_unix_ns']
        self.assertEqual(self.parse(row),row)
        bad=copy.deepcopy(row);bad['wheel_ids']=[8,7,9]
        with self.assertRaises(ValueError):self.parse(bad)

    @unittest.skipUnless(os.environ.get('ROBOT_MIXED_TOLERANCE_SOFTWARE_FIXTURE'),'未提供非零容差同源Rust回执')
    def test_actual_rust_tolerated_anchor_preserves_original_calibration_and_matching_policy(self):
        row=m.strict_json(Path(os.environ['ROBOT_MIXED_TOLERANCE_SOFTWARE_FIXTURE']).read_text())
        self.addCleanup(m.configure_bus_mapping,BUS_FIXTURE_PATH,LEFT_BUS_FIXTURE_PATH)
        self.calibration={name:cal for name,cal in zip(m.MIXED_ARM_NAMES,row['loaded_calibration'])}
        old=self.bindings['bus2_mixed_raw_hold']['arm_calibration'];path=Path(old['path'])
        path.write_text(json.dumps(self.calibration));info=path.stat()
        reference={**old,'bytes':info.st_size,'sha256':hashlib.sha256(path.read_bytes()).hexdigest(),
                   'mtime_ns':info.st_mtime_ns,'ctime_ns':info.st_ctime_ns}
        self.bindings['refs']=[reference if ref['path']==str(path) else ref for ref in self.bindings['refs']]
        self.bindings['bus2_mixed_raw_hold']['arm_calibration']=reference
        m.configure_bus_mapping(write_bus_fixture(self.directory/'tolerance-bus2.toml',
            tolerances=[*row['calibration_range_tolerances_raw'],0,0,0]),LEFT_BUS_FIXTURE_PATH)
        self.plan=m.make_plan(self.bindings,self.root+'/run','mixed-fixture',self.root,bus2_mixed_raw_hold=True)
        row['arm_calibration_path']=str(path);self.expiry=row['go_expires_at_unix_ns']
        self.assertEqual((row['arm_anchors'][2]['raw'],row['arm_anchors'][2]['calibration_max']),(2992,2991))
        self.assertEqual(self.parse(row),row)

    def test_mixed_tolerances_require_exact_echo_and_keep_raw_anchor_at_effective_edges(self):
        legacy=self.receipt();legacy.pop('calibration_range_tolerances_raw')
        self.assertEqual(self.parse(legacy),legacy)
        self.addCleanup(m.configure_bus_mapping,BUS_FIXTURE_PATH,LEFT_BUS_FIXTURE_PATH)
        m.configure_bus_mapping(write_bus_fixture(self.directory/'tolerance-bus2.toml',
            tolerances=[0,20,7,0,0,20,0,0,0]),LEFT_BUS_FIXTURE_PATH)
        self.plan=m.make_plan(self.bindings,self.root+'/run','mixed-fixture',self.root,bus2_mixed_raw_hold=True)
        self.calibration[m.MIXED_ARM_NAMES[1]].update(range_min=1020,range_max=3000)
        self.calibration[m.MIXED_ARM_NAMES[2]].update(range_min=1007)
        def receipt(value):
            row=self.receipt()
            for anchor,cal in zip(row['arm_anchors'],self.calibration.values()):
                anchor.update(calibration_min=cal['range_min'],calibration_max=cal['range_max'])
            row['arm_anchors'][1].update(raw=value,goal_raw=value);row['arm_final'][1]['raw']=value
            for sample in row['wheel_samples']:sample['arm_position_raw'][1]=value
            return row
        for value,accepted in ((1000,True),(999,False),(3020,True),(3021,False)):
            row=receipt(value)
            with self.subTest(value=value):
                if accepted:self.assertEqual(self.parse(row),row)
                else:
                    with self.assertRaises(ValueError):self.parse(row)
        for echo in (None,[20]*6,[0,20,True,0,0,20],[0]*5):
            row=receipt(1000)
            if echo is None:row.pop('calibration_range_tolerances_raw')
            else:row['calibration_range_tolerances_raw']=echo
            with self.subTest(echo=echo),self.assertRaises(ValueError):self.parse(row)


    def test_only_toml_changes_all_mixed_slots_argv_feedback_names_and_invalidates_old_go(self):
        self.addCleanup(m.configure_bus_mapping,BUS_FIXTURE_PATH)
        original_directory=self.directory;path=original_directory/'bus2.toml'
        previous_plan=previous_go=None
        for ids in itertools.permutations((7,8,9)):
            m.configure_bus_mapping(write_bus_fixture(path,ids))
            self.directory=original_directory/('order-'+''.join(map(str,ids)));self.directory.mkdir()
            self.plan=m.make_plan(self.bindings,self.root+'/run','mixed-fixture',self.root,bus2_mixed_raw_hold=True)
            go=WheelFeedbackTests.go(self,self.plan,1000)
            m.validate_go(go,self.plan,2000)
            if previous_plan is not None:
                with self.assertRaises(ValueError):m.validate_go(previous_go,previous_plan,2000)
                with self.assertRaises(ValueError):m.validate_go(previous_go,self.plan,2000)
            previous_plan,previous_go=copy.deepcopy(self.plan),copy.deepcopy(go)
            with self.subTest(ids=ids):
                engine=self.perform()
                contract=self.plan['bus2_mixed_raw_hold']
                self.assertEqual(contract['wheel_ids'],list(ids))
                self.assertEqual(contract['motor_ids'],[*range(1,7),*ids])
                self.assertEqual(contract['wheel_raw_goal'],[500 if motor==9 else 0 for motor in ids])
                argv=engine.mixed_wire_evidence['original_argv']
                self.assertEqual(argv[-4:],['--wheel-ids',','.join(map(str,ids)),
                    '--calibration-range-tolerances-raw','0,0,0,0,0,0'])
                self.assertEqual(argv[2:4],['9','mixed-hold-wheel9'])
                selected_role=m.WHEEL_STOP_TARGETS[9]
                self.assertEqual(engine.rows[-1]['name'],f'base_{selected_role}_wheel')
                self.assertEqual(engine.rows[-1]['motor'],9)
                for role,motor in zip(m.WHEEL_ROLES,ids):
                    self.assertEqual(self.plan[f'{role}{motor}_stop_contract']['target'],
                                     ['right',motor,f'base_{role}_wheel'])
        self.directory=original_directory
        path.write_bytes(path.read_bytes()+b'# bytes changed without mapping change\n')
        with self.assertRaisesRegex(ValueError,'字节身份变化'):m.validate_go(go,self.plan,2000)
        with patch.object(m,'RealExecutor',side_effect=AssertionError('配置篡改不得构造执行器')):
            with self.assertRaisesRegex(ValueError,'字节身份变化'):m.execute(self.plan,go,original_directory/'go.json')

    def test_invalid_bus_config_is_rejected_before_plan_or_child(self):
        self.addCleanup(m.configure_bus_mapping,BUS_FIXTURE_PATH)
        path=self.directory/'bad-bus2.toml'
        valid=write_bus_fixture(path).read_text()
        for old,new in (('[1, 2, 3, 4, 5, 6, 8, 9, 7]','[1, 2, 3, 4, 5, 6, 8, 8, 7]'),
                        ('[1, 2, 3, 4, 5, 6, 8, 9, 7]','[2, 1, 3, 4, 5, 6, 8, 9, 7]'),
                        ('base_front_wheel','unknown_wheel'),('base_right_wheel','base_left_wheel')):
            path.write_text(valid.replace(old,new))
            with self.subTest(new=new),self.assertRaises(ValueError):m.configure_bus_mapping(path)
        with self.assertRaises(FileNotFoundError):m.configure_bus_mapping(self.directory/'missing.toml')

    def test_mixed_plan_binds_new_refs_exact_targets_and_rejects_all_old_mode_combinations(self):
        self.assertEqual(m.position_targets_from_plan(self.plan),[])
        self.assertEqual(self.plan['bus2_mixed_raw_hold']['allowed_raw_targets'],[0,500])
        self.assertEqual(self.plan['bus2_mixed_raw_hold']['wheel_ids'],[8,9,7])
        self.assertEqual(self.plan['bus2_mixed_raw_hold']['wheel_raw_goal'],[0,500,0])
        default=m.make_plan(self.bindings,self.root+'/default','mixed-default',self.root)
        self.assertNotIn('bus2_mixed_raw_hold',default)
        self.assertTrue(all(row['decision']=='DEFERRED' for row in default['base_targets']))
        for options in ({'right_wheel_pulse':True},{'wheel_pulse_target':'left'},
                        {'right_wheel_feedback_only':True},{'front_wheel_feedback_only':True},
                        {'left_wheel_feedback_only':True},{'zero_stop_check':True}):
            with self.subTest(options=options),self.assertRaises(ValueError):
                m.make_plan(self.bindings,self.root+'/run','mixed-fixture',self.root,bus2_mixed_raw_hold=True,**options)
        with self.assertRaises(ValueError):
            m.make_plan(self.bindings,self.root+'/run','mixed-fixture',self.root,['head-1'],bus2_mixed_raw_hold=True)
        for mutation in ('old_tool','missing_ref','calibration','negative','old_operation','old_order'):
            bad=copy.deepcopy(self.plan)
            if mutation=='old_tool':bad['bindings']['bus2_mixed_raw_hold']['tool']['sha256']=m.TOOL_SHA
            if mutation=='missing_ref':bad['bindings']['refs'].pop()
            if mutation=='calibration':bad['bus2_mixed_raw_hold']['arm_calibration']['sha256']='0'*64
            if mutation=='negative':bad['bus2_mixed_raw_hold']['wheel_raw_goal']=[0,0,-500]
            if mutation=='old_operation':bad['bus2_mixed_raw_hold']['operation']='mixed-hold-right9'
            if mutation=='old_order':bad['bus2_mixed_raw_hold']['wheel_ids']=[8,7,9]
            with self.subTest(mutation=mutation),self.assertRaises(ValueError):m.position_targets_from_plan(bad)
        with patch.object(m.subprocess,'Popen',side_effect=AssertionError('不许启动')):
            with contextlib.redirect_stdout(io.StringIO()):self.assertEqual(run_main(['--bus2-mixed-raw-hold']),0)
            with self.assertRaises(ValueError):run_main(['--bus2-mixed-raw-hold','--execute'])

    def test_mixed_go_requires_original_ten_minute_scope_and_current_wheel_facts(self):
        go={'schema':'MAINTENANCE_GO_V2','issued_by':'root','artifact_root':self.root,
            'plan_sha256':m.digest(self.plan),'host':self.bindings['host'],'source':'本次现场答复',
            'issued_at_ns':1000,'expires_at_ns':self.expiry,
            'physical':{side:{**{key:True for key in m.FACTS},'confirmed_at_ns':1000} for side in m.SERIALS}}
        go['physical']['right'].update(all_three_wheels_off_ground=True,base_stably_supported=True,
            continuous_wheel_observation=True,base_confirmation_kind='CURRENT_SETUP_READY',
            base_confirmation_scope=m.MIXED_RAW_SCOPE,base_confirmation_answer='本次混合维护就绪')
        m.validate_go(go,self.plan,2000)
        for mutation in ('scope','ttl','watch','old_plan'):
            bad=copy.deepcopy(go)
            if mutation=='scope':bad['physical']['right']['base_confirmation_scope']=m.pulse_scope('right')
            if mutation=='ttl':bad['expires_at_ns']=bad['issued_at_ns']+600_000_000_001
            if mutation=='watch':bad['physical']['right']['continuous_wheel_observation']=False
            if mutation=='old_plan':
                old=copy.deepcopy(self.plan)
                old['bus2_mixed_raw_hold'].update(operation='mixed-hold-right9',wheel_ids=[8,7,9],wheel_raw_goal=[0,0,500])
                bad['plan_sha256']=m.digest(old)
            with self.subTest(mutation=mutation),self.assertRaises(ValueError):m.validate_go(bad,self.plan,2000)

    def test_mixed_raw_receipt_preserves_words_wraps_and_each_original_zero_case(self):
        for intermediate,final in ((0,0),(0x8032,50),(250,0x8032)):
            row=self.receipt();row['zero_samples'][0]['present_velocity_word']=intermediate
            row['zero_samples'][-1]['present_velocity_word']=final
            row['wheel_samples'][0]['velocity_word'][::2]=[50,0x8032]
            self.assertEqual(self.parse(row),row)

    def test_mixed_receipt_rejects_forged_targets_timing_calibration_motion_and_cleanup(self):
        changes=[('schema',lambda r:r.update(schema='PASS')),
            ('old_schema',lambda r:r.update(schema='FEETECH_MIXED_RIGHT9_V1')),
            ('old_operation',lambda r:r.update(operation='mixed-hold-right9')),
            ('extra',lambda r:r.update(hidden_write=True)),
            ('id',lambda r:r['motor_ids'].__setitem__(0,True)),
            ('raw',lambda r:r['wheel_raw_goal'].__setitem__(1,-500)),
            ('expiry',lambda r:r.update(go_expires_at_unix_ns=self.expiry+1)),
            ('calibration',lambda r:r['loaded_calibration'][0].update(range_max=4094)),
            ('goal',lambda r:r['arm_anchors'][0].update(goal_raw=1001)),
            ('stale',lambda r:r['arm_anchors'][0].update(sampled_ns=1)),
            ('arm',lambda r:r['wheel_samples'][0]['arm_position_raw'].__setitem__(0,1005)),
            ('arm_final',lambda r:r['arm_final'][0].update(raw=1005)),
            ('wheel_anchor',lambda r:r['wheel_anchors'][0].update(motor_id=9)),
            ('other_wheel',lambda r:r['wheel_samples'][0]['position_raw'].__setitem__(0,1005)),
            ('other_speed',lambda r:r['wheel_samples'][0]['velocity_word'].__setitem__(0,51)),
            ('no_pv',lambda r:[s['velocity_word'].__setitem__(1,0) for s in r['wheel_samples']]),
            ('no_travel',lambda r:[s['position_raw'].__setitem__(1,4000) for s in r['wheel_samples']]),
            ('reverse',lambda r:r['wheel_samples'][0]['velocity_word'].__setitem__(1,0x8032)),
            ('cycle_missing',lambda r:r['cycles'].pop()),
            ('deadline',lambda r:r['cycles'][2].update(source_deadline_ns=r['cycles'][2]['source_deadline_ns']+1)),
            ('first_late',lambda r:r['cycles'][0].update(finished_ns=r['timing']['first_nonzero_ns']-1)),
            ('sample_late',lambda r:r['wheel_samples'][0].update(finished_ns=r['cycles'][5]['finished_ns']+1)),
            ('zero_late',lambda r:r['zero_samples'][-1].update(finished_ns=r['timing']['zero_started_ns']+500_000_001)),
            ('te1',lambda r:r['zero_samples'][0].update(torque_enable=0)),
            ('zero',lambda r:r['zero_samples'][-1].update(present_velocity_word=51)),
            ('cleanup',lambda r:r['cleanup'].update(disable_ok=False)),
            ('torque',lambda r:r['cleanup']['torque_enable_after'].__setitem__(8,1)),
            ('failed',lambda r:r.update(result='FAIL',error='原失败'))]
        for name,change in changes:
            row=self.receipt();change(row)
            with self.subTest(name=name),self.assertRaises(ValueError):self.parse(row)
        text=json.dumps(self.receipt())
        for bad in (text+'\n'+text,text.replace('"result": "PASS"','"result":"PASS","result":"PASS"')):
            with self.assertRaises(ValueError):m.parse_mixed_raw_result(bad,self.plan,self.expiry,self.calibration)

    def test_mixed_run_uses_one_traced_child_and_original_position_stop_identity_gates(self):
        engine=self.perform();self.assertEqual(len(engine.io.calls),1)
        self.assertEqual(engine.io.calls[0][1:4],['-f','-I','3'])
        self.assertEqual([probe['kind'] for probe in engine.rows],['position_hold']*6+['velocity'])
        self.assertTrue(all('outward' not in probe and 'position_accuracy' not in probe for probe in engine.rows))
        before=idle_rows();after=idle_rows()
        for motor in range(1,7):before[f'right:{motor}']['Goal_Position']=0
        after['right:9']['Present_Position']=1400
        position=m.classify_position_snapshot(after,before,engine.rows);self.assertEqual(position['status'],'PASS')
        report={'plan':self.plan,'probes':engine.rows,'mixed_raw_result':engine.mixed_raw_result,
                'stop_verification':{'status':'PASS'},'identity_verification':{'status':'PASS'},
                'position_verification':position,'child_active':False}
        self.assertEqual(m.classify_run_result(report),'PASS')
        for key,value in (('stop_verification',{'status':'UNKNOWN'}),('identity_verification',{'status':'FAIL'}),
                          ('position_verification',{'status':'FAIL'}),('child_active',True)):
            bad=copy.deepcopy(report);bad[key]=value;self.assertNotEqual(m.classify_run_result(bad),'PASS')
        for motor,register,value in (('right:1','Goal_Position',1001),('right:1','Present_Position',1005),
                                     ('left:1','Goal_Position',1001),('right:8','Goal_Position',1001)):
            bad=copy.deepcopy(after);bad[motor][register]=value
            self.assertEqual(m.classify_position_snapshot(bad,before,engine.rows)['status'],'FAIL')

    def test_mixed_bad_receipt_stickily_blocks_all_later_uart_and_never_retries(self):
        row=self.receipt();row['cleanup']['disable_ok']=False
        engine=self.perform(row,fail=True);self.assertEqual(len(engine.io.calls),1)
        self.assertTrue(engine.manual_cut);self.assertTrue(engine.right_pulse['uart_blocked'])
        with self.assertRaises(ValueError):engine.io.command(list(engine.mixed_trace_argv),allowed=(0,1),mixed_stop_check=engine.mixed_stop_check)
        self.assertEqual(len(engine.io.calls),1)

    def test_mixed_failed_receipt_preserves_original_error_before_success_validation(self):
        row=self.failed_receipt()
        with self.assertRaises(ValueError) as caught:self.parse(row)
        self.assertEqual(str(caught.exception),'混合child失败: '+row['error'])

    def test_mixed_failed_child_keeps_original_error_and_sticky_uart_block(self):
        row=self.failed_receipt();engine=self.perform(row,fail=True)
        self.assertEqual(engine.mixed_raw_result,row)
        self.assertEqual(engine.rows[-1]['error'],repr(ValueError('混合child失败: '+row['error'])))
        self.assertTrue(all(probe['status']=='FAIL' for probe in engine.rows))
        self.assertTrue(engine.manual_cut);self.assertTrue(engine.right_pulse['uart_blocked'])
        self.assertEqual(len(engine.io.calls),1);self.assertEqual(len(engine.io.commands),1)
        self.assertEqual(engine.io.commands[0]['exit_code'],1);self.assertTrue(engine.io.commands[0]['reaped'])
        with self.assertRaises(ValueError):
            engine.io.command(list(engine.mixed_trace_argv),allowed=(0,1),mixed_stop_check=engine.mixed_stop_check)
        self.assertEqual(len(engine.io.calls),1)

    def test_mixed_result_branch_keeps_identity_and_pass_checks_strict(self):
        for field,value in (('schema','OTHER'),('operation','other'),('serial_device',m.DEVICES['left']),
                            ('arm_calibration_path','/other.json'),('go_expires_at_unix_ns',self.expiry+1),
                            ('wheel_raw_goal',[0,0,500]),('pulse_duration_ns',1),
                            ('result','UNKNOWN'),('error',None),('error',''),('error',{'message':'失败'})):
            row=self.failed_receipt();row[field]=value
            with self.subTest(field=field,value=value),self.assertRaises(ValueError) as caught:self.parse(row)
            self.assertNotEqual(str(caught.exception),'混合child失败: '+str(row['error']))
        for field,value in (('error','伪成功携带错误'),('result','UNKNOWN'),('cycle_count',399)):
            row=self.receipt();row[field]=value
            with self.subTest(field=field),self.assertRaises(ValueError):self.parse(row)


class S0MixedBridgeTests(unittest.TestCase):
    def setUp(self):
        temporary=tempfile.TemporaryDirectory();self.addCleanup(temporary.cleanup)
        self.root=Path(temporary.name).resolve();self.output=self.root/'output';self.output.mkdir()
        self.addCleanup(m.configure_bus_mapping,BUS_FIXTURE_PATH,LEFT_BUS_FIXTURE_PATH)
        def reference(path):
            info=path.stat();raw=path.read_bytes()
            return {'path':str(path),'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest(),
                    'uid':info.st_uid,'gid':info.st_gid,'mode':stat.S_IMODE(info.st_mode),'device':info.st_dev,
                    'inode':info.st_ino,'mtime_ns':info.st_mtime_ns,'ctime_ns':info.st_ctime_ns,
                    'nlink':info.st_nlink,'symlink':False}
        self.reference=reference
        calibration=self.root/'calibration.json';calibration.write_text('{}')
        config=write_bus_fixture(self.root/'robot.toml')
        config.write_text('[robot]\nid="bridge_fixture"\nconfig_version="test"\n'+config.read_text()+
                          '\nid="right_arm_full_with_base"\nauto_enable=false\nfeetech_bus2_mixed_raw=true\n'
                          'calibration_paths='+json.dumps([str(calibration)])+'\n')
        m.configure_bus_mapping(config,LEFT_BUS_FIXTURE_PATH)
        self.bindings={'schema':'MAINTENANCE_BINDINGS_V1',
            'host':{'hostname':'fixture','uid':os.getuid(),'gid':os.getgid(),'boot_id':'fixture-boot'},
            'tool':{'path':'/not-uart','sha256':m.TOOL_SHA,'bytes':m.TOOL_BYTES},
            'usb':{side:{'device':m.DEVICES[side],'serial':m.SERIALS[side]} for side in m.SERIALS},
            'refs':[reference(calibration)],'binary_paths':[],'protection':{},
            's0_hardware_id':'right_arm_full_with_base'}
        for name in ('s0_daemon','supervised_demo'):
            ref=feedback_reference(self.root/name)
            self.bindings[name]=ref;self.bindings['refs'].append(ref);self.bindings['binary_paths'].append(ref['path'])
        for side in m.SERIALS:
            directory=self.root/side;directory.mkdir();self.bindings['protection'][side]={}
            for name in ('rx_trust.lock','estop_latch.lock','rx_trust_marker','rx_trust_marker.bak','estop_latch','estop_latch.bak'):
                path=directory/name;path.write_bytes(b'1\n');self.bindings['protection'][side][name]=reference(path)
        with patch.object(m,'MAINTENANCE_NAMESPACE',self.root.parent):
            self.plan=m.make_plan(self.bindings,self.output,'bridge',self.root,s0_mixed_hold=True)
        self.expiry=600_000_000_000

    def receipt(self):
        contract=self.plan['s0_mixed_hold'];start=1_100_000_000;enabled=1_000_000_000;zero=start+20_000_000_000
        anchor_stamp=enabled
        cycles=[]
        for index in range(400):
            attempt=start+index*50_000_000
            source=min(attempt+200_000_000,enabled+21_000_000_000)
            if index==0:source=min(source,anchor_stamp+200_000_000)
            cycles.append({'cycle':index,'scheduled_ns':attempt,'publish_attempt_ns':attempt,
                'publish_return_ns':attempt+1000,'local_deadline_ns':attempt+50_000_000,
                'source_deadline_ns':source})
        wheels=contract['wheel_ids'];selected=contract['selected_motor_id']
        motion=[{'window':index,'started_ns':start+index*250_000_000,
                 'finished_ns':start+index*250_000_000+1_000_000,
                 'wheels':[{'id':motor,'position':(1000+index*80)%4096 if motor==selected else 1000,
                            'velocity':500 if motor==selected else 0} for motor in wheels]} for index in range(1,80)]
        samples=[{'started_ns':zero+offset*1_000_000,'finished_ns':zero+offset*1_000_000+1_000_000,
                  'wheels':[{'id':motor,'torque':1,'goal':0,'velocity':0} for motor in wheels]} for offset in (50,250,450)]
        return {'schema':'SUPERVISED_S0_MIXED_V1','result':'SOFTWARE_PATH_PASS','config':contract['config']['path'],
            'hardware_id':contract['hardware_id'],'layout_id':123,'selected_motor_id':selected,'wheel_ids':wheels,
            'go_expires_at_unix_ns':self.expiry,'cleanup_errors':[],'manual_power_cut_required':False,
            'motion_position_verified':True,'motion_velocity_raw':'PASS','disable_retired':True,
            'torque_off_confirmed':True,'diagnostics_dropped_zero_verified':True,'final_estop_published':True,
            'physical_motion':'NOT_OBSERVED','stop51':'NOT_RUN','full_hardware_acceptance':False,'deployable':False,
            'anchor':{'raw':[1000]*9,'target':[0.0]*9,'stamp_ns':anchor_stamp},'enable_attempt_ns':enabled,
            'nonzero_start_ns':start,'nonzero_end_ns':zero,'zero_attempt_ns':zero,'zero_end_ns':zero+500_000_000,
            'zero_publish_return_ns':zero+1000,'final_estop_attempt_ns':zero+600_000_000,
            'final_estop_publish_return_ns':zero+600_001_000,'finished_ns':zero+600_002_000,
            'cycles':cycles,'motion_samples':motion,'zero_samples':samples,'position_travel_raw':78*80}

    def test_plan_only_cli_and_plan_exclude_legacy_actions_and_sensor_helpers(self):
        with patch.object(m.subprocess,'Popen',side_effect=AssertionError('PLAN_ONLY不得启动')),contextlib.redirect_stdout(io.StringIO()) as out:
            self.assertEqual(run_main(['--s0-mixed-hold']),0)
        self.assertEqual(json.loads(out.getvalue())['scope'],m.S0_MIXED_SCOPE)
        for kwargs in ({'bus2_mixed_raw_hold':True},{'right_wheel_pulse':True},{'position_target_names':['head-1']}):
            with self.subTest(kwargs=kwargs),patch.object(m,'MAINTENANCE_NAMESPACE',self.root.parent),self.assertRaises(ValueError):
                m.make_plan(self.bindings,self.output,'bridge',self.root,s0_mixed_hold=True,**kwargs)
        config=Path(m.BUS_CONFIG['path']);original=config.read_text()
        config.write_text(original+'\n[[sensor]]\nhardware_id="right_arm_full_with_base"\n')
        with self.assertRaises(ValueError):m.s0_mixed_contract(self.bindings)
        config.write_text(original.replace('auto_enable=false','auto_enable=true'))
        with self.assertRaises(ValueError):m.s0_mixed_contract(self.bindings)

    def test_receipt_checks_fixed_time_raw_identity_and_distinct_sender_anchor_contract(self):
        result=self.receipt()
        self.assertEqual(m.parse_s0_mixed_result(json.dumps(result),self.plan,self.expiry),result)
        for key,value in (('schema','FEETECH_MIXED_WHEEL_V1'),('hardware_id','other'),('selected_motor_id',7),
                          ('manual_power_cut_required',True),('final_estop_published',False),('disable_retired',False),
                          ('diagnostics_dropped_zero_verified',False),('zero_attempt_ns',result['zero_attempt_ns']-1),
                          ('zero_end_ns',result['zero_end_ns']+1),('motion_velocity_raw','NOT_EVALUATED')):
            bad=copy.deepcopy(result);bad[key]=value
            with self.subTest(key=key),self.assertRaises(ValueError):m.parse_s0_mixed_result(json.dumps(bad),self.plan,self.expiry)
        for mutate in (lambda row:row['cycles'].pop(),lambda row:row['cycles'][5].update(scheduled_ns=0),
                       lambda row:row['cycles'][1].update(source_deadline_ns=row['cycles'][1]['source_deadline_ns']-1),
                       lambda row:row['cycles'][0].update(source_deadline_ns=row['cycles'][0]['publish_attempt_ns']+200_000_000),
                       lambda row:row['motion_samples'][0].update(started_ns=row['nonzero_start_ns']+500_000_000,
                                                                   finished_ns=row['nonzero_start_ns']+500_000_000),
                       lambda row:row['motion_samples'][1]['wheels'][0].update(velocity=0xffff),
                       lambda row:row['zero_samples'][0]['wheels'][0].update(torque=0),
                       lambda row:row['zero_samples'][2]['wheels'][0].update(velocity=500)):
            bad=copy.deepcopy(result);mutate(bad)
            with self.assertRaises(ValueError):m.parse_s0_mixed_result(json.dumps(bad),self.plan,self.expiry)
        result['zero_samples'][0]['wheels'][0]['velocity']=500
        self.assertEqual(m.parse_s0_mixed_result(json.dumps(result),self.plan,self.expiry),result)

    def test_s0_wheels_follow_configuration_slots_and_leave_legacy_role_order_unchanged(self):
        config=Path(m.BUS_CONFIG['path']);text=config.read_text();hardware=m.tomllib.loads(text)['hardware'][0]
        names=[*m.MIXED_ARM_NAMES,'base_right_wheel','base_front_wheel','base_left_wheel']
        text=text.replace(json.dumps(hardware['joints']),json.dumps(names))
        text=text.replace(json.dumps(hardware['motor_ids']),json.dumps(list(range(1,10))))
        config.write_text(text);m.configure_bus_mapping(config,LEFT_BUS_FIXTURE_PATH)
        contract=m.s0_mixed_contract(self.bindings)
        self.assertEqual(contract['wheel_ids'],[7,8,9]);self.assertEqual(contract['selected_motor_id'],8)
        self.assertEqual(m.wheel_ids(),[8,9,7])

    def test_right_persistent_rename_and_lock_pid_are_allowed_but_left_or_lock_inode_changes_fail(self):
        right=self.bindings['protection']['right'];left=self.bindings['protection']['left']
        Path(right['rx_trust.lock']['path']).write_text('12345\n')
        marker=Path(right['rx_trust_marker']['path']);replacement=marker.with_name('new-marker')
        replacement.write_bytes(b'1\n');replacement.replace(marker)
        m.verify_s0_protection(self.bindings,'final')
        Path(right['estop_latch']['path']).write_bytes(b'0\n')
        m.verify_s0_protection(self.bindings,'running')
        with self.assertRaises(ValueError):m.verify_s0_protection(self.bindings,'final')
        Path(right['estop_latch']['path']).write_bytes(b'1\n')
        Path(left['rx_trust.lock']['path']).write_text('changed\n')
        with self.assertRaises(ValueError):m.verify_s0_protection(self.bindings,'final')
        Path(left['rx_trust.lock']['path']).write_bytes(b'1\n')
        lock=Path(right['rx_trust.lock']['path']);replacement.write_bytes(b'1\n');replacement.replace(lock)
        with self.assertRaises(ValueError):m.verify_s0_protection(self.bindings,'final')

    def test_ready_requires_complete_exact_original_line_with_bounded_pending(self):
        pending,ready=m.s0_ready_chunk(b'',b'No config\nboot_ready spawned_ns=1 first_exit_ns=2 ready_ns=3')
        self.assertFalse(ready)
        pending,ready=m.s0_ready_chunk(pending,b'\n')
        self.assertTrue(ready);self.assertEqual(pending,b'')
        for raw in (b'prefix boot_ready spawned_ns=1 first_exit_ns=2 ready_ns=3\n',b'boot_ready\n'):
            self.assertFalse(m.s0_ready_chunk(b'',raw)[1])
        with self.assertRaises(ValueError):m.s0_ready_chunk(b'x'*65536,b'x')

    def test_only_replaced_right_latch_inode_accepts_writer_0640_on_the_original_device(self):
        right=self.bindings['protection']['right']
        for name in ('estop_latch','estop_latch.bak'):
            path=Path(right[name]['path']);path.chmod(0o600);right[name]=self.reference(path)
        m.verify_s0_protection(self.bindings,'final')
        for name in ('estop_latch','estop_latch.bak'):
            with self.subTest(name=name):
                path=Path(right[name]['path']);path.chmod(0o640)
                with self.assertRaisesRegex(ValueError,'保护文件身份/权限变化'):
                    m.verify_s0_protection(self.bindings,'running')
                path.chmod(0o600)
                replacement=self.root/'new-latch';replacement.write_bytes(b'1\n');replacement.chmod(0o640)
                replacement.replace(path)
                for phase in ('running','final'):
                    evidence=m.verify_s0_protection(self.bindings,phase)
                    row=next(row for row in evidence if row['path']==str(path))
                    self.assertEqual((row['device'],row['mode']),(right[name]['device'],0o640))
                    self.assertNotEqual(row['inode'],right[name]['inode'])
                right[name]['device']+=1
                with self.assertRaisesRegex(ValueError,'保护文件身份/权限变化'):
                    m.verify_s0_protection(self.bindings,'final')
                right[name]['device']-=1
        for name in ('rx_trust_marker','rx_trust_marker.bak'):
            Path(right[name]['path']).write_bytes(b'0\n')
        with self.assertRaisesRegex(ValueError,'保护文件身份/权限变化'):
            m.verify_s0_protection(self.bindings,'authorized')

    def test_eacces_identity_uses_original_privileged_snapshot_and_preserves_unknown(self):
        identity={'pid':123,'start_ticks':0,'boot_id':'boot','pgid':123,'uids':[os.getuid()]*4,
                  'exe':'/bound/daemon','argv':['/bound/daemon']}
        executor=SimpleNamespace(output=self.output,process_snapshots=[])
        probe=lambda _:None
        helper=SimpleNamespace(read_process=lambda _:{'status':'permission'},privileged_process_probe=probe)
        def snapshot(*args,**kwargs):
            self.assertEqual(args[0],123);self.assertIs(kwargs['probe_runner'],probe)
            return {'related':[identity],'unreadable':[],'clean':False,'ordinary_denied':[{'pid':123}]}
        helper.process_snapshot=snapshot
        self.assertEqual(m.s0_process_identity(executor,helper,123),identity)
        self.assertEqual(executor.process_snapshots[-1]['ordinary_denied'],[{'pid':123}])
        helper.process_snapshot=lambda *args,**kwargs:{'related':[identity],'unreadable':[{'pid':999}]}
        with self.assertRaises(ValueError):m.s0_process_identity(executor,helper,123)
        helper.read_process=lambda _:{'status':'vanished'}
        with self.assertRaises(ValueError):m.s0_process_identity(executor,helper,123)

    def test_running_process_allowance_is_full_identity_only_and_never_changes_raw_clean(self):
        identity={'pid':123,'start_ticks':0,'boot_id':'boot','pgid':123,'uids':[os.getuid()]*4,
                  'exe':'/bound/daemon','argv':['/bound/daemon','--config','/bound/config']}
        executor=SimpleNamespace(output=self.output,process_snapshots=[])
        def helper(row,unreadable=()):
            return SimpleNamespace(privileged_process_probe=lambda _:None,
                process_snapshot=lambda *args,**kwargs:{'clean':False,'related':[row],'unreadable':list(unreadable)})
        snapshot=m.inspect_processes(executor,self.bindings,helper(identity),owned=[identity])
        self.assertFalse(snapshot['clean']);self.assertTrue(snapshot['s0_owned_only'])
        for key,value in (('pid',124),('start_ticks',1),('boot_id','other'),('pgid',125),
                          ('uids',[0]*4),('exe','/other'),('argv',['/other'])):
            bad={**identity,key:value}
            with self.subTest(key=key),self.assertRaises(ValueError):
                m.inspect_processes(executor,self.bindings,helper(bad),owned=[identity])
        with self.assertRaises(ValueError):
            m.inspect_processes(executor,self.bindings,helper(identity,[{'pid':999}]),owned=[identity])

    def run_bridge(self,fault=None,*,pin_latches=False):
        events=[];identities={};daemon_locks=[];key,active_side=m.s0_plan_entry(self.plan);contract=self.plan[key]
        held_side='left' if active_side=='right' else 'right'
        pin_fds={};actual_open=os.open
        latch_paths={self.bindings['protection'][active_side][name]['path'] for name in ('estop_latch','estop_latch.bak')}
        def open_pin(path,flags,*args,**kwargs):
            fd=actual_open(path,flags,*args,**kwargs)
            if pin_latches and str(path) in latch_paths:
                self.assertEqual(flags,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC)
                self.assertEqual(m.fcntl.fcntl(fd,m.fcntl.F_GETFL)&os.O_ACCMODE,os.O_RDONLY)
                self.assertTrue(m.fcntl.fcntl(fd,m.fcntl.F_GETFD)&m.fcntl.FD_CLOEXEC)
                pin_fds[str(path)]=fd
            return fd
        executor=m.RealExecutor(self.bindings['tool'],self.output);self.addCleanup(executor.journal.close)
        executor.t=0;executor.now=lambda:executor.t
        executor.sleep=lambda seconds,cleanup=False:setattr(executor,'t',executor.t+round(seconds*1e9))
        engine=m.ProbeEngine(executor,self.plan);report={'plan':self.plan}
        def locked(path):
            with Path(path).open('rb') as stream:
                try:m.fcntl.flock(stream,m.fcntl.LOCK_EX|m.fcntl.LOCK_NB)
                except BlockingIOError:return True
                m.fcntl.flock(stream,m.fcntl.LOCK_UN);return False
        def locks_present(side):
            return all(locked(self.bindings['protection'][side][name]['path']) for name in ('rx_trust.lock','estop_latch.lock'))
        class Process:
            def __init__(process,pid,argv):
                process.pid=pid;process.done=False;process.argv=list(argv);process.signals=[]
                identities[pid]={'pid':pid,'start_ticks':12,'boot_id':'fixture-boot','pgid':pid,
                    'uids':[os.getuid()]*4,'exe':argv[0],'argv':list(argv)}
            def poll(process):return 0 if process.done else None
            def send_signal(process,signum):
                events.append(('signal',signum));process.signals.append(signum)
            def wait(process,timeout):
                events.append(('wait',timeout))
                if fault=='daemon_timeout':raise subprocess.TimeoutExpired(process.argv,timeout)
                process.done=True;identities.pop(process.pid,None)
                for stream in daemon_locks:stream.close()
                return 0
        def spawn(argv,**kwargs):
            events.append(('spawn',));self.assertTrue(locks_present(held_side));self.assertFalse(locks_present(active_side))
            process=Process(910001,argv)
            for name in ('rx_trust.lock','estop_latch.lock'):
                path=Path(self.bindings['protection'][active_side][name]['path'])
                stream=path.open('r+b');daemon_locks.append(stream);m.fcntl.flock(stream,m.fcntl.LOCK_EX|m.fcntl.LOCK_NB)
                stream.seek(0);stream.truncate();stream.write(f'{process.pid}\n'.encode());stream.flush()
            for name in ('rx_trust_marker','rx_trust_marker.bak'):
                Path(self.bindings['protection'][active_side][name]['path']).write_bytes(b'1\n')
            kwargs['stdout'].write(b'No config\nboot_ready spawned_ns=1 first_exit_ns=2 ready_ns=3\n');kwargs['stdout'].flush()
            if fault=='daemon_bind':identities[process.pid]['argv']=['wrong']
            return process
        def command(argv,**kwargs):
            if kwargs.get('s0_control'):
                operation=argv[2];events.append((operation,))
                self.assertTrue(locks_present(held_side))
                self.assertFalse(locked(self.bindings['protection'][active_side]['rx_trust.lock']['path']))
                if operation=='authorize':self.assertTrue(locked(self.bindings['protection'][active_side]['estop_latch.lock']['path']))
                if operation=='authorize' and pin_latches:
                    self.assertEqual(set(pin_fds),latch_paths)
                    for name in ('estop_latch','estop_latch.bak'):
                        ref=self.bindings['protection'][active_side][name];info=os.fstat(pin_fds[ref['path']])
                        self.assertEqual((info.st_dev,info.st_ino,info.st_nlink),(ref['device'],ref['inode'],1))
                for name in ('rx_trust_marker','rx_trust_marker.bak'):
                    Path(self.bindings['protection'][active_side][name]['path']).write_bytes(b'0\n' if operation=='authorize' else b'1\n')
                if operation=='authorize' and fault=='authorize':raise ValueError('授权已生效但回执失败')
                return ''
            self.assertTrue(kwargs.get('s0_sender'));self.assertIn('--s0-position-hold' if active_side=='left' else '--s0-mixed-hold',argv);events.append(('sender',));self.assertTrue(locks_present(held_side))
            self.assertTrue(locks_present(active_side));executor.child=Process(910002,argv);executor.s0_sender_started=True
            executor.s0_bind_process(executor.child,argv,'sender');kwargs['mixed_stop_check']()
            result=self.receipt()
            if fault=='sender':result.update(result='FAIL',error='未知收尾',manual_power_cut_required=True)
            for name in ('estop_latch','estop_latch.bak'):
                ref=self.bindings['protection'][active_side][name];path=Path(ref['path'])
                if pin_latches:
                    for value in (b'0\n',b'1\n'):
                        replacement=self.root/'replacement-latch';replacement.write_bytes(value);replacement.chmod(0o640)
                        replacement.replace(path)
                        info=os.fstat(pin_fds[ref['path']])
                        self.assertEqual((info.st_dev,info.st_ino,info.st_nlink),(ref['device'],ref['inode'],0))
                        self.assertNotEqual(path.stat().st_ino,info.st_ino)
                        self.assertEqual(os.pread(pin_fds[ref['path']],2,0),b'1\n')
                else:path.write_bytes(b'0\n' if fault=='latch' else b'1\n')
            Path(argv[argv.index('--output')+1]).write_text(json.dumps(result))
            identities.pop(executor.child.pid);executor.child=None
            executor.commands.append({'reaped':True,'exit_code':1 if fault=='sender' else 0})
            if fault=='sender_stop':raise ValueError('原stopcheck拒绝')
            return ''
        executor.command=command
        helper=SimpleNamespace(clean_environment=lambda env:dict(env),
            read_process=lambda pid:{'status':'observed','identity':identities[pid]},
            privileged_process_probe=lambda _:None,
            process_snapshot=lambda *args,**kwargs:{'clean':not identities,'related':list(identities.values()),'unreadable':[]})
        def holder(*args,**kwargs):
            events.append(('holder',));self.assertFalse(identities)
            self.assertTrue(locks_present(held_side));self.assertTrue(locks_present(active_side));return {}
        def check_owned(*args):self.assertTrue(locks_present(active_side))
        with contextlib.ExitStack() as stack:
            locks={}
            for side in m.SERIALS:
                for name in ('rx_trust.lock','estop_latch.lock'):
                    stream=stack.enter_context(Path(self.bindings['protection'][side][name]['path']).open('rb'))
                    m.fcntl.flock(stream,m.fcntl.LOCK_EX|m.fcntl.LOCK_NB);locks[(side,name)]=stream
            with (patch.object(m.os,'open',side_effect=open_pin),
                  patch.object(m.subprocess,'Popen',side_effect=spawn),patch.object(m,'validate_go'),
                  patch.object(m,'verify_s0_daemon_locks',side_effect=check_owned),
                  patch.object(m,'inspect_holder',side_effect=holder),patch.object(m.time,'time_ns',return_value=0)):
                if fault:
                    with self.assertRaises((ValueError,KeyError)) as caught:
                        m.run_s0_mixed_hold(engine,{'expires_at_ns':self.expiry},helper,locks,report)
                    events.append(('primary_error',str(caught.exception)))
                else:m.run_s0_mixed_hold(engine,{'expires_at_ns':self.expiry},helper,locks,report)
            self.assertTrue(locks_present(held_side))
            for fd in pin_fds.values():
                with self.assertRaises(OSError):os.fstat(fd)
        for stream in daemon_locks:
            if not stream.closed:stream.close()
        return executor,engine,report,events

    def test_handoff_keeps_left_locks_authorizes_before_daemon_and_reaps_before_uart_permission(self):
        executor,engine,report,events=self.run_bridge()
        self.assertEqual(events[:3],[('authorize',),('spawn',),('sender',)])
        self.assertLess(events.index(('signal',signal.SIGTERM)),events.index(('wait',8.0)))
        self.assertLess(events.index(('wait',8.0)),events.index(('holder',)))
        self.assertTrue(executor.s0_post_ready);self.assertIsNone(executor.s0_daemon)
        self.assertEqual(len(engine.rows),7);self.assertFalse(engine.manual_cut)
        self.assertTrue(all(row['anchor_source']=='sender_frozen_joint_state' for row in engine.rows[:6]))
        self.assertFalse(any('hold_verification' in row for row in engine.rows))
        self.assertTrue(report['s0_daemon']['reaped'])

    def test_original_latch_fds_survive_two_renames_and_close_after_final_protection_check(self):
        for name in ('estop_latch','estop_latch.bak'):
            ref=self.bindings['protection']['right'][name];path=Path(ref['path']);path.chmod(0o600)
            self.bindings['protection']['right'][name]=self.reference(path)
        executor,engine,report,events=self.run_bridge(pin_latches=True)
        self.assertTrue(executor.s0_post_ready);self.assertFalse(engine.manual_cut)
        for name in ('estop_latch','estop_latch.bak'):
            path=self.bindings['protection']['right'][name]['path']
            row=next(row for row in report['s0_final_protection'] if row['path']==path)
            self.assertEqual(row['mode'],0o640)
            self.assertNotEqual(row['inode'],self.bindings['protection']['right'][name]['inode'])

    def test_authorize_unknown_outcome_revokes_once_before_any_daemon_or_sender(self):
        executor,engine,report,events=self.run_bridge('authorize')
        self.assertEqual(events[:2],[('authorize',),('revoke',)])
        self.assertFalse(any(event[0] in ('spawn','sender') for event in events));self.assertTrue(executor.s0_post_ready)

    def test_unreaped_daemon_blocks_uart_and_keeps_handle_without_kill(self):
        executor,engine,report,events=self.run_bridge('daemon_timeout')
        self.assertFalse(executor.s0_post_ready);self.assertIsNotNone(executor.s0_daemon);self.assertTrue(engine.manual_cut)
        self.assertFalse(any(event[0]=='holder' for event in events))
        self.assertEqual([event for event in events if event[0]=='signal'],[('signal',signal.SIGTERM)])

    def test_bind_failure_still_terminates_and_waits_for_its_owned_daemon(self):
        executor,engine,report,events=self.run_bridge('daemon_bind')
        self.assertIn(('signal',signal.SIGTERM),events);self.assertIn(('wait',8.0),events)
        self.assertIsNone(executor.s0_daemon);self.assertTrue(engine.manual_cut)
        self.assertTrue(report['s0_cleanup_errors']);self.assertFalse(any(event[0]=='sender' for event in events))

    def test_sender_unknown_cleanup_stays_manual_even_after_files_and_locks_return(self):
        executor,engine,report,events=self.run_bridge('sender')
        self.assertTrue(executor.s0_post_ready);self.assertTrue(engine.manual_cut)
        self.assertTrue(report['s0_sender_result']['manual_power_cut_required'])

    def test_sender_report_is_collected_after_stopcheck_error_without_clearing_original_failure(self):
        executor,engine,report,events=self.run_bridge('sender_stop')
        self.assertIn(('primary_error','原stopcheck拒绝'),events)
        self.assertTrue(engine.manual_cut);self.assertTrue(executor.s0_post_ready)
        self.assertEqual(report['s0_sender_result']['result'],'SOFTWARE_PATH_PASS')
        path=Path(report['s0_sender_result_ref']['path']);raw=path.read_bytes()
        self.assertEqual(report['s0_sender_result_ref']['sha256'],hashlib.sha256(raw).hexdigest())
        self.assertEqual(report['s0_sender_result_ref']['bytes'],len(raw))

    def test_latch_not_persisted_still_reaps_daemon_and_forbids_uart(self):
        executor,engine,report,events=self.run_bridge('latch')
        self.assertIsNone(executor.s0_daemon);self.assertTrue(engine.manual_cut);self.assertFalse(executor.s0_post_ready)
        self.assertIn(('wait',8.0),events)


class S0PositionBridgeTests(unittest.TestCase):
    run_bridge=S0MixedBridgeTests.run_bridge

    def setUp(self):
        S0MixedBridgeTests.setUp(self)
        config=write_bus_fixture(self.root/'left-position.toml',side='left')
        calibration=self.root/'calibration.json'
        config.write_text('[robot]\nid="bridge_fixture"\nconfig_version="test"\n'+config.read_text()+
            '\nid="left_arm_full_with_head"\nauto_enable=false\ncalibration_paths='+json.dumps([str(calibration)])+'\n')
        m.configure_bus_mapping(BUS_FIXTURE_PATH,config)
        self.bindings['s0_hardware_id']='left_arm_full_with_head'
        self.bindings['refs'].append(self.reference(config))
        with patch.object(m,'MAINTENANCE_NAMESPACE',self.root.parent):
            self.plan=m.make_plan(self.bindings,self.output,'bridge',self.root,s0_position_hold=True)

    def receipt(self):
        contract=self.plan['s0_position_hold'];enabled=1_000_000_000;start=enabled+100_000_000;end=start+20_000_000_000
        cycles=[];samples=[]
        for index in range(400):
            scheduled=start+index*50_000_000;source=min(scheduled+200_000_000,enabled+21_000_000_000)
            if index==0:source=min(source,enabled+200_000_000)
            cycles.append({'cycle':index,'scheduled_ns':scheduled,'publish_attempt_ns':scheduled,
                'publish_return_ns':scheduled+1000,'local_deadline_ns':scheduled+50_000_000,
                'source_deadline_ns':source})
            samples.append({'cycle':index,'stamp_ns':scheduled+2000,'observed_ns':scheduled+3000,'raw':[1000]*8})
        return {'schema':'SUPERVISED_S0_POSITION_HOLD_V1','result':'SOFTWARE_PATH_PASS',
            'config':contract['config']['path'],'hardware_id':contract['hardware_id'],'motor_ids':contract['motor_ids'],
            'layout_id':123,'go_expires_at_unix_ns':self.expiry,'cleanup_errors':[],'manual_power_cut_required':False,
            'hold_position_verified':True,'disable_retired':True,'torque_off_confirmed':True,'final_estop_published':True,
            'physical_motion':'NOT_OBSERVED','stop51':'NOT_RUN','full_hardware_acceptance':False,'deployable':False,
            'anchor':{'raw':[1000]*8,'target':[0.0]*8,'stamp_ns':enabled},'enable_attempt_ns':enabled,
            'enable_deadline_ns':enabled+21_000_000_000,'enable_transition_deadline_ns':enabled+300_000_000,
            'enable_confirmed_ns':start,'hold_start_ns':start,'hold_end_ns':end,
            'disable_attempt_ns':end,'torque_off_confirmed_ns':end+100_000_000,
            'final_estop_attempt_ns':end+100_000_000,'final_estop_publish_return_ns':end+100_001_000,
            'finished_ns':end+100_002_000,'cycles':cycles,'hold_samples':samples}

    def test_plan_only_and_left_plan_exclude_all_other_actions_and_sensor(self):
        with patch.object(m.subprocess,'Popen',side_effect=AssertionError('PLAN_ONLY不得启动')),contextlib.redirect_stdout(io.StringIO()) as out:
            self.assertEqual(run_main(['--s0-position-hold']),0)
        self.assertEqual(json.loads(out.getvalue())['scope'],m.S0_POSITION_SCOPE)
        for kwargs in ({'s0_mixed_hold':True},{'bus2_mixed_raw_hold':True},{'right_wheel_pulse':True},
                       {'bus2_arm_range_only':True},{'right_gripper_release':True},{'position_target_names':['head-1']}):
            with self.subTest(kwargs=kwargs),patch.object(m,'MAINTENANCE_NAMESPACE',self.root.parent),self.assertRaises(ValueError):
                m.make_plan(self.bindings,self.output,'bridge',self.root,s0_position_hold=True,**kwargs)
        config=Path(m.LEFT_BUS_CONFIG['path']);original=config.read_text()
        for text in (original+'\n[[sensor]]\nhardware_id="left_arm_full_with_head"\n',
                     original.replace('auto_enable=false','auto_enable=true'),original+'feetech_bus2_mixed_raw=true\n'):
            config.write_text(text)
            with self.assertRaises(ValueError):m.s0_position_contract(self.bindings)
        config.write_text(original)
        self.assertTrue(all(row['decision']=='DEFERRED' for row in self.plan['base_targets']))

    def test_real_left_config_and_calibration_are_bound_and_rechecked(self):
        m.verify_s0_mixed_refs(self.plan)
        config=Path(m.LEFT_BUS_CONFIG['path']);original=config.read_bytes()
        self.bindings['refs']=[ref for ref in self.bindings['refs'] if ref['path']!=str(config)]
        with self.assertRaisesRegex(ValueError,'实际配置'):m.s0_position_contract(self.bindings)
        self.bindings['refs'].append(self.reference(config))
        config.write_bytes(original+b'\n')
        with self.assertRaises(ValueError):m.verify_s0_mixed_refs(self.plan)

    def test_receipt_rejects_false_success_missing_samples_stale_feedback_and_deadline_changes(self):
        result=self.receipt()
        self.assertEqual(m.parse_s0_position_result(json.dumps(result),self.plan,self.expiry),result)
        for key,value in (('schema','SUPERVISED_S0_MIXED_V1'),('hardware_id','right'),('motor_ids',list(range(8))),
                          ('hold_position_verified',False),('torque_off_confirmed',False),('disable_retired',False),
                          ('manual_power_cut_required',True),('full_hardware_acceptance',True),('deployable',True),
                          ('enable_deadline_ns',result['enable_deadline_ns']+1),
                          ('enable_transition_deadline_ns',result['enable_transition_deadline_ns']+1),
                          ('enable_confirmed_ns',result['enable_transition_deadline_ns']+1),('hold_end_ns',result['hold_end_ns']-1),
                          ('torque_off_confirmed_ns',result['disable_attempt_ns']+200_000_001),
                          ('finished_ns',result['enable_attempt_ns']+21_000_000_001)):
            bad=copy.deepcopy(result);bad[key]=value
            with self.subTest(key=key),self.assertRaises(ValueError):m.parse_s0_position_result(json.dumps(bad),self.plan,self.expiry)
        for mutate in (lambda row:row['cycles'].pop(),lambda row:row['hold_samples'].pop(),
                       lambda row:row['motor_ids'].__setitem__(0,True),
                       lambda row:row['cycles'][3].update(cycle=True),
                       lambda row:row['cycles'][3].update(source_deadline_ns=0),
                       lambda row:row['hold_samples'][0].update(stamp_ns=row['anchor']['stamp_ns']),
                       lambda row:row['hold_samples'][4].update(stamp_ns=row['hold_samples'][3]['stamp_ns']),
                       lambda row:row['hold_samples'][8].update(stamp_ns=row['hold_samples'][8]['observed_ns']-200_000_000),
                       lambda row:row['hold_samples'][1].update(observed_ns=row['hold_start_ns']+100_000_000),
                       lambda row:row['hold_samples'][200]['raw'].__setitem__(7,1005),
                       lambda row:row['anchor']['raw'].pop()):
            bad=copy.deepcopy(result);mutate(bad)
            with self.assertRaises(ValueError):m.parse_s0_position_result(json.dumps(bad),self.plan,self.expiry)
        shortened=copy.deepcopy(result)
        shortened['anchor']['stamp_ns']=shortened['hold_start_ns']-199_000_000
        first_deadline=shortened['anchor']['stamp_ns']+200_000_000
        shortened['cycles'][0].update(local_deadline_ns=first_deadline,source_deadline_ns=first_deadline)
        self.assertEqual(m.parse_s0_position_result(json.dumps(shortened),self.plan,self.expiry),shortened)
        shortened['hold_samples'][0]['observed_ns']=first_deadline
        with self.assertRaisesRegex(ValueError,'逐拍反馈'):
            m.parse_s0_position_result(json.dumps(shortened),self.plan,self.expiry)

    def test_left_handoff_preserves_right_locks_and_returns_all_eight_position_rows(self):
        executor,engine,report,events=self.run_bridge(pin_latches=True)
        self.assertEqual(events[:3],[('authorize',),('spawn',),('sender',)])
        self.assertLess(events.index(('wait',8.0)),events.index(('holder',)))
        self.assertTrue(executor.s0_post_ready);self.assertFalse(engine.manual_cut)
        self.assertEqual([(row['side'],row['motor'],row['kind']) for row in engine.rows],
                         [('left',motor,'position_hold') for motor in range(1,9)])
        self.assertTrue(all(row['anchor_source']=='sender_frozen_joint_state' for row in engine.rows))
        report.update(go={'expires_at_ns':self.expiry},probes=engine.rows,s0_post_ready=True,
                      stop_verification={'status':'PASS'},identity_verification={'status':'PASS'},
                      position_verification={'status':'PASS'})
        self.assertEqual(m.classify_run_result(report),'PASS')
        for mutate in (lambda row:row['probes'].pop(),lambda row:row['probes'][0].update(side='right'),
                       lambda row:row['probes'][7].update(anchor_raw=1001),
                       lambda row:row.update(s0_post_ready=False),
                       lambda row:row.update(error='原始错误粘性'),
                       lambda row:row['plan']['base_targets'][0].update(decision='RAW_DIAGNOSTIC')):
            bad=copy.deepcopy(report);mutate(bad)
            self.assertEqual(m.classify_run_result(bad),'FAIL')

    def test_left_failure_reclaims_locks_preserves_failure_and_never_retries_sender(self):
        for fault in ('authorize','daemon_bind','sender','sender_stop','latch','daemon_timeout'):
            with self.subTest(fault=fault):
                self.output=self.root/f'failure-{fault}';self.output.mkdir();self.plan['output']=str(self.output)
                executor,engine,report,events=self.run_bridge(fault)
                self.assertLessEqual(sum(event[0]=='sender' for event in events),1)
                if fault=='authorize':
                    self.assertEqual(events[:2],[('authorize',),('revoke',)])
                    self.assertTrue(executor.s0_post_ready)
                elif fault=='daemon_timeout':
                    self.assertFalse(executor.s0_post_ready);self.assertIsNotNone(executor.s0_daemon)
                elif fault=='latch':self.assertFalse(executor.s0_post_ready)
                else:self.assertTrue(executor.s0_post_ready)
                if fault!='authorize':self.assertTrue(engine.manual_cut)
                # 下一次仅为独立软件fixture；恢复原保护绑定，不复用任何运行结果。
                for side,files in self.bindings['protection'].items():
                    for name,ref in list(files.items()):
                        path=Path(ref['path']);path.write_bytes(b'1\n');files[name]=self.reference(path)
                with patch.object(m,'MAINTENANCE_NAMESPACE',self.root.parent):
                    self.plan=m.make_plan(self.bindings,self.output,'bridge',self.root,s0_position_hold=True)

    def test_left_go_requires_fresh_matching_scope_and_has_no_wheel_action_requirements(self):
        physical={side:{**dict.fromkeys(m.FACTS,True),'confirmed_at_ns':100} for side in m.SERIALS}
        physical['left'].update(position_hold_confirmation_scope=m.S0_POSITION_SCOPE,
                                position_hold_confirmation_answer='本轮左八关节原位保持20秒')
        go={'schema':'MAINTENANCE_GO_V2','artifact_root':self.plan['artifact_root'],'issued_by':'root',
            'plan_sha256':m.digest(self.plan),'issued_at_ns':100,'expires_at_ns':600_000_000_100,
            'host':self.bindings['host'],'source':'本轮现场','physical':physical}
        m.validate_go(go,self.plan,101)
        for mutate in (lambda row:row.update(expires_at_ns=row['expires_at_ns']+1),
                       lambda row:row['physical']['left'].update(position_hold_confirmation_scope=m.S0_MIXED_SCOPE),
                       lambda row:row['physical']['left'].update(position_hold_confirmation_answer=''),
                       lambda row:row['physical']['right'].update(continuous_presence_until_cleanup=False)):
            bad=copy.deepcopy(go);mutate(bad)
            with self.assertRaises(ValueError):m.validate_go(bad,self.plan,101)

    def test_only_left_persistent_replacement_is_allowed_and_final_positions_keep_four_raw_limit(self):
        left=self.bindings['protection']['left'];right=self.bindings['protection']['right']
        replacement=self.root/'replacement';replacement.write_bytes(b'1\n')
        replacement.replace(Path(left['rx_trust_marker']['path']))
        m.verify_s0_protection(self.bindings,'final','left')
        Path(right['rx_trust_marker']['path']).write_bytes(b'0\n')
        with self.assertRaises(ValueError):m.verify_s0_protection(self.bindings,'final','left')
        baseline={key:{'Present_Position':1000,'Goal_Position':1000} for key in m.MOTOR_KEYS}
        probes=[{'side':'left','motor':motor,'kind':'position_hold','anchor_raw':1000,'status':'PASS'} for motor in range(1,9)]
        actual=copy.deepcopy(baseline);actual['left:8']['Present_Position']=1004
        self.assertEqual(m.classify_position_snapshot(actual,baseline,probes)['status'],'PASS')
        actual['left:8']['Present_Position']=1005
        self.assertEqual(m.classify_position_snapshot(actual,baseline,probes)['status'],'FAIL')
        actual=copy.deepcopy(baseline);actual['right:1']['Goal_Position']=1001
        self.assertEqual(m.classify_position_snapshot(actual,baseline,probes)['status'],'FAIL')


if __name__=='__main__': unittest.main(verbosity=2)
