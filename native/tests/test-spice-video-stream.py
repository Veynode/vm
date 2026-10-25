#!/usr/bin/env python3
"""Real disposable Guest AU framing/forced keyframe/decode and pipe lifecycle test."""
import argparse,base64,hashlib,importlib.util,json,os,pathlib,struct,subprocess,sys,threading,time
ROOT=pathlib.Path(__file__).resolve().parents[2];SELF=pathlib.Path(__file__).resolve();BINARY=ROOT/'.local/bin/spice-video-stream'
os.umask(0o077)

def exact(stream,count):
 data=bytearray()
 while len(data)<count:
  chunk=stream.read(count-len(data))
  if not chunk:
   if data:raise RuntimeError('Partial stream record')
   return None
  data.extend(chunk)
 return bytes(data)

def capture(socket,prefix,codec):
 events=[];raw_path=pathlib.Path(prefix+'.'+codec);stderr=pathlib.Path(prefix+'.stderr')
 with stderr.open('x') as error,raw_path.open('xb') as raw:
  child=subprocess.Popen([str(BINARY),socket,'--codec',codec],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=error)
  timer=threading.Timer(20,child.kill);timer.start();requested=False;stopped=False
  try:
   while True:
    header=exact(child.stdout,13)
    if header is None:break
    size,pts,key=struct.unpack('>IQB',header)
    if not 0<size<=4*1024*1024 or key not in (0,1):raise RuntimeError('Invalid AU framing')
    payload=exact(child.stdout,size)
    if payload is None or not payload.startswith(b'\0\0\0\1'):raise RuntimeError('Missing Annex B AU')
    if (not events and pts!=0) or (events and pts<=events[-1]['pts_us']):raise RuntimeError('PTS not zero-based strictly increasing')
    types=[(n[0]&31) if codec=='h264' else ((n[0]>>1)&63) for n in payload.split(b'\0\0\0\1')[1:] if n]
    if key and not ({7,8}.issubset(types) if codec=='h264' else {32,33,34}.issubset(types)):raise RuntimeError('Keyframe missing parameter sets')
    if key and not (5 in types if codec=='h264' else any(16<=t<=23 for t in types)):raise RuntimeError('Keyframe flag lacks intra random-access NAL')
    if codec=='h264' and key:
     sps=next(n for n in payload.split(b'\0\0\0\1')[1:] if n and n[0]&31==7)
     if len(sps)<4 or sps[1]!=0x42 or sps[2]&0x4f!=0x40 or sps[3]!=31:raise RuntimeError('H264 SPS mismatches RFC6184 constrained Baseline3.1')
    raw.write(payload);events.append({'size':size,'pts_us':pts,'keyframe':key,'nal_types':types,'h264_profile_level_id':sps[1:4].hex() if codec=='h264' and key else None})
    if len(events)==10:child.stdin.write(b'keyframe\n');child.stdin.flush();requested=True
    if len(events)==60:child.stdin.write(b'stop\n');child.stdin.flush();stopped=True
    if len(events)>65:raise RuntimeError('Stop did not bound further frames')
   code=child.wait(timeout=3)
  finally:
   timer.cancel()
   if child.poll() is None:child.kill();child.wait()
 if code!=0 or not requested or not stopped or not 60<=len(events)<=65:raise RuntimeError('Real stream did not stop cleanly')
 if not any(e['keyframe'] for e in events[10:13]):raise RuntimeError('Explicit keyframe request was not honored promptly')
 logs=[json.loads(line) for line in stderr.read_text().splitlines() if line.startswith('{')]
 if not any(e.get('event')=='ready' and e.get('hardware_verified') for e in logs):raise RuntimeError('No actual hardware-ready event')
 meta=json.loads(subprocess.check_output(['ffprobe','-v','error','-count_frames','-select_streams','v:0','-show_entries','stream=codec_name,profile,level,width,height,nb_read_frames','-of','json',str(raw_path)],timeout=20))['streams'][0]
 if meta['codec_name']!=codec or int(meta['nb_read_frames'])!=len(events):raise RuntimeError('Independent decoded frame count/codec mismatch')
 checksums=subprocess.check_output(['ffmpeg','-v','error','-i',str(raw_path),'-f','framemd5','-'],text=True,timeout=20)
 hashes=[line.rsplit(',',1)[1].strip() for line in checksums.splitlines() if line and not line.startswith('#')]
 pixels=subprocess.check_output(['ffmpeg','-v','error','-i',str(raw_path),'-frames:v','1','-f','rawvideo','-pix_fmt','rgb24','-'],timeout=15)
 nonblack=sum(any(pixels[p+c]>8 for c in range(3)) for p in range(0,len(pixels)-2,48))
 if len(hashes)!=len(events) or len(set(hashes))<2 or nonblack<100:raise RuntimeError('Actual stream decoded no changing/nonblack pixels')
 ready=next(e for e in logs if e.get('event')=='ready')
 expected=(1152,720) if codec=='h264' else (1280,800)
 if (meta['width'],meta['height'])!=expected or (ready['source_width'],ready['source_height'])!=(1280,800):raise RuntimeError('Source/encoded sizing mismatch')
 if codec=='h264' and (meta['level']!=31 or meta['profile']!='Constrained Baseline'):raise RuntimeError('H264 decoded profile must beConstrainedBaseline3.1')
 if codec=='hevc':
  ptl=next((e.get('actual_hevc_profile_tier_level') for e in logs if e.get('event')=='codec-parameters'),None)
  if not ptl or ptl!={'profile_space':0,'profile_id':1,'tier_flag':0,'level_id':meta['level']} or meta['profile']!='Main' or meta['level']>120:raise RuntimeError('Actual HEVC SPS gate/independent decode profile mismatch')
 guest_png=pathlib.Path(prefix.rsplit('-extra-',1)[0]+'-guest.png')
 guest=subprocess.check_output(['ffmpeg','-v','error','-i',str(guest_png),'-f','rawvideo','-pix_fmt','rgb24','-'],timeout=15)
 def centroid(rgb,w,h):
  xs=ys=count=0
  for y in range(0,h,2):
   for x in range(0,w,2):
    offset=(y*w+x)*3;r,g,b=rgb[offset:offset+3]
    if b>80 and b>1.3*r and b>1.3*g:xs+=x;ys+=y;count+=1
  if count<100:raise RuntimeError('Guest blue gear marker absent')
  return {'x_normalized':xs/count/w,'y_normalized':ys/count/h,'samples':count}
 source_blue=centroid(guest,1280,800);decoded_blue=centroid(pixels,*expected)
 if max(abs(source_blue[key]-decoded_blue[key]) for key in ['x_normalized','y_normalized'])>.05:raise RuntimeError('Scaled bitstream orientation/crop differs from actual Guest')
 orientation={'source_blue_centroid':source_blue,'decoded_blue_centroid':decoded_blue,'normalized_tolerance':.05,'aspect_error':abs(expected[0]/expected[1]-1280/800)}
 return {'codec':codec,'success':True,'frames':len(events),'metadata':meta,'orientation':orientation,'unique_decoded_frame_hashes':len(set(hashes)),'first_frame_nonblack_samples':nonblack,'keyframe_indices':[i for i,e in enumerate(events) if e['keyframe']],'keyframe_requested_after_frames':10,'records':events,'native_events':logs,'bitstream_sha256':hashlib.sha256(raw_path.read_bytes()).hexdigest()}

