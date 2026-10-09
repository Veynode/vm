#!/usr/bin/env python3
"""Actual installed Guest lost-start-ACK acceptance; never launches a daemon.

1. --prepare creates a fresh private output directory, new VM ID and shim config.
2. Operator starts a dedicated frozen controller with the printed environment
   and worker override, retaining the normal registered QEMU/SPICE runtime.
3. Repeat the command without --prepare to boot ONLY that disposable clone.

The supplied --config must name a maintained execution-v2 disposable disk, not
the original desktop base. No Guest/worker binary or production protocol edits.

Example (substitute the three explicit fixture paths):
  python3 scripts/test-installed-qemu-exec-lost-ack.py \\
    --binary .local/installed-qemu-exec-bin-v3/vm-control \\
    --worker .local/installed-qemu-exec-bin-v3/vm-qemu \\
    --state .local/control-exec-lost-ack-v1 --config /absolute/clone-config.json \\
    --base /absolute/readonly-base.raw --output .local/evidence/exec-lost-ack-v1 --prepare
After operator daemon launch, repeat without --prepare. This file does not
construct a daemon command because the operator must select its frozen native
helper and trusted runtime registration; the worker override is printed.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shlex
import stat
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / 'scripts' / path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


base = module('lost_ack_baseline', 'test-installed-qemu-exec.py')
process = module('lost_ack_process', 'test-qemu-exec-guardian.py')
require, digest = base.require, base.digest


def private_write(path, value):
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600), 'w') as f:
        json.dump(value, f, indent=2)
        f.write('\n')


def traces(path):
    require(path.stat().st_size <= 32768, 'trace bound exceeded')
    data = path.read_bytes()
    return [json.loads(line) for line in data.split(b'\n')[:-1]]


def one(events, kind):
    selected = [e for e in events if e['event'] == kind]
    require(len(selected) == 1, 'missing or duplicated ' + kind)
    return selected[0]


# A session-detached grandchild stays in its atomic clone3 cgroup. It writes
# actual readiness and bounded ticks independently of the shell/group parent.
# Parent remains alive past the marker deadline; ordinary completion and the
# 30s command timeout therefore cannot explain absence in the 22..27s window.
CHILD = r'''test "$(id -u)" = 1000 || exit 61
read start _ < /proc/uptime
printf '%s\n' "$start" > "$1.ready"
cat /proc/self/cgroup > "$1.cgroup"
read pid comm state ppid pgrp sid rest < /proc/self/stat
printf '%s %s %s\n' "$pid" "$pgrp" "$sid" > "$1.proc"
(sleep 20; printf escaped > "$1.marker") </dev/null >/dev/null 2>&1 &
i=0
while test "$i" -lt 125; do printf x >> "$1.ticks"; sleep .2; i=$((i+1)); done
wait'''
OUTER = ('read pid comm state ppid pgrp sid rest < /proc/self/stat; printf "%s\\n" "$sid" > "$1.outer-sid"; '
         'setsid /bin/sh -c \'("$@") </dev/null >/dev/null 2>&1 & exit 0\' detached /bin/sh -c "$2" child "$1" & sleep 28 & wait')
READ = r'''test -s "$1.ready" && test -s "$1.ticks" || exit 62
cat "$1.ready"
read now _ < /proc/uptime
printf '%s\n' "$now"
wc -c < "$1.ticks"
if test -e "$1.marker"; then printf 'present\n'; else printf 'absent\n'; fi
cat "$1.cgroup"
cat "$1.proc"
cat "$1.outer-sid"'''


def parse_observation(stdout):
    fields = stdout.splitlines()
    require(len(fields) == 7 and fields[3] == 'absent'
            and re.fullmatch(r'0::/veynode-exec/job-[^\s/]+', fields[4]), 'invalid actual descendant oracle')
    start, now, ticks = float(fields[0]), float(fields[1]), int(fields[2])
    pid, pgrp, sid = map(int, fields[5].split())
    require(pid > 1 and sid > 1 and pid != sid and pgrp == sid and sid != int(fields[6]),
            'actual double-fork/session separation not established')
    require(ticks >= 2 and 0 <= now-start < 27, 'oracle must precede command completion/timeout')
    return {'guest_elapsed_seconds': now-start, 'tick_bytes': ticks, 'marker_absent': True,
            'session_detached_grandchild': True}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for key in ('binary', 'worker', 'state', 'config', 'base', 'output'):
        p.add_argument('--' + key, type=Path, required=True)
    p.add_argument('--prepare', action='store_true')
    a = p.parse_args()
    for key, value in vars(a).items():
        if isinstance(value, Path):
            setattr(a, key, value.resolve())
    if a.prepare:
        require(not a.state.exists(), 'prepare requires a NEW independent daemon state')
        cfg = json.loads(a.config.read_text())
        require(cfg.get('qemu_guest_exec') is True and cfg.get('guest') == 'linux'
                and cfg.get('backend') == 'qemu-hvf', 'installed execution-v2 config required')
        a.output.mkdir(mode=0o700, parents=True, exist_ok=False)
        token = uuid.uuid4().hex
        cfg.update(id='lost-ack-' + token[:16], window=False, serial_log=str(a.output / 'guest.serial.log'))
        private_write(a.output / 'config.json', cfg)
        private_write(a.output / 'shim.json', {'worker': str(a.worker), 'worker_sha256': digest(a.worker),
                      'trace': str(a.output / 'shim.jsonl'), 'tag': 'veynode-lost-ack-' + token})
        private_write(a.output / 'prepared.json', {'controller_sha256': digest(a.binary),
            'config_sha256': digest(a.output / 'config.json'), 'state': str(a.state), 'base': str(a.base),
            'shim_sha256': digest(ROOT / 'scripts/qemu-exec-lost-ack-worker.py')})
        print('Start the dedicated daemon with its normal runtime registration plus:')
        print('VEYNODE_LOST_ACK_CONFIG=' + shlex.quote(str(a.output / 'shim.json')))
        print('--state-dir ' + shlex.quote(str(a.state)))
        print('--qemu-worker ' + shlex.quote(str(ROOT / 'scripts/qemu-exec-lost-ack-worker.py')))
        print('Then repeat this acceptance command without --prepare. No daemon/VM was started.')
        return

    prepared = json.loads((a.output / 'prepared.json').read_text())
    shim = json.loads((a.output / 'shim.json').read_text())
    require(prepared == {'controller_sha256': digest(a.binary), 'config_sha256': digest(a.output / 'config.json'),
            'state': str(a.state), 'base': str(a.base),
            'shim_sha256': digest(ROOT / 'scripts/qemu-exec-lost-ack-worker.py')}, 'prepared artifact changed')
    require(shim['worker'] == str(a.worker) and shim['worker_sha256'] == digest(a.worker), 'worker changed')
    config = json.loads((a.output / 'config.json').read_text())
    vm, disk = config['id'], Path(config['disk']).resolve()
    require(disk.is_relative_to(ROOT / '.local') and a.base.is_relative_to(ROOT / '.local'), 'fixture paths required')
    ds, bs = disk.stat(), a.base.stat()
    require(stat.S_ISREG(ds.st_mode) and stat.S_ISREG(bs.st_mode) and not bs.st_mode & 0o222
            and (ds.st_dev, ds.st_ino) != (bs.st_dev, bs.st_ino), 'separate writable clone/read-only base required')
    report = {'passed': False, 'checks': [], 'scope': 'actual Guest start ACK lost between real worker and daemon',
        'limits': ['not serial ACK loss, daemon crash or performance acceptance'],
        'artifacts': prepared, 'worker_sha256': digest(a.worker), 'script_sha256': {
            name: digest(ROOT / 'scripts' / name) for name in
            ('test-installed-qemu-exec-lost-ack.py', 'test-installed-qemu-exec.py',
             'test-installed-qemu-desktop.py', 'test-qemu-exec-guardian.py')}}
    private_write(a.output / 'result.json', report)
    clients, started = [], False
    log, trace = Path(config['serial_log']), Path(shim['trace'])

    def save():
        (a.output / 'result.json').write_text(json.dumps(report, indent=2) + '\n')

    def record(name, value):
        report['checks'].append({'name': name, 'evidence': value})
        save()
        print(name, flush=True)

    def local(method, value):
        r = subprocess.run([str(a.binary), 'call', '--state-dir', str(a.state), method, json.dumps(value)],
                           capture_output=True, text=True, timeout=42)
        require(r.returncode == 0 and len(r.stdout) <= 2 * 1024 * 1024, 'local operation failed: ' + method)
        return json.loads(r.stdout)

    def tool(client, method, **values):
        return client.tool(method, {'id': vm, **values})

    def execute(client, argv):
        job = tool(client, 'exec_start', argv=argv, timeout_ms=2000)['result']['job_id']
        until = time.monotonic() + 4
        while time.monotonic() < until:
            result = tool(client, 'exec_poll', job_id=job)['result']
            if result['state'] == 'finished':
                require('error' not in result, 'observer containment error')
                result = result['result']
                require(result['exit_code'] == 0 and not any(result[k] for k in
                    ('timed_out', 'cancelled', 'stdout_truncated', 'stderr_truncated')), 'observer command failed')
                return result['stdout']
            time.sleep(.03)
        raise AssertionError('observer command deadline')

    try:
        require(local('list', {}) == [], 'dedicated daemon state must contain no VMs')
        base_before = digest(a.base)
        local('create', config)
        for flag in (False, True, True):
            clients.append(base.MCP(a.binary, a.state, flag))
        life, owner, observer = clients
        started = True
        tool(life, 'start')
        deadline, boot = time.monotonic() + 180, None
        while time.monotonic() < deadline:
            status = local('status', {'id': vm})
            require(status['state'] in ('starting', 'running'), 'Guest stopped before readiness')
            if log.exists():
                require(log.stat().st_size <= base.desktop.MAX_LOG, 'serial log bound')
                boot = base.desktop.boot_evidence(log.read_bytes())
                if boot:
                    break
            time.sleep(.3)
        require(boot is not None, 'installed graphics readiness deadline')
        record('actual ext4 installed boot and 15 pixel checks', boot)
        runtime = status['runtime']
        dp = runtime['disk_provenance']
        require(dp['device'] == ds.st_dev and dp['inode'] == ds.st_ino
                and dp['descriptor_backed'] is True and dp['qmp_fdsets_verified_at_start'] is True, 'disk identity')
        spawn = one(traces(trace), 'worker_spawned')
        g = runtime['execution_guardian']
        require(g['owns_qemu_child'] is True and g['heartbeat_timeout_ms'] == 5000
                and g['backend_pid'] == runtime['backend_pid'], 'actual guardian declaration')
        pins = [process.snapshot(pid) for pid in (status['pid'], spawn['worker_pid'], g['guardian_pid'], g['backend_pid'])]
        require(all(pins) and pins[0]['pid'] == spawn['shim_pid'], 'process identities missing')
        require(all(pins[i]['ppid'] == pins[i-1]['pid'] for i in (1, 2, 3)), 'shim/worker/guardian/QEMU ancestry')
        require(pins[1]['executable'] == str(a.worker) and pins[2]['executable'] == str(a.worker), 'real frozen worker/guardian identity')
        actual_instance = process.option(process.command_arguments(pins[1]['pid']), '--guest-worker-instance')
        require(hashlib.sha256(actual_instance.encode()).hexdigest() == spawn['worker_instance_sha256'], 'original worker instance changed')
        record('actual descriptor binding and unchanged guardian chain', {'disk': dp, 'processes': pins})
        info, _ = base.wait_guest_info(owner, vm, lambda: local('status', {'id': vm}), status['pid'])
        require(info['transport'] == 'virtio-serial-private-exec-v2'
                and info['execution_containment'] == 'cgroup-v2-clone3-kill', 'real v2 containment required')
        record('actual serial execution v2 readiness', info)
        tool(owner, 'acquire_control')
        path = '/home/veynode/' + shim['tag']
        begin = time.monotonic()
        denied = owner.tool('exec_start', {'id': vm, 'argv': ['/bin/sh', '-c', OUTER, shim['tag'], path, CHILD],
                            'timeout_ms': 30000}, denied=True)
        text = json.dumps(denied)
        require('backend response unavailable' in text and 'job_id' not in text, 'expected only missing ACK error, no job ID')
        events = traces(trace)
        one(events, 'target_start')
        one(events, 'successful_ack_dropped')
        record('one real successful ACK dropped; owner received no job ID', {'elapsed_seconds': time.monotonic()-begin,
                'drop_count': 1, 'original_owner_start_count': 1})
        tool(owner, 'release_control')
        require(time.monotonic() - begin < 17, 'revocation too late for independent marker oracle')
        events = traces(trace)
        revoke = one(events, 'original_scope_revoke')
        clean = one(events, 'original_scope_empty_ack')
        require(revoke['outer_id'] == clean['outer_id'], 'original revoke receipt mismatch')
        record('original scope revoked with real empty-cgroup ACK without job ID', clean)
        tool(observer, 'acquire_control')
        observations = []
        while time.monotonic() - begin < 28:
            observed = parse_observation(execute(observer, ['/bin/sh', '-eu', '-c', READ, 'observe', path]))
            observations.append(observed)
            require(all(v['tick_bytes'] == observed['tick_bytes'] for v in observations), 'descendant continued writing after scope revoke')
            if observed['guest_elapsed_seconds'] >= 22 and len(observations) >= 2:
                break
            time.sleep(.5)
        require(len(observations) >= 2 and 22 <= observations[-1]['guest_elapsed_seconds'] < 27, 'missed actual delayed-marker window')
        record('actual detached descendant wrote then stopped before its own timeout', observations)
        for pin in pins:
            process.exact_live(pin)
        require(local('status', {'id': vm})['pid'] == pins[0]['pid'], 'incarnation changed')
        record('same worker guardian and QEMU survived scope revocation', True)
        tool(observer, 'release_control')
        for client in clients:
            client.close()
        clients.clear()
        shutdown = local('shutdown', {'id': vm})
        require(shutdown.get('execution_quiescing') is True and shutdown.get('mechanism') == 'qmp-system-powerdown', 'normal shutdown required')
        deadline = time.monotonic() + 65
        while time.monotonic() < deadline:
            status = local('status', {'id': vm})
            if status['state'] in ('stopped', 'crashed', 'error'):
                break
            time.sleep(.2)
        require(status['state'] == 'stopped' and status.get('exit_code') == 0
                and b'VEYNODE_INSTALLED_ACPI_POWER_BUTTON' in log.read_bytes(), 'normal shutdown unconfirmed')
        require(one(traces(trace), 'worker_reaped')['exit_code'] == 0, 'real worker not reaped normally')
        for pin in pins:
            require(not process.same_incarnation(pin, process.snapshot(pin['pid'])), 'owned process remains after normal shutdown')
        started = False
        record('actual ACPI shutdown and wrapper/worker exit zero', status)
        require(digest(a.base) == base_before, 'immutable base changed')
        record('immutable base full hash unchanged', base_before)
        report['passed'] = True
    except BaseException as error:
        report['error'] = re.sub(r'[0-9a-f]{32,64}', '<redacted-token>', str(error))
        raise
    finally:
        if started:
            try:
                local('force_stop', {'id': vm, 'confirm': vm})
                deadline = time.monotonic() + 15
                while time.monotonic() < deadline:
                    status = local('status', {'id': vm})
                    if status['state'] in ('stopped', 'crashed', 'error'):
                        break
                    time.sleep(.1)
                report['failure_cleanup_confirmed'] = status['state'] in ('stopped', 'crashed', 'error')
            except Exception:
                report['failure_cleanup_confirmed'] = False
        for client in clients:
            try:
                client.close()
            except Exception:
                report['client_cleanup_unconfirmed'] = True
        save()


if __name__ == '__main__':
    main()
