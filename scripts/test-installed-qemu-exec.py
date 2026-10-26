#!/usr/bin/env python3
"""Real installed accelerated Linux -> serial v2 -> stdio MCP acceptance.

Attach an existing controller. Register one NEW fixture VM on an explicitly
supplied disposable clone. Never boot the immutable base or start a controller.
This baseline is not the worker-pause/service-crash acceptance gate.
"""
import argparse
import base64
import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('installed_desktop', ROOT / 'scripts/test-installed-qemu-desktop.py')
desktop = importlib.util.module_from_spec(spec)
spec.loader.exec_module(desktop)
require, digest = desktop.require, desktop.digest


class MCP(desktop.MCP):
    def __init__(self, binary, state, execute=True):
        self.p = subprocess.Popen([str(binary), 'mcp', '--state-dir', str(state),
            '--allow-lifecycle', '--allow-files', *(['--allow-execute'] if execute else [])],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.sequence, self.buffer = 0, bytearray()
        try:
            reply = self.rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                'clientInfo': {'name': 'installed-qemu-exec-acceptance', 'version': '1'}})
            require(reply.get('result', {}).get('protocolVersion') == '2025-06-18', 'MCP initialization failed')
            self.send({'jsonrpc': '2.0', 'method': 'notifications/initialized'})
        except BaseException:
            self.close()
            raise

    def close(self):
        if self.p.poll() is not None:
            require(self.p.returncode == 0, 'MCP cleanup failed')
            return
        super().close()


