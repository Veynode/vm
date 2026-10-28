#!/usr/bin/env python3
"""Collect one explicit Blender run via existing scoped stdio MCP; never runs Blender."""
import argparse
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
_spec = importlib.util.spec_from_file_location('macos_files_fixture', ROOT/'scripts/test-macos-files.py')
_fixture = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_fixture)
MCP = _fixture.MCP
MAX_FILE = 8*1024*1024
CHUNK = 32768


def require(value, message):
    if not value:
        raise ValueError(message)


def download(client, vm, name, output, deadline):
    evidence = {'name': name, 'integrity_verified': False, 'bytes_written': 0}
    token = None
    def call(method, **fields):
        require(time.monotonic() < deadline, 'collection deadline exceeded')
        return client.tool(method, {'id': vm, **fields})
    try:
        begin = call('file_download_begin', path=name)
        size, expected = begin.get('size'), begin.get('sha256')
        require(type(size) is int and 0 <= size <= MAX_FILE, 'invalid declared file size')
        require(isinstance(expected, str) and re.fullmatch('[0-9a-f]{64}', expected), 'invalid declared SHA256')
        token = begin.get('token')
        require(isinstance(token, str) and re.fullmatch('[0-9a-f]{32}', token), 'invalid download token')
        evidence.update(expected_bytes=size, expected_sha256=expected)
        digest = hashlib.sha256()
        while True:
            reply = call('file_download_read', token=token, offset=evidence['bytes_written'])
            encoded = reply.get('data')
            require(isinstance(encoded, str) and len(encoded) <= 4*((CHUNK+2)//3), 'oversized encoded chunk')
            block = base64.b64decode(encoded, validate=True)
            require(len(block) <= CHUNK, 'oversized decoded chunk')
            offset = evidence['bytes_written']+len(block)
            require(type(reply.get('offset')) is int and reply['offset'] == offset, 'download offset mismatch')
            require(offset <= size and offset <= MAX_FILE, 'download exceeds declared size')
            require(type(reply.get('eof')) is bool, 'invalid EOF marker')
            require(bool(block) or reply['eof'], 'download made no progress')
            # Preserve exact retrieved bytes even when final size/hash verification fails.
            output.write(block); digest.update(block); evidence['bytes_written'] = offset
            if reply['eof']:
                require(offset == size, 'early EOF')
                evidence['sha256'] = digest.hexdigest()
                require(evidence['sha256'] == expected, 'download SHA256 mismatch')
                evidence['integrity_verified'] = True
                break
        output.flush(); os.fsync(output.fileno())
    except Exception as error:
        # Do not serialize a backend response or token into the report.
        evidence['error_type'] = type(error).__name__
        evidence['error'] = str(error)[:200] if isinstance(error, ValueError) else 'MCP download failed'
        if token:
            try:
                reply = call('file_cancel', token=token)
                evidence['cancel_reply_received'] = type(reply.get('cancelled')) is bool
            except Exception:
                evidence['cancel_reply_received'] = False
    return evidence


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--state-dir', type=Path, required=True)
    parser.add_argument('--id', required=True)
    parser.add_argument('--run-id', required=True)
    parser.add_argument('--output-dir', type=Path, required=True)
    parser.add_argument('--png', action='store_true', help='Also collect the exact optional render PNG')
    parser.add_argument('--launcher', action='store_true', help='Also collect the v3 supervisor result')
    args = parser.parse_args()
    require(re.fullmatch('[a-z0-9][a-z0-9-]{0,47}', args.run_id), 'invalid run ID')
    require(args.output_dir.is_dir(), 'output directory must already exist')
    stem = 'blender-'+args.run_id
    names = [stem+'.json', stem+'.log'] + ([stem+'.png'] if args.png else [])
    if args.launcher: names.append(stem+'.launcher.json')
    names.append(stem+'.collection.json')
    outputs = {}
    # Reserve every destination before connecting; never clobber older evidence.
    try:
        for name in names:
            fd = os.open(args.output_dir/name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            outputs[name] = os.fdopen(fd, 'wb')
    except BaseException:
        for stream in outputs.values(): stream.close()
        raise
    report = {'schema': 1, 'run_id': args.run_id, 'vm_id': args.id,
              'collection_passed': False, 'workload_passed': False,
              'mcp_close_confirmed': False, 'files': [],
              'scope': 'Exact run artifact integrity and MCP process cleanup; no new render or GPU acceptance'}
    client = MCP.__new__(MCP)  # Retain child ownership even if initialization raises.
    deadline = time.monotonic()+60
    try:
        MCP.__init__(client, args.binary.resolve(), args.state_dir.resolve(), files=True)
        for name in names[:-1]:
            report['files'].append(download(client, args.id, name, outputs[name], deadline))
        valid = all(x['integrity_verified'] for x in report['files'])
        for name in names[:-1]: outputs[name].flush()
        if valid:
            try:
                diagnostic = json.loads((args.output_dir/names[0]).read_bytes())
                report['workload_passed'] = diagnostic.get('passed') is True and (
                    diagnostic.get('mode') == 'probe' or diagnostic.get('rendered') is True)
                report['workload_passed'] &= all(x['bytes_written'] > 0 for x in report['files'])
                report['metal_device_available'] = diagnostic.get('metal_device_available') is True
                report['workload_mode'] = diagnostic.get('mode')
                report['rendered'] = diagnostic.get('rendered') is True
                if args.png:
                    image = (args.output_dir/(stem+'.png')).read_bytes()
                    report['png_matches_diagnostic'] = bool(image) and image[:8] == b'\x89PNG\r\n\x1a\n' and (
                        len(image) == diagnostic.get('image_bytes') and hashlib.sha256(image).hexdigest() == diagnostic.get('image_sha256'))
                    report['workload_passed'] &= report['png_matches_diagnostic']
            except (ValueError, TypeError, AttributeError):
                report['workload_error'] = 'diagnostic JSON is empty or invalid'
        if valid and args.launcher:
            try:
                launcher = json.loads((args.output_dir/(stem+'.launcher.json')).read_bytes())
                report['launcher_succeeded'] = (launcher.get('launcher_version') == 3 and launcher.get('run_id') == args.run_id and launcher.get('launcher_succeeded') is True and launcher.get('owned_process_reaped') is True and launcher.get('exit_code') == 0 and launcher.get('forced_kill') is not True)
                report['workload_passed'] &= report['launcher_succeeded']
            except (ValueError, TypeError, AttributeError):
                report['launcher_succeeded'] = False
                report['workload_passed'] = False
        report['collection_passed'] = valid
    except Exception as error:
        report['error_type'] = type(error).__name__
        report['error'] = 'MCP initialization or collection failed'
    finally:
        if hasattr(client, 'process'):
            try:
                if not client.process.stdin.closed: client.process.stdin.close()
                code = client.process.wait(timeout=45)
                report['mcp_exit_code'] = code
                report['mcp_close_confirmed'] = code == 0
            except Exception:
                report['cleanup_error'] = 'MCP did not confirm successful close'
                # Only reap our retained, unreaped child; forced exit never proves cleanup.
                if client.process.poll() is None:
                    client.process.kill()
                    try: client.process.wait(timeout=2)
                    except subprocess.TimeoutExpired: report['owned_child_reap_failed'] = True
            finally:
                for stream in (client.process.stdin, client.process.stdout, client.process.stderr):
                    if stream and not stream.closed: stream.close()
        report['collection_passed'] &= report['mcp_close_confirmed']
        outputs[names[-1]].write((json.dumps(report, indent=2)+'\n').encode())
        for stream in outputs.values():
            stream.flush(); os.fsync(stream.fileno()); stream.close()
    print(json.dumps(report, indent=2))
    return 0 if report['collection_passed'] and report['workload_passed'] else 1

if __name__ == '__main__':
    raise SystemExit(main())
