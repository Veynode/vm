#!/usr/bin/env python3
"""Extra consumer: actual stopped Guest rendering, owned-frame PLI and producer death."""
import json,os,pathlib,signal,struct,subprocess,sys,threading,time
ROOT=pathlib.Path(__file__).resolve().parents[2];BINARY=ROOT/'.local/bin/spice-video-stream'
os.umask(0o077)
def exact(stream,size):
 data=bytearray()
 while len(data)<size:
  part=stream.read(size-len(data))
  if not part:
   if data:raise RuntimeError('Truncated AU record')
   return None
  data.extend(part)
 return bytes(data)
def read_events(path):return [json.loads(line) for line in path.read_text().splitlines() if line.startswith('{')]
def run(socket,prefix,mode):
 stderr=pathlib.Path(prefix+'.stderr');raw=pathlib.Path(prefix+'.h264');records=[];requested=None;request_wall=None;stopped=False;killed=False;kill_wall=None
 with stderr.open('x') as error,raw.open('xb') as output:
  child=subprocess.Popen([str(BINARY),socket,'--codec','h264'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=error)
  timer=threading.Timer(25,child.kill);timer.start()
  try:
   while True:
    header=exact(child.stdout,13)
    if header is None:break
    size,pts,key=struct.unpack('>IQB',header)
    if not 0<size<=4194304 or key not in (0,1):raise RuntimeError('Invalid frame header')
    payload=exact(child.stdout,size)
    if payload is None:raise RuntimeError('Missing frame payload')
    output.write(payload);records.append({'pts_us':pts,'keyframe':key,'after_producer_kill':killed})
    if len(records)>300:raise RuntimeError('Idle test exceeds frame budget')
    events=read_events(stderr);idle=next((e for e in events if e.get('event')=='idle-refresh'),None)
    if idle and len(records)>idle['encoded'] and not requested:
     requested=len(records);request_wall=time.monotonic();child.stdin.write(b'keyframe\n');child.stdin.flush()
    if requested and len(records)==requested+1:
     if not key or time.monotonic()-request_wall>0.5:raise RuntimeError('Idle keyframe request did not promptly emit keyframe')
    if requested and pts-records[requested-1]['pts_us']>=2200000 and not stopped and not killed:
     if mode=='normal':child.stdin.write(b'stop\n');child.stdin.flush();stopped=True
     else:
      pids=set(subprocess.check_output(['/usr/sbin/lsof','-t',socket],text=True).split())
      if len(pids)!=1:raise RuntimeError('Private socket producer not unique')
      pid=int(next(iter(pids)));command=subprocess.check_output(['/bin/ps','-p',str(pid),'-o','command='],text=True).strip()
      if not command.startswith(str(ROOT/'.local/spice-reconnect-build/runtime/probe-loader')+' ') or f'addr={socket},' not in command:raise RuntimeError('Producer ownership check failed')
      os.kill(pid,signal.SIGKILL);killed=True;kill_wall=time.monotonic()
   code=child.wait(timeout=3)
   exit_after_kill_ms=round((time.monotonic()-kill_wall)*1000,2) if killed else None
  finally:
   timer.cancel()
   if child.poll() is None:child.kill();child.wait()
 events=read_events(stderr);idle=next((e for e in events if e.get('event')=='idle-refresh'),None);cached=next((e for e in events if e.get('event')=='cached-keyframe'),None)
 if not events:raise RuntimeError('No frame/events: fresh static consumer requires a new producer gl-draw')
 end=events[-1]
 if not requested or not idle or not cached or not end['cached_frame_released']:raise RuntimeError('Missing actual idle/cache lifecycle evidence')
 if end['draw_events']!=idle['draw_events'] or end['normalization_gpu_copies']!=idle['source_copies']:raise RuntimeError('Guest still produced new draws/copies during claimed static interval')
 if end['idle_refresh_frames']<3 or end['cached_keyframe_frames']<1:raise RuntimeError('Missing bounded idle heartbeat/keyframe')
 if mode=='normal' and (code!=0 or end['event']!='stopped'):raise RuntimeError('Normal idle stream did not stop cleanly')
 if mode=='death' and (not killed or code!=1 or end['event']!='error' or any(r['after_producer_kill'] for r in records)):raise RuntimeError('Producer failure continued stale cached frames')
 metadata=json.loads(subprocess.check_output(['ffprobe','-v','error','-count_frames','-select_streams','v:0','-show_entries','stream=codec_name,profile,width,height,nb_read_frames','-of','json',str(raw)],timeout=20))['streams'][0]
 if int(metadata['nb_read_frames'])!=len(records):raise RuntimeError('Idle bitstream decoding failed')
 return {'mode':mode,'success':True,'records':records,'native_events':events,'decoded_metadata':metadata,'exit_code':code,'keyframe_requested_after_record':requested,'producer_killed':killed,'exit_after_producer_kill_ms':exit_after_kill_ms}
if len(sys.argv)!=3:raise SystemExit('Usage: test-spice-idle-stream.py SOCKET PREFIX')
socket,prefix=sys.argv[1:];report={'success':False,'producer_fault_injected':False,'scope':'Actual Guest Xorg/glxgears/xterm SIGSTOP after5seconds, not VM pause. Native draw counters must remain fixed during idle.'}
try:
 report['cycles']=[]
 report['cycles'].append(run(socket,prefix+'-initial','death'))
 report['producer_fault_injected']=report['cycles'][-1]['producer_killed'];report['success']=True
except Exception as error:report['error']=str(error)
pathlib.Path(prefix+'.idle.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report,indent=2));raise SystemExit(0 if report['success'] else 1)
