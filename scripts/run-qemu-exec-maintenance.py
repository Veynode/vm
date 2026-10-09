#!/usr/bin/env python3
"""Run a prepared disposable-clone updater via an existing guarded controller.

The supplied fixture must be produced by prepare-installed-qemu-exec.py. Success
means the clone updater completed and QEMU exited; it is NOT installed Guest,
command-execution or normal ACPI-shutdown acceptance.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ('fixture', 'binary', 'state', 'output'):
        p.add_argument('--' + name, type=Path, required=True)
    args = p.parse_args()
    for name, value in vars(args).items():
        setattr(args, name, value.resolve())
    args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    report = {'passed': False, 'started_at_unix': time.time(), 'checks': [],
              'scope': 'RAM maintenance of a new independent installed desktop clone only'}
    started = False
    vm = None
    disk_identity = None
    provenance_seen = False

    def require(value, message):
        if not value:
            raise AssertionError(message)

    def call(method, payload):
        result = subprocess.run([str(args.binary), 'call', '--state-dir', str(args.state), method,
            json.dumps(payload)], capture_output=True, text=True, timeout=42)
        require(result.returncode == 0, f'{method}: {result.stderr}')
        return json.loads(result.stdout)

    def record(name, value):
        report['checks'].append({'name': name, 'evidence': value})
        (args.output / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
        print(name, flush=True)

    def terminal(timeout):
        nonlocal provenance_seen
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            status = call('status', {'id': vm})
            provenance = status.get('runtime', {}).get('disk_provenance')
            if not provenance_seen and provenance is not None:
                require((provenance.get('device'), provenance.get('inode')) == disk_identity
                    and provenance.get('descriptor_backed') is True
                    and provenance.get('qmp_fdsets_verified_at_start') is True,
                    'running backend disk binding does not match disposable clone')
                record('actual clone descriptor and QMP FD binding', provenance)
                provenance_seen = True
            if status['state'] in ('stopped', 'crashed', 'error'):
                return status
            time.sleep(.2)
        raise AssertionError('Guest terminal state deadline')

    try:
        require(args.fixture.is_relative_to(ROOT / '.local'), 'repository fixture required')
        prep = json.loads((args.fixture / 'preparation.json').read_text())
        config_path = args.fixture / 'maintenance.json'
        require(prep['prepared'] is True and prep['vm_started'] is False, 'fixture preparation incomplete')
        require(hashlib.sha256(config_path.read_bytes()).hexdigest() == prep['config_sha256']['maintenance.json'],
                'maintenance config differs from prepared manifest')
        config = json.loads(config_path.read_text())
        vm = config['id']
        require(config['backend'] == 'qemu-hvf' and config['guest'] == 'linux'
            and config.get('qemu_guest_exec', False) is False, 'maintenance must not opt into execution')
        require(Path(config['disk']).resolve() == args.fixture / 'images/clones/exec-v2/disk.raw',
                'only prepared disposable clone may be updated')
        disk_stat = Path(config['disk']).stat()
        disk_identity = (disk_stat.st_dev, disk_stat.st_ino)
        require(hashlib.sha256(Path(config['initrd']).read_bytes()).hexdigest() == prep['initrd_sha256'],
                'maintenance payload differs from prepared manifest')
        require('rdinit=/veynode/exec-v2/maintenance.sh veynode.exec_fixture_maintenance=1' in config['command_line'],
                'missing maintenance guard')
        require(vm not in {entry['config']['id'] for entry in call('list', {})}, 'maintenance VM already registered')
        log = Path(config['serial_log'])
        require(not log.exists(), 'maintenance serial log already exists')
        record('prepared config and RAM payload match', {'config': config, 'preparation': prep,
            'controller_sha256': hashlib.sha256(args.binary.read_bytes()).hexdigest()})
        call('create', config)
        started = True
        record('guarded clone start', call('start', {'id': vm}))
        status = terminal(120)
        require(provenance_seen, 'actual runtime disk binding was not observed')
        require(log.stat().st_size <= 8 * 1024 * 1024, 'maintenance log too large')
        data = log.read_bytes()
        (args.output / 'maintenance.serial.log').write_bytes(data)
        lines = data.replace(b'\r\n', b'\n').split(b'\n')[:-1]
        require(status['state'] == 'stopped' and status.get('exit_code') == 0, 'QEMU updater exit unsuccessful')
        require(lines.count(b'VEYNODE_EXEC_MAINTENANCE_PASS') == 1
            and not any(b'VEYNODE_EXEC_MAINTENANCE_FAIL' in line for line in lines), 'updater did not pass')
        started = False
        record('RAM updater reports verified writes unmount and QEMU exit zero', status)
        report['passed'] = True
    except BaseException as error:
        report['error'] = str(error)
        raise
    finally:
        if started:
            try:
                status = call('status', {'id': vm})
                if status['state'] not in ('stopped', 'crashed', 'error'):
                    report['failure_force_stop'] = call('force_stop', {'id': vm, 'confirm': vm})
                    status = terminal(15)
                report['failure_terminal_state'] = status
            except Exception as error:
                report['cleanup_error'] = str(error)
        report['finished_at_unix'] = time.time()
        (args.output / 'result.json').write_text(json.dumps(report, indent=2) + '\n')


if __name__ == '__main__':
    main()
