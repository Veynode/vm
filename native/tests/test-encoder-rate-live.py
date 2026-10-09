#!/usr/bin/env python3
"""Moving synthetic NV12 -> real hardware VT live bitrate changes -> FFmpeg.

No Guest, desktop capture, browser or network. Measures elementary-stream bytes,
not RTP wire rate, visual quality, congestion adaptation or Parallels performance.
Requires a newly built guest-video-stream with the private rate-control channel.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import socket
import struct
import subprocess
import tempfile
import threading
import time

import numpy as np

ROOT = Path(__file__).resolve().parents[2]


def require(value, message):
    if not value:
        raise AssertionError(message)


def event(**value):
    body = b'\x01' + json.dumps(value).encode()
    return struct.pack('>I', len(body)) + body


def exact(stream, length):
    result = bytearray()
    while len(result) < length:
        block = stream.read(length - len(result))
        if not block:
            raise EOFError('partial AU')
        result.extend(block)
    return bytes(result)


class RateChannel:
    """One synchronous request in flight, bounded framing and absolute timeout."""
    def __init__(self, connection, expect_window=False):
        self.connection = connection
        self.buffer = bytearray()
        self.sequence = 0
        self.replies = []
        self.expect_window = expect_window

    def call(self, value):
        wire = json.dumps(value, separators=(',', ':')).encode() + b'\n'
        require(len(wire) <= 1024, 'oversized rate request')
        self.connection.settimeout(3)
        self.connection.sendall(wire)
        deadline = time.monotonic() + 3
        while b'\n' not in self.buffer:
            left = deadline - time.monotonic()
            require(left > 0 and select.select([self.connection], [], [], left)[0], 'rate reply timeout')
            block = self.connection.recv(1025 - len(self.buffer))
            require(block, 'rate reply EOF')
            self.buffer.extend(block)
            require(len(self.buffer) <= 1024, 'oversized rate reply')
        line, _, remainder = self.buffer.partition(b'\n')
        require(not remainder, 'unsolicited rate reply')
        self.buffer.clear()
        reply = json.loads(line)
        self.replies.append({'request': value, 'reply': reply})
        require(set(reply) == {'version', 'request_id', 'result'} and reply['version'] == 1
                and reply['request_id'] == value['request_id'], 'rate response envelope')
        result = reply['result']
        fields = {'encoder_instance', 'epoch', 'sequence', 'target_bps', 'readback_bps',
                  'submitted_frames', 'hardware_verified'}
        if self.expect_window:
            fields |= {'data_rate_limit_bytes', 'data_rate_limit_seconds'}
        require(set(result) == fields, 'rate response fields')
        require(all(type(result[k]) is int for k in ('epoch', 'sequence', 'target_bps', 'readback_bps', 'submitted_frames')),
                'rate response integer fields')
        require(result['hardware_verified'] is True, 'hardware rate response')
        if self.expect_window:
            require(type(result['data_rate_limit_bytes']) is int and result['data_rate_limit_bytes'] == result['target_bps']*3//16
                    and type(result['data_rate_limit_seconds']) is int and result['data_rate_limit_seconds'] == 1,
                    'experimental window property readback')
        return result


def run(binary, out, codec, pattern, expect_window):
    report = {'codec': codec, 'pattern': pattern, 'seed': 20261009,
              'experimental_window_expected': expect_window,
              'passed': False, 'phases': [], 'forced_kill': False}
    records, errors = [], []
    changed = threading.Condition()
    process = None
    connection = None
    reader = None
    source_reader = None
    source_done = threading.Event()
    source_errors = []
    source_requests = []
    source_send = threading.Lock()
    stop_ack = False
    start_wall = time.monotonic()
    # Fixed seed, newly generated high-detail frames. This deliberately demands
    # bits at both targets; it is an actuator stress fixture, not a desktop model.
    rng = np.random.Generator(np.random.PCG64(20261009))
    elementary = out / f'{codec}.annexb'
    stderr_path = out / f'{codec}.stderr'
    try:
        with tempfile.TemporaryDirectory(prefix='rate-', dir=ROOT / '.local/run') as td, \
                stderr_path.open('xb') as stderr, elementary.open('xb') as stream:
            source_path = str(Path(td) / 'source')
            with socket.socket(socket.AF_UNIX) as listener:
                listener.bind(source_path)
                os.chmod(source_path, 0o600)
                listener.listen(1)
                listener.settimeout(8)
                control, child_control = socket.socketpair()
                with control, child_control:
                    rate = RateChannel(control, expect_window)
                    process = subprocess.Popen([str(binary), source_path, '--codec', codec,
                                                '--rate-control-fd', str(child_control.fileno())],
                                               pass_fds=(child_control.fileno(),), stdin=subprocess.PIPE,
                                               stdout=subprocess.PIPE, stderr=stderr)
                    child_control.close()

                    def read_output():
                        try:
                            while True:
                                head = process.stdout.read(13)
                                if not head:
                                    break
                                head += exact(process.stdout, 13-len(head))
                                length, pts, key = struct.unpack('>IQB', head)
                                require(0 < length <= 4*1024*1024 and key in (0, 1), 'AU header')
                                body = exact(process.stdout, length)
                                require(len(records) < 1024, 'AU count limit')
                                stream.write(body)
                                with changed:
                                    records.append({'pts_us': pts, 'keyframe': bool(key), 'bytes': length,
                                                    'sha256': hashlib.sha256(body).hexdigest()})
                                    changed.notify_all()
                        except Exception as error:
                            with changed:
                                errors.append(type(error).__name__ + ': ' + str(error))
                                changed.notify_all()

                    reader = threading.Thread(target=read_output, daemon=True)
                    reader.start()
                    connection, _ = listener.accept()
                    connection.settimeout(5)
                    source_commands = connection.makefile('rb', buffering=0)
                    start = json.loads(source_commands.readline(4097))
                    w, h = start['width'], start['height']
                    require(start == {'id': 'host-start', 'method': 'start', 'width': w, 'height': h, 'fps': 30}, 'source start')
                    require((w, h) == ((1280, 720) if codec == 'h264' else (1920, 1080)), 'geometry')
                    report['geometry'] = [w, h]
                    connection.sendall(event(event='started', id='host-start', width=w, height=h,
                                             pixel_format='nv12-video-range', color_matrix='ITU-R BT.709',
                                             color_space='ITU-R BT.709', capture_dynamic_range='SDR'))
                    # An encoder failure can request source cleanup before all
                    # fixture frames are sent. Always service that request;
                    # otherwise a failed encode would manufacture a stop timeout.
                    def answer_source_stop():
                        nonlocal stop_ack
                        pending = bytearray()
                        try:
                            while not source_done.is_set():
                                if not select.select([connection], [], [], .1)[0]:
                                    continue
                                block = connection.recv(4097-len(pending))
                                if not block:
                                    break
                                pending.extend(block)
                                require(len(pending) <= 4096, 'source command bound')
                                if b'\n' not in pending:
                                    continue
                                line, _, tail = pending.partition(b'\n')
                                require(not tail, 'extra source commands')
                                stop = json.loads(line)
                                source_requests.append(stop)
                                require(stop == {'id': 'host-stop', 'method': 'stop'}, 'source stop')
                                with source_send:
                                    connection.sendall(event(event='stopped', id='host-stop'))
                                stop_ack = True
                                break
                        except Exception as error:
                            source_errors.append(type(error).__name__ + ': ' + str(error))

                    source_reader = threading.Thread(target=answer_source_stop, daemon=True)
                    source_reader.start()
                    seq = 0

                    def send_frame():
                        nonlocal seq
                        seq += 1
                        if pattern == 'noise':
                            luma = rng.integers(16, 236, size=w*h, dtype=np.uint8)
                            chroma = rng.integers(80, 177, size=w*h//2, dtype=np.uint8)
                        else:
                            # Same320x180 changing luminance cells for both codecs,
                            # expanded to full output geometry. Constant neutral
                            # chroma isolates temporal/spatial luma complexity.
                            cell = w//320
                            luma = np.repeat(np.repeat(rng.integers(16, 236, size=(180, 320), dtype=np.uint8),
                                                      cell, axis=0), cell, axis=1).reshape(-1)
                            chroma = np.full(w*h//2, 128, dtype=np.uint8)
                        body = b'\x02' + struct.pack('>QQIIII', seq, seq*33333333, w, h, w*h, w*h//2) + luma.tobytes() + chroma.tobytes()
                        with source_send:
                            connection.sendall(struct.pack('>I', len(body)) + body)
                        deadline = time.monotonic()+5
                        with changed:
                            while len(records) < seq and not errors and process.poll() is None and time.monotonic() < deadline:
                                changed.wait(min(.05, max(0, deadline-time.monotonic())))
                            require(not errors and len(records) == seq, 'one actual AU per moving source frame')

                    send_frame()  # Creates the actual required-hardware session.
                    # Protocol calls are deliberately adjacent to the measured
                    # source boundary. No reconnect, keyframe or encoder restart.
                    status = rate.call({'version': 1, 'request_id': 'status', 'method': 'rate_status'})
                    report['initial_status'] = status
                    identity = {k: status[k] for k in ('encoder_instance', 'epoch')}
                    for number, target in enumerate((12000000, 2000000, 12000000), 1):
                        request = {'version': 1, 'request_id': f'rate-{number}', 'method': 'set_bitrate',
                                   **identity, 'sequence': number, 'target_bps': target}
                        reply = rate.call(request)
                        require(all(reply.get(k) == v for k, v in identity.items()), 'rate identity')
                        require(reply['target_bps'] == target and reply['readback_bps'] == target
                                and reply['sequence'] == number and reply['submitted_frames'] == len(records), 'rate application/boundary')
                        phase = {'target_bps': target, 'ack': reply, 'first_record': len(records),
                                 'settling_frames': 60, 'measurement_frames': 120}
                        report['phases'].append(phase)
                        begin = time.monotonic()
                        for index in range(180):
                            delay = begin+index/30-time.monotonic()
                            if delay > 0:
                                time.sleep(delay)
                            send_frame()
                        selected = records[phase['first_record']+60:phase['first_record']+180]
                        duration = (selected[-1]['pts_us']-selected[0]['pts_us'])/1e6+1/30
                        phase.update(elapsed_seconds=time.monotonic()-begin, measured_seconds=duration,
                                     measured_bytes=sum(x['bytes'] for x in selected),
                                     measured_bps=sum(x['bytes'] for x in selected)*8/duration)
                    final_status = rate.call({'version': 1, 'request_id': 'final-status', 'method': 'rate_status'})
                    require(all(final_status[k] == v for k, v in identity.items()) and final_status['sequence'] == 3
                            and final_status['submitted_frames'] == len(records), 'same encoder final status')
                    report['final_status'] = final_status
                    report['rate_exchange'] = rate.replies
                    process.stdin.write(b'stop\n')
                    process.stdin.flush()
                    report['exit_code'] = process.wait(timeout=8)
                    source_reader.join(timeout=2)
                    require(stop_ack and not source_errors and not source_reader.is_alive(), 'source stop acknowledged')
                    reader.join(timeout=3)
                    require(not reader.is_alive() and not errors, 'AU reader clean EOF')
                    stream.flush()
                    os.fsync(stream.fileno())
                    source_commands.close()
            require(report['exit_code'] == 0, 'encoder exit')
        diagnostics = []
        for line in stderr_path.read_text().splitlines():
            try:
                diagnostics.append(json.loads(line))
            except json.JSONDecodeError:
                pass
        require(diagnostics, 'encoder diagnostics missing')
        report['summary'] = diagnostics[-1]
        require(report['summary']['hardware_verified'] and report['summary']['guest_cleanup_confirmed'], 'hardware/cleanup verification')
        require(records[0]['pts_us'] == 0 and all(b['pts_us'] > a['pts_us'] for a, b in zip(records, records[1:])), 'continuous PTS')
        decoded = subprocess.run(['ffmpeg', '-v', 'error', '-i', str(elementary), '-f', 'framemd5', '-'],
                                 capture_output=True, timeout=60, check=True)
        (out/f'{codec}-decoded.framemd5').write_bytes(decoded.stdout)
        require(not decoded.stderr, 'decoder errors')
        hashes = [line.rsplit(b',', 1)[-1].strip() for line in decoded.stdout.splitlines() if line and not line.startswith(b'#')]
        require(len(hashes) == len(records) and len(set(hashes)) == len(hashes), 'complete changing decoded frames')
        report['decoded_frames'] = len(hashes)
        high, low, restored = [p['measured_bps'] for p in report['phases']]
        report['rate_ratios'] = {'high_over_low': high/low, 'restored_over_low': restored/low}
        require(high/low > 1.4 and restored/low > 1.4, 'actual output did not decrease and recover')
        report['passed'] = True
    except Exception as error:
        report['error'] = type(error).__name__ + ': ' + str(error)[:400]
    finally:
        if process:
            if process.poll() is None:
                try:
                    process.stdin.write(b'stop\n')
                    process.stdin.flush()
                    process.wait(timeout=3)
                except (OSError, subprocess.TimeoutExpired):
                    report['forced_kill'] = True
                    process.kill()
                    process.wait(timeout=3)
            report['owned_child_reaped'] = process.poll() is not None
            report.setdefault('exit_code', process.returncode)
            for pipe in (process.stdin, process.stdout):
                if pipe and not pipe.closed:
                    pipe.close()
        source_done.set()
        if source_reader:
            source_reader.join(timeout=2)
        if connection:
            connection.close()
        if reader:
            reader.join(timeout=3)
        report.update(source_stop_ack_sent=stop_ack, records=records, reader_errors=errors,
                      source_requests=source_requests, source_errors=source_errors,
                      total_seconds=time.monotonic()-start_wall)
        (out/f'{codec}-result.json').write_text(json.dumps(report, indent=2)+'\n')
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--pattern', choices=('noise', 'blocks'), default='noise',
                        help='Retain noise stress test; blocks adds a separate lower spatial-complexity control.')
    parser.add_argument('--expect-data-rate-limit', action='store_true',
                        help='Require separate experimental dual-property schema; never assumes production supports it.')
    args = parser.parse_args()
    os.umask(0o077)
    args.output.mkdir(parents=True, exist_ok=False)
    (ROOT/'.local/run').mkdir(exist_ok=True)
    report = {'scope': __doc__, 'binary_sha256': hashlib.sha256(args.binary.read_bytes()).hexdigest(), 'codecs': []}
    for codec in ('h264', 'hevc'):
        result = run(args.binary.resolve(), args.output, codec, args.pattern, args.expect_data_rate_limit)
        report['codecs'].append({'codec': codec, 'passed': result['passed']})
        print(json.dumps(report['codecs'][-1]), flush=True)
    report['passed'] = all(c['passed'] for c in report['codecs'])
    (args.output/'result.json').write_text(json.dumps(report, indent=2)+'\n')
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
