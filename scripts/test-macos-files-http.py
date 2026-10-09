#!/usr/bin/env python3
"""Actual existing-macOS-Guest files over loopback HTTPS Streamable HTTP.

Requires an already running daemon/Guest with opt-in files. Never changes VM,
Guest service, permissions, OS certificate trust, or network exposure. Starts
only its own HTTPS server using explicitly supplied frozen binaries. Evidence is
exclusive/private and contains no bodies, bearer/transfer/session tokens, keys,
raw replies, or stderr. A uniquely named committed fixture remains in the Guest.

--signal-test additionally requires graceful SIGTERM of the owned server (exit
zero) with an active partial upload, then independently observes Guest cleanup.
Without it, sessions are explicitly deleted before terminating the test server;
that final process termination is NOT claimed to test graceful signal cleanup.
"""
import argparse
import base64
import hashlib
import http.client as http_client
import importlib.util
import json
import os
from pathlib import Path
import secrets
import socket
import ssl
import subprocess
import tempfile
import time


# Reuse the bounded, redacted stdio observer; importing does not start processes.
spec = importlib.util.spec_from_file_location(
    'macos_file_fault_support', Path(__file__).with_name('test-macos-files-worker-fault.py'))
support = importlib.util.module_from_spec(spec)
spec.loader.exec_module(support)
Failure, check, MCP = support.Failure, support.check, support.MCP
LIMIT = 1024 * 1024
ORIGIN = 'https://veynode-files-test.invalid'


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


