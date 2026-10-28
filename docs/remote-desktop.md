# Remote desktop

Browser access to a VM's desktop from the LAN or the Internet, for QEMU/HVF and Apple Virtualization. Status: the protocol, the gateway and the host are implemented and checked offline (unit and loopback tests); nothing here has run against a live VM, a real browser, EC2 or Cloudflare yet. See [Not yet](#not-yet). Decision record: [0025](decisions/0025-remote-desktop-transport.md).

```text
Browser ── wss (TLS 443/tcp) ─────┐
        ── WebTransport (443/udp) ─┤  vm-gateway: routes only, no accounts, no session keys
                                   │  (EC2: ACME; LAN: self-signed/files; Cloudflare Tunnel: external)
                                   │
                                   └─ QUIC tunnel (7443/udp), dialed out by the Mac, keys pinned both ways
                                        │
Mac: vm-media-local remote (Keychain host key, handshake, E2E records)
       hub per VM+codec ── desktop.stream {tracks} (API 1) ── vz-desktop-stream / vm-console-stream
```

## Pieces

| Piece | Where | What it does |
|---|---|---|
| Protocol | `crates/remote-proto` | Records, secure records, fragments, pacer, bitrate controller, rate limits, TURN credentials, config, tunnel framing and keys |
| Handshake domains | `crates/session-auth/src/remote.rs` | `remote-offer`, `remote-answer`, `remote-confirm` on the existing `SessionGate` (grants, revocation, single-use challenges) |
| Gateway | `crates/gateway` (`vm-gateway`) | WebTransport, WebSocket, `/v1/capabilities`, static UI, ACME HTTP-01, host tunnels |
| Host | `crates/media-local/src/remote` (`vm-media-local remote`) | Tunnel client, handshake, sessions, shared encoder hub with crop tracks |
| Relay | `crates/control/src/services/desktop_stream.rs` (`desktop.stream`) | `desktop.stream {name, codec, tracks: true}` on the daemon's socket (`--state-dir DIR` for another home): kind 10 frames with a track byte, `{"encoder":"LINE"}` passthrough as `control` messages |
| Encoders | `native/tools/vz-desktop-stream.m`, `spice-console.m` | Runtime bitrate, crop, scale; VZ tracks ([browser-desktop](browser-desktop.md)) |
| Browser | `web/src/remote` | Transports, secure records, reassembly, decode worker, crop view, hop stats |
| Deploy | `deploy/gateway` | Dockerfile, compose with coturn, systemd unit, example config |

## Wire format

Units on the reliable path (a WebTransport bidirectional stream as `u32 len | unit`, or one WebSocket message per unit) are told apart by their first byte:

- `0x07 | JSON`: a plaintext handshake message, only before keys exist.
- `kind | u64 seq | AES-128-GCM(payload) | tag`: a secure record, kinds 1–9 as in `crates/remote-proto/src/record.rs`. Nonce `u32 direction | u64 seq`, AAD `kind | seq`.
- `0xff | JSON`: a gateway notice (its dwell time once a second). Unauthenticated; display only.

Video is one secure record (kind 8, a 41-byte header with epoch, frame number, flags, PTS, host queue time, input acknowledgement and the view rectangle, then Annex B) per WebTransport stream, or per WebSocket message. In datagram mode each fragment is its own kind 8 record. Byte vectors shared by both implementations: `web/src/remote/vectors.json`, checked by `crates/remote-proto/tests/vectors.rs` and the web tests.

Between gateway and host, each browser session is one tunnel bidirectional stream that starts with a `RouteOpen` frame (`route_id`, transport, origin, client address); host video streams and datagrams start with the 8-byte route id.

### Handshake

1. Browser: `{"t":"hello","proto":1,"device_id","vm_id","scopes"}`. Host: `{"t":"challenge","challenge"}` (60 s, single use, bound to this host process).
2. Browser: `{"t":"offer","offer":{"intent","signature"}}`, signed by the device key over the challenge, its ephemeral ECDH key, its codecs, the transport (`wt` or `wss`) and the page origin. The host refuses an offer whose transport or origin differs from what the gateway reports.
3. Host: `{"t":"answer","answer":{"transcript","signature"}}` under the Keychain host key: the host's ephemeral key, session id, codec (HEVC when offered) and expiry (at most 8 h).
4. Both derive `HKDF-SHA256(ECDH, salt = transcript digest)`, one key per direction. Browser: `{"t":"confirm","session_id","confirmation"}`, its device signature over the digest. The host's first secure record is `{"event":"session",...}`.

Any failure answers `{"t":"error","code":4001,"message"}` and ends the route.

