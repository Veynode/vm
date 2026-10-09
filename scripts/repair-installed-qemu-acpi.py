#!/usr/bin/env python3
"""Prepare (do not boot) a single-file ACPI instrumentation repair for our seed."""
import argparse
import gzip
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
REPAIR = b'''#!/bin/sh
set -eu
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
trap 'echo VEYNODE_ACPI_REPAIR_FAIL; sync; poweroff -f' EXIT
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
test -e /lib/modules || ln -s /usr/lib/modules /lib/modules
modprobe virtio_blk
modprobe ext4
mkdir -p /newroot
mount -t ext4 /dev/vda /newroot
test "$(cat /newroot/etc/alpine-release)" = 3.23.6
test -f /newroot/opt/veynode-fixture/modules.sha256
grep -Fx 'action=/etc/acpi/handler.sh %e' /newroot/etc/acpi/events/anything
cp /veynode/new-acpi-handler.sh /newroot/etc/acpi/handler.sh
chmod 755 /newroot/etc/acpi/handler.sh
sha256sum /newroot/etc/acpi/handler.sh
sync
umount /newroot
trap - EXIT
echo VEYNODE_ACPI_REPAIR_PASS
poweroff -f
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--directory', type=Path, required=True)
    args = parser.parse_args()
    directory = args.directory.resolve(strict=True)
    if not directory.is_relative_to(ROOT / '.local'):
        raise ValueError('only isolated repository fixtures supported')
    config = json.loads((directory / 'installed-first.json').read_text())
    if Path(config['disk']) != directory / 'seed.raw':
        raise ValueError('repair must target the fixture seed')
    first = json.loads((directory / 'installed-first-result.json').read_text())
    if first['final']['state'] != 'stopped' or first['final']['exit_code'] != 0:
        raise ValueError('repair requires prior stopped seed evidence; worker also enforces live locks')
    spec = importlib.util.spec_from_file_location('cpio', ROOT / 'scripts/build-linux-fixture.py')
    cpio = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(cpio)
    handler = (ROOT / 'scripts/installed-qemu-acpi-handler.sh').read_bytes()
    for data in [handler, REPAIR]:
        subprocess.run(['/bin/sh', '-n'], input=data, check=True)
    archive = b''.join([cpio.entry('veynode/acpi-repair', REPAIR, 0o100755),
                         cpio.entry('veynode/new-acpi-handler.sh', handler, 0o100755),
                         cpio.entry('TRAILER!!!', b'')])
    initrd = directory / 'acpi-repair-initramfs.gz'
    with initrd.open('xb') as output:
        output.write((directory / 'provision-initramfs.gz').read_bytes() + gzip.compress(archive, mtime=0))
    config.update(id='installed-desktop-acpi-repair', initrd=str(initrd),
                  command_line='console=ttyAMA0 rdinit=/veynode/acpi-repair',
                  serial_log=str(directory / 'acpi-repair.serial.log'))
    with (directory / 'acpi-repair.json').open('x') as output:
        json.dump(config, output, indent=2)
        output.write('\n')
    print(json.dumps({'config': str(directory / 'acpi-repair.json'),
                      'initrd_sha256': hashlib.sha256(initrd.read_bytes()).hexdigest(),
                      'handler_sha256': hashlib.sha256(handler).hexdigest()}))


if __name__ == '__main__':
    main()
