#!/usr/bin/env python3
"""Bounded browser adapter for an already-running private persistent console."""
import json,os,pathlib,select,signal,subprocess,sys,time,urllib.request
ROOT=pathlib.Path(__file__).resolve().parents[2]
os.umask(0o077)
if len(sys.argv)!=3:raise SystemExit('Usage: live-console-browser.py CONSOLE_SOCKET OUTPUT_PREFIX')
socket,prefix=sys.argv[1:];prefix=pathlib.Path(prefix)
ready=pathlib.Path(str(prefix)+'.ready.json');stop=pathlib.Path(str(prefix)+'.stop')
if ready.exists() or stop.exists():raise SystemExit('Refusing existing ready/stop marker')
command=[str(ROOT/'target/debug/vm-media-local'),'--console',socket,str(ROOT/'browser'),'8787']
if os.environ.get('VEYNODE_MEDIA_DROP_FIRST_KEYFRAME')=='1':command.append('--fault-drop-first-keyframe')
if os.environ.get('VEYNODE_MEDIA_SUPPRESS_HEVC_FRAMES')=='1':command.append('--fault-suppress-hevc-frames')
child=None;result={'success':False,'scope':'Loopback-only actual Guest/browser integration diagnostic,150s bounded persistent-console fixture'}
try:
 with pathlib.Path(str(prefix)+'.media.stderr').open('x') as error:
  child=subprocess.Popen(command,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=error,text=True,start_new_session=True)
  limit=time.monotonic()+10;initial=None
  while time.monotonic()<limit:
   if child.poll() is not None:raise RuntimeError('Media sender exited during startup')
   if select.select([child.stdout],[],[],0.2)[0]:
    line=child.stdout.readline(8193)
    if len(line)>8192:raise RuntimeError('Startup output exceeded limit')
    initial=json.loads(line);break
  if not initial or initial.get('url')!='http://127.0.0.1:8787':raise RuntimeError('Sender startup URL missing/unexpected')
  with urllib.request.urlopen(initial['url']+'/capabilities',timeout=3) as response:
   capabilities=json.loads(response.read(8193))
  state={**initial,'pid':child.pid,'capabilities':capabilities,'stop_file':str(stop),'max_lifetime_seconds':150}
  with ready.open('x') as output:json.dump(state,output,indent=2);output.write('\n')
  print(json.dumps(state),flush=True)
  deadline=time.monotonic()+150
  while time.monotonic()<deadline and not stop.exists():
   if child.poll() is not None:raise RuntimeError('Media sender exited before stop/deadline')
   time.sleep(.2)
  result.update(success=True,stop_reason='marker' if stop.exists() else '150-second-deadline')
except (Exception,KeyboardInterrupt) as error:result['error']=str(error)
finally:
 if child:
  if child.poll() is None:
   child.send_signal(signal.SIGINT)
   try:child.wait(timeout=4)
   except subprocess.TimeoutExpired:
    os.killpg(child.pid,signal.SIGTERM)
    try:child.wait(timeout=2)
    except subprocess.TimeoutExpired:os.killpg(child.pid,signal.SIGKILL);child.wait(timeout=2)
  result['sender_exit_code']=child.returncode
 pathlib.Path(str(prefix)+'.live.json').write_text(json.dumps(result,indent=2)+'\n')
 print(json.dumps(result),flush=True)
raise SystemExit(0 if result['success'] else 1)
