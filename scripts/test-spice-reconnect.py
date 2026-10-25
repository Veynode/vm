#!/usr/bin/env python3
"""Inspect GL scanout FDs across fresh consumers; failed reconnect remains exit 1."""
import argparse,datetime,json,os,pathlib,subprocess,sys,time,re,threading
ROOT=pathlib.Path(__file__).resolve().parents[1]
os.umask(0o077)

def classify(cycles):
 if len(cycles)!=3:return {'consumed_shared_pipe_confirmed':False}
 before=[]
 for cycle in cycles:
  entries=[e for e in cycle['events'] if e.get('event')=='fd' and e.get('stage')=='before-read']
  if len(entries)!=1:return {'consumed_shared_pipe_confirmed':False}
  before.append(entries[0])
 same=len({(e['device'],e['inode']) for e in before})==1
 confirmed=(same and [e['available_bytes'] for e in before]==[4,4,0] and all(e['is_fifo'] and not e['pollhup'] for e in before) and cycles[1]['exit_code']==0 and cycles[2]['exit_code']!=0 and any(e.get('event')=='read' and e.get('bytes')==4 for e in cycles[1]['events']) and any(e.get('event')=='result' and e.get('notifications')==1 and e.get('draws',0)>0 for e in cycles[2]['events']))
 return {'consumed_shared_pipe_confirmed':confirmed,'same_fifo_inode_across_processes':same,'available_bytes_before_each_connection': [e['available_bytes'] for e in before],'pollhup_on_any_connection':any(e['pollhup'] for e in before),'reconnect_passed':False if confirmed else None}

def consumer(socket,prefix):
 cycles=[]
 count=int(os.environ.get('VEYNODE_RECONNECT_CYCLES','3'))
 if not 3<=count<=40:raise ValueError('Diagnostic cycles must be3..40')
 modes=['consume']*count if os.environ.get('VEYNODE_RECONNECT_THREE_CAPTURES')=='1' else ['observe','consume','consume']
 stress=os.environ.get('VEYNODE_RECONNECT_STRESS')=='1';fd_counts=[];fault=None;producer_pid=None
 if stress:
  pids=subprocess.check_output(['/usr/sbin/lsof','-t',socket],text=True).split()
  if len(set(pids))!=1:raise RuntimeError('Private fixture socket must have exactly one producer PID')
  producer_pid=int(pids[0])
 def fd_count():
  fields=subprocess.check_output(['/usr/sbin/lsof','-a','-p',str(producer_pid),'-F','f'],text=True)
  return sum(bool(re.match(r'^f[0-9]+',line)) for line in fields.splitlines())
 for index,mode in enumerate(modes):
  if stress and index==4:
   child=subprocess.Popen([str(ROOT/'.local/bin/spice-reconnect-probe'),socket,'consume'],stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,text=True)
   timer=threading.Timer(7,child.kill);timer.start();bound=False
   try:
    for line in child.stdout:
     if line.startswith('{') and json.loads(line).get('event')=='bound-surface':bound=True;child.kill();break
    code=child.wait(timeout=3)
   finally:
    timer.cancel()
    if child.poll() is None:child.kill();child.wait()
   if not bound:raise RuntimeError('Crash injection did not reach bound actual surface')
   fault={'after_completed_cycles':index,'child_killed_after_surface_bound':True,'exit_code':code};time.sleep(0.1)
  run=subprocess.run([str(ROOT/'.local/bin/spice-reconnect-probe'),socket,mode],capture_output=True,text=True,timeout=15)
  events=[json.loads(line) for line in run.stdout.splitlines() if line.startswith('{')]
  cycles.append({'mode':mode,'exit_code':run.returncode,'events':events,'stderr':run.stderr})
  if stress:time.sleep(0.1);fd_counts.append(fd_count())
 report={'producer_fd_counts_after_cycles':fd_counts,'fault_injection':fault,'diagnosis':classify(cycles),'cycles':cycles,'reconnect_passed':all(c['exit_code']==0 for c in cycles),'all_consumers_capture':all(m=='consume' for m in modes),'scope':'Independent processes; each consume reads only its newly delivered live FD. Optional observer leaves descriptor data untouched. No cached surface IDs; optional crash injection kills only our diagnostic child.'}
 if stress:
  report['fd_count_stable_after_warmup']=len(set(fd_counts[2:]))==1
  report['reconnect_passed'] &= report['fd_count_stable_after_warmup'] and fault is not None
 pathlib.Path(prefix+'.reconnect.json').write_text(json.dumps(report,indent=2)+'\n')
 print(json.dumps(report,indent=2))
 return 0 if report['reconnect_passed'] else 1

if len(sys.argv)==3 and not sys.argv[1].startswith('-'):
 raise SystemExit(consumer(sys.argv[1],sys.argv[2]))
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--analyze',type=pathlib.Path,help='Classify saved evidence without starting a VM; diagnosis is not reconnect acceptance')
parser.add_argument('--output',type=pathlib.Path,default=ROOT/'.local/evidence'/('spice-reconnect-'+datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ')+'.json'))
args=parser.parse_args()
if args.analyze:
 diagnosis=classify(json.loads(args.analyze.read_text())['cycles']);print(json.dumps(diagnosis,indent=2));raise SystemExit(0 if diagnosis['consumed_shared_pipe_confirmed'] else 1)
output=args.output.resolve();output.parent.mkdir(parents=True,exist_ok=True)
if output.exists():raise SystemExit('Refusing to overwrite evidence')
subprocess.run([str(ROOT/'native/tools/build-spice-reconnect.sh')],check=True)
fixture=output.with_suffix('.fixture.json');prefix=str(fixture.with_suffix(''))
run=subprocess.run(['python3',str(ROOT/'scripts/qemu-linux-desktop-probe.py'),'--output',str(fixture),'--capture-cycles','0','--extra-consumer',str(pathlib.Path(__file__).resolve())],capture_output=True,text=True,timeout=270)
pathlib.Path(prefix+'.runner.log').write_text(run.stdout+run.stderr)
report={'timestamp_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'fixture_exit_code':run.returncode,'reconnect_passed':False}
if fixture.exists():report['fixture']=json.loads(fixture.read_text())
result=pathlib.Path(prefix+'-extra.reconnect.json')
if result.exists():report.update(json.loads(result.read_text()))
output.write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report,indent=2))
raise SystemExit(0 if report['reconnect_passed'] else 1)
