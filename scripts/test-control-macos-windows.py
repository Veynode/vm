#!/usr/bin/env python3
"""Attach-only real macOS window observation through observe-only stdio MCP.

Never starts/stops a VM, executes Guest commands, changes permissions or grants
input. Writes fresh local evidence only. Requires an already-visible Terminal.
"""
import argparse
import base64
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import time

spec = importlib.util.spec_from_file_location('bounded_collection', Path(__file__).with_name('collect-godot-macos.py'))
common = importlib.util.module_from_spec(spec)
spec.loader.exec_module(common)
require = common.require


class Observer(common.BoundedMCP):
    def __init__(self, binary, state, deadline):
        self.deadline = deadline
        self.rpc_deadline = min(deadline, time.monotonic()+12)
        common.shared.MCP.__init__(self, binary, state, files=False)
        os.set_blocking(self.process.stdin.fileno(), False)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary', type=Path, required=True)
    p.add_argument('--state-dir', type=Path, required=True)
    p.add_argument('--id', required=True)
    p.add_argument('--output-dir', type=Path, required=True)
    a = p.parse_args()
    os.umask(0o077)
    a.output_dir.mkdir(mode=0o700, parents=True, exist_ok=False)
    report = {'schema':1, 'passed':False, 'started_unix':time.time(), 'checks':[],
              'controller_sha256':hashlib.sha256(a.binary.read_bytes()).hexdigest(),
              'scope':'Actual observe-only windows and screenshot, not AX, input, atomic display or multiscreen acceptance.'}
    c = Observer.__new__(Observer)
    phase = 'initialize'
    def save(name, value):
        with (a.output_dir/name).open('xb') as f:
            f.write(value if isinstance(value, bytes) else (json.dumps(value, indent=2)+'\n').encode())
            f.flush(); os.fsync(f.fileno())
    def check(name):
        report['checks'].append(name); print(name, flush=True)
    def call(name, args, denied=False):
        reply = c.rpc('tools/call', {'name':'vm_'+name, 'arguments':args})
        result = reply.get('result', {})
        failure = 'error' in reply or result.get('isError') is True
        require(failure == denied, 'unexpected MCP success/error')
        return result
    def text(result):
        require(result.get('isError') is False and result['content'][0]['type']=='text', 'MCP text result')
        return common.strict_json(result['content'][0]['text'].encode())
    try:
        Observer.__init__(c, a.binary.resolve(), a.state_dir.resolve(), time.monotonic()+90)
        phase = 'tool_discovery'
        listing = c.rpc('tools/list', {})['result']
        names = [x['name'] for x in listing['tools']]
        require('vm_windows' in names and not any(n in names for n in ('vm_start','vm_exec','vm_input_click','vm_file_upload_begin')), 'observe scope')
        save('tools.json', listing); check('observe_only_tool_discovery')
        phase = 'guest_info'
        info = text(call('guest_info', {'id':a.id}))['result']
        save('guest-info.json', info)
        require(info['os']=='darwin' and info['machine'].startswith('VirtualMac') and info['capabilities']['windows'] is True, 'real Guest windows capability')
        require(info['capabilities']['async_exec'] is False, 'legacy execution remains disabled')
        check('actual_guest_capability')
        snapshots = []
        for i in range(2):
            phase = f'window_snapshot_{i}'
            value = text(call('windows', {'id':a.id}))
            save(f'windows-{i}.json', value)
            require(value['version']==1 and value['coordinate_space']=='guest-desktop-global-points', 'window coordinates')
            require(value['titles_included'] is False and value['accessibility_tree'] is False and value['input_authorization'] is False, 'window boundary')
            require(value['windows'] and value['truncated'] is False, 'complete actual window list')
            require(any(w['owner']['bundle_identifier']=='com.apple.Terminal' for w in value['windows']), 'actual Terminal missing')
            require(all('title' not in w and 'title' not in w['owner'] for w in value['windows']), 'title leaked')
            require(len(json.dumps(value, separators=(',',':'),ensure_ascii=False).encode())<=48*1024, 'result size')
            snapshots.append(value)
        require(snapshots[0]['snapshot_id']!=snapshots[1]['snapshot_id'] and snapshots[1]['completed_monotonic_us']>snapshots[0]['completed_monotonic_us'], 'fresh snapshots')
        require(snapshots[0]['console_identity']==snapshots[1]['console_identity'], 'console changed')
        check('two_actual_bounded_title_free_snapshots')
        phase = 'negative_scope_checks'
        for name,args in [('windows',{'id':a.id,'titles':True}), ('windows',{'id':'windows-observe-unknown'}),
                          ('exec',{'id':a.id,'argv':['true']}),('file_upload_begin',{'id':a.id})]:
            call(name,args,denied=True)
        check('extra_arguments_unknown_vm_and_ungranted_scopes_denied')
        phase = 'same_console_screenshot'
        capture = call('screenshot', {'id':a.id})
        image, metadata = capture['content']
        meta = common.strict_json(metadata['text'].encode())
        data = base64.b64decode(image['data'], validate=True)
        require(image['mimeType']=='image/jpeg' and len(data)==meta['byte_length']<=700*1024 and hashlib.sha256(data).hexdigest()==meta['sha256'], 'capture integrity')
        from PIL import Image
        with Image.open(io.BytesIO(data)) as im:
            im.load(); require(im.format=='JPEG' and im.size==(meta['width'],meta['height']), 'full capture decode')
        current = snapshots[-1]
        require(meta['console_identity']==current['console_identity'], 'same-console capture')
        display = next(d for d in current['displays'] if d['display_id']==meta['display_id'])
        require(display['bounds_points']==meta['desktop_bounds_points'] and display['point_pixel_scale']==meta['point_pixel_scale'], 'screenshot/display mapping')
        save('console.jpg',data);save('console.json',meta)
        check('independently_decoded_same_console_geometry')
        report['passed']=True
    except Exception as e:
        report['error_type']=type(e).__name__
        report['failed_phase']=phase
        report['error']='actual window observation check failed; inspect preserved artifacts'
    finally:
        report['mcp_cleanup']=common.close_client(c)
        report['passed'] &= report['mcp_cleanup']['confirmed']
        report['finished_unix']=time.time()
        save('report.json',report)
    print(json.dumps(report,indent=2))
    return 0 if report['passed'] else 1


if __name__=='__main__':
    raise SystemExit(main())
