#!/usr/bin/env python3
"""Boot two APFS clones of a stopped repository-created installed Linux fixture.

Temporary clone disks are disposable. The source is never booted or modified.
This verifies offline image isolation, not live snapshots or memory restore.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import time

from native_client import NativeClient

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--config', type=Path, required=True)
parser.add_argument('--marker-path', default='/home/veynode/veynode-persistence-test')
parser.add_argument('--image-binary', type=Path, default=ROOT / 'target/release/vm-image')
args = parser.parse_args()
image_binary = args.image_binary.resolve()
if not image_binary.is_file():
    raise SystemExit('Build optimized large-image tooling: cargo build --release --locked --bin vm-image')
if not args.config.resolve().is_relative_to(ROOT / '.local/vms'):
    raise SystemExit('Only repository-created disposable fixtures are accepted')
configuration = json.loads(args.config.read_text())
source = Path(configuration['disk']).resolve()
if not source.is_relative_to(ROOT / '.local/vms'):
    raise SystemExit('Source must be a repository-created disposable disk')
evidence = {'started_at_unix': time.time(), 'checks': [],
            'limits': 'Offline APFS clones; no live snapshot, memory restore or shared-block accounting claim'}


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(4 * 1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def record(name, value):
    evidence['checks'].append({'name': name, 'evidence': value})


try:
    original = digest(source)
    with tempfile.TemporaryDirectory(prefix='vm-installed-clones-', dir=ROOT / '.local') as directory:
        temporary = Path(directory)
        store = temporary / 'images'

        def image(*arguments):
            process = subprocess.run([str(image_binary), *arguments,
                                      '--store', str(store)], text=True, capture_output=True, timeout=120)
            assert process.returncode == 0, process.stderr
            return json.loads(process.stdout)

        base = image('import', '--id', 'installed-base', '--source', str(source), '--source-stopped')
        base_path = Path(base['disk_path'])
        assert base_path.stat().st_mode & 0o777 == 0o400
        record('stopped installed filesystem imported to readonly base', base)
        clones = [image('clone', '--base', 'installed-base', '--id', name) for name in ['a', 'b']]
        original_marker = None
        for index, clone in enumerate(clones):
            cfg = dict(configuration, id=f'installed-clone-{index}', disk=clone['disk_path'],
                       window=False, serial_log=str(temporary / f'{index}.serial.log'))
            config_path = temporary / f'{index}.json'
            config_path.write_text(json.dumps(cfg))
            with NativeClient(ROOT / '.local/bin/vm-apple', config_path) as worker:
                info = worker.ready()
                assert info['capabilities']['shutdown'], info
                read = worker.guest('exec', argv=['/bin/cat', args.marker_path], timeout_ms=5000)
                assert read['exit_code'] == 0 and read['stdout'], read
                if index == 0:
                    original_marker = read['stdout']
                    write = worker.guest('exec', argv=['/bin/sh', '-eu', '-c',
                        'printf clone-a-private-write > "$1"; sync', 'clone-test', args.marker_path], timeout_ms=5000)
                    assert write['exit_code'] == 0, write
                else:
                    assert read['stdout'] == original_marker, read
                    record('second actual Guest sees base data, not first clone writes', True)
                shutdown = worker.guest('shutdown')
                assert shutdown['requested'], shutdown
                assert worker.process.wait(timeout=20) == 0
                record(f'clone {index} booted installed Guest and stopped normally', True)
        assert digest(base_path) == original
        assert digest(source) == original
        assert digest(Path(clones[0]['disk_path'])) != original
        record('real Guest filesystem writes preserve complete base and source SHA256', original)
        # Reboot the first clone to verify its private filesystem write persisted.
        cfg = dict(configuration, id='installed-clone-a-reboot', disk=clones[0]['disk_path'],
                   window=False, serial_log=str(temporary / 'reboot.serial.log'))
        path = temporary / 'reboot.json'
        path.write_text(json.dumps(cfg))
        with NativeClient(ROOT / '.local/bin/vm-apple', path) as worker:
            worker.ready()
            read = worker.guest('exec', argv=['/bin/cat', args.marker_path], timeout_ms=5000)
            assert read['stdout'] == 'clone-a-private-write' and read['exit_code'] == 0, read
            assert worker.guest('shutdown')['requested']
            assert worker.process.wait(timeout=20) == 0
            record('clone private data survives clean shutdown and reboot', True)
    evidence['passed'] = True
except Exception as error:
    evidence.update(passed=False, error=str(error))
    raise
finally:
    evidence['finished_at_unix'] = time.time()
    output = ROOT / '.local/evidence/installed-clones.json'
    if not evidence.get('passed'):
        output = output.with_suffix('.failed.json')
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(evidence, indent=2) + '\n')
print('Installed Guest APFS clones: isolation, persistence and normal shutdown passed')
