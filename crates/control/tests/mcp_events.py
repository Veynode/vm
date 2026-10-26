#!/usr/bin/env python3
"""Pushed MCP events through real vm-control mcp (stdio) and vm-mcp-http (TLS).

The daemon is a synthetic Unix-socket fixture scripted by this file: no VM,
Guest or screen. It proves the protocol: capabilities, resources, subscribe and
notifications/resources/updated, progress, principal filtering and bounds.
Usage: mcp_events.py VM_CONTROL stdio | mcp_events.py VM_CONTROL https VM_MCP_HTTP
"""
import base64
import hashlib
import http.client
import json
import queue
import secrets
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

CONTROL = str(Path(sys.argv[1]).resolve())
MODE = sys.argv[2]
HTTP = str(Path(sys.argv[3]).resolve()) if MODE == 'https' else None
JOB, WAITED = 'a' * 32, 'b' * 32
checks = []

def check(name, condition, detail=''):
    if not condition:
        raise AssertionError(f'{name}: {detail}')
    checks.append(name)

class Daemon:
    """Answers the daemon methods the MCP adapter uses; the test drives state."""
    def __init__(self, path):
        self.lock = threading.Lock()
        self.events, self.next = [], 1
        self.log_size = 10
        self.job_done = False
        self.waited_until = None
        self.screen = threading.Event()
        self.calls = []
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.bind(str(path / 'control.sock'))
        self.socket.listen(32)
        self.socket.settimeout(.1)
        self.stop = False
        threading.Thread(target=self.run, daemon=True).start()
    def emit(self, vm, event, **fields):
        with self.lock:
            self.events.append(dict(seq=self.next, vm=vm, event=event, **fields))
            self.next += 1
    def run(self):
        while not self.stop:
            try:
                peer, _ = self.socket.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            threading.Thread(target=self.handle, args=(peer,), daemon=True).start()
    def answer(self, method, args):
        with self.lock:
            self.calls.append(method)
        vm = args.get('id')
        if vm not in (None, 'allowed', 'other'):
            return {'error': 'VM not found'}
        if method == 'session_end':
            empty = {'attempted': 0, 'unconfirmed_requests': 0, 'outcomes': []}
            return {'result': {'closed': True, 'job_cancellation': empty, 'file_cancellation': empty}}
        if method == 'list':
            return {'result': [{'config': {'id': 'allowed'}}, {'config': {'id': 'other'}}]}
        if method == 'status':
            return {'result': {'config': {'id': vm, 'disk': '/Users/secret/disk.img'}, 'state': 'running', 'pid': 4242,
                               'runtime': {'state': 'running', 'serial_log': '/Users/secret/serial.log'}}}
        if method == 'events':
            with self.lock:
                if 'after' not in args:
                    return {'result': {'events': [], 'next': self.next, 'lost': False}}
                return {'result': {'events': [e for e in self.events if e['seq'] > args['after']], 'next': self.next, 'lost': False}}
        if method == 'log_read':
            with self.lock:
                return {'result': {'size': self.log_size, 'text': 'serial line\n' if args.get('max_bytes', 1) else '', 'truncated': False, 'exists': True, 'stream': args['stream']}}
        if method == 'exec_poll':
            if args['job_id'] == JOB:
                done = self.job_done
            elif args['job_id'] == WAITED:
                done = self.waited_until is not None and time.monotonic() >= self.waited_until
            else:
                return {'error': 'job is unknown or expired on this VM'}
            if not done:
                return {'result': {'id': '1', 'result': {'state': 'running'}}}
            return {'result': {'id': '1', 'result': {'state': 'finished', 'result': {'exit_code': 3, 'stdout': 'out', 'stderr': '', 'timed_out': False, 'cancelled': False}}}}
        if method == 'screen_wait':
            changed = self.screen.wait(min(args['timeout_ms'], 1000) / 1000)
            self.screen.clear()
            return {'result': {'changed': changed, 'settled': False, 'resized': False}}
        if method == 'screenshot':
            return {'result': {'data_base64': base64.b64encode(b'\x89PNG fixture').decode(), 'mime_type': 'image/png', 'frame_id': 'f' * 36, 'width': 1, 'height': 1}}
        return {'error': f'unexpected {method}'}
    def handle(self, peer):
        with peer:
            peer.settimeout(5)
            request = json.loads(peer.makefile('rb').readline(1048576))
            try:
                peer.sendall(json.dumps(self.answer(request['method'], request.get('arguments') or {})).encode() + b'\n')
            except (BrokenPipeError, ConnectionResetError):
                pass
    def close(self):
        self.stop = True
        self.socket.close()

