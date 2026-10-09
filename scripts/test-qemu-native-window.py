#!/usr/bin/env python3
"""Attach-only QEMU native-window MCP acceptance; never create/start/stop a VM.
RPC state, actual native GPU completion counters and independently decoded PNGs
are checked. Visible pixels and physical display presentation require manual CUA.
"""
import argparse
import base64
import hashlib
import json
import math
import os
from pathlib import Path
import re
import select
import stat
import subprocess
import time


class MCP:
    def __init__(self, binary, state, timeout, lifecycle=False):
        self.process = subprocess.Popen([str(binary), 'mcp', '--state-dir', str(state),
            *(['--allow-lifecycle'] if lifecycle else [])], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.timeout = timeout
        self.number = 0
        self.buffer = bytearray()
        try:
            reply = self.rpc('initialize', {'protocolVersion':'2025-06-18','capabilities':{},
                'clientInfo':{'name':'qemu-native-window-acceptance','version':'1'}})
            assert reply.get('result', {}).get('protocolVersion') == '2025-06-18', 'MCP initialization failed'
            self.send({'jsonrpc':'2.0','method':'notifications/initialized'})
        except BaseException:
            self.dispose()
            raise

    def send(self, message):
        self.process.stdin.write(json.dumps(message).encode() + b'\n')
        self.process.stdin.flush()

    def rpc(self, method, params):
        self.number += 1
        self.send({'jsonrpc':'2.0','id':self.number,'method':method,'params':params})
        deadline = time.monotonic() + self.timeout
        while b'\n' not in self.buffer:
            left = deadline - time.monotonic()
            if left <= 0 or not select.select([self.process.stdout], [], [], left)[0]:
                raise TimeoutError('MCP response deadline')
            chunk = os.read(self.process.stdout.fileno(), 65536)
            if not chunk:
                raise RuntimeError('MCP EOF before response')
            self.buffer.extend(chunk)
            assert len(self.buffer) <= 1024 * 1024, 'MCP response exceeds bound'
        line, _, rest = self.buffer.partition(b'\n')
        self.buffer = bytearray(rest)
        reply = json.loads(line)
        assert reply.get('id') == self.number, 'MCP response ID mismatch'
        return reply

    def tool(self, name, vm, denied=False):
        assert name in {'vm_show_window','vm_close_window','vm_screenshot'}, 'test refuses other mutations'
        reply = self.rpc('tools/call', {'name':name,'arguments':{'id':vm}})
        failed = 'error' in reply or reply.get('result', {}).get('isError') is True
        if denied:
            assert failed, 'observe-only client mutated native window'
            return {'denied':True, 'jsonrpc_error': 'error' in reply}
        assert not failed, f'{name} rejected: {reply.get("error", reply.get("result"))}'
        return reply['result']

    def close(self):
        if not self.process.stdin.closed:
            self.process.stdin.close()
        try:
            self.process.wait(timeout=12)
            assert self.process.returncode == 0, 'MCP session cleanup not confirmed'
        finally:
            self.dispose()

    def dispose(self):
        if self.process.poll() is None:
            self.process.kill()  # only this still-owned MCP child, never a VM
            self.process.wait(timeout=3)
        for stream in [self.process.stdin, self.process.stdout, self.process.stderr]:
            if stream and not stream.closed:
                stream.close()


def pacing_limits(value):
    def integer(key, maximum):
        n = value.get(key)
        assert type(n) is int and 0 < n <= maximum, f'invalid pacing limit {key}'
        return n
    if 'capture_limit_fps' in value:
        integer('capture_limit_fps', 60)
    extended = ['native_window_capture_limit_fps','background_capture_limit_fps','encoder_limit_fps',
                'idle_encode_interval_ms','display_refresh_preference_hz']
    if any(key in value for key in extended):
        capture = integer('capture_limit_fps',60)
        window = integer('native_window_capture_limit_fps',60)
        background = integer('background_capture_limit_fps',60)
        encoder = integer('encoder_limit_fps',60)
        integer('idle_encode_interval_ms',10000); integer('display_refresh_preference_hz',240)
        assert encoder <= background <= window <= capture, 'inconsistent pacing ceilings'
        if 'capture_current_limit_fps' in value:
            assert type(value['capture_current_limit_fps']) is int and value['capture_current_limit_fps'] in (background,window)
    else:
        assert 'capture_current_limit_fps' not in value, 'current limit has no pacing contract'


def window_status(value, expected=None):
    pacing_limits(value)
    assert value.get('native_window') is True and value.get('native_window_protocol') == 1
    assert value.get('display_only') is True and value.get('input_enabled') is False
    assert type(value.get('window_open')) is bool
    if expected is not None:
        assert value['window_open'] is expected, 'window state did not match acknowledgement'
    for key in ['drawables_submitted','gpu_commands_completed','gpu_commands_failed']:
        assert type(value.get(key)) is int and value[key] >= 0, f'invalid {key}'
    assert type(value.get('display_gpu_inflight')) is bool, 'invalid display GPU in-flight flag'
    assert type(value.get('display_gpu_inflight_limit')) is int and value['display_gpu_inflight_limit'] == 1, 'display GPU in-flight limit changed'
    assert value['gpu_commands_completed'] <= value['drawables_submitted']
    return value


def displayed_frame(value):
    assert value.get('displayed_frame_protocol') == 1
    assert value.get('displayed_frame_authorizes_input') is False
    available = value.get('displayed_frame_available')
    assert type(available) is bool
    frame = value.get('displayed_frame')
    if not available:
        assert frame is None and value.get('displayed_frame_age_ms') is None
        return None
    assert value['window_open'] is True and frame['protocol_version'] == 1
    assert re.fullmatch('[0-9a-f]{32}', frame['display_instance'])
    for key in ['presentation_serial','window_epoch','invalidation_epoch','source_generation',
                'source_sequence','source_width','source_height','source_captured_monotonic_us']:
        assert type(frame[key]) is int and frame[key] > 0, key
    assert frame['source_generation'] == value['source_generation']
    assert frame['source_sequence'] <= value['source_sequence']
    assert (frame['source_width'], frame['source_height']) == (value['display_source_width'], value['display_source_height'])
    for key in ['metal_presented_time_seconds','callback_received_monotonic_seconds','drawable_width','drawable_height']:
        assert type(frame[key]) in (int,float) and math.isfinite(frame[key]) and frame[key] > 0, key
    age = value['displayed_frame_age_ms']
    assert type(age) in (int,float) and math.isfinite(age) and age >= 0
    assert type(frame['view_is_flipped']) is bool
    for key in ['view_bounds_points','backing_bounds_pixels','viewport_pixels']:
        rect = frame[key]
        assert all(type(rect[k]) in (int,float) and math.isfinite(rect[k]) for k in ['x','y','width','height'])
        assert rect['width'] > 0 and rect['height'] > 0
    viewport = frame['viewport_pixels']
    assert 0 <= viewport['x'] and viewport['x'] + viewport['width'] <= frame['drawable_width']
    assert 0 <= viewport['y'] and viewport['y'] + viewport['height'] <= frame['drawable_height']
    return frame


def decode_png(data, metadata):
    width, height = metadata.get('width'), metadata.get('height')
    assert type(width) is int and type(height) is int and 0 < width <= 3840 and 0 < height <= 2160
    assert 0 < len(data) <= 700 * 1024 and metadata.get('byte_length') == len(data)
    assert data.startswith(b'\x89PNG\r\n\x1a\n')
    decoded = subprocess.run(['ffmpeg','-hide_banner','-loglevel','error','-xerror','-i','pipe:0',
        '-map','0:v:0','-frames:v','1','-f','framemd5','pipe:1'], input=data,
        capture_output=True, timeout=15)
    assert decoded.returncode == 0, 'independent PNG decode failed'
    output = decoded.stdout.decode('ascii')
    dimensions = re.search(r'^#dimensions 0: (\d+)x(\d+)$', output, re.MULTILINE)
    assert dimensions and tuple(map(int, dimensions.groups())) == (width, height), 'decoded geometry mismatch'
    frames = [line for line in output.splitlines() if line and not line.startswith('#')]
    assert len(frames) == 1 and len(frames[0].split(',')) == 6, 'decoder did not emit exactly one frame'
    return {'decoder':'FFmpeg framemd5','decoded_frames':1,'width':width,'height':height,
            'decoded_frame_digest':frames[0].split(',')[-1].strip()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--state', type=Path, required=True)
    parser.add_argument('--vm', required=True)
    parser.add_argument('--output', type=Path, required=True, help='new private evidence directory; never overwritten')
    parser.add_argument('--expect-width', type=int)
    parser.add_argument('--expect-height', type=int)
    parser.add_argument('--require-displayed-frame', action='store_true', help='require real Metal presentation records and close/reopen invalidation')
    parser.add_argument('--rpc-timeout', type=float, default=20)
    parser.add_argument('--frame-timeout', type=float, default=15)
    args = parser.parse_args()
    if not 3 <= args.rpc_timeout <= 60 or not 1 <= args.frame_timeout <= 60:
        parser.error('timeouts must be bounded: RPC 3..60 seconds; frames 1..60 seconds')
    if not re.fullmatch('[a-zA-Z0-9_-]{1,64}', args.vm):
        parser.error('invalid VM identity')
    if (args.expect_width is None) != (args.expect_height is None):
        parser.error('expected screenshot dimensions must be supplied together')
    if args.expect_width is not None and not (320 <= args.expect_width <= 3840 and 320 <= args.expect_height <= 2160):
        parser.error('expected dimensions outside supported capture bounds')
    binary, state = args.binary.resolve(strict=True), args.state.resolve(strict=True)
    output = args.output.absolute()
    output.mkdir(mode=0o700)  # exclusive new directory, including symlink refusal
    socket_path = state/'control.sock'
    inode = None
    report = {'passed':False,'started_at_unix':time.time(),'vm':args.vm,'checks':[],
        'scope':__doc__,'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),
        'script_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'manual_visible_pixel_acceptance':'not performed by this script',
        'physical_presentation_acceptance':False,
        'pacing_fields_semantics':'configuration ceilings/preferences only; actual FPS not measured',
        'expected_screenshot_dimensions':None if args.expect_width is None else [args.expect_width,args.expect_height]}
    clients = []
    initial = None
    identity = None
    lifecycle = None

    def save():
        path = output/'report.json'
        fd = os.open(path, os.O_CREAT | os.O_TRUNC | os.O_WRONLY, 0o600)
        with os.fdopen(fd, 'w') as file:
            json.dump(report, file, indent=2); file.write('\n')

    def record(name, evidence):
        report['checks'].append({'name':name,'evidence':evidence})
        save(); print(name, flush=True)

    def call(method):
        assert method in {'status','window_status'}, 'test refuses local lifecycle mutations'
        now = socket_path.stat()
        assert (now.st_dev, now.st_ino) == (inode.st_dev, inode.st_ino), 'controller instance changed'
        result = subprocess.run([str(binary),'call','--state-dir',str(state),method,json.dumps({'id':args.vm})],
            capture_output=True, text=True, timeout=args.rpc_timeout)
        if result.returncode:
            raise RuntimeError(f'{method} failed: {result.stderr[:2048]}')
        assert len(result.stdout) <= 1024 * 1024
        return json.loads(result.stdout)

    def verify_vm():
        value = call('status')
        assert value['config']['id'] == args.vm and value['config']['backend'] == 'qemu-hvf'
        assert value['state'] == 'running', 'specified existing QEMU VM is not running'
        current = (value.get('pid'), value.get('runtime', {}).get('console_socket'))
        assert type(current[0]) is int and current[0] > 1 and current[1]
        if identity is not None:
            assert current == identity, 'VM worker or console changed; refusing further mutation'
        return current

    def client(allowed=False):
        value = MCP(binary, state, args.rpc_timeout, lifecycle=allowed)
        clients.append(value)
        return value

    def mutate(method):
        verify_vm()
        reply = lifecycle.tool(method, args.vm)
        value = json.loads(reply['content'][0]['text'])
        window_status(value, method == 'vm_show_window')
        return value

    def wait_for_draw(previous):
        deadline = time.monotonic() + args.frame_timeout
        while True:
            verify_vm()
            value = window_status(call('window_status'), True)
            assert value['gpu_commands_failed'] == previous['gpu_commands_failed'], 'new GPU command failure'
            if (value.get('frame_available') is True
                    and value['drawables_submitted'] > previous['drawables_submitted']
                    and value['gpu_commands_completed'] > previous['gpu_commands_completed'] + int(previous['display_gpu_inflight'])):
                if not args.require_displayed_frame or displayed_frame(value) is not None:
                    return value
            if time.monotonic() >= deadline:
                raise TimeoutError('no newly completed native display GPU command after show')
            time.sleep(.1)

    def screenshot(name):
        verify_vm()
        reply = lifecycle.tool('vm_screenshot', args.vm)
        assert reply['content'][0]['type'] == 'image' and reply['content'][0]['mimeType'] == 'image/png'
        data = base64.b64decode(reply['content'][0]['data'], validate=True)
        metadata = json.loads(reply['content'][1]['text'])
        decoded = decode_png(data, metadata)
        if args.expect_width is not None:
            assert (metadata['width'],metadata['height']) == (args.expect_width,args.expect_height), 'actual screenshot does not match expected initial mode'
        filename = name + '.png'
        fd = os.open(output/filename, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, 'wb') as image: image.write(data)
        result = {k:metadata[k] for k in ['frame_id','source_generation','source_sequence','captured_at_us','captured_monotonic_us','width','height','byte_length'] if k in metadata}
        result.update({'file':filename,'sha256':hashlib.sha256(data).hexdigest(),'independent_decode':decoded})
        record(name, result)

    try:
        save()
        inode = socket_path.stat()
        assert stat.S_ISSOCK(inode.st_mode), 'existing controller socket required'
        identity = verify_vm()
        initial = window_status(call('window_status'))
        if args.require_displayed_frame:
            displayed_frame(initial)
        record('initial_window_status', initial)
        observer = client()
        listed = observer.rpc('tools/list', {})['result']['tools']
        assert not {'vm_show_window','vm_close_window'} & {tool['name'] for tool in listed}
        for name in ['vm_show_window','vm_close_window']:
            record('observe_only_denied_' + name, observer.tool(name,args.vm,denied=True))
            assert call('window_status')['window_open'] is initial['window_open']
        observer.close();clients.remove(observer)
        lifecycle = client(True)
        # A pre-open window must be closed first to require a genuinely new draw
        # after show, including for completely static Guest desktops.
        if initial['window_open']:
            record('prepare_initially_open_window', mutate('vm_close_window'))
        before = window_status(call('window_status'), False)
        record('show_ack', mutate('vm_show_window'))
        shown = wait_for_draw(before)
        record('after_show_gpu_completion', shown)
        screenshot('open-screenshot')
        record('close_ack', mutate('vm_close_window'))
        closed = window_status(call('window_status'), False)
        record('closed_window_status', closed)
        if args.require_displayed_frame:
            assert displayed_frame(closed) is None, 'closed window retained a displayed anchor'
        screenshot('closed-screenshot-capture-continues')
        record('reopen_ack', mutate('vm_show_window'))
        reopened = wait_for_draw(closed)
        record('reopened_gpu_completion', reopened)
        if args.require_displayed_frame:
            old, new = displayed_frame(shown), displayed_frame(reopened)
            assert new['display_instance'] == old['display_instance']
            assert new['presentation_serial'] > old['presentation_serial']
            assert new['window_epoch'] > old['window_epoch']
            assert new['invalidation_epoch'] > old['invalidation_epoch']
            record('actual_presented_record_close_reopen', {'before':old,'after':new,'closed_record':None})
        screenshot('reopened-screenshot')
        report['checks_passed'] = True
    except Exception as error:
        report['error'] = {'type':type(error).__name__,'message':str(error)[:4096]}
    finally:
        if initial is not None:
            try:
                verify_vm()
                if lifecycle is None or lifecycle.process.poll() is not None:
                    lifecycle = client(True)
                current = window_status(call('window_status'))
                if current['window_open'] is not initial['window_open']:
                    mutate('vm_show_window' if initial['window_open'] else 'vm_close_window')
                restored = window_status(call('window_status'), initial['window_open'])
                report['restoration'] = {'confirmed':True,'window_status':restored}
            except Exception as error:
                report['restoration'] = {'confirmed':False,'type':type(error).__name__,'message':str(error)[:2048]}
        for c in clients:
            try: c.close()
            except Exception as error:
                report.setdefault('client_cleanup_errors', []).append(type(error).__name__)
                c.dispose()
        report['passed'] = bool(report.get('checks_passed') and report.get('restoration',{}).get('confirmed') and not report.get('client_cleanup_errors'))
        report['finished_at_unix'] = time.time()
        save()
    print(json.dumps({'passed':report['passed'],'report':str(output/'report.json')}))
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
