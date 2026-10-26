# Browser desktop

`veynodevm ui` shows and controls a running Linux desktop container in the
browser at `/desktop/NAME` (container page: **Open desktop in browser**). It
works for both desktop backends:

| Container | Video source | Input path |
| --- | --- | --- |
| `--desktop --gpus virgl\|venus` (QEMU/HVF) | worker's persistent SPICE console broker | daemon `desktop_input` → QMP `input-send-event` |
| `--desktop` (Apple Virtualization) | guest agent's DRM scanout over vsock 5001 | guest agent uinput keyboard and tablet |

The design follows the low-latency viewer in `veynode/core`: hardware
VideoToolbox encoding with no B-frames and no encoder frame delay, one
WebSocket per viewer, WebCodecs `VideoDecoder` with `optimizeForLatency`,
frames painted to a canvas from the decoder callback (no `<video>` jitter
buffer), and input on the same socket. WebRTC (`crates/media-local`,
`browser/`) remains the separate signed localhost path.

## Pipeline

```
browser canvas ── WebSocket (/desktop/ws, cookie + exact Origin) ── web/desktop.mjs
                                                                      │ stdin/stdout pipes
                                                     veynodevm desktop --stream [--codec hevc|h264] NAME
                                           QEMU ─┤                                   ├─ Apple VZ
              vm-console-stream ← console broker ← SPICE          vz-desktop-stream ← worker bridge ← vsock 5001
              (spice-console VideoToolbox)                         (VideoToolbox)       ← guest agent: DRM scanout, uinput
```

- `web/desktop.mjs`: a dependency-free RFC 6455 server. Access units go out as
  binary messages the moment they arrive. A viewer more than 2 MiB behind
  drops delta frames and asks for a keyframe, so backlog never becomes lag.
- `veynodevm desktop --stream` (`crates/cli/src/desktop_stream.rs`): stdout
  records are `u32 BE length, u8 kind, payload` (kind 1 JSON event, kind 2
  `u64 pts_us, u8 keyframe, Annex B`); stdin takes `{"events":[...]}` and
  `{"keyframe":true}` lines. Held keys and buttons are released when the
  viewer goes away.
- Input events (`crates/control/src/desktop_input.rs`): `move` (source
  pixels plus the frame size), `button` (left/middle/right), `wheel` (±10
  ticks) and `key` (browser `KeyboardEvent.code`, layout-independent physical
  keys), at most 64 per message. The daemon's `desktop_input` RPC validates
  every batch; on QEMU/HVF it is refused while an MCP input operation or the
  native window's input is active.

### QEMU/HVF

The console helper (`native/tools/spice-console.m`) captures at the window
cadence while a browser stream is active and encodes on the trailing edge of
each draw (a 4 ms flush timer), so a lone keystroke is never held until the
next idle tick. HEVC keeps the guest size (1280x800); H.264 is Constrained
Baseline 3.1 scaled to 1152x720.

### Apple Virtualization (Linux)

Apple Virtualization has no host capture API for a Linux display, so the
guest does it. With `--desktop`, container-init starts the agent with
`--desktop-stream`, which listens on vsock 5001 (host CID 2 only, one viewer
at a time; `guest/veynode-agent/desktop_linux.go`):

- Every 8 ms it reads the active CRTC's framebuffer (`GETCRTC`, `GETFB2`,
  `MAP_DUMB`, read-only mmap; XRGB8888, linear). It diffs 64x64 tiles against
  a shadow copy and sends each changed 64-row band as one BGRX rectangle,
  followed by a frame-end record. A still desktop sends nothing; typing sends
  a few KiB.
- Input arrives as JSON lines and drives two uinput devices, "VeynodeVM
  keyboard" and "VeynodeVM tablet" (absolute 0..32767 axes, mouse buttons,
  wheels), which udev and Xorg/libinput pick up like USB devices.

On the host, `vz-desktop-stream` (`native/tools/vz-desktop-stream.m`,
registered by `scripts/install-dev-runtime.py` as `vz_desktop_stream`)
connects to the worker's bridge socket, patches rectangles into a canvas,
copies it into a pooled IOSurface and encodes once per guest frame. It
re-encodes once more 300 ms after the screen settles, because a speed-first
encode of a change can be soft.

### H.264 decode latency

VideoToolbox writes H.264 SPS without VUI `bitstream_restriction`. A decoder
must then assume frames may be reordered and holds output until its DPB is
full, which is 5 to 8 frames at desktop sizes: on a mostly still desktop that
is seconds of lag. The browser client (`web/src/desktop/sps.ts`) rewrites each
SPS to declare `max_num_reorder_frames = 0`, as WebRTC's SpsVuiRewriter does.
Constrained Baseline has no B-frames, so the declaration is true. HEVC SPS
already carries `sps_max_num_reorder_pics = 0`.

## Measured latency

Apple M-series host, XFCE terminal echo, median of 10 keystrokes from the key
event entering the relay to the encoded access unit leaving it:

| Backend | Key → encoded frame |
| --- | --- |
| QEMU/HVF virgl, HEVC | ~12 ms (measured in the browser, key to painted frame) |
| Apple VZ, HEVC/H.264 | 14–20 ms |

Browser decode adds 2–5 ms. The guest scanout diff costs 0.2–0.5 ms per poll
at 1280x800. A browser tab in the background is throttled, and the page's
input→frame figure is then meaningless; measure with the tab visible.

## Limits

- Apple VZ: one browser viewer per container at a time; a second viewer is
  refused with a clear error. The guest cursor is a hardware plane and is not
  in the stream; the browser shows its local cursor.
- Firefox has no HEVC decoder in WebCodecs; use the **Use H.264** link.
- The stream is local only (loopback UI with a per-launch token). Remote and
  multi-viewer sharing remain on the WebRTC path.
