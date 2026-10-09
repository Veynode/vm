#!/usr/bin/env python3
"""Prepare a fresh RAM Xorg/Zink/Venus WSI diagnostic; never starts a VM."""
import argparse
import gzip
import json
import os
from pathlib import Path
import subprocess
import importlib.util
ROOT=Path(__file__).resolve().parents[1]
def load(name,path):
 s=importlib.util.spec_from_file_location(name,path);m=importlib.util.module_from_spec(s);s.loader.exec_module(m);return m
C=load('wsi_control_prepare',ROOT/'scripts/linux-vulkan-prepare-control.py')

WSI=r'''
# Same repository/Mesa version throughout; no software fallback accepted.
apk add --no-cache --no-progress mesa-utils mesa-dri-gallium=26.1.6-r0 mesa-egl=26.1.6-r0 mesa-gles=26.1.6-r0 mesa-gbm=26.1.6-r0 xorg-server=21.1.25-r0 xf86-input-libinput twm font-dejavu eudev xwininfo xdpyinfo xwd coreutils || fail wsi_packages
printf 'WSI_PACKAGES_BEGIN\n'; apk info -v; printf 'WSI_PACKAGES_END\n'
mkdir -p /dev/pts /root/.config
mount -t devpts devpts /dev/pts || fail devpts
udevd --daemon || fail udev
udevadm trigger && udevadm settle || fail udev_devices
export DISPLAY=:0 HOME=/root XDG_CONFIG_HOME=/root/.config
export MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink
cat > /veynode/xorg.conf <<'XORG'
Section "Device"
 Identifier "Virtio"
 Driver "modesetting"
 Option "AccelMethod" "glamor"
EndSection
Section "ServerFlags"
 Option "AutoAddDevices" "false"
EndSection
XORG
printf 'RandomPlacement\nNoGrabServer\n' > /root/.twmrc
Xorg :0 vt1 -config /veynode/xorg.conf -ac -noreset -nolisten tcp -logfile /veynode/Xorg.log > /run/xorg.stdout 2>&1 &
xorg=$!
i=0
while ! xdpyinfo >/run/xdpyinfo 2>&1; do
 kill -0 "$xorg" || fail xorg_dead
 i=$((i+1)); [ "$i" -lt 80 ] || fail xorg_deadline
 sleep .25
done
printf 'WSI_XORG_BEGIN\n'; cat /veynode/Xorg.log; printf 'WSI_XORG_END\n'
# This is an independent desktop context, not the application's Vulkan identity.
timeout -s KILL 10 glxinfo -B > /run/glxinfo 2>&1 || fail gl_identity
printf 'WSI_GL_BEGIN\n'; cat /run/glxinfo; printf 'WSI_GL_END\n'
grep -qi 'zink' /run/glxinfo || fail gl_not_zink
grep -q 'Apple M1 Pro' /run/glxinfo || fail gl_not_apple
grep -q 'Accelerated: yes' /run/glxinfo || fail gl_not_accelerated
if grep -Ei 'llvmpipe|lavapipe|softpipe|SwiftShader' /run/glxinfo; then fail software_gl; fi
grep -q 'glamor initialized' /veynode/Xorg.log || fail xorg_glamor
xdpyinfo -queryExtensions > /run/extensions || fail x_extensions
cat /run/extensions
grep -q DRI3 /run/extensions && grep -q Present /run/extensions || fail dri3_present
[ -f /usr/lib/vkmark/xcb.so ] || fail xcb_plugin
twm -f /root/.twmrc > /run/twm.log 2>&1 &
# Pinned official application, explicit XCB surface/swapchain. No headless fallback.
printf 'WSI_CASE_BEGIN shading-phong\n'
timeout -s KILL 75 stdbuf -oL -eL vkmark --debug --winsys xcb --size 800x600 --present-mode fifo -b shading:duration=60.0:shading=phong > /run/vkmark.log 2>&1 &
app=$!
i=0
while :; do
 kill -0 "$app" || fail app_before_window
 xwininfo -root -tree > /run/windows 2>&1 || fail window_query
 if grep -q '"vkmark ' /run/windows && grep -q 'Swapchain contains' /run/vkmark.log; then break; fi
 i=$((i+1)); [ "$i" -lt 60 ] || fail swapchain_deadline
 sleep .25
done
cat /run/windows
cat /run/vkmark.log
printf 'VEYNODE_WSI_WINDOW_READY\n'
wait "$app"; rc=$?
printf 'WSI_APP_LOG_BEGIN\n'; cat /run/vkmark.log; printf 'WSI_APP_LOG_END\n'
printf 'WSI_CASE_EXIT shading-phong %s\n' "$rc"
[ "$rc" -eq 0 ] || fail vkmark
printf 'VEYNODE_WSI_WORKLOAD_DONE\n'
'''

