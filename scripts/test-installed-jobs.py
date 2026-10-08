#!/usr/bin/env python3
"""Actual rootless Linux commands/cancellation/revocation via stdio MCP.

Use a NEW repository-created fixture from provision-linux-qemu.py. This test
does not replace real Windows/macOS integration, desktop input or sandbox tests.
"""
import argparse
import json
from pathlib import Path
import queue
import subprocess
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--config', type=Path, required=True)
args = parser.parse_args()
config = json.loads(args.config.read_text())
if not Path(config['disk']).resolve().is_relative_to(ROOT / '.local/vms'):
    raise SystemExit('Only repository-created disposable fixture disks accepted')
evidence = {'started_at_unix': time.time(), 'checks': [],
            'scope': 'Real installed ARM Linux → VZ vsock → Rust daemon → stdio MCP'}


def record(name, value):
    evidence['checks'].append({'name': name, 'evidence': value})


class MCP:
    def __init__(self, state, execute=True):
        self.process = subprocess.Popen([str(ROOT / 'target/debug/vm-control'), 'mcp',
            '--state-dir', str(state), '--allow-lifecycle', *(['--allow-execute'] if execute else [])],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
        self.sequence = 0
        self.replies = queue.Queue()

        def reader():
            for line in self.process.stdout:
                self.replies.put(json.loads(line))
        threading.Thread(target=reader, daemon=True).start()
        self.request('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                                  'clientInfo': {'name': 'installed-job-validator', 'version': '1'}})
        self.process.stdin.write(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/initialized'}) + '\n')
        self.process.stdin.flush()

    def request(self, method, params):
        self.sequence += 1
        self.process.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': self.sequence,
                                            'method': method, 'params': params}) + '\n')
        self.process.stdin.flush()
        reply = self.replies.get(timeout=40)
        assert reply['id'] == self.sequence, reply
        return reply

    def tool(self, name, arguments, denied=False):
        reply = self.request('tools/call', {'name': name, 'arguments': arguments})
        failed = 'error' in reply or reply.get('result', {}).get('isError')
        if denied:
            assert failed, reply
            return reply
        assert not failed, reply
        return json.loads(reply['result']['content'][0]['text'])

    def close(self):
        if self.process.poll() is None:
            self.process.stdin.close()
            self.process.wait(timeout=8)


