#!/bin/bash
# 本轮非UART身份与占用者后验检查；不打开串口、不执行控制产物。
set -eu
export PYTHONDONTWRITEBYTECODE=1
exec /home/loosqk/.local/share/uv/python/cpython-3.12-linux-aarch64-gnu/bin/python3.12 - <<'PY'
import hashlib,json,os,stat,subprocess,time
from pathlib import Path
b=Path('/home/loosqk/robot-checks/h2-exit-observation-20261009-r1')
out=b/'final-buses-r2';out.mkdir()
result={'status':'RUNNING','read_only':True,'uart_opened':False,'started_ns':time.time_ns(),'commands':[],'checks':[],'errors':[]}
def save():
 (out/'result.json').write_text(json.dumps(result,ensure_ascii=False,indent=2)+'\n')
def require(value,message):
 if not value:raise ValueError(message)
def command(name,argv):
 row={'name':name,'argv':argv,'started_ns':time.time_ns(),'signals_sent':[]};result['commands'].append(row);save()
 child=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
 row['pid']=child.pid;stdout,stderr=child.communicate();row.update(exit_code=child.returncode,reaped=True,finished_ns=time.time_ns(),stdout_sha256=hashlib.sha256(stdout).hexdigest(),stderr_sha256=hashlib.sha256(stderr).hexdigest(),stdout_bytes=len(stdout),stderr_bytes=len(stderr))
 (out/(name+'.stdout')).write_bytes(stdout);(out/(name+'.stderr')).write_bytes(stderr);save();return row,stdout,stderr
try:
 post=json.loads((b/'right-h2-final-readonly-r2/result.json').read_text());prior=json.loads((b/'right-h2-cleanup-r2/result.json').read_text())
 require(post['readonly_cleanup_status']=='PASS' and post['driver_evidence_status']=='PASS' and len(post['checks'])==11 and all(x['status']=='PASS' for x in post['checks']),'最终11检查不完整')
 result['post_check_status']=post['readonly_cleanup_status'];result['original_h2_status']=post['driver_status']
 old=next(x['value'] for x in prior['checks'] if x['name']=='state_files');new=next(x['value'] for x in post['checks'] if x['name']=='state_files');require(old==new,'保护文件与读回前清场变化')
 result['checks'].append({'name':'protection_unchanged','status':'PASS'})
 require(Path('/proc/sys/kernel/random/boot_id').read_text().strip()=='6cf7ee3e-a3c5-4c91-80bc-4ca924818b53','boot改变')
 for side,serial in [('left','5A7C121427'),('right','5A7C122340')]:
  device=Path('/dev/serial/by-id/usb-1a86_USB_Single_Serial_'+serial+'-if00')
  row,stdout,stderr=command(side+'-udev',['udevadm','info','--query=property','--name',str(device)])
  require(row['exit_code']==0 and stderr==b'','udev失败')
  lines=stdout.decode().splitlines()
  for key,value in {'ID_VENDOR_ID':'1a86','ID_MODEL_ID':'55d3','ID_SERIAL_SHORT':serial,'ID_USB_DRIVER':'cdc_acm'}.items():require([x for x in lines if x.startswith(key+'=')]==[key+'='+value],'USB身份不符')
  s=device.stat();require(stat.S_ISCHR(s.st_mode) and os.access(device,os.R_OK|os.W_OK),'USB类型或权限改变')
  result['checks'].append({'name':side+'-usb','status':'PASS','path':str(device),'resolved':str(device.resolve()),'rdev':s.st_rdev,'uid':s.st_uid,'gid':s.st_gid,'mode':stat.S_IMODE(s.st_mode)})
  paths=[device]+[Path('/var/lib/robot/xlerobot_'+side)/name for name in ('rx_trust.lock','estop_latch.lock')]
  for index,path in enumerate(paths):
   for elevated in (False,True):
    name=side+'-holder-'+str(index)+('-sudo' if elevated else '-ordinary');argv=(['sudo','-n'] if elevated else [])+['fuser',str(path)]
    row,stdout,stderr=command(name,argv);require(row['exit_code']==1 and stdout==stderr==b'','占用者非空或检查失败');result['checks'].append({'name':name,'status':'PASS','path':str(path)})
 elf=Path('/home/loosqk/robot-checks/h2-right-ready-20261009-r1/readback-tool-r1/feetech_poke');data=elf.read_bytes();require(len(data)==1510912 and hashlib.sha256(data).hexdigest()=='13c97115b89d9f8780ea8b9cac80a091ddcc2b73549d34b2d5cbad83c718ffe1','读回ELF后验变化');result['checks'].append({'name':'readback_elf','status':'PASS','sha256':hashlib.sha256(data).hexdigest(),'bytes':len(data)})
 require(hashlib.sha256((b/'right-h2-run-02/run.json').read_bytes()).hexdigest()=='0087b8945543d6199c214e386f8fbf5be5485bcd4bdb47bc3d512924711d09cb','原H2变化')
 result['checks'].append({'name':'original_h2_unchanged','status':'PASS'});result['status']='PASS'
except BaseException as error:result['status']='FAIL';result['errors'].append(repr(error))
result['finished_ns']=time.time_ns();save();print(json.dumps(result,ensure_ascii=False),flush=True)
raise SystemExit(0 if result['status']=='PASS' else 1)
PY
