#!/usr/bin/env python3
"""Actual Guest worker-death/restart acceptance, with two manual Guest phases.

No VM lifecycle, signals, service restart, or permission changes are performed by
this host script. Run the Guest-only helper after --fault-ready appears; explicitly
restart the same opt-in service after --restart-ready appears. Evidence contains
no transfer tokens, file bodies, raw MCP replies, or credentials.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import select
import stat
import subprocess
import time
import uuid


class Failure(Exception):
    pass


def check(condition, code):
    if not condition:
        raise Failure(code)


def private_new(path):
    check(path.is_absolute(), 'absolute_evidence_path_required')
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    check(stat.S_IMODE(os.fstat(fd).st_mode) == 0o600, 'private_evidence_mode_required')
    return fd


def milestone(path, phase):
    fd = private_new(path)
    with os.fdopen(fd, 'w') as file:
        json.dump({'phase': phase, 'time_unix': time.time(), 'automatic_guest_actions': False}, file)
        file.write('\n'); file.flush(); os.fsync(file.fileno())
    print(phase, flush=True)


class MCP:
    def __init__(self, binary, state, files):
        self.proc = subprocess.Popen([str(binary), 'mcp', '--state-dir', str(state),
            *(['--allow-files'] if files else [])], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.sequence = 0
        self.buffer = bytearray()
        try:
            reply = self.rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                'clientInfo': {'name': 'veynode-files-worker-fault', 'version': '1'}})
            check(reply.get('result', {}).get('protocolVersion') == '2025-06-18', 'initialize_failed')
            self.send({'jsonrpc': '2.0', 'method': 'notifications/initialized'})
        except BaseException:
            self.close()
            raise

    def send(self, value):
        self.proc.stdin.write((json.dumps(value) + '\n').encode()); self.proc.stdin.flush()

    def rpc(self, method, params, timeout=15):
        self.sequence += 1
        self.send({'jsonrpc': '2.0', 'id': self.sequence, 'method': method, 'params': params})
        deadline = time.monotonic() + timeout
        while b'\n' not in self.buffer:
            left = deadline - time.monotonic()
            check(left > 0 and select.select([self.proc.stdout], [], [], max(0, left))[0], 'mcp_response_deadline')
            block = os.read(self.proc.stdout.fileno(), 65536)
            check(bool(block), 'mcp_unexpected_eof'); self.buffer.extend(block)
            check(len(self.buffer) <= 1024 * 1024, 'mcp_response_bound')
        line, _, rest = self.buffer.partition(b'\n'); self.buffer = bytearray(rest)
        reply = json.loads(line)
        check(reply.get('id') == self.sequence and reply.get('jsonrpc') == '2.0', 'mcp_response_identity')
        return reply

    def tool(self, vm, name, fields=None, denied_codes=None, unavailable_ok=False):
        reply = self.rpc('tools/call', {'name': 'vm_' + name, 'arguments': {'id': vm, **(fields or {})}})
        result = reply.get('result', {})
        check('error' not in reply and isinstance(result, dict), 'mcp_rpc_error')
        content = result.get('content', [])
        check(len(content) == 1 and content[0].get('type') == 'text', 'mcp_content_shape')
        text = content[0]['text']
        if result.get('isError') is True:
            if unavailable_ok:
                return None
            check(denied_codes is not None and text.startswith('guest: '), 'guest_tool_failed')
            error = json.loads(text[len('guest: '):])
            check(error.get('code') in denied_codes, 'unexpected_guest_denial')
            return None
        check(denied_codes is None, 'required_denial_missing')
        wire = json.loads(text)
        check(isinstance(wire.get('result'), dict) and 'error' not in wire, 'guest_result_shape')
        return wire['result']

    def close(self, expect_cleanup_failure=False):
        if not self.proc.stdin.closed:
            self.proc.stdin.close()
        try:
            code = self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill(); self.proc.wait(timeout=5)
            raise Failure('mcp_cleanup_forced')
        if expect_cleanup_failure:
            check(code > 0, 'unconfirmed_cleanup_must_exit_nonzero_without_signal')
        else:
            check(code == 0, 'mcp_nonzero_exit')
        return code


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ['binary', 'state-dir', 'output', 'fault-ready', 'restart-ready']:
        p.add_argument('--' + name, type=Path, required=True)
    p.add_argument('--id', required=True)
    p.add_argument('--fault-timeout', type=int, default=40,
                   help='10..45 seconds; must inject before the Guest 60-second transfer idle deadline')
    p.add_argument('--restart-timeout', type=int, default=180)
    p.add_argument('--expect-cleanup-failure', action='store_true',
                   help='After worker fault, require original files MCP EOF to exit nonzero; use a new owner only after that observed failure')
    p.add_argument('--cleanup-retention-wait', type=int, default=0,
                   help='With expected cleanup failure, wait331..360 real seconds after Guest fault, then trigger another owner file begin before original EOF')
    args = p.parse_args()
    check(all(path.is_absolute() for path in [args.binary, args.state_dir, args.output, args.fault_ready, args.restart_ready]), 'absolute_paths_required')
    check(len({args.output, args.fault_ready, args.restart_ready}) == 3, 'evidence_paths_must_differ')
    check(not args.fault_ready.exists() and not args.restart_ready.exists(), 'readiness_evidence_exists')
    check(10 <= args.fault_timeout <= 45 and 10 <= args.restart_timeout <= 300, 'invalid_phase_timeout')
    check(args.cleanup_retention_wait == 0 or (args.expect_cleanup_failure and 331 <= args.cleanup_retention_wait <= 360), 'invalid_retention_wait')
    check(args.binary.is_file() and not args.binary.is_symlink(), 'regular_frozen_binary_required')
    check(args.state_dir.is_dir() and not args.state_dir.is_symlink(), 'state_directory_required')
    fd = private_new(args.output)
    report = {'passed': False, 'started_at_unix': time.time(), 'checks': [],
        'controller_sha256': hashlib.sha256(args.binary.read_bytes()).hexdigest(),
        'expect_cleanup_failure': args.expect_cleanup_failure,
        'scope': 'Actual manually signalled Guest file worker; no VM or service lifecycle actions by host script'}
    clients = []
    phase = 'setup'
    def record(name, evidence=True):
        report['checks'].append({'name': name, 'evidence': evidence})
        print(name, flush=True)
    def info(observer, transient=False):
        value = observer.tool(args.id, 'guest_info', unavailable_ok=transient)
        if value is not None:
            check(value.get('os') == 'darwin' and str(value.get('machine', '')).startswith('VirtualMac')
                and value.get('uid', 0) > 0 and value.get('capabilities', {}).get('exec') is False
                and value['capabilities'].get('async_exec') is False, 'guest_identity_or_exec_boundary')
        return value
    try:
        observer = MCP(args.binary, args.state_dir, False); clients.append(observer)
        files = MCP(args.binary, args.state_dir, True); clients.append(files)
        initial = info(observer)
        check(initial['capabilities'].get('files') is True, 'files_not_available')
        policy = initial.get('file_policy', {})
        check(policy.get('uid') == initial['uid'] and policy.get('active_transfers') == 0
            and policy.get('staged_upload_files') == 0, 'fixture_not_idle')
        name = 'worker-fault-' + uuid.uuid4().hex + '.bin'
        body = b'partial-worker-fault-fixture'
        token = files.tool(args.id, 'file_upload_begin', {'path': name, 'size': len(body), 'sha256': hashlib.sha256(body).hexdigest()})['token']
        partial_started = time.monotonic()
        written = files.tool(args.id, 'file_upload_write', {'token': token, 'offset': 0, 'data': base64.b64encode(body[:5]).decode()})
        check(written.get('offset') == 5, 'partial_offset')
        pending = info(observer)['file_policy']
        check(pending.get('active_transfers') == 1 and pending.get('staged_upload_files') == 1, 'partial_not_observed')
        record('actual partial upload ready', {'bytes_written': 5, 'published': False})
        phase = 'await-manual-worker-sigkill'
        milestone(args.fault_ready, phase)
        deadline = min(time.monotonic() + args.fault_timeout, partial_started + 45)
        while True:
            current = info(observer)
            check(time.monotonic() < deadline, 'manual_fault_deadline_before_idle_expiry')
            if current['capabilities'].get('files') is False:
                error = current.get('file_policy', {}).get('error', {})
                check(error.get('code') in ['unavailable', 'timeout'], 'unexpected_fence_reason')
                break
            check(current.get('file_policy', {}).get('active_transfers') == 1
                and current['file_policy'].get('staged_upload_files') == 1, 'partial_disappeared_before_worker_fault')
            time.sleep(0.2)
        files.tool(args.id, 'file_upload_begin', {'path': 'fenced-' + uuid.uuid4().hex, 'size': 0, 'sha256': hashlib.sha256(b'').hexdigest()}, denied_codes=['unavailable'])
        check(info(observer)['capabilities'].get('files') is False, 'worker_implicitly_restarted')
        record('main service survived while files remained fenced; new transfer denied')
        if args.cleanup_retention_wait:
            phase = 'wait-beyond-old-controller-route-retention'
            print(phase, flush=True)
            began = time.monotonic()
            while time.monotonic() - began < args.cleanup_retention_wait:
                # No original-owner file request may refresh the route during
                # this interval. Guest-info is a separate observe-only client.
                check(info(observer)['capabilities'].get('files') is False, 'worker_restarted_during_retention_wait')
                time.sleep(1)
            challenger = MCP(args.binary, args.state_dir, True); clients.append(challenger)
            challenger.tool(args.id, 'file_upload_begin', {'path': 'retention-trigger-' + uuid.uuid4().hex,
                'size': 0, 'sha256': hashlib.sha256(b'').hexdigest()}, denied_codes=['unavailable'])
            code = challenger.close(expect_cleanup_failure=True); clients.remove(challenger)
            record('new owner request after old route TTL also reports unconfirmed cleanup',
                   {'elapsed_seconds': time.monotonic()-began, 'requested_wait_seconds': args.cleanup_retention_wait,
                    'challenger_exit_code': code, 'original_owner_file_requests_during_wait': 0})
        if args.expect_cleanup_failure:
            phase = 'unconfirmed-owner-cleanup-exit'
            code = files.close(expect_cleanup_failure=True)
            clients.remove(files)
            record('original files MCP reported unconfirmed cleanup with nonzero exit',
                   {'exit_code': code, 'guest_cleanup_confirmed': False})
            # This is deliberately a different controller owner. Original token
            # bytes remain only in memory for the post-restart negative check;
            # independent zero-staging evidence proves actual Guest cleanup.
            files = MCP(args.binary, args.state_dir, True); clients.append(files)
            check(info(observer)['capabilities'].get('files') is False, 'worker_restarted_by_new_client')
        phase = 'await-explicit-service-restart'
        milestone(args.restart_ready, phase)
        deadline = time.monotonic() + args.restart_timeout
        while True:
            current = info(observer, transient=True)
            if current is not None and current['capabilities'].get('files') is True:
                break
            check(time.monotonic() < deadline, 'manual_restart_deadline'); time.sleep(0.2)
        policy = current.get('file_policy', {})
        check(policy.get('active_transfers') == 0 and policy.get('staged_upload_files') == 0, 'restart_staging_not_clean')
        files.tool(args.id, 'file_upload_write', {'token': token, 'offset': 5, 'data': base64.b64encode(body[5:]).decode()}, denied_codes=['not_found', 'forbidden'])
        files.tool(args.id, 'file_download_begin', {'path': name}, denied_codes=['not_found'])
        record('explicit restart cleared staging; old token rejected and partial never published')
        phase = 'post-restart-roundtrip'
        body = '恢复后的文件 ✓\n'.encode() + os.urandom(256)
        name = 'worker-recovered-' + uuid.uuid4().hex + '.bin'
        sha = hashlib.sha256(body).hexdigest()
        token = files.tool(args.id, 'file_upload_begin', {'path': name, 'size': len(body), 'sha256': sha})['token']
        check(files.tool(args.id, 'file_upload_write', {'token': token, 'offset': 0, 'data': base64.b64encode(body).decode()})['offset'] == len(body), 'recovered_upload_offset')
        committed = files.tool(args.id, 'file_upload_commit', {'token': token})
        check(committed.get('published') is True and committed.get('size') == len(body) and committed.get('sha256') == sha, 'recovered_commit')
        opened = files.tool(args.id, 'file_download_begin', {'path': name})
        part = files.tool(args.id, 'file_download_read', {'token': opened['token'], 'offset': 0})
        decoded = base64.b64decode(part['data'], validate=True)
        check(part.get('eof') is True and part.get('offset') == len(body) and opened.get('size') == len(body)
            and opened.get('sha256') == sha and part.get('sha256') == sha
            and decoded == body and hashlib.sha256(decoded).hexdigest() == sha, 'recovered_roundtrip')
        record('new post-restart atomic roundtrip', {'bytes': len(body), 'sha256': sha, 'published_filename': name})
        check(hashlib.sha256(args.binary.read_bytes()).hexdigest() == report['controller_sha256'], 'controller_changed')
        report['passed'] = True
    except Exception as error:
        report['error'] = {'phase': phase, 'code': str(error) if isinstance(error, Failure) else 'acceptance_operation_failed'}
    finally:
        for client in reversed(clients):
            try:
                client.close()
            except Exception:
                report['passed'] = False; report['mcp_cleanup_unconfirmed'] = True
        report['finished_at_unix'] = time.time()
        with os.fdopen(fd, 'w') as file:
            json.dump(report, file, indent=2); file.write('\n'); file.flush(); os.fsync(file.fileno())
    print(json.dumps({'passed': report['passed'], 'checks': len(report['checks'])}), flush=True)
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except Exception:
        print('Fault acceptance setup failed; no raw protocol data emitted.', flush=True)
        raise SystemExit(1)
