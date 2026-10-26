#!/usr/bin/env python3
"""Real daemon file probe. Default: reproduce writes without publication.
--acceptance: publish two unique fixtures, then verify actual downloads byte for
byte and with SHA256. Published fixtures intentionally remain in the Guest root;
only temporary transfers are cleaned. No VM/service lifecycle changes.
"""
import argparse
import base64
import hashlib
import json
import os
import pathlib
import secrets
import socket
import time

CHUNK = 32768
MAX_FILE = 8388608


class CheckFailure(Exception):
    """Contains only a fixed test classification, never protocol data."""


def check(condition, code):
    if not condition:
        raise CheckFailure(code)


def integer(value):
    return type(value) is int and value >= 0


def validate_cleanup(reply):
    check(isinstance(reply, dict) and 'error' not in reply, 'cleanup_controller_error')
    v = reply.get('result', {})
    check(v.get('closed') is True, 'cleanup_unconfirmed')
    check(set(v) == {'closed', 'job_cancellation', 'file_cancellation'}, 'unexpected_cleanup_fields')
    jobs = v.get('job_cancellation', {})
    check(type(jobs.get('attempted')) is int and type(jobs.get('unconfirmed_requests')) is int
          and jobs.get('attempted') == 0 and jobs.get('unconfirmed_requests') == 0
          and jobs.get('outcomes') == [], 'unexpected_jobs')
    files = v.get('file_cancellation', {})
    outcomes = files.get('outcomes')
    check(isinstance(outcomes, list) and integer(files.get('attempted'))
          and files['attempted'] == len(outcomes)
          and type(files.get('unconfirmed_requests')) is int
          and files['unconfirmed_requests'] == 0, 'cleanup_count_invalid')
    count = 0
    for outcome in outcomes:
        check(isinstance(outcome, dict) and 'error' not in outcome, 'cleanup_guest_error')
        result = outcome.get('result', {})
        check('error' not in result, 'cleanup_guest_error')
        cancelled = result.get('result', {}).get('cancelled_transfers')
        check(integer(cancelled), 'cleanup_ack_invalid')
        count += cancelled
    return count


class Controller:
    def __init__(self, state, vm, seconds):
        self.socket = str(pathlib.Path(state).resolve() / 'control.sock')
        self.vm = vm
        self.deadline = time.monotonic() + seconds

    def rpc(self, method, args, cleanup=False):
        wire = json.dumps({'method': method, 'arguments': args}, separators=(',', ':')).encode() + b'\n'
        check(len(wire) <= 65536, 'request_bound_exceeded')
        seconds = 12 if cleanup or method == 'session_end' else min(12, self.deadline - time.monotonic())
        check(seconds > 0, 'probe_deadline')
        deadline = time.monotonic() + seconds
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(seconds)
            stream.connect(self.socket)
            stream.sendall(wire)
            data = bytearray()
            while not data.endswith(b'\n'):
                remaining = deadline - time.monotonic()
                check(remaining > 0, 'rpc_deadline')
                stream.settimeout(remaining)
                part = stream.recv(65536)
                check(bool(part), 'incomplete_controller_response')
                data.extend(part)
                check(len(data) <= 1048576, 'controller_response_bound')
        return json.loads(data)

    def guest(self, method, owner=None, **args):
        args['id'] = self.vm
        if owner is not None:
            args['owner'] = owner
        reply = self.rpc(method, args)
        check('error' not in reply, method + '_controller_error')
        result = reply.get('result', {})
        check(isinstance(result, dict) and 'error' not in result
              and isinstance(result.get('result'), dict), method + '_guest_error')
        return result['result']

    def observe_clean(self):
        # Independent connections and unrelated observer owner. The observer
        # cannot inherit or clean the transfer owner's session.
        observer = secrets.token_hex(32)
        try:
            reply = self.rpc('guest_info', {'id': self.vm}, cleanup=True)
            check('error' not in reply, 'independent_observation_error')
            info = reply.get('result', {}).get('result', {})
            policy = info.get('file_policy', {})
            check(info.get('capabilities', {}).get('files') is True, 'independent_files_unavailable')
            check(type(policy.get('active_transfers')) is int
                  and type(policy.get('staged_upload_files')) is int, 'independent_policy_missing')
            check(policy['active_transfers'] == 0 and policy['staged_upload_files'] == 0,
                  'independent_transfers_not_empty')
            return {'active_transfers': 0, 'staged_upload_files': 0}
        finally:
            check(validate_cleanup(self.rpc('session_end', {'owner': observer}, cleanup=True)) == 0,
                  'independent_observer_cleanup_invalid')


