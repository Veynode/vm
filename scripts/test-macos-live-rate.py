#!/usr/bin/env python3
"""Attach-only real macOS Godot console -> required-hardware encoder rate trial.

No VM lifecycle, UI actions, Guest command execution, browser or network test.
Requires an already-running visible Godot window and unused proven capture bridge.
Private output includes captured desktop Annex B: retain as sensitive diagnostics.
"""
import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import re
import select
import socket
import stat
import struct
import subprocess
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
MAX_AUS = 1024
MAX_BYTES = 128*1024*1024


def require(ok, code):
    if not ok:
        raise AssertionError(code)


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


def safe_error(error):
    value = str(error)
    return value if isinstance(error, AssertionError) and re.fullmatch('[a-z][a-z0-9_]{0,79}', value) else type(error).__name__


def godot_window_tuple(windows, display_id):
    """One observable window, not a producer nonce or unobscured-pixel proof."""
    found = [w for w in windows if w['owner']['bundle_identifier']=='org.godotengine.godot'
             and display_id in w['display_ids'] and w['bounds_points']['width']>0 and w['bounds_points']['height']>0]
    require(found, 'visible_godot_window_missing')
    require(len(found)==1, 'ambiguous_godot_windows')
    w=found[0];bounds=w['bounds_points']
    require(type(w['window_id']) is int and w['window_id']>0 and type(w['owner']['pid']) is int
            and w['owner']['pid']>0, 'invalid_godot_window_identity')
    require(set(bounds)=={'x','y','width','height'} and all(type(v) in (int,float) and math.isfinite(v)
            for v in bounds.values()), 'invalid_godot_window_geometry')
    return {'window_id':w['window_id'],'pid':w['owner']['pid'],'bounds_points':dict(bounds)}


def same_godot_window(before, after):
    require(before==after, 'godot_window_tuple_changed')


def terminal_diagnostic(value):
    """Never retain arbitrary errors, nested objects, paths or unknown values."""
    if value.get('event') not in ('stopped','error'):
        return None
    result={'event':value['event']}
    numeric=('encoded','submitted','keyframes','width','height')
    boolean=('hardware_verified','expected_console_identity_matched','console_identity_binding_requested',
             'guest_cleanup_confirmed','cleanup_receipt_requested','cleanup_receipt_persisted')
    invalid=False
    for key in numeric+boolean+('codec',):
        if key not in value:
            continue
        item=value[key]
        okay=(type(item) is int and 0<=item<=9007199254740991) if key in numeric else (
            type(item) is bool if key in boolean else item in ('h264','hevc'))
        if okay:result[key]=item
        else:invalid=True
    result['invalid_fields']=invalid
    error=value.get('error')
    result['error_present']=value['event']=='error' or error is not None
    fixed={'Guest capture failed':'guest_capture_failed',
           'Guest capture failed; waiting for independent cleanup attestation':'guest_capture_failed',
           'Hardware frame encode failed':'hardware_frame_encode_failed',
           'Guest stop acknowledgement exceeded 1500ms':'cleanup_timeout',
           'Private rate control disconnected or invalid':'rate_control_failed'}
    result['error_category']='none' if not result['error_present'] else 'encoder_failure'
    if isinstance(error,str):
        result['error_category']=fixed.get(error,result['error_category'])
        if re.fullmatch(r'Encode callback failed status=-?[0-9]+ flags=[0-9]+',error):
            result['error_category']='encode_callback_failed'
    return result


def private_json(path, value):
    with open(path, 'xb') as stream:
        stream.write((json.dumps(value, indent=2, sort_keys=True)+'\n').encode())
        stream.flush()
        os.fsync(stream.fileno())


