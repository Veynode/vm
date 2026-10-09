#!/usr/bin/env python3
"""TEST ONLY: forward a real worker, drop one successful exec_start reply.

Register this executable as --qemu-worker only in a dedicated test daemon.
VEYNODE_LOST_ACK_CONFIG names the private configuration made by the acceptance
script's --prepare mode. Never synthesizes Guest or worker responses. The real
worker retains its executable identity, process group, serial link and guardian.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import select
import stat
import subprocess
import sys
import time

MAX_LINE = 1024 * 1024


def require(ok, message):
    if not ok:
        raise RuntimeError(message)


def digest(path):
    h = hashlib.sha256()
    with open(path, 'rb') as f:
        for block in iter(lambda: f.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def load_config(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        s = os.fstat(fd)
        require(stat.S_ISREG(s.st_mode) and s.st_uid == os.getuid()
                and stat.S_IMODE(s.st_mode) == 0o600, 'private regular config required')
        data = os.read(fd, 8193)
        require(len(data) <= 8192, 'config size bound')
        value = json.loads(data)
    finally:
        os.close(fd)
    require(set(value) == {'worker', 'worker_sha256', 'trace', 'tag'}, 'config schema')
    require(re.fullmatch(r'veynode-lost-ack-[0-9a-f]{32}', value['tag']), 'config tag')
    require(Path(value['worker']).is_absolute() and digest(value['worker']) == value['worker_sha256'],
            'frozen worker hash mismatch')
    parent = Path(value['trace']).parent
    s = parent.lstat()
    require(stat.S_ISDIR(s.st_mode) and s.st_uid == os.getuid()
            and stat.S_IMODE(s.st_mode) == 0o700, 'private trace directory required')
    return value


class Filter:
    def __init__(self, tag, record):
        self.tag, self.record = tag, record
        self.target, self.revokes = None, {}
        self.dropped = False
        self.original_starts = 0

    def incoming(self, line):
        value = json.loads(line)
        req = value.get('request', {})
        if value.get('method') == 'guest_request' and req.get('method') == 'exec_start':
            scope = req.get('scope')
            if self.target and scope == self.target['scope']:
                self.original_starts += 1
                raise RuntimeError('original scope issued another start')
            if self.tag in req.get('argv', []):
                require(self.target is None and type(value.get('id')) is int
                        and req.get('id') == str(value['id']), 'ambiguous target start')
                require(isinstance(scope, dict) and set(scope) == {'owner', 'epoch'}
                        and re.fullmatch('[0-9a-f]{64}', scope['owner'])
                        and type(scope['epoch']) is int and scope['epoch'] > 0, 'target scope invalid')
                self.target = {'id': value['id'], 'nested_id': req['id'], 'scope': scope}
                self.original_starts = 1
                self.record('target_start', outer_id=value['id'], epoch=scope['epoch'],
                            owner_sha256=hashlib.sha256(scope['owner'].encode()).hexdigest())
        if value.get('method') == 'guest_scope_revoke' and self.target and req.get('scope') == self.target['scope']:
            require(type(value.get('id')) is int and req.get('id') == str(value['id'])
                    and req.get('method') == 'scope_revoke', 'invalid original scope revoke')
            self.revokes[value['id']] = req['id']
            self.record('original_scope_revoke', outer_id=value['id'])
        if req.get('method') == 'exec_cancel' and self.target and req.get('scope') == self.target['scope']:
            raise RuntimeError('unexpected original job-id cancellation')

    def outgoing(self, line):
        value = json.loads(line)
        ident = value.get('id')
        if self.target and ident == self.target['id']:
            require(not self.dropped, 'duplicate target ACK')
            nested = value.get('result', {})
            result = nested.get('result', {})
            require('error' not in value and 'error' not in nested
                    and nested.get('id') == self.target['nested_id']
                    and result.get('state') == 'running'
                    and isinstance(result.get('job_id'), str)
                    and re.fullmatch('[0-9a-f]{32}', result['job_id']), 'target did not return real successful start')
            self.dropped = True
            self.record('successful_ack_dropped', outer_id=ident, drop_count=1,
                        job_id_sha256=hashlib.sha256(result['job_id'].encode()).hexdigest())
            return False
        if type(ident) is int and ident in self.revokes:
            nested = value.get('result', {})
            require('error' not in value and nested.get('id') == self.revokes.pop(ident)
                    and nested.get('result') == {'revoked': True, 'pending_starts': 0, 'cgroup_empty': True},
                    'real original scope cleanup unconfirmed')
            self.record('original_scope_empty_ack', outer_id=ident, cgroup_empty=True)
        return True


def relay(child, filt):
    # All four endpoints nonblocking, bounded queues and absolute pending-write
    # deadlines. No reader thread can retain a duplicate child stdin writer.
    child_in, child_out = child.stdin.fileno(), child.stdout.fileno()
    source = {sys.stdin.fileno(): child_in, child_out: sys.stdout.fileno()}
    partial = {fd: bytearray() for fd in source}
    queued, until = {fd: bytearray() for fd in source.values()}, {}
    for fd in set(source) | set(source.values()):
        os.set_blocking(fd, False)
    eof_at = None
    while source or any(queued.values()):
        now = time.monotonic()
        require(all(now < deadline for deadline in until.values()), 'pending pipe write deadline')
        require(eof_at is None or now - eof_at < 65, 'worker EOF cleanup deadline')
        ready_r, ready_w, _ = select.select(list(source), [fd for fd, b in queued.items() if b], [], .05)
        for fd in ready_w:
            try:
                sent = os.write(fd, queued[fd])
            except BlockingIOError:
                continue
            require(sent > 0, 'zero pipe write')
            del queued[fd][:sent]
            if not queued[fd]:
                until.pop(fd, None)
        for fd in ready_r:
            try:
                block = os.read(fd, 65536)
            except BlockingIOError:
                continue
            if not block:
                require(not partial[fd], 'partial JSONL at EOF')
                del source[fd]
                eof_at = eof_at or time.monotonic()
                if fd == child_out:
                    # A naturally stopped worker need not wait for daemon stdin.
                    source.pop(sys.stdin.fileno(), None)
                continue
            partial[fd].extend(block)
            while b'\n' in partial[fd]:
                line, _, rest = partial[fd].partition(b'\n')
                partial[fd] = bytearray(rest)
                require(len(line) <= MAX_LINE, 'JSONL bound')
                forward = True
                if fd == sys.stdin.fileno():
                    filt.incoming(line)
                else:
                    forward = filt.outgoing(line)
                if forward:
                    dest = source[fd]
                    require(len(queued[dest]) + len(line) + 1 <= 2 * MAX_LINE, 'queue bound')
                    until.setdefault(dest, time.monotonic() + 2)
                    queued[dest].extend(line + b'\n')
            require(len(partial[fd]) <= MAX_LINE, 'unterminated JSONL bound')
        if sys.stdin.fileno() not in source and not queued[child_in] and not child.stdin.closed:
            child.stdin.close()
    return child.wait(timeout=5)


def main():
    config = load_config(os.environ['VEYNODE_LOST_ACK_CONFIG'])
    trace_fd = os.open(config['trace'], os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    events = 0
    def record(event, **data):
        nonlocal events
        events += 1
        require(events <= 64, 'trace count bound')
        payload = json.dumps({'event': event, 'monotonic': time.monotonic(), **data}).encode() + b'\n'
        require(os.write(trace_fd, payload) == len(payload), 'trace write')
    child = None
    try:
        require(sys.argv.count('--guest-worker-instance') == 1, 'one v2 worker instance required')
        instance = sys.argv[sys.argv.index('--guest-worker-instance') + 1]
        require(re.fullmatch('[0-9a-f]{64}', instance), 'worker instance format')
        child = subprocess.Popen([config['worker'], *sys.argv[1:]], stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, close_fds=True)
        record('worker_spawned', shim_pid=os.getpid(), worker_pid=child.pid,
               worker_instance_sha256=hashlib.sha256(instance.encode()).hexdigest(),
               worker_sha256=config['worker_sha256'])
        code = relay(child, Filter(config['tag'], record))
        record('worker_reaped', exit_code=code)
        return code if 0 <= code <= 255 else 1
    except BaseException:
        record('shim_failed')  # Never expose protocol lines, job IDs or argv.
        raise
    finally:
        if child and child.poll() is None:
            child.stdin.close()
            try:
                child.wait(timeout=7)
            except subprocess.TimeoutExpired:
                child.terminate()
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=3)
        os.close(trace_fd)


if __name__ == '__main__':
    try:
        sys.exit(main())
    except Exception:
        # Generic only; real daemon intentionally ignores worker stderr.
        print('lost-ACK test worker failed', file=sys.stderr)
        sys.exit(1)
