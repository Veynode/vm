#!/usr/bin/env python3
"""Prepare only: reviewed local runtime registration and fresh RAM Venus appliance.
No daemon/VM is launched, no existing disk/base is attached or changed.
"""
import argparse
import gzip
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess
ROOT=Path(__file__).resolve().parents[1]
def load(name,path):
 s=importlib.util.spec_from_file_location(name,path);m=importlib.util.module_from_spec(s);s.loader.exec_module(m);return m
A=load('vkmark_control_prepare',ROOT/'scripts/linux-vulkan-application-probe.py')
F=load('cpio_control_prepare',ROOT/'scripts/build-linux-fixture.py')
ORIGINAL_GUEST_SCRIPT=A.guest_script
AGENT_SHA='8410f79979b9433b81a454a045abaec2757869986cbd2c9347cc1ff9923ff579'

def pinned(path,expected,limit=128*1024*1024):
 path=Path(path)
 if not path.is_absolute() or path.resolve()!=path:raise ValueError('noncanonical artifact')
 fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
 try:
  before=os.fstat(fd)
  if not stat.S_ISREG(before.st_mode) or before.st_nlink!=1 or not 0<before.st_size<=limit or before.st_mode&0o022 or before.st_uid not in (0,os.getuid()):raise ValueError('unsafe artifact')
  digest=hashlib.sha256();count=0
  while True:
   b=os.read(fd,min(65536,limit+1-count))
   if not b:break
   count+=len(b)
   if count>limit:raise ValueError('artifact exceeds bound')
   digest.update(b)
  after=os.fstat(fd);named=os.lstat(path)
  if (before.st_dev,before.st_ino,before.st_size,before.st_mtime_ns)!=(after.st_dev,after.st_ino,after.st_size,after.st_mtime_ns) or (before.st_dev,before.st_ino)!=(named.st_dev,named.st_ino) or count!=before.st_size or digest.hexdigest()!=expected:raise ValueError('artifact pin mismatch')
 finally:os.close(fd)
 return {'path':str(path),'sha256':expected}

def registration(runtime,catalog):
 app=(ROOT/'.local/runtime/utm/UTM.app/Contents').resolve();frameworks=app/'Frameworks'
 roles={'loader':Path(runtime['loader']),'host_library':Path(runtime['host_library']),
 'render_server':Path(runtime['binary']), 'qemu_library':frameworks/'qemu-aarch64-softmmu.framework/Versions/A/qemu-aarch64-softmmu',
 'spice_library':ROOT/'.local/spice-reconnect-build/runtime/libspice-server-fixed.dylib',
 'icd':app/'Resources/vulkan/icd.d/MoltenVK_icd.json','firmware':app/'Resources/qemu/edk2-aarch64-code.fd'}
 result={'schema':catalog['schema'],'profile_revision':catalog['profile_revision'],'frameworks_dir':str(frameworks)}
 if set(roles)!=set(catalog['roles']):raise ValueError('catalog role contract changed')
 for name,path in roles.items():result[name]=pinned(path.resolve(),catalog['roles'][name])
 for relative,sha in catalog['frameworks'].items():pinned(frameworks/relative,sha)
 return result

SERIAL_INIT='''# Exact fixed observer port only; no TCP listener or side-effect scopes.
count=0
for entry in /sys/class/virtio-ports/*; do
 [ -f "$entry/name" ] || continue
 [ "$(cat "$entry/name")" = org.veynode.agent.0 ] || continue
 port=${entry##*/}
 case "$port" in vport[0-9]*p[0-9]*) ;; *) fail observer_port_name;; esac
 [ -c "/dev/$port" ] || fail observer_port_device
 chown 0:0 "/dev/$port" && chmod 600 "/dev/$port" || fail observer_permissions
 mkdir -p /dev/virtio-ports
 ln -s "/dev/$port" /dev/virtio-ports/org.veynode.agent.0 || fail observer_link
 count=$((count+1))
done
[ "$count" -eq 1 ] || fail observer_port_count
/veynode/observer-agent --virtio-serial > /run/observer.log 2>&1 &
observer=$!
printf 'VEYNODE_VENUS_OBSERVER_STARTED\\n'
'''
END='''kill -0 "$observer" || fail observer_dead
acpid -d > /run/acpid.log 2>&1 &
printf 'VEYNODE_CONTROL_VENUS_READY\\n'
# A bounded outer host test requests the real ACPI button; PID1 runs shutdown.
exec /sbin/init'''

def guest_script():
 init=ORIGINAL_GUEST_SCRIPT().replace("#!/bin/sh\n", "#!/bin/sh\n[ \"$$\" -eq 1 ] || exit 90\n",1)
 old='vulkan-loader=1.4.347-r0 ||'
 if init.count(old)!=1:raise ValueError('package anchor changed')
 init=init.replace(old,'vulkan-loader=1.4.347-r0 gcc musl-dev vulkan-headers glslang ||')
 init=init.replace('modprobe virtio_gpu || fail virtio_gpu','modprobe virtio_gpu || fail virtio_gpu\nmodprobe evdev || fail evdev\nmodprobe button || fail button\n'+SERIAL_INIT)
 pixel=A.base.PIXEL_RUN.split('sync\npoweroff -f')[0]
 anchor='[ "$rc" -eq 0 ] || fail instance\n'
 if init.count(anchor)!=1:raise ValueError('instance anchor changed')
 init=init.replace(anchor,pixel+'\n',1)
 if init.count('sync\npoweroff -f')!=1:raise ValueError('shutdown anchor changed')
 return init.replace('sync\npoweroff -f',END)