def guest_script():
 init=C.A.ORIGINAL_INIT.replace('#!/bin/sh\n','#!/bin/sh\n[ "$$" -eq 1 ] || exit 90\n',1)
 old="fail() { printf '\\nVULKAN_SETUP_FAIL %s\\n' \"$1\"; poweroff -f; exit 1; }"
 new="""fail() { printf '\\nVULKAN_SETUP_FAIL %s\\n' "$1"; for f in /veynode/Xorg.log /run/xorg.stdout /run/glxinfo /run/vkmark.log; do if [ -f "$f" ]; then printf 'WSI_FAILURE_LOG %s\\n' "$f"; cat "$f"; fi; done; sync; poweroff -f; exit 1; }"""
 if init.count(old)!=1:raise ValueError('failure anchor changed')
 init=init.replace(old,new)
 init=init.replace('modprobe virtio_gpu || fail virtio_gpu','modprobe virtio_gpu || fail virtio_gpu\nmodprobe evdev || fail evdev\nmodprobe button || fail button\n'+C.SERIAL_INIT)
 init=init.replace("printf '\\nVULKAN_PACKAGES_BEGIN\\n'", "apk add --force-non-repository --no-cache --no-progress /veynode/vkmark.apk || fail vkmark_package\nprintf '\\nVULKAN_PACKAGES_BEGIN\\n'")
 init=init.replace('sync\npoweroff -f','[ "$rc" -eq 0 ] || fail instance\n'+WSI+'\n'+C.END.replace('VEYNODE_CONTROL_VENUS_READY','VEYNODE_WSI_READY'))
 return init

def prepare(out,vm):
 # Isolated imported module; original files/frozen artifacts unchanged.
 C.guest_script=guest_script
 C.prepare(out,ROOT/'.local/linux-vulkan-host-runtime-v1/manifest.json',ROOT/'.local/qemu-observer-guest-v1/veynode-agent-r3',vm)
 config=json.loads((out/'config.json').read_text());config.update(window=True,width=1280,height=800,memory_mib=2048)
 (out/'config.json').write_text(json.dumps(config,indent=2)+'\n')
 init=guest_script();subprocess.run(['/bin/sh','-n'],input=init,text=True,check=True)
 # C.prepare injected the overridden script already; source assertions validate identity.
 assert (out/'guest/guest-init.sh').read_text()==init
 report=json.loads((out/'fixture.json').read_text())
 report['inputs']=[x for x in report['inputs'] if x['path']!=str(out/'config.json')]
 for p in [Path(__file__),ROOT/'scripts/linux-vulkan-wsi-test.py',out/'config.json']:
  report['inputs'].append({'path':str(p),'sha256':C.A.base.digest(p)})
 report['limits']=['Fresh RAM Guest only; no persistent/native Vulkan acceptance until actual run',
 'Xorg Zink/Venus GL identity and vkmark Vulkan identity are separate guards',
 'Official Mesa26 has no virgl; no software fallback or mixed Mesa version is accepted',
 'Application has no full-frame reference oracle; screenshot and actual native-window observation are required separately',
 'Single60s Phong scene is a compatibility workload, not a performance comparison',
 'Transitive APK dependencies resolved at run time; actual versions recorded, not fully pinned']
 report['wsi']={'window_system':'xcb','size':[800,600],'duration_seconds':60,'hard_timeout_seconds':75,'desktop':'Xorg modesetting/glamor via explicit Zink/Venus'}
 (out/'fixture.json').write_text(json.dumps(report,indent=2)+'\n')
 return report
if __name__=='__main__':
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('--output-dir',type=Path,required=True);p.add_argument('--id',required=True);a=p.parse_args();os.umask(0o077)
 prepare(a.output_dir.absolute(),a.id);print(json.dumps({'prepared':True,'vm_run':False}))