def download_verified(controller, owner, name, expected, report):
    digest = hashlib.sha256(expected).hexdigest()
    begin = controller.guest('file_download_begin', owner, path=name)
    check(begin.get('size') == len(expected) and begin.get('sha256') == digest
          and begin.get('chunk_bytes') == CHUNK, 'download_identity_mismatch')
    token = begin['token']
    offset = 0
    hashed = hashlib.sha256()
    chunks = 0
    while True:
        value = controller.guest('file_download_read', owner, token=token, offset=offset)
        encoded = value.get('data')
        check(isinstance(encoded, str) and len(encoded) <= 43692, 'download_encoding_bound')
        try:
            data = base64.b64decode(encoded, validate=True)
        except Exception:
            raise CheckFailure('download_invalid_base64') from None
        check(base64.b64encode(data).decode() == encoded and len(data) <= CHUNK,
              'download_noncanonical_chunk')
        check(type(value.get('eof')) is bool and (value['eof'] or len(data) > 0), 'download_eof_invalid')
        check(type(value.get('offset')) is int and value['offset'] == offset + len(data)
              and offset + len(data) <= len(expected) and value.get('sha256') == digest,
              'download_offset_or_digest_mismatch')
        check(len(data) == min(CHUNK, len(expected) - offset), 'download_chunk_size_mismatch')
        check(data == expected[offset:offset + len(data)], 'download_bytes_mismatch')
        hashed.update(data)
        offset += len(data)
        chunks += 1
        if value['eof']:
            check(offset == len(expected), 'download_early_eof')
            break
        check(offset < len(expected), 'download_missing_eof')
    check(hashed.hexdigest() == digest, 'download_sha256_mismatch')
    report.update(download_chunks=chunks, downloaded_bytes=offset,
                  downloaded_sha256=hashed.hexdigest(), byte_comparison_passed=True)


def acceptance_case(controller, data, label, report):
    owner = secrets.token_hex(32)
    name = 'backend-acceptance-' + secrets.token_hex(12) + '.bin'
    digest = hashlib.sha256(data).hexdigest()
    report.update(label=label, size=len(data), sha256=digest, fixture_name=name,
                  published=False, passed=False, cleanup_confirmed=False)
    file_request_started = False
    try:
        report['baseline'] = controller.observe_clean()
        file_request_started = True
        begin = controller.guest('file_upload_begin', owner, path=name, size=len(data), sha256=digest)
        check(begin.get('size') == len(data) and begin.get('sha256') == digest
              and begin.get('chunk_bytes') == CHUNK, 'upload_identity_mismatch')
        token = begin['token']
        chunks = 0
        for offset in range(0, len(data), CHUNK):
            value = controller.guest('file_upload_write', owner, token=token, offset=offset,
                                     data=base64.b64encode(data[offset:offset + CHUNK]).decode())
            check(type(value.get('offset')) is int and value['offset'] == offset + len(data[offset:offset + CHUNK]),
                  'upload_offset_mismatch')
            chunks += 1
        report['upload_chunks'] = chunks
        commit = controller.guest('file_upload_commit', owner, token=token)
        check(commit.get('published') is True and commit.get('size') == len(data)
              and commit.get('sha256') == digest, 'publication_unconfirmed')
        report['published'] = True
        download_verified(controller, owner, name, data, report)
        report['transfer_verified'] = True
    except CheckFailure as error:
        report['transfer_failure'] = str(error)
        raise
    except Exception as error:
        report['transfer_exception_type'] = type(error).__name__
        raise
    finally:
        cleanup = controller.rpc('session_end', {'owner': owner}, cleanup=True)
        count = validate_cleanup(cleanup)
        check(cleanup['result']['file_cancellation']['attempted'] == (1 if file_request_started else 0),
              'cleanup_file_owner_route_missing')
        report.update(cleanup_confirmed=True, cancelled_transfers=count)
        report['independent_after_close'] = controller.observe_clean()
    check(count == 0, 'completed_transfers_unexpectedly_active')
    report['passed'] = True


