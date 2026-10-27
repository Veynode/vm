#!/usr/bin/env python3
"""Build the VeynodeVM micro-VM boot assets from pinned upstream Alpine media.

Output (default .local/boot):
  Image          raw ARM64 kernel (Alpine linux-virt, from scripts/prepare-linux.py)
  initramfs.gz   Alpine initramfs-virt + overlay: guest agent as container init,
                 mke2fs and resize2fs from pinned e2fsprogs packages
  modloop        the matching full kernel module squashfs, attached read-only
  manifest.json  SHA256 of every output and input

Nothing is extracted to the host filesystem; packages are read as archives.
"""
import argparse, gzip, hashlib, importlib.util, io, json, os, pathlib, shutil, subprocess, tarfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
MEDIA = ROOT / '.local/media/alpine-3.24.2'
CACHE = ROOT / '.local/media/apk-3.24'
REPO = 'https://dl-cdn.alpinelinux.org/alpine/v3.24/main/aarch64/'
PACKAGES = {
    'e2fsprogs-1.47.4-r0.apk': '52fd401e79ce6b0ff7648733ba4de3135083cd0132543534e72106e7ea767a00',
    'e2fsprogs-extra-1.47.4-r0.apk': '70b0a01bf62349263a8f400809c7512e991b7db13dbebbf9b3a1cb3b3d21efcc',
    'e2fsprogs-libs-1.47.4-r0.apk': '52124971ae397d599aadd79263fcd68fc9e271922ccf8185b124f35fe24c60c8',
    'libcom_err-1.47.4-r0.apk': 'a75b5955ff57046e33fc2842cfe466dc77d8e68305d5c86c6646b38a069490a2',
}
# Only what container-init needs to format and grow the writable layer.
WANTED = {'sbin/mke2fs', 'usr/sbin/resize2fs', 'etc/mke2fs.conf', 'usr/lib/libe2p.so.2', 'usr/lib/libe2p.so.2.3',
          'usr/lib/libext2fs.so.2', 'usr/lib/libext2fs.so.2.4', 'usr/lib/libcom_err.so.2', 'usr/lib/libcom_err.so.2.1'}
INIT = b'#!/bin/sh\nexec /veynode/agent container-init\n'

spec = importlib.util.spec_from_file_location('fixture', ROOT / 'scripts/build-linux-fixture.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


def digest(path):
    h = hashlib.sha256()
    with open(path, 'rb') as stream:
        for block in iter(lambda: stream.read(1 << 20), b''):
            h.update(block)
    return h.hexdigest()


def fetch(name, expected):
    CACHE.mkdir(parents=True, exist_ok=True)
    path = CACHE / name
    if not path.exists():
        part = path.with_suffix('.part')
        subprocess.run(['curl', '--fail', '--location', '--retry', '3', '--output', str(part), REPO + name], check=True)
        if digest(part) != expected:
            part.unlink()
            raise SystemExit(f'checksum mismatch: {name}')
        part.replace(path)
    if digest(path) != expected:
        raise SystemExit(f'checksum mismatch: {name}')
    return path


def package_entries(path):
    """Yield cpio entries for wanted members of an apk (concatenated gzip tar streams)."""
    with gzip.open(path) as stream:
        archive = tarfile.open(fileobj=io.BytesIO(stream.read()), mode='r:', ignore_zeros=True)
        for member in archive.getmembers():
            if member.name not in WANTED:
                continue
            if member.issym():
                yield member.name, member.linkname.encode(), 0o120777
            elif member.isfile():
                yield member.name, archive.extractfile(member).read(), 0o100000 | (member.mode & 0o7777)


def build(out, agent):
    for name in ('Image', 'initramfs-virt', 'modloop-virt'):
        if not (MEDIA / name).exists():
            raise SystemExit(f'missing {MEDIA / name}; run scripts/prepare-linux.py first')
    out.mkdir(parents=True, exist_ok=True)
    overlay = b''
    directories = set()
    inode = 100

    def add(name, data, mode):
        nonlocal overlay, inode
        for parent in reversed(pathlib.PurePosixPath(name).parents):
            if str(parent) != '.' and str(parent) not in directories:
                directories.add(str(parent))
                inode += 1
                overlay += fixture.entry(str(parent), b'', 0o40755, inode)
        inode += 1
        overlay += fixture.entry(name, data, mode, inode)

    inputs = {}
    for package, expected in PACKAGES.items():
        path = fetch(package, expected)
        inputs[package] = expected
        for name, data, mode in package_entries(path):
            add(name, data, mode)
    add('veynode/agent', agent.read_bytes(), 0o100755)
    add('veynode/init', INIT, 0o100755)
    add('etc/os-release', b'NAME="VeynodeVM micro-VM init"\nID=veynodevm\n', 0o100644)
    overlay += fixture.entry('TRAILER!!!', b'')
    initramfs = out / 'initramfs.gz'
    initramfs.write_bytes((MEDIA / 'initramfs-virt').read_bytes() + gzip.compress(overlay, mtime=0))
    shutil.copyfile(MEDIA / 'Image', out / 'Image')
    shutil.copyfile(MEDIA / 'modloop-virt', out / 'modloop')
    for name in ('Image', 'initramfs.gz', 'modloop'):
        os.chmod(out / name, 0o444)
    manifest = {
        'kernel': 'alpine-3.24.2 linux-virt',
        'outputs': {name: digest(out / name) for name in ('Image', 'initramfs.gz', 'modloop')},
        'inputs': {**inputs, 'agent': digest(agent), 'initramfs-virt': digest(MEDIA / 'initramfs-virt')},
        'command_line': 'console=hvc0 rdinit=/veynode/init quiet loglevel=1',
    }
    (out / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--output', type=pathlib.Path, default=ROOT / '.local/boot')
    args = parser.parse_args()
    agent = ROOT / '.local/bin/veynode-guest'
    agent.parent.mkdir(parents=True, exist_ok=True)
    go = shutil.which('go') or '/opt/homebrew/bin/go'
    subprocess.run([go, '-C', str(ROOT / 'guest/veynode-agent'), 'build', '-trimpath', '-o', str(agent), '.'],
                   check=True, env={**os.environ, 'GOOS': 'linux', 'GOARCH': 'arm64', 'CGO_ENABLED': '0'})
    out = args.output.resolve()
    if out.exists():
        for name in ('Image', 'initramfs.gz', 'modloop', 'manifest.json'):
            if (out / name).exists():
                os.chmod(out / name, 0o644)
                (out / name).unlink()
    print(json.dumps({'output': str(out), **build(out, agent)}, indent=2))


if __name__ == '__main__':
    main()
