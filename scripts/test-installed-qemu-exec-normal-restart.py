#!/usr/bin/env python3
"""Actual v7 normal restart, attaching an already-running EMPTY dedicated daemon.

Never launches/restarts a daemon or calls reconcile_execution. The caller supplies
one stopped disposable installed exec-v2 clone and a distinct immutable base.
Example (root/operator runs this; offline tests do not invoke main):
  python3 scripts/test-installed-qemu-exec-normal-restart.py \
    --binary .local/installed-qemu-exec-bin-v7/vm-control \
    --worker .local/installed-qemu-exec-bin-v7/vm-qemu \
    --state .local/control-normal-restart-v1 --config /absolute/new-config.json \
    --base /absolute/readonly-base.raw --output .local/evidence/normal-restart-v1
Only the NEW configured VM is operated. Source/base/config are never written.
Default journal version 2 preserves the original contract. Explicit
--journal-version 3 selects bounded FIFO receipts and requires completed witness
metadata GC. Reused inode numbers after GC are allowed, never reused instances;
the new active file must still match its runtime identity/content exactly.
"""
import argparse
import base64
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import secrets
import stat
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]


def module(name, file):
    spec = importlib.util.spec_from_file_location(name, ROOT / 'scripts' / file)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


base = module('normal_restart_baseline', 'test-installed-qemu-exec.py')
process = module('normal_restart_process', 'test-qemu-exec-guardian.py')
witness = module('normal_restart_witness', 'test-qemu-exec-witness-recovery.py')
require, digest = base.require, base.digest


def journal_matches(value, launches, completed, scopes=(), version=2):
    """Exact identities, not merely empty arrays/counts or worker IDs."""
    witness.validate_journal(json.dumps(value).encode(), version)
    expected = {'version': version, 'launches': launches, 'completed': completed, 'scopes': list(scopes)}
    require(all(sorted(value[k], key=lambda x: json.dumps(x, sort_keys=True)) ==
                sorted(expected[k], key=lambda x: json.dumps(x, sort_keys=True))
                for k in ('launches', 'completed', 'scopes')), 'unexpected execution journal transition')
    if version == 3:
        require(value['completed'] == completed, 'v3 completed receipt order mismatch')


def distinct_boots(before, after):
    require(before['boot']['boot_id'] != after['boot']['boot_id'], 'installed Guest boot identity reused')


def distinct_witnesses(before, after, version=2):
    witness.journal_limit(version)
    witness.identity_valid(before); witness.identity_valid(after)
    require(before['vm'] == after['vm'] and before['worker_instance'] != after['worker_instance']
            and (version == 3 or (before['device'], before['inode']) != (after['device'], after['inode'])),
            'normal restart reused an incarnation or witness inode')


def authority_denied(reply):
    # Only the lease check is evidence here; unknown methods, transport failure,
    # unsupported capabilities and generic failures must NOT satisfy the oracle.
    expected = {'acquire VM control first', 'control lease expired, revoked or belongs to another client'}
    content = reply.get('result', {}).get('content', [])
    require(reply.get('result', {}).get('isError') is True and len(content) == 1
            and content[0].get('type') == 'text' and content[0].get('text') in expected,
            'expected explicit lease denial, not a generic failure')


def normal_exit_proof(status, journal, receipts, pins, observations, version=2):
    failures = [name for name, passed in (
        ('state', status.get('state') == 'stopped'),
        ('exit_code', type(status.get('exit_code')) is int and status['exit_code'] == 0),
        ('execution_reconciliation_status', status.get('execution_reconciliation_status') != 'unconfirmed'),
        ('execution_recovery_required', not status.get('execution_recovery_required', False)),
        ('execution_reconciliation_required', not status.get('execution_reconciliation_required', False))) if not passed]
    require(not failures, 'automatic normal termination unconfirmed: ' + ','.join(failures))
    require(len(pins) == len(observations) == 3
            and all(not process.same_incarnation(p, current) for p, current in zip(pins, observations)),
            'owned VM process incarnation still exists')
    journal_matches(journal, [], receipts, version=version)


def append_shutdown_observation(path, value):
    """Append and fsync before evaluation; failure cleanup never replaces this log."""
    data = (json.dumps(value, sort_keys=True) + '\n').encode()
    require(len(data) <= 256 * 1024, 'shutdown observation size bound')
    fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'ab', buffering=0) as target:
        st = os.fstat(target.fileno())
        require(stat.S_ISREG(st.st_mode) and st.st_uid == os.getuid()
                and stat.S_IMODE(st.st_mode) == 0o600 and st.st_nlink == 1
                and st.st_size + len(data) <= 16 * 1024 * 1024, 'unsafe or oversized shutdown trace')
        require(target.write(data) == len(data), 'short shutdown trace write')
        os.fsync(target.fileno())


