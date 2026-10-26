# Ubuntu 24.04 GNOME desktop as a full system: systemd is PID 1, GDM logs the
# "ubuntu" user in automatically on Xorg (software rendering through llvmpipe
# on Apple Virtualization, virgl with --gpus virgl).
#   veynodevm build -t veynodevm/ubuntu-gnome guest/images/ubuntu-gnome
#   veynodevm run -d --init systemd --desktop -m 4g --name gnome veynodevm/ubuntu-gnome
#   veynodevm desktop gnome
# Needs --init systemd: the image's CMD is /sbin/init.
FROM ubuntu:24.04
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      systemd systemd-sysv systemd-resolved udev kmod dbus dbus-user-session polkitd dconf-cli \
      gdm3 gnome-shell gnome-session ubuntu-session ubuntu-settings gnome-settings-daemon \
      gnome-shell-extension-ubuntu-dock gnome-shell-extension-appindicator \
      gnome-terminal nautilus gnome-control-center gnome-text-editor \
      yaru-theme-gnome-shell yaru-theme-gtk yaru-theme-icon fonts-ubuntu fonts-dejavu-core \
      xserver-xorg-core xserver-xorg-input-libinput xwayland x11-xserver-utils x11-apps netpbm \
      libgl1-mesa-dri libegl-mesa0 libgbm1 mesa-vulkan-drivers mesa-utils \
      network-manager iproute2 iputils-ping ca-certificates sudo locales less \
 && rm -rf /var/lib/apt/lists/*
COPY rootfs/ /
# Autologin needs no password; "ubuntu" unlocks sudo and the lock screen.
RUN echo 'ubuntu:ubuntu' | chpasswd \
 && usermod -aG sudo,video,render,input ubuntu \
 && dconf update \
 && systemctl set-default graphical.target \
 && systemctl enable gdm.service NetworkManager.service \
 && systemctl mask systemd-networkd-wait-online.service apt-daily.timer apt-daily-upgrade.timer motd-news.timer
CMD ["/sbin/init"]