class Stdio:
    def __init__(self, case):
        self.child = subprocess.Popen([CONTROL, 'mcp', '--state-dir', str(case), '--allow-execute'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.inbox = queue.Queue()
        self.notifications = queue.Queue()
        threading.Thread(target=self.read, daemon=True).start()
    def read(self):
        for line in self.child.stdout:
            message = json.loads(line)
            (self.inbox if 'id' in message and 'method' not in message else self.notifications).put(message)
    def send(self, message):
        self.child.stdin.write(json.dumps(message).encode() + b'\n')
        self.child.stdin.flush()
    def request(self, message):
        """The response, and the notifications written before it."""
        self.send(message)
        response = self.inbox.get(timeout=40)
        assert response['id'] == message['id'], response
        return response, None
    def close(self):
        self.child.stdin.close()
        return self.child.wait(timeout=10)

class Https:
    def __init__(self, case, principal_scopes):
        self.case = case
        cert, key = case / 'cert.pem', case / 'key.pem'
        subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-sha256', '-keyout', str(key), '-out', str(cert), '-days', '1', '-subj', '/CN=localhost', '-addext', 'subjectAltName=DNS:localhost,IP:127.0.0.1'], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        key.chmod(0o600)
        self.context = ssl.create_default_context(cafile=str(cert))
        self.token = secrets.token_hex(32)
        with socket.socket() as reservation:
            reservation.bind(('127.0.0.1', 0))
            self.port = reservation.getsockname()[1]
        config = case / 'config.json'
        config.write_text(json.dumps({'bind': f'127.0.0.1:{self.port}', 'tls_cert': str(cert), 'tls_key': str(key),
            'principals': [{'name': 'agent', 'token_sha256': hashlib.sha256(self.token.encode()).hexdigest(), 'scopes': principal_scopes, 'vm_ids': ['allowed']}]}))
        config.chmod(0o600)
        self.server = subprocess.Popen([HTTP, '--config', str(config), '--state-dir', str(case), '--control-bin', CONTROL], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.sid = None
        self.notifications = queue.Queue()
        self.stream_ended = threading.Event()
        end = time.monotonic() + 10
        while time.monotonic() < end:
            try:
                if self.http('GET', authenticated=False)[0] == 401:
                    break
            except OSError:
                time.sleep(.05)
        else:
            raise AssertionError('HTTPS server did not listen')
    def connection(self, seconds=48):
        return http.client.HTTPSConnection('127.0.0.1', self.port, context=self.context, timeout=seconds)
    def headers(self, accept='application/json, text/event-stream', authenticated=True):
        headers = {'Accept': accept, 'Content-Type': 'application/json', 'MCP-Protocol-Version': '2025-06-18'}
        if authenticated:
            headers['Authorization'] = 'Bearer ' + self.token
        if self.sid:
            headers['MCP-Session-Id'] = self.sid
        return headers
    def http(self, method, payload=None, authenticated=True, accept='application/json, text/event-stream'):
        connection = self.connection()
        try:
            connection.request(method, '/mcp', json.dumps(payload).encode() if payload is not None else b'', self.headers(accept, authenticated))
            response = connection.getresponse()
            return response.status, dict(response.getheaders()), response.read()
        finally:
            connection.close()
    def send(self, message):
        status, _, _ = self.http('POST', message)
        assert status == 202, status
    def request(self, message):
        status, headers, body = self.http('POST', message)
        assert status == 200, (status, body)
        if message['method'] == 'initialize':
            self.sid = headers['mcp-session-id']
        if headers.get('content-type', '').startswith('text/event-stream'):
            frames = [json.loads(line[6:]) for line in body.decode().split('\n') if line.startswith('data: ')]
            return frames[-1], frames[:-1]
        return json.loads(body), None
    def listen(self):
        """The session's GET stream, read into self.notifications."""
        connection = self.connection(seconds=120)
        connection.request('GET', '/mcp', headers=self.headers('text/event-stream'))
        response = connection.getresponse()
        assert response.status == 200 and response.getheader('content-type') == 'text/event-stream', response.status
        def read():
            try:
                while True:
                    line = response.readline()
                    if not line:
                        self.stream_ended.set()
                        return
                    if line.startswith(b'data: '):
                        self.notifications.put(json.loads(line[6:]))
            except (OSError, http.client.HTTPException, ValueError, AttributeError):
                return
        threading.Thread(target=read, daemon=True).start()
        return connection
    def close(self):
        status = self.http('DELETE')[0]
        self.server.terminate()
        return 0 if status == 204 and self.server.wait(timeout=48) == 0 else 1

def updates(client, uri, seconds=6, count=1):
    """Waits for `count` resources/updated notifications for uri."""
    found, end = [], time.monotonic() + seconds
    while len(found) < count and time.monotonic() < end:
        try:
            message = client.notifications.get(timeout=max(.01, end - time.monotonic()))
        except queue.Empty:
            break
        if message.get('method') == 'notifications/resources/updated' and message['params']['uri'] == uri:
            found.append(message['params']['_meta']['veynode.dev/event'])
    return found

def drain(client):
    drained = []
    while True:
        try:
            drained.append(client.notifications.get_nowait())
        except queue.Empty:
            return drained

def scenario(root):
    case = root / MODE
    case.mkdir(mode=0o700)
    daemon = Daemon(case)
    client = Stdio(case) if MODE == 'stdio' else Https(case, ['observe', 'execute'])
    rid = iter(range(1, 1000))
    def call(method, params=None, meta=None):
        message = {'jsonrpc': '2.0', 'id': next(rid), 'method': method}
        if params is not None:
            message['params'] = params
        if meta:
            message.setdefault('params', {})['_meta'] = meta
        return client.request(message)
    try:
        init, _ = call('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 'events', 'version': '1'}})
        capabilities = init['result']['capabilities']
        check('initialize advertises subscribable resources and keeps tools', capabilities.get('resources') == {'subscribe': True, 'listChanged': False} and 'tools' in capabilities, capabilities)
        check('instructions describe events', 'veynode.dev/event' in init['result']['instructions'])
        client.send({'jsonrpc': '2.0', 'method': 'notifications/initialized'})
        tools, _ = call('tools/list')
        poll = next(t for t in tools['result']['tools'] if t['name'] == 'vm_exec_poll')
        check('polling tools remain and vm_exec_poll gains wait_ms', 'wait_ms' in poll['inputSchema']['properties'] and any(t['name'] == 'vm_screen_wait' for t in tools['result']['tools']))
        listed, _ = call('resources/list')
        uris = {r['uri'] for r in listed['result']['resources']}
        check('resources/list names status, screen and logs', {'veynode://vm/allowed/status', 'veynode://vm/allowed/screen', 'veynode://vm/allowed/log/serial', 'veynode://vm/allowed/log/worker'} <= uris, uris)
        templates, _ = call('resources/templates/list')
        check('job template is offered with execute', any('job' in t['uriTemplate'] for t in templates['result']['resourceTemplates']))
        status, _ = call('resources/read', {'uri': 'veynode://vm/allowed/status'})
        text = status['result']['contents'][0]['text']
        screen, _ = call('resources/read', {'uri': 'veynode://vm/allowed/screen'})
        check('screen resource is the screenshot blob', base64.b64decode(screen['result']['contents'][0]['blob']) == b'\x89PNG fixture' and screen['result']['contents'][0]['mimeType'] == 'image/png')
        log, _ = call('resources/read', {'uri': 'veynode://vm/allowed/log/serial'})
        check('log resource is text', log['result']['contents'][0]['text'] == 'serial line\n')
        unknown, _ = call('resources/read', {'uri': 'veynode://vm/../status'})
        check('unknown resource URIs are errors', 'error' in unknown, unknown)
        if MODE == 'https':
            check('remote status resource hides host paths and pid', '/Users/secret' not in text and '4242' not in text and 'running' in text, text)
            check('resources/list is filtered to the allowlist', not any('/other/' in u for u in uris), uris)
            for method in ('resources/read', 'resources/subscribe'):
                denied, _ = call(method, {'uri': 'veynode://vm/other/status'})
                check(f'{method} outside the allowlist is refused', denied.get('error', {}).get('code') == -32602, denied)
            # Slow client: no GET stream yet, a burst of events is bounded.
            call('resources/subscribe', {'uri': 'veynode://vm/allowed/status'})
            for index in range(400):
                daemon.emit('allowed', 'state', state=f's{index}')
            time.sleep(2)
            refused = client.http('GET', accept='application/json')[0]
            check('GET requires text/event-stream', refused == 406, refused)
            stream = client.listen()
            first = client.notifications.get(timeout=5)
            check('a slow client loses the oldest events and is told so', first['method'] == 'notifications/veynode/events_dropped' and first['params']['dropped'] >= 400 - 256, first)
            time.sleep(1)
            drain(client)
        else:
            check('local status resource is vm_status', json.loads(text)['state'] == 'running')
            call('resources/subscribe', {'uri': 'veynode://vm/allowed/status'})
        subscribed, _ = call('resources/subscribe', {'uri': 'veynode://vm/allowed/screen'})
        check('screen subscribe succeeds', subscribed.get('result') == {}, subscribed)
        for uri in ('veynode://vm/allowed/log/serial', f'veynode://vm/allowed/job/{JOB}'):
            subscribed, _ = call('resources/subscribe', {'uri': uri})
            check(f'subscribe {uri.split("/")[4]}', subscribed.get('result') == {}, subscribed)
        expired, _ = call('resources/subscribe', {'uri': 'veynode://vm/allowed/job/' + 'c' * 32})
        check('subscribing to an unknown job fails', 'error' in expired, expired)
        daemon.emit('other', 'started')
        daemon.emit('allowed', 'crashed', state='crashed', exit_code=1)
        lifecycle = updates(client, 'veynode://vm/allowed/status')
        check('lifecycle change arrives as resources/updated', lifecycle and lifecycle[0]['event'] == 'crashed' and lifecycle[0]['exit_code'] == 1, lifecycle)
        with daemon.lock:
            daemon.log_size = 99
        appended = updates(client, 'veynode://vm/allowed/log/serial')
        check('log growth arrives with its size', appended == [{'event': 'appended', 'size': 99}], appended)
        daemon.job_done = True
        finished = updates(client, f'veynode://vm/allowed/job/{JOB}')
        check('job exit arrives with exit_code', finished and finished[0]['event'] == 'finished' and finished[0]['exit_code'] == 3, finished)
        time.sleep(.3)
        daemon.screen.set()
        screen_events = updates(client, 'veynode://vm/allowed/screen', count=2)
        check('screen changed then settled', [e['event'] for e in screen_events] == ['changed', 'settled'], screen_events)
        daemon.screen.set()
        started = time.monotonic()
        rate = updates(client, 'veynode://vm/allowed/screen', count=1)
        check('screen changes are at most one a second', rate and rate[0]['event'] == 'changed', rate)
        updates(client, 'veynode://vm/allowed/screen', count=1)
        check('rate limit spacing', time.monotonic() - started >= .5, time.monotonic() - started)
        call('resources/unsubscribe', {'uri': 'veynode://vm/allowed/screen'})
        time.sleep(1.5)
        drain(client)
        daemon.screen.set()
        check('no screen events after unsubscribe', updates(client, 'veynode://vm/allowed/screen', seconds=2.5) == [])
        daemon.waited_until = time.monotonic() + 2.5
        reply, streamed = call('tools/call', {'name': 'vm_exec_poll', 'arguments': {'id': 'allowed', 'job_id': WAITED, 'wait_ms': 8000}}, meta={'progressToken': 'p1'})
        result = json.loads(reply['result']['content'][0]['text'])
        check('vm_exec_poll wait_ms returns the finished job', result['result']['state'] == 'finished', result)
        progress = streamed if MODE == 'https' else [m for m in drain(client) if m.get('method') == 'notifications/progress']
        check('progress notifications precede the result', len(progress) >= 1 and all(p['params']['progressToken'] == 'p1' for p in progress), progress)
        check('progress increases', [p['params']['progress'] for p in progress] == sorted(p['params']['progress'] for p in progress))
        reply, _ = call('tools/call', {'name': 'vm_exec_poll', 'arguments': {'id': 'allowed', 'job_id': WAITED}})
        check('plain vm_exec_poll still answers at once', json.loads(reply['result']['content'][0]['text'])['result']['state'] == 'finished')
        if MODE == 'https':
            leaked = [m for m in drain(client) if 'other' in json.dumps(m)]
            check('no event for a VM outside the allowlist', leaked == [], leaked)
        check('session ends cleanly', client.close() == 0)
        if MODE == 'https':
            check('the GET stream ends with its session', client.stream_ended.wait(5))
            stream.close()
        check('session_end reached the daemon', 'session_end' in daemon.calls)
    finally:
        if MODE == 'https' and client.server.poll() is None:
            client.server.kill()
        if MODE == 'stdio' and client.child.poll() is None:
            client.child.kill()
        daemon.close()

with tempfile.TemporaryDirectory(prefix='vm-events-', dir='/tmp') as tmp:
    scenario(Path(tmp))
print(json.dumps({'passed': True, 'transport': MODE, 'checks': checks}))
