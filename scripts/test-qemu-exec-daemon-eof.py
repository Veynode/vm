#!/usr/bin/env python3
"""Actual dedicated-daemon death after stopping its one new QEMU worker.

DANGEROUS TEST FIXTURE: --daemon-pid explicitly identifies the dedicated test
controller to SIGTERM. It must have no other active VM, no macOS records, and the
exact --state directory. No daemon is started here. The caller keeps this daemon
exclusive (no concurrent create/start); process identities are rechecked before
signals. Oracle v2 verifies original process retirement, not guardian timeout.
POSIX orphan-group SIGHUP/SIGCONT may end/resume the worker before our next sample:
https://pubs.opengroup.org/onlinepubs/9799919799/functions/_exit.html
No signal trace establishes which mechanism caused a particular exit. The prior
failed oracle remains archived in archive/docs-evidence/installed-qemu-daemon-eof-failure.
No durable cleanup reconciliation or Guest cgroup ACK is claimed.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import signal
import stat
import subprocess
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
_spec = importlib.util.spec_from_file_location('guardian_fixture', ROOT / 'scripts/test-qemu-exec-guardian.py')
guardian_fixture = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(guardian_fixture)
baseline = guardian_fixture.baseline
require, digest, desktop = guardian_fixture.require, guardian_fixture.digest, guardian_fixture.desktop
snapshot, same_incarnation = guardian_fixture.snapshot, guardian_fixture.same_incarnation
exact_live, command_arguments, option = guardian_fixture.exact_live, guardian_fixture.command_arguments, guardian_fixture.option


class RetirementOracle:
    """Pure identity/absence oracle; never sends signals or infers Guest cleanup."""
    def __init__(self, original):
        self.original = original
        self.absent = {name: 0 for name in original}

    def observe(self, processes):
        require(set(processes) == set(self.original), 'incomplete process sample')
        for name, value in processes.items():
            require(value is None or same_incarnation(self.original[name], value),
                name + ' PID was reused; no signal or retirement inference allowed')
            self.absent[name] = self.absent[name] + 1 if value is None else 0
        daemon = processes['daemon']
        worker = processes['worker']
        daemon_ended = daemon is None or daemon['state'] == 5
        backend_retired = self.absent['backend'] >= 2
        return {'daemon_ended': daemon_ended, 'backend_absent_twice': backend_retired,
            'all_vm_absent_twice': all(self.absent[name] >= 2 for name in ('worker', 'guardian', 'backend')),
            'resume_worker': daemon_ended and backend_retired and worker is not None and worker['state'] == 4,
            'absence_counts': dict(self.absent)}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ('binary', 'state', 'config', 'base', 'qemu-loader', 'output'):
        p.add_argument('--' + name, type=Path, required=True)
    p.add_argument('--worker-binary', type=Path)
    p.add_argument('--daemon-pid', type=int, required=True, help='exact dedicated test daemon to terminate')
    a = p.parse_args()
    a.worker_binary = a.worker_binary or a.binary.parent / 'vm-qemu'
    for key, value in vars(a).items():
        if isinstance(value, Path):
            setattr(a, key, value.resolve())
    require(sys.platform == 'darwin', 'this actual fixture requires Darwin process identity APIs')
    a.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    report = {'passed': False, 'oracle_version': 2, 'scope': 'actual dedicated-daemon death after worker stop; original process retirement, mechanism not inferred',
        'not_tested': ['independent guardian timeout during this combined fault', 'signal cause attribution', 'lost start ACK', 'Guest service crash', 'whole VM pause', 'durable recovery', 'Guest cgroup cleanup acknowledgement'],
        'started_at_unix': time.time(), 'checks': [], 'observations': []}
    clients, worker, suspended, started, vm = [], None, False, False, None
    daemon, daemon_terminated = None, False

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
        record('explicit dedicated daemon authorization precondition', {'daemon': daemon, 'registry': registered})
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
        record('verified original runtime and process chain', {'status': status,
            'daemon': daemon, 'worker': worker, 'guardian': guardian, 'backend': backend})
        # Final registry check immediately before injecting either fault. The
        # caller must keep this dedicated daemon exclusive throughout the run.
        registered = local('list', {})
        active = [x for x in registered if x['state'] not in ('stopped', 'crashed', 'error')]
        require(len(active) == 1 and active[0]['config']['id'] == vm
            and active[0].get('pid') == worker['pid'], 'another active VM appeared; refusing daemon termination')
        require(all(x['config'].get('backend') == 'qemu-hvf' and x['config'].get('guest') == 'linux'
            for x in registered), 'unrelated backend appeared; refusing daemon termination')
        record('exclusive registry immediately before fault', registered)
        topology = {name: {'pid': process['pid'], 'process_group': os.getpgid(process['pid']),
            'session_id': os.getsid(process['pid'])}
            for name, process in {'daemon': daemon, 'worker': worker, 'guardian': guardian, 'backend': backend}.items()}
        record('pre-fault process groups and sessions, not signal attribution', topology)
        exact_live(worker)
        t0 = time.monotonic()
        os.kill(worker['pid'], signal.SIGSTOP)
        suspended = True
        stopped_deadline = time.monotonic() + .5
        while time.monotonic() < stopped_deadline:
            w = exact_live(worker)
            report.setdefault('worker_stop_observations', []).append({'elapsed_seconds': time.monotonic() - t0, 'worker': w})
            save()
            if w['state'] == 4:
                break
            time.sleep(.01)
        require(w['state'] == 4, 'worker did not enter stopped state before daemon EOF')
        exact_live(daemon)
        os.kill(daemon['pid'], signal.SIGTERM)
        daemon_terminated = True
        report['daemon_signal'] = {'pid': daemon['pid'], 'signal': 'SIGTERM',
            'elapsed_from_worker_stop': time.monotonic() - t0}
        original = {'worker': worker, 'daemon': daemon, 'guardian': guardian, 'backend': backend}
        oracle = RetirementOracle(original)

        def observe(phase):
            values, errors = {}, {}
            for name, process in original.items():
                try:
                    values[name] = snapshot(process['pid'])
                except Exception as error:
                    errors[name] = str(error)
            row = {'phase': phase, 'elapsed_seconds': time.monotonic() - t0,
                'processes': values, 'snapshot_errors': errors}
            # Persist raw observations BEFORE any oracle assertion, including
            # rapid exits, resumed workers, zombies and PID reuse failures.
            report['observations'].append(row)
            save()
            require(not errors, 'process snapshot unavailable; no absence proof')
            decision = oracle.observe(values)
            row['decision'] = decision
            save()
            return values, decision

        until, decision = t0 + 8, None
        while time.monotonic() < until:
            values, decision = observe('after_daemon_termination')
            if decision['daemon_ended'] and decision['backend_absent_twice']:
                break
            time.sleep(.1)
        require(decision and decision['daemon_ended'] and decision['backend_absent_twice'],
            'daemon ended plus original QEMU retirement not observed within8s')
        record('original QEMU retired after combined daemon and worker fault', {
            'elapsed_seconds': time.monotonic() - t0,
            'daemon_exit_observed': decision['daemon_ended'], 'backend_no_process_entry_twice': True,
            'guardian_timeout_proven': False, 'exit_signal_cause': 'not traced',
            'proof': 'two BSD process absence observations of original incarnation; not kill(pid,0)'})
        # The OS may already have resumed or terminated an orphaned stopped
        # process group. Only explicitly continue an original worker still T,
        # and only after the QEMU child has actually disappeared twice.
        values, decision = observe('before_optional_worker_resume')
        if decision['resume_worker']:
            current = exact_live(worker)
            report['worker_resume_precondition'] = current
            save()
            if current['state'] == 4:
                os.kill(worker['pid'], signal.SIGCONT)
                report['worker_resume_action'] = 'SIGCONT sent to original stopped worker after backend retirement'
            else:
                report['worker_resume_action'] = 'no signal: original worker already continued before final check'
        else:
            report['worker_resume_action'] = 'no signal: worker already continued or exited'
        suspended = False
        save()
        until = time.monotonic() + 15
        while time.monotonic() < until:
            values, decision = observe('final_process_retirement')
            if decision['all_vm_absent_twice']:
                break
            time.sleep(.1)
        require(decision['all_vm_absent_twice'], 'original worker/guardian/QEMU did not all leave process table')
        started = False
        record('all three original VM processes retired', {
            'absence_counts': decision['absence_counts'], 'elapsed_from_stop_seconds': time.monotonic() - t0,
            'exit_code': None, 'exit_code_note': 'script is not worker parent; no invented wait status',
            'daemon_status_used_after_termination': False, 'scope_cleanup_confirmed': False,
            'guardian_independent_timeout_proven': False})
        stderr_path = Path(runtime['stderr_log'])
        if stderr_path.exists():
            require(stderr_path.is_relative_to(a.state) and stderr_path.stat().st_size <= 8 * 1024 * 1024,
                'backend diagnostic log outside fixture bounds')
            (a.output / 'backend.stderr.log').write_bytes(stderr_path.read_bytes())
        outcomes = [close_client(client) for client in clients]
        clients.clear()
        require(not any(x['forced'] for x in outcomes), 'MCP EOF deadline required forced child termination')
        require(outcomes[1]['adapter_cleanup_confirmed'] is False,
            'execution owner falsely confirmed cleanup after daemon loss without a Guest scope acknowledgement')
        record('MCP EOF outcomes independent of worker death', {'lifecycle_client': outcomes[0],
            'execution_owner': outcomes[1], 'execution_scope_cleanup_confirmed': False})
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
                value = snapshot(worker['pid'])
                report['finally_worker_before_resume'] = value
                save()
                if value is None:
                    report['finally_worker_resume_action'] = 'no signal: original worker absent'
                else:
                    require(same_incarnation(worker, value), 'PID reused; refusing cleanup signal')
                    if value['state'] == 4:
                        if daemon_terminated:
                            # Do not resume a stopped authority worker until its
                            # original backend is independently observed absent.
                            first = snapshot(backend['pid'])
                            time.sleep(.1)
                            second = snapshot(backend['pid'])
                            report['finally_backend_resume_precondition'] = [first, second]
                            save()
                            require(first is None and second is None,
                                'backend retirement unconfirmed; stopped worker retained for explicit recovery')
                        exact_live(worker)
                        os.kill(worker['pid'], signal.SIGCONT)
                        report['finally_worker_resume_action'] = 'SIGCONT original stopped worker for failure cleanup'
                    else:
                        report['finally_worker_resume_action'] = 'no signal: original worker not stopped'
            except Exception as error:
                report['resume_error'] = str(error)
        if started and vm and not daemon_terminated:
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
        if started and daemon_terminated:
            # No dead-daemon RPC and no broad process-group kill. SIGCONT above
            # permits the worker to observe its actual daemon EOF. If it cannot
            # retire, preserve the exact unresolved identities for the operator.
            try:
                until = time.monotonic() + 15
                while time.monotonic() < until and worker and snapshot(worker['pid']) is not None:
                    time.sleep(.1)
                report['failure_processes_after_resume'] = {
                    'worker': snapshot(worker['pid']) if worker else None,
                    'guardian': snapshot(guardian['pid']) if 'guardian' in locals() and guardian else None,
                    'backend': snapshot(backend['pid']) if 'backend' in locals() and backend else None}
                if any(report['failure_processes_after_resume'].values()):
                    report['cleanup_error'] = 'dedicated daemon ended; original process cleanup remains unconfirmed'
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
