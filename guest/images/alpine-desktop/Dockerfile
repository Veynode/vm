# VeynodeVM Linux desktop: Xorg (modesetting on virtio-gpu) + XFCE.
#   veynodevm build -t veynodevm/alpine-desktop guest/images/alpine-desktop
#   veynodevm run -d --desktop --name desk veynodevm/alpine-desktop      # Apple VZ, 2D
#   veynodevm run -d --desktop --gpus virgl --name desk veynodevm/alpine-desktop  # QEMU, GL
#   veynodevm desktop desk
FROM alpine:3.22
RUN apk add --no-cache \
      xorg-server xf86-input-libinput eudev \
      mesa-dri-gallium mesa-vulkan-swrast mesa-utils vulkan-tools \
      xfce4 xfce4-terminal adwaita-icon-theme font-dejavu \
      dbus dbus-x11 xrandr
COPY veynodevm-desktop /usr/local/bin/veynodevm-desktop
COPY 10-veynodevm.conf /etc/X11/xorg.conf.d/10-veynodevm.conf
ENV XDG_RUNTIME_DIR=/run/user/0
CMD ["/usr/local/bin/veynodevm-desktop"]
