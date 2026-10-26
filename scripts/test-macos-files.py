#!/usr/bin/env python3
"""Actual already-running macOS Guest -> controller -> scoped stdio MCP files.

Never starts/stops a VM, grants permissions, or enables Guest execution. Requires
the explicit private test directory prepared by macos-files-fixture.sh. Generated
published files remain in that disposable directory; partial files must clean up.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import select
import subprocess
import time
import uuid


class MCP:
    def __init__(self, binary, state, files=False):
        self.process = subprocess.Popen(
            [str(binary), 'mcp', '--state-dir', str(state), *(['--allow-files'] if files else [])],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.number = 0
        self.buffer = bytearray()
        init = self.rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
            'clientInfo': {'name': 'macos-files-acceptance', 'version': '1'}})
        assert init['result']['protocolVersion'] == '2025-06-18', init
        self.send({'jsonrpc': '2.0', 'method': 'notifications/initialized'})

    def send(self, value):
        self.process.stdin.write(json.dumps(value).encode() + b'\n')
        self.process.stdin.flush()

    def rpc(self, method, params):
        self.number += 1
        self.send({'jsonrpc': '2.0', 'id': self.number, 'method': method, 'params': params})
        end = time.monotonic() + 40
        while b'\n' not in self.buffer:
            left = end - time.monotonic()
            assert left > 0 and select.select([self.process.stdout], [], [], left)[0], 'MCP deadline'
            chunk = os.read(self.process.stdout.fileno(), 65536)
            assert chunk, 'MCP EOF before response'
            self.buffer.extend(chunk)
            assert len(self.buffer) <= 1024 * 1024, 'oversized MCP response'
        line, _, rest = self.buffer.partition(b'\n')
        self.buffer = bytearray(rest)
        value = json.loads(line)
        assert value['id'] == self.number, value
        return value

    def tool(self, name, arguments, denied=False):
        reply = self.rpc('tools/call', {'name': 'vm_' + name, 'arguments': arguments})
        failed = 'error' in reply or reply.get('result', {}).get('isError', False)
        assert bool(failed) == denied, reply
        return reply if denied else json.loads(reply['result']['content'][0]['text'])['result']

    def close(self):
        if not self.process.stdin.closed:
            self.process.stdin.close()
        self.process.wait(timeout=10)
        assert self.process.returncode == 0, 'MCP cleanup exited unsuccessfully'


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary', type=Path, required=True)
    p.add_argument('--state-dir', type=Path, required=True)
    p.add_argument('--id', required=True)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--maximum', action='store_true', help='Also transfer the exact 8MiB boundary')
    p.add_argument('--expiry', action='store_true', help='Exercise actual 60second idle expiration')
    args = p.parse_args()
    fd = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    report = {'passed': False, 'started_at_unix': time.time(), 'checks': [],
              'scope': 'Actual macOS private-root same-UID file transfer; not command sandboxing'}
    clients = []
    def client(files=False):
        c = MCP(args.binary.resolve(), args.state_dir.resolve(), files)
        clients.append(c)
        return c
    def tool(c, name, denied=False, **kw):
        return c.tool(name, {'id': args.id, **kw}, denied)
    def record(name, value=True):
        report['checks'].append({'name': name, 'evidence': value})
        print(name, flush=True)
    def digest(data):
        return hashlib.sha256(data).hexdigest()
    def upload(c, name, data):
        return tool(c, 'file_upload_begin', path=name, size=len(data), sha256=digest(data))['token']
    def append(c, token, data):
        for offset in range(0, len(data), 32768):
            block = data[offset:offset + 32768]
            reply = tool(c, 'file_upload_write', token=token, offset=offset,
                         data=base64.b64encode(block).decode())
            assert reply['offset'] == offset + len(block), reply
    def download(c, name, foreign=None):
        begin = tool(c, 'file_download_begin', path=name)
        token, data = begin['token'], bytearray()
        if foreign:
            tool(foreign, 'file_download_read', denied=True, token=token, offset=0)
        while True:
            chunk = tool(c, 'file_download_read', token=token, offset=len(data))
            decoded = base64.b64decode(chunk['data'], validate=True)
            assert len(decoded) <= 32768
            data.extend(decoded)
            assert chunk['offset'] == len(data), chunk
            assert len(data) <= 8388608
            if chunk['eof']:
                break
        assert digest(data) == begin['sha256'] and len(data) == begin['size'], begin
        return bytes(data)
    def no_partial():
        end = time.monotonic() + 5
        while True:
            policy = tool(observer, 'guest_info')['file_policy']
            if policy['active_transfers'] == 0 and policy['staged_upload_files'] == 0:
                return policy
            assert time.monotonic() < end, policy
            time.sleep(.05)
    try:
        observer, files, other = client(), client(True), client(True)
        info = tool(observer, 'guest_info')
        assert info['os'] == 'darwin' and info['machine'].startswith('VirtualMac'), info
        assert info['uid'] != 0 and info['capabilities']['files'], info
        assert info['capabilities']['exec'] is False and info['capabilities']['async_exec'] is False
        assert info['file_policy']['uid'] == info['uid'], info
        assert info['file_policy']['max_file_bytes'] == 8388608
        assert info['file_policy']['chunk_bytes'] == 32768
        report['guest'] = info
        manifest = json.loads(download(files, 'fixture-manifest.json'))
        assert manifest == {'fixture': 'macos-files-v1', 'symlink': True, 'hardlink': True,
                            'fifo': True, 'directory': True}, manifest
        record('actual Guest identity and explicit private-root fixture', info['file_policy'])
        tool(observer, 'file_download_begin', denied=True, path='fixture-manifest.json')
        tool(files, 'exec', denied=True, argv=['/usr/bin/true'])
        tool(files, 'input_key', denied=True, keys=['esc'])
        tool(files, 'file_upload_begin', denied=True, path='owner-injection', size=0,
             sha256=digest(b''), owner='a' * 64)
        record('file scope independent of observation/input/execute; owner injection denied')
        bodies = [b'', b'\x00\xff' + '你好 ✓\n'.encode() * 10000 + os.urandom(8192)]
        if args.maximum:
            bodies.append(bytes(range(256)) * 32768)
        for data in bodies:
            name = 'roundtrip-' + uuid.uuid4().hex + '.bin'
            token = upload(files, name, data)
            tool(other, 'file_upload_write', denied=True, token=token, offset=0, data='')
            tool(files, 'file_download_begin', denied=True, path=name)
            append(files, token, data)
            committed = tool(files, 'file_upload_commit', token=token)
            assert committed['published'] and committed['sha256'] == digest(data), committed
            tool(files, 'file_upload_begin', denied=True, path=name, size=0, sha256=digest(b''))
            assert download(files, name, other) == data
            record('atomic roundtrip and foreign-owner refusal', {'bytes': len(data), 'sha256': digest(data), 'path': name})
        for path in ['../outside', '/etc/passwd', 'outside-link', 'outside-hardlink',
                     'fifo', 'sub', 'sub/file', '.veynode-staging', 'a\\b', '.', '..',
                     '../\u0301escaped', 'back\\\u0301slash', 'nul\x00\u0301suffix']:
            tool(files, 'file_download_begin', denied=True, path=path)
            tool(files, 'file_upload_begin', denied=True, path=path, size=0, sha256=digest(b''))
        record('actual symlink/hardlink/FIFO/directory and traversal rejection')
        partial = tool(files, 'file_download_begin', path=name)
        tool(files, 'file_download_read', denied=True, token=partial['token'], offset=1)
        first = tool(files, 'file_download_read', token=partial['token'], offset=0)
        assert not first['eof'] and first['offset'] == 32768, first
        assert tool(files, 'file_cancel', token=partial['token'])['cancelled']
        tool(files, 'file_download_read', denied=True, token=partial['token'], offset=32768)
        record('sequential download offsets and explicit cancellation', no_partial())
        tool(files, 'file_upload_begin', denied=True, path='oversize', size=8388609, sha256='0' * 64)
        token = upload(files, 'cancel-' + uuid.uuid4().hex, b'abcdef')
        for kw in [{'offset': 1, 'data': 'YQ=='}, {'offset': 0, 'data': 'not-base64!'},
                   {'offset': 0, 'data': base64.b64encode(b'x' * 32769).decode()}]:
            tool(files, 'file_upload_write', denied=True, token=token, **kw)
        assert tool(files, 'file_cancel', token=token)['cancelled']
        record('size/chunk/offset/encoding bounds and cancellation', no_partial())
        name = 'hash-fail-' + uuid.uuid4().hex
        token = tool(files, 'file_upload_begin', path=name, size=3, sha256='0' * 64)['token']
        append(files, token, b'abc')
        tool(files, 'file_upload_commit', denied=True, token=token)
        tool(files, 'file_download_begin', denied=True, path=name)
        record('checksum failure leaves no publication or staging', no_partial())
        token = upload(files, 'eof-' + uuid.uuid4().hex, b'partial-file')
        append(files, token, b'partial')
        files.close()
        clients.remove(files)
        tool(other, 'file_upload_commit', denied=True, token=token)
        record('actual MCP EOF removes partial upload and retires ownership', no_partial())
        if args.expiry:
            token = upload(other, 'expiry-' + uuid.uuid4().hex, b'partial-file')
            append(other, token, b'partial')
            assert tool(observer, 'guest_info')['file_policy']['staged_upload_files'] == 1
            print('waiting for actual 60second idle expiration', flush=True)
            end = time.monotonic() + 62
            while time.monotonic() < end:
                time.sleep(min(1, end - time.monotonic()))
            tool(other, 'file_upload_commit', denied=True, token=token)
            record('actual idle expiration removes partial upload', no_partial())
        report['passed'] = True
    except BaseException as error:
        report['error'] = repr(error)
        raise
    finally:
        for c in clients:
            try:
                c.close()
            except Exception as error:
                report.setdefault('cleanup_errors', []).append(repr(error))
                report['passed'] = False
        report['finished_at_unix'] = time.time()
        with os.fdopen(fd, 'w') as output:
            json.dump(report, output, indent=2)
            output.write('\n')
    assert report['passed'], report.get('cleanup_errors')
    print('PASS: actual macOS Guest file roundtrips and scoped cleanup')


if __name__ == '__main__':
    main()