def prepare(out,runtime_path,agent,vm_id):
 if not vm_id or len(vm_id)>48 or not all(c.isascii() and (c.isalnum() or c in '-_') for c in vm_id):raise ValueError('fixture ID')
 catalog_path=ROOT/'crates/control/src/qemu_venus_catalog.json'
 catalog=json.loads(catalog_path.read_text());runtime=json.loads(runtime_path.read_text())
 reg=registration(runtime,catalog);pinned(agent,AGENT_SHA,8*1024*1024)
 out.mkdir(mode=0o700,parents=True,exist_ok=False)
 (out/'venus-runtime.json').write_text(json.dumps(reg,indent=2)+'\n');os.chmod(out/'venus-runtime.json',0o600)
 # Reuse pinned upstream APK and exact accepted scene parser, in an isolated module.
 A.guest_script=guest_script
 A.prepare(out/'guest',runtime_path,ROOT/'.local/linux-vulkan-application-source')
 init=guest_script()
 entries=[]
 for ext in ['c','vert','frag']:entries.append(F.entry('veynode/triangle.'+ext,(ROOT/'scripts'/('linux-vulkan-triangle.'+ext)).read_bytes(),0o100644))
 entries.append(F.entry('veynode/observer-agent',agent.read_bytes(),0o100755))
 modloop=A.base.MEDIA/'modloop-virt';version='6.18.52-0-virt'
 deps=dict(x.split(':',1) for x in subprocess.check_output(['unsquashfs','-cat',str(modloop),f'modules/{version}/modules.dep'],text=True).splitlines());modules=set()
 def include(name):
  if name in modules:return
  modules.add(name)
  for dep in deps[name].split():include(dep)
 for name in ['kernel/drivers/acpi/button.ko','kernel/drivers/input/evdev.ko']:include(name)
 directories=set()
 for name in sorted(modules):
  target=Path(f'usr/lib/modules/{version}/{name}')
  for parent in reversed(target.parents):
   if str(parent)!='.' and parent not in directories:directories.add(parent);entries.append(F.entry(str(parent),b'',0o40755))
  entries.append(F.entry(str(target),subprocess.check_output(['unsquashfs','-cat',str(modloop),f'modules/{version}/{name}'])))
 for name,data,mode in [('etc/acpi',b'',0o40755),('etc/acpi/PWRF',b'',0o40755),('etc/acpi/PWRF/00000080',b'#!/bin/sh\nprintf "VEYNODE_VENUS_ACPI_BUTTON\\n" >/dev/console\n/sbin/poweroff\n',0o100755),('etc/inittab',b'::sysinit:/bin/true\n::shutdown:/veynode/shutdown\n',0o100644),('veynode/shutdown',b'#!/bin/sh\nprintf "VEYNODE_VENUS_NORMAL_SHUTDOWN\\n" >/dev/console\nsync\n',0o100755)]:entries.append(F.entry(name,data,mode))
 entries.append(F.entry('TRAILER!!!',b''))
 with (out/'guest/initramfs.gz').open('ab') as f:f.write(gzip.compress(b''.join(entries),mtime=0))
 subprocess.run(['/bin/sh','-n'],input=init,text=True,check=True)
 config={'id':vm_id,'backend':'qemu-hvf','guest':'linux','cpus':2,'memory_mib':1536,'window':False,
 'qemu_graphics_profile':'venus-moltenvk-experimental',
 'kernel':str(A.base.MEDIA/'Image'),'initrd':str(out/'guest/initramfs.gz'),
 'command_line':'console=ttyAMA0 rdinit=/veynode/vulkan-init','serial_log':str(out/'serial.log')}
 (out/'config.json').write_text(json.dumps(config,indent=2)+'\n')
 inputs=[Path(__file__),ROOT/'scripts/linux-vulkan-test-control.py',*[ROOT/'scripts'/('linux-vulkan-triangle.'+ext) for ext in ['c','vert','frag']],ROOT/'scripts/linux-vulkan-probe.py',ROOT/'scripts/linux-vulkan-application-probe.py',ROOT/'scripts/build-linux-fixture.py',catalog_path,agent,modloop,A.base.MEDIA/'Image',out/'guest/initramfs.gz',out/'guest/guest-init.sh',out/'venus-runtime.json',out/'config.json']
 report={'schema':1,'vm_run':False,'disk_attached':False,'observer_agent_sha256':AGENT_SHA,'registration':{'schema':reg['schema'],'profile_revision':reg['profile_revision']},'inputs':[{'path':str(p),'sha256':A.base.digest(p)} for p in inputs],
 'limits':['vkmark uses headless winsys: no native Vulkan presentation or screenshot claim','triangle asserts10566pixels; vkmark has no image oracle or performance comparison','runtime/API admission is host proof; Guestinfo and actual GPU outputs require the later live test','APK transitive dependencies remain repository-resolved and must be recorded from the actual run']}
 (out/'fixture.json').write_text(json.dumps(report,indent=2)+'\n');return report

def main():
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('--output-dir',type=Path,required=True);p.add_argument('--id',required=True)
 p.add_argument('--runtime-manifest',type=Path,default=ROOT/'.local/linux-vulkan-host-runtime-v1/manifest.json');p.add_argument('--agent',type=Path,default=ROOT/'.local/qemu-observer-guest-v1/veynode-agent-r3');a=p.parse_args();os.umask(0o077)
 r=prepare(a.output_dir.absolute(),a.runtime_manifest.resolve(),a.agent.resolve(),a.id);print(json.dumps({'prepared':True,'vm_run':r['vm_run']}))
if __name__=='__main__':main()
