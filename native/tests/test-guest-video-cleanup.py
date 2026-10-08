#!/usr/bin/env python3
"""Focused synthetic Guest terminal attestation tests; no screen capture/VM."""
import argparse
import json
import os
import pathlib
import socket
import struct
import subprocess
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parents[2]


def packet(value):
    data = b'\x01' + json.dumps(value).encode()
    return struct.pack('>I', len(data)) + data


def run(binary, mode, delegate_supported=True):
    with tempfile.TemporaryDirectory(prefix='gc-', dir=ROOT / '.local/run') as temp:
        path = pathlib.Path(temp) / 'socket'
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(path))
        os.chmod(path, 0o600)
        listener.listen(1)
        listener.settimeout(5)
        process = subprocess.Popen([str(binary), str(path), '--codec', 'h264'],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        conn = None
        began = time.monotonic()
        try:
            conn, _ = listener.accept()
            conn.settimeout(3)
            request = json.loads(conn.makefile('rb', buffering=0).readline(1025))
            assert request['id'] == 'host-start' and request['method'] == 'start'
            started = {'event': 'started', 'id': 'host-start', 'width': 1280, 'height': 720,
                       'pixel_format': 'nv12-video-range', 'color_matrix': 'ITU-R BT.709',
                       'color_space': 'ITU-R BT.709', 'capture_dynamic_range': 'SDR'}
            error = {'event': 'error', 'id': 'host-start', 'code': 'main_display_unavailable',
                     'main_display_active': False, 'main_display_asleep': True}
            terminal = {'event': 'terminated', 'id': 'host-start', 'reason': 'capture_failed',
                        'cleanup_confirmed': True, 'cleanup_basis': 'no_stream_created'}
            if mode.startswith('delegate-'):
                conn.sendall(packet(started))
                error.update(code='capture_stopped_with_error', error_domain='com.apple.ScreenCaptureKit.SCStreamErrorDomain', error_code=-3815)
                terminal['cleanup_basis'] = 'delegate_reported_stopped'
                if mode == 'delegate-wrong-code':
                    error['code'] = 'main_display_unavailable'
                elif mode == 'delegate-wrong-domain':
                    error['error_domain'] = 'not-Apple'
                elif mode == 'delegate-boolean-error':
                    error['error_code'] = True
                elif mode == 'delegate-missing-error':
                    del error['error_code']
                elif mode == 'delegate-clean-secondary':
                    terminal['secondary_stop_error'] = {'error_domain': 'com.apple.ScreenCaptureKit.SCStreamErrorDomain', 'error_code': -3808}
            if mode == 'normal-stop':
                conn.sendall(packet(started))
                time.sleep(.05)
                process.stdin.write(b'stop\n')
                process.stdin.flush()
                stop = json.loads(conn.makefile('rb', buffering=0).readline(1025))
                assert stop == {'id': 'host-stop', 'method': 'stop'}
                conn.sendall(packet({'event': 'stopped', 'id': 'host-stop'}))
                conn.shutdown(socket.SHUT_WR)
            else:
                if mode in ('after-start-clean', 'contradictory-basis'):
                    conn.sendall(packet(started))
                    if mode == 'after-start-clean':
                        terminal['cleanup_basis'] = 'stop_capture_completed'
                if mode != 'unsolicited-terminal':
                    conn.sendall(packet(error))
                if mode == 'wrong-id':
                    terminal['id'] = 'other'
                elif mode == 'false-confirmed':
                    terminal['cleanup_confirmed'] = False
                elif mode == 'numeric-confirmed':
                    terminal['cleanup_confirmed'] = 1
                elif mode == 'invalid-basis':
                    terminal['cleanup_basis'] = 'assumed'
                elif mode == 'wrong-reason':
                    terminal['reason'] = 'success'
                if mode == 'missing-terminal':
                    pass
                elif mode == 'stop-failed':
                    conn.sendall(packet({'event': 'error', 'code': 'stop_capture_failed'}))
                elif mode == 'fragmented-clean':
                    raw = packet(terminal)
                    for offset in range(0, len(raw), 3):
                        conn.sendall(raw[offset:offset + 3])
                        time.sleep(.001)
                else:
                    raw = packet(terminal)
                    if mode == 'trailing-byte':
                        raw += b'x'
                    if mode == 'duplicate-terminal':
                        raw += packet(terminal)
                    conn.sendall(raw)
                if mode != 'missing-eof':
                    conn.shutdown(socket.SHUT_WR)
            status = process.wait(timeout=4)
            stdout, stderr = process.communicate(timeout=1)
            assert not stdout, 'synthetic no-frame cases must not emit media'
            events = [json.loads(line) for line in stderr.splitlines()]
            result = events[-1]
            valid_failure = mode in ('early-clean', 'fragmented-clean', 'after-start-clean') or (delegate_supported and mode in ('delegate-clean', 'delegate-clean-secondary'))
            assert status == (0 if mode == 'normal-stop' else 78 if valid_failure else 1), (mode, status, result)
            assert result['guest_cleanup_confirmed'] == (valid_failure or mode == 'normal-stop'), (mode, result)
            assert result['event'] == ('stopped' if mode == 'normal-stop' else 'error'), result
            if valid_failure:
                assert result['guest_error']['code'] == ('capture_stopped_with_error' if mode.startswith('delegate-') else 'main_display_unavailable')
                assert result['cleanup_basis'] == terminal['cleanup_basis']
                if mode == 'delegate-clean-secondary':
                    assert result['secondary_stop_error'] == terminal['secondary_stop_error']
            return {'mode': mode, 'exit_code': status, 'elapsed_ms': round((time.monotonic()-began)*1000, 2),
                    'summary': result}
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=2)
            if conn:
                conn.close()
            listener.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=pathlib.Path, default=ROOT / '.local/bin/guest-video-stream-stage4')
    parser.add_argument('--legacy-binary', type=pathlib.Path)
    parser.add_argument('--output', type=pathlib.Path, required=True)
    args = parser.parse_args()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = {'scope': 'synthetic private Guest protocol; no Guest screen or actual SCK teardown', 'passed': False, 'cases': []}
    with args.output.open('x') as output:
        try:
            capability = json.loads(subprocess.check_output([str(args.binary), '--capabilities']))
            assert capability == {'protocol': 'veynode-guest-video-stream-v1', 'cleanup_contract_version': 1, 'cleanup_confirmed_failure_exit': 78}
            report['capabilities'] = capability
            for mode in ['normal-stop', 'early-clean', 'fragmented-clean', 'after-start-clean', 'missing-terminal',
                         'wrong-id', 'false-confirmed', 'numeric-confirmed', 'invalid-basis', 'wrong-reason',
                         'contradictory-basis', 'unsolicited-terminal', 'trailing-byte', 'duplicate-terminal',
                         'stop-failed', 'missing-eof', 'delegate-clean', 'delegate-clean-secondary',
                         'delegate-wrong-code', 'delegate-wrong-domain', 'delegate-boolean-error', 'delegate-missing-error']:
                try:
                    result = run(args.binary, mode)
                except Exception as error:
                    report['cases'].append({'mode': mode, 'passed': False, 'error': str(error), 'error_type': type(error).__name__})
                    raise
                report['cases'].append(result)
                print('PASS', mode, flush=True)
            if args.legacy_binary:
                report['legacy_new_basis_rejected'] = run(args.legacy_binary, 'delegate-clean', delegate_supported=False)
            report['passed'] = True
        finally:
            json.dump(report, output, indent=2)
            output.write('\n')


if __name__ == '__main__':
    main()
