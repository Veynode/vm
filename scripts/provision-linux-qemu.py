#!/usr/bin/env python3
"""Install fresh persistent Alpine disk via QEMU user-mode NAT, then boot on VZ.
The current VZ NAT reaches DHCP/DNS but outbound HTTPS times out on this host.
This controlled alternate installer does not alter host networking/protections.
"""
import argparse,re,gzip,importlib.util,json,os,pathlib,shutil,signal,stat,subprocess,tarfile,time
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--name',default='linux-persistent')
parser.add_argument('--allow-files-fixture',action='store_true',help='Install dedicated UID1001 files worker and private transfer directory in this NEW fixture only')
args=parser.parse_args()
if not re.fullmatch(r'[A-Za-z0-9_-]{1,64}',args.name):raise SystemExit('Invalid fixture name')
ROOT=pathlib.Path(__file__).resolve().parents[1];MEDIA=ROOT/'.local/media/alpine-3.24.2';DEST=ROOT/'.local/vms'/args.name
if DEST.exists():raise SystemExit('Destination exists. Refusing to overwrite; preserve and inspect installation logs.')
DEST.mkdir(parents=True,mode=0o700)
config_spec=importlib.util.spec_from_file_location('fixture',ROOT/'scripts/build-linux-fixture.py');f=importlib.util.module_from_spec(config_spec);config_spec.loader.exec_module(f)
entries=[];source=MEDIA/'alpine-minirootfs-3.24.2-aarch64.tar.gz'
with tarfile.open(source) as archive:
 for m in archive.getmembers():
  name=m.name.removeprefix('./')
  if name in ('','.'):continue
  if name.startswith('/') or '..' in pathlib.Path(name).parts:raise RuntimeError('Unsafe rootfs member')
  if m.isdir():mode,data=stat.S_IFDIR|m.mode,b''
  elif m.issym():mode,data=stat.S_IFLNK|m.mode,m.linkname.encode()
  elif m.isfile():mode,data=stat.S_IFREG|m.mode,archive.extractfile(m).read()
  else:raise RuntimeError('Unsupported rootfs member')
  entries.append(f.entry(name,data,mode))
