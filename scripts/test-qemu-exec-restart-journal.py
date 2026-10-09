#!/usr/bin/env python3
"""DESTRUCTIVE opt-in test: one new fixture VM and its dedicated daemon only.

Terminate exactly --daemon-pid after a real rootless command, then spawn one
replacement using that daemon's exact public Darwin argv and state directory.
Never reconcile the durable journal or infer Guest cleanup from process death.
Original worker/guardian/QEMU must first be absent twice: this verifies durable
fencing and old-owner EOF, NOT safety of overlapping old/new daemon lifetimes.
Replay preserves argv/state, not the original environment. Only explicitly
configured absolute backend paths are accepted; no environment is exported.
No worker stop/resume; no GUI or unrelated processes. Root runs this fixture.
"""
import argparse
import ctypes
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import stat
import struct
import subprocess
import sys
import threading
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
_spec = importlib.util.spec_from_file_location('daemon_fixture', ROOT / 'scripts/test-qemu-exec-daemon-eof.py')
fixture = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(fixture)
baseline, desktop = fixture.baseline, fixture.desktop
require, digest = fixture.require, fixture.digest
snapshot, same_incarnation, exact_live = fixture.snapshot, fixture.same_incarnation, fixture.exact_live
command_arguments, option = fixture.command_arguments, fixture.option


def private_write(path, data):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'wb') as target:
        target.write(data)


def parse_procargs(raw):
    # sysctl KERN_PROCARGS2: argc int, executable C string, padding, argv.
    # Stop exactly at argc; never return or record the following environment.
    require(4 < len(raw) <= 1024 * 1024, 'bounded procargs required')
    argc = struct.unpack_from('=i', raw)[0]
    require(2 <= argc <= 128, 'invalid daemon argc')
    end = raw.find(b'\0', 4)
    require(end > 4, 'missing executable terminator')
    pos = end + 1
    while pos < len(raw) and raw[pos] == 0:
        pos += 1
    argv = []
    for _ in range(argc):
        end = raw.find(b'\0', pos)
        require(end >= pos, 'truncated argv')
        argv.append(os.fsdecode(raw[pos:end]))
        pos = end + 1
    require(sum(len(x) for x in argv) <= 32768, 'daemon argv too large')
    return argv


def process_argv(pid):
    # Public macOS SDK sys/sysctl.h: CTL_KERN=1, KERN_PROCARGS2=49.
    lib = ctypes.CDLL('/usr/lib/libSystem.B.dylib', use_errno=True)
    lib.sysctl.argtypes = [ctypes.POINTER(ctypes.c_int), ctypes.c_uint, ctypes.c_void_p,
        ctypes.POINTER(ctypes.c_size_t), ctypes.c_void_p, ctypes.c_size_t]
    lib.sysctl.restype = ctypes.c_int
    mib = (ctypes.c_int * 3)(1, 49, pid)
    size = ctypes.c_size_t()
    require(lib.sysctl(mib, 3, None, ctypes.byref(size), None, 0) == 0
        and 4 < size.value <= 1024 * 1024, 'procargs size query failed')
    data = ctypes.create_string_buffer(size.value)
    require(lib.sysctl(mib, 3, data, ctypes.byref(size), None, 0) == 0, 'procargs read failed')
    return parse_procargs(data.raw[:size.value])


