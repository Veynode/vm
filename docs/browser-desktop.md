# Browser desktop

`veynodevm ui` shows and controls a running desktop container in the browser
at `/desktop/NAME` (container page: **Desktop**). It works for Linux desktops
on both backends and for macOS containers:

| Container | Video source | Input path |
| --- | --- | --- |
| `--desktop --gpus virgl\|venus` (QEMU/HVF) | worker's persistent SPICE console broker | daemon `desktop_input` → QMP `input-send-event` |
| `--desktop` (Apple Virtualization) | guest agent's DRM scanout over vsock 5001 | guest agent uinput keyboard and tablet |
| macOS container (Apple Virtualization) | the Guest's Screen Sharing (RFB, raw rectangles), read by the worker | the same RFB session (key and pointer events) |

The design follows the low-latency viewer in `veynode/core`: hardware
VideoToolbox encoding with no B-frames and no encoder frame delay, one
WebSocket per viewer, WebCodecs `VideoDecoder` with `optimizeForLatency`,
frames painted to a canvas from the decoder callback (no `<video>` jitter
buffer), and input on the same socket. WebRTC (`crates/media-local`,
`browser/`) remains the separate signed localhost path.

## Pipeline

```
browser canvas ── WebSocket (/desktop/ws, exact Origin) ────────── web/desktop.mjs
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
each draw (a 2 ms flush timer), so a lone keystroke is never held until the
next idle tick. HEVC keeps the guest size (1280x800). H.264 is Constrained Baseline: 3.1
scaled to 1152x720 for the WebRTC console (it negotiates `profile-level-id`
42e01f), and 4.0 at up to 1920x1080 for the WebCodecs desktop stream, which
`vm-console-stream` asks for with `"level":40` in the `start` request (the
broker and helper accept only 31 or 40, and only for h264; the helper checks the
actual SPS level byte: 3.1 only for the console, 3.1, 3.2 or 4.0 for the desktop,
since AutoLevel picks the lowest that fits: Linux 1280x800 comes out at 3.2). A WebCodecs viewer takes the codec string
from the SPS, so it decodes either. Linux GNOME H.264 now keeps 1280x800 (was 1152x720):
SSIM 0.990 to 0.995, keystroke-to-frame median 46.7 ms. Windows 1920x1080 over H.264: static SSIM
0.937 (scaled to 720p) to 0.989, drag 50 fps at 7.0 Mbps, hover-to-frame median
34-44 ms (HEVC 34-43 ms).

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

Measured live on the `ubuntu-gnome` image without `--gpus` (1280x800, software
rendered): pointer move to a dock highlight to first frame, median 31 ms (HEVC) and
29 ms (H.264); a click that opens the application grid, median 91-102 ms with
the previous encoder and 91-100 ms with the still-session one, so that time is the
guest drawing the animation, not the stream. A settled screen decodes at SSIM
0.994 (HEVC and H.264) and sends its full-quality keyframe.

`--init systemd` desktop containers get the same flag from their agent unit
(`guest/veynode-agent/systemd.go`), so a GNOME guest without `--gpus` streams
on Apple Virtualization too: 62 fps with glxgears under software rendering and
~77 ms for the overview toggle, which is GNOME's own rendering time.

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

### Colour tagging

The encoders convert the BGRA scanout to BT.709 YUV and write that into the
stream (`ColorPrimaries`, `TransferFunction`, `YCbCrMatrix`). Untagged, a
decoder that does not default to BT.709 (ffmpeg assumes BT.601) shifts every
colour and scores far lower on any sharpness measure: a static GNOME desktop
decoded SSIM 0.955 / PSNR 33.0 dB untagged and 0.993 / 45.1 dB tagged (HEVC
1280x800 at 8 Mbps; H.264 1152x720 0.950 → 0.989). A higher quality setting or
bitrate on the same frame gained at most 0.001 SSIM at 13x the bytes, so the
rate settings stay as they are. Checked offline against a daemon screenshot of
the QEMU `gnome` container; the Apple VZ encoder carries the same tags but was
not run live, and the browser's own rendering was not compared.

### Photographic content: a full-quality keyframe once the screen settles

A flat desktop is already at 0.99, but a photographic one is not: the Windows
Neptune desktop at 1920x1080 (default wallpaper) decoded at SSIM 0.938 on its
first frame and 0.973 after 8 s of 1 Hz idle refreshes (HEVC, 8 Mbps). One
keyframe of that screenshot with the helper's properties, encoded offline
(`VTCompressionSession`, hardware):

| `Quality` set at session creation | keyframe bytes | SSIM |
|---|---|---|
| unset (the speed-first session) | 302 KB | 0.938 |
| 0.6 | 302 KB | 0.933 |
| 0.8 | 665-754 KB | 0.978-0.980 |
| 0.9 | about 1.1 MB | 0.987 |
| 1.0 | 1.96-2.04 MB | 0.991 |

`PrioritizeEncodingSpeedOverQuality` changed neither size nor SSIM. `Quality`
is only taken when the session is created: setting it on a live session left
the next frame's bytes and SSIM as they were. Constant quality as the main
session is also unsafe: panning photographs reached 57-162 Mbps at 0.8-0.9
(`DataRateLimits` is ignored in that mode), far above the 8 Mbps target.

So `spice-console` keeps the speed-first session for everything that moves and
holds a second session, created on first use with `Quality` 0.9 and the speed
priority off, for a screen that has stopped changing. After 700 ms without a
change (at most once per 3 s, only up to 4.2 Mpx so a keyframe stays far below
the 4 MiB access unit bound) the still session encodes one keyframe and carries
the 1 Hz idle refreshes; at the next changed frame, a `set_scale`/`set_crop`
rebuild or a rate command the speed-first session takes over again with a
forced keyframe (the two reference chains differ). A failed creation leaves the
speed-first session alone. The still session is outside rate control, so it is only
entered while the speed-first session's bitrate target is still the 8 Mbps default: a
viewer the controller has backed off for would stall on a 1 MB keyframe.

Measured live (Apple M-series, HEVC, static screen, decoded SSIM against a
lossless daemon screenshot):

| Screen | last frame before | last frame now |
|---|---|---|
| Windows Neptune 1920x1080 (photographic) | 0.938-0.973 | 0.988 |
| GNOME 1280x800 | 0.9924 | 0.9944 |
| GNOME H.264 1152x720 | 0.987 | 0.990 |

Windows H.264 (1280x720 after the scale-down) went from 0.909 to 0.937. No
regression where it matters: Linux keystroke-to-frame median 46 ms (41-47 ms
before), Windows pointer-to-frame median 34.6 ms (35-40 ms before), Windows
drag 53.9 fps at 6.8 Mbps, and three motion/still cycles decode without error
(343 access units, 343 frames). The cost is one 0.7-1.1 MB keyframe burst per
settle on photographic content (a few KB on flat desktops) and a keyframe at the
first change afterwards. In the built-in browser (`/desktop/win11`, HEVC) the settled
keyframe decoded in 11.6 ms (burst 18 Mbps, then 1 fps at 0.9 Mbps) with no console errors.

`vz-desktop-stream`, the encoder for Apple Virtualization desktops (Linux guest
agent and macOS over Screen Sharing), does the same per track. It already
re-encoded once 300 ms after the last change; that frame is now a keyframe from
the still session (at most every 3 s, otherwise the earlier refinement), and a
rate command goes to the speed-first session, parked or not; it too only enters still at the
default bitrate (a backed-off 2 Mbps track kept its 40 KB first keyframe and never sent the 929 KB one). Offline against a
fake guest feeding a noisy 1920x1080 test image, the settled keyframe went from
5 KB to 929 KB and its SSIM from 0.78 to 0.86; crop, scale, bitrate and
keyframe commands during a still screen kept the stream decodable. Live on a
macOS Tahoe guest (1920x1080 photographic wallpaper, scored against the JPEG
screenshot, so the ceiling is lower): HEVC 0.977 to 0.980, H.264 0.977 to
0.980, window drag 36-37 fps both ways (3.4-3.9 Mbps). The macOS gain is small
because the old settle frame was already close, and a macOS desktop is rarely
still: a viewer connecting makes Screen Sharing show a banner and a menu bar
indicator, and the Photos and weather widgets and a terminal cursor keep
changing the screen (hiding widgets brought the update rate to 0.7/s). Not
measured: the Linux VZ guest agent path live.

The 36-37 fps drag figure was the test's input rate, not a pipeline limit: a
rubber-band drag only changes the screen when the pointer moves, and the test
sent about 54 moves/s. With the same drag at 100 and 250 moves/s the stream
delivered 57.5 and 57.6 fps (7.5 Mbps), matching the Screen Sharing server's
~56 Hz update cadence (a raw RFB client reading the same drag got 48-50
updates/s, the lockstep variant 54). A browser sends pointermove at 60-120/s,
so macOS motion in the browser is limited by the server's ~56 Hz, not by the
worker, bridge or encoder (the worker's `write_ms` is 15-30 ms per 5 s).
Live, one macOS Tahoe guest.

macOS input latency, same guest, Command+Space opening Spotlight (a full-screen
update) ten times per run: the first encoded access unit over 2 KB leaves the relay a
median 80.1 and 60.3 ms after the key-down (two runs; 50-101 ms range), against a median
54.9 ms (22.6-65.2) for a raw RFB client's first update over 20000 px for the same key.
The browser path therefore adds about 5-25 ms (HEVC encode, two 8 MB copies and the
bridge for a full-screen frame) on top of the Screen Sharing server's own ~55 ms; the
spread between runs is as large as that gap, so no single stage is worth chasing without
a per-stage timer. A threshold of 700 B or 20 KB picks a later frame of Spotlight's
animation (91 ms median at 20 KB) and typed characters stay under 700 B, so the access
unit size used as the trigger matters more than the pipeline.

Bitrate against motion fidelity, offline (VideoToolbox HEVC, realtime and speed-first as the
encoders configure it, ffmpeg `testsrc2` 1920x1080 at 60 fps for 5 s, SSIM against the source):
4 Mbps 0.983, 8 Mbps (the default of both browser-desktop encoders) 0.988, 16 Mbps 0.995,
32 Mbps 0.999. A synthetic pattern is harsher than most desktops, so this bounds what a higher
default could buy on busy motion, and the fixed default stays 8 Mbps: a higher one would
double what a slow viewer must absorb. The relay (`web/desktop.mjs`, `BitrateGovernor`) now
raises an Apple Virtualization viewer's target one step every 5 s (8, 12, 16, 24 Mbps) while
nothing is queued behind its socket (under 256 KiB), and any queue sends it back to 8 Mbps and
doubles the wait before the next step, up to a minute. It uses the encoder's existing `bitrate`
line. Live on a macOS Tahoe guest, a rubber-band drag at 8, 16 and 24 Mbps targets (two runs each)
kept 56-57 fps and spent 4.2-4.3, 6.9-7.0 and 8.3-8.5 Mbps (12, 19 and 24 KB per access unit),
so the encoder uses a higher target to lower its quantizer even where 8 Mbps was not a hard cap.
Through the real relay over a 30 s drag the target stepped 12, 16, 24 Mbps at 5 s intervals and
the stream went from 4.0 to 10.2 Mbps at 57 fps; with the viewer's socket paused for 12 s the
relay sent 8 Mbps and later climbed to 12 again. QEMU desktops (Linux, Windows) are governed too:
the relay's `bitrate` line goes through `veynodevm desktop --stream`, which owns the controller end
of `vm-console-stream`'s private rate channel (`--rate-control-fd`: `rate_status`, then `set_bitrate`
with the next sequence, one request at a time, the encoder's read-back checked, one status refresh
after a stale refusal). Live on the QEMU GNOME desktop, toggling the overview every 0.46 s, the
target stepped 12, 16, 24 Mbps and the stream went from 1.4 to 24.6 Mbps in 5 s windows (about
45 fps throughout, GNOME's own animation rate), with no rate errors. Not measured: picture quality
of these streams (no reference frames for live motion).

A viewer on a slow link, simulated by a client that reads its socket at 6 Mbps (QEMU GNOME, the
overview toggling every 0.46 s, 60 s runs; delay is each frame's arrival over its timestamp, against
the lowest in the run): with a fixed 8 Mbps the delay settled at a median 155 ms (p95 235 ms). With
the first governor, which looked only at bytes queued in the relay, it kept stepping up to 24 Mbps
and back, and the delay spiked to a p95 of 1.15-1.24 s (max 1.34 s) each time: the kernel's send
buffer fills before the relay's queue does, so the queue was seen late. The governor now also pings
the viewer every 250 ms (`LinkDelay`): the pong returns behind everything sent before it, and its
round trip over the shortest recent one is the delay the link adds; more than 40 ms counts like a
queue. The same 6 Mbps run then went to 16 Mbps once, was sent back to 8 Mbps and stayed there (median
delay 159-189 ms once saturated, max 311 ms), and an unconstrained link still reached 24 Mbps with
a 3-4 ms median delay.

Stepping back to 8 Mbps is not enough on a link below 8 Mbps: a Windows desktop (Neptune, the Start
menu toggling every 0.46 s) over the same 6 Mbps client sat at a 239-761 ms median delay (p95 up to
1.16 s, max 1.22 s) at 8 Mbps, since its motion fills the whole target. The governor therefore also
steps a viewer that still queues or delays at 8 Mbps down to 6, 4, 2 Mbps (at most every 2 s) and
climbs again after the usual quiet wait. The same Windows run then held a 6 ms median delay with
bursts of 130-250 ms while the target moved between 4, 6 and 8 Mbps (4-6 Mbps delivered, 38-58 fps),
and an unconstrained Windows link still stepped 12, 16, 24 Mbps, reaching 28-38 Mbps at 53-57 fps
with a 2-8 ms median delay. Lower targets cost picture quality (4 Mbps measured SSIM 0.983 offline
against 0.988 at 8); a viewer on a link that cannot carry the stream is better served by that than by
a second of delay. A long link with jitter must not read as a queue, so the delay is the lower of
the last two ping round trips (a queue holds across pings, jitter does not; one already 150 ms over
the shortest counts alone), checked offline with round trips of 95-170 ms and no queue: the target
never dropped below 8 Mbps and still climbed to 24. Step-downs now come at most every second. Over
the 6 Mbps client on Windows (four runs) the first 10 s still carry 0.3-1.0 s p95 delay while the
backlog built at 8 Mbps drains, then the median is 2-7 ms; each later probe back up to 8 Mbps costs
one 0.3-1.1 s burst, spaced by the doubling wait. A delay over 250 ms now drops the target two levels
instead of one: the first 10 s's worst delay went from 0.7-0.8 s to 0.27-0.57 s (three runs), at the
cost of reaching 2 Mbps sooner. Not measured: a real WAN link (loss, jitter), only
a rate-limited local socket.

## Measured latency

Apple M-series host, XFCE terminal echo, median of 10 keystrokes from the key
event entering the relay to the encoded access unit leaving it:

| Backend | Key → encoded frame |
| --- | --- |
| QEMU/HVF virgl, HEVC | ~12 ms (measured in the browser, key to painted frame) |
| Apple VZ, HEVC/H.264 | 14–20 ms |

Key to first encoded frame on the QEMU GNOME desktop, measured at the daemon
(RPC key injection to the stream's first frame): median 34–38 ms, 26–46 ms
range; the first press after the stream starts is slower (one run: 291 ms). That is the guest's
first GNOME overview after boot, not the stream: three fresh streams on a just-booted guest gave a
first press of 381, 67 and 34 ms, then 37-61 ms for the rest (run medians 54.3, 48.0, 40.9 ms).
Idle before the key costs little: median 43.5 ms after 0.8 s and 48.8 ms after
4 s (10 presses each). With six host CPU spinners running the 4 s case fell to
38.7 ms (34.6–44.2), against 50.3 ms (41.2–54.6) on an idle host, so about
12 ms is the host's cores waking, not the stream. A latency-critical activity
plus user-interactive QoS in `spice-console` did not change it (53.8 / 46.1 ms
at 4 s) and was reverted. Animations off in GNOME did not change it either. Frame rate with a continuously redrawing guest app (glxgears on GNOME, guest
display 75 Hz): 60.0 fps and a 16.7 ms median gap for both HEVC and H.264. The
capture side already sits on a 60 Hz slot grid; the encode gate used to demand
a full 16.667 ms since the last frame and could only fire on a 4 ms timer, so a
copy admitted a fraction early waited a tick and the stream averaged 54 fps.
`encodeSlack` (3 ms) and a 2 ms flush timer fixed that without changing key
latency (median 37 ms). On Apple Virtualization (XFCE `ubuntu-desktop`, no GPU) the same measures give
17.9 ms median key (typing in a terminal) to first frame, 125 fps encoded for a
continuously redrawing app (one encode per guest frame, 8 ms scanout poll), and
static SSIM 0.997 (HEVC) / 0.998 (H.264) once tagged. A viewer started by a
client that is killed without closing stdin (SIGTERM) leaks the daemon's single
bridge slot until `stop_guest_stream_bridge`; the web server closes stdin. Browser decode adds 2–5 ms. The guest scanout diff costs 0.2–0.5 ms per poll
at 1280x800. A browser tab in the background is throttled, and the page's
input→frame figure is then meaningless; measure with the tab visible.

## Windows (QEMU, `virtio-ramfb`)

Measured on a Windows 11 ARM64 install (Insider 26300, 2026-10-27), HEVC:

- Basic Display, the default, runs at **800x600**: an idle desktop produced 27
  fps of byte-identical frames, because the driver flushes the ramfb on a
  timer. The console helper now compares each source copy with the last one
  and skips an unchanged frame (the 1 Hz idle refresh and keyframe requests
  still encode), so an idle Windows desktop streams 1 fps. Hover-highlight to
  first frame: median 40 ms (17-47 ms). Static SSIM 0.987 (HEVC) / 0.988
  (H.264). The Start menu itself takes about 395 ms to appear in the guest.
- Adding virtio-win's `viogpudo` to the injected drivers gives **1280x800**
  (`virtio-ramfb,xres=...` alone does not: edk2's ramfb only offers its fixed
  modes). It is not the default because it measured worse: hover latency
  median 74 ms, Start-menu animation frames 32 ms apart (about 31 fps), and a
  `vm_screenshot` of a photographic 1280x800 desktop (the default wallpaper)
  was over the native console's old 2 MiB PNG bound, so MCP screenshots failed
  where the 800x600 ones fit. The native console now matches the documented
  contract (PNG <= 8 MiB; one JSON record up to `console_wire::MAX_JSON_BODY`,
  12 MiB, queued beside the 8 MiB budget): a Neptune 1280x1024 Windows
  screenshot of 2.62 MB came back in 0.44 s.
- Frame rate: both display drivers delivered frames about 30 ms apart (27 fps
  idle redraws on Basic Display, 31.7 ms animation gaps on viogpudo), well
  below the 60 fps the Linux virgl path reaches. The cause is not QEMU's 2D
  display in general: a Linux guest on the same QEMU/SPICE path with a plain
  `virtio-gpu-pci` 2D scanout (`qemu_graphics_profile: virtio-2d`, the `gnome`
  container with that profile) streamed glxgears (software-rendered, 2724 fps
  unthrottled) at 60.0 fps with a 16.9 ms median gap, because virtio-gpu
  resource flushes wake the display. Basic Display on `virtio-ramfb` has no
  flush event, which fits a 30 ms poll (not measured at the source), and
  viogpudo's 31.7 ms gaps point at the driver.
- **`--gpus neptune` reaches 60 fps.** The same install re-measured on
  2026-10-27 (Insider 26300) with UTM guest tools 0.1.273's
  viogpu3d driver and the `neptune-dxmt-experimental` profile
  (`virtio-ramfb-gl`): the guest is 1280x1024, HEVC, and a Start-menu click
  every 1.5 s streamed bursts with a **16.5 ms median frame gap (60.5 fps)**,
  against 30.9 ms (32 fps) on Basic Display in the same install and harness.
  An idle desktop still streams 1 fps; pointer move to first frame: median
  36 ms (16 trials, 14-820 ms). So the 30 ms cadence is the
  flush-less display path, not the encoder; for Windows at 60 fps create the
  machine with `--gpus neptune` (see known-issues for the driver version that
  must match the host). The console does not choose the guest size under
  Neptune: viogpu3d picked 1280x1024. viogpu3d without the GL profile does
  not work: the same driver on plain `virtio-ramfb` reports Status Error
  and Windows falls back to 800x600 Basic Display, so the 60 fps
  comes from the host 3D path, not from virtio-gpu flushes alone.
  Retried on this install after Neptune was set up (`machine update --gpus none`, boot): the console
  stayed on "Display output is not active" for over 7 minutes instead of falling back to Basic Display,
  so the 32 fps path cannot be re-measured here without a fresh install; `--gpus neptune` restored the
  desktop at 1920x1080 on the next boot.
  Resolution under Neptune, measured 2026-10-27: Windows offers modes up to
  2048x1152 (1920x1080 among them, 150% scale recommended) and 1920x1080 can
  be chosen in Settings > Display; dragging a window then streamed at about
  50 fps (two 10 s runs: 50.3 and 49.8) and a lossless screenshot was 724 KB.
  The host cannot choose it: `virtio-ramfb-gl,...,xres=1920,yres=1080` was
  accepted by QEMU but Windows still recommended 1280x1024 (and treated the
  changed EDID as a new monitor, dropping the mode picked by hand). With the
  device arguments unchanged, the 1920x1080 picked by hand survived a clean
  shutdown and the next boot streamed and screenshotted at 1920x1080.
  `veynodevm machine display NAME WIDTHxHEIGHT` does that step: ChangeDisplaySettings
  only works on the interactive desktop (from the SSH session itself it returns
  -1), so the command runs it as a one-shot scheduled task of the logged-in user
  through the machine's SSH login. Live on this install: 1600x1200 and 1920x1080
  both applied (`result=0`), 1234x777 was refused (`result=-2`, reported as an error);
  1600x900 is refused the same way (not a mode the Neptune guest offers) while 1280x720 applies and the
  console screenshot follows to 1280x720. The VM's page in the web UI has a Display section with the
  same command (type a size, Apply; checked in the built-in browser: 1280x720 applied through the form).
  A click sent as one `desktop_input` batch (button down and up together) never
  reached Windows: QMP syncs once per `input-send-event`, so the guest saw only
  the released state (Start menu, 0 of 6; 6 of 6 once the up was its own command,
  even 5 ms later). The QEMU worker now sends each button or key change as its
  own command (`desktop_input::sync_groups`): the same-batch click opened the menu
  6 of 6, and Linux key-to-frame latency was unchanged (median 46.8 ms against 46.9).
  The QEMU build shipped in UTM has no display refresh or update-interval
  option for `ramfb` (only a `refresh_rate` EDID property of virtio-gpu, which
  Basic Display does not use), so the 30 ms cadence cannot be tuned from the
  machine configuration.
- MCP input on the Neptune 1920x1080 desktop (`input_click` after a `screenshot`, the
  path `vm_input_click` takes): four clicks on the Start button toggled the menu
  open, closed, open, closed (4 of 4), each press and release a separate QMP command
  20 ms apart, and the 1080p screenshots were 3.5-4.1 MB of PNG.
- Installing from an ISO under `~/Downloads` failed (`Resource temporarily
  unavailable`) for the worker; a copy under `.local/iso` worked.

## macOS

macOS containers stream through the same worker bridge and `vz-desktop-stream`
encoder as Linux on Apple Virtualization, with the Guest's built-in Screen
Sharing server as the frame source (`MacDesktopStream` in
`native/worker/ScreenSharing.swift`). It needs nothing installed in the Guest
beyond what MCP screenshots already use (Screen Sharing on, the login file).

- The worker opens its own RFB connection (shared session, so the native
  window, MCP screenshots and the browser desktop coexist), asks for raw
  rectangles, and re-emits them in the Linux agent's record format (`mode`
  event, rectangles of at most 64 rows, frame end) over a socketpair that the
  bridge treats as the Guest. Input arrives as the same `abs`/`key`/`wheel`
  events; evdev codes are mapped to X keysyms (`screenSharingEvdev`), and
  Meta is Command. Held keys and buttons are released when the viewer leaves.
- Measured on a macOS 26 guest at 1920x1080 (default Tahoe wallpaper, Terminal
  open), HEVC at the default 8 Mbps:
  - Raw RFB incremental updates reach about 52 updates/s while a window is
    dragged (about 590 Mbit/s of raw pixels on the host-only network), so
    Screen Sharing, not the encoder or the bridge, sets the frame rate. The
    encoder produced 44 fps through the stream (6 Mbit/s), limited by the
    harness's 48 pointer moves/s.
  - The worker sends the next incremental request as soon as an update's first
    rectangle header is read, instead of after the whole update was forwarded
    (RFB allows several outstanding requests). Against a guest scrolling text
    (about 150k pixels per update, the guest's redraw rate being the limit),
    three alternating runs of 8 s each gave 19.4, 22.2 and 20.4 updates/s with
    it and 16.5, 18.4 and 20.0 without: about 13% more, never fewer. The
    recorded HEVC stream decodes in full (236 of 236 frames).
  - A keystroke echo in Terminal shows up in the stream after a median of 40
    ms (11–59 ms); the same echo is available over RFB after about 30 ms, so
    the host pipeline adds about 10 ms.
  - Command+Space to the first changed frame (Spotlight opening, access
    unit above 700 bytes), 16 presses on a fresh guest: median 78 ms (46-100
    ms; one 2 ms sample was the previous Escape's frame).
  - Re-measured 2026-10-27 on a fresh guest (20 key presses, first access
    unit above 700 bytes): median 66 ms, minimum 28 ms, maximum 137 ms; most
    presses changed a large part of the screen (80-130 KB access units), the
    smallest echo (6.5 KB) took 28 ms.
  - Command+Space (Spotlight opens, a large change) to the first access unit
    over 700 bytes: median 74 ms over 24 presses (7-184 ms, 2026-10-27, warm
    guest). That includes Spotlight's own drawing time.
  - Clarity against a lossless Screen Sharing screenshot: SSIM 0.979 (PSNR
    38.9 dB) for HEVC, 0.977 for H.264. Raising the bitrate to 20 Mbit/s
    moves SSIM only to 0.980 and lowering it to 3 Mbit/s to 0.976: the limit
    is 4:2:0 chroma on text and a photographic wallpaper, not the bitrate.
  - HEVC at 1920x1080 is accepted here (Main level 4.1): the level-4 check
    above is in `vm-console-stream` (QEMU) only; `vz-desktop-stream` has none
    and Chrome decodes the stream.
- The bridge forwards 256 KiB per step (was 64 KiB) and the socketpair buffers
  are 1 MiB: a full-screen update is 8 MB, so the 8 KB default and the small
  chunk made large updates cost tens of milliseconds.
- Keys: Apple's Screen Sharing server takes `Alt_L`/`Alt_R` as Command
  (Command+K cleared Terminal) and ignores `Meta_L`/`Meta_R`. Option never
  appeared: with Terminal focused, a chord of each keysym below plus `x` (Option+x
  is `≈`) typed a plain `x` (the modifier is ignored: 0xff7e, 0xfe03, 0xfe05,
  0xfe11, 0xfe13, `Shift_R`) or typed nothing and never `≈` (`Hyper_L`/`Hyper_R`,
  `Super_L`/`Super_R`, `Control_R`, and the Alt keys, which are Command). Fifteen
  keysyms in all (2026-10-27); no key event reached Option. The browser's
  Meta (Command) keys therefore send Alt, and its Alt keys send Meta, which has
  no effect. The MCP key map (`screenSharingKeysyms`: `meta_l` → `Meta_L`,
  `alt` → `Alt_L`) has the two the other way round, so an MCP `meta_l` chord
  does not act as Command; that map is unchanged.
- Not verified: a guest resolution change (the desktop-size pseudo-rectangle
  is handled but VZ's macOS display size is fixed in this setup). The stream
  shows no guest cursor; the browser draws its own.

## Why 1280x800 at 60 fps, and why Apple Virtualization is not capped

- HEVC is held to Main level 4.1 or lower (the helper rejects a higher-level
  SPS: `hevc_profile_validation_failed`). Level 4 allows 66.8 Msamples/s,
  which at 60 fps is about 1.1 Mpixels, so 1280x800 (61 Msamples/s) is the
  largest size at level 4; level 4.1 (123) is accepted too and carries
  1920x1080 at 60 fps. `veynodevm run --desktop --gpus virgl --desktop-size
  1920x1080` (even sides, 640..3840 x 400..2160) sets the QEMU guest size
  when the container is created; `veynodevm update --desktop on|off
  --desktop-size WxH [--restart] NAME` (or the Desktop display and Display size
  rows on the container's page) changes it later, restarting a running
  container and keeping its disk. The image must contain a desktop.
  Measured on a 1920x1080 Ubuntu GNOME guest: 60 fps under glxgears, SSIM
  0.992 / PSNR 46.4 dB against the guest framebuffer, key-to-pixel median
  36.9 ms. A bigger size gives more desktop area, not sharper text, since the
  guest does not scale its UI.
- Apple Virtualization encodes once per guest frame (about 125 fps under
  animation, 17 % of one core for the encoder). The browser shows at most its
  display rate, but a 120 Hz display does use the extra frames and the cost is
  small, so it is not rate-limited. QEMU's stream stays on the 60 Hz capture
  grid (the guest display there refreshes at 75 Hz).
  Tried 2026-10-27: lowering the helper's encode interval from 16.7 ms to
  13.3 ms (75 Hz) changed nothing (glxgears, 60.0 fps, 16.9
  ms median gap), so the limit is the cadence of the display updates QEMU
  hands the helper, not the helper's pacing; going past 60 there would need a
  change in QEMU/SPICE itself. Not changed.
- Native window and ProMotion: Virtualization.framework's headers (macOS SDK
  used here) have no refresh-rate setting on its display devices, so a
  `VZVirtualMachineView` window is paced by the framework and the guest
  display; the QEMU native console's `MTKView` is set to
  `preferredFramesPerSecond=60`, which matches its 60 Hz source, so raising it
  alone would show nothing new. Not tested on a ProMotion display.

## Limits

- Apple VZ: one browser viewer per container at a time; a second viewer is
  refused with a clear error. The guest cursor is a hardware plane and is not
  in the stream; the browser shows its local cursor.
- Windows (QEMU) paints its cursor into the display surface, so the stream already shows it and a
  second, page-drawn arrow made two cursors, the guest's one trailing the real pointer by a frame
  or more. The `ready` event now says `cursor: "guest"` for a QEMU Windows machine and the page
  hides its own cursor over the canvas (`cursor: none`); every other guest says `"local"`. Seen on
  win11 (Neptune profile, live): the arrow is in the decoded frame and the canvas cursor is `none`.
  Not verified on Windows with another GPU profile; a guest cursor that is a hardware plane would
  not be in the stream and would leave no cursor at all.
- Firefox has no HEVC decoder in WebCodecs; use the **Use H.264** link.
- The guest scanout sets the source resolution; the host cannot change it.
  On QEMU a resolution change inside the guest replaces the scanout, which
  ends the encoding epoch; `vm-console-stream` stops it and starts the next
  one on the same connection, and the browser reconfigures its decoder on the
  keyframe's new SPS (fixture test only; not yet checked on a live guest).
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
- The stream is local only (loopback UI, Host and Origin checked). Remote
  viewing, including several viewers of one Apple Virtualization desktop and
  crop views for phones, is [remote desktop](remote-desktop.md).
