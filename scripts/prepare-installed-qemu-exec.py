#!/usr/bin/env python3
"""Prepare a NEW installed desktop clone for private serial exec v2. Never boots.

Uses vm-image's locked APFS import/clone, then emits a guarded RAM-maintenance
config. Only that disposable clone may be updated. Original disks stay read-only.
"""
import argparse
import gzip
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import stat
import subprocess

ROOT = Path(__file__).resolve().parents[1]
SEED_DIRECTORY = ROOT / '.local/installed-qemu-desktop-v6'
BASE = SEED_DIRECTORY / 'images/bases/desktop-base/disk.raw'
BASE_SHA = 'cce06d89365ce27a18c3cbf7880956763d16342abd5f48eb929052fcaac43e04'
RAM_SHA = '207ced607ed1bf3b9972990f37ec08b4412af5ede3d4a47c264ada9aaadb8538'
KERNEL_SHA = 'e698a107e4d04117db1a7b0daee99bdae5f0647fba2af50f3cd020a666950ab9'
INITRD_SHA = '0deca858e325949e408521509ca1cdf36ae5a668eefda6f02bf03c918377f2d2'
OLD_AGENT_SHA = '8410f79979b9433b81a454a045abaec2757869986cbd2c9347cc1ff9923ff579'
PRESERVED = {
    'opt/veynode-fixture/desktop-boot.sh': 'cbeb6cfe5534ba80637a948f6bc5ade12c8de46b5aa644bb6181fe6adf818efe',
    'opt/veynode-fixture/boot-marker.py': '8eaa18b0019b0bd1c0cdfb0689c41d8fe5f015e1472be0ef6eb539babfff0ddb',
    'opt/veynode-fixture/modules-fixed/libglamoregl.so': 'ba64c24cb70c953d26ac032633b6bca98577ea415f7abdc32f7f451d9a115f67',
    'etc/acpi/handler.sh': '12f7ee5f46334cacae29b68df4e0eeb86d67a9716bc3a9278805f8e26c155039',
}


def read_regular(path, maximum, expected=None):
    """Bound reads before allocation, reject links/FIFOs and changing inputs."""
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as stream:
        before = os.fstat(stream.fileno())
        if not stat.S_ISREG(before.st_mode) or before.st_size > maximum:
            raise ValueError('input must be a bounded regular file')
        data = stream.read(maximum + 1)
        after = os.fstat(stream.fileno())
    if len(data) > maximum or (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) != (
            after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns):
        raise ValueError('input changed or exceeded bound')
    if expected is not None and hashlib.sha256(data).hexdigest() != expected:
        raise ValueError('input hash mismatch')
    return data