init=r'''#!/bin/sh
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
stage=init
fail(){ echo "VEYNODE_INSTALL_FAIL stage=$stage"; sync; poweroff -f; }
trap fail EXIT
set -eu
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /tmp /run /newroot
mount -t tmpfs tmpfs /tmp
ln -s /usr/lib/modules /lib/modules
stage=network
modprobe virtio_net
modprobe virtio_blk
modprobe ext4
ip link set lo up
ip link set eth0 up
udhcpc -i eth0 -n -q -t 5 -s /veynode/dhcp
stage=mkfs-tools
apk add --no-cache --no-progress e2fsprogs
stage=format-new-fixture-disk
mkfs.ext4 -F -L veynode-root /dev/vda
mount /dev/vda /newroot
tar -xzf /veynode/rootfs.tar.gz -C /newroot
cp /etc/resolv.conf /newroot/etc/resolv.conf
stage=install-base
apk --root /newroot add --no-cache --no-progress alpine-base acpid e2fsprogs
mkdir -p /newroot/usr/local/bin /newroot/usr/lib/modules
cp /veynode/agent /newroot/usr/local/bin/veynode-guest
chmod 755 /newroot/usr/local/bin/veynode-guest
cp -a /usr/lib/modules/. /newroot/usr/lib/modules/
ln -s /usr/lib/modules /newroot/lib/modules
mkdir -p /newroot/usr/lib/modules/$(uname -r)/kernel/net/vmw_vsock
cp /veynode/modules/*.ko /newroot/usr/lib/modules/$(uname -r)/kernel/net/vmw_vsock/
ln -s /usr/bin/kmod /tmp/depmod
/tmp/depmod -b /newroot $(uname -r)
printf 'virtio_net\nvirtio_gpu\nvmw_vsock_virtio_transport\n' > /newroot/etc/modules
stage=configure
# A locked non-root command account; no credentials or network login are created.
chroot /newroot /usr/sbin/adduser -D -u 1000 -s /bin/sh veynode
printf '/dev/vda / ext4 defaults 0 1\n' > /newroot/etc/fstab
printf 'veynode-linux\n' > /newroot/etc/hostname
printf 'auto lo\niface lo inet loopback\nauto eth0\niface eth0 inet dhcp\n' > /newroot/etc/network/interfaces
cat > /newroot/etc/init.d/veynode-agent <<'RC'
#!/sbin/openrc-run
name="Veynode guest integration"
command="/usr/local/bin/veynode-guest"
command_args="--allow-execute --allow-shutdown --exec-user=veynode"
command_background="yes"
pidfile="/run/veynode-agent.pid"
depend() { need modules; }
RC
chmod 755 /newroot/etc/init.d/veynode-agent
mkdir -p /newroot/etc/runlevels/sysinit /newroot/etc/runlevels/boot /newroot/etc/runlevels/default /newroot/etc/runlevels/shutdown
for service in devfs dmesg mdev hwdrivers; do ln -s /etc/init.d/$service /newroot/etc/runlevels/sysinit/$service; done
for service in bootmisc hostname sysctl modules; do ln -s /etc/init.d/$service /newroot/etc/runlevels/boot/$service; done
for service in networking acpid veynode-agent; do ln -s /etc/init.d/$service /newroot/etc/runlevels/default/$service; done
for service in mount-ro killprocs savecache; do ln -s /etc/init.d/$service /newroot/etc/runlevels/shutdown/$service; done
# Root remains locked; no network login service or guessed password.
# BusyBox init watches hvc0 console but it still requires login authentication.
printf 'hvc0::respawn:/sbin/getty 115200 hvc0\n' >> /newroot/etc/inittab
printf '\nVEYNODE_PACKAGES_BEGIN\n'
chroot /newroot /sbin/apk info -v
printf 'VEYNODE_PACKAGES_END\n'
sync
umount /newroot
trap - EXIT
echo VEYNODE_INSTALL_PASS
poweroff -f
'''
if args.allow_files_fixture:
 subprocess.run([shutil.which('go') or '/opt/homebrew/bin/go','-C',str(ROOT/'guest/veynode-agent'),'build','-trimpath','-o',str(DEST/'veynode-guest'),'.'],check=True,env={**os.environ,'GOOS':'linux','GOARCH':'arm64','CGO_ENABLED':'0'})
 entries.append(f.entry('veynode/agent',(DEST/'veynode-guest').read_bytes(),0o100755))
 init=init.replace('stage=configure\n', '''stage=configure
chroot /newroot /usr/sbin/adduser -D -u 1001 -s /sbin/nologin veynode-files
mkdir -p /newroot/home/veynode-files/transfers
chown 1001:1001 /newroot/home/veynode-files /newroot/home/veynode-files/transfers
chmod 700 /newroot/home/veynode-files /newroot/home/veynode-files/transfers
printf outside-boundary > /newroot/home/veynode-files/outside
chown 1001:1001 /newroot/home/veynode-files/outside
ln -s /home/veynode-files/outside /newroot/home/veynode-files/transfers/outside-link
ln /newroot/home/veynode-files/outside /newroot/home/veynode-files/transfers/outside-hardlink
printf root-owned > /newroot/home/veynode-files/transfers/root-owned
mkfifo /newroot/home/veynode-files/transfers/fifo
chown 1001:1001 /newroot/home/veynode-files/transfers/fifo
''').replace('--exec-user=veynode"', '--exec-user=veynode --allow-files --files-user=veynode-files --files-root=/home/veynode-files/transfers"')
dhcp=r'''#!/bin/sh
case "$1" in bound|renew)
ip addr replace "$ip/24" dev "$interface"
ip route replace default via "$router"
: > /etc/resolv.conf
for server in $dns; do echo "nameserver $server" >> /etc/resolv.conf; done
;; esac
'''
entries.extend([f.entry('veynode/disk-init',init.encode(),0o100755),f.entry('veynode/dhcp',dhcp.encode(),0o100755),f.entry('veynode/rootfs.tar.gz',source.read_bytes()),f.entry('TRAILER!!!',b'')])
initrd=DEST/'install-initramfs.gz';initrd.write_bytes((MEDIA/'veynode-initramfs.gz').read_bytes()+gzip.compress(b''.join(entries),mtime=0))
disk=DEST/'disk.raw';fd=os.open(disk,os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600);os.ftruncate(fd,16*1024**3);os.close(fd)
status={'state':'installing','started_at_unix':time.time(),'backend':'QEMU11.1.2 HVF user-mode NAT for installation; Apple VZ for subsequent acceptance','media':'Alpine3.24.2 pinned minirootfs + kernel6.18.52','network_package_versions':'recorded in serial log; repository dependencies not vendored/pinned yet'}
(DEST/'installation-status.json').write_text(json.dumps(status,indent=2)+'\n')
cmd=[shutil.which('qemu-system-aarch64') or '/opt/homebrew/bin/qemu-system-aarch64','-machine','virt,accel=hvf','-cpu','host','-smp','2','-m','1024','-nodefaults','-nic','user,model=virtio-net-pci','-display','none','-serial','stdio','-monitor','none','-kernel',str(MEDIA/'Image'),'-initrd',str(initrd),'-append','console=ttyAMA0 rdinit=/veynode/disk-init','-drive',f'file={disk},format=raw,if=virtio']
try:
 with (DEST/'installation-serial.log').open('w') as log:
  p=subprocess.Popen(cmd,stdin=subprocess.DEVNULL,stdout=log,stderr=log,start_new_session=True)
  try:code=p.wait(timeout=240)
  except (subprocess.TimeoutExpired,KeyboardInterrupt):
   os.killpg(p.pid,signal.SIGTERM)
   try:p.wait(timeout=5)
   except subprocess.TimeoutExpired:os.killpg(p.pid,signal.SIGKILL);p.wait()
   raise
 text=(DEST/'installation-serial.log').read_text(errors='replace')
 if code or 'VEYNODE_INSTALL_PASS' not in text or 'VEYNODE_INSTALL_FAIL' in text:raise RuntimeError('Installer failed; see installation-serial.log')
 config={'id':args.name,'guest':'linux','cpus':2,'memory_mib':1024,'kernel':str(MEDIA/'Image'),'initrd':str(MEDIA/'veynode-initramfs.gz'),'command_line':'console=hvc0 root=/dev/vda rootfstype=ext4 modules=ext4,virtio_blk','disk':str(disk),'window':False,'serial_log':str(DEST/'boot-serial.log')}
 (DEST/'config.json').write_text(json.dumps(config,indent=2)+'\n');status['state']='installed-unbooted';print('Installed persistent Guest: '+str(DEST/'config.json'))
except Exception as e:status.update(state='failed',error=str(e));raise
finally:status['finished_at_unix']=time.time();(DEST/'installation-status.json').write_text(json.dumps(status,indent=2)+'\n')
