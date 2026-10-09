#!/usr/bin/env python3
"""Create a private APFS seed clone; never boot or write the source disk.

The vm-image tool holds the existing cross-backend disk locks while importing.
Provisioning and normal boots must subsequently use the locked QEMU worker.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--directory', type=Path, required=True)
    parser.add_argument('--source', type=Path, default=ROOT / '.local/vms/linux-cgroup-fixed/disk.raw')
    parser.add_argument('--image-binary', type=Path, default=ROOT / 'target/release/vm-image')
    args = parser.parse_args()
    source = args.source.resolve(strict=True)
    directory = args.directory.resolve()
    if not source.is_relative_to(ROOT / '.local/vms'):
        parser.error('only repository-created installed source images are permitted')
    if not directory.is_relative_to(ROOT / '.local'):
        parser.error('fixture must be under repository .local')
    directory.mkdir(mode=0o700, parents=True, exist_ok=False)
    report = {'version': 1, 'source': str(source), 'vm_started': False,
              'source_write_mounted': False, 'passed': False}
    try:
        report['source_sha256_before'] = digest(source)
        def image(*arguments):
            completed = subprocess.run([str(args.image_binary.resolve()), *arguments,
                                        '--store', str(directory / 'images'), '--min-free-mib', '2048'],
                                       check=True, capture_output=True, text=True, timeout=180)
            return json.loads(completed.stdout)
        base = image('import', '--id', 'installed-base', '--source', str(source), '--source-stopped')
        seed = image('clone', '--base', 'installed-base', '--id', 'desktop-seed')
        report.update(base=base, seed=seed)
        base_path, seed_path = Path(base['disk_path']), Path(seed['disk_path'])
        report['source_sha256_after'] = digest(source)
        report['base_sha256'] = digest(base_path)
        report['seed_initial_sha256'] = digest(seed_path)
        if len({report[key] for key in ['source_sha256_before', 'source_sha256_after',
                                       'base_sha256', 'seed_initial_sha256']}) != 1:
            raise RuntimeError('source/base/seed integrity mismatch')
        if base_path.stat().st_mode & 0o777 != 0o400 or seed_path.stat().st_mode & 0o777 != 0o600:
            raise RuntimeError('incorrect disk permissions')
        if len({(p.stat().st_dev, p.stat().st_ino) for p in [source, base_path, seed_path]}) != 3:
            raise RuntimeError('source/base/seed inode alias')
        report['passed'] = True
    finally:
        report['preparer_sha256'] = digest(Path(__file__))
        fd = os.open(directory / 'clone-preparation.json', os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, 'w') as output:
            json.dump(report, output, indent=2)
            output.write('\n')
    print(directory / 'clone-preparation.json')


if __name__ == '__main__':
    main()
