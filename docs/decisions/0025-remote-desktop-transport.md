# Remote desktop: a transport-only gateway and end-to-end sessions

Accepted 2026-10-26. Amends [0004](0004-signed-local-ice.md) for the remote path; the loopback WebRTC path it describes is unchanged.

## Context

The browser desktop was loopback only. Remote use (LAN and Internet, QEMU/HVF and Apple Virtualization) has to keep latency near local (QEMU about 12 ms, VZ 14–20 ms from input to encoded frame), and [security](../security.md) treats a hosted relay as untrusted: whoever terminates TLS in front of the Mac must not be able to read, change or inject desktop traffic. veynode/core already runs a gateway that only routes, an E2E relay with signed ephemeral keys, and the TURN, lease and close-code conventions; we follow its shape without depending on its account service.

## Decision

- `vm-gateway` (`crates/gateway`) only routes. It terminates browser TLS (wss) and QUIC (WebTransport), accepts outbound QUIC tunnels from hosts (mutual TLS, both sides pin the other's key; no CA), and copies opaque records between a browser session and its host. It has no accounts and no session keys. The same binary serves EC2 (`tls.mode = "acme"`), a LAN (`self-signed`, `files`) and Cloudflare Tunnel (`external`, loopback only).
- Authentication is the existing session-auth device model with new signed domains `remote-offer`, `remote-answer` and `remote-confirm` (`crates/session-auth/src/remote.rs`). The browser's offer binds the challenge (host instance, device, VM, scopes, grant version), its ephemeral P-256 ECDH key, the outer transport and the page origin; the host answers under the Keychain host key with its own ephemeral key; the browser confirms the transcript digest. Keys are HKDF-SHA256 of the ECDH secret salted with that digest, one AES-128-GCM key per direction, with a 4096-record replay window (wide enough for a datagram-mode keyframe to pass a reliable record). The vm-mcp-http bearer-token model was not chosen: a bearer token seen by the gateway would let the gateway impersonate the browser.
- The host process is `vm-media-local remote`, because the Keychain host key only signs for the executable that created it.
- One encoder per VM and codec serves all remote viewers (`crates/media-local/src/remote/hub.rs`), fanned out through per-viewer pacers that keep one newest frame and one keyframe. This removes the Apple Virtualization single-viewer limit for remote viewers without touching the guest bridge, the worker or the agent, which each keep one slot.
- Crop views for phones are encoder tracks: viewers asking for the same rectangle share a track, at most `tracks_per_vm` (4) exist, and a viewer that finds none free keeps the full screen and zooms in the browser. QEMU has one track.
- WebTransport carries each access unit on its own short-lived stream, reset when it misses `clamp(max(2 x frame interval, 1.5 x RTT), 33, 150)` ms; pointer moves go as datagrams. WebSocket carries one message per record. Cloudflare does not proxy WebTransport to an origin, so a Cloudflare deployment advertises only wss.

## Consequences

- The gateway operator can deny service and see sizes and timing, not content. The browser JavaScript is still trusted code served by whoever runs the gateway; the plan's mitigation (showing the bundle hash) is not built yet.
- Control leases are not in the daemon yet: every viewer with the `input` scope may operate, and takeover/release answer `{"event":"lease","mode":"shared"}`. Computer use phase 3 owns the lease.
- The WebRTC fallback over remote TURN is not built: [0004](0004-signed-local-ice.md)'s loopback-only ICE policy still applies, and the gateway does not advertise `webrtc`.
