# GPU userspace for QEMU/HVF containers: virgl (OpenGL) and Venus (Vulkan,
# Ubuntu's mesa-vulkan-drivers includes the virtio driver).
#   veynodevm build -t veynodevm/ubuntu-gpu guest/images/ubuntu-gpu
#   veynodevm run --rm --gpus venus veynodevm/ubuntu-gpu            # vulkaninfo
#   veynodevm run --rm --gpus virgl -e EGL_PLATFORM=surfaceless veynodevm/ubuntu-gpu eglinfo -B
FROM ubuntu:24.04
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      mesa-vulkan-drivers libgl1-mesa-dri libegl1 mesa-utils vulkan-tools ca-certificates \
 && rm -rf /var/lib/apt/lists/*
ENV VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.json
CMD ["vulkaninfo", "--summary"]
