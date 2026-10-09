#!/usr/bin/env python3
"""Prepare ONE new disk clone + bounded RAM updater; does not start a VM.

Host source must already be stopped and its full SHA explicitly supplied. The
updater only adds a test watcher; it preserves frozen agent/graphics/ACPI files.
Run --help for arguments. Never install this image as a production base.
"""
import argparse
import gzip
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import secrets
import stat
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    obj = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(obj)
    return obj


prior = load('exec_preparation', ROOT / 'scripts/prepare-installed-qemu-exec.py')
cpio = load('crash_cpio', ROOT / 'scripts/build-linux-fixture.py')
AGENT_SHA = '4ef57dc808e8c2cabaeaf2e2ecc4fa7907f0379f07d911c4c4772eaa84800bb4'
PRESERVED = {**prior.PRESERVED, 'usr/local/bin/veynode-guest': AGENT_SHA,
    'etc/init.d/veynode-agent': '40bed00e946814bb51781b0c78e3a574aecf896817c04bd31f7d0e0dc35c22bb',
    'opt/veynode-exec-v2/cgroup.sh': '863cf9c611fc6917e172fc7e74ba5fffc5f45590800d0e4b75361eac775e74a8'}


def configs(directory, source, disk, initrd, nonce):
    cfg = dict(source)
    cfg.update(id='guest-crash-installed', disk=str(disk), window=False, qemu_guest_exec=True,
               command_line=source['command_line'] + ' veynode.guest_crash_once=' + nonce,
               serial_log=str(directory / 'installed.serial.log'))
    maintenance = dict(cfg)
    maintenance.update(id='guest-crash-maintenance', initrd=str(initrd), width=1280, height=800,
        command_line='console=ttyAMA0 rdinit=/veynode/crash/maintenance.sh veynode.crash_maintenance=1',
        serial_log=str(directory / 'maintenance.serial.log'))
    maintenance.pop('qemu_guest_exec')
    return maintenance, cfg


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ('source', 'config', 'directory', 'watcher'):
        p.add_argument('--' + name, type=Path, required=True)
    p.add_argument('--source-stopped', action='store_true', required=True)
    p.add_argument('--source-sha256', required=True)
    p.add_argument('--watcher-sha256', required=True)
    p.add_argument('--image-binary', type=Path, default=ROOT / 'target/release/vm-image')
    a = p.parse_args()
    directory = a.directory.absolute()
    if not directory.is_relative_to(ROOT / '.local') or '..' in directory.parts or any(x.is_symlink() for x in [directory, *directory.parents]):
        p.error('fresh repository-local directory without symlink traversal required')
    source = a.source.resolve(strict=True)
    if not source.is_relative_to(ROOT / '.local') or prior.disk_digest(source) != a.source_sha256:
        p.error('explicit stopped repository source SHA mismatch')
    cfg = json.loads(prior.read_regular(a.config, 16384))
    if cfg.get('qemu_guest_exec') is not True or cfg.get('backend') != 'qemu-hvf' or cfg.get('guest') != 'linux' or Path(cfg['disk']).resolve() != source:
        p.error('matching execution-v2 source config required')
    if 'veynode.guest_crash' in cfg['command_line'] or 'veynode.fixture_marker' in cfg['command_line']:
        p.error('source config already has diagnostic boot mutation')
    watcher = prior.read_regular(a.watcher, 32*1024**2, a.watcher_sha256)
    prior.validate_agent(watcher, a.watcher_sha256)
    ram = prior.read_regular(prior.SEED_DIRECTORY/'provision-initramfs.gz', 128*1024**2, prior.RAM_SHA)
    prior.read_regular(Path(cfg['kernel']), 128*1024**2, prior.KERNEL_SHA)
    prior.read_regular(Path(cfg['initrd']), 128*1024**2, prior.INITRD_SHA)
    nonce = secrets.token_hex(32)
    fixture = ROOT / 'scripts/fixtures/guest-crash'
    payload = {'watcher': watcher, 'config.json': (json.dumps({'nonce': nonce, 'agent_sha256': AGENT_SHA})+'\n').encode(),
               'preserved.sha256': prior.checksums(PRESERVED)}
    for name in ('maintenance.sh', 'watcher.openrc'):
        data = prior.read_regular(fixture/name, 65536)
        subprocess.run(['/bin/sh', '-n'], input=data, check=True)
        payload[name] = data
    payload['payload.sha256'] = prior.checksums({k:hashlib.sha256(v).hexdigest() for k,v in payload.items()})
    directory.mkdir(mode=0o700, parents=True, exist_ok=False)
    report = {'prepared': False, 'vm_started': False, 'source_write_mounted': False, 'source_sha256_before': a.source_sha256,
              'source': str(source), 'watcher_sha256': a.watcher_sha256, 'preserved_guest_sha256': PRESERVED}
    try:
        def image(*args):
            r = subprocess.run([str(a.image_binary.resolve()), *args, '--store', str(directory/'images'), '--min-free-mib', '2048'],
                               capture_output=True, text=True, check=True, timeout=180)
            return json.loads(r.stdout)
        report['imported'] = image('import', '--id', 'exec-base', '--source', str(source), '--source-stopped')
        report['cloned'] = image('clone', '--base', 'exec-base', '--id', 'guest-crash')
        base, disk = Path(report['imported']['disk_path']), Path(report['cloned']['disk_path'])
        if base != directory/'images/bases/exec-base/disk.raw' or disk != directory/'images/clones/guest-crash/disk.raw':
            raise ValueError('unexpected image publication')
        if len({(v.stat().st_dev,v.stat().st_ino) for v in (source,base,disk)}) != 3 or stat.S_IMODE(base.stat().st_mode)!=0o400 or stat.S_IMODE(disk.stat().st_mode)!=0o600:
            raise ValueError('private independent image inode/mode failure')
        for key,path in [('source_sha256_after',source),('imported_sha256',base),('initial_clone_sha256',disk)]:
            report[key]=prior.disk_digest(path)
            if report[key]!=a.source_sha256: raise ValueError('source/import/clone SHA mismatch')
        entries=[cpio.entry('veynode/crash',b'',0o40700)]
        entries += [cpio.entry('veynode/crash/'+k,v,0o100700 if k.endswith('.sh') else 0o100600) for k,v in payload.items()]
        entries.append(cpio.entry('TRAILER!!!',b''))
        initrd=directory/'maintenance-initramfs.gz'
        with initrd.open('xb') as f:f.write(ram+gzip.compress(b''.join(entries),mtime=0))
        initrd.chmod(0o600)
        maintenance,installed=configs(directory,cfg,disk,initrd,nonce)
        prior.write_json(directory/'maintenance.json',maintenance)
        prior.write_json(directory/'installed.json',installed)
        report.update(prepared=True,config_sha256={k:hashlib.sha256((directory/k).read_bytes()).hexdigest() for k in ('maintenance.json','installed.json')},
                      initrd_sha256=hashlib.sha256(initrd.read_bytes()).hexdigest(), image_binary_sha256=hashlib.sha256(prior.read_regular(a.image_binary,32*1024**2)).hexdigest(),
                      limit='Preparation only; no maintenance, pidfd signal or actual Guest crash was run.')
    finally:
        prior.write_json(directory/'preparation.json',report)
    print(directory/'preparation.json')


if __name__ == '__main__': main()
