#!/usr/bin/env python3
"""Actual Guest resize / owned producer crash / fresh VM capture diagnostics."""
import argparse, datetime, hashlib, importlib.util, json, os, pathlib, signal, subprocess, sys, threading, time
ROOT=pathlib.Path(__file__).resolve().parents[1]
SELF=pathlib.Path(__file__).resolve()
RUNTIME=ROOT/'.local/spice-reconnect-build/runtime'
PROBE=ROOT/'.local/bin/spice-scanout-lifecycle-probe'
os.umask(0o077)

def read_json(path):
 return json.loads(path.read_text()) if path.exists() else None

def consumer(socket, prefix):
 scenario=os.environ['VEYNODE_LIFECYCLE_SCENARIO']; reports=[]; killed=None
 modes=['resize','once'] if scenario=='resize' else ['death' if scenario=='death' else 'once']
 for index,mode in enumerate(modes):
  path=pathlib.Path(prefix+f'-{index}')
  args=[str(PROBE),socket,mode,str(path)]
  if mode=='death':
   pids=set(subprocess.check_output(['/usr/sbin/lsof','-t',socket],text=True).split())
   if len(pids)!=1:raise RuntimeError('Private fixture socket must identify exactly one producer')
   pid=int(next(iter(pids)))
   command=subprocess.check_output(['/bin/ps','-p',str(pid),'-o','command='],text=True).strip()
   if not command.startswith(str(RUNTIME/'probe-loader')+' ') or f'addr={socket},' not in command:
    raise RuntimeError('Refusing to signal a process not matching our exact fixture and socket')
   with pathlib.Path(str(path)+'.stderr').open('w') as err:
    process=subprocess.Popen(args,stdout=subprocess.PIPE,stderr=err,text=True)
    timer=threading.Timer(45,process.kill);timer.start();lines=[]
    try:
     for line in process.stdout:
      lines.append(line)
      if killed is None and line.startswith('{') and json.loads(line).get('event')=='frame':
       started=time.monotonic();os.kill(pid,signal.SIGKILL)
       killed={'signal':'SIGKILL','producer_pid':pid,'after_actual_frame':True}
     code=process.wait(timeout=5)
     if killed:killed['consumer_exit_after_kill_ms']=round((time.monotonic()-started)*1000,2)
    finally:
     timer.cancel()
     if process.poll() is None:process.kill();process.wait()
    pathlib.Path(str(path)+'.stdout').write_text(''.join(lines))
  else:
   run=subprocess.run(args,capture_output=True,text=True,timeout=45);code=run.returncode
   pathlib.Path(str(path)+'.stdout').write_text(run.stdout);pathlib.Path(str(path)+'.stderr').write_text(run.stderr)
  result=read_json(pathlib.Path(str(path)+'.lifecycle.json'))
  changing=None if mode=='death' else True
  if result and mode!='death':
   frames=[e for e in result['events'] if e['event']=='frame']
   changing=bool(frames) and all(len({f['rgb_hash'] for f in frames if (f['width'],f['height'])==size})>=2 for size in {(f['width'],f['height']) for f in frames})
  reports.append({'exit_code':code,'probe':result,'captured_sizes_have_changing_frames':changing})
  if code or not result or not result['success']:break
 passed=len(reports)==len(modes) and all(r['exit_code']==0 and r['probe']['success'] and r['captured_sizes_have_changing_frames'] is not False for r in reports)
 if scenario=='resize':passed=passed and reports[-1]['probe']['resized_frames']>=2
 if scenario=='death':passed=passed and killed is not None
 result={'scenario':scenario,'success':passed,'consumers':reports,'fault_injection':killed,'scope':'Actual fresh scanout FD per generation; producer death targets only exact private disposable fixture socket/process. No global surface search or cached IDs.'}
 pathlib.Path(prefix+'.lifecycle.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2))
 return 0 if passed else 1

def fixture(scenario,output):
 spec=importlib.util.spec_from_file_location('desktop_fixture',ROOT/'scripts/qemu-linux-desktop-probe.py');module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module);module.BASE=RUNTIME
 if scenario=='resize':
  assert module.INIT.count('apk add --no-cache --no-progress ')==1
  assert module.INIT.count('echo VEYNODE_DESKTOP_READY')==1
  module.INIT=module.INIT.replace('apk add --no-cache --no-progress ','apk add --no-cache --no-progress xrandr ')
  module.INIT=module.INIT.replace('echo VEYNODE_DESKTOP_READY','''(
 sleep 10
 echo VEYNODE_RESIZE_BEGIN
 xrandr --query
 if xrandr --output Virtual-1 --mode 1024x768; then echo VEYNODE_RESIZE_OK; else echo VEYNODE_RESIZE_FAILED; fi
 xrandr --query
 echo VEYNODE_RESIZE_END
) &
echo VEYNODE_DESKTOP_READY''')
 pathlib.Path(str(output)+'.init-sha256').write_text(hashlib.sha256(module.INIT.encode()).hexdigest()+'\n')
 sys.argv=[str(SELF),'--output',str(output),'--capture-cycles','0','--extra-consumer',str(SELF)]
 return module.main()

