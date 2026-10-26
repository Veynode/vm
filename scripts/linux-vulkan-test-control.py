#!/usr/bin/env python3
"""Finite actual controller→worker RAM Venus acceptance; attach to explicit daemon.
No daemon spawn, no existing disks, no native Vulkan presentation claim.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess
import time
ROOT=Path(__file__).resolve().parents[1]
def load(name,path):
 s=importlib.util.spec_from_file_location(name,path);m=importlib.util.module_from_spec(s);s.loader.exec_module(m);return m
BASE=load('installed_mcp',ROOT/'scripts/test-installed-qemu-desktop.py')
APP=load('vkmark_checker',ROOT/'scripts/linux-vulkan-application-probe.py')

def graphics_proof(status):
 r=status.get('runtime') or {};p=r.get('graphics_profile',{});h=p.get('host_admission',{})
 BASE.require(p.get('configured')=='venus-moltenvk-experimental' and p.get('host_supported') is True and p.get('guest_verified') is False,'runtime profile proof missing')
 BASE.require(p.get('runtime_revision')=='owned-group-v1' and p.get('qmp_gpu_properties_verified') is True and p.get('ipa_granule_requested_bytes')==4096 and p.get('hostmem_aperture_bytes')==268435456 and p.get('renderer_ownership')=='inherits-worker-process-group','pre-cont GPU proof mismatch')
 BASE.require(h.get('architecture')=='aarch64' and h.get('public_hv_config_4k_readback') is True and h.get('vm_created_by_probe') is False,'public host API proof missing')
 return p

def workload(serial):
 text=serial.decode(errors='replace').replace('\r\n','\n')
 BASE.require('VULKAN_SETUP_FAIL' not in text,'Guest setup failed')
 if '\nVEYNODE_CONTROL_VENUS_READY\n' not in text:return None
 pixels=APP.base.classify_pixels(text,APP.base.classify(text,0));apps=APP.classify(text,0)
 BASE.require(pixels['raster_pixel_oracle_passed'],'pixel oracle failed')
 BASE.require(apps['workload_completed'],'vkmark workload failed')
 BASE.require(text.splitlines().count('VEYNODE_CONTROL_VENUS_READY')==1,'duplicate readiness')
 return {'pixels':pixels,'vkmark':apps,'scope':'GPU content gate; normal process exit verified separately'}

def guest_observer(info):
 BASE.require(info.get('transport')=='virtio-serial-private-observer-v1' and info.get('transport_reconnect') is False,'observer transport mismatch')
 BASE.require(info.get('os')=='linux' and info.get('arch')=='arm64','Guest platform mismatch')
 BASE.require(all(info.get('capabilities',{}).get(k) is False for k in ['exec','async_exec','files','shutdown','desktop','descendant_containment']),'observer scope exceeded')

def read_serial(path):
 with path.open('rb') as f:data=f.read(8*1024*1024+1)
 BASE.require(len(data)<=8*1024*1024,'serial bound');return data

def main():
 p=argparse.ArgumentParser(description=__doc__)
 for key in ['binary','state-dir','fixture-dir','output-dir']:p.add_argument('--'+key,type=Path,required=True)
 a=p.parse_args();os.umask(0o077)
 for key in vars(a):setattr(a,key,getattr(a,key).resolve())
 a.output_dir.mkdir(mode=0o700,parents=True,exist_ok=False)
 report={'schema':1,'passed':False,'checks':[],'limits':['diskless headless Vulkan, no desktop/native presentation acceptance','vkmark printed rate is not a controlled performance comparison','no new descendant wait-reap proof; prior independent runtime lifecycle evidence is separate']}
 client=None;registered=False;vm=None
 def save(): (a.output_dir/'result.json').write_text(json.dumps(report,indent=2)+'\n')
 def record(name,value):report['checks'].append({'name':name,'value':value});save();print(name,flush=True)
 def call(method,params):
  r=subprocess.run([str(a.binary),'call','--state-dir',str(a.state_dir),method,json.dumps(params)],capture_output=True,text=True,timeout=42)
  BASE.require(r.returncode==0,'controller '+method+' failed');return json.loads(r.stdout)
 def terminal(seconds):
  end=time.monotonic()+seconds;last=None
  while time.monotonic()<end:
   last=call('status',{'id':vm})
   with (a.output_dir/'status.jsonl').open('a') as f:f.write(json.dumps({'at':time.time(),'status':last})+'\n')
   if last.get('state') in ['stopped','crashed','error']:return last
   time.sleep(.25)
  raise TimeoutError('VM retirement deadline')
 try:
  fixture=json.loads((a.fixture_dir/'fixture.json').read_text());config=json.loads((a.fixture_dir/'config.json').read_text());vm=config['id']
  for item in fixture['inputs']:BASE.require(BASE.digest(Path(item['path']))==item['sha256'],'fixture input changed')
  BASE.require(config.get('backend')=='qemu-hvf' and config.get('guest')=='linux' and config.get('qemu_graphics_profile')=='venus-moltenvk-experimental' and config.get('qemu_guest_exec',False) is False and not config.get('disk') and config.get('window') is False,'wrong fixture profile')
  BASE.require(not Path(config['serial_log']).exists(),'fixture was already run')
  BASE.require(call('list',{})==[],'requires dedicated empty controller state')
  record('pinned inputs',{'fixture_sha256':BASE.digest(a.fixture_dir/'fixture.json'),'binary_sha256':BASE.digest(a.binary),'config_sha256':BASE.digest(a.fixture_dir/'config.json')})
  # Retain object before initialize so constructor failures still own/reap MCP.
  client=BASE.MCP.__new__(BASE.MCP);client.__init__(a.binary,a.state_dir)
  record('MCP create',client.tool('create',config));registered=True
  record('start',client.tool('start',{'id':vm}))
  end=time.monotonic()+170;checked=None;proof=None;info=None
  while time.monotonic()<end:
   status=call('status',{'id':vm})
   if status.get('state') in ['stopped','crashed','error']:record('early terminal',status);raise AssertionError('VM ended before ready')
   if (status.get('runtime') or {}).get('graphics_profile'):proof=graphics_proof(status)
   serial=Path(config['serial_log'])
   if serial.exists():checked=workload(read_serial(serial))
   if checked and proof:break
   time.sleep(.25)
  BASE.require(checked is not None and proof is not None,'graphics readiness deadline')
  record('actual host admission',proof);record('actual GPU workloads',checked)
  info=client.tool('guest_info',{'id':vm})['result'];guest_observer(info);record('actual observer info',info)
  for name,payload in [('exec_start',{'argv':['/bin/true'],'timeout_ms':1000}),('file_upload_begin',{'path':'must-not-exist','size':0,'sha256':'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'})]:
   denial=client.tool(name,{'id':vm,**payload},denied=True);BASE.require('unsupported' in json.dumps(denial).lower(),'backend capability denial missing');record(name+' denied',denial)
  shutdown=client.tool('shutdown',{'id':vm});record('shutdown request',shutdown)
  BASE.require(shutdown.get('requested') is True and shutdown.get('completed') is False and shutdown.get('mechanism')=='qmp-system-powerdown','wrong shutdown mechanism')
  status=terminal(45);record('terminal before assertion',status)
  BASE.require(status.get('state')=='stopped' and status.get('exit_code')==0,'normal exit not confirmed')
  text=read_serial(Path(config['serial_log']));BASE.require(b'VEYNODE_VENUS_ACPI_BUTTON' in text and b'VEYNODE_VENUS_NORMAL_SHUTDOWN' in text,'actual ACPI/init shutdown markers absent')
  record('serial hash',{'sha256':BASE.digest(Path(config['serial_log'])),'bytes':len(text)})
  client.close();record('MCP cleanup',{'exit_code':client.p.returncode});client=None
  report['passed']=True
 except Exception as e:report['error']=type(e).__name__+':'+str(e)[:500]
 finally:
  if registered:
   try:
    status=call('status',{'id':vm});record('cleanup initial status',status)
    if status.get('state') not in ['stopped','crashed','error']:
     record('failure force request',call('force_stop',{'id':vm,'confirm':vm}));status=terminal(15);record('failure terminal',status)
    BASE.require(status.get('state') in ['stopped','crashed'] and status.get('exit_code') is not None,'owned worker retirement unconfirmed')
   except Exception as e:report['cleanup_unconfirmed']=type(e).__name__;report['passed']=False
  if client is not None and hasattr(client,'p'):
   try:client.close();record('MCP final cleanup',{'exit_code':client.p.returncode})
   except Exception as e:
    report['mcp_cleanup_unconfirmed']=type(e).__name__;report['passed']=False
    if client.p.poll() is None:
     client.p.kill();client.p.wait(timeout=3)
    report['mcp_fallback_exit']=client.p.returncode
  save()
 print(json.dumps({'passed':report['passed'],'output':str(a.output_dir)}));return 0 if report['passed'] else 1
if __name__=='__main__':raise SystemExit(main())
