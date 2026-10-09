#!/usr/bin/env python3
"""Actual QEMU same-console input through MCP; owned disposable RAM fixtures only."""
import argparse,base64,concurrent.futures,hashlib,importlib.util,json,os,secrets,signal,subprocess,tempfile,time
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('qemu_test',ROOT/'scripts/test-control-qemu.py');q=importlib.util.module_from_spec(spec);spec.loader.exec_module(q)
def parse_events(text):
 # Serial output appends concurrently. Only newline-terminated records are complete;
 # a malformed complete record must still fail instead of hiding oracle corruption.
 lines=text.split('\n')[:-1]
 return tuple([json.loads(line.split(prefix,1)[1]) for line in lines if line.startswith(prefix)] for prefix in ['X_INPUT ','RAW_INPUT '])
def main():
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('--output',type=Path,required=True)
 p.add_argument('--cleanup-retry-fault',action='store_true',help='Temporarily SIGSTOP only the owned test worker to verify failed/concurrent cleanup receipts; always SIGCONT in finally')
 for name,default in [('control','target/debug/vm-control'),('worker','target/debug/vm-qemu'),('broker','target/debug/vm-console'),('helper','.local/bin/spice-console'),('apple-backend','.local/bin/vm-apple')]:
  p.add_argument('--'+name,type=Path,default=ROOT/default)
 a=p.parse_args();out=a.output.resolve();out.parent.mkdir(parents=True,exist_ok=True)
 if out.exists():raise SystemExit('Refusing evidence overwrite')
 report={'passed':False,'checks':[],'started_at':time.time(),'scope':'Actual Linux ARM QEMU/HVF console, physical input only; no Unicode/layout, other Guest OS, Internet or performance claim'}
 def record(name,value=True):report['checks'].append({'name':name,'evidence':value});print(name,flush=True)
 binary=a.control.resolve();worker=a.worker.resolve();broker=a.broker.resolve();helper=a.helper.resolve();apple=a.apple_backend.resolve()
 for path in [binary,worker,broker,helper,apple]:
  if not path.is_file() or not os.access(path,os.X_OK):raise SystemExit('Missing executable: '+str(path))
 runtime=['--qemu-worker',str(worker),'--qemu-launcher',str(ROOT/'.local/spice-reconnect-build/runtime/probe-loader'),'--qemu-spice-library',str(ROOT/'.local/spice-reconnect-build/runtime/libspice-server-fixed.dylib'),'--qemu-library',str(q.BASE/'UTM.app/Contents/Frameworks/qemu-aarch64-softmmu.framework/Versions/A/qemu-aarch64-softmmu'),'--qemu-resources',str(q.BASE/'UTM.app/Contents/Resources/qemu'),'--qemu-console-broker',str(broker),'--qemu-console-helper',str(helper)]
 report['runtime']={'control':str(binary),'worker':str(worker),'broker':str(broker),'helper':str(helper),'apple_backend':str(apple)}
 report['sha256']={str(x.relative_to(ROOT) if x.is_relative_to(ROOT) else x):hashlib.sha256(x.read_bytes()).hexdigest() for x in [Path(__file__),ROOT/'scripts/test-control-qemu.py',ROOT/'scripts/linux-control-input-oracle.c',binary,worker,broker,helper,apple]}
 with tempfile.TemporaryDirectory(prefix='vi-',dir='/tmp') as temporary:
  directory=Path(temporary);state=directory/'state';initrd,meta=q.fixture(directory,False,ROOT/'scripts/linux-control-input-oracle.c');report['fixture']=meta
  daemon=subprocess.Popen([str(binary),'daemon','--state-dir',str(state),'--backend',str(apple)]+runtime,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE);clients=[];ids=[]
  def call(method,args):
   r=subprocess.run([str(binary),'call','--state-dir',str(state),method,json.dumps(args)],capture_output=True,text=True,timeout=40)
   if r.returncode:raise RuntimeError(r.stderr)
   return json.loads(r.stdout)
  def ok(reply):assert reply.get('isError') is False,reply;return json.loads(reply['content'][0]['text'])
  def denied(reply):assert reply.get('isError') is True,reply;return reply['content'][0]['text']
  def release_confirmed(reply):
   # This confirms worker/QMP dispatch, not Guest delivery; the independent oracle below proves delivery.
   assert reply.get('input_release')=={'confirmed':True,'vm_count':1,'guest_delivery_confirmed':False},reply
   return reply
  def client(**kwargs):c=q.MCP(binary,state,**kwargs);clients.append(c);return c
  def shot(c):
   s=c.tool('vm_screenshot',{'id':'input-vm'});assert not s['isError'],s
   v=json.loads(s['content'][1]['text']);return {'id':'input-vm',**{k:v[k] for k in ['frame_id','source_generation','width','height']}}
  def events():
   return parse_events((state/'input.serial.log').read_text(errors='replace'))
  def wait_for(predicate,seconds=5):
   end=time.monotonic()+seconds
   while time.monotonic()<end:
    x,r=events()
    if predicate(x,r):return x,r
    time.sleep(.03)
   raise AssertionError(('Guest event not observed',x[-30:],r[-30:]))
  try:
   for _ in range(100):
    if (state/'control.sock').exists():break
    time.sleep(.05)
   c=client(input=True);observer=client();executor=client(execute=True)
   for x in [observer,executor]:
    assert not any(t['name'].startswith('vm_input_') for t in x.rpc('tools/list',{})['tools'])
    x.number+=1;x.p.stdin.write(json.dumps({'jsonrpc':'2.0','id':x.number,'method':'tools/call','params':{'name':'vm_input_click','arguments':{'id':'input-vm'}}})+'\n');x.p.stdin.flush()
    denied_scope=json.loads(x.p.stdout.readline());assert denied_scope['error']['code']==-32602,denied_scope
   assert any(t['name']=='vm_input_click' for t in c.rpc('tools/list',{})['tools']);record('independent_input_scope')
   config={'id':'input-vm','backend':'qemu-hvf','guest':'linux','cpus':2,'memory_mib':1536,'kernel':str(q.MEDIA/'Image'),'initrd':str(initrd),'command_line':'console=ttyAMA0 rdinit=/veynode/gpu-init','serial_log':str(state/'input.serial.log')}
   call('create',config);ids.append('input-vm');call('start',{'id':'input-vm'})
   end=time.monotonic()+200
   while time.monotonic()<end:
    log=(state/'input.serial.log').read_text(errors='replace') if (state/'input.serial.log').exists() else ''
    if 'CONTROL_INPUT_READY' in log and 'VEYNODE_DESKTOP_READY' in log:break
    if 'VEYNODE_DESKTOP_FAIL' in log:raise RuntimeError(log[-6000:])
    time.sleep(.2)
   else:raise TimeoutError(log[-6000:])
   assert 'Accelerated: yes' in log and 'INPUT_DEVICE' in log
   f=shot(c);record('real_xorg_evdev_console_ready',f)
   record('no_lease_rejected',denied(c.tool('vm_input_click',{**f,'x':200,'y':200})))
   ok(c.tool('vm_acquire_control',{'id':'input-vm'}))
   for key,value in [('frame_id','00000000-0000-0000-0000-000000000000'),('source_generation',999999),('width',f['width']+1),('x',f['width'])]:
    frame=shot(c);bad={**frame,'x':200,'y':200,key:value};record('reject_'+key,denied(c.tool('vm_input_click',bad)))
   stale=shot(c);time.sleep(5.2);record('expired_frame_rejected',denied(c.tool('vm_input_click',{**stale,'x':200,'y':200})))
   ok(c.tool('vm_input_click',{**shot(c),'x':200,'y':200}));x,r=wait_for(lambda x,r:any(e.get('type')==5 and e.get('button')==1 and e.get('x')==200 and e.get('y')==200 for e in x) and any(e=={'type':1,'code':272,'value':0} for e in r));record('authorized_click_exact_guest_xorg_evdev',{'xorg':x,'evdev':r})
   ok(c.tool('vm_input_key',{**shot(c),'keys':['a']}));x,r=wait_for(lambda x,r:any(e.get('type')==3 and e.get('keysym')==97 for e in x) and any(e=={'type':1,'code':30,'value':0} for e in r));record('physical_key_guest_layout_a',{'xorg':x[-4:],'evdev':r[-4:]})
   ok(c.tool('vm_input_drag',{**shot(c),'x':220,'y':220,'end_x':280,'end_y':260}));x,r=wait_for(lambda x,r:any(e.get('type')==6 and e.get('x')==280 and e.get('y')==260 and e.get('state',0)&256 for e in x));record('drag_actual_button_motion',x[-25:])
   ok(c.tool('vm_input_scroll',{**shot(c),'x':250,'y':250,'direction':'down','ticks':2}));x,r=wait_for(lambda x,r:sum(e.get('type')==5 and e.get('button')==5 for e in x)>=2);record('scroll_actual_guest_events',x[-8:])
   second={**config,'id':'other-vm','memory_mib':512,'serial_log':str(state/'other.serial.log')};call('create',second);ids.append('other-vm');call('start',{'id':'other-vm'});ok(c.tool('vm_acquire_control',{'id':'other-vm'}));time.sleep(1)
   record('cross_vm_frame_rejected',denied(c.tool('vm_input_click',{**shot(c),'id':'other-vm','x':200,'y':200})));call('force_stop',{'id':'other-vm','confirm':'other-vm'})
   before=len(events()[0]);f=shot(c)
   with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
    future=pool.submit(c.tool,'vm_input_drag',{**f,'x':220,'y':220,'end_x':350,'end_y':350})
    wait_for(lambda x,r:len(x)>before and any(e.get('type')==4 and e.get('button')==1 for e in x[before:]))
    takeover=release_confirmed(call('human_takeover',{'id':'input-vm'}));reply=future.result(timeout=15);denied(reply)
   x,r=wait_for(lambda x,r:any(e.get('type')==5 and e.get('button')==1 for e in x[before:]));assert not any(e.get('type')==6 and e.get('x')==350 and e.get('y')==350 for e in x[before:]);record('takeover_interrupts_drag_and_releases',{'reply':reply,'takeover':takeover,'events':x[before:]})
   time.sleep(.2);n=len(events()[0]);record('revoked_client_rejected',denied(c.tool('vm_input_click',{**shot(c),'x':200,'y':200})));time.sleep(.2);assert len(events()[0])==n;record('revoked_request_produces_no_guest_input')
   call('human_release',{'id':'input-vm'});ok(c.tool('vm_acquire_control',{'id':'input-vm'}))
   before=len(events()[0]);ok(c.tool('vm_input_key',{**shot(c),'keys':['a']}))
   x,r=wait_for(lambda x,r:any(e.get('type')==3 and e.get('keysym')==97 for e in x[before:]));record('new_epoch_actual_key_before_explicit_release',x[before:])
   record('explicit_release_worker_dispatch_confirmed',release_confirmed(ok(c.tool('vm_release_control',{'id':'input-vm'}))))
   ok(c.tool('vm_acquire_control',{'id':'input-vm'}));before=len(events()[0]);f=shot(c)
   # Close stdio during an active drag. The adapter disconnect monitor revokes the lease.
   c.number+=1;c.p.stdin.write(json.dumps({'jsonrpc':'2.0','id':c.number,'method':'tools/call','params':{'name':'vm_input_drag','arguments':{**f,'x':220,'y':220,'end_x':360,'end_y':360}}})+'\n');c.p.stdin.flush()
   wait_for(lambda x,r:any(e.get('type')==4 and e.get('button')==1 for e in x[before:]));c.p.stdin.close();c.p.wait(timeout=12);assert c.p.returncode==0,{'exit':c.p.returncode,'stderr':c.p.stderr.read()}
   x,r=wait_for(lambda x,r:any(e.get('type')==5 and e.get('button')==1 for e in x[before:]));assert not any(e.get('type')==6 and e.get('x')==360 and e.get('y')==360 for e in x[before:]);record('stdio_disconnect_releases_drag',x[before:])
   if a.cleanup_retry_fault:
    owner=secrets.token_hex(32);epoch=call('acquire_control',{'id':'input-vm','owner':owner})['epoch'];f=shot(observer);before=len(events()[0])
    # Direct local control is deliberate: a known disposable owner lets us test
    # the daemon lifecycle independently of an adapter's own cleanup latch.
    down={**f,'owner':owner,'epoch':epoch,'operation_id':secrets.token_hex(16),'begin':True,'events':[
     {'type':'abs','data':{'axis':'x','value':((2*200+1)*32768)//(2*f['width'])}},
     {'type':'abs','data':{'axis':'y','value':((2*200+1)*32768)//(2*f['height'])}},
     {'type':'btn','data':{'button':'left','down':True}}]}
    call('input_step',down)
    status=call('status',{'id':'input-vm'});worker_pid=status['pid'];assert status['config']['id']=='input-vm' and type(worker_pid) is int and worker_pid>1
    # This VM belongs to the temporary state above. Never signal an external VM.
    stopped=False;attempts=[]
    def end_owner():
     result=subprocess.run([str(binary),'call','--state-dir',str(state),'session_end',json.dumps({'owner':owner})],capture_output=True,text=True,timeout=20)
     return {'exit_code':result.returncode,'response':json.loads(result.stdout) if result.returncode==0 else None,'error':result.stderr.strip() if result.returncode else None}
    # The worker also drains the Guest UART, so this must happen before STOP.
    wait_for(lambda x,r:any(e.get('type')==4 and e.get('button')==1 for e in x[before:]),seconds=2)
    try:
     os.kill(worker_pid,signal.SIGSTOP);stopped=True
     with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
      first=pool.submit(end_owner);time.sleep(.2);second=pool.submit(end_owner)
      attempts=[first.result(timeout=20),second.result(timeout=20)]
    finally:
     if stopped:
      try:os.kill(worker_pid,signal.SIGCONT)
      except ProcessLookupError:record('owned_worker_disappeared_before_resume',call('status',{'id':'input-vm'}))
    # After resuming, require independent Guest release even in the failing old
    # implementation. Do not leave the injected failure holding a test button.
    x,r=wait_for(lambda x,r:any(e.get('type')==5 and e.get('button')==1 for e in x[before:]),seconds=10)
    retry=call('session_end',{'owner':owner})
    record('concurrent_failed_cleanup_raw_results',{'attempts':attempts,'retry':retry,'guest_events':x[before:],'fault':'SIGSTOP owned worker only; resumed before cleanup verification'})
    assert all(v['exit_code']!=0 and 'unconfirmed' in v['error'] for v in attempts),attempts
    assert retry['closed'] is True and retry['retired_owner'] is True,retry
    record('failed_cleanup_retry_does_not_claim_success')
    recovery=client(input=True);ok(recovery.tool('vm_acquire_control',{'id':'input-vm'}));before=len(events()[0])
    ok(recovery.tool('vm_input_click',{**shot(recovery),'x':230,'y':230}));x,r=wait_for(lambda x,r:any(e.get('type')==5 and e.get('x')==230 and e.get('y')==230 for e in x[before:]));record('new_owner_input_after_cleanup_recovery',x[before:])
   call('shutdown',{'id':'input-vm'});end=time.monotonic()+20
   while time.monotonic()<end:
    if call('status',{'id':'input-vm'})['state']=='stopped':break
    time.sleep(.1)
   else:raise TimeoutError('normal shutdown incomplete')
   record('normal_shutdown');report['passed']=True
  except Exception as error:report['error']=repr(error);raise
  finally:
   for c in clients:
    try:c.close()
    except Exception:pass
   for vm in ids:
    try:call('force_stop',{'id':vm,'confirm':vm})
    except Exception:pass
   for name in ['input.serial.log','other.serial.log']:
    path=state/name
    if path.exists():out.with_suffix('.'+name).write_bytes(path.read_bytes())
   for path in state.glob('*.stderr.log'):out.with_suffix('.'+path.name).write_bytes(path.read_bytes())
   daemon.terminate()
   try:daemon.wait(timeout=10)
   except subprocess.TimeoutExpired:daemon.kill();daemon.wait()
   report['ended_at']=time.time();out.write_text(json.dumps(report,indent=2)+'\n')
if __name__=='__main__':main()
