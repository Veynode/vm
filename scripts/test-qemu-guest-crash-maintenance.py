#!/usr/bin/env python3
"""Operator-invoked RAM updater acceptance; attach an existing guarded daemon.

Every local reply, startup proof and final assertion input is saved before
validation. Failure cleanup has a separate evidence phase. This script never
starts a daemon or accepts a Guest PASS marker as host disk/exit proof.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import time


def require(ok, message):
    if not ok:
        raise AssertionError(message)


def append_trace(path, value):
    data = (json.dumps(value, sort_keys=True) + '\n').encode()
    require(len(data) <= 2 * 1024**2 + 65536, 'trace record too large')
    fd = os.open(path, os.O_APPEND | os.O_WRONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'ab', buffering=0) as target:
        st = os.fstat(target.fileno())
        require(stat.S_ISREG(st.st_mode) and st.st_uid == os.getuid() and st.st_nlink == 1
                and stat.S_IMODE(st.st_mode) == 0o600 and st.st_size + len(data) <= 32 * 1024**2,
                'unsafe or full evidence trace')
        require(target.write(data) == len(data), 'short trace write')
        os.fsync(target.fileno())


def validate_binding(proof, device, inode):
    checks = {'disk_device': proof.get('device') == device,
              'disk_inode': proof.get('inode') == inode,
              'descriptor_backed': proof.get('descriptor_backed') is True,
              'qmp_fdsets_verified_at_start': proof.get('qmp_fdsets_verified_at_start') is True}
    require(all(checks.values()), 'disk descriptor binding mismatch: ' + ','.join(k for k, v in checks.items() if not v))


def validate_completion(bound, status, marker):
    checks = {'startup_binding_observed': bound is True,
              'state_stopped': status.get('state') == 'stopped',
              'worker_exit_zero': type(status.get('exit_code')) is int and status['exit_code'] == 0,
              'one_complete_pass_marker': marker.get('pass_count') == 1,
              'no_failure_marker': marker.get('fail_count') == 0}
    require(all(checks.values()), 'guarded maintenance unconfirmed: ' + ','.join(k for k, v in checks.items() if not v))
    return checks


def marker_evidence(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as source:
        st = os.fstat(source.fileno())
        require(stat.S_ISREG(st.st_mode) and st.st_size <= 2 * 1024**2, 'unsafe/oversized Guest log')
        data = source.read(2 * 1024**2 + 1)
    require(len(data) <= 2 * 1024**2, 'Guest log size bound')
    lines = data.replace(b'\r\n', b'\n').split(b'\n')[:-1]
    return {'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest(),
            'pass_count': lines.count(b'VEYNODE_CRASH_MAINTENANCE_PASS'),
            'fail_count': sum(line.startswith(b'VEYNODE_CRASH_MAINTENANCE_FAIL') for line in lines)}


def final_proof(trace, bound, status, marker, elapsed):
    # This evidence survives a raised assertion and any later force_stop status.
    append_trace(trace, {'phase': 'acceptance', 'event': 'completion_assertion_inputs',
                         'bound': bound, 'status': status, 'marker': marker, 'elapsed_seconds': elapsed})
    return validate_completion(bound, status, marker)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for key in ('fixture', 'binary', 'state', 'output'):
        p.add_argument('--' + key, type=Path, required=True)
    a = p.parse_args()
    for key, val in vars(a).items():
        setattr(a, key, val.resolve())
    a.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    result, trace = a.output / 'result.json', a.output / 'observations.jsonl'
    for path in (result, trace):
        os.close(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600))
    report = {'passed': False, 'scope': 'RAM-only updater of independent Guest crash fixture clone',
              'checks': [], 'trace': trace.name, 'limitations': ['controller terminal status is not an independent full process-tree observation']}
    started, vm, phase, status = False, None, 'acceptance', {}
    begin = time.monotonic()

    def save():
        result.write_text(json.dumps(report, indent=2) + '\n')

    def observe(event, **values):
        append_trace(trace, {'phase': phase, 'event': event, 'elapsed_seconds': time.monotonic()-begin, **values})

    def call(method, payload):
        observe('call_begin', method=method)
        r = subprocess.run([str(a.binary), 'call', '--state-dir', str(a.state), method, json.dumps(payload)],
                           capture_output=True, text=True, timeout=42)
        require(len(r.stdout) < 2 * 1024**2 and len(r.stderr) <= 65536, 'local response bound')
        # stderr can contain paths; retain digest/exit, never inject raw text into reports.
        try:
            reply = json.loads(r.stdout)
        except ValueError:
            observe('call_invalid_json', method=method, exit_code=r.returncode,
                    stdout_sha256=hashlib.sha256(r.stdout.encode()).hexdigest())
            raise AssertionError('invalid local JSON: ' + method) from None
        observe('call_reply', method=method, exit_code=r.returncode, reply=reply,
                stderr_sha256=hashlib.sha256(r.stderr.encode()).hexdigest())
        require(r.returncode == 0, 'local operation failed: ' + method)
        return reply

    try:
        prep = json.loads((a.fixture / 'preparation.json').read_text())
        data = (a.fixture / 'maintenance.json').read_bytes()
        require(prep['prepared'] and hashlib.sha256(data).hexdigest() == prep['config_sha256']['maintenance.json'], 'prepared config mismatch')
        cfg = json.loads(data); vm = cfg['id']; disk = Path(cfg['disk'])
        require(disk == a.fixture / 'images/clones/guest-crash/disk.raw' and not cfg.get('qemu_guest_exec', False), 'wrong maintenance disk/transport')
        require(hashlib.sha256(Path(cfg['initrd']).read_bytes()).hexdigest() == prep['initrd_sha256'], 'RAM updater hash mismatch')
        require(vm not in {v['config']['id'] for v in call('list', {})}, 'maintenance ID already registered')
        cfg['serial_log'] = str(a.output / 'guest.serial.log'); ds = disk.stat()
        report['expected_disk'] = {'device': ds.st_dev, 'inode': ds.st_ino}
        call('create', cfg); started = True
        # start itself can already contain the verified runtime. Count only an
        # actual proof with exactly the same strict predicate as status replies.
        bound = False
        def inspect(value):
            nonlocal bound
            proof = value.get('runtime', {}).get('disk_provenance')
            observe('disk_proof_observation', proof=proof, bound_before=bound)
            if proof is not None:
                require(isinstance(proof, dict), 'malformed disk provenance')
                validate_binding(proof, ds.st_dev, ds.st_ino)
                bound = True
                report.setdefault('first_verified_disk_proof', proof)
            report['binding_observed'] = bound
            save()
        status = call('start', {'id': vm})
        report['start_reply'] = status; save(); inspect(status)
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            status = call('status', {'id': vm})
            report['last_acceptance_status'] = status; save(); inspect(status)
            if status['state'] in ('stopped', 'crashed', 'error'):
                break
            time.sleep(.2)
        marker = marker_evidence(Path(cfg['serial_log']))
        report['completion_inputs'] = {'bound': bound, 'status': status, 'marker': marker}
        save()
        checks = final_proof(trace, bound, status, marker, time.monotonic()-begin)
        started = False
        report.update(passed=True, status=status, disk_binding_verified=True, checks=checks)
    except BaseException as e:
        report['error'] = str(e)
        raise
    finally:
        phase = 'failure_cleanup'
        if started:
            try:
                observe('before_cleanup', original_status=status)
                # Do not overwrite a terminal observation by needlessly force-
                # stopping an already exited VM. This cannot repair acceptance.
                cleanup = call('status', {'id': vm})
                if cleanup['state'] not in ('stopped', 'crashed', 'error'):
                    report['failure_force_stop_reply'] = call('force_stop', {'id': vm, 'confirm': vm})
                    deadline = time.monotonic()+15
                    while time.monotonic() < deadline:
                        cleanup = call('status', {'id': vm})
                        if cleanup['state'] in ('stopped', 'crashed', 'error'):
                            break
                        time.sleep(.1)
                report['failure_cleanup_status'] = cleanup
                report['failure_controller_terminal_observed'] = cleanup['state'] in ('stopped', 'crashed', 'error')
            except Exception as e:
                report['failure_controller_terminal_observed'] = False
                report['cleanup_error_type'] = type(e).__name__
        save()


if __name__ == '__main__':
    main()
