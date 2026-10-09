#!/usr/bin/env python3
"""Fixed-path, restricted-marker diagnostic for an isolated installed fixture."""
import json
import os
from pathlib import Path
import re
import stat

ROOT = Path('/var/lib/veynode-fixture')
MARKER = ROOT / 'persistence-marker'
PATTERN = re.compile(r'[A-Za-z0-9_-]{1,64}', re.ASCII)


def value(text):
    if not PATTERN.fullmatch(text):
        raise ValueError('invalid fixed fixture marker')
    return text


def main():
    mounted = [line.split() for line in Path('/proc/mounts').read_text().splitlines()]
    roots = [parts for parts in mounted if parts[1] == '/']
    if len(roots) != 1 or roots[0][0] != '/dev/vda' or roots[0][2] != 'ext4':
        raise RuntimeError('installed fixture must boot from /dev/vda ext4')
    root_stat = ROOT.lstat()
    if not stat.S_ISDIR(root_stat.st_mode) or root_stat.st_uid != 0 or root_stat.st_mode & 0o077:
        raise RuntimeError('unsafe marker directory')
    requested = [item.partition('=')[2] for item in Path('/proc/cmdline').read_text().split()
                 if item.startswith('veynode.fixture_marker=')]
    if len(requested) > 1:
        raise ValueError('duplicate fixture marker')
    requested = value(requested[0]) if requested else None
    before = None
    try:
        fd = os.open(MARKER, os.O_RDONLY | os.O_NOFOLLOW)
    except FileNotFoundError:
        pass
    else:
        with os.fdopen(fd, 'rb') as stream:
            metadata = os.fstat(stream.fileno())
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1 or metadata.st_uid != 0:
                raise RuntimeError('unsafe marker inode')
            before = value(stream.read(65).decode('ascii'))
    if requested:
        temporary = ROOT / ('marker-' + str(os.getpid()))
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, 'w') as output:
            output.write(requested)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, MARKER)
        os.sync()
    boot_id = Path('/proc/sys/kernel/random/boot_id').read_text().strip()
    if not re.fullmatch('[0-9a-f-]{36}', boot_id):
        raise RuntimeError('invalid kernel boot identifier')
    print('VEYNODE_INSTALLED_BOOT ' + json.dumps(dict(boot_id=boot_id, root_device=roots[0][0],
          root_fstype=roots[0][2], marker_before=before, marker_after=requested or before,
          marker_write_requested=requested is not None), separators=(',', ':')), flush=True)


if __name__ == '__main__':
    main()
