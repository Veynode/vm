#!/usr/bin/env python3
"""Opt-in actual stopped-worker or whole-QEMU-stop containment on one NEW VM.

Attaches an existing daemon; never starts/stops a daemon or another VM. Default
worker fault verifies the independent guardian timeout. Explicit --fault-target
qemu pauses all QEMU threads and verifies host transport failure terminates that
incarnation; it does not prove host sleep, stale queued dispatch or Guest crash.
"""
import argparse
import ctypes
import errno
import importlib.util
import json
import os
from pathlib import Path
import shlex
import signal
import stat
import subprocess
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
_spec = importlib.util.spec_from_file_location('exec_baseline', ROOT / 'scripts/test-installed-qemu-exec.py')
baseline = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(baseline)
require, digest, desktop = baseline.require, baseline.digest, baseline.desktop


# Public Darwin sys/proc_info.h PROC_PIDTBSDINFO=3 layout. Start seconds AND
# microseconds pin a PID incarnation; state4=SSTOP,state5=SZOMB. proc_pidpath
# identifies actual executable, not an arbitrary process title/argv[0].
class BSDInfo(ctypes.Structure):
    _fields_ = [(name, ctypes.c_uint32) for name in (
        'flags', 'status', 'xstatus', 'pid', 'ppid', 'uid', 'gid', 'ruid',
        'rgid', 'svuid', 'svgid', 'reserved')]
    _fields_ += [('comm', ctypes.c_char * 16), ('name', ctypes.c_char * 32)]
    _fields_ += [(name, ctypes.c_uint32) for name in ('nfiles', 'pgid', 'jobc', 'tdev', 'tpgid')]
    _fields_ += [('nice', ctypes.c_int32), ('start_sec', ctypes.c_uint64), ('start_usec', ctypes.c_uint64)]


def snapshot(pid):
    require(type(pid) is int and pid > 1 and sys.platform == 'darwin', 'Darwin exact PID required')
    require(ctypes.sizeof(BSDInfo) == 136, 'unexpected public proc_bsdinfo ABI')
    lib = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)
    lib.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
    lib.proc_pidinfo.restype = ctypes.c_int
    value = BSDInfo()
    ctypes.set_errno(0)
    count = lib.proc_pidinfo(pid, 3, 0, ctypes.byref(value), ctypes.sizeof(value))
    if count == 0 and ctypes.get_errno() == errno.ESRCH:
        return None
    require(count == ctypes.sizeof(value), f'proc_pidinfo unavailable, not disappearance proof: {count}/{ctypes.get_errno()}')
    path = ctypes.create_string_buffer(4096)
    lib.proc_pidpath.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]
    lib.proc_pidpath.restype = ctypes.c_int
    size = lib.proc_pidpath(pid, path, len(path))
    require(size > 0 or value.status == 5, 'executable path unavailable for live process')
    return {'pid': value.pid, 'ppid': value.ppid, 'uid': value.uid, 'state': value.status,
        'start_sec': value.start_sec, 'start_usec': value.start_usec, 'exit_status': value.xstatus,
        'executable': str(Path(os.fsdecode(path.value)).resolve()) if size > 0 else None}


def same_incarnation(before, after):
    return after is not None and all(before[key] == after[key]
        for key in ('pid', 'uid', 'start_sec', 'start_usec'))


def exact_live(before):
    value = snapshot(before['pid'])
    require(same_incarnation(before, value), 'PID disappeared or was reused; refusing signal')
    require(value['executable'] == before['executable'] and value['state'] != 5,
            'original executable no longer live; refusing signal')
    return value


def command_arguments(pid):
    value = subprocess.run(['/bin/ps', '-ww', '-o', 'command=', '-p', str(pid)],
        capture_output=True, text=True, timeout=2)
    require(value.returncode == 0 and 0 < len(value.stdout) <= 32768, 'bounded process argv unavailable')
    return shlex.split(value.stdout.strip())


