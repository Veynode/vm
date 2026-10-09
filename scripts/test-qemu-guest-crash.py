#!/usr/bin/env python3
"""Actual one-shot SIGKILL of the installed Guest service; no daemon launch.

Only use a successfully maintained private prepare-qemu-guest-crash fixture.
This deliberately crashes a disposable VM. Never reports a scope-empty ACK.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import time

ROOT=Path(__file__).resolve().parents[1]
def load(name,path):
    s=importlib.util.spec_from_file_location(name,ROOT/'scripts'/path);m=importlib.util.module_from_spec(s);s.loader.exec_module(m);return m
base=load('crash_baseline','test-installed-qemu-exec.py')
process=load('crash_process','test-qemu-exec-guardian.py')
oracle=load('crash_oracle','test-installed-qemu-exec-lost-ack.py')
require=base.require


def events(log):
    if not log.exists():return []
    require(log.stat().st_size<=base.desktop.MAX_LOG,'Guest serial bound')
    lines=log.read_bytes().replace(b'\r\n',b'\n').split(b'\n')[:-1]
    return [json.loads(v[len(b'VEYNODE_GUEST_CRASH '):]) for v in lines if v.startswith(b'VEYNODE_GUEST_CRASH ')]


def crash_proof(records, expected_sha):
    require([v.get('event') for v in records] == ['watcher_ready', 'target_verified_before_signal',
            'sigkill_sent_to_pinned_service'], 'missing, repeated, refused or out-of-order watcher proof')
    identity=records[0].get('data',{})
    require(set(identity)=={'pid','start_ticks','executable_inode','executable_device','executable_sha256'},
            'invalid pinned service identity schema')
    require(all(type(identity[k]) is int and identity[k]>0 for k in
                ('pid','start_ticks','executable_inode','executable_device')) and identity['pid']>1
            and identity['executable_sha256']==expected_sha,'wrong frozen service identity')
    require(all(v.get('data')==identity for v in records),'service identity changed before signal')
    return records[-1]


def main():
    p=argparse.ArgumentParser(description=__doc__)
    for name in ('fixture','binary','worker','state','output'):p.add_argument('--'+name,type=Path,required=True)
    a=p.parse_args()
    for k,v in vars(a).items():setattr(a,k,v.resolve())
    a.output.mkdir(mode=0o700,parents=True,exist_ok=False)
    report={'passed':False,'scope':'actual installed Guest service SIGKILL; host incarnation fail-stop','checks':[],
            'limits':['No Guest scope-empty ACK or graceful shutdown claim','No power-loss filesystem durability guarantee']}
    clients=[];started=False;vm=None
    def save():(a.output/'result.json').write_text(json.dumps(report,indent=2)+'\n')
    def record(name,value):report['checks'].append({'name':name,'evidence':value});save();print(name,flush=True)
    def call(method,payload):
        r=subprocess.run([str(a.binary),'call','--state-dir',str(a.state),method,json.dumps(payload)],capture_output=True,text=True,timeout=42)
        require(r.returncode==0 and len(r.stdout)<=2*1024**2,'local call failed: '+method);return json.loads(r.stdout)
    def tool(client,method,**data):return client.tool(method,{'id':vm,**data})
    def execute(client,argv):
        job=tool(client,'exec_start',argv=argv,timeout_ms=2000)['result']['job_id'];deadline=time.monotonic()+4
        while time.monotonic()<deadline:
            value=tool(client,'exec_poll',job_id=job)['result']
            if value['state']=='finished':
                require('error' not in value,'oracle containment failed');v=value['result']
                require(v['exit_code']==0 and not any(v[k] for k in ('timed_out','cancelled','stdout_truncated','stderr_truncated')),'oracle command failed');return v['stdout']
            time.sleep(.03)
        raise AssertionError('oracle command deadline')
    try:
        prep=json.loads((a.fixture/'preparation.json').read_text());cfgbytes=(a.fixture/'installed.json').read_bytes()
        require(prep['prepared'] and hashlib.sha256(cfgbytes).hexdigest()==prep['config_sha256']['installed.json'],'prepared config changed')
        cfg=json.loads(cfgbytes);vm=cfg['id'];disk=Path(cfg['disk']);base_disk=a.fixture/'images/bases/exec-base/disk.raw'
        require(disk==a.fixture/'images/clones/guest-crash/disk.raw' and cfg.get('qemu_guest_exec') is True,'wrong disposable installed fixture')
        nonce=re.findall(r'(?:^| )veynode.guest_crash_once=([0-9a-f]{64})(?: |$)',cfg['command_line']);require(len(nonce)==1,'explicit once nonce required');nonce=nonce[0]
        require(vm not in {v['config']['id'] for v in call('list',{})},'new installed VM ID required')
        baseline=base.digest(base_disk);require(baseline==prep['imported_sha256'],'immutable import mismatch')
        cfg['serial_log']=str(a.output/'guest.serial.log');log=Path(cfg['serial_log']);ds=disk.stat()
        record('frozen artifacts',{'controller_sha256':base.digest(a.binary),'worker_sha256':base.digest(a.worker),
                'watcher_sha256':prep['watcher_sha256'],'agent_sha256':prep['preserved_guest_sha256']['usr/local/bin/veynode-guest'],'config_sha256':hashlib.sha256(cfgbytes).hexdigest()})
        call('create',cfg)
        for allowed in (False,True):clients.append(base.MCP(a.binary,a.state,allowed))
        life,owner=clients;started=True;tool(life,'start')
        deadline=time.monotonic()+180;boot=None;ready=[]
        while time.monotonic()<deadline:
            status=call('status',{'id':vm});require(status['state'] in ('starting','running'),'Guest stopped before crash readiness')
            es=events(log);require(not any(v['event']=='refused' for v in es),'watcher refused fixture')
            ready=[v for v in es if v['event']=='watcher_ready']
            if log.exists():boot=base.desktop.boot_evidence(log.read_bytes())
            if boot and ready:break
            time.sleep(.2)
        require(boot is not None and len(ready)==1,'actual boot/watcher readiness missing')
        record('actual installed graphics and one-shot watcher ready',{'boot':boot,'watcher':ready[0]})
        runtime=status['runtime'];dp=runtime['disk_provenance'];g=runtime['execution_guardian']
        require(dp.get('device')==ds.st_dev and dp.get('inode')==ds.st_ino and dp.get('descriptor_backed') is True and dp.get('qmp_fdsets_verified_at_start') is True,'disk binding failed')
        require(g['owns_qemu_child'] is True and g['backend_pid']==runtime['backend_pid'],'guardian missing')
        pins=[process.snapshot(pid) for pid in (status['pid'],g['guardian_pid'],g['backend_pid'])]
        require(all(pins) and pins[0]['executable']==str(a.worker) and pins[1]['executable']==str(a.worker)
                and pins[1]['ppid']==pins[0]['pid'] and pins[2]['ppid']==pins[1]['pid'],'original process chain mismatch')
        record('original worker guardian QEMU identities',pins)
        info,_=base.wait_guest_info(owner,vm,lambda:call('status',{'id':vm}),status['pid'])
        require(info['transport']=='virtio-serial-private-exec-v2' and info['execution_containment']=='cgroup-v2-clone3-kill','actual v2 containment required')
        tool(owner,'acquire_control')
        path='/home/veynode/crash-'+nonce[:16]
        job=tool(owner,'exec_start',argv=['/bin/sh','-c',oracle.OUTER,'guest-crash',path,oracle.CHILD],timeout_ms=30000)['result']['job_id']
        require(re.fullmatch('[0-9a-f]{32}',job),'actual start ACK missing')
        time.sleep(.5)
        observations=[]
        for _ in range(2):
            observations.append(oracle.parse_observation(execute(owner,['/bin/sh','-eu','-c',oracle.READ,'observe',path])));time.sleep(.3)
        require(observations[1]['tick_bytes']>observations[0]['tick_bytes'],'real detached descendant not writing')
        record('real command acknowledged and detached child actively writing',{'job_id_sha256':hashlib.sha256(job.encode()).hexdigest(),'observations':observations})
        # No elevated command: UID1000 may only overwrite the root-precreated
        # arm inode. Trigger ACK may be lost to the deliberately killed service.
        begin=time.monotonic()
        arm=owner.rpc('tools/call',{'name':'vm_exec_start','arguments':{'id':vm,'argv':['/bin/sh','-eu','-c',
            'printf "%s\\n" "$1" > /var/lib/veynode-crash-once/arm','arm',nonce],'timeout_ms':2000}})
        record('rootless arm request sent; ACK not used as kill proof',{'rpc_error': 'error' in arm or arm.get('result',{}).get('isError') is True})
        deadline=begin+15;status={};absent_samples=0
        report['retirement_timeline']=[]
        while time.monotonic()<deadline:
            status=call('status',{'id':vm})
            observed=[process.snapshot(v['pid']) for v in pins]
            absent=all(not process.same_incarnation(v,o) for v,o in zip(pins,observed))
            absent_samples=absent_samples+1 if absent else 0
            report['retirement_timeline'].append({'elapsed_seconds':time.monotonic()-begin,
                'state':status['state'],'processes':observed,'absent_samples':absent_samples})
            save()
            if status['state'] in ('stopped','crashed','error','recovery_required') and absent_samples>=2:break
            time.sleep(.05)
        require(absent_samples>=2,'original process absence not observed twice')
        sent=crash_proof(events(log),prep['preserved_guest_sha256']['usr/local/bin/veynode-guest'])
        require(all(not process.same_incarnation(v,process.snapshot(v['pid'])) for v in pins),'original QEMU/guardian/worker still alive')
        require(time.monotonic()-begin<15,'host incarnation fail-stop deadline exceeded')
        record('actual service-only SIGKILL and original host process tree gone',{'watcher_signal':sent,
                'elapsed_seconds':time.monotonic()-begin,'status_state':status['state'],'guest_cleanup_ack':False})
        started=False
        denied=owner.tool('exec_start',{'id':vm,'argv':['/bin/true']},denied=True)
        require('error' in denied or denied.get('result',{}).get('isError') is True,'new execution not refused')
        record('new execution denied after service death',True)
        try:owner.close()
        except AssertionError:pass
        require(owner.p.returncode==1,'original MCP did not explicitly fail cleanup with exit 1')
        record('original MCP EOF explicitly cleanup-unconfirmed',{'exit_code':owner.p.returncode})
        clients.remove(owner);life.close();clients.clear()
        require(base.digest(base_disk)==baseline,'immutable import changed')
        record('immutable import unchanged',baseline)
        report['passed']=True
    except BaseException as e:report['error']=re.sub(r'[0-9a-f]{32,64}','<redacted>',str(e));raise
    finally:
        # Preserve independent observations even if strict serial proof parsing
        # failed. These never promote a failed run to PASS or imply a clean ACK.
        try:
            report['watcher_records_observed']=events(log) if 'log' in locals() else []
            if 'pins' in locals():
                report['process_retirement_observations']=[{'original':v,'observed':process.snapshot(v['pid'])} for v in pins]
        except Exception:
            report['retirement_observation_error']=True
        if started:
            try:
                call('force_stop',{'id':vm,'confirm':vm});deadline=time.monotonic()+15
                while time.monotonic()<deadline:
                    status=call('status',{'id':vm})
                    if status['state'] in ('stopped','crashed','error','recovery_required'):break
                    time.sleep(.1)
                report['failure_cleanup_confirmed']=all(not process.same_incarnation(v,process.snapshot(v['pid'])) for v in pins) if 'pins' in locals() else False
                report['failure_terminal_state']=status['state']
            except Exception:report['failure_cleanup_confirmed']=False
        for client in clients:
            entry={'role':'original_execution_owner' if 'owner' in locals() and client is owner else 'lifecycle_observer'}
            try:
                client.close()
                entry['cleanup_acknowledged']=True
            except Exception as e:
                report['client_cleanup_unconfirmed']=True
                entry.update(cleanup_acknowledged=False,error_type=type(e).__name__)
            entry['exit_code']=client.p.returncode
            report.setdefault('final_client_exits',[]).append(entry)
        save()


if __name__=='__main__':main()
