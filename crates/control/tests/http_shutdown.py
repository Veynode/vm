#!/usr/bin/env python3
"""Real TLS/HTTP + real vm-control subprocess teardown, synthetic Unix daemon only.
No Guest or VM acceptance; no TLS-verification bypass. Signals target owned children.
"""
import concurrent.futures
import hashlib
import http.client
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time

HTTP, CONTROL = map(lambda p: str(Path(p).resolve()), sys.argv[1:3])
GRAPHICS_ONLY = sys.argv[3:] == ['--graphics-only']
checks = []

def wait_until(predicate, seconds=8):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        if predicate():
            return
        time.sleep(.02)
    raise AssertionError('bounded wait failed')

class Daemon:
    def __init__(self, path, mode):
        self.mode = mode
        self.calls = []
        self.lock = threading.Lock()
        self.started = threading.Event()
        self.cleaned = threading.Event()
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.bind(str(path / 'control.sock'))
        self.socket.listen(32)
        self.socket.settimeout(.1)
        self.stop = False
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()
    def run(self):
        while not self.stop:
            try:
                peer, _ = self.socket.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            threading.Thread(target=self.handle, args=(peer,), daemon=True).start()
    def handle(self, peer):
        with peer:
            peer.settimeout(5)
            request = json.loads(peer.makefile('rb').readline(1048576))
            method = request['method']
            if method == 'session_end':
                with self.lock:
                    self.calls.append(request['arguments']['owner'])
                time.sleep(.08) # Exercise monitor/finalizer overlap.
                self.cleaned.set()
                result = {'closed': True, 'retired_owner': True,
                          'job_cancellation': {'attempted': 0, 'unconfirmed_requests': 0, 'outcomes': []},
                          'file_cancellation': {'attempted': 0, 'unconfirmed_requests': 0, 'outcomes': []}}
                if self.mode == 'file_failure':
                    result['file_cancellation'] = {'attempted': 1, 'unconfirmed_requests': 0, 'outcomes': [{'error': 'fixture cleanup failed'}]}
                if self.mode == 'malformed_file':
                    result['file_cancellation'] = {'attempted': 1, 'unconfirmed_requests': 0, 'outcomes': [{'result': {'result': {}}}]}
                if self.mode == 'input_failure':
                    result['input_release'] = {'confirmed': False}
                if self.mode == 'job_failure':
                    result['job_cancellation'] = {'attempted': 1, 'unconfirmed_requests': 1, 'outcomes': []}
                response = {'result': result}
            else:
                self.started.set()
                if self.mode == 'inflight':
                    assert self.cleaned.wait(5), 'stdin EOF did not interrupt in-flight request'
                response = {'result': {'result': {'fixture': True}}}
            try:
                peer.sendall(json.dumps(response).encode() + b'\n')
            except (BrokenPipeError, ConnectionResetError):
                pass
    def close(self):
        self.stop = True
        self.thread.join(1)
        self.socket.close()