def validate_daemon_argv(argv, binary, state):
    require(len(argv) >= 6 and Path(argv[0]).resolve() == binary and argv[1] == 'daemon', 'not exact daemon invocation')
    allowed = {'--state-dir', '--backend', '--memory-budget-mib', '--qemu-worker', '--qemu-launcher',
        '--qemu-library', '--qemu-resources', '--qemu-spice-library', '--qemu-screenshot-helper',
        '--qemu-console-broker', '--qemu-console-helper'}
    require((len(argv) - 2) % 2 == 0, 'daemon options must have explicit values')
    names = argv[2::2]
    require(len(names) == len(set(names)) and set(names) <= allowed, 'unexpected or duplicate daemon argument')
    require(Path(option(argv, '--state-dir')).resolve() == state, 'daemon state mismatch')
    require(all(value and not value.startswith('--') for value in argv[3::2]), 'missing argument value')
    for name, value in zip(names, argv[3::2]):
        if name == '--memory-budget-mib':
            require(value.isdigit() and 0 < int(value) <= 65536, 'memory budget invalid')
        else:
            require(Path(value).is_absolute(), 'relative daemon path cannot be safely replayed')


def strict_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, 'duplicate journal key')
        result[key] = value
    return result


def validate_journal(raw):
    require(len(raw) <= 131072, 'journal size exceeded')
    value = json.loads(raw, object_pairs_hook=strict_object)
    require(isinstance(value, dict) and set(value) == {'version', 'scopes'}
        and type(value['version']) is int and value['version'] == 1
        and isinstance(value['scopes'], list) and len(value['scopes']) <= 256, 'journal schema mismatch')
    seen = set()
    for record in value['scopes']:
        require(isinstance(record, dict) and set(record) == {'vm', 'worker_instance', 'owner_sha256', 'epoch'},
            'journal contains unexpected fields, possibly command or output')
        require(isinstance(record['vm'], str) and re.fullmatch(r'[a-zA-Z0-9_-]{1,64}', record['vm'])
            and all(isinstance(record[k], str) and re.fullmatch(r'[a-f0-9]{64}', record[k]) for k in ('worker_instance', 'owner_sha256'))
            and type(record['epoch']) is int and 1 <= record['epoch'] <= 9007199254740991, 'invalid journal scope')
        key = tuple(record[k] for k in ('vm', 'worker_instance', 'owner_sha256', 'epoch'))
        require(key not in seen, 'duplicate journal scope')
        seen.add(key)
    return value


def read_journal(state):
    directory = state / 'exec-journal'
    ds = directory.lstat()
    require(stat.S_ISDIR(ds.st_mode) and ds.st_uid == os.getuid() and stat.S_IMODE(ds.st_mode) == 0o700, 'unsafe journal directory')
    def read(name, limit):
        path = directory / name
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, 'rb') as source:
            before = os.fstat(source.fileno())
            require(stat.S_ISREG(before.st_mode) and before.st_uid == os.getuid() and before.st_nlink == 1
                and stat.S_IMODE(before.st_mode) == 0o600 and before.st_size <= limit, 'unsafe journal file')
            data = source.read(limit + 1)
            after = os.fstat(source.fileno()); at = path.lstat()
            fields = ('st_dev', 'st_ino', 'st_size', 'st_mtime_ns', 'st_ctime_ns')
            require(len(data) <= limit and all(getattr(before,k) == getattr(after,k) == getattr(at,k) for k in fields), 'journal changed while reading')
        return data
    require(read('exec-scopes.required', 3) == b'v1\n', 'journal marker mismatch')
    raw = read('exec-scopes.json', 131072)
    return validate_journal(raw), raw


def require_retained(before, after):
    # No other clients mutate this exclusive test daemon. Exact snapshot equality
    # catches deletion, substitution, duplicate records and unplanned additions.
    require(before == after and len(before['scopes']) == 1, 'original recovery obligation changed or cleared')