def bounded_file(path, maximum, private=False):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        before = os.fstat(fd)
        require(stat.S_ISREG(before.st_mode) and before.st_uid == os.getuid()
                and before.st_nlink == 1 and 0 < before.st_size <= maximum, 'file_type_or_size')
        if private:
            require(before.st_mode & 0o777 == 0o600, 'file_not_private')
        data = bytearray()
        while len(data) <= maximum:
            block = os.read(fd, min(65536, maximum+1-len(data)))
            if not block:
                break
            data.extend(block)
        after = os.fstat(fd)
        identity = lambda s: (s.st_dev,s.st_ino,s.st_size,s.st_mtime_ns,s.st_ctime_ns)
        require(len(data) <= maximum and identity(before) == identity(after), 'file_changed_or_grew')
        return bytes(data)
    finally:
        os.close(fd)


def bounded_command(argv, timeout, stdout_limit=1024*1024, stderr_limit=65536):
    """Owned process; bounded output and absolute deadline; never logs raw stderr."""
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    result = [bytearray(), bytearray()]
    streams = {child.stdout:0, child.stderr:1}
    deadline = time.monotonic()+timeout
    try:
        while streams:
            left = deadline-time.monotonic()
            require(left > 0, 'command_deadline')
            readable, _, _ = select.select(list(streams), [], [], min(.1, left))
            for stream in readable:
                index = streams[stream]
                data = os.read(stream.fileno(), 65536)
                if not data:
                    del streams[stream]
                    continue
                require(len(result[index])+len(data) <= (stdout_limit if index == 0 else stderr_limit), 'command_output_limit')
                result[index].extend(data)
        code = child.wait(timeout=max(.001, deadline-time.monotonic()))
        return code, bytes(result[0]), bytes(result[1])
    finally:
        if child.poll() is None:
            child.kill()
            child.wait(timeout=3)
        child.stdout.close()
        child.stderr.close()


def au_header(data, previous):
    require(len(data) == 13, 'partial_au_header')
    length, pts, key = struct.unpack('>IQB', data)
    require(0 < length <= 4*1024*1024 and key in (0,1), 'invalid_au_header')
    require((previous is None and pts == 0 and key == 1)
            or (previous is not None and previous < pts <= previous+10_000_000), 'discontinuous_au_pts')
    return length, pts, key


class CaptureOutput:
    def __init__(self, child, path):
        self.child, self.path = child, path
        self.records, self.errors, self.diagnostics = [], [], []
        self.changed = threading.Condition()
        self.deadline = time.monotonic()+50
        self.stderr_digest = hashlib.sha256()
        self.stderr_bytes = 0
        self.eof = False
        self.threads = []

    def exact(self, stream, size, allow_eof=False):
        result = bytearray()
        while len(result) < size:
            left = self.deadline-time.monotonic()
            require(left > 0, 'encoder_output_deadline')
            if not select.select([stream], [], [], min(.1,left))[0]:
                continue
            part = os.read(stream.fileno(), size-len(result))
            if not part:
                require(allow_eof and not result, 'partial_au_record')
                return None
            result.extend(part)
        return bytes(result)

    def video(self):
        total, previous = 0, None
        try:
            with self.path.open('xb') as output:
                while True:
                    head = self.exact(self.child.stdout,13,True)
                    if head is None:
                        self.eof = True
                        break
                    length, pts, key = au_header(head,previous)
                    require(len(self.records) < MAX_AUS and total+length <= MAX_BYTES, 'encoded_output_limit')
                    body = self.exact(self.child.stdout,length)
                    output.write(body)
                    total += length
                    previous = pts
                    with self.changed:
                        self.records.append({'index':len(self.records),'pts_us':pts,'keyframe':bool(key),
                            'bytes':length,'sha256':hashlib.sha256(body).hexdigest(),'received_monotonic_ns':time.monotonic_ns()})
                        self.changed.notify_all()
                output.flush()
                os.fsync(output.fileno())
        except Exception as error:
            with self.changed:
                self.errors.append(safe_error(error));self.changed.notify_all()

    def stderr(self):
        pending = bytearray()
        try:
            while True:
                left = self.deadline-time.monotonic()
                require(left > 0, 'encoder_stderr_deadline')
                if not select.select([self.child.stderr], [], [], min(.1,left))[0]:
                    continue
                block = os.read(self.child.stderr.fileno(),4096)
                if not block:
                    require(not pending, 'partial_encoder_diagnostic')
                    break
                self.stderr_bytes += len(block);self.stderr_digest.update(block)
                require(self.stderr_bytes <= 256*1024, 'encoder_stderr_limit')
                pending.extend(block)
                while b'\n' in pending:
                    line, _, pending = pending.partition(b'\n')
                    require(len(line) <= 16384, 'encoder_diagnostic_line_limit')
                    try:
                        value = json.loads(line)
                    except (ValueError, UnicodeError):
                        continue
                    if isinstance(value,dict):
                        require(len(self.diagnostics)<128, 'encoder_diagnostic_count_limit')
                        # In-memory only: later report stores an explicit allowlist.
                        self.diagnostics.append(value)
                require(len(pending) <= 16384, 'encoder_diagnostic_line_limit')
        except Exception as error:
            with self.changed:
                self.errors.append(safe_error(error));self.changed.notify_all()

    def start(self):
        for target in (self.video,self.stderr):
            thread = threading.Thread(target=target,daemon=True)
            self.threads.append(thread);thread.start()

    def wait_until(self, deadline, first=False):
        with self.changed:
            while time.monotonic() < deadline:
                require(not self.errors, 'encoder_collector_failed')
                require(self.child.poll() is None, 'encoder_exited_early')
                if first and self.records:
                    return
                self.changed.wait(min(.05,max(0,deadline-time.monotonic())))
            require(not first or self.records, 'first_frame_deadline')
            require(not self.errors and self.child.poll() is None, 'encoder_phase_failed')