def option(argv, name):
    require(argv.count(name) == 1 and argv.index(name) + 1 < len(argv), f'missing/ambiguous {name}')
    return argv[argv.index(name) + 1]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ('binary', 'state', 'config', 'base', 'qemu-loader', 'output'):
        p.add_argument('--' + name, type=Path, required=True)
    p.add_argument('--worker-binary', type=Path)
    p.add_argument('--fault-target', choices=('worker', 'qemu'), default='worker')
    a = p.parse_args()
    a.worker_binary = a.worker_binary or a.binary.parent / 'vm-qemu'
    for key, value in vars(a).items():
        if isinstance(value, Path):
            setattr(a, key, value.resolve())
    require(sys.platform == 'darwin', 'this actual fixture requires Darwin process identity APIs')
    a.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    report = {'passed': False, 'scope': ('actual single worker SIGSTOP; independent guardian reaps original QEMU'
            if a.fault_target == 'worker' else 'actual whole QEMU SIGSTOP; host transport failure retires original incarnation'),
        'fault_target': a.fault_target,
        'not_tested': ['daemon EOF', 'lost start ACK', 'Guest service crash', 'host sleep', 'stale queued dispatch'],
        'started_at_unix': time.time(), 'checks': [], 'observations': []}
    clients, worker, suspended, started, vm = [], None, False, False, None

    def save():
        (a.output / 'result.json').write_text(json.dumps(report, indent=2) + '\n')

    def record(name, value):
        report['checks'].append({'name': name, 'evidence': value})
        save()
        print(name, flush=True)

    def local(method, args):
        reply = subprocess.run([str(a.binary), 'call', '--state-dir', str(a.state), method, json.dumps(args)],
            capture_output=True, text=True, timeout=42)
        require(reply.returncode == 0, f'{method} failed: {reply.stderr[:8192]}')
        require(len(reply.stdout) <= 2 * 1024 * 1024, 'local reply too large')
        return json.loads(reply.stdout)

    def tool(client, method, **args):
        return client.tool(method, {'id': vm, **args})

    def execute(client, argv, timeout=1500):
        job = tool(client, 'exec_start', argv=argv, timeout_ms=timeout)['result']['job_id']
        until = time.monotonic() + 3
        while time.monotonic() < until:
            value = tool(client, 'exec_poll', job_id=job)['result']
            if value['state'] == 'finished':
                require('error' not in value, 'probe command cleanup failed')
                return value['result']
            time.sleep(.025)
        raise AssertionError('short command deadline')

    def close_client(client):
        # Process exit0 is recorded only as the adapter's cleanup report. It is
        # never inferred from worker death and never substitutes for cgroup ACK.
        value = {'pid': client.p.pid, 'forced': False}
        if client.p.poll() is None:
            if not client.p.stdin.closed:
                client.p.stdin.close()
            try:
                client.p.wait(timeout=42)
            except subprocess.TimeoutExpired:
                value['forced'] = True
                client.p.kill()  # Only this script's exact owned MCP child.
                client.p.wait(timeout=3)
        value['exit_code'] = client.p.returncode
        value['adapter_cleanup_confirmed'] = client.p.returncode == 0 and not value['forced']
        value['scope_cleanup_proven_by_this_script'] = False
        return value

    try:
        config = json.loads(a.config.read_text())
        vm = config['id']
        require(config.get('backend') == 'qemu-hvf' and config.get('guest') == 'linux'
            and config.get('qemu_guest_exec') is True, 'explicit Linux QEMU v2 required')
        require(vm not in {x['config']['id'] for x in local('list', {})}, 'NEW fixture VM ID required')
        disk = Path(config['disk']).resolve()
        require(disk.is_relative_to(ROOT / '.local') and a.base.is_relative_to(ROOT / '.local'), 'only explicit repository fixture clones')
        ds, bs = disk.stat(), a.base.stat()
        require(stat.S_ISREG(ds.st_mode) and stat.S_ISREG(bs.st_mode)
            and (ds.st_dev, ds.st_ino) != (bs.st_dev, bs.st_ino) and bs.st_mode & 0o222 == 0,
            'distinct clone and immutable base required')
        base_hash = digest(a.base)
        config['serial_log'] = str(a.output / 'guest.serial.log')
        (a.output / 'config.json').write_text(json.dumps(config, indent=2) + '\n')
        record('fixture identity', {'config': config, 'base_sha256': base_hash,
            'binary_sha256': digest(a.binary), 'worker_sha256': digest(a.worker_binary),
            'qemu_loader_sha256': digest(a.qemu_loader), 'disk_dev': ds.st_dev, 'disk_ino': ds.st_ino})
        local('create', config)
        clients.append(baseline.MCP(a.binary, a.state, False))
        clients.append(baseline.MCP(a.binary, a.state, True))
        life, owner = clients
        started = True
        record('start', tool(life, 'start'))
        until, boot = time.monotonic() + 180, None
        log = Path(config['serial_log'])
        while time.monotonic() < until:
            status = local('status', {'id': vm})
            require(status['state'] not in ('stopped', 'crashed', 'error'), 'fixture exited before readiness')
            if log.exists():
                require(log.stat().st_size <= desktop.MAX_LOG, 'serial log too large')
                boot = desktop.boot_evidence(log.read_bytes())
                if boot:
                    break
            time.sleep(.5)
        require(boot is not None, 'actual ext4/15-pixel readiness deadline')
        record('actual installed ext4 and 15 pixels', boot)
        info, startup_attempts = baseline.wait_guest_info(
            owner, vm, lambda: local('status', {'id': vm}), status['pid'])
        record('actual integration startup within original worker', {'temporary_refusals': startup_attempts})
        require(info.get('transport') == 'virtio-serial-private-exec-v2'
            and info.get('transport_reconnect') is False
            and info.get('execution_containment') == 'cgroup-v2-clone3-kill'
            and info.get('execution_identity', {}).get('rootless') is True
            and info.get('capabilities', {}).get('descendant_containment') is True, 'actual v2 containment precondition failed')
        record('actual guest info', info)
        record('initial lease', tool(owner, 'acquire_control'))
        nonce = uuid.uuid4().hex
        marker = '/home/veynode/guardian-ready-' + nonce
        command = ['/bin/sh', '-eu', '-c', 'test "$(id -u)" = 1000; '
            'case "$(cat /proc/self/cgroup)" in *:/veynode-exec/job-*) ;; *) exit 91;; esac; '
            'printf "%s" "$2" > "$1"; sleep 25', 'guardian-test', marker, nonce]
        job = tool(owner, 'exec_start', argv=command, timeout_ms=30000)['result']['job_id']
        until, ready = time.monotonic() + 3, False
        while time.monotonic() < until:
            value = execute(owner, ['/bin/sh', '-c', 'cat "$1" 2>/dev/null', 'ready', marker])
            if value['exit_code'] == 0 and value['stdout'] == nonce:
                ready = True
                break
            time.sleep(.025)
        require(ready, 'actual rootless long-running command readiness not observed')
        require(tool(owner, 'exec_poll', job_id=job)['result']['state'] == 'running', 'long job already ended')
        record('actual rootless cgroup command running', {'job_id': job, 'ready_nonce': nonce})
        status = local('status', {'id': vm})
        runtime = status['runtime']
        prov, guard = runtime['disk_provenance'], runtime['execution_guardian']
        require(prov.get('device') == ds.st_dev and prov.get('inode') == ds.st_ino
            and prov.get('descriptor_backed') is True and prov.get('qmp_fdsets_verified_at_start') is True, 'wrong disk binding')
        require(guard.get('owns_qemu_child') is True and guard.get('heartbeat_timeout_ms') == 5000
            and guard.get('backend_pid') == runtime.get('backend_pid'), 'wrong guardian contract')
        worker, guardian, backend = snapshot(status['pid']), snapshot(guard['guardian_pid']), snapshot(runtime['backend_pid'])
        require(worker and guardian and backend, 'original process chain missing')
        daemon = snapshot(worker['ppid'])
        require(daemon and daemon['executable'] == str(a.binary), 'worker parent is not exact controller')
        require(worker['executable'] == str(a.worker_binary) and guardian['executable'] == str(a.worker_binary)
            and backend['executable'] == str(a.qemu_loader), 'wrong actual process executables')
        require(guardian['ppid'] == worker['pid'] and backend['ppid'] == guardian['pid']
            and len({daemon['pid'], worker['pid'], guardian['pid'], backend['pid']}) == 4, 'invalid parent chain')
        require(all(x['uid'] == os.getuid() and x['state'] not in (4, 5) for x in (daemon, worker, guardian, backend)), 'not our live process chain')
        guardian_argv = command_arguments(guardian['pid'])
        require(len(guardian_argv) >= 5 and guardian_argv[1] == '--supervise-qemu'
            and guardian_argv[3] == '--' and Path(guardian_argv[4]).resolve() == a.qemu_loader,
            'guardian is not the exact QEMU supervisor invocation')
        argv = command_arguments(worker['pid'])
        stored_config = a.state / (vm + '.json')
        require(Path(option(argv, '--config')).resolve() == stored_config
            and json.loads(stored_config.read_text()) == status['config'], 'worker config binding mismatch')
        require(Path(option(command_arguments(daemon['pid']), '--state-dir')).resolve() == a.state,
            'controller state directory mismatch')
        record('verified original runtime and process chain', {'status': status,
            'daemon': daemon, 'worker': worker, 'guardian': guardian, 'backend': backend})
        if a.fault_target == 'worker':
            exact_live(worker)
            t0 = time.monotonic()
            os.kill(worker['pid'], signal.SIGSTOP)
            suspended = True
            until, reaped = t0 + 8, False
            absent_count = 0
            while time.monotonic() < until:
                w, d, g, b = snapshot(worker['pid']), snapshot(daemon['pid']), snapshot(guardian['pid']), snapshot(backend['pid'])
                require(same_incarnation(worker, w), 'original stopped worker disappeared')
                require(same_incarnation(daemon, d) and d['state'] not in (4, 5), 'controller did not remain running')
                require(g is None or (same_incarnation(guardian, g) and g['state'] != 4), 'guardian stopped or PID reused')
                report['observations'].append({'elapsed_seconds': time.monotonic() - t0,
                    'worker': w, 'guardian': g, 'backend': b, 'daemon_state': d['state']})
                if time.monotonic() - t0 > .5:
                    require(w['state'] == 4, 'worker not continuously stopped')
                if b is None:
                    absent_count += 1
                else:
                    require(same_incarnation(backend, b), 'backend PID reused during observation')
                    absent_count = 0
                # A zombie still exists: absence must be observed twice through BSD
                # process metadata, with guardian exit/Z and original worker still T.
                if absent_count >= 2 and w['state'] == 4 and (g is None or g['state'] == 5):
                    reaped = True
                    break
                time.sleep(.1)
            save()
            require(reaped, 'original QEMU not observed reaped within8s while worker stopped')
            require(time.monotonic() - t0 >= 4, 'backend ended before expected guardian heartbeat-expiry window')
            record('guardian backend reap observed without resuming worker', {'elapsed_seconds': time.monotonic() - t0,
                'backend_no_process_entry_twice': True, 'worker_still_stopped': True,
                'proof': 'original parent chain + public BSD process absence, not kill(pid,0); guardian exited/Z'})
            exact_live(worker)
            os.kill(worker['pid'], signal.SIGCONT)
            suspended = False
        else:
            # No Guest clock can advance while all QEMU threads are stopped.
            # The host transport must fail and terminate the original process;
            # this test never resumes it to let old Guest work continue.
            exact_live(backend)
            t0 = time.monotonic()
            os.kill(backend['pid'], signal.SIGSTOP)
            until, stop_observed = t0 + .5, False
            while time.monotonic() < until:
                b = snapshot(backend['pid'])
                report['observations'].append({'elapsed_seconds': time.monotonic() - t0,
                    'phase': 'confirm_qemu_stopped', 'backend': b})
                require(same_incarnation(backend, b), 'QEMU disappeared before actual pause was observed')
                if b['state'] == 4:
                    stop_observed = True
                    break
                time.sleep(.005)
            save()
            require(stop_observed, 'whole QEMU stop not observed')
            until, absent_count = t0 + 8, 0
            while time.monotonic() < until:
                w, d, g, b = snapshot(worker['pid']), snapshot(daemon['pid']), snapshot(guardian['pid']), snapshot(backend['pid'])
                report['observations'].append({'elapsed_seconds': time.monotonic() - t0,
                    'phase': 'host_fence', 'worker': w, 'guardian': g, 'backend': b, 'daemon': d})
                save()
                require(same_incarnation(daemon, d) and d['state'] not in (4, 5), 'controller did not remain running')
                for original, current in ((worker, w), (guardian, g), (backend, b)):
                    require(current is None or same_incarnation(original, current), 'process PID reused during fault')
                require(g is None or g['state'] != 4, 'guardian itself stopped')
                require(w is None or w['state'] != 4, 'worker itself stopped')
                if b is None:
                    absent_count += 1
                else:
                    require(b['state'] in (4, 5), 'QEMU unexpectedly resumed during containment')
                    absent_count = 0
                if absent_count >= 2 and (g is None or g['state'] == 5):
                    break
                time.sleep(.05)
            require(absent_count >= 2 and (g is None or g['state'] == 5),
                'host failed to retire stopped QEMU within8s')
            record('whole QEMU pause ended by host containment without resume', {
                'elapsed_seconds': time.monotonic() - t0, 'actual_stopped_state_observed': True,
                'backend_no_process_entry_twice': True, 'qemu_resume_sent': False,
                'proof': 'original process chain plus BSD process absence; no Guest timer proof'})
        until = time.monotonic() + 12
        while time.monotonic() < until:
            status = local('status', {'id': vm})
            if status['state'] in ('stopped', 'crashed', 'error'):
                break
            time.sleep(.1)
        require(status['state'] == 'crashed' and status.get('exit_code') not in (None, 0), 'worker failure not reported truthfully')
        started = False
        record('worker reports crashed after fault', status)
        stderr_path = Path(runtime['stderr_log'])
        if a.fault_target == 'qemu':
            require(status['runtime'].get('state') == 'error'
                and 'serial execution incarnation fenced' in status['runtime'].get('error', ''),
                'missing actual serial transport failure after QEMU pause')
        if stderr_path.exists():
            require(stderr_path.is_relative_to(a.state) and stderr_path.stat().st_size <= 8 * 1024 * 1024,
                'backend diagnostic log outside fixture bounds')
            (a.output / 'backend.stderr.log').write_bytes(stderr_path.read_bytes())
        denied = owner.tool('exec_start', {'id': vm, 'argv': ['/bin/true'], 'timeout_ms': 1000}, denied=True)
        require('Invalid tool arguments' not in json.dumps(denied), 'invalid arguments are not execution fencing')
        record('old execution rejected', denied)
        # Existing semantics allow same-owner renewal until scope retirement is
        # requested; worker death alone is deliberately not a cleanup proof.
        denied = owner.tool('release_control', {'id': vm}, denied=True)
        text = json.dumps(denied).lower()
        require('cleanup' in text and 'unconfirmed' in text, 'release did not retain unknown original-scope cleanup')
        record('original scope retirement remains explicitly unconfirmed', denied)
        denied = owner.tool('acquire_control', {'id': vm}, denied=True)
        require('previous execution scope cleanup unconfirmed; acquire denied' in json.dumps(denied),
            'new lease not fenced by unresolved original scope')
        record('old lease renewal rejected after retirement attempt', denied)
        outcomes = [close_client(client) for client in clients]
        clients.clear()
        require(not any(x['forced'] for x in outcomes), 'MCP EOF deadline required forced child termination')
        record('MCP EOF outcomes independent of worker death', outcomes)
        require(snapshot(backend['pid']) is None and snapshot(guardian['pid']) is None, 'backend/guardian process remained after worker retirement')
        require(digest(a.base) == base_hash, 'immutable base changed')
        record('immutable base unchanged', base_hash)
        report['passed'] = True
    except BaseException as error:
        report['error'] = str(error)
        raise
    finally:
        if suspended and worker:
            try:
                value = exact_live(worker)
                if value['state'] == 4:
                    os.kill(worker['pid'], signal.SIGCONT)
                report['finally_resumed_own_worker'] = True
            except Exception as error:
                report['resume_error'] = str(error)
        if started and vm:
            try:
                status = local('status', {'id': vm})
                if status['state'] not in ('stopped', 'crashed', 'error'):
                    report['failure_force_stop'] = local('force_stop', {'id': vm, 'confirm': vm})
                    until = time.monotonic() + 15
                    while time.monotonic() < until:
                        status = local('status', {'id': vm})
                        if status['state'] in ('stopped', 'crashed', 'error'):
                            break
                        time.sleep(.1)
                report['failure_terminal_status'] = status
                require(status['state'] in ('stopped', 'crashed', 'error'), 'failure cleanup unconfirmed')
            except Exception as error:
                report['cleanup_error'] = str(error)
        for client in clients:
            try:
                report.setdefault('finally_mcp_outcomes', []).append(close_client(client))
            except Exception as error:
                report.setdefault('client_cleanup_errors', []).append(str(error))
        report['finished_at_unix'] = time.time()
        save()


if __name__ == '__main__':
    main()