if len(sys.argv)==3 and not sys.argv[1].startswith('-'):raise SystemExit(consumer(*sys.argv[1:]))
parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--scenario',choices=['resize','death','recovery'],required=True);parser.add_argument('--output',type=pathlib.Path,required=True);parser.add_argument('--fixture',action='store_true');args=parser.parse_args()
os.environ['VEYNODE_LIFECYCLE_SCENARIO']=args.scenario
if not os.environ.get('VEYNODE_SPICE_LIBRARY'):raise SystemExit('Set VEYNODE_SPICE_LIBRARY to explicit independently built server')
if args.fixture:raise SystemExit(fixture(args.scenario,args.output))
output=args.output.resolve();output.parent.mkdir(exist_ok=True,parents=True)
if output.exists():raise SystemExit('Refusing to overwrite evidence')
subprocess.run([str(ROOT/'native/tools/build-spice-lifecycle.sh')],check=True)
fixture_path=output.with_suffix('.fixture.json')
process=subprocess.Popen([sys.executable,str(SELF),'--fixture','--scenario',args.scenario,'--output',str(fixture_path)],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
fixture_timed_out=False
try: stdout,stderr=process.communicate(timeout=270)
except subprocess.TimeoutExpired:
 fixture_timed_out=True
 # The imported fixture catches KeyboardInterrupt and cleans up only its own VM.
 process.send_signal(signal.SIGINT)
 try: stdout,stderr=process.communicate(timeout=15)
 except subprocess.TimeoutExpired:
  process.kill();stdout,stderr=process.communicate()
run=subprocess.CompletedProcess(process.args,process.returncode,stdout,stderr)
output.with_suffix('.runner.log').write_text(run.stdout+run.stderr)
extra=read_json(pathlib.Path(str(fixture_path.with_suffix(''))+'-extra.lifecycle.json'));outer=read_json(fixture_path)
serial=fixture_path.with_suffix('.serial.log').read_text(errors='replace')
passed=not fixture_timed_out and bool(extra and extra['success'])
if args.scenario!='death':passed=passed and run.returncode==0 and bool(outer and outer.get('capture_path_passed'))
else:passed=passed and run.returncode==1 and bool(outer and outer.get('extra_consumer_exit_code')==0 and outer.get('capture_path_passed') is False)
resize_evidence=serial.split('VEYNODE_RESIZE_BEGIN',1)[1].split('VEYNODE_RESIZE_END',1)[0] if 'VEYNODE_RESIZE_END' in serial else None
if args.scenario=='resize':passed=passed and bool(resize_evidence and 'VEYNODE_RESIZE_OK' in resize_evidence)
result={'timestamp_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'scenario':args.scenario,'success':passed,'fixture_exit_code':run.returncode,'fixture_timed_out':fixture_timed_out,'fixture':outer,'extra_consumer_lifecycle':extra,'guest_resize_evidence':resize_evidence,'library_sha256':hashlib.sha256(pathlib.Path(os.environ['VEYNODE_SPICE_LIBRARY']).read_bytes()).hexdigest(),'injected_guest_init_sha256':pathlib.Path(str(fixture_path)+'.init-sha256').read_text().strip(),'scope':'Outer default reconnect test is disabled; actual independent lifecycle consumers are recorded here. Producer death deliberately fails outer still-running assertion; it is not normal capture success. Recovery is a fresh RAM-only VM, not resume. Visual corruption remains; no performance test.'}
output.write_text(json.dumps(result,indent=2)+'\n');print(json.dumps({'success':passed,'scenario':args.scenario,'output':str(output)},indent=2));raise SystemExit(0 if passed else 1)
