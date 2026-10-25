# XFCE desktop with Vulkan through Venus (Mesa's virtio ICD) and OpenGL
# through virgl. Vulkan applications present to X11 over DRI3.
#   veynodevm build -t veynodevm/ubuntu-desktop -f guest/images/ubuntu-desktop/Dockerfile guest/images
#   veynodevm run -d --desktop --gpus venus --name desk veynodevm/ubuntu-desktop
FROM ubuntu:24.04
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      xserver-xorg-core xserver-xorg-input-libinput x11-xserver-utils udev \
      xfce4 xfce4-terminal dbus dbus-x11 adwaita-icon-theme fonts-dejavu-core \
      mesa-vulkan-drivers libgl1-mesa-dri libegl1 mesa-utils vulkan-tools ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY desktop/veynodevm-desktop /usr/local/bin/veynodevm-desktop
COPY desktop/10-veynodevm.conf /etc/X11/xorg.conf.d/10-veynodevm.conf
ENV XDG_RUNTIME_DIR=/run/user/0 VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.json
CMD ["/usr/local/bin/veynodevm-desktop"]
