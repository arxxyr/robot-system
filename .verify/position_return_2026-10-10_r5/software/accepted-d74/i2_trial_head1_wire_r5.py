# 爸爸，仅本轮head1(left7)临时I2候选；既有右8/9停止PV附原帧，默认PLAN_ONLY。
import argparse, hashlib, importlib.util, json, re, stat, sys
from pathlib import Path
sys.dont_write_bytecode=True
SOURCE_SHA='d97e0da9b792d048681053054296de66e7f3ef37cf392852784587b51a8b1d41'
RAW_TOOL={'path':'/home/loosqk/robot-checks/h2-right-ready-20261009-r1/readback-tool-r1/feetech_poke','sha256':'13c97115b89d9f8780ea8b9cac80a091ddcc2b73549d34b2d5cbad83c718ffe1','bytes':1510912}
TOOL={'path':'/home/loosqk/robot-checks/h2-serial-recovery-20260930-r1/readback-tool-r1/feetech_poke','sha256':'f2014ee4008311e7a8725235a39f0d1b3913391f17cbd977b7441f120539114b','bytes':1469904}
STRACE={'path':'/usr/bin/strace','sha256':'5392f54d889a87341d49f970d91e21ab8c1f70a835ac9ead6ee4d17e532ae7f7','bytes':1583504}
RIGHT_DEVICE='/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C122340-if00'
STRACE_FLAGS=('-ttt','-xx','-s','256','-yy','-e','trace=openat,close,read,write,ioctl')
TARGET=('left',7,'head_motor_1')
BACKUP={'Model_Number': 777, 'Firmware_Major_Version': 3, 'Firmware_Minor_Version': 10, 'Operating_Mode': 0, 'Torque_Enable': 0, 'Goal_Position': 0, 'Present_Position': 2642, 'Goal_Velocity': 0, 'Present_Velocity': 0, 'Phase': 12, 'P_Coefficient': 16, 'D_Coefficient': 43, 'I_Coefficient': 0, 'CW_Dead_Zone': 1, 'CCW_Dead_Zone': 1, 'Lock': 1, 'Min_Position_Limit': 797, 'Max_Position_Limit': 3081, 'Homing_Offset': 1303}
META={'Model_Number':(3,2),'Firmware_Major_Version':(0,1),'Firmware_Minor_Version':(1,1),'Operating_Mode':(33,1),'Torque_Enable':(40,1),'Goal_Position':(42,2),'Present_Position':(56,2),'Goal_Velocity':(46,2),'Present_Velocity':(58,2),'Phase':(18,1),'P_Coefficient':(21,1),'D_Coefficient':(22,1),'I_Coefficient':(23,1),'CW_Dead_Zone':(26,1),'CCW_Dead_Zone':(27,1),'Lock':(55,1),'Min_Position_Limit':(9,2),'Max_Position_Limit':(11,2),'Homing_Offset':(31,2)}
# GP/PP动态值不与旧备份硬比。
PARAMETERS=tuple(k for k in BACKUP if k not in ('Goal_Position','Present_Position'))
RESTORE_PARAMETERS=('I_Coefficient','Lock','P_Coefficient','D_Coefficient','CW_Dead_Zone','CCW_Dead_Zone')

def require(value,message):
 if not value:raise ValueError(message)

def same_reference(row,expected):
 return all(row.get(key)==value for key,value in expected.items())

def required_references(bindings,reference):
 require(same_reference(bindings['tool'],TOOL),'必须原冻结f201工具完整引用')
 require(all(any(same_reference(row,ref) for row in bindings['refs']) for ref in (reference,RAW_TOOL,TOOL,STRACE)),
         '候选/raw/f201/strace完整引用必须加入本轮refs')
 require(STRACE['path'] in bindings['binary_paths'],'strace必须加入本轮binary_paths')

def install_stop_wire_capture(m):
 # 仅包装既有精确命令；原poke读值/17×3停止门仍使用同一份stdout。
 original=m.RealExecutor.command
 def command(io,argv,**kwargs):
  exact=next((motor for motor in (8,9) if argv==[TOOL['path'],RIGHT_DEVICE,str(motor),'read','Present_Velocity']),None)
  if exact is None:return original(io,argv,**kwargs)
  m.verify_ref(STRACE,executable=True)
  trace=io.output/f'right{exact}-normal-read-command_{io.sequence+1:04d}.strace'
  # 独占创建空原件后交strace；已有文件/符号链接不可覆盖。
  with trace.open('xb'):pass
  expanded=[STRACE['path'],*STRACE_FLAGS,'-o',str(trace),*argv]
  def preserve(success):
   try:
    info=trace.lstat()
    require(stat.S_ISREG(info.st_mode) and info.st_nlink==1,'strace原件不是独立普通文件')
    raw=trace.read_bytes()
    require(raw,'strace原件为空')
    if success:
     require(re.search(rb'(?:^|\n)[^\n]*\+\+\+ exited with 0 \+\+\+\n?\Z',raw),
             'strace原件未记录成功终态')
    io.commands[-1]['ordinary_read_trace']={'path':str(trace),'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest(),
     'original_argv':list(argv),'wire_frame_review':'PENDING_ROOT_REVIEW_OF_COMPLETE_STRACE'}
   except BaseException as error:
    io.evidence_errors.append('ordinary_read_trace:'+repr(error))
    if success:raise
  try:text=original(io,expanded,**kwargs)
  except BaseException:
   preserve(False)
   raise
  preserve(True)
  return text
 m.RealExecutor.command=command

