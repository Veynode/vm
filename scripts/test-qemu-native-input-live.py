#!/usr/bin/env python3
"""Attach-only actual native-input acceptance. Manual CUA phases; no UI synthesis.

Requires an already-running disposable QEMU VM with linux-control-input-oracle.c.
Never starts/stops a VM or daemon, never issues human_takeover/human_release, and
never sends native-control events. Only baseline/final clicks use real MCP.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import select
import stat
import subprocess
import time
import unittest

MAX_REPLY = 1_048_576
MAX_LOG = 16 * 1024 * 1024


def require(condition, reason):
    if not condition:
        raise AssertionError(reason)


def parse_events(data):
    """Ignore only an incomplete tail; malformed complete oracle records fail."""
    output = {'xorg': [], 'evdev': []}
    for line in data.split(b'\n')[:-1]:
        for prefix, kind in [(b'X_INPUT ', 'xorg'), (b'RAW_INPUT ', 'evdev')]:
            if line.startswith(prefix):
                value = json.loads(line[len(prefix):])
                require(type(value) is dict, 'oracle_record_not_object')
                require(all(type(v) is int for v in value.values()), 'oracle_record_noninteger')
                require(type(value.get('type')) is int, 'oracle_record_missing_type')
                if kind == 'xorg':
                    require(value.get('synthetic') == 0, 'oracle_received_XSendEvent_synthetic_input')
                    require(value['type'] in (2, 3, 4, 5, 6), 'oracle_unknown_xorg_event')
                else:
                    require(set(value) == {'type', 'code', 'value'}, 'oracle_invalid_evdev_fields')
                output[kind].append(value)
    return output


def ordered_pair(events, down, up):
    found = False
    for event in events:
        if down(event):
            found = True
        elif found and up(event):
            return True
    return False


def button_pair(events, x=None, y=None):
    def match(event, kind):
        return event.get('type') == kind and event.get('button') == 1 and (
            (event.get('x') == x and event.get('y') == y) if x is not None else
            100 <= event.get('x', -1) < 500 and 100 <= event.get('y', -1) < 400)
    return (ordered_pair(events['xorg'], lambda e: match(e, 4), lambda e: match(e, 5)) and
            ordered_pair(events['evdev'], lambda e: e == {'type': 1, 'code': 272, 'value': 1},
                         lambda e: e == {'type': 1, 'code': 272, 'value': 0}))


def return_pair(events):
    return (ordered_pair(events['xorg'], lambda e: e.get('type') == 2 and e.get('keysym') == 65293,
                         lambda e: e.get('type') == 3 and e.get('keysym') == 65293) and
            ordered_pair(events['evdev'], lambda e: e == {'type': 1, 'code': 28, 'value': 1},
                         lambda e: e == {'type': 1, 'code': 28, 'value': 0}))


def held_button(events):
    x_down = [i for i, e in enumerate(events['xorg']) if e.get('type') == 4 and e.get('button') == 1]
    r_down = [i for i, e in enumerate(events['evdev']) if e == {'type': 1, 'code': 272, 'value': 1}]
    if not x_down or not r_down:
        return False
    return not (any(e.get('type') == 5 and e.get('button') == 1 for e in events['xorg'][x_down[-1]+1:]) or
                any(e == {'type': 1, 'code': 272, 'value': 0} for e in events['evdev'][r_down[-1]+1:]))


class Serial:
    def __init__(self, path):
        self.path = path
        self.fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
        self.initial = os.fstat(self.fd)
        require(stat.S_ISREG(self.initial.st_mode) and self.initial.st_uid == os.getuid(), 'serial_not_owned_regular_file')
        require(self.initial.st_size <= MAX_LOG, 'serial_prefix_exceeds_bound')
        self.start = self.initial.st_size
        self.prefix = os.pread(self.fd, self.start, 0)
        require(b'CONTROL_INPUT_READY\n' in self.prefix.replace(b'\r\n', b'\n'), 'actual_oracle_not_ready')
        require(b'CONTROL_INPUT_ORACLE_ENDED' not in self.prefix, 'oracle_already_exited')
        # No acceptance may borrow an event whose prefix was logged before start.
        self.boundary = not self.prefix or self.prefix.endswith(b'\n')

    def data(self):
        now = os.fstat(self.fd)
        named = self.path.lstat()
        require((named.st_dev, named.st_ino) == (self.initial.st_dev, self.initial.st_ino), 'serial_identity_changed')
        require(self.start <= now.st_size <= self.start + MAX_LOG, 'serial_truncated_or_exceeded_bound')
        data = os.pread(self.fd, now.st_size - self.start, self.start)
        require(b'CONTROL_INPUT_ORACLE_ENDED' not in data, 'oracle_exited_during_test')
        if not self.boundary:
            data = data.partition(b'\n')[2]
        return data

    def offset(self):
        # Mark only the complete-line boundary; a pending partial line is kept.
        data = self.data()
        return data.rfind(b'\n') + 1

    def since(self, offset):
        data = self.data()
        require(0 <= offset <= len(data), 'serial_offset_invalid')
        return parse_events(data[offset:])

    def summary(self):
        require(os.pread(self.fd, self.start, 0) == self.prefix, 'serial_prefix_changed')
        data = self.data()
        return {'device': self.initial.st_dev, 'inode': self.initial.st_ino,
                'prefix_offset': self.start, 'prefix_sha256': hashlib.sha256(self.prefix).hexdigest(),
                'appended_bytes': len(data), 'appended_sha256': hashlib.sha256(data).hexdigest()}


class MCP:
    def __init__(self, binary, state, timeout):
        self.p = subprocess.Popen([str(binary), 'mcp', '--state-dir', str(state), '--allow-input'],
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.buffer = bytearray()
        self.number = 0
        self.timeout = timeout
        try:
            value = self.rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                'clientInfo': {'name': 'native-input-live-acceptance', 'version': '1'}})
            require(value.get('result', {}).get('protocolVersion') == '2025-06-18', 'mcp_initialize_failed')
            self.send({'jsonrpc': '2.0', 'method': 'notifications/initialized'})
        except BaseException:
            self.dispose()
            raise

    def send(self, message):
        data = json.dumps(message).encode() + b'\n'
        require(len(data) <= 8192, 'mcp_fixture_request_bound')
        self.p.stdin.write(data)
        self.p.stdin.flush()

    def rpc(self, method, params):
        self.number += 1
        self.send({'jsonrpc': '2.0', 'id': self.number, 'method': method, 'params': params})
        deadline = time.monotonic() + self.timeout
        while b'\n' not in self.buffer:
            remaining = deadline - time.monotonic()
            require(remaining > 0 and select.select([self.p.stdout], [], [], remaining)[0], 'mcp_deadline')
            block = os.read(self.p.stdout.fileno(), 65536)
            require(block, 'mcp_eof_without_reply')
            self.buffer.extend(block)
            require(len(self.buffer) <= MAX_REPLY, 'mcp_response_bound')
        line, _, rest = self.buffer.partition(b'\n')
        self.buffer = bytearray(rest)
        reply = json.loads(line)
        require(reply.get('id') == self.number, 'mcp_response_id_mismatch')
        return reply

    def tool(self, name, arguments, denied=False):
        require(name in ('acquire_control', 'release_control', 'input_click', 'screenshot'), 'fixture_tool_not_allowlisted')
        reply = self.rpc('tools/call', {'name': 'vm_' + name, 'arguments': arguments})
        failed = 'error' in reply or reply.get('result', {}).get('isError') is True
        require(failed == denied, 'unexpected_mcp_tool_outcome:' + name)
        if denied:
            message = json.dumps(reply, sort_keys=True)
            require('control lease expired, revoked or belongs to another client' in message,
                    'mcp_denial_was_not_authority_fence')
            return {'rejected': True, 'reason': 'control lease expired, revoked or belongs to another client',
                    'response_sha256': hashlib.sha256(message.encode()).hexdigest()}
        content = reply['result']['content']
        if name == 'screenshot':
            metadata = [json.loads(c['text']) for c in content if c.get('type') == 'text']
            require(len(metadata) == 1, 'screenshot_metadata_ambiguous')
            value = metadata[0]
            require(re.fullmatch(r'[0-9a-f-]{36}', value.get('frame_id', '')), 'screenshot_frame_id_invalid')
            return {k: value[k] for k in ('frame_id', 'source_generation', 'width', 'height')}
        require(len(content) == 1 and content[0].get('type') == 'text', 'tool_content_invalid')
        return json.loads(content[0]['text'])

    def close(self):
        self.p.stdin.close()
        try:
            self.p.wait(timeout=42)
            require(self.p.returncode == 0, 'mcp_session_cleanup_unconfirmed')
        finally:
            self.dispose()

    def dispose(self):
        if self.p.poll() is None:
            self.p.kill()  # only this test's still-owned stdio child
            self.p.wait(timeout=3)
        for f in (self.p.stdin, self.p.stdout):
            if f and not f.closed:
                f.close()


def bounded_call(binary, state, method, vm, timeout):
    require(method in ('status', 'window_status', 'close_window', 'show_window'), 'fixture_rpc_not_allowlisted')
    p = subprocess.Popen([str(binary), 'call', '--state-dir', str(state), method, json.dumps({'id': vm})],
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    buffers = {p.stdout: bytearray(), p.stderr: bytearray()}
    remaining = set(buffers)
    deadline = time.monotonic() + timeout
    try:
        while remaining:
            left = deadline - time.monotonic()
            require(left > 0, 'controller_call_deadline')
            ready = select.select(list(remaining), [], [], left)[0]
            require(ready, 'controller_call_deadline')
            for stream in ready:
                block = os.read(stream.fileno(), 65536)
                if not block:
                    remaining.remove(stream)
                else:
                    buffers[stream].extend(block)
                    require(len(buffers[stream]) <= (MAX_REPLY if stream is p.stdout else 4096), 'controller_call_response_bound')
        p.wait(timeout=max(.1, deadline-time.monotonic()))
        require(p.returncode == 0, 'controller_call_failed:' + method)
        return json.loads(buffers[p.stdout])
    finally:
        if p.poll() is None:
            p.kill()
            p.wait(timeout=3)
        p.stdout.close()
        p.stderr.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--state', type=Path, required=True)
    parser.add_argument('--vm', required=True)
    parser.add_argument('--serial-log', type=Path, required=True)
    parser.add_argument('--helper', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True, help='new private evidence directory')
    parser.add_argument('--phase-timeout', type=float, default=120)
    parser.add_argument('--rpc-timeout', type=float, default=20)
    parser.add_argument('--skip-drag', action='store_true', help='explicitly mark held-drag acceptance untested; never claim a full pass')
    args = parser.parse_args()
    require(10 <= args.phase_timeout <= 300 and 3 <= args.rpc_timeout <= 40, 'timeouts_out_of_bounds')
    require(re.fullmatch(r'[A-Za-z0-9_-]{1,64}', args.vm), 'invalid_vm_id')
    binary, state, helper = (p.resolve(strict=True) for p in (args.binary, args.state, args.helper))
    output = args.output.absolute()
    output.mkdir(mode=0o700)
    socket_identity = (state/'control.sock').lstat()
    require(stat.S_ISSOCK(socket_identity.st_mode) and socket_identity.st_uid == os.getuid(), 'controller_socket_identity_invalid')
    report = {'passed': False, 'tested_subset_passed': False, 'started_at_unix': time.time(),
              'vm': args.vm, 'checks': [], 'phase': 'initializing', 'scope': __doc__,
              'held_drag_tested': False, 'held_key_focus_loss_tested': False,
              'manual_ui': 'External CUA only; this script performs no UI or host event injection',
              'artifact_hashes': {str(p): hashlib.sha256(p.read_bytes()).hexdigest()
                 for p in [binary, helper, Path(__file__).resolve(), binary.with_name('vm-qemu'), binary.with_name('vm-console')]}}
    clients = []
    serial = None
    identity = None

    def save():
        temp = output/'report.next'
        fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, 'w') as f:
            json.dump(report, f, indent=2)
            f.write('\n')
        os.replace(temp, output/'report.json')

    def phase(name, instruction):
        report['phase'] = name
        report['phase_instruction'] = instruction
        report['phase_started_at_unix'] = time.time()
        save()
        print(json.dumps({'phase': name, 'instruction': instruction}), flush=True)

    def record(name, evidence=True):
        require(len(json.dumps(evidence)) <= 512 * 1024, 'phase_evidence_bound')
        report['checks'].append({'name': name, 'evidence': evidence})
        save()

    def call(method):
        current = (state/'control.sock').lstat()
        require((current.st_dev, current.st_ino) == (socket_identity.st_dev, socket_identity.st_ino), 'controller_replaced')
        return bounded_call(binary, state, method, args.vm, args.rpc_timeout)

    def running():
        value = call('status')
        require(value['state'] == 'running' and value['config']['id'] == args.vm and value['config']['backend'] == 'qemu-hvf', 'existing_vm_not_running_qemu')
        actual = (value.get('pid'), value.get('runtime', {}).get('console_socket'))
        require(type(actual[0]) is int and actual[0] > 1 and actual[1], 'vm_runtime_identity_missing')
        if identity is not None:
            require(actual == identity, 'vm_worker_or_console_changed')
        return actual

    def status():
        value = call('window_status')
        require(value.get('native_window_protocol') == 2 and value.get('native_input_protocol') == 1, 'native_input_protocol_missing')
        for key in ('native_input_ready', 'native_human_hold', 'native_release_confirmed', 'window_open'):
            require(type(value.get(key)) is bool, 'native_status_type:' + key)
        require(value.get('native_input_state') != 'cleanup_unconfirmed', 'native_cleanup_unconfirmed')
        return value

    def compact(value):
        return {k: v for k, v in value.items() if k.startswith('native_') or k in ('window_open', 'window_visible', 'input_enabled', 'display_only', 'display_instance')}

    def wait(predicate, seconds, interval=.025):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            result = predicate()
            if result:
                return result
            time.sleep(interval)
        raise TimeoutError('phase_deadline:' + report['phase'])

    def client():
        value = MCP(binary, state, args.rpc_timeout)
        clients.append(value)
        return value

    def shot(c):
        return {'id': args.vm, **c.tool('screenshot', {'id': args.vm})}

    def click(c, x=200, y=200, denied=False):
        return c.tool('input_click', {**shot(c), 'x': x, 'y': y}, denied=denied)

    def deny_without_events(c):
        # Operator must not touch Guest image during this explicit quiet phase.
        time.sleep(.2)
        mark = serial.offset()
        result = click(c, denied=True)
        until = time.monotonic() + .6
        while time.monotonic() < until:
            events = serial.since(mark)
            require(not events['xorg'] and not events['evdev'], 'guest_events_during_denied_mcp_request')
            time.sleep(.025)
        return result

    try:
        identity = running()
        serial = Serial(args.serial_log.absolute())
        report['serial'] = serial.summary()
        old = client()
        old.tool('acquire_control', {'id': args.vm})
        lease_at = time.monotonic()
        mark = serial.offset()
        dispatch = click(old)
        require(dispatch.get('dispatched') is True and dispatch.get('guest_delivery_confirmed') is False, 'mcp_dispatch_contract')
        wait(lambda: button_pair(serial.since(mark), 200, 200), 8)
        record('baseline_mcp_click_guest_xorg_and_evdev', serial.since(mark))
        phase('wait-native-take', 'Within 45 seconds click native Take Control. Then keep pointer/keys out of Guest until next phase.')
        acquired = wait(lambda: (v if (v := status())['native_input_ready'] else None), min(45, args.phase_timeout), .2)
        require(time.monotonic()-lease_at < 55, 'old_lease_expired_before_takeover_proof')
        require(acquired['native_human_hold'] is True, 'takeover_without_human_hold')
        record('native_take_confirmed', compact(acquired))
        phase('checking-old-mcp-denied', 'Do not interact with Guest; script checks denial and independent event silence.')
        record('previous_mcp_denied_without_guest_events', deny_without_events(old))
        mark = serial.offset()
        phase('wait-native-click-return', 'Click INSIDE Guest oracle [100,100..500,400] in native image, then press Return. Do not click Return to Agent yet.')
        wait(lambda: button_pair(serial.since(mark)) and return_pair(serial.since(mark)), args.phase_timeout)
        record('native_click_and_return_actual_guest_xorg_evdev', serial.since(mark))
        if not args.skip_drag:
            mark = serial.offset()
            phase('wait-native-drag', 'Perform a slow native drag inside oracle. Script closes window immediately after observed held button-down; keep drag in progress until it closes.')
            wait(lambda: held_button(serial.since(mark)), args.phase_timeout, .01)
            before_close = serial.since(mark)
            require(held_button(before_close), 'button_already_released_before_close_dispatch')
            close_started = time.monotonic()
            closed = call('close_window')
            require(closed.get('window_open') is False and closed.get('native_input_ready') is False, 'window_close_did_not_locally_fence')
            record('window_closed_after_observed_held_button', {'before_close_events': before_close, 'window_ack': compact(closed), 'window_ack_is_release_proof': False})
            wait(lambda: button_pair(serial.since(mark)), 12)
            released = wait(lambda: (v if (v := status())['native_release_confirmed'] and not v['native_input_ready'] else None), 15, .1)
            require(released['native_human_hold'] is True, 'window_close_unexpectedly_returned_authority')
            report['held_drag_tested'] = True
            record('window_close_release_guest_up_and_separate_native_ack', {'events': serial.since(mark), 'status': compact(released), 'elapsed_since_close_request_s': time.monotonic()-close_started,
                'causal_limit': 'Guest up and ACK observed after close request; physical CUA drag release timing is external, not independently measured'})
        else:
            record('held_drag_not_tested', 'Explicit --skip-drag; no full acceptance pass claimed')
        phase('checking-human-hold', 'Do not interact; old MCP must still be rejected without Guest events.')
        record('human_hold_previous_mcp_still_denied', deny_without_events(old))
        shown = call('show_window')
        require(shown.get('window_open') is True, 'window_reopen_failed')
        phase('wait-return-agent', 'Click native Return to Agent explicitly. Do not touch Guest image during final MCP check.')
        returned = wait(lambda: (v if not (v := status())['native_human_hold'] and v['native_release_confirmed'] and not v['native_input_ready'] else None), args.phase_timeout, .2)
        record('explicit_return_to_agent_confirmed', compact(returned))
        fresh = client()
        fresh.tool('acquire_control', {'id': args.vm})
        mark = serial.offset()
        click(fresh, 230, 230)
        wait(lambda: button_pair(serial.since(mark), 230, 230), 8)
        record('fresh_mcp_lease_and_guest_click_restored', serial.since(mark))
        release = fresh.tool('release_control', {'id': args.vm})
        require(release.get('input_release') == {'confirmed': True, 'vm_count': 1, 'guest_delivery_confirmed': False}, 'final_mcp_release_unconfirmed')
        record('fresh_mcp_release_dispatch_confirmed', release)
        running()
        report['tested_subset_passed'] = True
        report['passed'] = not args.skip_drag
    except BaseException as error:
        report['error'] = str(error)[:2048]
        raise
    finally:
        cleanup = []
        for c in reversed(clients):
            try:
                c.close()
                cleanup.append({'exit_code': c.p.returncode, 'confirmed': True})
            except BaseException as error:
                c.dispose()
                cleanup.append({'confirmed': False, 'reason': str(error)[:200]})
                report['passed'] = report['tested_subset_passed'] = False
        report['mcp_cleanup'] = cleanup
        if serial:
            try:
                report['serial'] = serial.summary()
            except BaseException as error:
                report['serial_final_error'] = str(error)[:200]
                report['passed'] = report['tested_subset_passed'] = False
            os.close(serial.fd)
        report['finished_at_unix'] = time.time()
        report['phase'] = 'finished' if report['tested_subset_passed'] else 'failed'
        save()
        print(json.dumps({'phase': report['phase'], 'passed': report['passed'], 'report': str(output/'report.json')}), flush=True)
    require(report['tested_subset_passed'], 'acceptance_or_cleanup_failed')


class PureTests(unittest.TestCase):
    def test_newline_framing_and_malformed_complete(self):
        self.assertEqual(parse_events(b'RAW_INPUT {"type":1,"code":272,"value":1}\nX_INPUT {')[ 'evdev'], [{'type': 1, 'code': 272, 'value': 1}])
        with self.assertRaises(json.JSONDecodeError):
            parse_events(b'X_INPUT {broken}\n')
        with self.assertRaises(AssertionError):
            parse_events(b'X_INPUT {"type":4,"synthetic":1}\n')
        with self.assertRaises(AssertionError):
            parse_events(b'RAW_INPUT {"type":true,"code":272,"value":1}\n')

    def test_independent_ordered_down_up_and_held(self):
        down = {'xorg': [{'type': 4, 'button': 1, 'x': 200, 'y': 200}], 'evdev': [{'type': 1, 'code': 272, 'value': 1}]}
        self.assertTrue(held_button(down))
        self.assertFalse(button_pair(down))
        full = {k: list(v) for k, v in down.items()}
        full['xorg'].append({'type': 5, 'button': 1, 'x': 200, 'y': 200})
        self.assertFalse(button_pair(full))
        full['evdev'].append({'type': 1, 'code': 272, 'value': 0})
        self.assertTrue(button_pair(full, 200, 200))
        self.assertFalse(held_button(full))
        self.assertFalse(button_pair({k: list(reversed(v)) for k, v in full.items()}))
        self.assertFalse(return_pair(full))


if __name__ == '__main__':
    import sys
    if sys.argv[1:] == ['--self-test']:
        unittest.main(argv=[sys.argv[0]])
    else:
        main()
