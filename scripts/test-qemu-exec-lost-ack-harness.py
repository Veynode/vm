#!/usr/bin/env python3
"""Host-only protocol/pipe regression tests. These are NOT Guest acceptance."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import time
import unittest

HERE = Path(__file__).resolve().parent


def load(name, path):
    s = importlib.util.spec_from_file_location(name, path)
    m = importlib.util.module_from_spec(s)
    s.loader.exec_module(m)
    return m


shim = load('lost_ack_shim', HERE / 'qemu-exec-lost-ack-worker.py')
acceptance = load('lost_ack_acceptance', HERE / 'test-installed-qemu-exec-lost-ack.py')
TAG = 'veynode-lost-ack-' + '1' * 32
SCOPE = {'owner': 'a' * 64, 'epoch': 17}


def start(ident=3, tag=TAG, scope=SCOPE):
    return {'id': ident, 'method': 'guest_request', 'request': {'id': str(ident),
        'method': 'exec_start', 'scope': scope, 'argv': ['/bin/sh', '-c', 'true', tag]}}


def ack(ident=3):
    return {'id': ident, 'result': {'id': str(ident), 'result': {'job_id': 'b' * 32, 'state': 'running'}}}


def revoke():
    return {'id': 4, 'method': 'guest_scope_revoke', 'request': {'id': '4', 'method': 'scope_revoke', 'scope': SCOPE}}


def empty():
    return {'id': 4, 'result': {'id': '4', 'result': {'revoked': True, 'pending_starts': 0, 'cgroup_empty': True}}}


def wire(value):
    return json.dumps(value).encode() + b'\n'


class Rules(unittest.TestCase):
    def setUp(self):
        self.events = []
        self.f = shim.Filter(TAG, lambda event, **data: self.events.append({'event': event, **data}))

    def test_only_exact_successful_ack_dropped_and_real_revoke_forwarded(self):
        self.f.incoming(wire(start()))
        self.assertTrue(self.f.outgoing(wire(ack(90))))
        self.assertFalse(self.f.outgoing(wire(ack())))
        self.f.incoming(wire(revoke()))
        self.assertTrue(self.f.outgoing(wire(empty())))
        self.assertEqual([v['event'] for v in self.events], ['target_start', 'successful_ack_dropped',
                         'original_scope_revoke', 'original_scope_empty_ack'])
        self.assertNotIn(SCOPE['owner'], json.dumps(self.events))
        self.assertNotIn('b' * 32, json.dumps(self.events))

    def test_error_wrong_nested_id_and_duplicate_cannot_count_as_dropped_start(self):
        self.f.incoming(wire(start()))
        for value in ({'id': 3, 'error': 'failed'}, {'id': 3, 'result': {'id': '99', 'result': ack()['result']['result']}}):
            with self.assertRaises(RuntimeError):
                self.f.outgoing(wire(value))
        self.assertFalse(self.f.dropped)
        self.f.outgoing(wire(ack()))
        with self.assertRaises(RuntimeError):
            self.f.outgoing(wire(ack()))

    def test_original_owner_second_start_fails_but_other_scope_observer_allowed(self):
        self.f.incoming(wire(start()))
        with self.assertRaises(RuntimeError):
            self.f.incoming(wire(start(9, 'observer')))
        self.f.incoming(wire(start(10, 'observer', {'owner': 'c' * 64, 'epoch': 18})))

    def test_nonempty_or_error_cleanup_is_not_acknowledged(self):
        self.f.incoming(wire(start()))
        for result in ({'incarnation_terminated': True}, {'revoked': True, 'pending_starts': 0, 'cgroup_empty': False}):
            self.f.incoming(wire(revoke()))
            with self.assertRaises(RuntimeError):
                self.f.outgoing(wire({'id': 4, 'result': {'id': '4', 'result': result}}))
        self.assertNotIn('original_scope_empty_ack', [v['event'] for v in self.events])

    def test_oracle_requires_real_writes_separate_session_and_pre_timeout_window(self):
        valid = '100.0\n122.5\n50\nabsent\n0::/veynode-exec/job-abc\n102 101 101\n90\n'
        self.assertEqual(acceptance.parse_observation(valid)['guest_elapsed_seconds'], 22.5)
        for changed in (valid.replace('50\n', '0\n'), valid.replace('absent', 'present'),
                        valid.replace('122.5', '130'), valid.replace('102 101 101', '101 101 101'),
                        valid.replace('\n90\n', '\n101\n'), valid.replace('job-abc', '../bad')):
            with self.assertRaises(AssertionError):
                acceptance.parse_observation(changed)

    def test_guest_shell_syntax(self):
        for value in (acceptance.OUTER, acceptance.CHILD, acceptance.READ):
            subprocess.run(['/bin/sh', '-n', '-c', value], check=True, timeout=2)


FAKE = '''#!/usr/bin/env python3
import json,os,sys,time
def send(v):
 b=(json.dumps(v)+'\\n').encode()
 for i in range(0,len(b),7):
  os.write(1,b[i:i+7]); time.sleep(.001)
send({'event':'state','state':'running'})
for line in sys.stdin.buffer:
 v=json.loads(line); r=v.get('request',{})
 if r.get('method')=='exec_start':
  result={'job_id':'b'*32,'state':'running'}
 elif r.get('method')=='scope_revoke':
  result={'revoked':True,'pending_starts':0,'cgroup_empty':True}
 else: result={'pong':True}
 send({'id':v['id'],'result':{'id':str(v['id']),'result':result}})
'''


class Pipes(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.worker = self.root / 'worker'
        self.worker.write_text(FAKE)
        self.worker.chmod(0o700)
        self.trace = self.root / 'trace.jsonl'
        self.config = self.root / 'config.json'
        self.config.write_text(json.dumps({'worker': str(self.worker),
            'worker_sha256': hashlib.sha256(self.worker.read_bytes()).hexdigest(), 'trace': str(self.trace), 'tag': TAG}))
        self.config.chmod(0o600)
        self.p = None
        self.buffer = bytearray()

    def tearDown(self):
        if self.p:
            if self.p.poll() is None:
                self.p.stdin.close()
                try:
                    self.p.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    self.p.kill()
                    self.p.wait(timeout=3)
            for pipe in (self.p.stdin, self.p.stdout, self.p.stderr):
                pipe.close()
        self.temp.cleanup()

    def launch(self):
        self.p = subprocess.Popen([sys.executable, str(HERE / 'qemu-exec-lost-ack-worker.py'),
            'run', '--guest-worker-instance', 'd' * 64], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            env={**os.environ, 'VEYNODE_LOST_ACK_CONFIG': str(self.config)})

    def line(self):
        deadline = time.monotonic() + 4
        while b'\n' not in self.buffer:
            self.assertTrue(select.select([self.p.stdout], [], [], max(0, deadline-time.monotonic()))[0])
            b = os.read(self.p.stdout.fileno(), 65536)
            self.assertTrue(b)
            self.buffer.extend(b)
        line, _, rest = self.buffer.partition(b'\n')
        self.buffer = bytearray(rest)
        return json.loads(line)

    def test_fragmented_real_pipes_one_drop_then_scope_ack_and_eof_reap(self):
        self.launch()
        self.assertEqual(self.line()['event'], 'state')
        for b in wire(start()):
            self.p.stdin.write(bytes([b]))
        self.p.stdin.flush()
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            if self.trace.exists() and 'successful_ack_dropped' in self.trace.read_text():
                break
            time.sleep(.01)
        self.assertIn('successful_ack_dropped', self.trace.read_text())
        self.assertFalse(self.buffer)
        self.assertFalse(select.select([self.p.stdout], [], [], .1)[0])
        self.p.stdin.write(wire(revoke()))
        self.p.stdin.flush()
        self.assertEqual(self.line(), empty())
        self.p.stdin.close()
        self.assertEqual(self.p.wait(timeout=4), 0)
        events = acceptance.traces(self.trace)
        self.assertEqual(acceptance.one(events, 'worker_reaped')['exit_code'], 0)
        self.assertEqual(len([v for v in events if v['event'] == 'successful_ack_dropped']), 1)

    def test_partial_request_eof_is_failure_and_child_is_reaped(self):
        self.launch()
        self.line()
        child = acceptance.one(acceptance.traces(self.trace), 'worker_spawned')['worker_pid']
        self.p.stdin.write(b'{"id":')
        self.p.stdin.close()
        self.assertEqual(self.p.wait(timeout=10), 1)
        with self.assertRaises(ProcessLookupError):
            os.kill(child, 0)
        self.assertIn('shim_failed', self.trace.read_text())

    def test_modified_worker_and_nonprivate_config_refused(self):
        self.worker.write_text(FAKE + '\n# changed\n')
        with self.assertRaises(RuntimeError):
            shim.load_config(self.config)
        self.config.chmod(0o644)
        with self.assertRaises(RuntimeError):
            shim.load_config(self.config)

    def test_blocked_daemon_output_has_absolute_write_deadline(self):
        self.worker.write_text('#!/usr/bin/env python3\nimport json\nprint(json.dumps({"event":"bulk","data":"x"*900000}),flush=True)\n')
        value = json.loads(self.config.read_text())
        value['worker_sha256'] = hashlib.sha256(self.worker.read_bytes()).hexdigest()
        self.config.write_text(json.dumps(value))
        begin = time.monotonic()
        self.launch()
        # Intentionally never consume stdout. One bounded line fits in the shim
        # queue, but cannot flush into the actual OS pipe within its 2s budget.
        self.assertEqual(self.p.wait(timeout=6), 1)
        self.assertLess(time.monotonic()-begin, 5)
        self.assertIn('shim_failed', self.trace.read_text())

    def test_real_worker_nonzero_exit_not_reported_as_success(self):
        self.worker.write_text(FAKE + '\nsys.exit(7)\n')
        value = json.loads(self.config.read_text())
        value['worker_sha256'] = hashlib.sha256(self.worker.read_bytes()).hexdigest()
        self.config.write_text(json.dumps(value))
        self.launch()
        self.line()
        self.p.stdin.close()
        self.assertEqual(self.p.wait(timeout=4), 7)
        self.assertEqual(acceptance.one(acceptance.traces(self.trace), 'worker_reaped')['exit_code'], 7)

    def test_fifo_config_and_symlink_refused_without_blocking(self):
        fifo = self.root / 'fifo'
        os.mkfifo(fifo, 0o600)
        with self.assertRaises(RuntimeError):
            shim.load_config(fifo)
        link = self.root / 'link'
        link.symlink_to(self.config)
        with self.assertRaises(OSError):
            shim.load_config(link)


if __name__ == '__main__':
    unittest.main(verbosity=2)
