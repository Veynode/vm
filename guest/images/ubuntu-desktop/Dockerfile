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
      at-spi2-core \
 && rm -rf /var/lib/apt/lists/*
# The desktop session's user (uid 1000); Xorg alone runs as root. input is
# for uinput, which cua-driver's background pointer uses.
RUN userdel -r ubuntu \
 && useradd -m -u 1000 -s /bin/bash -G input,video,render veynode \
 && echo 'KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"' > /etc/udev/rules.d/70-veynodevm-uinput.rules
# cua-driver, the Guest's computer-use helper, pinned in runtime/cua-driver.json.
# The agent starts it on first use as the session user (docs/computer-use.md).
ADD --checksum=sha256:ed8a51125ade8807c7717c3dde13d62bd31e6275e6b5e2446cf018d0b01d93b3 \
    https://github.com/trycua/cua/releases/download/cua-driver-rs-v0.38.0/cua-driver-rs-0.38.0-linux-arm64.tar.gz /tmp/cua-driver.tar.gz
RUN tar -xzf /tmp/cua-driver.tar.gz -C /tmp \
 && echo "26e9eee8165e9ce7b614b5e3d68e5d963231385a82b65236406718b5ce1ff936  /tmp/cua-driver-rs-0.38.0-linux-arm64/cua-driver" | sha256sum -c - \
 && install -m 0755 /tmp/cua-driver-rs-0.38.0-linux-arm64/cua-driver /usr/local/bin/cua-driver \
 && install -D -m 0644 -t /usr/share/doc/cua-driver /tmp/cua-driver-rs-0.38.0-linux-arm64/LICENSE /tmp/cua-driver-rs-0.38.0-linux-arm64/THIRD_PARTY_NOTICES.md \
 && rm -rf /tmp/cua-driver.tar.gz /tmp/cua-driver-rs-0.38.0-linux-arm64
COPY desktop/veynodevm-desktop /usr/local/bin/veynodevm-desktop
COPY desktop/10-veynodevm.conf /etc/X11/xorg.conf.d/10-veynodevm.conf
ENV VEYNODEVM_DESKTOP_USER=veynode VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.json
CMD ["/usr/local/bin/veynodevm-desktop"]