def wait_guest_info(client, vm, status_query, expected_worker_pid, timeout=30):
    """Graphics readiness precedes OpenRC service startup; never reconnect a worker.

    Poll only the explicit temporary transport admission refusal. A terminal or
    replaced worker, malformed reply or any other error ends the test immediately.
    The actual Guest identity/capabilities are still validated by the caller.
    """
    deadline, attempts = time.monotonic() + timeout, []
    while time.monotonic() < deadline:
        status = status_query()
        require(status.get('pid') == expected_worker_pid and status['state'] in ('starting', 'running'),
                'original worker unavailable during Guest integration startup')
        reply = client.rpc('tools/call', {'name': 'vm_guest_info', 'arguments': {'id': vm}})
        result = reply.get('result', {})
        if 'error' not in reply and result.get('isError') is not True:
            content = result.get('content', [])
            require(len(content) == 1 and content[0].get('type') == 'text', 'invalid actual Guest info content')
            return json.loads(content[0]['text'])['result'], attempts
        require(result.get('isError') is True and result.get('content') == [
            {'text': 'backend: "serial execution unavailable or fenced"', 'type': 'text'}],
            f'Guest integration failed instead of pending startup: {reply}')
        attempts.append({'elapsed_seconds': timeout - max(0, deadline - time.monotonic()), 'reply': reply})
        time.sleep(.1)
    raise AssertionError('actual Guest integration readiness deadline')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('binary', 'state', 'config', 'base', 'output'):
        parser.add_argument('--' + name, type=Path, required=True)
    args = parser.parse_args()
    for name, value in vars(args).items():
        setattr(args, name, value.resolve())
    args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    evidence = {'passed': False, 'started_at_unix': time.time(), 'checks': [],
        'scope': 'real installed Linux accelerated desktop and explicit rootless serial v2 MCP jobs',
        'limits': ['single VM baseline; not performance or Parallels comparison',
                   'worker/Guest SIGSTOP, daemon death and whole-VM pause are separate gates']}
    clients, started = [], False
    vm = None

    def record(name, value):
        evidence['checks'].append({'name': name, 'evidence': value})
        (args.output / 'result.json').write_text(json.dumps(evidence, indent=2) + '\n')
        print(name, flush=True)

    def local(method, payload):
        reply = subprocess.run([str(args.binary), 'call', '--state-dir', str(args.state), method,
            json.dumps(payload)], capture_output=True, text=True, timeout=42)
        require(reply.returncode == 0, f'local {method}: {reply.stderr}')
        return json.loads(reply.stdout)

    def tool(client, name, **payload):
        return client.tool(name, {'id': vm, **payload})

    def finish(client, job, timeout=12):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            reply = tool(client, 'exec_poll', job_id=job)['result']
            if reply['state'] == 'finished':
                require('error' not in reply, f'job containment failed: {reply}')
                result = reply['result']
                require(all(type(result.get(key)) is bool for key in
                    ('cancelled', 'timed_out', 'stdout_truncated', 'stderr_truncated')), 'invalid terminal job proof')
                return result
            time.sleep(.05)
        raise AssertionError('terminal job result deadline')

    def execute(client, argv, timeout=5000):
        job = tool(client, 'exec_start', argv=argv, timeout_ms=timeout)['result']['job_id']
        return finish(client, job)

    try:
        config = json.loads(args.config.read_text())
        vm = config['id']
        require(config.get('qemu_guest_exec') is True and config.get('backend') == 'qemu-hvf'
            and config.get('guest') == 'linux', 'explicit Linux QEMU exec v2 config required')
        require(vm not in {entry['config']['id'] for entry in local('list', {})}, 'fixture VM ID already exists')
        disk = Path(config['disk']).resolve()
        require(disk.is_relative_to(ROOT / '.local') and args.base.is_relative_to(ROOT / '.local'),
                'only repository-created fixtures allowed')
        ds, bs = disk.stat(), args.base.stat()
        require(stat.S_ISREG(ds.st_mode) and stat.S_ISREG(bs.st_mode)
            and (ds.st_dev, ds.st_ino) != (bs.st_dev, bs.st_ino)
            and bs.st_mode & 0o222 == 0, 'disposable clone and separate read-only base required')
        base_before = digest(args.base)
        record('fixture provenance', {'config': config, 'controller_sha256': digest(args.binary),
            'base_sha256': base_before, 'disk_device': ds.st_dev, 'disk_inode': ds.st_ino})
        config['serial_log'] = str(args.output / 'guest.serial.log')
        (args.output / 'config.json').write_text(json.dumps(config, indent=2) + '\n')
        local('create', config)
        for allow_execute in (False, True, True):
            clients.append(MCP(args.binary, args.state, allow_execute))
        life, owner, other = clients
        started = True
        record('actual start', tool(life, 'start'))
        log = Path(config['serial_log'])
        deadline, actual = time.monotonic() + 180, None
        while time.monotonic() < deadline:
            status = local('status', {'id': vm})
            require(status['state'] not in ('stopped', 'crashed', 'error'), 'Guest exited before graphics readiness')
            if log.exists():
                require(log.stat().st_size <= desktop.MAX_LOG, 'serial log bound exceeded')
                actual = desktop.boot_evidence(log.read_bytes())
                if actual:
                    break
            time.sleep(.5)
        require(actual is not None, 'actual installed graphics readiness deadline')
        record('installed ext4 root and 15 actual pixel cases', actual)
        runtime = status['runtime']
        provenance = runtime['disk_provenance']
        require(provenance.get('device') == ds.st_dev and provenance.get('inode') == ds.st_ino
            and provenance.get('descriptor_backed') is True
            and provenance.get('qmp_fdsets_verified_at_start') is True, 'descriptor-backed clone identity failed')
        record('actual backend runtime and disk binding', runtime)
        guardian = runtime.get('execution_guardian', {})
        require(guardian.get('owns_qemu_child') is True
            and guardian.get('heartbeat_timeout_ms') == 5000
            and guardian.get('backend_pid') == runtime.get('backend_pid')
            and type(guardian.get('guardian_pid')) is int
            and guardian['guardian_pid'] != guardian['backend_pid'], 'mandatory host guardian missing')
        parent = int(subprocess.check_output(['ps', '-o', 'ppid=', '-p', str(guardian['backend_pid'])]).strip())
        require(parent == guardian['guardian_pid'], 'actual QEMU parent is not its guardian')
        record('actual QEMU parent is independent guardian', {'backend_parent_pid': parent})
        info, startup_attempts = wait_guest_info(owner, vm, lambda: local('status', {'id': vm}), status['pid'])
        record('actual integration startup within original worker', {'temporary_refusals': startup_attempts})
        require(info.get('transport') == 'virtio-serial-private-exec-v2'
            and info.get('transport_reconnect') is False
            and info.get('execution_containment') == 'cgroup-v2-clone3-kill', 'wrong actual exec transport')
        require(all(info['capabilities'].get(key) is True for key in ('exec', 'async_exec', 'descendant_containment'))
            and all(info['capabilities'].get(key) is False for key in ('files', 'shutdown', 'desktop')),
            'incorrect actual capability declaration')
        record('actual v2 Guest capability query', info)
        denied = life.tool('exec_start', {'id': vm, 'argv': ['/bin/true']}, denied=True)
        require('Tool unknown or permission not granted' in json.dumps(denied), 'wrong execute-scope denial')
        record('client without execute scope denied', denied)
        identity = execute(owner, ['/bin/sh', '-eu', '-c',
            'id; printf "HOME=%s\\nPWD=%s\\n" "$HOME" "$PWD"; cat /proc/self/cgroup; '
            'test "$(id -u)" = 1000; test "$(id -G)" = 1000; '
            'test ! -w /sys/fs/cgroup/cgroup.procs; test ! -w /sys/fs/cgroup/veynode-exec/cgroup.procs'])
        require(identity['exit_code'] == 0 and 'HOME=/home/veynode' in identity['stdout']
            and '/veynode-exec/job-' in identity['stdout'], 'rootless cgroup membership failed')
        record('actual UID groups home and atomic cgroup membership', identity)
        denied = execute(owner, ['/bin/sh', '-c', 'printf forbidden > /root/veynode-exec-must-not-write'])
        require(denied['exit_code'] != 0 and 'Permission denied' in denied['stderr'], 'root write unexpectedly allowed')
        record('root filesystem permission denial', denied)
        normal = execute(owner, ['/bin/sh', '-c', 'printf "你好, VM"; exit 7'])
        require(normal['stdout'] == '你好, VM' and normal['exit_code'] == 7
            and not normal['cancelled'] and not normal['timed_out'], 'Unicode completion failed')
        record('actual Unicode and nonzero exit status', normal)
        bounded = execute(owner, ['/bin/sh', '-c', 'head -c 70000 /dev/zero | tr "\\000" x'])
        require(bounded['exit_code'] == 0 and bounded['stdout_truncated'] is True
            and len(bounded['stdout'].encode()) == 65536, 'output limit not enforced')
        record('actual output bound', {k: v for k, v in bounded.items() if k != 'stdout'})
        files = owner.tool('file_upload_begin', {'id': vm, 'path': 'unsupported', 'size': 0,
            'sha256': 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'}, denied=True)
        require('unsupported' in json.dumps(files).lower(), 'files denial was not an unsupported capability')
        record('files remain explicitly unsupported', files)

        denied = other.tool('exec_poll', {'id': vm, 'job_id': uuid.uuid4().hex}, denied=True)
        require('job is unknown or expired on this VM' in json.dumps(denied), 'unknown job not refused')
        record('unknown job refused', denied)

        token = uuid.uuid4().hex
        for reason in ('cancel', 'timeout', 'completed', 'disconnect'):
            marker = f'/home/veynode/v2-{token}-{reason}-marker'
            ready = f'/home/veynode/v2-{token}-{reason}-ready'
            # The detached grandchild changes its session and outlives its parent.
            # Only process-tree containment prevents the delayed filesystem write.
            detached = ('setsid /bin/sh -c \'(sleep 4; printf escaped > "$2") '
                '</dev/null >/dev/null 2>&1 & printf ready > "$1"; exit 0\' detached "$1" "$2" & ')
            command = detached + ('wait' if reason == 'completed' else 'sleep 20 & wait')
            job = tool(owner, 'exec_start', argv=['/bin/sh', '-c', command, 'outer', ready, marker],
                timeout_ms=1500 if reason == 'timeout' else 15000)['result']['job_id']
            begin = time.monotonic()
            if reason != 'completed':
                deadline = begin + 1.2
                ready_seen = False
                while time.monotonic() < deadline:
                    if execute(owner, ['/bin/sh', '-c', 'test -e "$1"', 'ready', ready])['exit_code'] == 0:
                        ready_seen = True
                        break
                    time.sleep(.025)
                require(ready_seen, 'detached process did not announce readiness before cancellation')
            if reason == 'cancel':
                require(tool(owner, 'exec_cancel', job_id=job)['result']['cancel_requested'] is True, 'cancel request failed')
            elif reason == 'disconnect':
                owner.close()
                clients.remove(owner)
                owner = other
                other = MCP(args.binary, args.state)
                clients.append(other)
            if reason in ('cancel', 'timeout', 'completed'):
                result = finish(owner, job)
                require(result['cancelled'] is (reason == 'cancel')
                    and result['timed_out'] is (reason == 'timeout'), 'incorrect terminal cause')
                if reason == 'completed':
                    require(result['exit_code'] == 0, 'ordinary parent did not complete')
                    require(execute(owner, ['/bin/sh', '-c', 'test -e "$1"', 'ready', ready])['exit_code'] == 0,
                            'completed parent did not launch detached descendant')
            else:
                result = {'cleanup': 'MCP session end cancelled the job'}
            time.sleep(max(0, begin + 4.5 - time.monotonic()))
            absent = execute(owner, ['/bin/sh', '-c', 'test ! -e "$1"', 'marker', marker])
            require(absent['exit_code'] == 0, 'detached descendant escaped and wrote marker')
            record(reason + ' actual detached descendant cannot write delayed marker', {'result': result,
                'marker_absent': True, 'readiness_observed': True})

        content = tool(owner, 'guest_info')['result']
        require(content['capabilities']['async_exec'] is True, 'service unavailable after cleanup tests')
        image = owner.tool('screenshot', {'id': vm}, raw=True)['result']['content']
        pictures = [item for item in image if item.get('type') == 'image']
        require(len(pictures) == 1 and pictures[0]['mimeType'] == 'image/png', 'missing actual PNG')
        path = args.output / 'after-exec.png'
        path.write_bytes(base64.b64decode(pictures[0]['data'], validate=True))
        decode = json.loads(subprocess.check_output(['ffprobe', '-v', 'error', '-show_entries',
            'stream=codec_name,width,height', '-of', 'json', str(path)], text=True, timeout=15))
        require(decode['streams'] == [{'codec_name': 'png', 'width': config['width'], 'height': config['height']}],
                'desktop capture regression')
        record('accelerated desktop screenshot after command tests', {'decode': decode, 'sha256': digest(path)})
        for client in clients:
            client.close()
        clients.clear()
        record('all MCP session EOF cleanup exit zero', True)
        shutdown = local('shutdown', {'id': vm})
        require(shutdown.get('requested') is True and shutdown.get('mechanism') == 'qmp-system-powerdown',
                'expected real ACPI power-button shutdown')
        require(shutdown.get('execution_quiescing') is True and shutdown.get('shutdown_timeout_seconds') == 60,
                'shutdown lacks bounded execution quiescing')
        record('bounded normal shutdown after all MCP sessions ended', shutdown)
        clients.append(MCP(args.binary, args.state))
        denied = clients[-1].tool('exec_start', {'id': vm, 'argv': ['/bin/true']}, denied=True)
        require('VM execution is quiescing for shutdown' in json.dumps(denied),
                'shutdown accepted new execution')
        clients[-1].close()
        clients.clear()
        record('quiescing shutdown rejects new execution from a fresh MCP session', denied)
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            status = local('status', {'id': vm})
            if status['state'] in ('stopped', 'crashed', 'error'):
                break
            time.sleep(.25)
        require(status['state'] == 'stopped' and status.get('exit_code') == 0
            and b'VEYNODE_INSTALLED_ACPI_POWER_BUTTON' in log.read_bytes(), 'normal shutdown unconfirmed')
        started = False
        record('actual Guest ACPI handler and normal worker exit', status)
        require(digest(args.base) == base_before, 'immutable source base changed')
        record('complete immutable base hash unchanged', base_before)
        evidence['passed'] = True
    except BaseException as error:
        evidence['error'] = str(error)
        raise
    finally:
        if started:
            try:
                status = local('status', {'id': vm})
                if status['state'] not in ('stopped', 'crashed', 'error'):
                    evidence['failure_force_stop'] = local('force_stop', {'id': vm, 'confirm': vm})
                    deadline = time.monotonic() + 15
                    while time.monotonic() < deadline:
                        status = local('status', {'id': vm})
                        if status['state'] in ('stopped', 'crashed', 'error'):
                            break
                        time.sleep(.1)
                evidence['failure_terminal_status'] = status
                require(status['state'] in ('stopped', 'crashed', 'error'), 'failure cleanup unconfirmed')
            except Exception as error:
                evidence['cleanup_error'] = str(error)
        for client in clients:
            try:
                client.close()
            except Exception as error:
                evidence.setdefault('client_cleanup_errors', []).append(str(error))
        evidence['finished_at_unix'] = time.time()
        (args.output / 'result.json').write_text(json.dumps(evidence, indent=2) + '\n')


if __name__ == '__main__':
    main()
