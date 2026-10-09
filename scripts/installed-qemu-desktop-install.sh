#!/bin/sh
# Runs inside the new clone chroot only, with pinned same-release Xorg sources.
set -eu
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
cd /opt/veynode-fixture
sha256sum -c payload.sha256
printf 'https://dl-cdn.alpinelinux.org/alpine/v3.23/main\nhttps://dl-cdn.alpinelinux.org/alpine/v3.23/community\n' > /etc/apk/repositories
apk add --no-cache --no-progress alpine-base e2fsprogs xorg-server=21.1.23-r0 mesa-utils mesa-dri-gallium=25.2.7-r1 mesa-egl mesa-gles mesa-demos xwd xf86-input-libinput xterm twm font-dejavu eudev udev-init-scripts-openrc acpid kmod python3 build-base libx11-dev libxrender-dev mesa-dev
apk info -vv | sort > installed-packages.txt
cat installed-packages.txt
mkdir modules-fixed
cp libglamoregl.so modules-fixed/
sha256sum /opt/veynode-fixture/modules-fixed/libglamoregl.so > modules.sha256
cat modules.sha256
cc -O2 -Wall -Wextra xrender-probe.c -o xrender-probe -lX11 -lXrender -lGL
mkdir -p /usr/local/bin
cp agent /usr/local/bin/veynode-guest
chmod 755 /usr/local/bin/veynode-guest
/usr/local/bin/veynode-guest --help 2>&1 | grep -- '--virtio-serial\|-virtio-serial'
depmod "$(uname -r)"
printf 'virtio_net\nvirtio_gpu\nvirtio_input\nevdev\nbutton\n' > /etc/modules
# Fresh installed Alpine; no network login service or unlocked account.
printf '/dev/vda / ext4 defaults 0 1\n' > /etc/fstab
printf 'veynode-desktop\n' > /etc/hostname
printf 'auto lo\niface lo inet loopback\nauto eth0\niface eth0 inet dhcp\n' > /etc/network/interfaces
mkdir -p /etc/runlevels/sysinit /etc/runlevels/boot /etc/runlevels/default /etc/runlevels/shutdown
for service in devfs dmesg udev udev-trigger udev-settle; do ln -s /etc/init.d/$service /etc/runlevels/sysinit/$service; done
for service in bootmisc hostname sysctl modules; do ln -s /etc/init.d/$service /etc/runlevels/boot/$service; done
for service in networking acpid veynode-agent; do ln -s /etc/init.d/$service /etc/runlevels/default/$service; done
for service in mount-ro killprocs savecache; do ln -s /etc/init.d/$service /etc/runlevels/shutdown/$service; done
# Preserve tty1..6 gettys; GUI runs vt7.
sed -i '/^hvc0::/d' /etc/inittab
printf 'ttyAMA0::respawn:/sbin/getty 115200 ttyAMA0\n' >> /etc/inittab
cat > /etc/init.d/veynode-desktop <<'RC'
#!/sbin/openrc-run
name="Veynode installed GPU diagnostic"
command="/bin/sh"
command_args="/opt/veynode-fixture/desktop-boot.sh"
command_background="yes"
pidfile="/run/veynode-desktop.pid"
depend() { need modules localmount udev-settle; before veynode-agent; }
RC
cat > /etc/init.d/veynode-agent <<'RC'
#!/sbin/openrc-run
name="Veynode info-only serial integration"
command="/usr/local/bin/veynode-guest"
command_args="--virtio-serial"
command_background="yes"
pidfile="/run/veynode-agent.pid"
depend() { need modules veynode-desktop; }
start_pre() {
    n=0
    while [ "$n" -lt 90 ]; do
        if [ -f /run/veynode-desktop.ready ] && [ -e /dev/virtio-ports/org.veynode.agent.0 ]; then
            port=$(readlink -f /dev/virtio-ports/org.veynode.agent.0)
            case "$port" in /dev/vport[0-9]*p[0-9]*) ;; *) return 1 ;; esac
            test -c "$port" && test "$(stat -c %u "$port")" = 0 || return 1
            chmod 600 "$port"
            return 0
        fi
        sleep 1
        n=$((n+1))
    done
    eerror "GPU or private serial endpoint not ready"
    return 1
}
RC
chmod 755 /etc/init.d/veynode-agent /etc/init.d/veynode-desktop
ln -s /etc/init.d/veynode-desktop /etc/runlevels/default/veynode-desktop
mkdir -p /etc/acpi/PWRF
cat > /etc/acpi/PWRF/00000080 <<'POWER'
#!/bin/sh
echo VEYNODE_INSTALLED_ACPI_POWER_BUTTON >/dev/console
/sbin/poweroff
POWER
chmod 755 /etc/acpi/PWRF/00000080
# The installed acpid package routes events/anything to this handler, unlike
# the RAM BusyBox fixture's PWRF directory dispatch.
cp acpi-handler.sh /etc/acpi/handler.sh
chmod 755 /etc/acpi/handler.sh
cat > xorg.conf <<'XORG'
Section "Device"
 Identifier "Virtio"
 Driver "modesetting"
 Option "AccelMethod" "glamor"
EndSection
Section "ServerFlags"
 Option "AutoAddDevices" "true"
EndSection
XORG
echo VEYNODE_INSTALLED_CONFIGURED_INFO_ONLY
