#!/usr/bin/env python3
"""Read-only actual MCP info vs public-Metal probe snapshot; no image/GPU/VM actions."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('macos_files_fixture', ROOT/'scripts/test-macos-files.py')
fixture = importlib.util.module_from_spec(spec); spec.loader.exec_module(fixture)
FEATURES = ['supports_'+name+'_'+stage for name in ('function_pointers','dynamic_libraries','raytracing') for stage in ('compute','render')]
ASSESSMENT = {'shader_rendering':'not-tested-by-this-query','performance':'not-measured',
              'per_kernel_limits':'unavailable-without-pipeline'}


def require(value, code):
    if not value: raise ValueError(code)


def integer(value, minimum=0, maximum=2**63-1):
    return type(value) is int and minimum <= value <= maximum


def text(value, limit=1024):
    return isinstance(value,str) and 0 < len(value.encode()) <= limit and '\0' not in value


def validate_device(device):
    require(type(device) is dict, 'device_not_object')
    required = {'schema','name','architecture_name','registry_id','os_version','family_support','unified_memory',
                'max_threads_per_threadgroup_axes','max_threadgroup_memory_bytes','max_buffer_bytes',
                'max_argument_buffer_sampler_count','argument_buffers_support','read_write_texture_support',
                'per_kernel_limits','limit_semantics','interpretation',*FEATURES}
    require(set(device)==required, 'device_fields_changed')
    require(type(device['schema']) is int and device['schema']==1, 'device_schema')
    for key in ('name','architecture_name','os_version','limit_semantics','interpretation'):
        require(text(device[key]), 'device_text')
    registry=device['registry_id']
    require(isinstance(registry,str) and re.fullmatch(r'0|[1-9][0-9]{0,19}',registry) and int(registry)<=2**64-1, 'registry_format')
    require(type(device['unified_memory']) is bool and all(type(device[k]) is bool for k in FEATURES), 'feature_bool')
    family=device['family_support']
    require(type(family) is dict and len(family)<=64 and all(isinstance(k,str) and re.fullmatch(r'(apple|mac|metal|common)[1-9][0-9]{0,2}',k) and type(v) is bool for k,v in family.items()), 'family_format')
    axes=device['max_threads_per_threadgroup_axes']
    require(type(axes) is dict and set(axes)=={'width','height','depth'} and all(integer(v,1) for v in axes.values()), 'thread_axes')
    for key in ('max_threadgroup_memory_bytes','max_buffer_bytes','max_argument_buffer_sampler_count'):
        require(integer(device[key]), 'device_limit_integer')
    for key, known in [('argument_buffers_support',{0:'tier1',1:'tier2'}),('read_write_texture_support',{0:'none',1:'tier1',2:'tier2'})]:
        value=device[key]
        require(type(value) is dict and set(value)=={'raw_value','tier'} and integer(value['raw_value']), 'tier_format')
        require(value['tier']==known.get(value['raw_value'],'unknown'), 'tier_name_mismatch')
    require(device['per_kernel_limits'] is None, 'kernel_limits_inferred')
    require('not multiplied into a total' in device['limit_semantics'] and 'not proof' in device['interpretation'], 'capability_semantics_missing')
    return device


def validate_info(info):
    require(type(info) is dict and info.get('os')=='darwin' and isinstance(info.get('machine'),str) and info['machine'].startswith('VirtualMac'), 'not_macos_guest')
    graphics=info.get('graphics')
    require(type(graphics) is dict, 'graphics_missing')
    base={'schema','api','status','device_selection','evidence','queried_at_unix_ms','device','assessment','scope'}
    require(set(graphics) in (base,base|{'reason'}), 'graphics_fields_changed')
    require(type(graphics['schema']) is int and graphics['schema']==1 and graphics['api']=='metal', 'graphics_schema')
    require(graphics['device_selection']=='system-default' and graphics['evidence']=='public-device-properties', 'graphics_evidence_basis')
    require(integer(graphics['queried_at_unix_ms']) and text(graphics['scope']), 'graphics_metadata')
    require(graphics['assessment']==ASSESSMENT, 'graphics_unverified_claim')
    if graphics['status']=='unavailable':
        require(graphics.get('reason')=='no_metal_device' and graphics['device'] is None and info.get('metal_device')=='none', 'no_device_inconsistent')
    else:
        require(graphics['status']=='reported' and 'reason' not in graphics, 'graphics_status')
        device=validate_device(graphics['device'])
        require(info.get('metal_device')==device['name'] and info.get('os_release')==device['os_version'], 'legacy_device_or_os_mismatch')
    # Only these reviewed public-property fields leave this function. File roots,
    # username, console identities and any unexpected info body are not archived.
    return {'metal_device':info['metal_device'],'graphics':graphics}


def load_probe(path):
    fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
    with os.fdopen(fd,'rb') as source:
        st=os.fstat(source.fileno());require(stat.S_ISREG(st.st_mode) and 0<st.st_size<=65536,'probe_file_bound')
        raw=source.read(65537);require(len(raw)<=65536,'probe_file_bound')
    rows=[json.loads(line) for line in raw.splitlines()]
    require(1<=len(rows)<=8,'probe_events_bound')
    completed=[row for row in rows if row.get('event')=='completed']
    require(len(completed)==1 and rows[-1] is completed[0], 'probe_not_completed')
    row=completed[0]
    require(row.get('mode')=='capabilities' and row.get('capabilities_read') is True and
            all(row.get(k) is False for k in ('pipeline_created','library_created','gpu_dispatch','render_passed')), 'not_capability_probe')
    device=dict(row['capabilities'])
    if 'host_os' in device:
        require('os_version' not in device,'ambiguous_probe_os')
        device['os_version']=device.pop('host_os')
    validate_device(device)
    return device,hashlib.sha256(raw).hexdigest()


def compare_probe(observation, probe):
    require(observation['graphics']['status']=='reported','probe_compare_requires_device')
    device=observation['graphics']['device']
    stable=set(device)-{'registry_id'}
    differences=sorted(key for key in stable if device[key]!=probe[key])
    return {'properties_equal':not differences,'different_property_names':differences,
            'registry_ids':{'info':device['registry_id'],'probe':probe['registry_id'],
                            'equal':device['registry_id']==probe['registry_id']},
            'info_queried_at_unix_ms':observation['graphics']['queried_at_unix_ms'],
            'comparison_scope':'Public properties; registry IDs may change across reboot/device incarnation. Probe has no query timestamp. This comparison does not authenticate console identity or prove rendering.'}


class Observer(fixture.MCP):
    def __init__(self,*args):
        try: super().__init__(*args)
        except Exception:
            self.close()
            raise

    def close(self):
        process=getattr(self,'process',None)
        if process is None:return
        if not process.stdin.closed:
            try:process.stdin.close()
            except BrokenPipeError:pass
        try:code=process.wait(timeout=45)
        except subprocess.TimeoutExpired:
            process.kill();process.wait(timeout=2)
            raise ValueError('mcp_cleanup_timeout') from None
        require(code==0,'mcp_cleanup_nonzero')


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('binary','state-dir','probe','output'):parser.add_argument('--'+name,type=Path,required=True)
    parser.add_argument('--id',required=True)
    args=parser.parse_args()
    fd=os.open(args.output,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o600)
    report={'schema':1,'passed':False,'actual_mcp_query':False,'cleanup_confirmed':False,
            'started_at_unix':time.time(),'scope':'Guest public graphics properties only; no image capture, GPU work, input, file mutation or VM lifecycle action'}
    client=None
    try:
        probe,digest=load_probe(args.probe)
        report['probe_log_sha256']=digest
        report['controller_sha256']=hashlib.sha256(args.binary.read_bytes()).hexdigest()
        client=Observer(args.binary.resolve(),args.state_dir.resolve())
        info=client.tool('guest_info',{'id':args.id})
        report['actual_mcp_query']=True
        report['observation']=validate_info(info)
        report['comparison']=compare_probe(report['observation'],probe)
        require(report['comparison']['properties_equal'],'probe_properties_mismatch')
        report['passed']=True
    except Exception as error:
        message=str(error)
        report['error']=message if isinstance(error,ValueError) and re.fullmatch(r'[a-z_]{1,80}',message) else type(error).__name__
    finally:
        try:
            if client is not None:client.close();report['cleanup_confirmed']=True
        except Exception:report['passed']=False;report['cleanup_error']='mcp_cleanup_unconfirmed'
        report['passed']=report['passed'] and report['cleanup_confirmed']
        with os.fdopen(fd,'w') as output:json.dump(report,output,indent=2);output.write('\n')
    print(json.dumps({'passed':report['passed'],'cleanup_confirmed':report['cleanup_confirmed']}))
    return 0 if report['passed'] else 1


if __name__=='__main__':raise SystemExit(main())
