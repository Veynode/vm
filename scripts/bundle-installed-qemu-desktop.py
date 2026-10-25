#!/usr/bin/env python3
"""Prepare pinned RAM provisioning inputs for a new, empty private seed.

Never launches QEMU. The disk-writing config requires the guarded QEMU worker.
"""
import argparse
import gzip
import hashlib
import importlib.util
import json
from pathlib import Path
import stat
import subprocess
import tarfile

ROOT = Path(__file__).resolve().parents[1]
MEDIA = ROOT / '.local/media/alpine-3.24.2'
ROOTFS_SHA = 'b17a57958e29735ff0e6e64254d958e65903687a70ca30cc430b33a965ad49d7'
GLAMOR_SHA = 'ba64c24cb70c953d26ac032633b6bca98577ea415f7abdc32f7f451d9a115f67'



def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--directory', type=Path, required=True)
    parser.add_argument('--agent', type=Path, required=True)
    args = parser.parse_args()
    directory = args.directory.resolve()
    if not directory.is_relative_to(ROOT / '.local'):
        raise ValueError('fixture must be under repository .local')
    directory.mkdir(mode=0o700, parents=True, exist_ok=False)
    disk = directory / 'seed.raw'
    # This fresh disk is never derived by downgrading a previously installed Guest.
    import os
    fd = os.open(disk, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    os.ftruncate(fd, 16 * 1024**3)
    os.close(fd)
    rootfs = ROOT / '.local/runtime/utm/alpine-minirootfs-3.23.6-aarch64.tar.gz'
    if hashlib.sha256(rootfs.read_bytes()).hexdigest() != ROOTFS_SHA:
        raise ValueError('rootfs hash mismatch')
    spec = importlib.util.spec_from_file_location('cpio', ROOT / 'scripts/build-linux-fixture.py')
    cpio = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(cpio)
    entries = []
    with tarfile.open(rootfs) as archive:
        for member in archive:
            name = member.name.removeprefix('./')
            if name in ('', '.'):
                continue
            if name.startswith('/') or '..' in Path(name).parts:
                raise ValueError('unsafe rootfs member')
            if member.isdir():
                mode, data = stat.S_IFDIR | member.mode, b''
            elif member.issym():
                mode, data = stat.S_IFLNK | member.mode, member.linkname.encode()
            elif member.isfile():
                mode, data = stat.S_IFREG | member.mode, archive.extractfile(member).read()
            else:
                raise ValueError('unsupported rootfs member')
            entries.append(cpio.entry(name, data, mode))
    payload = {
        'agent': args.agent.read_bytes(),
        'acpi-handler.sh': (ROOT / 'scripts/installed-qemu-acpi-handler.sh').read_bytes(),
        'desktop-boot.sh': (ROOT / 'scripts/installed-qemu-desktop-boot.sh').read_bytes(),
        'boot-marker.py': (ROOT / 'scripts/installed-qemu-boot-marker.py').read_bytes(),
        'install.sh': (ROOT / 'scripts/installed-qemu-desktop-install.sh').read_bytes(),
        'libglamoregl.so': (ROOT / '.local/runtime/utm/glamor-causal-artifacts/modules-fixed/libglamoregl.so').read_bytes(),
        'xrender-probe.c': (ROOT / 'scripts/linux-xrender-probe.c').read_bytes(),
    }
    if payload['agent'][:6] != b'\x7fELF\x02\x01' or int.from_bytes(payload['agent'][18:20], 'little') != 183:
        raise ValueError('agent must be the Linux ARM64 binary')
    if hashlib.sha256(payload['libglamoregl.so']).hexdigest() != GLAMOR_SHA:
        raise ValueError('fixed glamor hash mismatch')
    hashes = {name: hashlib.sha256(data).hexdigest() for name, data in payload.items()}
    payload['payload.sha256'] = ''.join(f'{sha}  {name}\n' for name, sha in hashes.items()).encode()
    entries.extend([cpio.entry('veynode', b'', 0o40755), cpio.entry('veynode/payload', b'', 0o40700)])
    for name, data in payload.items():
        entries.append(cpio.entry('veynode/payload/' + name, data, 0o100644))
    version = '6.18.52-0-virt'
    modloop = MEDIA / 'modloop-virt'
    deps = dict(line.split(':', 1) for line in subprocess.check_output(
        ['unsquashfs', '-cat', str(modloop), f'modules/{version}/modules.dep'], text=True).splitlines())
    modules = set()
    def include(name):
        if name not in modules:
            modules.add(name)
            for child in deps[name].split():
                include(child)
    for name in ['kernel/drivers/acpi/button.ko', 'kernel/drivers/input/evdev.ko',
                 'kernel/drivers/virtio/virtio_input.ko', 'kernel/drivers/gpu/drm/virtio/virtio-gpu.ko']:
        include(name)
    directories = set()
    for name in sorted(modules):
        target = Path(f'usr/lib/modules/{version}/{name}')
        for parent in reversed(target.parents):
            if str(parent) != '.' and parent not in directories:
                directories.add(parent)
                entries.append(cpio.entry(str(parent), b'', 0o40755))
        entries.append(cpio.entry(str(target), subprocess.check_output(
            ['unsquashfs', '-cat', str(modloop), f'modules/{version}/{name}'])))
    provision = (ROOT / 'scripts/installed-qemu-desktop-provision.sh').read_bytes()
    subprocess.run(['/bin/sh', '-n'], input=provision, check=True)
    desktop_spec = importlib.util.spec_from_file_location('desktop', ROOT / 'scripts/qemu-linux-desktop-probe.py')
    desktop = importlib.util.module_from_spec(desktop_spec)
    desktop_spec.loader.exec_module(desktop)
    entries.extend([cpio.entry('veynode/rootfs.tar.gz', rootfs.read_bytes()),
                    cpio.entry('veynode/installed-provision', provision, 0o100755),
                    cpio.entry('veynode/dhcp', desktop.DHCP.encode(), 0o100755), cpio.entry('TRAILER!!!', b'')])
    initrd = directory / 'provision-initramfs.gz'
    with initrd.open('xb') as output:
        output.write((MEDIA / 'veynode-initramfs.gz').read_bytes() + gzip.compress(b''.join(entries), mtime=0))
    config = dict(id='installed-desktop-seed', backend='qemu-hvf', guest='linux', cpus=2,
                  memory_mib=1536, kernel=str(MEDIA / 'Image'), disk=str(disk), window=False,
                  width=1280, height=800, serial_log=str(directory / 'seed.serial.log'))
    for name, extra in [('provision', dict(initrd=str(initrd), command_line='console=ttyAMA0 rdinit=/veynode/installed-provision')),
                        ('installed', dict(initrd=str(MEDIA / 'veynode-initramfs.gz'), command_line='console=ttyAMA0 root=/dev/vda rootfstype=ext4 modules=ext4,virtio_blk'))]:
        with (directory / (name + '.json')).open('x') as output:
            json.dump(dict(config, **extra), output, indent=2)
            output.write('\n')
    with (directory / 'bundle.json').open('x') as output:
        json.dump(dict(payload_sha256=hashes, rootfs_sha256=ROOTFS_SHA, fixed_glamor_sha256=GLAMOR_SHA, modules=sorted(modules),
                       initrd_sha256=hashlib.sha256(initrd.read_bytes()).hexdigest(),
                       kernel_sha256=hashlib.sha256((MEDIA / 'Image').read_bytes()).hexdigest(),
                       stock_initrd_sha256=hashlib.sha256((MEDIA / 'veynode-initramfs.gz').read_bytes()).hexdigest(),
                       vm_started=False,
                       limitation='Repository packages fetched in isolated Guest; actual versions recorded at build time.'), output, indent=2)
        output.write('\n')
    print(directory / 'provision.json')


if __name__ == '__main__':
    main()
