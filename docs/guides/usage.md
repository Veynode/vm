# Using VeynodeVM

Every `veynodevm` command is a client of the control daemon's local API 1. Build and start it first:

```sh
cargo build -p veynodevm -p vm-control && ./scripts/build-native.sh
python3 guest/kernel/build.py                  # pinned kernel + initramfs with the agent as PID 1
python3 scripts/install-dev-runtime.py --dns 1.1.1.1
veynodevm daemon start                         # launchd user agent
```

State lives in `~/Library/Application Support/VeynodeVM` (override with `VEYNODEVM_HOME`).

## Containers

One micro-VM per container:

```sh
veynodevm run --rm alpine echo hi
veynodevm run -d -p 8080:80 -v $PWD:/data:ro --name web nginx:alpine
veynodevm ps -a; veynodevm logs -f web; veynodevm exec web nginx -v
veynodevm stop web && veynodevm rm web && veynodevm image gc
```

## Shared docker engine

```sh
veynodevm engine start --share ~/src            # shared docker-compatible VM (add --gpus venus for a GPU)
docker --context veynodevm compose up -d
veynodevm run --shared -p 8081:80 nginx:alpine
veynodevm build -t myimg .                     # build in the engine, import for micro-VMs
```

## Desktops and GPUs

GPUs need the QEMU/HVF runtime; see [building](building.md#gpu-runtime).

```sh
veynodevm run -d --desktop --name desk veynodevm/alpine-desktop
veynodevm run -d --desktop --gpus venus --name vkdesk veynodevm/ubuntu-desktop   # Vulkan desktop
veynodevm desktop desk                         # native AppKit window
veynodevm ui                                   # web UI; container page → Open desktop in browser
veynodevm run --rm --gpus venus veynodevm/ubuntu-gpu   # vulkaninfo: Virtio-GPU Venus (Apple M…)
veynodevm update --gpus virgl --restart desk   # add, change or drop (none) a GPU later; keeps the disk
veynodevm update --cpus 4 -m 4g --disk-size 32 -p 8080:80 --restart desk   # hardware later, one restart; disks only grow
```

See also the [browser desktop](../browser-desktop.md).

## macOS containers

Tart-compatible OCI images on Apple Virtualization:

```sh
veynodevm pull ghcr.io/cirruslabs/macos-tahoe-vanilla   # macOS guest (~22 GB download, ~26 GB on disk)
veynodevm run -d --name mac --disk-size 100 -v $PWD:/Users/Shared/src ghcr.io/cirruslabs/macos-tahoe-vanilla
veynodevm exec mac sw_vers; veynodevm cp ./tool mac:/Users/Shared/tool; veynodevm desktop mac
veynodevm run -it --rm ghcr.io/cirruslabs/macos-tahoe-vanilla   # throwaway macOS login shell
```

## Full system containers

systemd as PID 1, GDM logs in to GNOME (Ubuntu 24.04):

```sh
veynodevm build -t veynodevm/ubuntu-gnome guest/images/ubuntu-gnome
veynodevm run -d --init systemd --desktop --name gnome veynodevm/ubuntu-gnome   # add --gpus virgl for QEMU/virgl
veynodevm run -d --desktop --desktop-size 1920x1080 --gpus virgl --name big veynodevm/ubuntu-gnome   # QEMU desktop size, default 1280x800
veynodevm update --desktop-size 1280x800 --restart big   # resize later; --desktop on|off adds or removes the display
veynodevm desktop gnome; veynodevm exec gnome systemctl status gdm
veynodevm stop gnome                           # systemctl poweroff inside the guest
```

## Checkpoints and forks

Linux containers: APFS clones of the writable layer, plus memory on Apple Virtualization (`--disk-only` to skip it).

```sh
veynodevm checkpoint create web before-upgrade # ~0.25 s for a 512 MiB guest, no disk data copied
veynodevm rollback web before-upgrade          # restore it with processes running (~0.4 s; --cold boots it)
veynodevm fork -n 3 --checkpoint before-upgrade web   # 3 containers, fresh id/hostname/MAC/ports
veynodevm fork rm web                          # remove every fork of web
```

## Web UI and MCP

```sh
veynodevm ui                                   # TanStack Start web UI on 127.0.0.1, opens the browser
veynodevm mcp --lifecycle                      # stdio MCP server
veynodevm mcp --input --tools core,screen,input  # list only some tool groups
```

See [computer use](../computer-use.md) for the MCP desktop tools and the [low-level control path](low-level-control.md) for the daemon's own MCP server.