def measured_window(records, begin_ns, end_ns, submitted):
    selected = [x for x in records if begin_ns <= x['received_monotonic_ns'] < end_ns and x['index'] >= submitted]
    require(len(selected) >= 3, 'insufficient_measurement_frames')
    require(all(b['index']==a['index']+1 and b['pts_us']>a['pts_us'] for a,b in zip(selected,selected[1:])), 'measurement_not_contiguous')
    duration = (selected[-1]['pts_us']-selected[0]['pts_us'])/1e6
    # Count frames whose preceding PTS interval is present; no invented 30fps tail.
    count = sum(x['bytes'] for x in selected[1:])
    require(3 <= duration <= 5 and count > 0, 'invalid_measurement_duration')
    return {'first_index':selected[0]['index'],'last_index':selected[-1]['index'],
            'observed_frames':len(selected),'counted_frames':len(selected)-1,
            'measured_seconds':duration,'measured_bytes':count,'measured_bps':count*8/duration,
            'arrival_window_start_ns':begin_ns,'arrival_window_end_ns':end_ns}


def stop_encoder(child, output):
    result = {'stop_sent':False,'forced_kill':False,'owned_child_reaped':False}
    try:
        if child.poll() is None:
            fd = child.stdin.fileno();os.set_blocking(fd,False)
            require(select.select([], [fd], [], .5)[1], 'stop_write_deadline')
            require(os.write(fd,b'stop\n') == 5, 'stop_write_incomplete')
            result['stop_sent'] = True
        child.wait(timeout=8)
    except Exception as error:
        result['stop_error'] = safe_error(error)
    finally:
        if child.poll() is None:
            result['forced_kill'] = True
            child.kill()
            try:child.wait(timeout=3)
            except subprocess.TimeoutExpired:pass
        result['exit_code'] = child.poll()
        result['owned_child_reaped'] = child.poll() is not None
        if output:
            for thread in output.threads:thread.join(timeout=2)
            result['readers_finished'] = not any(t.is_alive() for t in output.threads)
        for pipe in (child.stdin,child.stdout,child.stderr):
            if pipe and not pipe.closed:pipe.close()
    return result


