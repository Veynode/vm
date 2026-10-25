#!/usr/bin/env python3
"""Actual persistent capture: stop encoding, freeze Guest, reuse owned snapshot/codec."""
import argparse,base64,hashlib,importlib.util,json,os,pathlib,queue,struct,subprocess,sys,threading,time
ROOT=pathlib.Path(__file__).resolve().parents[2];SELF=pathlib.Path(__file__).resolve()
os.umask(0o077)
def exact(stream,size):
 data=bytearray()
 while len(data)<size:
  part=stream.read(size-len(data))
  if not part:
   if data:raise RuntimeError('Truncated record')
   return None
  data.extend(part)
 return bytes(data)
class Client:
 def __init__(self,socket,prefix):
  self.errors=open(prefix+'.stderr','x');self.child=subprocess.Popen([str(ROOT/'.local/bin/spice-console'),socket,'--persistent'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=self.errors);self.q=queue.Queue();self.events=[];self.frames={};self.ident=0
  def reader():
   try:
    while True:
     h=exact(self.child.stdout,13)
     if h is None:break
     length,kind,epoch=struct.unpack('>IBQ',h)
     if not 0<length<=4194304 or kind not in (1,2):raise RuntimeError('Invalid frame envelope')
     body=exact(self.child.stdout,length)
     if body is None:raise RuntimeError('Missing body')
     self.q.put((kind,epoch,body))
   except Exception as e:self.q.put(e)
   self.q.put(None)
  threading.Thread(target=reader,daemon=True).start()
 def next(self,timeout=10):
  item=self.q.get(timeout=timeout)
  if item is None:raise RuntimeError('Console exited unexpectedly')
  if isinstance(item,Exception):raise item
  kind,epoch,body=item
  if kind==1:
   data=json.loads(body);self.events.append({'epoch':epoch,**{k:v for k,v in data.items() if k!='result'}})
   if data.get('event') in ['capture-error','session-error']:raise RuntimeError(json.dumps(data))
   return kind,epoch,data
  if len(body)<10:raise RuntimeError('Short AU')
  pts,key=struct.unpack('>QB',body[:9]);payload=body[9:]
  if key not in (0,1) or not payload.startswith(b'\0\0\0\1'):raise RuntimeError('Invalid AU')
  records=self.frames.setdefault(epoch,[])
  if (not records and (pts!=0 or key!=1)) or (records and pts<=records[-1]['pts_us']):raise RuntimeError('Epoch PTS/initial keyframe violation')
  frame={'pts_us':pts,'keyframe':key,'payload':payload};records.append(frame);return kind,epoch,frame
 def request(self,method,**fields):
  self.ident+=1;ident='c'+str(self.ident);self.child.stdin.write(json.dumps({'id':ident,'method':method,**fields}).encode()+b'\n');self.child.stdin.flush()
  while True:
   kind,epoch,data=self.next()
   if kind==1 and data.get('id')==ident:return data
 def close(self):
  if self.child.poll() is None:self.child.kill();self.child.wait()
  self.errors.close()
def death_consumer(socket,prefix):
 result={'passed':False,'producer_fault_injected':False,'scope':'Existing persistent owned static frame invalidates on actual private QEMU SIGKILL'};client=None
 try:
  client=Client(socket,prefix)
  while True:
   kind,e,d=client.next()
   if kind==1 and d.get('event')=='ready':break
  # Wait through the fixture's ten-second source freeze while retaining capture.
  deadline=time.monotonic()+13
  while time.monotonic()<deadline:
   try:kind,e,d=client.next(timeout=min(.5,deadline-time.monotonic()))
   except queue.Empty:continue
   if kind==2:raise RuntimeError('Capture encoded without start request')
  snapshot=client.request('screenshot')['result']
  result['snapshot']={k:v for k,v in snapshot.items() if k!='data_base64'}
  client.request('start',epoch=1,codec='hevc')
  while len(client.frames.get(1,[]))<2:client.next()
  pids=set(subprocess.check_output(['/usr/sbin/lsof','-t',socket],text=True).split())
  if len(pids)!=1:raise RuntimeError('Private socket producer is not unique')
  pid=int(next(iter(pids)));command=subprocess.check_output(['/bin/ps','-p',str(pid),'-o','command='],text=True).strip()
  if not command.startswith(str(ROOT/'.local/spice-reconnect-build/runtime/probe-loader')+' ') or f'addr={socket},' not in command:raise RuntimeError('Private producer ownership mismatch')
  before=len(client.frames[1]);start=time.monotonic();os.kill(pid,9);result['producer_fault_injected']=True
  try:
   while True:
    kind,e,d=client.next(timeout=3)
    if kind==2:raise RuntimeError('Stale cached AU emitted after producer death')
  except RuntimeError as error:
   if 'capture-error' not in str(error):raise
  result['exit_code']=client.child.wait(timeout=3);result['exit_after_kill_ms']=round((time.monotonic()-start)*1000,2)
  if result['exit_code']!=1 or len(client.frames[1])!=before:raise RuntimeError('Producer failure was not fail-closed')
  result['no_post_death_au']=True;result['events']=client.events;result['passed']=True
 except Exception as error:result['error']=str(error)
 finally:
  if client:client.close()
 pathlib.Path(prefix+'.console.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2));return 0 if result['passed'] else 1

def consumer(socket,prefix):
 result={'passed':False,'scope':'Actual persistent SPICE consumer, live Guest then SIGSTOP graphics, no browser or performance acceptance'};client=None
 try:
  client=Client(socket,prefix)
  while True:
   kind,e,d=client.next()
   if kind==1 and d.get('event')=='ready':break
  result['ready']=d
  result['initial_start']=client.request('start',epoch=1,codec='hevc')
  while len(client.frames.get(1,[]))<5:client.next()
  result['initial_stop']=client.request('stop',epoch=1)
  if 'error' in result['initial_stop']:raise RuntimeError('Stop rejected')
  # Fixture freezes source processes ten seconds after desktop-ready. Capture
  # stays attached and encoding stays off while the final source frame settles.
  deadline=time.monotonic()+12
  while time.monotonic()<deadline:
   try:kind,e,d=client.next(timeout=min(.5,deadline-time.monotonic()))
   except queue.Empty:continue
   if kind==2:raise RuntimeError('Encoding continued after stop ACK')
  a=client.request('screenshot')['result'];time.sleep(1.2);b=client.request('screenshot')['result']
  keys=['source_generation','source_sequence','captured_at_us','captured_monotonic_us','data_base64']
  if any(a[k]!=b[k] for k in keys):raise RuntimeError('Static snapshot metadata/pixels changed without source draw')
  png=base64.b64decode(a['data_base64'],validate=True)
  if len(png)!=a['byte_length'] or not png.startswith(b'\x89PNG\r\n\x1a\n'):raise RuntimeError('Bad PNG')
  pathlib.Path(prefix+'.png').write_bytes(png)
  result['static_snapshot']={k:v for k,v in a.items() if k!='data_base64'};result['static_snapshot']['sha256']=hashlib.sha256(png).hexdigest();result['static_snapshot_repeated_identically']=True
  result['sessions']=[]
  for epoch,codec in [(2,'hevc'),(3,'h264')]:
   start=client.request('start',epoch=epoch,codec=codec)
   if 'error' in start:raise RuntimeError('Cached start rejected')
   busy=client.request('start',epoch=epoch+10,codec=codec)
   if busy.get('error',{}).get('code')!='encoding_busy':raise RuntimeError('Concurrent encoder not rejected')
   while len(client.frames.get(epoch,[]))<2:client.next()
   client.request('keyframe',epoch=epoch)
   while len(client.frames.get(epoch,[]))<6:client.next()
   stop=client.request('stop',epoch=epoch)
   if 'error' in stop:raise RuntimeError('Stop failed')
   stale=client.request('keyframe',epoch=epoch)
   if stale.get('error',{}).get('code')!='stale_or_inactive_epoch':raise RuntimeError('Stale session accepted')
   snapshot=client.request('screenshot')['result']
   if any(snapshot[k]!=a[k] for k in keys):raise RuntimeError('Codec switch replaced source cache or capture time')
   frames=client.frames[epoch];raw=pathlib.Path(prefix+'.'+codec);raw.write_bytes(b''.join(f['payload'] for f in frames))
   metadata=json.loads(subprocess.check_output(['ffprobe','-v','error','-count_frames','-select_streams','v:0','-show_entries','stream=codec_name,profile,level,width,height,nb_read_frames','-of','json',str(raw)],timeout=15))['streams'][0]
   if int(metadata['nb_read_frames'])!=len(frames):raise RuntimeError('Independent decoder frame mismatch')
   if not any(f['keyframe'] for f in frames[2:4]):raise RuntimeError('Static cached keyframe request ignored')
   result['sessions'].append({'epoch':epoch,'codec':codec,'start':start,'stop':stop,'metadata':metadata,'frames':len(frames),'keyframe_indices':[i for i,f in enumerate(frames) if f['keyframe']]})
  rejected=client.request('start',epoch=2,codec='h264')
  if rejected.get('error',{}).get('code')!='stale_epoch':raise RuntimeError('Reused epoch accepted')
  client.request('shutdown');result['exit_code']=client.child.wait(timeout=3)
  if result['exit_code']!=0:raise RuntimeError('Persistent capture did not stop normally')
  result['events']=client.events;result['passed']=True
 except Exception as error:result['error']=str(error)
 finally:
  if client:client.close()
 pathlib.Path(prefix+'.console.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2));return 0 if result['passed'] else 1
def main():
 if len(sys.argv)==3 and not sys.argv[1].startswith('--'):raise SystemExit((death_consumer if os.environ.get('VEYNODE_CONSOLE_DEATH_TEST')=='1' else consumer)(*sys.argv[1:]))
 parser=argparse.ArgumentParser();parser.add_argument('--output',required=True,type=pathlib.Path);parser.add_argument('--death-test',action='store_true');parser.add_argument('--extra-consumer',type=pathlib.Path);args=parser.parse_args()
 if args.death_test:os.environ['VEYNODE_CONSOLE_DEATH_TEST']='1'
 spec=importlib.util.spec_from_file_location('desktop',ROOT/'scripts/qemu-linux-desktop-probe.py');module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
 module.BASE=ROOT/'.local/spice-reconnect-build/runtime';os.environ['VEYNODE_SPICE_LIBRARY']=str(module.BASE/'libspice-server-fixed.dylib')
 module_file=ROOT/'.local/runtime/utm/glamor-causal-artifacts/modules-fixed/libglamoregl.so';blob=module_file.read_bytes();digest=hashlib.sha256(blob).hexdigest();assert digest=='ba64c24cb70c953d26ac032633b6bca98577ea415f7abdc32f7f451d9a115f67'
 inject="mkdir -p /veynode/modules-fixed\nprintf '%s' '"+base64.b64encode(blob).decode()+"' | base64 -d > /veynode/modules-fixed/libglamoregl.so\necho '"+digest+"  /veynode/modules-fixed/libglamoregl.so' | sha256sum -c -\n"
 module.INIT=module.INIT.replace('udevd --daemon',inject+'udevd --daemon').replace('Xorg :0 vt1','Xorg :0 vt1 -modulepath /veynode/modules-fixed,/usr/lib/xorg/modules')
 module.INIT=module.INIT.replace('echo VEYNODE_DESKTOP_READY','( sleep 10; pids="$(pidof Xorg glxgears xterm)"; kill -STOP $pids; echo VEYNODE_STATIC_DESKTOP_READY; for pid in $pids; do cat /proc/$pid/stat; done ) &\necho VEYNODE_DESKTOP_READY')
 sys.argv=[str(SELF),'--output',str(args.output),'--capture-cycles','0','--extra-consumer',str(args.extra_consumer.resolve() if args.extra_consumer else SELF),'--extra-consumer-timeout','210' if args.extra_consumer else '90'];status=module.main()
 if args.death_test:
  inner=json.loads(pathlib.Path(str(args.output.with_suffix(''))+'-extra.console.json').read_text());outer=json.loads(args.output.read_text())
  if inner.get('passed') and inner.get('producer_fault_injected') and status==1 and outer.get('extra_consumer_exit_code')==0 and outer.get('capture_path_passed') is False:status=0
 raise SystemExit(status)

if __name__=='__main__':main()