def evaluate_shutdown_observation(path, status, journal, receipts, pins, observations, elapsed, version=2):
    # Independent status and journal reads are not an atomic server snapshot.
    # Preserve both exact values and acquisition order rather than conflating them.
    value = copy.deepcopy({'elapsed_seconds': elapsed, 'status': status, 'journal': journal,
                           'process_observations': observations,
                           'read_order': ['status', 'journal', 'processes']})
    append_shutdown_observation(path, value)
    normal_exit_proof(status, journal, receipts, pins, observations, version)
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ('binary', 'worker', 'state', 'config', 'base', 'output'):
        parser.add_argument('--' + key, type=Path, required=True)
    parser.add_argument('--journal-version', type=int, choices=(2, 3), default=2)
    args = parser.parse_args()
    for key, value in vars(args).items():
        if isinstance(value, Path):
            setattr(args, key, value.resolve())
    args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    report = {'passed': False, 'journal_version': args.journal_version, 'scope': 'two actual installed boots; automatic normal-exit witness proof',
              'checks': [], 'local_methods': [], 'limits': [
                  'same live dedicated daemon; no controller restart or power-loss acceptance',
                  'no manual reconciliation, synthetic Guest cleanup or input dispatch',
                  'not performance, application compatibility or multi-VM acceptance']}
    result_path = args.output / 'result.json'
    fd = os.open(result_path, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
    os.close(fd)
    clients, all_pins, receipts, boots = [], [], [], []
    started, vm, daemon, current_pins = False, None, None, []

    def save():
        result_path.write_text(json.dumps(report, indent=2) + '\n')

    def record(name, value):
        report['checks'].append({'name': name, 'evidence': value})
        save()
        print(name, flush=True)

    def call(method, payload, denied=False):
        require(method in ('list', 'create', 'status', 'shutdown', 'force_stop',
                           'acquire_control', 'release_control', 'exec_start'), 'forbidden harness method')
        report['local_methods'].append(method)
        r = subprocess.run([str(args.binary), 'call', '--state-dir', str(args.state), method,
                            json.dumps(payload)], capture_output=True, text=True, timeout=42)
        require(len(r.stdout) <= 2 * 1024**2 and len(r.stderr) <= 65536, 'local response bound')
        if denied:
            require(r.returncode != 0 and 'control lease expired, revoked or belongs to another client' in r.stderr,
                    'old exact owner/epoch did not receive explicit stale-lease denial')
            return {'denied': True, 'reason': 'stale lease'}
        require(r.returncode == 0, 'local call failed: ' + method)
        return json.loads(r.stdout)

    def tool(client, method, **data):
        return client.tool(method, {'id': vm, **data})

    def execute(client):
        value = tool(client, 'exec_start', argv=['/bin/sh', '-eu', '-c',
            'test "$(id -u)" = 1000; test "$(id -G)" = 1000; '
            'test "$HOME" = /home/veynode; test ! -w /sys/fs/cgroup/veynode-exec/cgroup.procs; '
            'printf "UID=%s\\n" "$(id -u)"; cat /proc/self/cgroup; printf "正常重启\\n"'], timeout_ms=3000)['result']
        job = value['job_id']
        require(isinstance(job, str) and re.fullmatch('[a-f0-9]{32}', job), 'actual job ACK missing')
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline:
            terminal = tool(client, 'exec_poll', job_id=job)['result']
            if terminal['state'] == 'finished':
                require('error' not in terminal, 'actual job containment failed')
                done = terminal['result']
                require(done['exit_code'] == 0 and all(done[k] is False for k in
                        ('cancelled', 'timed_out', 'stdout_truncated', 'stderr_truncated'))
                        and re.fullmatch(r'UID=1000\n0::/veynode-exec/job-[^\s/]+\n正常重启\n', done['stdout'])
                        and done['stderr'] == '', 'actual rootless cgroup command failed')
                return job, done
            time.sleep(.05)
        raise AssertionError('actual command completion deadline')

    try:
        cfg = json.loads(args.config.read_text())
        vm = cfg['id']
        require(cfg.get('qemu_guest_exec') is True and cfg.get('backend') == 'qemu-hvf'
                and cfg.get('guest') == 'linux', 'installed exec v2 fixture required')
        require('veynode.guest_crash_once=' not in cfg.get('command_line', ''), 'one-shot crash fixture forbidden')
        require(call('list', {}) == [], 'an empty dedicated daemon is required')
        journal_matches(witness.read_journal(args.state, args.journal_version)[0], [], [], version=args.journal_version)
        disk = Path(cfg['disk']).resolve()
        require(disk.is_relative_to(ROOT / '.local') and args.base.is_relative_to(ROOT / '.local'),
                'repository disposable fixture required')
        ds, bs = disk.stat(), args.base.stat()
        require(stat.S_ISREG(ds.st_mode) and stat.S_ISREG(bs.st_mode)
                and (ds.st_dev, ds.st_ino) != (bs.st_dev, bs.st_ino)
                and stat.S_IMODE(ds.st_mode) == 0o600 and not bs.st_mode & 0o222,
                'independent private writable clone and immutable base required')
        before = digest(args.base)
        cfg.update(window=False, serial_log=str(args.output / 'guest.serial.log'))
        (args.output / 'config.json').write_text(json.dumps(cfg, indent=2) + '\n')
        record('frozen fixture', {'controller_sha256': digest(args.binary), 'worker_sha256': digest(args.worker),
               'input_config_sha256': digest(args.config), 'runtime_config_sha256': digest(args.output / 'config.json'),
               'base_sha256': before, 'disk_device': ds.st_dev, 'disk_inode': ds.st_ino,
               'script_sha256': {p: digest(ROOT / 'scripts' / p) for p in (
                   Path(__file__).name, 'test-installed-qemu-exec.py', 'test-installed-qemu-desktop.py',
                   'test-qemu-exec-guardian.py', 'test-qemu-exec-witness-recovery.py')}})
        call('create', cfg)
        clients.append(base.MCP(args.binary, args.state, False))
        clients.append(base.MCP(args.binary, args.state, True))
        life, old = clients
        original_job, old_authority = None, None
        for number in (1, 2):
            log = Path(cfg['serial_log'])
            require(not log.exists(), 'fresh serial file required per boot')
            current_pins = []
            started = True
            record(f'boot {number}: start', tool(life, 'start'))
            deadline, boot = time.monotonic() + 180, None
            while time.monotonic() < deadline:
                status = call('status', {'id': vm})
                require(status['state'] in ('starting', 'running'), 'Guest exited before graphics readiness')
                if log.exists():
                    require(log.stat().st_size <= base.desktop.MAX_LOG, 'serial log bound')
                    boot = base.desktop.boot_evidence(log.read_bytes())
                    if boot:
                        break
                time.sleep(.25)
            require(boot is not None, 'installed desktop readiness deadline')
            if boots:
                distinct_boots(boots[0], boot)
            boots.append(boot)
            record(f'boot {number}: actual ext4 and fifteen pixel assertions', boot)
            runtime = status['runtime']; dp = runtime['disk_provenance']; guardian = runtime['execution_guardian']
            require(dp.get('descriptor_backed') is True and dp.get('qmp_fdsets_verified_at_start') is True
                    and dp.get('device') == ds.st_dev and dp.get('inode') == ds.st_ino, 'disk inode binding failed')
            require(guardian.get('owns_qemu_child') is True and guardian.get('heartbeat_timeout_ms') == 5000
                    and guardian['backend_pid'] == runtime['backend_pid'], 'mandatory guardian missing')
            pins = [process.snapshot(p) for p in (status['pid'], guardian['guardian_pid'], guardian['backend_pid'])]
            require(all(pins) and all(p['uid'] == os.getuid() and p['state'] not in (4, 5) for p in pins)
                    and pins[0]['executable'] == str(args.worker) and pins[1]['executable'] == str(args.worker)
                    and pins[1]['ppid'] == pins[0]['pid'] and pins[2]['ppid'] == pins[1]['pid'], 'worker guardian QEMU chain mismatch')
            current_pins = pins
            all_pins.extend(pins)
            actual_daemon = process.snapshot(pins[0]['ppid'])
            require(actual_daemon is not None and actual_daemon['executable'] == str(args.binary)
                    and actual_daemon['uid'] == os.getuid()
                    and Path(process.option(process.command_arguments(actual_daemon['pid']), '--state-dir')).resolve() == args.state,
                    'running controller executable/state differs from supplied frozen controller')
            if daemon is not None:
                require(process.same_incarnation(daemon, actual_daemon), 'daemon restarted between normal boots')
            daemon = actual_daemon
            instance = process.option(process.command_arguments(pins[0]['pid']), '--guest-worker-instance')
            identity = witness.witness_from_runtime(runtime, vm, instance, args.state)
            if receipts:
                distinct_witnesses(receipts[0], identity, args.journal_version)
                if args.journal_version == 3:
                    witness.require_completed_gc(args.state, receipts[0])
            journal_matches(witness.read_journal(args.state, args.journal_version)[0], [identity], receipts, version=args.journal_version)
            record(f'boot {number}: exact runtime witness and process chain', {'witness': identity, 'processes': pins, 'daemon': daemon})
            info, attempts = base.wait_guest_info(old, vm, lambda: call('status', {'id': vm}), status['pid'])
            require(info.get('transport') == 'virtio-serial-private-exec-v2'
                    and info.get('transport_reconnect') is False
                    and info.get('execution_containment') == 'cgroup-v2-clone3-kill'
                    and all(info['capabilities'].get(k) is True for k in ('exec', 'async_exec', 'descendant_containment')),
                    'actual Guest execution integration missing')
            record(f'boot {number}: actual Guest info', {'info': info, 'temporary_refusals': len(attempts)})
            if number == 2:
                authority_denied(old.tool('exec_start', {'id': vm, 'argv': ['/bin/true']}, denied=True))
                for method in ('exec_poll', 'exec_cancel'):
                    reply = old.tool(method, {'id': vm, 'job_id': original_job}, denied=True)
                    require('job is unknown, expired or not owned by this client and lease epoch' in json.dumps(reply),
                            'wrong old job denial')
                call('exec_start', {'id': vm, **old_authority, 'argv': ['/bin/true'], 'timeout_ms': 1000}, denied=True)
                record('boot 2: old MCP job and exact old owner/epoch rejected before fresh acquire', True)
                clients.append(base.MCP(args.binary, args.state, True))
                owner = clients[-1]
            else:
                owner = old
            lease = tool(owner, 'acquire_control')
            job, done = execute(owner)
            journal, raw = witness.read_journal(args.state, args.journal_version)
            require(len(journal['scopes']) == 1 and journal['scopes'][0]['vm'] == vm
                    and journal['scopes'][0]['worker_instance'] == instance
                    and journal['scopes'][0]['epoch'] == lease['epoch'], 'command scope not bound to current incarnation/lease')
            journal_matches(journal, [identity], receipts, journal['scopes'], version=args.journal_version)
            require(job.encode() not in raw and done['stdout'].encode() not in raw, 'journal leaked job/output')
            record(f'boot {number}: fresh MCP rootless command and exact scope', {'result': done, 'journal': journal,
                   'job_id_sha256': hashlib.sha256(job.encode()).hexdigest()})
            if number == 1:
                original_job = job
            content = owner.tool('screenshot', {'id': vm}, raw=True)['result']['content']
            images = [v for v in content if v.get('type') == 'image']
            require(len(images) == 1 and images[0].get('mimeType') == 'image/png', 'actual PNG unavailable')
            png = args.output / f'boot{number}.png'
            png.write_bytes(base64.b64decode(images[0]['data'], validate=True))
            decoded = json.loads(subprocess.check_output(['ffprobe', '-v', 'error', '-show_entries',
                'stream=codec_name,width,height', '-of', 'json', str(png)], text=True, timeout=15))
            require(decoded['streams'] == [{'codec_name': 'png', 'width': cfg['width'], 'height': cfg['height']}], 'PNG decode/geometry mismatch')
            record(f'boot {number}: actual capture independently decoded', {'sha256': digest(png), 'decode': decoded})
            require(tool(owner, 'release_control').get('released') is True, 'release not confirmed')
            journal_matches(witness.read_journal(args.state, args.journal_version)[0], [identity], receipts, version=args.journal_version)
            if number == 1:
                # Preserve an exact old tuple independent of the MCP adapter's
                # cache clearing. This authority is released without any command.
                token = secrets.token_hex(32)
                prior = call('acquire_control', {'id': vm, 'owner': token})
                old_authority = {'owner': token, 'epoch': prior['epoch']}
                require(call('release_control', {'id': vm, **old_authority}).get('released') is True, 'diagnostic old tuple release failed')
            shutdown_trace = args.output / f'boot{number}.shutdown.jsonl'
            fd = os.open(shutdown_trace, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            os.close(fd)
            shutdown_begin = time.monotonic()
            shutdown = call('shutdown', {'id': vm})
            append_shutdown_observation(shutdown_trace, {'event': 'shutdown_response',
                'elapsed_seconds': time.monotonic() - shutdown_begin, 'response': shutdown})
            record(f'boot {number}: normal shutdown response (not completion proof)', shutdown)
            require(shutdown.get('requested') is True and shutdown.get('mechanism') == 'qmp-system-powerdown'
                    and shutdown.get('execution_quiescing') is True and shutdown.get('shutdown_timeout_seconds') == 60,
                    'bounded normal ACPI shutdown missing')
            deadline = time.monotonic() + 65
            while time.monotonic() < deadline:
                status = call('status', {'id': vm})
                # Save status first even if the subsequent private-file/process
                # observation fails. Never query again merely to reconstruct it.
                append_shutdown_observation(shutdown_trace, {'event': 'status_response',
                    'elapsed_seconds': time.monotonic() - shutdown_begin, 'status': status})
                report[f'boot{number}_last_shutdown_status'] = copy.deepcopy(status)
                save()
                journal, _ = witness.read_journal(args.state, args.journal_version)
                append_shutdown_observation(shutdown_trace, {'event': 'journal_observation',
                    'elapsed_seconds': time.monotonic() - shutdown_begin, 'journal': journal})
                observations = [process.snapshot(p['pid']) for p in pins]
                append_shutdown_observation(shutdown_trace, {
                    'elapsed_seconds': time.monotonic() - shutdown_begin,
                    'status': status, 'journal': journal, 'process_observations': observations,
                    'read_order': ['status', 'journal', 'processes']})
                report[f'boot{number}_last_shutdown_observation'] = {
                    'status': status, 'journal': journal, 'process_observations': observations,
                    'trace': shutdown_trace.name, 'elapsed_seconds': time.monotonic() - shutdown_begin}
                save()
                if status['state'] in ('stopped', 'crashed', 'error', 'recovery_required'):
                    break
                time.sleep(.2)
            require(b'VEYNODE_INSTALLED_ACPI_POWER_BUTTON' in log.read_bytes(), 'real ACPI handler not observed')
            # Keep the final decision sample too, so a raised assertion retains
            # its exact inputs independently of result.json/failure cleanup.
            evaluate_shutdown_observation(shutdown_trace, status, journal, receipts + [identity],
                                          pins, observations, time.monotonic() - shutdown_begin, args.journal_version)
            if args.journal_version == 3:
                witness.require_completed_gc(args.state, identity)
                record(f'boot {number}: completed witness metadata actually collected', identity)
            receipts.append(identity)
            started = False
            log.rename(args.output / f'boot{number}.serial.log')
            record(f'boot {number}: normal exit0, process retirement and automatic exact completion',
                   {'state': status['state'], 'exit_code': status['exit_code'], 'journal': journal,
                    'original_process_observations': observations, 'guest_scope_ack_inferred_from_exit': False})
        for client in clients:
            client.close()
        clients.clear()
        record('all MCP EOF cleanup exit zero', True)
        require(all(not process.same_incarnation(p, process.snapshot(p['pid'])) for p in all_pins), 'owned process revived')
        journal_matches(witness.read_journal(args.state, args.journal_version)[0], [], receipts, version=args.journal_version)
        require(digest(args.base) == before, 'immutable base changed')
        record('both original process chains gone; exactly two completed receipts; full base hash unchanged', before)
        report['passed'] = True
    except BaseException as error:
        report['error'] = re.sub(r'[0-9a-f]{32,64}', '<redacted>', str(error))
        raise
    finally:
        if started:
            try:
                call('force_stop', {'id': vm, 'confirm': vm})
                deadline = time.monotonic() + 15
                while time.monotonic() < deadline:
                    status = call('status', {'id': vm})
                    if status['state'] in ('stopped', 'crashed', 'error', 'recovery_required'):
                        break
                    time.sleep(.1)
                report['failure_terminal_state'] = status['state']
                report['failure_cleanup_confirmed'] = len(current_pins) == 3 and status['state'] in (
                    'stopped', 'crashed', 'error', 'recovery_required') and all(
                    not process.same_incarnation(p, process.snapshot(p['pid'])) for p in all_pins)
            except Exception:
                report['failure_cleanup_confirmed'] = False
        for client in clients:
            try:
                client.close()
                report.setdefault('client_cleanup', []).append({'exit_code': client.p.returncode, 'confirmed': True})
            except Exception:
                report.setdefault('client_cleanup', []).append({'exit_code': client.p.returncode, 'confirmed': False})
        report['finished_at_unix'] = time.time()
        save()


if __name__ == '__main__':
    main()
