#!/usr/bin/env python3
"""Actual 3-VM Linux service isolation/fault/exec-bounds acceptance, no mocks.
Only creates fresh diskless validation appliances; never consumes a user VM disk.
"""
import concurrent.futures, json, os, pathlib, signal, subprocess, tempfile, time, secrets
ROOT=pathlib.Path(__file__).resolve().parents[1]
BIN=ROOT/'target/debug/vm-control'
CONFIG=ROOT/'.local/run/linux-agent.json'
OUTPUT=ROOT/'.local/evidence/multivm.json'
base=json.loads(CONFIG.read_text())
if base.get('disk'):raise RuntimeError('This test only accepts diskless appliance configs')
OUTPUT.parent.mkdir(parents=True,exist_ok=True)
checks=[]
result={'fixture':'three Linux ARM initramfs appliances, NOT three OSs or desktop performance','allocation':{'each_cpus':2,'each_memory_mib':1024},'checks':checks,'started_at_unix':time.time()}
with tempfile.TemporaryDirectory(prefix='vm-multi-') as temp:
 state=pathlib.Path(temp)/'s'
 daemon=subprocess.Popen([str(BIN),'daemon','--state-dir',str(state),'--backend',str(ROOT/'.local/bin/vm-apple')],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
 ids=['multi-a','multi-b','multi-c']; leases={}; pids={}; owner=secrets.token_hex(32)
 def call(method,args={}):
  r=subprocess.run([str(BIN),'call','--state-dir',str(state),method,json.dumps(args)],capture_output=True,text=True,timeout=45)
  if r.returncode:raise RuntimeError(r.stderr)
  return json.loads(r.stdout)
 def execute(id,argv,timeout=5000):
  return call('exec',{'id':id,'owner':owner,'epoch':leases[id],'argv':argv,'timeout_ms':timeout})['result']
 try:
  for _ in range(100):
   if (state/'control.sock').exists():break
   time.sleep(.05)
  for id in ids:
   config={**base,'id':id,'serial_log':str(OUTPUT.parent/(id+'-serial.log'))}
   call('create',config);call('start',{'id':id})
  for id in ids:
   for _ in range(80):
    try:
     info=call('guest_info',{'id':id});break
    except RuntimeError:time.sleep(.1)
   else:raise RuntimeError('guest not ready: '+id)
   pids[id]=call('status',{'id':id})['pid']
   leases[id]=call('acquire_control',{'id':id,'owner':owner})['epoch']
  checks.append({'name':'three real independent guests ready','pids':pids,'guest_info':info})
  with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
   replies=list(pool.map(lambda id:execute(id,['/bin/sh','-c',f'echo {id} > /tmp/identity; cat /tmp/identity; uname -m']),ids))
  for id,reply in zip(ids,replies):assert reply['exit_code']==0 and reply['stdout']==id+'\naarch64\n',reply
  checks.append({'name':'concurrent guest execution and filesystem isolation','results':replies})
  unicode=execute(ids[0],['/bin/printf','你好 🙂'])
  assert unicode['stdout']=='你好 🙂',unicode
  checks.append({'name':'UTF-8 argv/output round trip','result':unicode})
  bounded=execute(ids[0],['/bin/sh','-c','yes z | head -c 100000'])
  assert len(bounded['stdout'])==65536 and bounded['stdout_truncated'],bounded
  checks.append({'name':'output limit','bytes':len(bounded['stdout']),'truncated':bounded['stdout_truncated']})
  with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
   slow=pool.submit(execute,ids[0],['/bin/sh','-c','sleep 20 & wait'],500)
   t=time.monotonic();fast=execute(ids[1],['/bin/echo','other-vm-responsive']);elapsed=time.monotonic()-t
   timeout=slow.result()
  assert timeout['timed_out'] and timeout['duration_ms']<2000,timeout
  assert fast['exit_code']==0 and elapsed<2,fast
  checks.append({'name':'deadline kills process group without blocking other VM','timeout':timeout,'other_vm_seconds':elapsed})
  os.kill(pids[ids[0]],signal.SIGKILL)
  for _ in range(40):
   crashed=call('status',{'id':ids[0]})
   if crashed['state']=='crashed':break
   time.sleep(.05)
  assert crashed['state']=='crashed',crashed
  survivors=[execute(id,['/bin/echo','survived']) for id in ids[1:]]
  assert all(r['stdout']=='survived\n' for r in survivors),survivors
  checks.append({'name':'one native worker crash leaves two VMs responsive','crashed_state':crashed['state'],'survivors':survivors})
  memory=subprocess.check_output(['ps','-o','pid=,rss=,%cpu=','-p',','.join(str(pids[i]) for i in ids[1:])],text=True)
  checks.append({'name':'diagnostic process RSS snapshot','output':memory,'limitation':'RSS only; excludes memory accounting needed for physical-footprint performance comparison'})
  # Controller-crash policy is fail-stop; inherited pipe EOF must end both workers.
  daemon.kill();daemon.wait(timeout=5)
  def alive(pid):
   try:os.kill(pid,0);return True
   except ProcessLookupError:return False
  deadline=time.monotonic()+10
  while any(alive(pids[i]) for i in ids[1:]) and time.monotonic()<deadline:time.sleep(.1)
  assert not any(alive(pids[i]) for i in ids[1:]),'worker orphan after controller crash'
  checks.append({'name':'daemon crash ends workers via EOF; no orphan VM','passed':True})
  # Reopen registry; never automatically restart guests after crash.
  daemon=subprocess.Popen([str(BIN),'daemon','--state-dir',str(state),'--backend',str(ROOT/'.local/bin/vm-apple')],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
  for _ in range(100):
   try:listed=call('list');break
   except RuntimeError:time.sleep(.05)
  assert len(listed)==3 and all(x['state']=='stopped' for x in listed),listed
  checks.append({'name':'registry recovered without automatic boot','count':len(listed)})
  result['passed']=True
 except Exception as exc:
  result['passed']=False;result['error']=str(exc);raise
 finally:
  if daemon.poll() is None:
   for id in ids:
    try:call('force_stop',{'id':id,'confirm':id})
    except Exception:pass
   daemon.terminate()
   try:daemon.wait(timeout=5)
   except subprocess.TimeoutExpired:daemon.kill();daemon.wait()
  result['finished_at_unix']=time.time();OUTPUT.write_text(json.dumps(result,ensure_ascii=False,indent=2)+'\n')
print('Real multi-VM acceptance passed: '+str(OUTPUT))
