#!/usr/bin/env python3
"""Summarize measured JSONL; no fake baseline or winner inference.
Usage: summarize-benchmark.py RUN_DIR [COMPARATOR_DIR]
"""
import json, math, pathlib, statistics, sys
MATCH_FIELDS=('host','guest','application','resources','display','measurement','scenario','background','network','sampling')
def numeric(value,label,minimum=0,integer=False):
    if not isinstance(value,(int,float)) or isinstance(value,bool) or not math.isfinite(value) or value<minimum or (integer and not isinstance(value,int)):
        raise ValueError('invalid numeric measurement: '+label)
def text(value,label):
    if not isinstance(value,str) or not value.strip():raise ValueError('missing nonempty metadata: '+label)
def validate_manifest(m):
    for key in (*MATCH_FIELDS,'implementation'):
        if key not in m:raise ValueError('Missing required manifest field: '+key)
    required_text={
        'implementation':('name','version','build'),
        'host':('model','chip','os_build','power_mode','power_source','thermal_state'),
        'guest':('os','build'),
        'application':('name','version','workload','architecture'),
        'measurement':('tool','version','frame_method','input_method','resource_method'),
    }
    for section,fields in required_text.items():
        if not isinstance(m[section],dict):raise ValueError('expected metadata object: '+section)
        for field in fields:text(m[section].get(field),section+'.'+field)
    if m['application']['architecture'] not in ('arm64','x86-translated','x86_64-translated'):
        raise ValueError('application architecture must distinguish ARM native, x86 and x64 translation')
    for section,fields in {'host':('ram_bytes',),'resources':('vcpus','memory_bytes'),'display':('width','height')}.items():
        if not isinstance(m[section],dict):raise ValueError('expected metadata object: '+section)
        for field in fields:numeric(m[section].get(field),section+'.'+field,1,True)
    for field in ('refresh_hz','scale'):numeric(m['display'].get(field),'display.'+field,0.01)
    scope=m['measurement'].get('process_scope')
    if not isinstance(scope,list):raise ValueError('measurement.process_scope must list included process roles')
    for role in scope:text(role,'measurement.process_scope')
    if not isinstance(m['sampling'],dict):raise ValueError('sampling must declare warmup and duration')
    numeric(m['sampling'].get('warmup_seconds'),'sampling.warmup_seconds')
    numeric(m['sampling'].get('duration_seconds'),'sampling.duration_seconds',0.001)
    if not isinstance(m['background'],list):raise ValueError('background must explicitly list other VM workloads; [] means none')
    for vm in m['background']:
        if not isinstance(vm,dict):raise ValueError('invalid background VM')
        for key in ('os_build','workload'):text(vm.get(key),'background.'+key)
        for key in ('vcpus','memory_bytes'):numeric(vm.get(key),'background.'+key,1,True)
    if m['scenario'] not in ('local','remote'):raise ValueError('scenario must be local or remote')
    network=m['network']
    if not isinstance(network,dict):raise ValueError('network must be an object')
    if m['scenario']=='local':
        if network.get('path')!='local':raise ValueError('local run must declare network.path=local')
    else:
        if network.get('path') not in ('direct','forced-turn'):raise ValueError('Remote run must distinguish direct from forced-turn')
        for field in ('client_hardware','browser_version','codec','rate_policy','network_shaping','turn_provider','turn_region'):
            text(network.get(field),'network.'+field)
        for field in ('rtt_ms','jitter_ms','loss_percent'):numeric(network.get(field),'network.'+field)
        numeric(network.get('bandwidth_mbps'),'network.bandwidth_mbps',0.001)
        if network['loss_percent']>100:raise ValueError('network.loss_percent must be <=100')
        pair=network.get('selected_candidate_pair')
        if not isinstance(pair,dict):raise ValueError('remote run needs actual selected ICE candidate pair')
        for endpoint in ('local_type','remote_type'):
            if pair.get(endpoint) not in ('host','srflx','prflx','relay'):raise ValueError('invalid ICE candidate type')
        relay='relay' in pair.values()
        if relay != (network['path']=='forced-turn'):raise ValueError('selected ICE candidate pair contradicts declared direct/forced-turn path')
def percentile(values,p):
    a=sorted(values)
    if not a:return None
    index=(len(a)-1)*p;lo=math.floor(index);hi=math.ceil(index)
    return a[lo]+(a[hi]-a[lo])*(index-lo)
def stats(values):
    return {'samples':len(values),'p50':percentile(values,.5),'p95':percentile(values,.95),'p99':percentile(values,.99),'max':max(values) if values else None}
def summarize(folder):
    folder=pathlib.Path(folder);manifest=json.loads((folder/'manifest.json').read_text())
    validate_manifest(manifest)
    hz=manifest['display']['refresh_hz']
    times=[];inputs=[];footprints=[];cpus=[]
    for line in (folder/'samples.jsonl').read_text().splitlines():
        row=json.loads(line);kind=row['kind']
        if kind not in ('frame','input','resource'):raise ValueError('unknown measurement kind: '+str(kind))
        if manifest['measurement'][kind+'_method']=='not-measured':raise ValueError('samples contradict not-measured declaration: '+kind)
        fields={'frame':['present_time_ns'],'input':['latency_ms'],'resource':['host_cpu_percent','host_physical_footprint_bytes','guest_assigned_bytes','timestamp_unix_ns']}[kind]
        for field in fields:
            value=row[field]
            numeric(value,field)
        if kind=='frame':
            t=row['present_time_ns']
            if times and t<=times[-1]:raise ValueError('frame timestamps must be strictly increasing')
            times.append(t)
        elif kind=='input':inputs.append(row['latency_ms'])
        else:
            if not manifest['measurement']['process_scope']:raise ValueError('resource measurements require declared process scope')
            assigned=manifest['resources']['memory_bytes']+sum(vm['memory_bytes'] for vm in manifest['background'])
            if row['guest_assigned_bytes'] != assigned:raise ValueError('resource assigned memory contradicts manifest including background VMs')
            footprints.append(row['host_physical_footprint_bytes']);cpus.append(row['host_cpu_percent'])
    intervals=[(b-a)/1e6 for a,b in zip(times,times[1:])]
    frame=stats(intervals);frame['stalls_over_2_refresh_periods']=sum(i>2000/hz for i in intervals)
    return {'manifest':manifest,'frame_interval_ms':frame,'input_latency_ms':stats(inputs),'host_physical_footprint_bytes':stats(footprints),'host_cpu_percent':stats(cpus),'limitations':['Single run descriptive statistics; no statistical superiority inference.','Missing sample categories remain empty; measurements and matching metadata must be independently audited.']}
def compare(a,b):
    validate_manifest(a['manifest']);validate_manifest(b['manifest'])
    mismatches=[key for key in MATCH_FIELDS if a['manifest'][key]!=b['manifest'][key]]
    if mismatches:raise ValueError('Non-comparable configuration fields: '+', '.join(mismatches))
    return {'a':a,'b':b,'configuration_matches':True,'winner':None,'note':'Matched declarations alone do not validate measurement methods or establish superiority.'}
if __name__=='__main__':
    if len(sys.argv) not in (2,3):raise SystemExit(__doc__)
    result=summarize(sys.argv[1])
    if len(sys.argv)==3:result=compare(result,summarize(sys.argv[2]))
    print(json.dumps(result,indent=2,allow_nan=False))
