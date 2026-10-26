#!/usr/bin/env python3
"""Real installed Guest/MCP/reboot/persistence/normal-shutdown acceptance.
Requires a disposable fixture from provision-linux-qemu.py, not a user VM.
"""
import argparse,json,pathlib,queue,secrets,shlex,subprocess,tempfile,threading,time
ROOT=pathlib.Path(__file__).resolve().parents[1]
a=argparse.ArgumentParser();a.add_argument('--config',type=pathlib.Path,required=True);a.add_argument('--marker-path',default='/home/veynode/veynode-persistence-test');args=a.parse_args()
config=json.loads(args.config.read_text());marker=secrets.token_hex(16)
if not args.config.resolve().is_relative_to(ROOT/'.local/vms'):raise SystemExit('Only repository-created disposable fixtures are accepted')
evidence={'config':config,'checks':[],'started_at_unix':time.time()}
def record(name,data):evidence['checks'].append({'name':name,'evidence':data})
class MCP:
 def __init__(self,state,execute=False):
  self.p=subprocess.Popen([str(ROOT/'target/debug/vm-control'),'mcp','--state-dir',str(state),'--allow-lifecycle',*(['--allow-execute']if execute else [])],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,bufsize=1);self.id=0;self.messages=queue.Queue()
  def read():
   for line in self.p.stdout:self.messages.put(json.loads(line))
  threading.Thread(target=read,daemon=True).start()
  self.request('initialize',{'protocolVersion':'2025-06-18','capabilities':{},'clientInfo':{'name':'persistent-real-validation','version':'1'}})
  self.p.stdin.write(json.dumps({'jsonrpc':'2.0','method':'notifications/initialized'})+'\n');self.p.stdin.flush()
 def request(self,method,params):
  self.id+=1;self.p.stdin.write(json.dumps({'jsonrpc':'2.0','id':self.id,'method':method,'params':params})+'\n');self.p.stdin.flush();r=self.messages.get(timeout=45)
  assert r['id']==self.id and 'error' not in r,r
  return r['result']
 def tool(self,name,arguments):
  r=self.request('tools/call',{'name':name,'arguments':arguments});assert not r.get('isError'),r;return json.loads(r['content'][0]['text'])
 def close(self):
  if self.p.poll()is None:self.p.stdin.close();self.p.wait(timeout=5)
with tempfile.TemporaryDirectory(prefix='vm-persist-')as tmp:
 state=pathlib.Path(tmp)/'s';daemon=subprocess.Popen([str(ROOT/'target/debug/vm-control'),'daemon','--state-dir',str(state),'--backend',str(ROOT/'.local/bin/vm-apple')],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE);clients=[]
 def call(method,payload={}):return json.loads(subprocess.check_output([str(ROOT/'target/debug/vm-control'),'call','--state-dir',str(state),method,json.dumps(payload)],stderr=subprocess.PIPE,timeout=40))
 def wait_guest(client):
  for _ in range(100):
   try:return client.tool('vm_guest_info',{'id':config['id']})
   except AssertionError:time.sleep(.1)
  raise TimeoutError('installed guest not ready')
 def normal_stop(client):
  reply=client.tool('vm_shutdown',{'id':config['id'],'mode':'guest'});assert reply['result']['requested'],reply
  for _ in range(150):
   state=call('status',{'id':config['id']})
   if state['state']=='stopped':return {'reply':reply,'actual_state':state['state']}
   time.sleep(.1)
  raise TimeoutError('Guest normal shutdown did not complete; no success claim')
 try:
  for _ in range(100):
   if(state/'control.sock').exists():break
   time.sleep(.05)
  call('create',config)
  life=MCP(state);clients.append(life)
  assert 'vm_exec' not in [t['name']for t in life.request('tools/list',{})['tools']]
  executor=MCP(state,True);clients.append(executor)
  executor.tool('vm_start',{'id':config['id']});info=wait_guest(executor)
  assert info['result']['capabilities']['shutdown'] is True,info;record('installed service advertises independent normal shutdown',info)
  script=f"command -v fsck.ext4; grep '/dev/vda / ext4' /proc/mounts; printf %s {marker} > {shlex.quote(args.marker_path)}; sync"
  write=executor.tool('vm_exec',{'id':config['id'],'argv':['/bin/sh','-eu','-c',script],'timeout_ms':5000})
  assert write['result']['exit_code']==0,write;record('installed ext4 root, fsck and persistent write',write)
  record('lifecycle-only MCP normal shutdown',normal_stop(life))
  life.tool('vm_start',{'id':config['id']});wait_guest(executor)
  read=executor.tool('vm_exec',{'id':config['id'],'argv':['/bin/cat',args.marker_path],'timeout_ms':5000})
  assert read['result']['stdout']==marker and read['result']['exit_code']==0,read;record('data survives real clean shutdown and reboot',True)
  record('second clean shutdown',normal_stop(life));evidence['passed']=True
 except Exception as e:evidence.update(passed=False,error=str(e));raise
 finally:
  for client in clients:
   try:client.close()
   except Exception:pass
  try:
   state_result=call('status',{'id':config['id']})
   if state_result['state']!='stopped':evidence['forced_failure_cleanup']=call('force_stop',{'id':config['id'],'confirm':config['id']})
  except Exception:pass
  daemon.terminate();daemon.wait(timeout=5)
  evidence['finished_at_unix']=time.time();out=ROOT/'.local/evidence/persistent-linux.json';out.parent.mkdir(exist_ok=True,parents=True);out.write_text(json.dumps(evidence,indent=2)+'\n')
print('Persistent Linux MCP, clean shutdown, reboot and persistence passed')
