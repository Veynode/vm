#!/usr/bin/env python3
"""Actual authorized Guest stream faults through its private host Unix bridge.

Never boots/stops a VM or modifies the Guest. Saves bounded metadata only, not
screen pixels. Use only when the coordinator has released the shared bridge.
"""
import argparse
import hashlib
import json
import os
import pathlib
import select
import socket
import stat
import struct
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
MAX_FRAME = 33 + 1920 * 1080 * 3 // 2


class Client:
    def __init__(self, path, small_buffer=False):
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(3)
        if small_buffer:
            self.socket.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
        self.socket.connect(str(path))
        self.sequence = self.pts = -1
        self.frames = 0

    def close(self):
        self.socket.close()

    def send(self, value):
        self.socket.sendall(json.dumps(value, separators=(',', ':')).encode() + b'\n')

    def exact(self, size, deadline):
        value = bytearray()
        while len(value) < size:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError('record deadline')
            self.socket.settimeout(remaining)
            chunk = self.socket.recv(size - len(value))
            if not chunk:
                raise EOFError(f'partial record {len(value)}/{size}')
            value.extend(chunk)
        return bytes(value)

    def record(self, deadline):
        length = struct.unpack('>I', self.exact(4, deadline))[0]
        assert 1 <= length <= MAX_FRAME, ('record_length', length)
        body = self.exact(length, deadline)
        if body[0] == 1:
            assert length <= 65536
            return 'event', json.loads(body[1:])
        assert body[0] == 2 and length >= 33
        seq, pts, width, height, y_bytes, uv_bytes = struct.unpack('>QQIIII', body[1:33])
        assert (width, height) == (1920, 1080), (width, height)
        assert y_bytes == width * height and uv_bytes == y_bytes // 2
        assert length == 33 + y_bytes + uv_bytes
        assert seq > self.sequence and pts > self.pts, ('nonadvancing_sample', seq, pts)
        self.sequence, self.pts = seq, pts
        self.frames += 1
        return 'frame', {'sequence': seq, 'pts_ns': pts, 'width': width, 'height': height,
                         'sha256': hashlib.sha256(body[33:]).hexdigest()}

    def start(self, request_id, deadline=None):
        self.send({'id': request_id, 'method': 'start', 'width': 1920, 'height': 1080, 'fps': 30})
        kind, event = self.record(deadline if deadline is not None else time.monotonic() + 6)
        assert kind == 'event' and event.get('event') == 'started' and event.get('id') == request_id, event
        assert (event.get('width'), event.get('height'), event.get('pixel_format')) == (1920, 1080, 'nv12-video-range'), event
        return event

    def frame(self):
        kind, frame = self.record(time.monotonic() + 6)
        assert kind == 'frame', frame
        return frame

    def stop(self, request_id):
        began = time.monotonic()
        self.send({'id': request_id, 'method': 'stop'})
        deadline = began + 3
        while True:
            kind, value = self.record(deadline)
            if kind == 'event':
                assert value.get('event') == 'stopped' and value.get('id') == request_id, value
                break
        self.socket.settimeout(1)
        assert self.socket.recv(1) == b'', 'bytes after stopped ACK'
        return {'ack': value, 'ack_elapsed_ms': round((time.monotonic() - began) * 1000, 2)}


def reconnect(path, label):
    """Finite recovery window; preserve every rejection rather than hiding it."""
    began, rejected = time.monotonic(), []
    while time.monotonic() - began < 6:
        client = None
        try:
            client = Client(path)
            client.start(label, began + 6)
            return client, {'rejected_attempts': rejected, 'reconnect_elapsed_ms': round((time.monotonic() - began) * 1000, 2)}
        except (EOFError, ConnectionError, OSError) as error:
            if client:
                client.close()
            rejected.append({'elapsed_ms': round((time.monotonic() - began) * 1000, 2), 'error': type(error).__name__})
            time.sleep(.1)
    raise AssertionError(('reconnect_deadline', rejected))


def roundtrip(path, label):
    client, result = reconnect(path, label)
    try:
        result['first_frame'] = client.frame()
        result['stop'] = client.stop(label + '-stop')
        return result
    finally:
        client.close()


def midframe(path):
    client, result = reconnect(path, 'midframe')
    try:
        deadline = time.monotonic() + 6
        length = struct.unpack('>I', client.exact(4, deadline))[0]
        assert length == MAX_FRAME, length
        head = client.exact(4096, deadline)
        assert head[0] == 2
        result.update(frame_body_bytes=length, bytes_read_before_disconnect=len(head))
    finally:
        client.close()
    result['recovery'] = roundtrip(path, 'after-midframe')
    return result


