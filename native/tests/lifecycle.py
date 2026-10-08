#!/usr/bin/env python3
"""Real VZ boot test; requires prepared Linux direct-kernel fixture, no mock backend."""
import argparse,base64,json,pathlib,queue,subprocess,tempfile,threading,time
p=argparse.ArgumentParser();p.add_argument('--config',required=True);p.add_argument('--binary',default='.local/bin/vm-apple');p.add_argument('--window',action='store_true');a=p.parse_args()
config=json.loads(pathlib.Path(a.config).read_text())
with tempfile.TemporaryDirectory(prefix='veynode-vz-test-') as directory:
    d=pathlib.Path(directory);config['serial_log']=str(d/'serial.log');config['window']=False
    (d/'config.json').write_text(json.dumps(config))
    child=subprocess.Popen([a.binary,'run','--config',str(d/'config.json')],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
    replies=queue.Queue();transcript=[]
    def reader():
        for line in child.stdout:
            try: replies.put(json.loads(line))
            except Exception as error: replies.put({'invalid_json':str(error),'line':line})
    threading.Thread(target=reader,daemon=True).start()
    def wait(predicate,seconds=20):
        end=time.monotonic()+seconds
        while time.monotonic()<end:
            response=replies.get(timeout=max(.01,end-time.monotonic()));transcript.append(response)
            if predicate(response): return response
        raise AssertionError('No matching response')
    sequence=0
    def command(method,**extra):
        global sequence
        sequence+=1;child.stdin.write(json.dumps({'id':sequence,'method':method,**extra})+'\n');child.stdin.flush()
        return wait(lambda v:v.get('id')==sequence)
    try:
        assert wait(lambda v:v.get('event')=='state')['state']=='running'
        assert command('status')['result']['state']=='running'
        assert command('nonexistent')['error']['code']=='unsupported'
        if a.window:
            assert command('show_window')['result']['window_open']
            time.sleep(1)
            assert command('close_window')['result']['state']=='running'
            assert not command('status')['result']['window_open']
        time.sleep(2)
        marker='VEYNODE_'+'REAL_GUEST'
        payload="echo 'VEYNODE_'REAL_GUEST; /bin/busybox uname -a; /bin/busybox mkdir -p /proc; /bin/busybox mount -t proc proc /proc; /bin/busybox cat /proc/meminfo | /bin/busybox head -3\n"
        assert command('serial_write',data=base64.b64encode(payload.encode()).decode())['result']['bytes']==len(payload)
        end=time.monotonic()+10
        while time.monotonic()<end:
            serial=(d/'serial.log').read_text(errors='replace')
            if '\n'+marker+'\n' in serial and 'Linux ' in serial and 'MemTotal:' in serial: break
            time.sleep(.1)
        else: raise AssertionError('Guest did not execute serial command')
        print(json.dumps({'test':'real-linux-lifecycle','window_tested':a.window,'guest_serial_tail':serial[-1400:],'protocol':transcript},indent=2))
        # Owner loss must not orphan its virtual machine.
        child.stdin.close();child.wait(timeout=10)
        assert child.returncode==0
        print('PASS: real boot, status, error, console execution, optional window close, owner EOF cleanup')
    finally:
        if child.poll() is None: child.kill();child.wait()