def disk_digest(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as stream:
        before = os.fstat(stream.fileno())
        if not stat.S_ISREG(before.st_mode) or before.st_size != 16 * 1024**3:
            raise ValueError('expected 16GiB regular desktop disk')
        hasher = hashlib.sha256()
        remaining = before.st_size
        while remaining:
            chunk = stream.read(min(8 * 1024**2, remaining))
            if not chunk:
                raise ValueError('disk shortened')
            remaining -= len(chunk)
            hasher.update(chunk)
        after = os.fstat(stream.fileno())
        if stream.read(1) or (before.st_ino, before.st_size, before.st_mtime_ns) != (
                after.st_ino, after.st_size, after.st_mtime_ns):
            raise ValueError('disk changed during hash')
    return hasher.hexdigest()


def validate_agent(data, expected):
    if not re.fullmatch('[0-9a-f]{64}', expected) or hashlib.sha256(data).hexdigest() != expected:
        raise ValueError('explicit frozen agent SHA256 mismatch')
    if len(data) < 64 or data[:6] != b'\x7fELF\x02\x01' or int.from_bytes(data[18:20], 'little') != 183:
        raise ValueError('expected Linux little-endian ARM64 ELF')


def checksums(entries):
    return ''.join(f'{sha}  {name}\n' for name, sha in entries.items()).encode()


def configs(directory, disk, kernel, stock_initrd, maintenance_initrd):
    common = dict(backend='qemu-hvf', guest='linux', cpus=2, memory_mib=1536,
                  kernel=str(kernel), disk=str(disk))
    maintenance = dict(common, id='installed-exec-v2-maintenance', window=False,
                       width=1280, height=800,
                       initrd=str(maintenance_initrd),
                       command_line='console=ttyAMA0 rdinit=/veynode/exec-v2/maintenance.sh veynode.exec_fixture_maintenance=1',
                       serial_log=str(directory / 'maintenance.serial.log'))
    installed = dict(common, id='installed-exec-v2', window=True, width=2560,
                     height=1440, qemu_guest_exec=True, initrd=str(stock_initrd),
                     command_line='console=ttyAMA0 root=/dev/vda rootfstype=ext4 modules=ext4,virtio_blk',
                     serial_log=str(directory / 'installed.serial.log'))
    return maintenance, installed


def write_json(path, value):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, 'w') as output:
        json.dump(value, output, indent=2)
        output.write('\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--directory', type=Path, required=True)
    parser.add_argument('--agent', type=Path, required=True)
    parser.add_argument('--agent-sha256', required=True)
    parser.add_argument('--image-binary', type=Path, default=ROOT / 'target/release/vm-image')
    args = parser.parse_args()
    directory = args.directory.absolute()
    if not directory.is_relative_to(ROOT / '.local') or '..' in directory.parts:
        parser.error('new isolated directory must be under repository .local')
    for parent in [directory, *directory.parents]:
        if parent.is_symlink():
            parser.error('fixture directory may not traverse symlinks')
    # Fail before disk creation if any frozen binary/runtime ingredient is wrong.
    agent = read_regular(args.agent, 32 * 1024**2)
    validate_agent(agent, args.agent_sha256)
    ram = read_regular(SEED_DIRECTORY / 'provision-initramfs.gz', 128 * 1024**2, RAM_SHA)
    kernel = ROOT / '.local/media/alpine-3.24.2/Image'
    stock_initrd = ROOT / '.local/media/alpine-3.24.2/veynode-initramfs.gz'
    read_regular(kernel, 128 * 1024**2, KERNEL_SHA)
    read_regular(stock_initrd, 128 * 1024**2, INITRD_SHA)
    if stat.S_IMODE(BASE.lstat().st_mode) != 0o400 or BASE.is_symlink():
        raise ValueError('accepted base must remain read-only regular disk')
    if disk_digest(BASE) != BASE_SHA:
        raise ValueError('accepted desktop base changed')
    payload = {'agent': agent, 'previous-files.sha256': checksums(dict(PRESERVED, **{'usr/local/bin/veynode-guest': OLD_AGENT_SHA})),
               'preserved-files.sha256': checksums(PRESERVED)}
    names = {'maintenance.sh': 'maintenance.sh', 'cgroup.sh': 'cgroup.sh', 'agent.openrc': 'agent.openrc'}
    for target, suffix in names.items():
        data = read_regular(ROOT / ('scripts/prepare-installed-qemu-exec-' + suffix), 65536)
        subprocess.run(['/bin/sh', '-n'], input=data, check=True)
        payload[target] = data
    hashes = {name: hashlib.sha256(data).hexdigest() for name, data in payload.items()}
    payload['payload.sha256'] = checksums(hashes)
    directory.mkdir(mode=0o700, parents=True, exist_ok=False)
    report = dict(version=1, vm_started=False, source_write_mounted=False, prepared=False,
                  base_sha256=BASE_SHA, source=str(BASE), payload_sha256=hashes,
                  required_worker='qemu_guest_exec v2 with incarnation guardian and verified disk FD sets')
    try:
        image_binary = args.image_binary.resolve(strict=True)
        report['image_binary_sha256'] = hashlib.sha256(read_regular(image_binary, 32 * 1024**2)).hexdigest()
        def image(*arguments):
            completed = subprocess.run([str(image_binary), *arguments, '--store', str(directory / 'images'),
                                        '--min-free-mib', '2048'], check=True, capture_output=True, text=True, timeout=180)
            return json.loads(completed.stdout)
        imported = image('import', '--id', 'desktop-base', '--source', str(BASE), '--source-stopped')
        cloned = image('clone', '--base', 'desktop-base', '--id', 'exec-v2')
        report.update(imported=imported, cloned=cloned)
        disk = Path(cloned['disk_path'])
        imported_disk = Path(imported['disk_path'])
        expected_disk = directory / 'images/clones/exec-v2/disk.raw'
        if disk != expected_disk or imported_disk != directory / 'images/bases/desktop-base/disk.raw':
            raise ValueError('unexpected image-tool publication path')
        if len({(p.stat().st_dev, p.stat().st_ino) for p in [BASE, imported_disk, disk]}) != 3:
            raise ValueError('disk inode alias')
        if stat.S_IMODE(disk.stat().st_mode) != 0o600 or stat.S_IMODE(imported_disk.stat().st_mode) != 0o400:
            raise ValueError('published disk permissions incorrect')
        report['source_sha256_after'] = disk_digest(BASE)
        for label, path in [('imported_sha256', imported_disk), ('clone_initial_sha256', disk)]:
            report[label] = disk_digest(path)
        if {report[k] for k in ['source_sha256_after', 'imported_sha256', 'clone_initial_sha256']} != {BASE_SHA}:
            raise ValueError('source or published clone integrity mismatch')
        spec = importlib.util.spec_from_file_location('cpio', ROOT / 'scripts/build-linux-fixture.py')
        cpio = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cpio)
        entries = [cpio.entry('veynode/exec-v2', b'', 0o40700)]
        for name, data in payload.items():
            entries.append(cpio.entry('veynode/exec-v2/' + name, data, 0o100700 if name.endswith('.sh') else 0o100600))
        entries.append(cpio.entry('TRAILER!!!', b''))
        initrd = directory / 'maintenance-initramfs.gz'
        with initrd.open('xb') as output:
            output.write(ram + gzip.compress(b''.join(entries), mtime=0))
        initrd.chmod(0o600)
        maintenance, installed = configs(directory, disk, kernel, stock_initrd, initrd)
        write_json(directory / 'maintenance.json', maintenance)
        write_json(directory / 'installed.json', installed)
        report.update(prepared=True, initrd_sha256=hashlib.sha256(initrd.read_bytes()).hexdigest(),
                      config_sha256={name: hashlib.sha256((directory / name).read_bytes()).hexdigest()
                                     for name in ['maintenance.json', 'installed.json']},
                      limit='Preparation only. Maintenance and installed cgroup/serial/GPU behavior not yet run or accepted.')
    finally:
        report['preparer_sha256'] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
        write_json(directory / 'preparation.json', report)
    print(directory / 'preparation.json')


if __name__ == '__main__':
    main()
