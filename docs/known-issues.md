# Known issues

## Networking

- **DNS when host resolvers listen only on loopback.** Apple NAT hands guests
  the gateway (`192.168.65.1`) as resolver; its forwarder cannot reach host
  resolvers bound only to `127.x` (for example local DNS-over-HTTPS proxies),
  so lookups time out. Set explicit servers per container (`--dns 1.1.1.1`),
  for the engine (`veynodevm engine start --dns 1.1.1.1`), or as the default
  in runtime.json (`scripts/install-dev-runtime.py --dns 1.1.1.1`).
- **Engine VM MTU.** Inside the engine VM, inbound TCP segments larger than
  about 1300 bytes were observed to be lost on Apple NAT (TLS handshakes stall
  after connect; MTU 1280 works, 1400 does not), while plain micro-VM
  containers on the same host worked at 1500. Root cause not yet isolated:
  iptables flush, removing docker0/br_netfilter and conntrack checksum or
  liberal settings did not change it. The engine therefore defaults to MTU
  1280 for its NIC and dockerd bridges (`--mtu` overrides). Plain containers
  keep 1500 unless `--mtu` or runtime.json `mtu` is set.

## Containers

- `exec` is bounded (30 s, 64 KiB output per stream) and not interactive;
  `-i/-t` on `run` are accepted but stdin is not connected yet.
- Named volumes are not implemented; use host bind mounts (`-v /host:/path`).
  Image `VOLUME`s become anonymous directories on the container's disk.
- Apple Virtualization publishing (`-p`) is TCP only; UDP needs QEMU.
- Hard links whose target is hidden by a later layer are skipped during
  flattening (reported as a warning).

## GPU and desktop

- The QEMU/HVF GPU path (`--gpus`, `--desktop`, engine `--gpus`) needs the
  locally fetched UTM runtime (`runtime/fetch-utm.sh`) and is not exercised by
  the default checks. Venus is experimental (see archive/docs-logs).
