#!/usr/bin/env python3
"""Build a tiny real ARM64 VM validation appliance, NOT a production desktop image.
Uses pinned upstream kernel/initramfs/modules, a statically linked guest service,
and an appended reproducible cpio archive. Never mounts/extracts Guest paths on host.
"""
import gzip, json, pathlib, shutil, struct, subprocess
ROOT = pathlib.Path(__file__).resolve().parents[1]
MEDIA = ROOT / '.local/media/alpine-3.24.2'
BIN = ROOT / '.local/bin/veynode-guest'

def entry(name, data, mode=0o100644, inode=1):
    filename = name.encode()+b'\0'
    values = [inode,mode,0,0,1,0,len(data),0,0,0,0,len(filename),0]
    header = b'070701'+b''.join(f'{v:08x}'.encode() for v in values)
    head = header+filename
    return head+b'\0'*(-len(head)%4)+data+b'\0'*(-len(data)%4)

def main():
    subprocess.run([shutil.which('go') or '/opt/homebrew/bin/go','-C',str(ROOT/'guest/veynode-agent'),'build','-trimpath','-o',str(BIN),'.'],check=True,env={**__import__('os').environ,'GOOS':'linux','GOARCH':'arm64','CGO_ENABLED':'0'})
    tool=shutil.which('unsquashfs') or '/opt/homebrew/bin/unsquashfs'
    source=MEDIA/'modloop-virt'
    listing=subprocess.check_output([tool,'-ll',str(source)],text=True)
    module_paths=[line.split('squashfs-root/',1)[1] for line in listing.splitlines() if 'squashfs-root/' in line and line.endswith(('/vsock.ko','/vmw_vsock_virtio_transport_common.ko','/vmw_vsock_virtio_transport.ko'))]
    if len(module_paths)!=3:raise RuntimeError('Expected 3 pinned vsock kernel modules')
    init=b'''#!/bin/sh
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /tmp /root /sys/kernel/debug
mount -t tmpfs tmpfs /tmp
mount -t debugfs debugfs /sys/kernel/debug
insmod /veynode/modules/vsock.ko
insmod /veynode/modules/vmw_vsock_virtio_transport_common.ko
insmod /veynode/modules/vmw_vsock_virtio_transport.ko
modprobe virtio_gpu
printf 'VEYNODE_FIXTURE_READY\\n'
printf 'VEYNODE_LEGACY_EXECUTION_DIAGNOSTIC: root process groups; setsid descendants are NOT contained\\n'
/veynode/agent --allow-execute --legacy-exec-process-groups &
while true; do /bin/sh; sleep 1; done
'''
    overlay=entry('veynode',b'',0o40755)+entry('veynode/modules',b'',0o40755)
    for path in module_paths:
        overlay+=entry('veynode/modules/'+path.rsplit('/',1)[1],subprocess.check_output([tool,'-cat',str(source),path]))
    # Direct-disk boot needs ext4; Alpine's stock netboot initramfs omits it.
    version = module_paths[0].split('/')[1]
    depbytes = subprocess.check_output([tool,'-cat',str(source),f'modules/{version}/modules.dep'])
    dependencies = dict(line.split(':',1) for line in depbytes.decode().splitlines())
    required_modules = set()
    def include(path):
        if path in required_modules: return
        required_modules.add(path)
        for dependency in dependencies[path].split(): include(dependency)
    include('kernel/fs/ext4/ext4.ko')
    include('kernel/net/vmw_vsock/vmw_vsock_virtio_transport.ko')
    directories = set()
    for path in sorted(required_modules):
        target = pathlib.PurePosixPath(f'usr/lib/modules/{version}/{path}')
        for parent in reversed(target.parents):
            if str(parent) != '.' and str(parent) not in directories:
                directories.add(str(parent)); overlay += entry(str(parent), b'', 0o40755)
        overlay += entry(str(target), subprocess.check_output([tool,'-cat',str(source),f'modules/{version}/{path}']))
    for metadata in ['modules.dep','modules.dep.bin','modules.alias','modules.alias.bin','modules.symbols','modules.symbols.bin']:
        overlay += entry(f'usr/lib/modules/{version}/{metadata}', subprocess.check_output([tool,'-cat',str(source),f'modules/{version}/{metadata}']))
    overlay+=entry('veynode/agent',BIN.read_bytes(),0o100755)+entry('veynode/init',init,0o100755)
    overlay+=entry('etc/os-release',b'NAME="Veynode validation appliance (Alpine initramfs)"\nID=alpine\nVERSION_ID=3.24.2\n')
    overlay+=entry('TRAILER!!!',b'')
    output=MEDIA/'veynode-initramfs.gz'
    output.write_bytes((MEDIA/'initramfs-virt').read_bytes()+gzip.compress(overlay,mtime=0))
    config=json.loads((ROOT/'.local/run/linux-smoke.json').read_text())
    config.update(id='linux-agent',initrd=str(output),command_line='console=hvc0 rdinit=/veynode/init',serial_log=str(ROOT/'.local/run/linux-agent-serial.log'))
    (ROOT/'.local/run/linux-agent.json').write_text(json.dumps(config,indent=2)+'\n')
    print(json.dumps({'config':str(ROOT/'.local/run/linux-agent.json'),'fixture':'real ARM64 initramfs appliance; no installed desktop','agent_transport':'vsock:5000; host CID2 only'}))
if __name__=='__main__':main()