def raw_read(m,engine,register,cleanup=False):
 require(register in META,'只读字段越界')
 m.verify_ref(RAW_TOOL,executable=True)
 device=m.DEVICES['left'];text=engine.io.command([RAW_TOOL['path'],device,'7','read-raw',register],cleanup=cleanup)
 require(len(text.splitlines())==1,'raw stdout行数错误')
 j=m.strict_json(text);address,width=META[register]
 require(set(j)=={'operation','serial_device','motor_id','register','requested_register_address','width','raw_unsigned','tx','rx'} and (j['operation'],j['serial_device'],j['motor_id'],j['register'],j['requested_register_address'],j['width'])==('sync_read',device,7,register,address,width),'raw字段身份错误')
 tx=[255,255,254,5,130,address,width,7];tx.append((~sum(tx[2:]))&255);rx=j['rx'];raw=j['raw_unsigned']
 require(type(raw) is int and j['tx']==tx and type(rx) is list and all(type(v) is int and 0<=v<=255 for v in rx) and len(rx)==6+width and rx[:5]==[255,255,7,width+2,0] and (sum(rx[2:])&255)==255 and int.from_bytes(bytes(rx[5:5+width]),'little')==raw,'raw原帧校验错误')
 return j

def write_i(m,engine,value,cleanup=False):
 require(type(value) is int and value in (0,2),'仅本轮I0/I2')
 m.verify_ref(RAW_TOOL,executable=True)
 text=engine.io.command([RAW_TOOL['path'],m.DEVICES['left'],'7','write','I_Coefficient',str(value)],cleanup=cleanup)
 require(text==f'write I_Coefficient@7 <- {value} ok\n','I写ACK stdout不匹配')

def run_trial(m,engine,side,motor,name,calibration,original_position,reader=raw_read,writer=write_i):
 require((side,motor,name)==TARGET,'本轮仅head1(left7)')
 engine.check_gate()
 trial={'status':'RUNNING','original_backup':BACKUP,'before_raw':{},'i2_raw':{},'restore_raw':{},'restore_status':'NOT_NEEDED','i2_write_attempted':False,'errors':[]}
 command=engine.io.command
 def guard(argv,**kwargs):
  if trial.get('uart_blocked') and str(argv[0]) in (engine.io.tool['path'],RAW_TOOL['path']):
   raise ValueError('I对照停止/恢复未确认，粘性拒绝后续UART')
  return command(argv,**kwargs)
 engine.io.command=guard
 motion_error=None;start_rows=len(engine.rows)
 def block(error):
  trial['uart_blocked']=True;engine.manual_cut=True;trial['errors'].append(repr(error))
  print('爸爸，I对照停止/恢复未确认，请立即人工断电；不再UART或动作。',flush=True)
 try:
  # 入口先确认断扭/静止；读取未知或非零立即封锁，不进入参数写或原finish的UART。
  for reg in ('Torque_Enable','Present_Velocity'):
   try:
    trial['before_raw'][reg]=reader(m,engine,reg)
    require(trial['before_raw'][reg]['raw_unsigned']==0,'入口停止未确认:'+reg)
   except BaseException as error:
    block(error);raise
  for reg in META:
   if reg not in trial['before_raw']:trial['before_raw'][reg]=reader(m,engine,reg)
  values={k:v['raw_unsigned'] for k,v in trial['before_raw'].items()}
  require(all(values[k]==BACKUP[k] for k in PARAMETERS),'本轮模式/安全/固件/控制参数与原备份不一致')
  require(values['Torque_Enable']==values['Present_Velocity']==0 and values['Lock']==1 and values['I_Coefficient']==0,'I试验入口非断扭/静止/I0/Lock1')
  require(values['Min_Position_Limit']<=values['Present_Position']<=values['Present_Position']+24<=values['Max_Position_Limit'],'当前起点/目标超内置限位')
  engine.check_gate()
  trial['i2_write_attempted']=True  # ACK不确定也必须进入恢复。
  writer(m,engine,2)
  for reg in ('I_Coefficient','Lock'):trial['i2_raw'][reg]=reader(m,engine,reg)
  require(trial['i2_raw']['I_Coefficient']['raw_unsigned']==2 and trial['i2_raw']['Lock']['raw_unsigned']==1,'I2/Lock1读回不符，不启动运动')
  original_position(engine,side,motor,name,calibration)
 except BaseException as error:
  motion_error=error;trial['errors'].append(repr(error))
 finally:
  if trial['i2_write_attempted']:
   trial['restore_status']='FAIL'
   try:
    require(not engine.manual_cut,'原position安全收尾已失败，不再UART或恢复写')
    # 原position已执行其safe_off；这里先独立确认断扭/静止，未知时不再写I。
    for reg in ('Torque_Enable','Present_Velocity'):
     trial['restore_raw'][reg]=reader(m,engine,reg,cleanup=True)
     require(trial['restore_raw'][reg]['raw_unsigned']==0,'恢复前目标停止未确认:'+reg)
    trial['restore_raw']['Lock']=reader(m,engine,'Lock',cleanup=True)
    require(trial['restore_raw']['Lock']['raw_unsigned']==1,'恢复前Lock非1/未知，禁止I0写')
    writer(m,engine,BACKUP['I_Coefficient'],cleanup=True)
    for reg in RESTORE_PARAMETERS:trial['restore_raw'][reg]=reader(m,engine,reg,cleanup=True)
    require(all(trial['restore_raw'][reg]['raw_unsigned']==BACKUP[reg] for reg in RESTORE_PARAMETERS),'I0/Lock/P/D/死区恢复不符')
    trial['restore_status']='PASS'
   except BaseException as error:block(error)
  if len(engine.rows)==start_rows:
   engine.rows.append({'side':side,'motor':motor,'name':name,'kind':'position','status':'FAIL','position_accuracy':{'status':'NOT_RUN','stages':{}},'cleanup_errors':[],'error':repr(motion_error)})
  row=engine.rows[start_rows];row['i_trial']=trial
  trial['status']='PASS' if motion_error is None and trial['restore_status']=='PASS' and not trial['errors'] else 'FAIL'
  if trial.get('uart_blocked'):
   row['status']='FAIL';row.setdefault('cleanup_errors',[]).append('I对照停止/参数恢复未确认；UART已封锁')
 if motion_error is not None:raise motion_error
 require(trial['status']=='PASS','I对照失败，原事实保留')

