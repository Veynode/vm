#!/usr/bin/env python3
"""Opted-in actual MCP input -> own-window Guest oracle, never host UI injection.

Requires operator-approved Accessibility, an already running service with input
and private-root files enabled, and a focused stage3 oracle with a fresh nonce.
No VM/process/permission changes. Does not claim Unicode or browser input support.
"""
import argparse
import base64
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import re
import subprocess
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
def load(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
    return module
files = load('input_file_fixture', 'scripts/test-macos-files.py')
image = load('input_image_fixture', 'scripts/test-macos-console-identity.py')


def require(value, code):
    if not value: raise ValueError(code)


class ToolFailure(ValueError):
    def __init__(self, name, detail):
        super().__init__("unexpected_tool_result_"+name); self.detail=detail


class Client(files.MCP):
    def __init__(self, binary, state):
        self.number = 0; self.buffer = bytearray()
        self.process = subprocess.Popen([str(binary), 'mcp', '--state-dir', str(state),
            '--allow-input', '--allow-files'], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        try:
            value = self.rpc('initialize', {'protocolVersion':'2025-06-18', 'capabilities':{},
                'clientInfo':{'name':'macos-input-acceptance','version':'1'}})
            require(value.get('result',{}).get('protocolVersion')=='2025-06-18','initialize_failed')
            self.send({'jsonrpc':'2.0','method':'notifications/initialized'})
        except BaseException:
            self.close(); raise

    def close(self):
        if not self.process.stdin.closed:
            try: self.process.stdin.close()
            except BrokenPipeError: pass
        try: code = self.process.wait(timeout=45)
        except subprocess.TimeoutExpired:
            self.process.kill(); self.process.wait(timeout=2)
            raise ValueError('mcp_cleanup_timeout_not_confirmed') from None
        require(code==0,'mcp_cleanup_exit_nonzero')

    def request(self, name, args, denied=False):
        reply = self.rpc('tools/call', {'name':'vm_'+name,'arguments':args})
        failed = 'error' in reply or reply.get('result',{}).get('isError') is True
        if failed and not denied: raise ToolFailure(name,json.dumps(reply)[:8192])
        require(failed is denied,'unexpected_tool_result_'+name)
        if denied:
            # Keep a bounded error, never arbitrary full tool content or images.
            text = json.dumps(reply, ensure_ascii=True)
            require(len(text)<8192,'oversized_error')
            return text
        return reply['result']

    def value(self, name, args):
        value=json.loads(self.request(name,args)['content'][0]['text'])
        return value['result'] if name=='guest_info' or name.startswith('file_') else value


def validate_snapshot(data, nonce):
    require(0<len(data)<=262144,'oracle_snapshot_size')
    value=json.loads(data)
    require(value.get('schema')==1 and value.get('run_id')==nonce,'oracle_nonce_schema')
    events=value.get('events')
    require(type(events) is list and 1<=len(events)<=513 and
        type(value.get('revision')) is int and value['revision']==len(events),'oracle_revision')
    require(value.get('log_dropped')==0 and value.get('log_write_failed') is False,'oracle_log_failure')
    require(all(type(e) is dict for e in events),'oracle_event_type')
    ready=[e for e in events if e.get('event')=='ready']
    require(len(ready)==1,'oracle_not_ready')
    require(not any(e.get('event')=='observation_gap' for e in events),'oracle_focus_gap')
    active=[e['active_key_window'] for e in events if 'active_key_window' in e]
    require(active and active[-1] is True,'oracle_not_key_window')
    sequences=[e['sequence'] for e in events if 'sequence' in e]
    require(all(type(n) is int and n>0 for n in sequences) and
        all(a<b for a,b in zip(sequences,sequences[1:])),'oracle_sequence')
    geometries=[e['geometry'] for e in events if 'geometry' in e]
    require(all(g==ready[0]['geometry'] for g in geometries),'oracle_geometry_changed')
    return value


def target(geometry, width, height):
    require(geometry.get('available') is True,'oracle_geometry_unavailable')
    display=geometry['display']
    require((display['physical_width'],display['physical_height'])==(width,height),'oracle_capture_geometry_mismatch')
    rectangle=geometry['target_main_display_pixels']
    a,b=rectangle['top_left'],rectangle['bottom_right']
    require(all(type(v) in (float,int) and math.isfinite(v) for p in (a,b) for v in p.values()),'oracle_coordinate_type')
    require(0<=a['x']<b['x']<=width and 0<=a['y']<b['y']<=height,'oracle_target_bounds')
    # Interior positions keep the whole drag in this receiving view.
    return [(int(a['x']+(b['x']-a['x'])*u),int(a['y']+(b['y']-a['y'])*.6)) for u in (.25,.70)]


def delivered(events, kind, point=None, **fields):
    candidates=[e for e in events if e.get('event')==kind and all(e.get(k)==v for k,v in fields.items())]
    if point is None: return bool(candidates)
    # Backend addresses pixel centres; allow <=1 physical pixel for AppKit rounding.
    return any(all(abs(e.get('main_display_pixels',{}).get(axis,-10000)-(pixel+.5))<=1
        for axis,pixel in zip(('x','y'),point)) for e in candidates)


def main():
    p=argparse.ArgumentParser(description=__doc__)
    for name in ('binary','state-dir','output'): p.add_argument('--'+name,type=Path,required=True)
    p.add_argument('--id',required=True); p.add_argument('--run-id',required=True)
    p.add_argument('--oracle-snapshot',required=True,help='Single filename inside existing Guest files root')
    p.add_argument('--wait-ended',type=int,default=150,help='Bounded wait for oracle own timer, 1..900s')
    a=p.parse_args()
    require(re.fullmatch('[0-9a-f]{32}',a.run_id) is not None,'invalid_nonce')
    require(re.fullmatch('[A-Za-z0-9][A-Za-z0-9._-]{0,127}',a.oracle_snapshot) is not None,'invalid_snapshot_name')
    require(1<=a.wait_ended<=900,'invalid_wait')
    fd=os.open(a.output,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o600)
    report={'passed':False,'cleanup_confirmed':False,'run_id':a.run_id,'checks':[],
        'scope':'Actual own-window AppKit delivery through stdio MCP; not Unicode/layout/browser/performance acceptance',
        'synthetic_provenance_authenticated':False,'active_operation_takeover_tested':False}
    client=None; lease=False; error=None
    try:
        report['controller_sha256']=hashlib.sha256(a.binary.read_bytes()).hexdigest()
        client=Client(a.binary.resolve(),a.state_dir.resolve())
        def value(name,**kw): return client.value(name,{'id':a.id,**kw})
        def record(name,evidence=True):
            report['checks'].append({'name':name,'evidence':evidence}); print(name,flush=True)
        def snapshot():
            # Replacement can unlink a retained read inode. Never weaken Guest
            # invariants: cancel that transfer and retry only this read-only snapshot.
            deadline=time.monotonic()+3; attempts=0
            while True:
                token=None
                try:
                    begin=value('file_download_begin',path=a.oracle_snapshot); token=begin['token']
                    require(type(begin['size']) is int and 0<begin['size']<=262144,'snapshot_download_size')
                    data=bytearray()
                    while True:
                        item=value('file_download_read',token=token,offset=len(data))
                        part=base64.b64decode(item['data'],validate=True)
                        require(0<len(part)<=32768,'snapshot_chunk')
                        data.extend(part)
                        require(item['offset']==len(data) and len(data)<=begin['size'],'snapshot_offset')
                        if item['eof']: break
                    token=None
                    require(len(data)==begin['size'] and hashlib.sha256(data).hexdigest()==begin['sha256'],'snapshot_hash')
                    result=validate_snapshot(data,a.run_id)
                    return result
                except ToolFailure as exc:
                    # Only a tool refusal during a mutable snapshot transfer is
                    # retryable; schema/nonce/hash errors remain fatal immediately.
                    if not str(exc).startswith('unexpected_tool_result_file_download') or 'changed' not in exc.detail: raise
                    attempts+=1
                    if attempts>=6 or time.monotonic()>=deadline: raise
                    time.sleep(.1)
                finally:
                    if token:
                        try: value('file_cancel',token=token)
                        except ToolFailure as exc:
                            if 'not_found' not in exc.detail: raise
        def shot():
            reply=client.request('screenshot',{'id':a.id})
            require(len(reply['content'])==2,'screenshot_shape')
            data=base64.b64decode(reply['content'][0]['data'],validate=True)
            meta=json.loads(reply['content'][1]['text'])
            require(reply['content'][0]['mimeType']=='image/jpeg','screenshot_format')
            require(image.jpeg_dimensions(data)==(meta['width'],meta['height']),'screenshot_jpeg_geometry')
            require(meta['input_anchor_available'] is True and meta['coordinate_space']=='guest-display-physical-pixels','screenshot_anchor')
            require(meta['width']==meta['source_width'] and meta['height']==meta['source_height'],'screenshot_physical_size')
            return {k:meta[k] for k in ('frame_id','source_generation','width','height')}
        info=value('guest_info')
        require(info['os']=='darwin' and info['machine'].startswith('VirtualMac') and info['uid']!=0,'not_guest')
        require(info['input_policy']['enabled'] is True and info['input_policy']['available'] is True,'input_not_authorized_or_faulted')
        require(info['capabilities']['files'] is True,'private_files_required')
        baseline=snapshot(); require(not any(e.get('event')=='ended' for e in baseline['events']),'oracle_already_ended')
        anchor=shot(); geometry=next(e['geometry'] for e in baseline['events'] if e.get('event')=='ready')
        left,right=target(geometry,anchor['width'],anchor['height'])
        report['target_pixels']={'click':left,'drag_end':right,'width':anchor['width'],'height':anchor['height']}
        report['initial_revision']=baseline['revision']
        acquired=value('acquire_control'); require(type(acquired.get('epoch')) is int and acquired['epoch']>0,'lease_epoch'); lease=True; record('permission_geometry_nonce_and_lease')
        def action(name,payload,predicate):
            before=snapshot(); frame=shot()
            require((frame['width'],frame['height'])==(anchor['width'],anchor['height']),'capture_geometry_changed')
            dispatch=value('input_'+name,**frame,**payload)
            require(dispatch.get('dispatched') is True and dispatch.get('guest_delivery_confirmed') is False,'dispatch_contract')
            deadline=time.monotonic()+4
            while True:
                after=snapshot(); fresh=after['events'][len(before['events']):]
                if predicate(fresh): break
                require(time.monotonic()<deadline,'oracle_delivery_missing_'+name); time.sleep(.1)
            record('actual_'+name,{'dispatch_ack_only':True,'from_revision':before['revision'],'to_revision':after['revision'],
                'events':[{k:v for k,v in e.items() if k not in ('source_pid','source_state_id','source_user_data','provenance')} for e in fresh]})
        action('click',{'x':left[0],'y':left[1]},lambda es: delivered(es,'mouse_down',left,button=0) and delivered(es,'mouse_up',left,button=0))
        action('key',{'keys':['a']},lambda es: delivered(es,'key_down',keycode=0) and delivered(es,'key_up',keycode=0))
        action('key',{'keys':['ctrl','a']},lambda es: any(e.get('event')=='key_down' and e.get('keycode')==0 and e.get('modifier_flags',0)&(1<<18) for e in es) and delivered(es,'key_up',keycode=0) and any(e.get('event')=='flags_changed' and e.get('keycode')==59 and not(e.get('modifier_flags',0)&(1<<18)) for e in es))
        action('drag',{'x':left[0],'y':left[1],'end_x':right[0],'end_y':right[1]},lambda es: delivered(es,'mouse_down',left,button=0) and delivered(es,'drag',right) and delivered(es,'mouse_up',right,button=0))
        action('scroll',{'x':right[0],'y':right[1],'direction':'down','ticks':2},lambda es: any(e.get('event')=='scroll' and e.get('scrolling_delta_y',0)<0 for e in es))
        def denial(name,frame):
            before=snapshot(); message=client.request('input_key',{'id':a.id,**frame,'keys':['a']},denied=True)
            time.sleep(.3); after=snapshot()
            require(after['revision']==before['revision'],'rejected_input_observed_events')
            record(name,{'error':message,'unchanged_revision':after['revision']})
        expired=shot(); time.sleep(5.3); denial('stale_frame_no_delivery',expired)
        released=value('release_control'); require(released.get('released') is True,'release_unconfirmed'); lease=False
        denial('released_lease_no_delivery',shot())
        deadline=time.monotonic()+a.wait_ended
        while True:
            final=snapshot(); ended=[e for e in final['events'] if e.get('event')=='ended']
            if ended: break
            require(time.monotonic()<deadline,'oracle_end_not_observed'); time.sleep(.5)
        require(len(ended)==1,'oracle_terminal_count'); end=ended[0]; summary=end['summary']
        require(summary['observed_pressed_keycodes']==[] and summary['observed_pressed_buttons']==[],'oracle_held_states')
        require(summary['observation_gap'] is False and summary['tracking_overflow'] is False and
                end['log_dropped']==0 and end['log_write_failed'] is False,'oracle_terminal_incomplete')
        require(summary['modifier_flags'] & ((1<<17)|(1<<18)|(1<<19)|(1<<20))==0,'oracle_modifiers_held')
        record('actual_oracle_terminal_no_held_input',summary)
        report['delivery_checks_passed']=True
    except BaseException as exc:
        error=exc; report['error']=str(exc) if isinstance(exc,ValueError) else type(exc).__name__
    finally:
        if client:
            try:
                if lease: client.value('release_control',{'id':a.id})
                client.close(); report['cleanup_confirmed']=True
            except BaseException as exc:
                report['cleanup_error']=str(exc) if isinstance(exc,ValueError) else type(exc).__name__
                error=error or exc
                # Even a failed release must close/reap the scoped adapter.
                try: client.close()
                except BaseException: pass
        report['passed']=error is None and report.get('delivery_checks_passed') is True and report['cleanup_confirmed']
        with os.fdopen(fd,'w') as out: json.dump(report,out,indent=2); out.write('\n')
    if error: raise SystemExit('FAIL: '+report.get('error',report.get('cleanup_error','unknown')))
    print('PASS: actual Guest delivery and rejected-input absence; cleanup confirmed')


class PureTests(unittest.TestCase):
    def test_nonce_revision_and_event_gaps(self):
        nonce='a'*32
        v={'schema':1,'run_id':nonce,'revision':1,'events':[{'event':'ready','active_key_window':True,'geometry':{}}], 'log_dropped':0,'log_write_failed':False}
        self.assertEqual(validate_snapshot(json.dumps(v).encode(),nonce),v)
        for field,bad in [('run_id','b'*32),('revision',2),('log_dropped',1),('log_write_failed',True)]:
            altered={**v,field:bad}
            with self.assertRaises(ValueError):validate_snapshot(json.dumps(altered).encode(),nonce)
        v['events'].append({'event':'observation_gap','active_key_window':False});v['revision']=2
        with self.assertRaises(ValueError):validate_snapshot(json.dumps(v).encode(),nonce)
    def test_controller_and_guest_envelopes_are_distinct(self):
        client=object.__new__(Client)
        client.request=lambda name,args: {'content':[{'text':json.dumps({'epoch':7})}]}
        self.assertEqual(client.value('acquire_control',{}),{'epoch':7})
        client.request=lambda name,args: {'content':[{'text':json.dumps({'result':{'token':'a'*32}})}]}
        self.assertEqual(client.value('file_download_begin',{}),{'token':'a'*32})
    def test_geometry_uses_actual_physical_bounds(self):
        g={'available':True,'display':{'physical_width':2560,'physical_height':1440},'target_main_display_pixels':{'top_left':{'x':100,'y':300},'bottom_right':{'x':1540,'y':800}}}
        self.assertEqual(target(g,2560,1440),[(460,600),(1108,600)])
        with self.assertRaises(ValueError):target(g,1280,720)
    def test_dispatch_is_not_delivery_wrong_coordinate_key_rejected(self):
        self.assertFalse(delivered([{'dispatched':True}],'mouse_up',(100,100)))
        event={'event':'mouse_up','button':0,'main_display_pixels':{'x':100.5,'y':100.5}}
        self.assertTrue(delivered([event],'mouse_up',(100,100),button=0))
        self.assertFalse(delivered([event],'mouse_up',(104,100),button=0))
        self.assertFalse(delivered([{'event':'key_up','keycode':1}],'key_up',keycode=0))

if __name__=='__main__':
    import sys
    if sys.argv[1:]==['--self-test']:unittest.main(argv=[sys.argv[0]])
    else:main()