def slow_reader(path):
    client = Client(path, small_buffer=True)
    try:
        client.start('slow-reader')
        # EV_EOF detects peer closure without draining queued screenshot bytes.
        # The host bridge has its own500ms write deadline; this test cannot
        # attribute the closure specifically to the Guest's2s writer deadline.
        began = time.monotonic()
        queue = select.kqueue()
        try:
            change = select.kevent(client.socket.fileno(), filter=select.KQ_FILTER_READ,
                                   flags=select.KQ_EV_ADD | select.KQ_EV_CLEAR)
            events = queue.control([change], 1, 0)
            while not any(item.flags & select.KQ_EV_EOF for item in events):
                remaining = 2 - (time.monotonic() - began)
                assert remaining > 0, 'slow reader did not observe EOF within2s'
                events = queue.control([], 1, remaining)
            elapsed = round((time.monotonic() - began) * 1000, 2)
        finally:
            queue.close()
        # Drain only after EOF evidence; no image body is written to disk.
        queued = 0
        client.socket.settimeout(1)
        while True:
            chunk = client.socket.recv(65536)
            if not chunk:
                break
            queued += len(chunk)
            assert queued <= 8 * 1024 * 1024
    finally:
        client.close()
    return {'peer_eof_ms': elapsed, 'queued_bytes_after_eof': queued,
            'deadline_attribution': 'combined bridge+Guest; bridge500ms may fire first',
            'recovery': roundtrip(path, 'after-slow-reader')}


def wrong_start(path):
    client = Client(path)
    try:
        began = time.monotonic()
        client.send({'id': 'invalid', 'method': 'start', 'width': True, 'height': 1080, 'fps': 30})
        kind, value = client.record(began + 2)
        assert kind == 'event' and value.get('event') == 'error' and value.get('code') == 'invalid_start', value
        client.socket.settimeout(1)
        assert client.socket.recv(1) == b''
        result = {'event': value, 'error_and_eof_ms': round((time.monotonic() - began) * 1000, 2)}
    finally:
        client.close()
    result['recovery'] = roundtrip(path, 'after-invalid')
    return result


def second_client(path):
    first, result = reconnect(path, 'first-owner')
    second = None
    try:
        result['first_frame'] = first.frame()
        # Keep draining owner frames while waiting for the competing connection
        # so the test itself does not turn owner backpressure into the fault.
        second = Client(path)
        began = time.monotonic()
        try:
            second.send({'id': 'second-owner', 'method': 'start', 'width': 1920, 'height': 1080, 'fps': 30})
        except (BrokenPipeError, ConnectionResetError) as error:
            # The exclusive bridge may reject before any request is written.
            # This is valid only for the competing-client negative; the first
            # owner must still receive its matching stopped ACK below.
            result['rejection'] = 'pre-send-' + type(error).__name__
        while 'rejection' not in result:
            remaining = 1 - (time.monotonic() - began)
            assert remaining > 0, 'second client rejection deadline'
            readable, _, _ = select.select([first.socket, second.socket], [], [], remaining)
            if second.socket in readable:
                try:
                    assert second.socket.recv(1) == b'', 'second client acquired stream'
                    result['rejection'] = 'EOF'
                except ConnectionResetError:
                    result['rejection'] = 'reset'
                break
            if first.socket in readable:
                kind, value = first.record(time.monotonic() + 2)
                assert kind == 'frame', value
        result['second_rejected_ms'] = round((time.monotonic() - began) * 1000, 2)
        result['first_owner_stop'] = first.stop('first-owner-stop')
        result['scope'] = 'private bridge exclusivity; not direct independent CID2 Guest listener test'
    finally:
        first.close()
        if second:
            second.close()
    result['recovery'] = roundtrip(path, 'after-second-client')
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--socket', type=pathlib.Path, default=ROOT / '.local/macos-stream/stream.sock')
    parser.add_argument('--output', type=pathlib.Path, required=True)
    args = parser.parse_args()
    metadata = args.socket.lstat()
    assert stat.S_ISSOCK(metadata.st_mode) and metadata.st_uid == os.getuid()
    assert metadata.st_mode & 0o077 == 0, 'bridge socket must be owner-only'
    args.output.parent.mkdir(parents=True, exist_ok=True)
    # Exclusive output ensures earlier failures cannot be silently replaced.
    with args.output.open('x') as output:
        report = {'scope': 'actual authorized Guest SCK via private host Unix-vsock bridge; no stored pixels',
                  'passed': False, 'cases': [], 'limitations': ['No direct Guest PID/RSS/FD instrumentation; reconnect+ACK do not prove absence of resource leaks.',
                                                               'Slow-reader closure includes bridge500ms deadline; Guest2s deadline isolated only by synthetic unit test.']}
        try:
            for name, test in [('baseline', lambda path: roundtrip(path, 'baseline')),
                               ('disconnect_midframe', midframe), ('slow_reader', slow_reader),
                               ('invalid_start', wrong_start), ('second_client', second_client)]:
                entry = {'name': name, 'passed': False}
                report['cases'].append(entry)
                try:
                    entry['result'] = test(args.socket)
                    entry['passed'] = True
                    print('PASS', name, flush=True)
                except Exception as error:
                    entry['error'] = str(error)
                    entry['error_type'] = type(error).__name__
                    raise
            report['sequential_recovery_cycles'] = []
            for i in range(5):
                cycle = {'index': i, 'passed': False}
                report['sequential_recovery_cycles'].append(cycle)
                try:
                    cycle['result'] = roundtrip(args.socket, f'cycle-{i}')
                    cycle['passed'] = True
                except Exception as error:
                    cycle.update(error=str(error), error_type=type(error).__name__)
                    raise
            report['passed'] = True
        finally:
            json.dump(report, output, indent=2)
            output.write('\n')


if __name__ == '__main__':
    main()