def lifecycle(socket,prefix,mode):
 path=pathlib.Path(prefix+'.'+mode+'.stderr');start=time.monotonic()
 with path.open('x') as error:
  child=subprocess.Popen([str(BINARY),socket,'--codec','h264'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=error)
  try:
   if mode=='eof':
    child.stdin.close()
   elif mode=='broken-pipe':child.stdout.close()
   elif mode!='backpressure':raise ValueError(mode)
   code=child.wait(timeout=8)
  finally:
   if child.poll() is None:child.kill();child.wait()
 elapsed=round((time.monotonic()-start)*1000,2);events=[json.loads(line) for line in path.read_text().splitlines() if line.startswith('{')]
 expected=code==0 if mode=='eof' else code==1 and any(e.get('event')=='error' and 'stdout' in e.get('error','') for e in events)
 if not expected:raise RuntimeError('Unexpected '+mode+' behavior: '+json.dumps(events))
 return {'mode':mode,'success':True,'exit_code':code,'elapsed_ms':elapsed,'native_events':events}

def consumer(socket,prefix):
 result={'scope':'Actual disposable Linux desktop stream, no WebRTC/browser performance claim','success':False}
 try:
  result['codecs']=[capture(socket,prefix+'-'+codec,codec) for codec in ['h264','hevc']]
  result['lifecycle']=[lifecycle(socket,prefix,mode) for mode in ['eof','broken-pipe','backpressure']]
  result['success']=True
 except Exception as error:result['error']=str(error)
 pathlib.Path(prefix+'.stream.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2));return 0 if result['success'] else 1

if len(sys.argv)==3 and not sys.argv[1].startswith('-'):raise SystemExit(consumer(*sys.argv[1:]))
parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--output',type=pathlib.Path,required=True);parser.add_argument('--live-browser',action='store_true');parser.add_argument('--static-desktop',action='store_true');parser.add_argument('--idle-test',action='store_true');args=parser.parse_args()
if args.output.exists():raise SystemExit('Refusing to overwrite evidence')
subprocess.run([str(ROOT/'native/tools/build-spice-video-stream.sh')],check=True)
# Real fixed server permits successive fresh clients; no cached surface IDs.
os.environ['VEYNODE_SPICE_LIBRARY']=str(ROOT/'.local/spice-reconnect-build/runtime/libspice-server-fixed.dylib')
spec=importlib.util.spec_from_file_location('desktop',ROOT/'scripts/qemu-linux-desktop-probe.py');module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module);module.BASE=ROOT/'.local/spice-reconnect-build/runtime'
path=ROOT/'.local/runtime/utm/glamor-causal-artifacts/modules-fixed/libglamoregl.so';blob=path.read_bytes();digest=hashlib.sha256(blob).hexdigest()
if digest!='ba64c24cb70c953d26ac032633b6bca98577ea415f7abdc32f7f451d9a115f67':raise SystemExit('Wrong validated Guest module')
assert module.INIT.count('udevd --daemon')==1 and module.INIT.count('Xorg :0 vt1')==1
inject="mkdir -p /veynode/modules-fixed\nprintf '%s' '"+base64.b64encode(blob).decode()+"' | base64 -d > /veynode/modules-fixed/libglamoregl.so\nprintf '%s\\n' '"+digest+"  /veynode/modules-fixed/libglamoregl.so' | sha256sum -c -\n"
module.INIT=module.INIT.replace('udevd --daemon',inject+'udevd --daemon').replace('Xorg :0 vt1','Xorg :0 vt1 -modulepath /veynode/modules-fixed,/usr/lib/xorg/modules')
if args.static_desktop or args.idle_test:
 assert module.INIT.count('echo VEYNODE_DESKTOP_READY')==1
 module.INIT=module.INIT.replace('echo VEYNODE_DESKTOP_READY','''(
 sleep FREEZE_DELAY_SECONDS
 pids="$(pidof Xorg glxgears xterm)"
 kill -STOP $pids
 echo VEYNODE_STATIC_DESKTOP_READY
 for pid in $pids; do cat /proc/$pid/stat; done
) &
echo VEYNODE_DESKTOP_READY'''.replace('FREEZE_DELAY_SECONDS',str(45 if args.live_browser else 5)))
consumer_path=ROOT/'native/tests/live-spice-browser.py' if args.live_browser else (ROOT/'native/tests/test-spice-idle-stream.py' if args.idle_test else SELF)
sys.argv=[str(SELF),'--output',str(args.output),'--capture-cycles','0','--extra-consumer',str(consumer_path),'--extra-consumer-timeout','210' if args.live_browser else '90']
status=module.main()
if args.idle_test:
 detail=pathlib.Path(str(args.output.with_suffix(''))+'-extra.idle.json')
 if detail.exists():
  evidence=json.loads(detail.read_text());outer=json.loads(args.output.read_text())
  # Intentional producer death must remain a failure of outer still-running check.
  if evidence.get('success') and evidence.get('producer_fault_injected') and status==1 and outer.get('extra_consumer_exit_code')==0 and outer.get('capture_path_passed') is False:status=0
raise SystemExit(status)