def diagnostic(controller, data, report):
    owner = secrets.token_hex(32)
    report.update(offsets=[], all_chunks_accepted=False, cleanup_confirmed=False,
                  size=len(data), sha256=hashlib.sha256(data).hexdigest())
    try:
        result = controller.rpc('file_upload_begin', {'id': controller.vm, 'owner': owner,
            'path': 'backend-probe-' + secrets.token_hex(12) + '.tmp', 'size': len(data), 'sha256': report['sha256']})
        if 'error' in result:
            report['private_error'] = result['error']
            return
        token = result['result']['result']['token']
        for offset in range(0, len(data), CHUNK):
            encoded = base64.b64encode(data[offset:offset + CHUNK]).decode()
            result = controller.rpc('file_upload_write', {'id': controller.vm, 'owner': owner,
                                                         'token': token, 'offset': offset, 'data': encoded})
            if 'error' in result:
                report.update(failed_offset=offset, failed_chunk_base64_bytes=len(encoded),
                              failed_chunk_forward_slashes=encoded.count('/'), private_error=result['error'])
                break
            accepted = result['result']['result']['offset']
            check(type(accepted) is int and accepted == min(len(data), offset + CHUNK), 'upload_offset_mismatch')
            report['offsets'].append(accepted)
        else:
            report['all_chunks_accepted'] = True
    finally:
        report['cancelled_transfers'] = validate_cleanup(controller.rpc('session_end', {'owner': owner}, cleanup=True))
        report['cleanup_confirmed'] = True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for arg in ['state', 'vm', 'source', 'output']:
        parser.add_argument('--' + arg, required=True)
    parser.add_argument('--acceptance', action='store_true', help='publish unique 8MiB and32KiB fixtures and verify both directions')
    args = parser.parse_args()
    fd = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    report = {'schema': 2, 'mode': 'acceptance' if args.acceptance else 'diagnostic',
              'scope': 'actual private daemon to existing Guest files; no browser',
              'started_at': time.time(), 'passed': False}
    try:
        with open(args.source, 'rb') as source:
            data = source.read(MAX_FILE + 1)
        check(0 < len(data) <= MAX_FILE, 'source_size_invalid')
        controller = Controller(args.state, args.vm, 180 if args.acceptance else 60)
        if args.acceptance:
            check(len(data) == MAX_FILE and all(data[i * CHUNK:(i + 1) * CHUNK] == bytes([i]) * CHUNK
                                              for i in range(256)), 'acceptance_pattern_mismatch')
            report['cases'] = []
            for label, body in [('pattern256-8MiB', data), ('all-FF-32KiB', bytes([255]) * CHUNK)]:
                case = {}
                report['cases'].append(case)
                acceptance_case(controller, body, label, case)
        else:
            diagnostic(controller, data, report)
        report['completed'] = True
        report['passed'] = True if args.acceptance else report['all_chunks_accepted']
    except CheckFailure as error:
        report['failure'] = str(error)  # fixed classification only
    except Exception as error:
        report['exception_type'] = type(error).__name__
    finally:
        report['finished_at'] = time.time()
        with os.fdopen(fd, 'w') as output:
            json.dump(report, output, indent=2)
            output.write('\n')
            output.flush()
            os.fsync(output.fileno())
        print(json.dumps({key: report.get(key) for key in ['mode', 'passed', 'all_chunks_accepted',
                         'failed_offset', 'failed_chunk_forward_slashes', 'cleanup_confirmed', 'failure']}))
    success = report['passed'] if args.acceptance else report.get('completed') and report.get('cleanup_confirmed')
    return 0 if success else 1


if __name__ == '__main__':
    raise SystemExit(main())
