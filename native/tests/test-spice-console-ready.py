#!/usr/bin/env python3
"""Prove persistent capture readiness before QMPcont; no Guest execution claim."""
import importlib.util,json,os,pathlib,subprocess,sys,tempfile,time,threading
ROOT=pathlib.Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('console_test',ROOT/'native/tests/test-spice-console.py')
console_test=importlib.util.module_from_spec(spec);spec.loader.exec_module(console_test)
spec=importlib.util.spec_from_file_location('desktop',ROOT/'scripts/windows-probe-linux-desktop.py');module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
os.umask(0o077);prefix=pathlib.Path(sys.argv[1]).resolve();prefix.parent.mkdir(parents=True,exist_ok=True);report={'passed':False,'guest_executed':False};client=None;proc=None
with tempfile.TemporaryDirectory(prefix='veynode-console-ready-') as folder:
 folder=pathlib.Path(folder);app=ROOT/'.local/windows-gpu/UTM.app/Contents';runtime=ROOT/'.local/spice-reconnect-build/runtime';sock=folder/'spice.sock';qmp=folder/'qmp.sock';env=os.environ.copy();env['VEYNODE_SPICE_LIBRARY']=str(runtime/'libspice-server-fixed.dylib');env['ANGLE_DEFAULT_PLATFORM']='metal'
 command=[str(runtime/'probe-loader'),str(app/'Frameworks/qemu-aarch64-softmmu.framework/Versions/A/qemu-aarch64-softmmu'),'-L',str(app/'Resources/qemu'),'-machine','virt,accel=hvf','-cpu','host','-m','512M','-smp','1','-nodefaults','-display','none','-serial','none','-monitor','none','-S','-qmp',f'unix:{qmp},server=on,wait=off','-spice',f'unix=on,addr={sock},disable-ticketing=on,gl=es','-device','virtio-gpu-gl-pci']
 with open(str(prefix)+'.qemu.log','x') as log:
  try:
   proc=subprocess.Popen(command,stdout=log,stderr=log,stdin=subprocess.DEVNULL,env=env)
   deadline=time.monotonic()+10
   while not (sock.exists() and qmp.exists()):
    if proc.poll() is not None or time.monotonic()>deadline:raise RuntimeError('Paused QEMU startup failed')
    time.sleep(.05)
   control=module.QMP(qmp);report['qmp_before']=control.run('query-status')
   client=console_test.Client(str(sock),str(prefix))
   deadline=time.monotonic()+5
   while True:
    kind,epoch,data=client.next(max(.01,deadline-time.monotonic()))
    if kind==1 and data.get('event')=='ready':report['ready']=data;break
   shot=client.request('screenshot');assert shot['error']['code']=='frame_unavailable';report['screenshot']=shot
   start=client.request('start',epoch=1,codec='h264');assert start['result']['state']=='waiting-frame';report['start']=start
   stop=client.request('stop',epoch=1);assert stop['result']['encoded']==0;report['stop']=stop
   client.request('shutdown');report['native_exit']=client.child.wait(timeout=3);assert report['native_exit']==0
   report['pipe_checks']=[]
   for mode in ['eof','broken-pipe','oversized-input','backpressure','shutdown-batch']:
    started=time.monotonic();error_path=pathlib.Path(str(prefix)+'.'+mode+'.stderr')
    with error_path.open('x') as errors:
     child=subprocess.Popen([str(ROOT/'.local/bin/spice-console'),str(sock),'--persistent'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=errors)
     writer=None
     if mode=='eof':child.stdin.close()
     elif mode=='broken-pipe':child.stdout.close()
     elif mode=='oversized-input':child.stdin.write(b'x'*4097);child.stdin.flush()
     elif mode=='shutdown-batch':child.stdin.write(b'{"id":"quit","method":"shutdown"}\n{"id":"late","method":"start","epoch":1,"codec":"hevc"}\n');child.stdin.flush()
     else:
      def flood():
       try:
        child.stdin.write((json.dumps({'id':'bounded','method':'screenshot'})+'\n').encode()*4000);child.stdin.flush()
       except (BrokenPipeError,OSError):pass
      writer=threading.Thread(target=flood,daemon=True);writer.start()
     try:code=child.wait(timeout=4)
     finally:
      if child.poll() is None:child.kill();child.wait()
     if writer:writer.join(timeout=1)
    native_events=[json.loads(line) for line in error_path.read_text().splitlines() if line.startswith('{')]
    expected=0 if mode in ['eof','shutdown-batch'] else 1
    if mode=='shutdown-batch':
     output=child.stdout.read();assert b'"quit"' in output and b'"late"' not in output
    assert code==expected and native_events[-1]['cached_frame_released']
    if mode=='oversized-input':assert native_events[-1]['error']=='command_line_limit'
    if mode in ['broken-pipe','backpressure']:assert native_events[-1]['error'] in ['stdout_timeout','stdout_closed_or_stalled']
    report['pipe_checks'].append({'mode':mode,'exit_code':code,'elapsed_ms':round((time.monotonic()-started)*1000,2),'native_events':native_events})
   report['qmp_after']=control.run('query-status');assert report['qmp_after']['running'] is False
   control.run('quit');control.close();report['qemu_exit']=proc.wait(timeout=5);assert report['qemu_exit']==0;report['passed']=True
  except Exception as e:report['error']=repr(e)
  finally:
   if client:client.close()
   if proc and proc.poll() is None:proc.terminate();proc.wait(timeout=5)
prefix.with_suffix('.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report,indent=2));raise SystemExit(0 if report['passed'] else 1)