def load_source(path):
 require(hashlib.sha256(path.read_bytes()).hexdigest()==SOURCE_SHA,'原维护执行源SHA变化')
 spec=importlib.util.spec_from_file_location('maintenance_i_trial_original',path);m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
 return m

def main(argv=None):
 parser=argparse.ArgumentParser(description=__doc__)
 parser.add_argument('--maintenance',type=Path);parser.add_argument('--bindings',type=Path)
 parser.add_argument('--output',type=Path);parser.add_argument('--run-id');parser.add_argument('--artifact-root',type=Path)
 parser.add_argument('--execute',action='store_true');parser.add_argument('--approval',type=Path)
 options=parser.parse_args(argv)
 if not all((options.maintenance,options.bindings,options.output,options.run_id,options.artifact_root)):
  require(not options.execute,'执行输入不完整')
  print(json.dumps({'status':'PLAN_ONLY','target':TARGET,'register_writes':0,'motion_commands':0},ensure_ascii=False));return 0
 m=load_source(options.maintenance);bindings=m.strict_json(options.bindings.read_text())
 raw=Path(__file__).read_bytes();reference={'path':str(Path(__file__).resolve()),'sha256':hashlib.sha256(raw).hexdigest(),'bytes':len(raw)}
 required_references(bindings,reference)
 plan=m.make_plan(bindings,options.output,options.run_id,options.artifact_root,['head-1'])
 plan['i_trial']={'wrapper':reference,'raw_tool':RAW_TOOL,'target':TARGET,'original_raw_backup':BACKUP,'original_readonly_result_sha256':'f189503bc9fd8d125f39f2a1b86f7d24b6e23e7848d858ee25e34bc3f12015e6','temporary_i':2,'restore_i':0,
  'ordinary_stop_wire':{'tool':TOOL,'strace':STRACE,'flags':STRACE_FLAGS,'right_device':RIGHT_DEVICE,'motors':[8,9],
   'additional_uart_reads':0,'wire_frame_review':'PENDING_ROOT_REVIEW_OF_COMPLETE_STRACE'}}
 if not options.execute:
  print(json.dumps({'status':'PLAN_ONLY','plan_sha256':m.digest(plan),'plan':plan},ensure_ascii=False,indent=2));return 0
 require(options.approval is not None,'必须root本次新GO')
 go=m.strict_json(options.approval.read_text())
 # 原GO digest覆盖候选SHA、raw工具、单目标和参数备份；旧baseline GO必拒。
 m.validate_go(go,plan,__import__('time').time_ns())
 m.verify_ref(STRACE,executable=True)
 install_stop_wire_capture(m)
 original=m.ProbeEngine.position
 m.ProbeEngine.position=lambda engine,side,motor,name,calibration:run_trial(m,engine,side,motor,name,calibration,original)
 return m.execute(plan,go,options.approval)

if __name__=='__main__':
 try:sys.exit(main())
 except Exception as error:print('爸爸，I候选前置拒绝：'+repr(error),file=sys.stderr);sys.exit(2)