class HTTPS:
    def __init__(self, port, cert, tokens):
        self.port, self.tokens = port, tokens
        self.context = ssl.create_default_context(cafile=str(cert))
        check(self.context.check_hostname and self.context.verify_mode == ssl.CERT_REQUIRED,
              'tls_verification_required')
        self.sequence = 0
        self.sessions = []

    def request(self, method='POST', value=None, principal='filer-a', session=None,
                extra=None):
        headers = {'Accept': 'application/json, text/event-stream',
                   'Content-Type': 'application/json', 'MCP-Protocol-Version': '2025-06-18',
                   'Origin': ORIGIN}
        if principal is not None:
            headers['Authorization'] = 'Bearer ' + self.tokens.get(principal, principal)
        if session is not None:
            headers['MCP-Session-Id'] = session
        headers.update(extra or {})
        connection = http_client.HTTPSConnection('127.0.0.1', self.port,
                                                context=self.context, timeout=20)
        try:
            body = json.dumps(value).encode() if value is not None else b''
            check(len(body) <= LIMIT, 'request_size_bound')
            connection.request(method, '/mcp', body, headers)
            response = connection.getresponse()
            payload = response.read(LIMIT + 1)
            check(len(payload) <= LIMIT, 'response_size_bound')
            # HTTP error text is intentionally never parsed or emitted.
            decoded = json.loads(payload) if payload and response.status == 200 else None
            return response.status, decoded, {k.lower(): v for k, v in response.getheaders()}
        finally:
            connection.close()

    def rpc(self, method, params=None, principal='filer-a', session=None):
        self.sequence += 1
        status, reply, headers = self.request(value={'jsonrpc': '2.0', 'id': self.sequence,
            'method': method, **({'params': params} if params is not None else {})},
            principal=principal, session=session)
        if status == 200:
            check(isinstance(reply, dict) and reply.get('id') == self.sequence
                  and reply.get('jsonrpc') == '2.0', 'http_rpc_identity')
        return status, reply, headers

    def initialize(self, principal='filer-a'):
        status, reply, headers = self.rpc('initialize', {'protocolVersion': '2025-06-18',
            'capabilities': {}, 'clientInfo': {'name': 'veynode-macos-files-https', 'version': '1'}},
            principal=principal)
        check(status == 200 and reply.get('result', {}).get('protocolVersion') == '2025-06-18',
              'http_initialize_failed')
        sid = headers.get('mcp-session-id')
        check(isinstance(sid, str) and 0 < len(sid) <= 256, 'http_session_missing')
        self.sessions.append((principal, sid))
        check(self.request(value={'jsonrpc': '2.0', 'method': 'notifications/initialized'},
              principal=principal, session=sid)[0] == 202, 'http_initialized_failed')
        return sid

    def delete(self, principal, session, expired=False):
        status = self.request('DELETE', principal=principal, session=session)[0]
        check(status == 204 or (expired and status == 404), 'http_delete_not_confirmed')
        self.sessions.remove((principal, session))

    def tool(self, vm, session, name, fields=None, principal='filer-a',
             policy_denied=False, guest_denied=None):
        status, reply, _ = self.rpc('tools/call', {'name': 'vm_' + name,
            'arguments': {'id': vm, **(fields or {})}}, principal, session)
        check(status == 200, 'http_tool_status')
        if policy_denied:
            check(reply.get('error', {}).get('code') == -32602, 'required_policy_denial_missing')
            return None
        result = reply.get('result', {})
        check('error' not in reply and isinstance(result, dict), 'http_tool_rpc_error')
        content = result.get('content', [])
        check(len(content) == 1 and content[0].get('type') == 'text', 'http_tool_content')
        text = content[0]['text']
        if guest_denied is not None:
            check(result.get('isError') is True and text.startswith('guest: '), 'required_guest_denial_missing')
            check(json.loads(text[7:]).get('code') in guest_denied, 'unexpected_guest_denial')
            return None
        check(result.get('isError') is not True, 'http_guest_tool_failed')
        wire = json.loads(text)
        check(isinstance(wire.get('result'), dict) and 'error' not in wire, 'http_guest_result_shape')
        return wire['result']


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ['server', 'control', 'state-dir', 'output']:
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--id', required=True)
    parser.add_argument('--signal-test', action='store_true')
    args = parser.parse_args()
    check(all(p.is_absolute() for p in [args.server, args.control, args.state_dir, args.output]),
          'absolute_paths_required')
    check(all(p.is_file() and not p.is_symlink() for p in [args.server, args.control]),
          'regular_frozen_binaries_required')
    check(args.state_dir.is_dir() and not args.state_dir.is_symlink(), 'existing_state_required')
    report_fd = support.private_new(args.output)
    report = {'passed': False, 'started_at_unix': time.time(), 'checks': [],
        'server_sha256': digest(args.server), 'controller_sha256': digest(args.control),
        'signal_test': args.signal_test,
        'scope': 'Existing macOS Guest files; loopback HTTPS terminates on this Mac; no VM lifecycle',
        'tls': 'Test-client CA only; CERT_REQUIRED with hostname checking; no OS trust changes'}
    observer = server = http = None
    phase = 'setup'

    def record(name, evidence=True):
        report['checks'].append({'name': name, 'evidence': evidence})
        print(name, flush=True)

    def info():
        value = observer.tool(args.id, 'guest_info')
        caps, policy = value.get('capabilities', {}), value.get('file_policy', {})
        check(value.get('os') == 'darwin' and str(value.get('machine', '')).startswith('VirtualMac')
              and value.get('uid', 0) > 0 and caps.get('files') is True
              and caps.get('exec') is False and caps.get('async_exec') is False,
              'guest_identity_or_capability_boundary')
        check(policy.get('uid') == value['uid'], 'guest_same_uid_file_policy')
        return policy

    def clean(deadline_seconds=10):
        until = time.monotonic() + deadline_seconds
        while True:
            policy = info()
            if policy.get('active_transfers') == 0 and policy.get('staged_upload_files') == 0:
                return
            check(time.monotonic() < until, 'guest_cleanup_deadline')
            time.sleep(0.15)

    def partial(session, prefix):
        name = prefix + '-' + secrets.token_hex(12)
        body = b'never-publish-this-partial'
        token = http.tool(args.id, session, 'file_upload_begin', {'path': name,
            'size': len(body), 'sha256': hashlib.sha256(body).hexdigest()})['token']
        result = http.tool(args.id, session, 'file_upload_write', {'token': token,
            'offset': 0, 'data': base64.b64encode(body[:5]).decode()})
        check(result.get('offset') == 5, 'partial_offset')
        policy = info()
        check(policy.get('active_transfers') == 1 and policy.get('staged_upload_files') == 1,
              'partial_not_observed')
        return name

    try:
        with tempfile.TemporaryDirectory(prefix='veynode-files-https-') as directory:
            private = Path(directory)
            private.chmod(0o700)
            cert, key, config = [private / name for name in ['cert.pem', 'key.pem', 'server.json']]
            subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-sha256',
                '-keyout', str(key), '-out', str(cert), '-days', '1', '-subj', '/CN=localhost',
                '-addext', 'subjectAltName=DNS:localhost,IP:127.0.0.1'], check=True, timeout=15,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            key.chmod(0o600); cert.chmod(0o600)
            tokens = {name: secrets.token_urlsafe(32) for name in ['observer', 'filer-a', 'filer-b']}
            with socket.socket() as bound:
                bound.bind(('127.0.0.1', 0)); port = bound.getsockname()[1]
            config_fd = support.private_new(config)
            with os.fdopen(config_fd, 'w') as stream:
                json.dump({'bind': f'127.0.0.1:{port}', 'tls_cert': str(cert), 'tls_key': str(key),
                    'allowed_origins': [ORIGIN], 'session_idle_seconds': 5,
                    'principals': [{'name': name, 'token_sha256': hashlib.sha256(token.encode()).hexdigest(),
                        'vm_ids': [args.id], 'scopes': ['observe'] + ([] if name == 'observer' else ['files'])}
                        for name, token in tokens.items()]}, stream)
            observer = MCP(args.control, args.state_dir, False)
            clean(0)
            server = subprocess.Popen([str(args.server), '--config', str(config), '--state-dir',
                str(args.state_dir), '--control-bin', str(args.control)], stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            http = HTTPS(port, cert, tokens)
            until = time.monotonic() + 8
            while True:
                check(server.poll() is None, 'https_server_exited_before_ready')
                try:
                    if http.request('GET', principal=None)[0] == 401:
                        break
                except (ConnectionRefusedError, ConnectionResetError):
                    pass
                check(time.monotonic() < until, 'https_server_start_deadline'); time.sleep(0.05)

            phase = 'https-auth-policy'
            connection = http_client.HTTPSConnection(
                '127.0.0.1', port, context=ssl.create_default_context(), timeout=3)
            try:
                try:
                    connection.request('GET', '/mcp'); connection.getresponse()
                except ssl.SSLCertVerificationError:
                    record('untrusted CA rejected')
                else:
                    raise Failure('untrusted_ca_accepted')
            finally:
                connection.close()
            check(http.rpc('ping', principal=None)[0] == 401, 'missing_auth_accepted')
            check(http.rpc('ping', principal='invalid-' + secrets.token_hex(32))[0] == 401, 'wrong_auth_accepted')
            check(http.request('GET', extra={'Origin': 'https://untrusted.invalid'})[0] == 403,
                  'foreign_origin_accepted')
            observe = http.initialize('observer'); session = http.initialize()
            check(http.rpc('ping', principal='filer-b', session=session)[0] == 404, 'cross_principal_session_accepted')
            check(http.rpc('ping', session='missing-' + secrets.token_hex(16))[0] == 404, 'unknown_session_accepted')
            http.tool(args.id, observe, 'file_download_begin', {'path': 'denied'}, 'observer', policy_denied=True)
            for name, fields in [('exec', {'argv': ['/usr/bin/true']}), ('start', {}),
                                 ('acquire_control', {})]:
                http.tool(args.id, session, name, fields, policy_denied=True)
            http.tool('unauthorized-' + secrets.token_hex(12), session, 'guest_info', policy_denied=True)
            actual = http.tool(args.id, session, 'guest_info')
            check(actual.get('os') == 'darwin'
                  and str(actual.get('machine', '')).startswith('VirtualMac')
                  and actual.get('uid', 0) > 0 and actual.get('capabilities', {}).get('files') is True
                  and actual['capabilities'].get('exec') is False
                  and actual['capabilities'].get('async_exec') is False,
                  'https_actual_guest_identity')
            http.delete('observer', observe)
            record('verified HTTPS; bearer, Origin, principal/session, independent files scope and VM allowlist enforced')

            phase = 'atomic-roundtrip'
            body = 'HTTPS 文件验收 ✓\n'.encode() + secrets.token_bytes(65000)
            sha = hashlib.sha256(body).hexdigest(); filename = 'https-' + secrets.token_hex(12) + '-测试.bin'
            token = http.tool(args.id, session, 'file_upload_begin', {'path': filename,
                'size': len(body), 'sha256': sha})['token']
            foreign = http.initialize('filer-b')
            http.tool(args.id, foreign, 'file_upload_write', {'token': token, 'offset': 0,
                'data': base64.b64encode(body[:1]).decode()}, 'filer-b', guest_denied=['forbidden', 'not_found'])
            http.delete('filer-b', foreign)
            http.tool(args.id, session, 'file_download_begin', {'path': filename}, guest_denied=['not_found'])
            for offset in range(0, len(body), 32768):
                chunk = body[offset:offset + 32768]
                wrote = http.tool(args.id, session, 'file_upload_write', {'token': token,
                    'offset': offset, 'data': base64.b64encode(chunk).decode()})
                check(wrote.get('offset') == offset + len(chunk), 'upload_chunk_offset')
            committed = http.tool(args.id, session, 'file_upload_commit', {'token': token})
            check(committed.get('published') is True and committed.get('size') == len(body)
                  and committed.get('sha256') == sha, 'atomic_commit_integrity')
            opened = http.tool(args.id, session, 'file_download_begin', {'path': filename})
            check(opened.get('size') == len(body) and opened.get('sha256') == sha, 'download_metadata')
            received = bytearray()
            for _ in range(4):
                chunk = http.tool(args.id, session, 'file_download_read', {'token': opened['token'], 'offset': len(received)})
                decoded = base64.b64decode(chunk['data'], validate=True)
                check(0 < len(decoded) <= 32768 and len(received) + len(decoded) <= len(body), 'download_chunk_bound')
                received.extend(decoded)
                check(chunk.get('offset') == len(received) and chunk.get('sha256') == sha, 'download_chunk_integrity')
                if chunk.get('eof') is True:
                    break
            else:
                raise Failure('download_eof_missing')
            check(bytes(received) == body and hashlib.sha256(received).hexdigest() == sha, 'roundtrip_integrity')
            clean()
            record('actual Unicode/binary atomic upload/download and foreign file-owner denial',
                   {'bytes': len(body), 'sha256': sha, 'published_filename': filename})

            phase = 'delete-cleanup'
            absent = [partial(session, 'https-delete-partial')]
            http.delete('filer-a', session)
            check(http.rpc('ping', session=session)[0] == 404, 'deleted_session_reusable')
            clean()
            record('DELETE acknowledged; independent stdio observed zero active transfers and staged files')

            phase = 'idle-cleanup'
            session = http.initialize(); absent.append(partial(session, 'https-idle-partial'))
            started = time.monotonic()
            clean(12)
            check(4 <= time.monotonic() - started < 15, 'cleanup_not_http_idle_bound')
            check(http.rpc('ping', session=session)[0] == 404, 'idle_session_still_live')
            http.delete('filer-a', session, expired=True)
            record('HTTP 5-second idle expiry cleaned partial before Guest 60-second idle limit',
                   {'elapsed_seconds': round(time.monotonic() - started, 3)})
            session = http.initialize()
            for name in absent:
                http.tool(args.id, session, 'file_download_begin', {'path': name}, guest_denied=['not_found'])
            record('DELETE and expired uploads never published')

            if args.signal_test:
                phase = 'signal-cleanup'
                partial(session, 'https-term-partial')
                started = time.monotonic()
                server.terminate()  # Only this Popen-owned server PID, never the daemon/Guest.
                code = server.wait(timeout=12)
                report['signal_exit_code'] = code
                clean(10)  # Independent stdio path survives the terminated HTTPS listener.
                check(code == 0, 'signal_cleanup_requires_exit_zero')
                http.sessions.clear()
                record('owned HTTPS SIGTERM exited zero; independent stdio observed zero active/staged files',
                       {'elapsed_seconds': round(time.monotonic() - started, 3)})
            else:
                http.delete('filer-a', session)
                clean()
            check(digest(args.server) == report['server_sha256']
                  and digest(args.control) == report['controller_sha256'], 'frozen_binary_changed')
            report['passed'] = True
    except Exception as error:
        report['error'] = {'phase': phase, 'code': str(error) if isinstance(error, Failure)
                          else 'acceptance_operation_failed'}
    finally:
        if http is not None and server is not None and server.poll() is None:
            for principal, session in list(http.sessions):
                try:
                    http.delete(principal, session, expired=True)
                except Exception:
                    report['passed'] = False; report['http_session_cleanup_unconfirmed'] = True
        if server is not None:
            if server.poll() is None:
                server.terminate()
                try:
                    server.wait(timeout=12)
                except subprocess.TimeoutExpired:
                    server.kill(); server.wait(timeout=5)
                    report['passed'] = False; report['server_forced_kill'] = True
            report['server_exit_code'] = server.returncode
        if observer is not None:
            try:
                clean(10)
            except Exception:
                report['passed'] = False; report['guest_cleanup_unconfirmed'] = True
            try:
                observer.close()
            except Exception:
                report['passed'] = False; report['stdio_cleanup_unconfirmed'] = True
        report['finished_at_unix'] = time.time()
        with os.fdopen(report_fd, 'w') as stream:
            json.dump(report, stream, indent=2); stream.write('\n'); stream.flush(); os.fsync(stream.fileno())
    print(json.dumps({'passed': report['passed'], 'checks': len(report['checks'])}), flush=True)
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except Exception:
        print('HTTPS files acceptance setup failed; no raw protocol data emitted.', flush=True)
        raise SystemExit(1)
