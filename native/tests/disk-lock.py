#!/usr/bin/env python3
"""Real VZ boot + cross-process writable disk hard-link lock validation."""
import argparse,json,os,pathlib,queue,subprocess,tempfile,threading
p=argparse.ArgumentParser();p.add_argument('--config',type=pathlib.Path,required=True);p.add_argument('--binary',default='.local/bin/vm-apple');a=p.parse_args()
base=json.loads(a.config.read_text())
def start(config):
    child=subprocess.Popen([a.binary,'run','--config',str(config)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
    output=queue.Queue()
    def read():
        for line in child.stdout:output.put(json.loads(line))
    threading.Thread(target=read,daemon=True).start()
    return child,output
with tempfile.TemporaryDirectory(prefix='veynode-disk-lock-') as temporary:
    directory=pathlib.Path(temporary);disk=directory/'disk.raw';alias=directory/'alias.raw'
    with disk.open('xb') as f:f.truncate(8*1024*1024)
    os.link(disk,alias)
    first={**base,'id':'disk-lock-first','disk':str(disk),'window':False,'serial_log':str(directory/'first.log')}
    second={**base,'id':'disk-lock-second','disk':str(alias),'window':False,'serial_log':str(directory/'second.log')}
    first_path=directory/'first.json';second_path=directory/'second.json'
    first_path.write_text(json.dumps(first));second_path.write_text(json.dumps(second))
    one,one_output=start(first_path)
    try:
        event=one_output.get(timeout=20);assert event.get('state')=='running',event
        two,two_output=start(second_path)
        try:
            event=two_output.get(timeout=20)
            assert event.get('state')=='error' and 'already in use' in event.get('message',''),event
            two.wait(timeout=10);assert two.returncode==1
        finally:
            if two.poll() is None:two.kill();two.wait()
        one.stdin.write(json.dumps({'id':1,'method':'status'})+'\n');one.stdin.flush()
        status=one_output.get(timeout=5);assert status['result']['state']=='running',status
        one.stdin.close();one.wait(timeout=10);assert one.returncode==0
        # Once the owning worker exits, the identical inode can be reused.
        three,three_output=start(second_path)
        try:
            event=three_output.get(timeout=20);assert event.get('state')=='running',event
            three.stdin.close();three.wait(timeout=10);assert three.returncode==0
        finally:
            if three.poll() is None:three.kill();three.wait()
        print('PASS: real Linux boot, second worker hard-link disk rejected, first stays running, lock released after worker exit')
    finally:
        if one.poll() is None:one.kill();one.wait()