### Session

- Input (kind 3, `{"seq","events"}`, seq strictly increasing) and pointer moves (kind 9) need the `input` scope; a view-only session gets `input_error`. Keys and buttons a viewer still holds are released when it leaves.
- Control (kind 4): `{"keyframe_req":true}`, `{"view":{x,y,w,h,out_w,out_h}}` or `{"view":null}`.
- Ping (kind 5) is answered with the host clock; stats (kind 6: `rtt` ms, `loss` 0–1, `fps`) feed the bitrate controller.
- Host events (kind 1): `session`, `ready`, `viewers`, `view` (`fallback: "full"`), `rate`, `lease`, `input_error`, `error`, and `close {code}` just before the host ends a session (4003 revoked, 4001 expired, 4009 too many records that failed to open).
- The registry is re-read every 250 ms; a revoked or narrowed grant ends its sessions with 4003.

## Latency

- Each viewer has a two-slot pacer (newest frame, pending keyframe). A delta that would be overwritten is dropped with the one before it, and the viewer waits for a keyframe instead of decoding late frames. Keyframe requests are limited to one per 500 ms per track.
- WebTransport: one stream per access unit, reset at `clamp(max(2 x frame interval, 1.5 x RTT), 33, 150)` ms; at most 4 in flight per viewer. The gateway copies each stream as bytes arrive (at most 3 in flight per session) and stops the host's stream when the browser side is behind, which the host counts as a loss.
- WebSocket: the gateway's writer drains reliable messages first and holds at most 2 video units; a third is refused at the tunnel.
- Bitrate: each viewer's controller (from veynode/core's adaptive constants) halves at 10 % loss, steps down 15 % on 2 % loss, backlog or rising queue delay, and up 8 % after three clean windows; a track runs at the median of its viewers. Apple Virtualization applies it at runtime; QEMU does not yet ([Not yet](#not-yet)).

## Crop views

A phone zoomed on part of the screen sends `{"view":...}`. Viewers asking for the same rectangle share an encoder track; the output size is the smaller of the crop and the viewer's pixels, within the full screen's pixel budget. At most `tracks_per_vm` tracks (default 4) run per VM. With none free, or on QEMU (one track), the viewer keeps the full screen and gets `{"event":"view","fallback":"full"}`.

## Deploy

`vm-gateway pin --config gateway.toml` prints the gateway's tunnel key pin; `vm-media-local remote` prints the host's at startup. Each goes into the other's config (`[host] gateway_spki_sha256`, `[tunnel] hosts`). See [deploy/gateway/gateway.example.toml](../deploy/gateway/gateway.example.toml).

- **EC2 or any Linux**: `deploy/gateway/compose.yaml` (gateway and coturn, host networking) or `vm-gateway.service`. `tls.mode = "acme"` uses HTTP-01 on port 80 and renews 30 days before expiry without a restart; start with `staging = true`.
- **LAN**: `tls.mode = "self-signed"`. The certificate lasts 13 days (WebTransport's `serverCertificateHashes` limit is 14) and rotates 2 days early; `/v1/capabilities` publishes its hash. Pages still need a certificate the browser trusts, so use `files` with your own CA for anything beyond testing.
- **Cloudflare Tunnel**: `tls.mode = "external"`, `listen.tcp` on loopback, no `listen.udp`. Cloudflare does not proxy WebTransport to an origin, so browsers use wss; for WebTransport put a DNS-only name on an EC2 gateway.

Rough EC2 cost (us-east-1, check before use): t4g.small about $12/month, public IPv4 $3.6, EBS $0.7, egress $0.09/GB after 100 GB (4 Mbps for 4 h a day is about 160 GB a month).

## Not yet

- **Live evidence**: no run against a VM, a browser, EC2 or Cloudflare. Latency targets in the plan are unmeasured.
- **Browser handshake**: `web/src/remote` has the records and transports; the handshake client, `0xff` notice handling and the UI route are in progress.
- **Control lease**: lives in the daemon (computer use phase 3). Until then every `input`-scoped viewer may operate.
- **WebRTC fallback over remote TURN**: `remote-proto::turn` mints credentials, but the host does not use them and the gateway does not advertise `webrtc`.
- **QEMU runtime bitrate** through the relay (`--rate-control-fd` is not wired in `desktop_stream`); resolution scaling from the controller (only bitrate is applied); `encode_us` in the video header is 0.
- **Datagram video mode** has unit tests for fragmentation but no end-to-end test.
- **Docker image** not built here; the network-impairment script (dnctl/pfctl) is not written.
