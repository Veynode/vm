#!/usr/bin/env python3
"""Real Apple VM resource-admission and hard-link disk conflict validation."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile
import time
ROOT=Path(__file__).resolve().parents[1]
p=argparse.ArgumentParser();p.add_argument('--config',type=Path,required=True);p.add_argument('--output',type=Path,default=ROOT/'.local/evidence/control-resources.json');args=p.parse_args()
result={'checks':[],'started_at_unix':time.time(),'fixture':'actual Apple Virtualization.framework VM workers; disposable raw disk'}
def record(name,value):result['checks'].append({'name':name,'evidence':value})
with tempfile.TemporaryDirectory(prefix='vm-resources-') as tmp:
    tmp=Path(tmp);state=tmp/'state';disk=tmp/'disk.raw';alias=tmp/'disk-alias.raw'
    with disk.open('wb') as f:f.truncate(8*1024*1024)
    alias.hardlink_to(disk)
    daemon=subprocess.Popen([str(ROOT/'target/debug/vm-control'),'daemon','--state-dir',str(state),'--backend',str(ROOT/'.local/bin/vm-apple'),'--memory-budget-mib','1024'],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
    def call(method,payload=None,success=True):
        command=[str(ROOT/'target/debug/vm-control'),'call','--state-dir',str(state),method,json.dumps(payload or {})]
        output=subprocess.run(command,capture_output=True,text=True,timeout=45)
        if success:
            assert output.returncode==0,output.stderr
            return json.loads(output.stdout)
        assert output.returncode!=0,output.stdout
        return output.stderr.strip()
    def running(vm_id):
        for _ in range(100):
            status=call('status',{'id':vm_id})
            if status['state']=='running':return status
            assert status['state'] not in ['error','crashed','stopped'],status
            time.sleep(.05)
        raise TimeoutError('native VM did not start')
    def no_worker(vm_id):
        status=call('status',{'id':vm_id})
        assert status['state']=='stopped' and 'pid' not in status,status
        assert not state.joinpath(vm_id+'.json').exists(),'per-worker config was written before admission'
        return status
    try:
        for _ in range(100):
            if state.joinpath('control.sock').exists():break
            if daemon.poll() is not None:raise RuntimeError(daemon.stderr.read().decode())
            time.sleep(.025)
        capabilities=call('capabilities')['resource_admission'];assert capabilities['vm_memory_budget_mib']==1024 and capabilities['active_assigned_memory_mib']==0;record('explicit budget capabilities',capabilities)
        template=json.loads(args.config.read_text())
        for vm_id,ram,attached in [('first',512,None),('disk-owner',512,disk),('hardlink-conflict',512,alias),('second',512,None),('over-budget',256,None)]:
            config=dict(template);config.update(id=vm_id,memory_mib=ram,serial_log=str(tmp/(vm_id+'.log')));config.pop('disk',None)
            if attached:config['disk']=str(attached)
            call('create',config)
        call('start',{'id':'first'});record('first real worker',running('first'))
        call('start',{'id':'second'});record('second real worker at exact budget',running('second'))
        capabilities=call('capabilities')['resource_admission'];assert capabilities['active_assigned_memory_mib']==1024 and capabilities['remaining_budget_mib']==0;record('exact budget accounted',capabilities)
        error=call('start',{'id':'over-budget'},success=False);assert 'memory admission denied' in error,error;record('over-budget denied before worker spawn',{'error':error,'status':no_worker('over-budget')})
        call('force_stop',{'id':'second','confirm':'second'})
        stopped=call('status',{'id':'second'});assert stopped['state']=='stopped';record('intentional stop is not reported as a crash',stopped)
        capabilities=call('capabilities')['resource_admission'];assert capabilities['active_assigned_memory_mib']==512;record('exited worker releases assigned budget',capabilities)
        call('start',{'id':'over-budget'});record('previously denied VM starts after capacity freed',running('over-budget'))
        for vm_id in ['first','over-budget']:call('force_stop',{'id':vm_id,'confirm':vm_id})
        call('start',{'id':'disk-owner'});record('real VM holds disk',running('disk-owner'))
        error=call('start',{'id':'hardlink-conflict'},success=False);assert 'inode' in error,error;record('hard-link disk conflict rejected before spawn',{'error':error,'status':no_worker('hardlink-conflict')})
        result['passed']=True
    except Exception as exc:
        result['passed']=False;result['error']=str(exc);raise
    finally:
        for vm_id in ['first','second','over-budget','disk-owner']:
            subprocess.run([str(ROOT/'target/debug/vm-control'),'call','--state-dir',str(state),'force_stop',json.dumps({'id':vm_id,'confirm':vm_id})],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=10)
        daemon.terminate();daemon.wait(timeout=5)
        result['finished_at_unix']=time.time();args.output.parent.mkdir(parents=True,exist_ok=True)
        output=args.output if result.get('passed') else args.output.with_suffix('.failed.json');output.write_text(json.dumps(result,indent=2)+'\n')
print(f'Resource admission checks passed: {args.output}')