with tempfile.TemporaryDirectory(prefix='vm-installed-jobs-') as directory:
    state = Path(directory) / 'state'
    daemon = subprocess.Popen([str(ROOT / 'target/debug/vm-control'), 'daemon', '--state-dir', str(state),
        '--backend', str(ROOT / '.local/bin/vm-apple')], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    clients = []

    def local(method, arguments):
        return json.loads(subprocess.check_output([str(ROOT / 'target/debug/vm-control'), 'call',
            '--state-dir', str(state), method, json.dumps(arguments)], text=True, timeout=40))

    def tool(client, name, **payload):
        return client.tool(name, {'id': config['id'], **payload})

    def execute(client, argv):
        return tool(client, 'vm_exec', argv=argv, timeout_ms=5000)['result']

    def finish(client, job):
        for _ in range(100):
            reply = tool(client, 'vm_exec_poll', job_id=job)['result']
            if reply['state'] == 'finished':
                assert 'error' not in reply, reply
                return reply['result']
            time.sleep(.05)
        raise TimeoutError('Job did not actually finish')

    try:
        for _ in range(100):
            if (state / 'control.sock').exists():
                break
            time.sleep(.05)
        local('create', config)
        life, owner, other = MCP(state, False), MCP(state), MCP(state)
        clients.extend([life, owner, other])
        tool(life, 'vm_start')
        for _ in range(100):
            try:
                info = tool(owner, 'vm_guest_info')['result']
                break
            except AssertionError:
                time.sleep(.1)
        else:
            raise TimeoutError('Guest not ready')
        assert info['capabilities']['async_exec'] and info['capabilities']['shutdown'], info
        record('installed service actual identity/capabilities', info)
        life.tool('vm_exec_start', {'id': config['id'], 'argv': ['/bin/true']}, denied=True)
        record('lifecycle-only client denied command job', True)
        tool(owner, 'vm_acquire_control')
        identity = execute(owner, ['/bin/sh', '-eu', '-c', 'id; printf "HOME=%s\nPWD=%s\n" "$HOME" "$PWD"; test "$(id -u)" = 1000; test "$(id -G)" = 1000'])
        assert identity['exit_code'] == 0 and 'HOME=/home/veynode' in identity['stdout'], identity
        record('actual child UID/GID/groups/home are unprivileged', identity)
        denied = execute(owner, ['/bin/sh', '-c', 'printf forbidden > /root/veynode-must-not-write'])
        assert denied['exit_code'] != 0 and 'Permission denied' in denied['stderr'], denied
        record('command cannot write root-only filesystem', denied)
        no_power = execute(owner, ['/sbin/poweroff'])
        assert no_power['exit_code'] != 0, no_power
        assert local('status', {'id': config['id']})['state'] == 'running'
        record('unprivileged command cannot bypass lifecycle shutdown permission', no_power)
        marker = '/home/veynode/cancel-must-not-exist'
        execute(owner, ['/bin/rm', '-f', marker])
        job = tool(owner, 'vm_exec_start', argv=['/bin/sh', '-c',
            '(sleep 1; printf leaked > "$1") & wait', 'cancel-test', marker], timeout_ms=10000)['result']['job_id']
        other.tool('vm_exec_poll', {'id': config['id'], 'job_id': job}, denied=True)
        other.tool('vm_exec_cancel', {'id': config['id'], 'job_id': job}, denied=True)
        record('other MCP owner cannot observe or cancel job', True)
        assert tool(owner, 'vm_exec_cancel', job_id=job)['result']['cancel_requested']
        result = finish(owner, job)
        assert result['cancelled'] and not result['timed_out'], result
        record('explicit cancellation actually terminates process', result)
        time.sleep(1.2)
        assert execute(owner, ['/bin/sh', '-c', 'test ! -e "$1"', 'marker-test', marker])['exit_code'] == 0
        record('cancelled ordinary descendant never writes late marker', True)
        completed = tool(owner, 'vm_exec_start', argv=['/bin/sh', '-c', 'printf "你好, VM"; exit 7'], timeout_ms=5000)['result']['job_id']
        result = finish(owner, completed)
        assert result['stdout'] == '你好, VM' and result['exit_code'] == 7 and not result['cancelled'], result
        record('normal async Unicode/nonzero completion', result)
        job = tool(owner, 'vm_exec_start', argv=['/bin/sh', '-c',
            '(sleep 2; printf leaked > "$1") & wait', 'takeover-test', marker], timeout_ms=10000)['result']['job_id']
        takeover = local('human_takeover', {'id': config['id']})
        record('human takeover response', takeover)
        owner.tool('vm_exec_start', {'id': config['id'], 'argv': ['/bin/true'], 'timeout_ms': 1000}, denied=True)
        other.tool('vm_acquire_control', {'id': config['id']}, denied=True)
        result = finish(owner, job)
        assert result['cancelled'], result
        record('takeover cancels active job and rejects old authority', result)
        local('human_release', {'id': config['id']})
        tool(other, 'vm_acquire_control')
        time.sleep(2.1)
        assert execute(other, ['/bin/sh', '-c', 'test ! -e "$1"', 'marker-test', marker])['exit_code'] == 0
        tool(other, 'vm_exec_start', argv=['/bin/sh', '-c',
            '(sleep 2; printf leaked > "$1") & wait', 'disconnect-test', marker], timeout_ms=10000)
        other.close()
        tool(owner, 'vm_acquire_control')
        time.sleep(2.1)
        assert execute(owner, ['/bin/sh', '-c', 'test ! -e "$1"', 'marker-test', marker])['exit_code'] == 0
        record('MCP disconnect cancels active job and releases control', True)
        tool(owner, 'vm_release_control')
        assert tool(life, 'vm_shutdown', mode='guest')['result']['requested']
        for _ in range(150):
            if local('status', {'id': config['id']})['state'] == 'stopped':
                break
            time.sleep(.1)
        else:
            raise TimeoutError('normal shutdown did not complete')
        record('separate privileged lifecycle shutdown actually stops Guest', True)
        evidence['passed'] = True
    except Exception as error:
        evidence.update(passed=False, error=str(error))
        raise
    finally:
        for client in clients:
            try:
                client.close()
            except Exception:
                pass
        try:
            if local('status', {'id': config['id']})['state'] != 'stopped':
                evidence['forced_failure_cleanup'] = local('force_stop', {'id': config['id'], 'confirm': config['id']})
        finally:
            daemon.terminate()
            daemon.wait(timeout=8)
            evidence['finished_at_unix'] = time.time()
            output = ROOT / '.local/evidence/installed-jobs.json'
            if not evidence.get('passed'):
                output = output.with_suffix('.failed.json')
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_text(json.dumps(evidence, indent=2) + '\n')
print('Actual rootless Linux MCP jobs, cancellation, takeover and normal shutdown passed')
