#!/usr/bin/env python3
"""Run a real Apple macOS install, recording progress and an explicit completion marker.

A failed/interrupted identity is never overwritten. Inspect its status before
choosing recovery; create a new named fixture when reinstalling is necessary.
"""
import argparse,datetime,json,pathlib,queue,subprocess,threading,time
p=argparse.ArgumentParser();p.add_argument('--config',type=pathlib.Path,required=True);p.add_argument('--binary',default='.local/bin/vm-apple');p.add_argument('--timeout-seconds',type=int,default=3600);a=p.parse_args()
config=a.config.resolve(strict=True);state_dir=config.parent
status_path=state_dir/'installation-status.json';events_path=state_dir/'installation-events.jsonl'
if status_path.exists():
    old=json.loads(status_path.read_text())
    raise SystemExit('Installation already attempted: '+old.get('state','unknown')+'. Refusing implicit overwrite/reinstall.')
def status(state,**extra):
    value={'state':state,'timestamp':datetime.datetime.now(datetime.timezone.utc).isoformat(),'config':str(config),**extra}
    temporary=status_path.with_suffix('.tmp');temporary.write_text(json.dumps(value,indent=2)+'\n');temporary.replace(status_path)
status('starting')
with events_path.open('x') as log,(state_dir/'installation-stderr.log').open('x') as errors:
    child=subprocess.Popen([a.binary,'install','--config',str(config)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=errors,text=True)
    messages=queue.Queue()
    def reader():
        for line in child.stdout: messages.put(line)
        messages.put(None)
    threading.Thread(target=reader,daemon=True).start()
    started=time.monotonic();last_percent=-1;complete=False
    try:
        while time.monotonic()-started < a.timeout_seconds:
            try: line=messages.get(timeout=1)
            except queue.Empty: continue
            if line is None: raise RuntimeError('Native process exited before installation_complete; exit='+str(child.poll()))
            event=json.loads(line);timestamp=datetime.datetime.now(datetime.timezone.utc).isoformat()
            log.write(json.dumps({'timestamp':timestamp,'event':event})+'\n');log.flush()
            if event.get('event')=='installation_progress':
                fraction=event['fraction'];percent=int(fraction*100)
                if percent != last_percent:
                    last_percent=percent;status('installing',fraction=fraction,elapsed_seconds=time.monotonic()-started);print(f'Installing {percent}%',flush=True)
            elif event.get('event')=='installation_complete':
                complete=True;status('installed',elapsed_seconds=time.monotonic()-started,native_state=event.get('state'));print('macOS installation_complete',flush=True);break
            elif event.get('state')=='error': raise RuntimeError(event.get('message','Native error'))
        if not complete: raise TimeoutError('Installation timeout')
    except BaseException as error:
        status('failed',error=str(error),elapsed_seconds=time.monotonic()-started)
        raise
    finally:
        child.stdin.close()
        try: child.wait(timeout=15)
        except subprocess.TimeoutExpired: child.terminate();child.wait(timeout=10)
