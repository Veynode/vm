#!/bin/sh
# Disposable cloned disk only. Run through guarded vm-qemu, never directly on host.
set -eu
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
[ "$(uname -s)" = Linux ]
[ "$(uname -m)" = aarch64 ]
stage=init
failed() { echo "VEYNODE_EXEC_MAINTENANCE_FAIL stage=$stage"; sync; poweroff -f; }
trap failed EXIT
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
grep -Eq '(^| )veynode.exec_fixture_maintenance=1( |$)' /proc/cmdline
# Reject an installed-root invocation; this updater must run from its RAM initrd.
awk '$2 == "/" && ($3 == "rootfs" || $3 == "tmpfs") {found=1} END {exit !found}' /proc/mounts
test -e /lib/modules || ln -s /usr/lib/modules /lib/modules
modprobe virtio_blk
modprobe ext4
mkdir -p /newroot
cd /veynode/exec-v2
sha256sum -c payload.sha256
stage=read-only-preflight
mount -t ext4 -o ro,noload /dev/vda /newroot
test "$(cat /newroot/etc/alpine-release)" = 3.23.6
cd /newroot
sha256sum -c /veynode/exec-v2/previous-files.sha256
test ! -e /newroot/opt/veynode-exec-v2
# A dedicated locked account is created once. Do not adopt an unknown existing UID.
awk -F: '$1 == "veynode" || $3 == 1000 {bad=1} END {exit bad}' etc/passwd
awk -F: '$1 == "veynode" || $3 == 1000 {bad=1} END {exit bad}' etc/group
test ! -e home/veynode
cd /
umount /newroot
stage=write-clone
mount -t ext4 /dev/vda /newroot
mkdir -m 700 /newroot/opt/veynode-exec-v2
cp /veynode/exec-v2/* /newroot/opt/veynode-exec-v2/
chmod 600 /newroot/opt/veynode-exec-v2/*
chroot /newroot /usr/sbin/addgroup -g 1000 veynode
chroot /newroot /usr/sbin/adduser -D -u 1000 -G veynode -h /home/veynode -s /bin/sh veynode
# BEGIN EXACT_SHADOW_LOCK
# BusyBox passwd -l returns nonzero for an already locked account. Accept that
# existing state only by checking exactly one well-formed shadow record first.
test -f /newroot/etc/shadow && test ! -L /newroot/etc/shadow
awk -F: '$1 == "veynode" {count++; if (NF != 9) bad=1} END {exit count != 1 || bad}' /newroot/etc/shadow
shadow_locked() {
    awk -F: '$1 == "veynode" {count++; if (NF != 9 || $2 !~ /^!/) bad=1} END {exit count != 1 || bad}' /newroot/etc/shadow
}
if ! shadow_locked; then
    chroot /newroot /usr/bin/passwd -l veynode
fi
shadow_locked
# END EXACT_SHADOW_LOCK
chroot /newroot /bin/chmod 700 /home/veynode
# No privileged or supplementary groups; the service also clears groups at exec.
chroot /newroot /usr/bin/id veynode
# Apply before installed boot; never unmount a live hybrid cgroup hierarchy.
test -f /newroot/etc/rc.conf && test ! -L /newroot/etc/rc.conf
printf '\n# Dedicated exec-v2 fixture; fail closed without automatic service restart.\nrc_cgroup_mode="unified"\nrc_crashed_start="NO"\n' >> /newroot/etc/rc.conf
cp /veynode/exec-v2/agent /newroot/usr/local/bin/veynode-guest
chmod 755 /newroot/usr/local/bin/veynode-guest
cp /veynode/exec-v2/cgroup.sh /newroot/opt/veynode-exec-v2/cgroup.sh
cp /veynode/exec-v2/agent.openrc /newroot/etc/init.d/veynode-agent
chmod 755 /newroot/opt/veynode-exec-v2/cgroup.sh /newroot/etc/init.d/veynode-agent
# Exact payload and unchanged graphics/ACPI identity are checked again after writes.
cd /newroot/opt/veynode-exec-v2
sha256sum -c payload.sha256
cd /newroot
sha256sum -c /veynode/exec-v2/preserved-files.sha256
sha256sum usr/local/bin/veynode-guest etc/init.d/veynode-agent
awk -F: '$1 == "veynode" && $3 == 1000 && $4 == 1000 && $6 == "/home/veynode" {found++} END {exit found != 1}' etc/passwd
awk -F: '$1 == "veynode" {count++; if (NF != 9 || $2 !~ /^!/) bad=1} END {exit count != 1 || bad}' etc/shadow
stage=unmount
sync
cd /
umount /newroot
trap - EXIT
echo VEYNODE_EXEC_MAINTENANCE_PASS
# This is RAM maintenance completion, not installed normal-shutdown acceptance.
poweroff -f
