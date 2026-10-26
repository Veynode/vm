#!/usr/bin/env python3
"""Actual setsid/double-fork cancellation negative control or contained acceptance.

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
parser.add_argument('--expect-contained', action='store_true')
args = parser.parse_args()
config = json.loads(args.config.read_text())
if not Path(config['disk']).resolve().is_relative_to(ROOT / '.local/vms'):
    raise SystemExit('Only repository-created disposable fixture disks accepted')
evidence = {'started_at_unix': time.time(), 'checks': [],
            'scope': 'Real installed ARM Linux → VZ vsock → Rust daemon → stdio MCP'}


def record(name, value):
    evidence['checks'].append({'name': name, 'evidence': value})


from cgroup_test_support import MCP


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
        probe = execute(owner, ['/bin/sh', '-c', 'uname -r; cat /proc/self/cgroup; cat /proc/filesystems; grep cgroup /proc/mounts || true; ls -ld /sys/fs/cgroup; cat /etc/init.d/veynode-agent'])
        record('actual installed kernel, mount, membership and service configuration', probe)
        if args.expect_contained:
            assert info['execution_containment'] == 'cgroup-v2-clone3-kill' and info['capabilities']['descendant_containment'], info
            membership = execute(owner, ['/bin/sh', '-c', 'cat /proc/self/cgroup; test ! -w /sys/fs/cgroup/cgroup.procs; test ! -w /sys/fs/cgroup/veynode-exec/cgroup.procs'])
            assert membership['exit_code'] == 0 and '/veynode-exec/job-' in membership['stdout'], membership
            record('actual command atomic membership and migration permissions', membership)
            unavailable = execute(owner, ['/usr/local/bin/veynode-guest', '--allow-execute'])
            assert unavailable['exit_code'] != 0 and 'containment requires root service and fixed non-root' in unavailable['stderr'], unavailable
            record('unprivileged nested service explicitly refuses unsupported containment', unavailable)


        for reason in (['cancel', 'timeout', 'completed', 'disconnect'] if args.expect_contained else ['cancel']):
            marker = '/home/veynode/cgroup-' + reason + '-marker'
            ready = '/home/veynode/cgroup-' + reason + '-ready'
            execute(owner, ['/bin/rm', '-f', marker, ready])
            # setsid + a background grandchild outlives both its session leader and
            # original shell unless cancellation contains the entire cgroup.
            detached = 'setsid /bin/sh -c \'(sleep 1.2; printf escaped > "$2") </dev/null >/dev/null 2>&1 & printf ready > "$1"; exit 0\' detached "$1" "$2" & '
            script = detached + ('wait' if reason == 'completed' else 'sleep 20 & wait')
            job = tool(owner, 'vm_exec_start', argv=['/bin/sh', '-c', script, 'outer', ready, marker], timeout_ms=500 if reason == 'timeout' else 10000)['result']['job_id']
            for _ in range(50):
                if execute(owner, ['/bin/sh', '-c', 'test -e "$1"', 'ready', ready])['exit_code'] == 0:
                    break
                time.sleep(.01)
            else:
                raise AssertionError('detached child never announced readiness')
            if reason == 'cancel':
                assert tool(owner, 'vm_exec_cancel', job_id=job)['result']['cancel_requested']
            if reason == 'disconnect':
                owner.close()
                owner, other = other, owner
                result = {'transport': 'MCP stdio EOF; session end cancels its job'}
            else:
                result = finish(owner, job)
            time.sleep(1.4)
            survived = execute(owner, ['/bin/sh', '-c', 'test -e "$1"', 'marker', marker])['exit_code'] == 0
            assert survived != args.expect_contained, (reason, result, survived)
            record(reason + ' actual setsid/double-fork marker', {'result': result, 'escaped_marker_survived': survived})
        after = tool(owner, 'vm_guest_info')['result']
        assert after['file_policy']['uid'] == 1001 and 'error' not in after['file_policy'], after
        record('service and separate UID1001 files worker remain responsive after job cleanup', after)
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
            output = ROOT / '.local/evidence' / (config['id'] + '-cgroup.json')
            if not evidence.get('passed'):
                output = output.with_suffix('.failed.json')
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_text(json.dumps(evidence, indent=2) + '\n')
print('Actual installed cgroup/setsid experiment saved')
