#!/usr/bin/env python3
"""Real VZ disk-lock and APFS CoW validation; blank disposable raw disk, no guest snapshot claim."""
import argparse
import fcntl
import hashlib
import json
from pathlib import Path
import select
import subprocess
import tempfile
import time
ROOT=Path(__file__).resolve().parents[1]
p=argparse.ArgumentParser();p.add_argument('--config',type=Path,required=True);p.add_argument('--output',type=Path,default=ROOT/'.local/evidence/control-images.json');args=p.parse_args()
result={'checks':[],'started_at_unix':time.time(),'fixture':'8 MiB disposable blank raw disk; real native VZ worker; no installed guest filesystem or snapshot acceptance'}
def record(name,value):result['checks'].append({'name':name,'evidence':value})
with tempfile.TemporaryDirectory(prefix='vm-images-') as tmp:
    tmp=Path(tmp);disk=tmp/'disk.raw';store=tmp/'store';config=tmp/'config.json';worker=None
    with disk.open('wb') as f:f.truncate(8*1024*1024)
    vm=json.loads(args.config.read_text());vm.update(id='images-lock-smoke',disk=str(disk),serial_log=str(tmp/'serial.log'));config.write_text(json.dumps(vm))
    def image(arguments,success=True):
        out=subprocess.run([str(ROOT/'target/debug/vm-image'),*arguments,'--store',str(store)],capture_output=True,text=True,timeout=30)
        if success:
            assert out.returncode==0,out.stderr
            return json.loads(out.stdout)
        assert out.returncode!=0,out.stdout
        return out.stderr.strip()
    def next_event():
        assert select.select([worker.stdout],[],[],15)[0],'worker output timeout'
        line=worker.stdout.readline();assert line,'worker closed stdout';return json.loads(line)
    try:
        worker=subprocess.Popen([str(ROOT/'.local/bin/vm-apple'),'run','--config',str(config)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,bufsize=1)
        state=next_event();assert state.get('state')=='running',state;record('actual VZ worker running with disk',state)
        with disk.open('rb') as source:
            try:fcntl.flock(source,fcntl.LOCK_SH|fcntl.LOCK_NB)
            except BlockingIOError:record('VZ exclusive disk lock rejects shared import lock',True)
            else:raise AssertionError('VZ live disk unexpectedly allowed shared flock')
        error=image(['import','--id','active','--source',str(disk),'--source-stopped'],success=False)
        assert 'busy' in error,error;assert not (store/'bases/active').exists();record('CLI refuses active disk before publication',error)
        worker.stdin.write(json.dumps({'id':1,'method':'stop'})+'\n');worker.stdin.flush()
        while worker.poll() is None:
            event=next_event()
            if event.get('event')=='state' and event.get('state')=='stopped':break
        worker.stdin.close();assert worker.wait(timeout=10)==0
        base=image(['import','--id','base','--source',str(disk),'--source-stopped']);record('stopped blank raw disk imported as readonly base',base)
        clone=image(['clone','--base','base','--id','copy']);record('APFS writable clone',clone)
        base_path=Path(base['disk_path']);clone_path=Path(clone['disk_path']);original=hashlib.sha256(base_path.read_bytes()).hexdigest()
        with clone_path.open('r+b') as f:f.seek(4096);f.write(b'PRIVATE-CLONE-BLOCK')
        assert hashlib.sha256(base_path.read_bytes()).hexdigest()==original==hashlib.sha256(disk.read_bytes()).hexdigest()
        assert hashlib.sha256(clone_path.read_bytes()).hexdigest()!=original;record('clone mutation preserves base and source hashes',{'base_sha256':original,'clone_sha256':hashlib.sha256(clone_path.read_bytes()).hexdigest()})
        failed=image(['clone','--base','base','--id','copy'],success=False);assert 'overwrite' in failed;record('existing image path cannot be overwritten',True)
        failed=image(['clone','--base','base','--id','no-space','--min-free-mib','17592186044415'],success=False);assert 'insufficient free space' in failed;assert not (store/'clones/no-space').exists();assert hashlib.sha256(base_path.read_bytes()).hexdigest()==original;record('free-space refusal preserves base and avoids partial publication',True)
        result['passed']=True
    except Exception as exc:result['passed']=False;result['error']=str(exc);raise
    finally:
        if worker and worker.poll() is None:worker.terminate();worker.wait(timeout=5)
        result['finished_at_unix']=time.time();args.output.parent.mkdir(parents=True,exist_ok=True)
        output=args.output if result.get('passed') else args.output.with_suffix('.failed.json');output.write_text(json.dumps(result,indent=2)+'\n')
print(f'Image checks passed: {args.output}')
