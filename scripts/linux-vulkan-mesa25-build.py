#!/usr/bin/env python3
"""Bounded process/log supervisor for the isolated RAM Guest Mesa build only."""
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import time

def supervise(argv, path, seconds=600, limit=1024*1024):
    process=None
    result={'passed':False,'output_limit':limit,'deadline_seconds':seconds}
    start=time.monotonic()
    with path.open('xb') as log:
        try:
            process=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,start_new_session=True)
            count=0
            while True:
                if time.monotonic()-start>=seconds:
                    result['failure']='deadline';break
                if not select.select([process.stdout],[],[],max(0,min(.1,seconds-(time.monotonic()-start))))[0]:continue
                data=os.read(process.stdout.fileno(),min(65536,limit-count+1))
                if not data:
                    result['stdout_eof']=True;break
                room=limit-count;log.write(data[:room]);count+=len(data)
                if count>limit:
                    result['failure']='output_limit';break
            result['output_bytes']=min(count,limit)
            if 'failure' not in result:
                try:result['exit_code']=process.wait(timeout=max(.01,seconds-(time.monotonic()-start)))
                except subprocess.TimeoutExpired:result['failure']='deadline'
            result['passed']=result.get('stdout_eof') is True and result.get('exit_code')==0 and 'failure' not in result
        finally:
            if process is not None:
                # Do not poll/reap the leader before killing its still-owned group.
                # A leader can exit while a descendant keeps this stdout pipe open.
                if process.returncode is None:
                    try:os.killpg(process.pid,signal.SIGKILL);result['forced_group_kill']=True
                    except ProcessLookupError:pass
                result['exit_code']=process.wait(timeout=5);result['owned_child_reaped']=True
                process.stdout.close()
            result['elapsed_seconds']=time.monotonic()-start
    return result

if __name__=='__main__':
    if os.uname().sysname!='Linux' or os.uname().machine!='aarch64' or 'rdinit=/veynode/gpu-init' not in Path('/proc/cmdline').read_text():
        raise SystemExit('requires explicit RAM Guest fixture')
    r=supervise(['/bin/sh','/build/compile.sh'],Path('/run/mesa25-build.log'))
    Path('/run/mesa25-build-result.json').write_text(json.dumps(r)+'\n')
    print('MESA25_BUILD_SUPERVISOR '+json.dumps(r),flush=True)
    raise SystemExit(0 if r['passed'] else 1)
