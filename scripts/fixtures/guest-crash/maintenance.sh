#!/bin/sh
# Test-only isolated clone update; never changes the existing agent or permissions.
set -eu
# rdinit runs as PID 1. Refuse ordinary host/CI invocation before even installing
# the failure trap: that trap intentionally powers down its disposable Guest.
test "$$" -eq 1
test -d /veynode/crash && test ! -L /veynode/crash
for payload in watcher config.json preserved.sha256 payload.sha256 watcher.openrc; do
    test -f "/veynode/crash/$payload" && test ! -L "/veynode/crash/$payload"
done
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
test "$(uname -s)" = Linux
test "$(uname -m)" = aarch64
stage=init
failed() { echo "VEYNODE_CRASH_MAINTENANCE_FAIL stage=$stage"; sync; poweroff -f; }
trap failed EXIT
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
grep -Eq '(^| )veynode.crash_maintenance=1( |$)' /proc/cmdline
awk '$2 == "/" && ($3 == "rootfs" || $3 == "tmpfs") {ok=1} END {exit !ok}' /proc/mounts
test -e /lib/modules || ln -s /usr/lib/modules /lib/modules
modprobe virtio_blk
modprobe ext4
mkdir -p /newroot
cd /veynode/crash
sha256sum -c payload.sha256
stage=preflight
mount -t ext4 -o ro,noload /dev/vda /newroot
test "$(cat /newroot/etc/alpine-release)" = 3.23.6
cd /newroot
sha256sum -c /veynode/crash/preserved.sha256
test ! -e opt/veynode-crash
test ! -e var/lib/veynode-crash-once
test ! -e etc/init.d/veynode-crash-once
test ! -e etc/runlevels/default/veynode-crash-once
awk -F: '$1 == "veynode" && $3 == 1000 && $4 == 1000 {n++} END {exit n!=1}' etc/passwd
grep -Fxq 'rc_crashed_start="NO"' etc/rc.conf
cd /
umount /newroot
stage=add-isolated-watcher
mount -t ext4 /dev/vda /newroot
mkdir -m 700 /newroot/opt/veynode-crash
cp /veynode/crash/watcher /veynode/crash/config.json /newroot/opt/veynode-crash/
chmod 700 /newroot/opt/veynode-crash/watcher
chmod 600 /newroot/opt/veynode-crash/config.json
mkdir -m 711 /newroot/var/lib/veynode-crash-once
mkdir -m 700 /newroot/var/lib/veynode-crash-once/state
# Root owns the parent: UID1000 may write this existing file but cannot replace it.
(umask 077; : > /newroot/var/lib/veynode-crash-once/arm)
chown 1000:1000 /newroot/var/lib/veynode-crash-once/arm
chmod 600 /newroot/var/lib/veynode-crash-once/arm
cp /veynode/crash/watcher.openrc /newroot/etc/init.d/veynode-crash-once
chmod 755 /newroot/etc/init.d/veynode-crash-once
ln -s /etc/init.d/veynode-crash-once /newroot/etc/runlevels/default/veynode-crash-once
cd /newroot
sha256sum -c /veynode/crash/preserved.sha256
sha256sum opt/veynode-crash/watcher etc/init.d/veynode-crash-once
sync
cd /
umount /newroot
trap - EXIT
echo VEYNODE_CRASH_MAINTENANCE_PASS
poweroff -f
