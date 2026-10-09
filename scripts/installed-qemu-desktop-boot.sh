#!/bin/sh
# Dedicated installed test fixture only, not a general command execution endpoint.
set -eu
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
export DISPLAY=:0 XDG_RUNTIME_DIR=/run
exec >>/dev/console 2>&1
xpid=; twmpid=; gearpid=; termpid=
cleanup() {
    trap - EXIT TERM INT
    for pid in "$termpid" "$gearpid" "$twmpid" "$xpid"; do
        if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; fi
    done
    for pid in "$termpid" "$gearpid" "$twmpid" "$xpid"; do
        if [ -n "$pid" ]; then wait "$pid" 2>/dev/null || true; fi
    done
    rm -f /run/veynode-desktop.ready
}
trap cleanup EXIT
trap 'exit 0' TERM INT
rm -f /run/veynode-desktop.ready
python3 /opt/veynode-fixture/boot-marker.py
modprobe virtio_gpu
modprobe virtio_input
modprobe evdev
modprobe button
test -S /run/udev/control
udevadm trigger
udevadm settle --timeout=10
mkdir -p /run/veynode-pixels
sha256sum -c /opt/veynode-fixture/modules.sha256
Xorg :0 vt7 -modulepath /opt/veynode-fixture/modules-fixed,/usr/lib/xorg/modules -config /opt/veynode-fixture/xorg.conf -ac -noreset -nolisten tcp -logfile /run/veynode-Xorg.log &
xpid=$!
sleep 4
kill -0 "$xpid"
cat /run/veynode-Xorg.log
glxinfo -B > /run/veynode-glxinfo
cat /run/veynode-glxinfo
grep -q 'Device: virgl' /run/veynode-glxinfo
grep -q 'Accelerated: yes' /run/veynode-glxinfo
grep '/opt/veynode-fixture/modules-fixed/libglamoregl.so' /proc/$xpid/maps
if grep -q '/usr/lib/xorg/modules/libglamoregl.so' /proc/$xpid/maps; then exit 1; fi
set +e
timeout 30 /opt/veynode-fixture/xrender-probe /run/veynode-pixels > /run/veynode-xrender.log 2>&1
status=$?
set -e
cat /run/veynode-xrender.log
test "$status" = 0
test "$(grep -c '^XRENDER_CASE ' /run/veynode-xrender.log)" = 15
test "$(grep '^XRENDER_CASE ' /run/veynode-xrender.log | grep -c '\"passed\":true')" = 15
grep -Fxq 'XRENDER_SUMMARY {"cases":15,"failed":0,"passed":true}' /run/veynode-xrender.log
grep 'VEYNODE_GLAMOR_GLYPH variant=explicit-one' /run/veynode-Xorg.log
apk info -v xorg-server mesa-dri-gallium
twm &
twmpid=$!
xterm -geometry 72x12+20+420 -title 'Veynode installed Linux desktop' -e sh -c 'while true; do date; sleep 1; done' &
termpid=$!
glxgears -geometry 640x400+20+20 &
gearpid=$!
sleep 2
kill -0 "$gearpid"
printf 'ready\n' > /run/veynode-desktop.ready
echo VEYNODE_INSTALLED_GRAPHICS_READY=15
wait "$xpid"
