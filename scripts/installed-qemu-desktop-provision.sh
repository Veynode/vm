#!/bin/sh
# RAM installer for a NEW vm-image clone; the host worker must hold the disk guard.
set -eu
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
stage=init
fail() { echo "VEYNODE_INSTALLED_PROVISION_FAIL stage=$stage"; sync; poweroff -f; }
trap fail EXIT
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /newroot /run /tmp
mount -t tmpfs tmpfs /tmp
test -e /lib/modules || ln -s /usr/lib/modules /lib/modules
modprobe virtio_blk
modprobe ext4
modprobe virtio_net
ip link set lo up
ip link set eth0 up
udhcpc -i eth0 -n -q -t 5 -s /veynode/dhcp
stage=fresh-disk
# The host bundle permits only its new O_EXCL sparse disk, never an imported clone.
test "$(cat /sys/class/block/vda/size)" = 33554432
head -c 1048576 /dev/vda > /tmp/disk-prefix
printf '30e14955ebf1352266dc2ff8067e68104607e750abb9d3b36582b8af909fcb58  /tmp/disk-prefix\n' | sha256sum -c -
apk add --no-cache --no-progress e2fsprogs
mkfs.ext4 -F -L veynode-desktop /dev/vda
mount -t ext4 /dev/vda /newroot
tar -xzf /veynode/rootfs.tar.gz -C /newroot
test "$(cat /newroot/etc/alpine-release)" = 3.23.6
test ! -e /newroot/opt/veynode-fixture
mkdir -p /newroot/opt/veynode-fixture /newroot/var/lib/veynode-fixture
chmod 700 /newroot/opt/veynode-fixture /newroot/var/lib/veynode-fixture
cp -a /veynode/payload/. /newroot/opt/veynode-fixture/
cp /etc/resolv.conf /newroot/etc/resolv.conf
mkdir -p /newroot/usr/lib/modules
cp -a /usr/lib/modules/. /newroot/usr/lib/modules/
ln -s /usr/lib/modules /newroot/lib/modules
for fs in proc sys dev; do mount --bind /$fs /newroot/$fs; done
stage=packages-and-module
chroot /newroot /bin/sh /opt/veynode-fixture/install.sh
stage=unmount
for fs in dev sys proc; do umount /newroot/$fs; done
sync
umount /newroot
trap - EXIT
echo VEYNODE_INSTALLED_PROVISION_PASS
# RAM provisioning is not installed-Guest normal shutdown acceptance.
poweroff -f
