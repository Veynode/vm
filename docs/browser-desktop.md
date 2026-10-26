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
- The guest scanout sets the source resolution; the host cannot change it.
  Bitrate (default 8 Mbps), output scale and a crop rectangle are
  runtime-adjustable, but nothing in the browser path drives them yet. Any
  crop or scale change rebuilds the compression session (the bitrate carries
  over) and its next frame is a keyframe.
  - `vz-desktop-stream PRIVATE_UNIX_SOCKET --codec hevc|h264 [--tracks]`,
    stdin, one command per line (`T` is a track, 0..7, default 0):

    ```text
    stop
    {"input":[...]}            forwarded to the guest
    [@T ]keyframe
    [@T ]bitrate N             bits/s, clamped to 250000..40000000
    [@T ]crop X Y W H          source pixels; clamped to the source, W/H made even;
                               W or H below 2 is refused; crop 0 0 0 0 = full screen
    [@T ]scale W H             even output size, at least 0.4x the view per side;
                               never larger than the crop; scale 0 0 = view size
    track add T                T = 1..7, needs --tracks
    track remove T             track 0 always exists
    ```

    Only one helper can hold the guest bridge, so viewers share one process:
    each track is its own compression session over the same captured frame.
    With `--tracks` the stdout header is `u32 size, u64 pts_us, u8 keyframe,
    u8 track`; without it the header stays 13 bytes and only track 0 exists.
    Stderr gets `{"event":"view","track","x","y","w","h","src_w","src_h",
    "out_w","out_h","bitrate"}` whenever a track's session is built (start,
    mode change, crop, scale, track add), `{"event":"rate",...}` for
    `bitrate`, and `{"event":"rate","track","command","error"}` for a refused
    command, which leaves the track as it was. A malformed line still ends the
    helper. On a guest mode change crops and scales carry over, clamped to
    the new mode.
  - `spice-console` (QEMU): `set_bitrate`/`rate_status` (rate control
    version 1), `{"method":"set_scale","epoch":E,"width":W,"height":H}` and
    `{"method":"set_crop","epoch":E,"x":X,"y":Y,"width":W,"height":H}`, with
    the same rules against the crop's default codec-fit size (HEVC up to
    1920x1080, H.264 up to 1280x720). The reply and a `view` event carry the
    same fields as on Apple VZ. Crop and scale reset with each encoding
    epoch. The vm-console broker forwards both methods and the `view` event
    for the encoder owner; `vm-console-stream` accepts `scale W H` and
    `crop X Y W H` on stdin and prints `view` lines on stderr. QEMU has one
    track: the broker serves one encoder owner per epoch, so several tracks
    there would need a broker change.
- The stream is local only (loopback UI with a per-launch token). Remote
  viewing, including several viewers of one Apple Virtualization desktop and
  crop views for phones, is [remote desktop](remote-desktop.md).