def run(args):
    os.umask(0o077)
    require(args.output_dir.is_absolute() and args.output_dir.parent.resolve()==args.output_dir.parent, 'output_parent_not_canonical')
    args.output_dir.mkdir(mode=0o700,exist_ok=False)
    report = {'schema':1,'passed':False,'codec':args.codec,'phases':[],
        'targets_bps':[args.high_bps,args.low_bps,args.high_bps],
        'experimental_window_expected':args.expect_data_rate_limit,
        'scope':'Actual macOS same-console animated Godot capture; hardware encoder actuator only. No browser, network adaptation, calibrated bandwidth, visual-quality or performance comparison.',
        'pixels_saved':'Private desktop Annex B; never publish without explicit review.',
        'started_unix':time.time(),'cleanup_confirmed':False}
    child = output = client = control = child_control = None
    observer = cleanup = rate_module = binding = identity = None
    rate = None
    phase = 'preflight'
    receipt_path = args.output_dir/'cleanup-receipt.json'
    identity_path = args.output_dir/'expected-console-identity.json'
    try:
        observer = load('live_rate_observer', ROOT/'scripts/test-control-macos-windows.py')
        cleanup = load('live_rate_cleanup', ROOT/'scripts/test-macos-capture-cleanup.py')
        rate_module = load('live_rate_channel',ROOT/'native/tests/test-encoder-rate-live.py')
        report['binary_sha256'] = hashlib.sha256(bounded_file(args.binary,64*1024*1024)).hexdigest()
        report['controller_sha256'] = hashlib.sha256(bounded_file(args.control_binary,128*1024*1024)).hexdigest()
        report['harness_sha256'] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
        report['rate_channel_source_sha256'] = hashlib.sha256((ROOT/'native/tests/test-encoder-rate-live.py').read_bytes()).hexdigest()
        binding_data = bounded_file(args.binding,65536)
        binding = observer.common.strict_json(binding_data)['binding']
        report['binding_artifact_sha256'] = hashlib.sha256(binding_data).hexdigest()

        def verify_binding():
            code, out, _ = bounded_command([str(args.control_binary.resolve()),'call','--state-dir',str(args.state_dir.resolve()),
                'guest_stream_bridge_binding',json.dumps({'id':args.id,**{k:binding[k] for k in ('worker_instance','bridge_instance')}})],10,65536)
            require(code == 0 and observer.common.strict_json(out)['binding'] == binding, 'bridge_provenance_changed')
            s = os.lstat(binding['socket_path'])
            require(stat.S_ISSOCK(s.st_mode) and s.st_uid == os.getuid() and s.st_mode&0o777 == 0o600, 'bridge_socket_not_private')
        verify_binding()
        client = observer.Observer.__new__(observer.Observer)
        observer.Observer.__init__(client,args.control_binary.resolve(),args.state_dir.resolve(),time.monotonic()+120)

        def tool(name):
            reply = client.rpc('tools/call',{'name':'vm_'+name,'arguments':{'id':args.id}})
            require('error' not in reply and reply.get('result',{}).get('isError') is False, 'observation_failed')
            content = reply['result']['content']
            require(len(content)==1 and content[0]['type']=='text', 'unexpected_observation_body')
            return observer.common.strict_json(content[0]['text'].encode())

        def godot_observation(expected=None):
            guest = tool('guest_info')['result']
            require(guest['os']=='darwin' and guest['machine'].startswith('VirtualMac')
                    and guest['console_identity_status']['identity_eligible'] is True, 'guest_identity_unavailable')
            current = guest['console_identity']
            if expected is not None:require(current==expected, 'console_identity_changed')
            windows = tool('windows')
            require(windows['console_identity']==current and windows['titles_included'] is False
                    and windows['filter']=='on_screen_application_windows_layer_0' and windows['truncated'] is False, 'window_observation_boundary')
            observed=godot_window_tuple(windows['windows'],current['display_id'])
            return current, {'snapshot_id':windows['snapshot_id'],'completed_monotonic_us':windows['completed_monotonic_us'],
                'godot_window':observed,
                'visibility_limit':'Exactly one listed on-screen window; PID/window ID/geometry tuple, not producer nonce, unobscured pixels or atomic capture.'}

        identity, report['godot_before'] = godot_observation()
        report['console_identity'] = identity
        private_json(identity_path,identity)
        verify_binding()
        phase = 'capture'
        control, child_control = socket.socketpair()
        rate = rate_module.RateChannel(control,args.expect_data_rate_limit)
        child = subprocess.Popen([str(args.binary.resolve()),binding['socket_path'],'--codec',args.codec,
            '--expected-console-identity',str(identity_path),'--cleanup-receipt',str(receipt_path),
            '--rate-control-fd',str(child_control.fileno())],pass_fds=(child_control.fileno(),),
            stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        child_control.close();child_control=None
        report['owned_encoder_pid'] = child.pid
        output = CaptureOutput(child,args.output_dir/'capture.annexb');output.start()
        output.wait_until(time.monotonic()+8,first=True)
        status = rate.call({'version':1,'request_id':'status','method':'rate_status'})
        require(re.fullmatch('[0-9a-f]{32}',status['encoder_instance']) and status['epoch']==1
                and status['sequence']==0 and status['target_bps']==status['readback_bps']==8000000, 'initial_rate_identity')
        report['initial_status'] = status
        encoder_id = {k:status[k] for k in ('encoder_instance','epoch')}
        for number,target in enumerate(report['targets_bps'],1):
            phase = f'rate_{number}'
            ack = rate.call({'version':1,'request_id':f'rate-{number}','method':'set_bitrate',**encoder_id,
                             'sequence':number,'target_bps':target})
            require(all(ack[k]==v for k,v in encoder_id.items()) and ack['sequence']==number
                    and ack['target_bps']==ack['readback_bps']==target, 'rate_update_identity')
            began = time.monotonic_ns();end = began+6_000_000_000
            row={'target_bps':target,'ack':ack,'phase_start_monotonic_ns':began,'phase_end_monotonic_ns':end,'settle_seconds':2,'measure_seconds':4}
            report['phases'].append(row)
            output.wait_until(end/1e9)
            with output.changed:
                row.update(measured_window(output.records,began+2_000_000_000,end,ack['submitted_frames']))
        report['final_status'] = rate.call({'version':1,'request_id':'final-status','method':'rate_status'})
        require(all(report['final_status'][k]==v for k,v in encoder_id.items()) and report['final_status']['sequence']==3,
                'final_encoder_identity')
        phase = 'final_godot_observation'
        _, report['godot_after'] = godot_observation(identity)
        same_godot_window(report['godot_before']['godot_window'],report['godot_after']['godot_window'])
        require(report['godot_after']['completed_monotonic_us']>report['godot_before']['completed_monotonic_us'], 'window_snapshot_not_fresh')
        report['measurement_completed'] = True
    except Exception as error:
        report['error'] = safe_error(error);report['failed_phase'] = phase
    finally:
        if child:
            report['encoder_cleanup'] = stop_encoder(child,output)
        if child_control:child_control.close()
        if control:control.close()
        if rate:report['rate_exchange']=rate.replies
        if output:
            report['records']=output.records
            report['collector_errors']=output.errors
            report['stderr_sha256']=output.stderr_digest.hexdigest()
            report['stderr_bytes']=output.stderr_bytes
            summaries=[safe for d in output.diagnostics if (safe:=terminal_diagnostic(d)) is not None]
            report['encoder_terminals']=summaries
            if len(summaries)==1:
                s=summaries[0]
                report['encoder_summary']=s
                report['encoder_error_present']=s['error_present'] or s['invalid_fields']
            report['au_eof']=output.eof
        # Never open a second bridge consumer until the owned encoder is reaped.
        if binding and cleanup and report.get('encoder_cleanup',{}).get('owned_child_reaped'):
            try:
                ticket = json.loads(bounded_file(receipt_path,2048,True))
                require(set(ticket)=={'version','service_instance','capture_instance','cleanup_retention_ms'}
                        and type(ticket['version']) is int and ticket['version']==1
                        and type(ticket['cleanup_retention_ms']) is int and ticket['cleanup_retention_ms']==120000
                        and all(re.fullmatch('[0-9a-f]{32}',ticket[k]) for k in ('service_instance','capture_instance')), 'invalid_cleanup_receipt')
                verify_binding()
                query_report={};report['independent_cleanup']=query_report
                cleanup.await_clean(Path(binding['socket_path']),{k:ticket[k] for k in ('service_instance','capture_instance')},query_report)
                verify_binding()
                report['cleanup_confirmed']=True
            except Exception as error:report['cleanup_query_error']=safe_error(error)
        if client and observer:
            report['mcp_cleanup']=observer.common.close_client(client,timeout=10)
        else:report['mcp_cleanup']={'confirmed':client is None}
        # Save failures before decoder or final assertions; never overwrite this checkpoint.
        private_json(args.output_dir/'capture-checkpoint.json',report)
    try:
        require(report.get('measurement_completed') is True, 'measurement_incomplete')
        require(report['encoder_cleanup']['exit_code']==0 and report['encoder_cleanup']['owned_child_reaped']
                and not report['encoder_cleanup']['forced_kill'] and report['encoder_cleanup']['readers_finished'], 'encoder_cleanup_unconfirmed')
        require(report['cleanup_confirmed'] and report['mcp_cleanup']['confirmed'] and report['au_eof']
                and not report['collector_errors'] and not report['encoder_error_present'], 'source_cleanup_or_output_failed')
        s=report['encoder_summary']
        require(all(s[k] is True for k in ('hardware_verified','expected_console_identity_matched','console_identity_binding_requested',
                   'guest_cleanup_confirmed','cleanup_receipt_requested','cleanup_receipt_persisted')), 'encoder_binding_attestation')
        require(s['codec']==args.codec and (s['width'],s['height'])==((1280,720) if args.codec=='h264' else (1920,1080))
                and s['encoded']==s['submitted']==len(report['records']), 'encoder_count_or_geometry')
        for row in report['phases']:
            require(0 < row['ack']['submitted_frames'] <= row['first_index'], 'rate_submission_boundary')
        code,out,err=bounded_command(['ffmpeg','-v','error','-i',str(args.output_dir/'capture.annexb'),'-f','framemd5','-'],60)
        require(code==0 and not err, 'independent_decode_failed')
        geometry = (1280,720) if args.codec=='h264' else (1920,1080)
        dimension_lines=[line for line in out.splitlines() if line.startswith(b'#dimensions')]
        require(len(dimension_lines)==1 and re.fullmatch(rb'#dimensions\s+0:\s+'+str(geometry[0]).encode()+b'x'+str(geometry[1]).encode(),dimension_lines[0]), 'independent_decode_geometry')
        with (args.output_dir/'decoded.framemd5').open('xb') as f:f.write(out)
        hashes=[line.rsplit(b',',1)[-1].strip().decode('ascii') for line in out.splitlines() if line and not line.startswith(b'#')]
        require(len(hashes)==len(report['records']) and all(re.fullmatch('[0-9a-f]{32}',h) for h in hashes), 'decode_count_or_hash')
        report['decoded_frames']=len(hashes)
        for row in report['phases']:
            row['distinct_decoded_frames']=len(set(hashes[row['first_index']:row['last_index']+1]))
            require(row['distinct_decoded_frames']>1, 'static_measurement_window')
        high,low,restored=[p['measured_bps'] for p in report['phases']]
        report['rate_ratios']={'high_over_low':high/low,'restored_over_low':restored/low}
        require(high/low>1.4 and restored/low>1.4, 'bitrate_efficacy_failed')
        report['passed']=True
    except Exception as error:
        report.setdefault('error',safe_error(error));report.setdefault('failed_phase','final_validation')
    report['finished_unix']=time.time()
    private_json(args.output_dir/'result.json',report)
    print(json.dumps({'passed':report['passed'],'cleanup_confirmed':report['cleanup_confirmed'],'error':report.get('error')}))
    return 0 if report['passed'] else 1


def self_test():
    """Pure framing/window negatives; no subprocess, capture or socket connection."""
    for head,prev in [(struct.pack('>IQB',0,0,1),None),(struct.pack('>IQB',10,2,0),None),
                      (struct.pack('>IQB',10,5,0),5),(b'bad',None)]:
        try:au_header(head,prev)
        except AssertionError:pass
        else:raise AssertionError('invalid_header_accepted')
    rows=[{'index':i,'received_monotonic_ns':i*1_000_000_000,'pts_us':i*1_000_000,'bytes':100} for i in range(7)]
    value=measured_window(rows,2_000_000_000,6_000_000_000,1)
    require(value['measured_seconds']==3 and value['measured_bytes']==300 and value['measured_bps']==800, 'window_math')
    for bad in [rows[:2],[{**r,'pts_us':0} for r in rows]]:
        try:measured_window(bad,2_000_000_000,6_000_000_000,1)
        except AssertionError:pass
        else:raise AssertionError('invalid_window_accepted')
    require(safe_error(ValueError('secret payload'))=='ValueError' and safe_error(AssertionError('secret payload'))=='AssertionError', 'error_redaction')
    window={'window_id':42,'owner':{'pid':500,'bundle_identifier':'org.godotengine.godot'},
            'bounds_points':{'x':0,'y':30,'width':1280,'height':616},'display_ids':[1]}
    observed=godot_window_tuple([window],1)
    same_godot_window(observed,godot_window_tuple([dict(window)],1))
    try:godot_window_tuple([window,{**window,'window_id':43}],1)
    except AssertionError:pass
    else:raise AssertionError('editor_plus_scene_accepted')
    for change in [{'window_id':43},{'pid':501},{'bounds_points':{**observed['bounds_points'],'width':1279}}]:
        try:same_godot_window(observed,{**observed,**change})
        except AssertionError:pass
        else:raise AssertionError('changed_window_tuple_accepted')
    for event in ['stopped','error']:
        safe=terminal_diagnostic({'event':event,'codec':'h264','encoded':8,'hardware_verified':True,
            'error':'SECRET credential/path','guest_error':{'message':'SECRET'},'width':'SECRET','extra':'SECRET'})
        require('SECRET' not in json.dumps(safe) and 'guest_error' not in safe and 'extra' not in safe
                and safe['invalid_fields'] and safe['error_category']=='encoder_failure', 'terminal_redaction')
        safe=terminal_diagnostic({'event':event,'encoded':8,'hardware_verified':True,'error':None})
        require(safe['error_present'] is (event=='error'), 'terminal_event_failure')
    require(terminal_diagnostic({'event':'error','error':'Encode callback failed status=-12911 flags=2'})['error_category']=='encode_callback_failed', 'terminal_fixed_category')
    print('PASS pure offline framing/window-tuple/terminal-redaction checks; no encoder execution')


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--self-test',action='store_true')
    for name in ('binary','control-binary','state-dir','binding','output-dir'):p.add_argument('--'+name,type=Path)
    p.add_argument('--id');p.add_argument('--codec',choices=('h264','hevc'))
    p.add_argument('--high-bps',type=int,default=12000000);p.add_argument('--low-bps',type=int,default=2000000)
    p.add_argument('--expect-data-rate-limit',action='store_true')
    a=p.parse_args()
    if a.self_test:self_test();return 0
    if not all(getattr(a,n) for n in ('binary','control_binary','state_dir','binding','output_dir','id','codec')):p.error('all attach paths, VM id and codec are required')
    if not 250000 <= a.low_bps < a.high_bps <= 40000000:p.error('require 250000 <= low-bps < high-bps <= 40000000')
    return run(a)


if __name__=='__main__':
    raise SystemExit(main())
