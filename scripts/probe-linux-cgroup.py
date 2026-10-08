#!/usr/bin/env python3
"""Actual kernel clone3/cgroup.kill proof in an isolated diskless ARM Guest."""
import gzip, importlib.util, os
from pathlib import Path
import signal, subprocess, tempfile
ROOT=Path(__file__).resolve().parents[1]
MEDIA=ROOT/'.local/media/alpine-3.24.2'
OUT=ROOT/'.local/evidence/cgroup-kernel'
OUT.mkdir(parents=True,exist_ok=True)
spec=importlib.util.spec_from_file_location('fixture',ROOT/'scripts/build-linux-fixture.py');f=importlib.util.module_from_spec(spec);spec.loader.exec_module(f)
subprocess.run(['go','build','-trimpath','-o',str(OUT/'probe'),str(ROOT/'scripts/linux-cgroup-probe.go')],check=True,env={**os.environ,'GOOS':'linux','GOARCH':'arm64','CGO_ENABLED':'0'})
init=b'''#!/bin/sh
set -eu
trap 'echo CGROUP_KERNEL_FAIL; poweroff -f' EXIT
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /tmp /sys/fs/cgroup
ls -ld / /tmp
chmod 755 /
chmod 1777 /tmp
uname -a
cat /proc/filesystems
mount -t cgroup2 -o nosuid,nodev,noexec cgroup2 /sys/fs/cgroup
cat /proc/self/cgroup
cat /sys/fs/cgroup/cgroup.controllers
ls -ld /veynode /veynode/cgroup-probe
cp /veynode/cgroup-probe /tmp/cgroup-probe
chmod 755 /tmp/cgroup-probe
/tmp/cgroup-probe
trap - EXIT
poweroff -f
'''
with tempfile.TemporaryDirectory(prefix='vm-cgroup-probe-') as temp:
 initrd=Path(temp)/'initrd.gz';initrd.write_bytes((MEDIA/'veynode-initramfs.gz').read_bytes()+gzip.compress(f.entry('veynode/cgroup-init',init,0o100755)+f.entry('veynode/cgroup-probe',(OUT/'probe').read_bytes(),0o100755)+f.entry('TRAILER!!!',b''),mtime=0))
 cmd=['/opt/homebrew/bin/qemu-system-aarch64','-machine','virt,accel=hvf','-cpu','host','-smp','2','-m','512','-nodefaults','-display','none','-serial','stdio','-monitor','none','-kernel',str(MEDIA/'Image'),'-initrd',str(initrd),'-append','console=ttyAMA0 rdinit=/veynode/cgroup-init']
 with (OUT/'serial.log').open('w') as log:
  p=subprocess.Popen(cmd,stdin=subprocess.DEVNULL,stdout=log,stderr=log,start_new_session=True)
  try: code=p.wait(timeout=30)
  finally:
   if p.poll() is None:os.killpg(p.pid,signal.SIGKILL);p.wait()
 text=(OUT/'serial.log').read_text(errors='replace')
 print('\n'.join(l for l in text.splitlines() if 'CGROUP_' in l or 'panic:' in l))
 if code or 'CGROUP_CAPABILITY_PROBE_PASS' not in text or 'CGROUP_KERNEL_FAIL' in text:raise SystemExit(1)
