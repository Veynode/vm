#!/usr/bin/env python3
"""Fetch pinned upstream Alpine ARM64 artifacts and prepare a local boot fixture.
Downloads are cached, SHA256 verified before extraction. No host-root changes.
"""
import gzip, hashlib, json, os, pathlib, struct, subprocess, sys, tarfile, urllib.request
ROOT = pathlib.Path(__file__).resolve().parents[1]
MEDIA = ROOT / '.local/media/alpine-3.24.2'
BASE = 'https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/aarch64/'
ARTIFACTS = {
 'alpine-netboot-3.24.2-aarch64.tar.gz': '124087f5df9ac698bb4e942d603eaae96721e451cdc4706c71a5e42ff863b589',
 'alpine-minirootfs-3.24.2-aarch64.tar.gz': '9bf70a7f18ea44094cbb5f70c58f9af129c8214745743db0e68e5502cc2ce773',
}
def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024*1024), b''): h.update(block)
    return h.hexdigest()
def main():
    MEDIA.mkdir(parents=True, exist_ok=True)
    for name, expected in ARTIFACTS.items():
        path = MEDIA / name
        if not path.exists():
            print('Downloading '+name, flush=True)
            tmp = path.with_suffix('.part')
            subprocess.run(['curl','--fail','--location','--retry','3','--max-time','900','--output',str(tmp), BASE+name],check=True)
            if digest(tmp) != expected: raise RuntimeError('Checksum mismatch: '+name)
            tmp.replace(path)
        if digest(path) != expected: raise RuntimeError('Checksum mismatch: '+name)
    with tarfile.open(MEDIA / next(iter(ARTIFACTS))) as archive:
        for suffix in ('vmlinuz-virt','initramfs-virt','modloop-virt'):
            matches = [m for m in archive.getmembers() if m.name.split('/')[-1] == suffix and m.isfile()]
            if len(matches) != 1: raise RuntimeError('Unexpected archive layout for '+suffix)
            (MEDIA/suffix).write_bytes(archive.extractfile(matches[0]).read())
    kernel = (MEDIA/'vmlinuz-virt').read_bytes()
    if kernel[4:8] == b'zimg':
        offset, size = struct.unpack_from('<II', kernel, 8)
        if offset + size > len(kernel): raise RuntimeError('Invalid EFI zboot payload')
        kernel = gzip.decompress(kernel[offset:offset+size])
    elif kernel[:2] == b'\x1f\x8b': kernel = gzip.decompress(kernel)
    if kernel[56:60] != b'ARM\x64': raise RuntimeError('Expected raw ARM64 Linux Image')
    (MEDIA/'Image').write_bytes(kernel)
    config = {'id':'linux-smoke','guest':'linux','cpus':2,'memory_mib':1024,
       'kernel':str(MEDIA/'Image'), 'initrd':str(MEDIA/'initramfs-virt'),
       'command_line':'console=hvc0 rdinit=/bin/sh',
       'window':False, 'serial_log':str(ROOT/'.local/run/linux-serial.log')}
    (ROOT/'.local/run').mkdir(exist_ok=True)
    (ROOT/'.local/run/linux-smoke.json').write_text(json.dumps(config,indent=2)+'\n')
    print(json.dumps({'config':str(ROOT/'.local/run/linux-smoke.json'),'sha256':{n:digest(MEDIA/n) for n in ARTIFACTS}}))
if __name__ == '__main__': main()