with tempfile.TemporaryDirectory(prefix='vm-http-stop-', dir='/tmp') as tmp:
    root = Path(tmp)
    cert, key = root/'cert.pem', root/'key.pem'
    subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-sha256', '-keyout', str(key), '-out', str(cert), '-days', '1', '-subj', '/CN=localhost', '-addext', 'subjectAltName=DNS:localhost,IP:127.0.0.1'], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    key.chmod(0o600)
    context = ssl.create_default_context(cafile=str(cert))
    def scenario(sig, mode, sessions=1, end='signal'):
        case = root / f'{sig}-{mode}-{end}-{sessions}'
        case.mkdir(mode=0o700)
        daemon = Daemon(case, mode)
        token = secrets.token_hex(32)
        with socket.socket() as reservation:
            reservation.bind(('127.0.0.1', 0))
            port = reservation.getsockname()[1]
        config = case/'config.json'
        config.write_text(json.dumps({'bind': f'127.0.0.1:{port}', 'tls_cert': str(cert), 'tls_key': str(key), 'session_idle_seconds': 1 if end=='expiry' else 900,
            'principals': [{'name': 'fixture', 'token_sha256': hashlib.sha256(token.encode()).hexdigest(), 'scopes': ['observe', 'files', 'lifecycle'] if GRAPHICS_ONLY else ['observe', 'files'], 'vm_ids': ['fixture']}]}))
        config.chmod(0o600)
        binary = CONTROL
        if mode == 'hung':
            binary = str(case/'hung-mcp')
            Path(binary).write_text('#!/usr/bin/env python3\nimport json,sys,time\nfor line in sys.stdin:\n r=json.loads(line)\n if "id" in r: print(json.dumps({"jsonrpc":"2.0","id":r["id"],"result":{"protocolVersion":"2025-06-18"}}),flush=True)\ntime.sleep(120)\n')
            Path(binary).chmod(0o700)
        log = open(case/'server.log', 'wb')
        server = subprocess.Popen([HTTP, '--config', str(config), '--state-dir', str(case), '--control-bin', binary], stdout=subprocess.DEVNULL, stderr=log, start_new_session=True)
        pids = []
        def request(method='POST', payload=None, sid=None, authenticated=True):
            connection = http.client.HTTPSConnection('127.0.0.1', port, context=context, timeout=48)
            headers = {'Accept': 'application/json, text/event-stream', 'Content-Type': 'application/json', 'MCP-Protocol-Version': '2025-06-18'}
            if authenticated: headers['Authorization'] = 'Bearer ' + token
            if sid: headers['MCP-Session-Id'] = sid
            try:
                connection.request(method, '/mcp', json.dumps(payload).encode() if payload is not None else b'', headers)
                response = connection.getresponse()
                body = response.read()
                return response.status, dict(response.getheaders()), json.loads(body) if body else None
            finally:
                connection.close()
        def ready():
            assert server.poll() is None, 'server exited before listening'
            try: return request('GET', authenticated=False)[0] == 401
            except OSError: return False
        pool = concurrent.futures.ThreadPoolExecutor(max_workers=2)
        try:
            wait_until(ready)
            ids=[]
            for index in range(sessions):
                status, headers, reply = request(payload={'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2025-06-18','capabilities':{},'clientInfo':{'name':'fixture','version':'1'}}})
                assert status == 200 and reply['result']['protocolVersion']=='2025-06-18', (status, reply)
                ids.append(headers['mcp-session-id'])
                assert request(payload={'jsonrpc':'2.0','method':'notifications/initialized'},sid=ids[-1])[0]==202
            if GRAPHICS_ONLY:
                status, _, reply = request(payload={'jsonrpc':'2.0','id':20,'method':'tools/list'},sid=ids[0])
                assert status == 200 and not any(t['name']=='vm_create' for t in reply['result']['tools']), reply
                for profile in ['virgl','venus-moltenvk-experimental','unknown']:
                    status, _, reply = request(payload={'jsonrpc':'2.0','id':21,'method':'tools/call','params':{'name':'vm_create','arguments':{'id':'fixture','backend':'qemu-hvf','qemu_graphics_profile':profile}}},sid=ids[0])
                    assert status == 200 and reply.get('error') == {'code': -32602, 'message': 'Tool or VM is not authorized for this principal'}, (status, reply)
                assert not daemon.started.is_set(), 'unauthorized create reached daemon'
            for line in subprocess.check_output(['ps','-axo','pid=,ppid='],text=True).splitlines():
                pid, parent=map(int,line.split())
                if parent==server.pid: pids.append(pid)
            assert len(pids)==sessions, (pids, sessions)
            active=None
            if mode=='inflight':
                active=pool.submit(request,payload={'jsonrpc':'2.0','id':2,'method':'tools/call','params':{'name':'vm_guest_info','arguments':{'id':'fixture'}}},sid=ids[0])
                assert daemon.started.wait(3)
            if end=='delete':
                status,_,_=request('DELETE',sid=ids[0])
                assert status == (204 if mode=='clean' else 502), status
            if end=='expiry':
                assert daemon.cleaned.wait(5)
                wait_until(lambda: request('GET',sid=ids[0])[0]==404)
            start=time.monotonic()
            server.send_signal(sig)
            if mode=='hung':
                # Listener/admission must stop while child cleanup is still pending.
                def refused():
                    try: return request('GET',authenticated=False)[0] in (503,404)
                    except (OSError,http.client.HTTPException): return True
                wait_until(refused,3)
            code=server.wait(timeout=48)
            expected=0 if mode in ('clean','inflight') else 1
            assert code==expected, (mode, end, code, (case/'server.log').read_text())
            if active is not None:
                try: active.result(timeout=2)
                except (OSError,http.client.HTTPException): pass
            for pid in pids:
                try: os.kill(pid,0)
                except ProcessLookupError: pass
                else: raise AssertionError('owned MCP child not reaped')
            if mode!='hung':
                assert len(daemon.calls)==sessions, daemon.calls
                assert len(set(daemon.calls))==sessions, 'same owner cleanup called twice'
            checks.append({'signal':signal.Signals(sig).name,'mode':mode,'termination':end,'sessions':sessions,'exit':code,'elapsed_seconds':round(time.monotonic()-start,3),'owned_children_reaped':True})
        finally:
            if server.poll() is None:
                os.killpg(server.pid, signal.SIGKILL);server.wait(timeout=3)
            pool.shutdown(wait=True)
            log.close()
            daemon.close()
    if GRAPHICS_ONLY:
        scenario(signal.SIGTERM,'clean',end='delete')
    else:
        for sig in (signal.SIGTERM,signal.SIGINT):
            scenario(sig,'clean',sessions=0)
            scenario(sig,'clean',sessions=2)
            scenario(sig,'file_failure')
            scenario(sig,'inflight')
        scenario(signal.SIGTERM,'malformed_file')
        scenario(signal.SIGTERM,'input_failure')
        scenario(signal.SIGTERM,'job_failure')
        scenario(signal.SIGTERM,'clean',end='delete')
        scenario(signal.SIGTERM,'file_failure',end='delete')
        scenario(signal.SIGTERM,'file_failure',end='expiry')
        scenario(signal.SIGTERM,'hung')
print(json.dumps({'passed':True,'scope':'real localhost TLS, HTTP, vm-control subprocesses and OS signals; synthetic daemon cleanup replies, no Guest/VM acceptance','cleanup_semantics':'authority/file/input cleanup and cancellation request acknowledgements; asynchronous job termination is not proven','checks':checks},indent=2))