def drain_log(source, path, overflow):
    with source, os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), 'wb') as target:
        remaining = 1024 * 1024
        while True:
            block = source.read(65536)
            if not block:
                return
            target.write(block[:remaining])
            if len(block) > remaining:
                overflow.set()
            remaining = max(0, remaining - len(block))


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ('binary', 'state', 'config', 'base', 'qemu-loader', 'output'):
        p.add_argument('--' + name, type=Path, required=True)
    p.add_argument('--worker-binary', type=Path)
    p.add_argument('--daemon-pid', type=int, required=True, help='exact dedicated test daemon to terminate and replace with identical argv')
    a = p.parse_args()
    a.worker_binary = a.worker_binary or a.binary.parent / 'vm-qemu'
    for key, value in vars(a).items():
        if isinstance(value, Path):
            setattr(a, key, value.resolve())
    require(sys.platform == 'darwin', 'this actual fixture requires Darwin process identity APIs')
    a.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    report = {'passed': False, 'oracle_version': 1, 'scope': 'actual daemon restart retains unresolved execution journal; no reconciliation',
        'not_tested': ['successful reconciliation', 'Guest cleanup acknowledgement after loss', 'crash-consistent storage under power loss', 'host sleep', 'resuming original QEMU'],
        'started_at_unix': time.time(), 'checks': [], 'observations': []}
    clients, worker, started, vm = [], None, False, None
    daemon, daemon_terminated = None, False
    replacement, replacement_identity, log_thread, log_overflow = None, None, None, threading.Event()
    original, base_hash = None, None

    def save():
        private_write(a.output / 'result.json', (json.dumps(report, indent=2) + '\n').encode())

    def record(name, value):
        report['checks'].append({'name': name, 'evidence': value})
        save()
        print(name, flush=True)

    def local(method, args, timeout=42):
        reply = subprocess.run([str(a.binary), 'call', '--state-dir', str(a.state), method, json.dumps(args)],
            capture_output=True, text=True, timeout=timeout)
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
        registered = local('list', {})
        require(vm not in {x['config']['id'] for x in registered}, 'NEW fixture VM ID required')
        require(all(x['state'] in ('stopped', 'crashed', 'error') for x in registered),
            'dedicated test daemon already has an active VM')
        require(all(x['config'].get('backend') == 'qemu-hvf' and x['config'].get('guest') == 'linux'
            for x in registered), 'refusing daemon with macOS or unrelated backend records')
        daemon = snapshot(a.daemon_pid)
        require(daemon and daemon['uid'] == os.getuid() and daemon['executable'] == str(a.binary)
            and daemon['state'] not in (4, 5), 'dedicated daemon executable/PID mismatch')
        require(Path(option(command_arguments(a.daemon_pid), '--state-dir')).resolve() == a.state,
            'dedicated daemon state directory mismatch')
        daemon_argv = process_argv(daemon['pid'])
        validate_daemon_argv(daemon_argv, a.binary, a.state)
        require(Path(option(daemon_argv, '--qemu-worker')).resolve() == a.worker_binary
            and Path(option(daemon_argv, '--qemu-launcher')).resolve() == a.qemu_loader, 'daemon backend argument mismatch')
        before_journal, before_bytes = read_journal(a.state)
        require(before_journal['scopes'] == [], 'fresh dedicated daemon journal must be empty')
        record('explicit dedicated daemon authorization precondition', {'daemon': daemon, 'registry': registered, 'argv': daemon_argv, 'journal': before_journal})
        disk = Path(config['disk']).resolve()
        require(disk.is_relative_to(ROOT / '.local') and a.base.is_relative_to(ROOT / '.local'), 'only explicit repository fixture clones')
        ds, bs = disk.stat(), a.base.stat()
        require(stat.S_ISREG(ds.st_mode) and stat.S_ISREG(bs.st_mode)
            and (ds.st_dev, ds.st_ino) != (bs.st_dev, bs.st_ino) and bs.st_mode & 0o222 == 0,
            'distinct clone and immutable base required')
        base_hash = digest(a.base)
        config['serial_log'] = str(a.output / 'guest.serial.log')
        (a.output / 'config.json').write_text(json.dumps(config, indent=2) + '\n')
        original_binary_hash = digest(a.binary)
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
        lease = tool(owner, 'acquire_control')
        record('initial lease', lease)
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
        require(worker['ppid'] == a.daemon_pid and same_incarnation(daemon, snapshot(a.daemon_pid)),
            'worker parent is not explicitly authorized dedicated daemon')
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
        original = {'daemon': daemon, 'worker': worker, 'guardian': guardian, 'backend': backend}
        record('verified original runtime and process chain', {'status': status,
            'daemon': daemon, 'worker': worker, 'guardian': guardian, 'backend': backend})
        journal, journal_bytes = read_journal(a.state)
        instance = option(argv, '--guest-worker-instance')
        require(len(journal['scopes']) == 1, 'expected one durable execution scope')
        scope = journal['scopes'][0]
        require(scope['vm'] == vm and scope['worker_instance'] == instance
            and scope['epoch'] == lease['epoch'], 'journal not bound to original worker/lease')
        require(nonce.encode() not in journal_bytes and job.encode() not in journal_bytes,
            'journal contains workload data')
        record('original scope durably recorded without command or output fields', {
            'journal': journal, 'sha256': hashlib.sha256(journal_bytes).hexdigest(),
            'marker': 'v1', 'publication_order_contract': 'production fsync before exec_start dispatch; not a power-loss test'})
        registered = local('list', {})
        active = [x for x in registered if x['state'] not in ('stopped', 'crashed', 'error')]
        require(len(active) == 1 and active[0]['config']['id'] == vm and active[0].get('pid') == worker['pid'],
            'another active VM appeared; refusing daemon termination')
        require(all(x['config'].get('backend') == 'qemu-hvf' and x['config'].get('guest') == 'linux'
            for x in registered), 'unrelated backend appeared')
        require(process_argv(daemon['pid']) == daemon_argv, 'daemon argv changed')
        exact_live(daemon)
        original = {'daemon': daemon, 'worker': worker, 'guardian': guardian, 'backend': backend}
        t0 = time.monotonic()
        os.kill(daemon['pid'], signal.SIGTERM)
        daemon_terminated = True
        report['daemon_signal'] = {'pid': daemon['pid'], 'signal': 'SIGTERM', 'worker_stopped': False}
        save()
        oracle = fixture.RetirementOracle(original)

        def observe(phase):
            values, errors = {}, {}
            for name, process in original.items():
                try:
                    values[name] = snapshot(process['pid'])
                except Exception as error:
                    errors[name] = str(error)
            row = {'phase': phase, 'elapsed_seconds': time.monotonic() - t0,
                'processes': values, 'snapshot_errors': errors}
            report['observations'].append(row)
            save()
            require(not errors, 'process observation failed')
            decision = oracle.observe(values)
            row['decision'] = decision
            save()
            return decision

        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            decision = observe('original_daemon_and_vm_retirement')
            if decision['daemon_ended'] and decision['all_vm_absent_twice']:
                break
            time.sleep(.1)
        require(decision['daemon_ended'] and decision['all_vm_absent_twice'], 'original process retirement unconfirmed')
        started = False
        record('original processes retired without scope cleanup inference', {
            'elapsed_seconds': time.monotonic() - t0, 'decision': decision, 'scope_cleanup_confirmed': False})
        require_retained(journal, read_journal(a.state)[0])
        require(digest(a.binary) == original_binary_hash, 'daemon binary changed before restart')
        # One new owned child only, exactly the original argv. No shell, env dump,
        # process-group signals, new backend flags or automatic retries.
        replacement = subprocess.Popen(daemon_argv, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, cwd=ROOT, start_new_session=True)
        log_thread = threading.Thread(target=drain_log, args=(replacement.stdout,
            a.output / 'replacement-daemon.log', log_overflow), daemon=True)
        log_thread.start()
        deadline = time.monotonic() + 10
        startup_errors = []
        recovery = None
        while time.monotonic() < deadline:
            require(replacement.poll() is None, 'replacement daemon exited during startup')
            require(not log_overflow.is_set(), 'replacement daemon log exceeded bound')
            try:
                recovery = local('status', {'id': vm}, timeout=1)
                break
            except (AssertionError, subprocess.TimeoutExpired) as error:
                startup_errors.append(str(error)[:512])
                time.sleep(.1)
        require(recovery is not None, 'replacement daemon readiness deadline')
        replacement_identity = snapshot(replacement.pid)
        require(replacement_identity and replacement_identity['ppid'] == os.getpid()
            and replacement_identity['uid'] == os.getuid() and replacement_identity['executable'] == str(a.binary),
            'replacement child identity mismatch')
        require(process_argv(replacement.pid) == daemon_argv, 'replacement argv mismatch')
        record('same argv replacement daemon serving original state', {'child': replacement_identity,
            'argv': daemon_argv, 'startup_errors': startup_errors, 'status': recovery})
        require(recovery['state'] == 'recovery_required' and recovery.get('execution_recovery_required') is True,
            'restart failed to fence recovered scope')
        require_retained(journal, read_journal(a.state)[0])
        fresh = baseline.MCP(a.binary, a.state, True)
        clients.append(fresh)
        expected_error = 'execution recovery required; original scope cleanup unconfirmed'
        for method, arguments in [('start', {}), ('acquire_control', {}),
                ('exec_start', {'argv': ['/bin/true'], 'timeout_ms': 1000})]:
            reply = fresh.tool(method, {'id': vm, **arguments}, denied=True)
            require(expected_error in json.dumps(reply), 'wrong recovery refusal')
            record('recovered VM denies ' + method, reply)
        release = owner.tool('release_control', {'id': vm}, denied=True)
        require(expected_error in json.dumps(release), 'old owner release did not retain unknown obligation')
        record('old owner release remains denied', release)
        # Crucial: original MCP is still alive until the replacement is serving.
        # A new owner alone cannot demonstrate hash matching the old owner token.
        require(owner.p.poll() is None, 'old execution-owner adapter exited before restart test')
        owner_result = close_client(owner)
        clients.remove(owner)
        record('old owner EOF reaches replacement and remains unconfirmed', owner_result)
        require(owner_result['exit_code'] == 1 and not owner_result['forced']
            and owner_result['adapter_cleanup_confirmed'] is False, 'old owner EOF falsely confirmed or timed out')
        after, after_bytes = read_journal(a.state)
        require_retained(journal, after)
        after_status = local('status', {'id': vm})
        require(after_status['state'] == 'recovery_required' and after_status.get('execution_recovery_required') is True,
            'old EOF cleared recovery state')
        record('original journal survives old owner EOF unchanged', {'journal': after,
            'sha256': hashlib.sha256(after_bytes).hexdigest(), 'status': after_status})
        outcomes = [close_client(client) for client in clients]
        clients.clear()
        record('remaining MCP adapter outcomes', outcomes)
        require(all(not item['forced'] for item in outcomes), 'remaining MCP required forced termination')
        exact_live(replacement_identity)
        replacement.terminate()
        replacement.wait(timeout=10)
        log_thread.join(timeout=2)
        require(not log_thread.is_alive() and not log_overflow.is_set(), 'replacement log drain failed')
        record('owned replacement daemon reaped', {'pid': replacement.pid, 'exit_code': replacement.returncode,
            'forced': False, 'scope_cleanup_confirmed': False})
        require(replacement.returncode == -signal.SIGTERM, 'replacement did not exit from intended SIGTERM')
        require_retained(journal, read_journal(a.state)[0])
        observe('final_original_process_absence_1')
        time.sleep(.1)
        require(observe('final_original_process_absence_2')['all_vm_absent_twice'], 'original VM process reappeared')
        require(digest(a.base) == base_hash, 'immutable base changed')
        record('immutable base unchanged', base_hash)
        report['passed'] = True
    except BaseException as error:
        report['error'] = str(error)
        raise
    finally:
        # Before injection, only the newly created VM can be force-stopped via
        # its still-pinned original daemon. After injection no guessed cleanup
        # or recovered-ledger deletion is attempted.
        if started and not daemon_terminated and vm:
            try:
                exact_live(daemon)
                report['failure_force_stop'] = local('force_stop', {'id': vm, 'confirm': vm})
                deadline = time.monotonic() + 15
                while time.monotonic() < deadline:
                    status = local('status', {'id': vm})
                    if status['state'] in ('stopped', 'crashed', 'error'):
                        break
                    time.sleep(.1)
                report['failure_terminal_status'] = status
                require(status['state'] in ('stopped', 'crashed', 'error'), 'failure VM retirement unconfirmed')
                report['failure_terminal_observed'] = True
                report['failure_scope_cleanup_confirmed'] = False
            except Exception as error:
                report['cleanup_error'] = str(error)
                report['passed'] = False
        for client in clients:
            try:
                outcome = close_client(client)
                report.setdefault('finally_mcp_outcomes', []).append(outcome)
                if outcome['forced']:
                    report['passed'] = False
            except Exception as error:
                report.setdefault('cleanup_errors', []).append(str(error))
                report['passed'] = False
        if replacement and replacement.poll() is None:
            try:
                if replacement_identity:
                    exact_live(replacement_identity)
                replacement.terminate()  # unreaped, owned Popen child; PID cannot be reused
                try:
                    replacement.wait(timeout=10)
                    forced = False
                except subprocess.TimeoutExpired:
                    replacement.kill()
                    replacement.wait(timeout=3)
                    forced = True
                report['finally_replacement'] = {'pid': replacement.pid,
                    'exit_code': replacement.returncode, 'forced': forced}
                if forced:
                    report['passed'] = False
            except Exception as error:
                report.setdefault('cleanup_errors', []).append(str(error))
                report['passed'] = False
        if log_thread:
            log_thread.join(timeout=2)
            if log_thread.is_alive() or log_overflow.is_set():
                report['replacement_log_error'] = 'drain incomplete or exceeded 1MiB'
                report['passed'] = False
        if original:
            try:
                deadline = time.monotonic() + 15
                absence = {name: 0 for name in ('worker', 'guardian', 'backend')}
                while time.monotonic() < deadline:
                    values, errors = {}, {}
                    for name, identity in original.items():
                        try:
                            values[name] = snapshot(identity['pid'])
                        except Exception as error:
                            errors[name] = str(error)
                    report.setdefault('finally_process_observations', []).append({
                        'monotonic': time.monotonic(), 'processes': values, 'errors': errors})
                    save()  # retain raw failure before identity/absence assertions
                    require(not errors, 'final process metadata unavailable')
                    for name, value in values.items():
                        require(value is None or same_incarnation(original[name], value),
                            'original PID reused; no cleanup inference or signal allowed')
                    for name in absence:
                        absence[name] = absence[name] + 1 if values[name] is None else 0
                    report['final_original_processes'] = values
                    if all(count >= 2 for count in absence.values()):
                        break
                    time.sleep(.1)
                report['final_process_retirement_confirmed'] = all(count >= 2 for count in absence.values())
                report['final_scope_cleanup_confirmed'] = False
                if not report['final_process_retirement_confirmed']:
                    report['cleanup_error'] = 'original VM process retirement unconfirmed; no broad signals sent'
                    report['passed'] = False
            except Exception as error:
                report['cleanup_error'] = str(error)
                report['passed'] = False
        if base_hash:
            try:
                report['final_base_sha256'] = digest(a.base)
                if report['final_base_sha256'] != base_hash:
                    report['passed'] = False
            except Exception as error:
                report['base_hash_error'] = str(error)
                report['passed'] = False
        report['finished_at_unix'] = time.time()
        save()
    require(report['passed'], 'restart acceptance failed during final cleanup; inspect saved report')


if __name__ == '__main__':
    main()
