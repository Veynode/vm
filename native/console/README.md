# Persistent QEMU console: native display target

This target adds an AppKit / MetalKit **view-only** window to the existing
`native/tools/spice-console.m` capture process. It is the same persistent SPICE
client and shared capture/encoding protocol, not a second SPICE consumer. A
window does not own the VM, broker, SPICE connection, screenshot cache or remote
encoding session. Closing the window does not request VM or capture shutdown.

## Build and offline validation

```sh
native/console/build.sh --output-app "$PWD/.local/VeynodeQEMUConsole-NEW.app"
native/console/test.sh "$PWD/.local/VeynodeQEMUConsole-NEW.app"
```

The output path must be absolute, new, and end in `.app`; existing paths and
symlinks are refused. The existing `.local/bin/spice-console` is untouched. This
is a development bundle with ad-hoc signature and absolute rpath to the pinned
CocoaSpice / UTM dependency cache used by the current prototype. It is **not** a
redistributable or notarized app package.

Launch only through the VM's sole broker owner, passing the private SPICE socket
and `--persistent`, as for the headless helper. Do not launch another consumer
against an active VM's socket. The patched producer's per-client generation FDs
do not make ordinary multi-client SPICE an independent capture path: the server
normally disconnects the old client, and debug multi-client rendering waits for
all draw acknowledgements.

`--probe-capabilities` prints one JSON object and does not open a window, connect
to SPICE, encode, or submit a GPU command. `--probe-codecs` retains its real
hardware encoder tests and adds the same capability fields. `native_window`
means the compiled display target has a public Metal device; a successful probe
is not evidence that a window was displayed. The unchanged headless build
reports `native_window: false`.

## Window control protocol

Use existing newline-delimited JSON on the single owner stdin. Window commands
have exactly two fields and **no epoch**:

```json
{"id":"w1","method":"capabilities"}
{"id":"w2","method":"show_window"}
{"id":"w3","method":"window_status"}
{"id":"w4","method":"close_window"}
```

Replies use the existing length / kind / epoch envelope: JSON kind `1`, epoch
`0`, `{ "id": "w2", "result": { ... } }` or
`{ "id": "w2", "error": { "code": "...", "message": "..." } }`.
Capabilities include `native_window`, `native_window_protocol: 1`,
`display_only: true`, `input_enabled: false`, the 60 fps visible / 30 fps background source limits, a separate fixed 30 fps
encoder limit, three owned slots and one in-flight display GPU command. `ready` is unchanged.
Window replies also include `window_open`, `window_visible`, frame availability,
source generation/sequence, submission/completion/failure counters and pool
usage. Minimized windows remain open even though not visible.

Only one window request is queued at a time; another returns
`native_window_busy`. Main-thread requests waiting more than one second expire
before mutation. AppKit performs show/close on the main thread; its result is
returned to the sole GLib owner via an attached source, then ACKed. A completed
show means a window exists, **not** that a frame has already reached the screen.
A completed close means the window is closed; `display_gpu_inflight` can remain
true until its previously submitted command completes. Initial Metal pipeline
creation and AppKit work have not been demonstrated to meet a latency bound;
the broker must retain its bounded timeout and treat missing ACK as uncertain.
No window request changes an encoding epoch or native input state.

The content view does not accept or forward keys, mouse buttons, wheel,
clipboard or accessibility operations. The title and a line above the picture say “View Only” only when there is no native input path; input that simply follows focus is not announced. Native
window chrome, move, resize and minimize still work. A future input path must
be authorized through the controller; this target does not bypass it.

## Ownership and display path

The capture worker waits for its existing Metal copy of the borrowed SPICE
IOSurface to finish, publishes an owned BGRA buffer, and then acknowledges the
producer. No video encoding is involved in the window path. The renderer samples
that owned buffer directly as a Metal texture into a drawable with aspect-ratio
letterboxing and Retina drawable pixel dimensions. Resizing the window scales
this view; it does not change Guest resolution. Existing raw-row orientation is
preserved; actual window orientation still requires Guest-grounded visual
validation.

A lock-protected three-slot CVPixelBuffer pool provides explicit leases for the
capture cache, latest display publication and GPU in-flight access. A slot is
never reused or resized until all leases are released. If no slot is available,
the incoming frame is dropped and the SPICE draw is acknowledged; no frame queue
or per-frame asynchronous task grows. At the maximum supported 3840×2160 BGRA
source, the three buffers are approximately 95 MiB plus alignment; they may
remain allocated after closing the window. Encoder scaling/drawable allocations
are separate. This is a bound, not a measured resident-memory result.

The renderer admits at most one GPU command. It skips unchanged source sequence
and drawable size, including static content after its first completed draw.
Generation replacement / producer loss clears publication, and a generation
check immediately before GPU submission rejects a command prepared for a retired
source. A command already submitted cannot be retroactively erased; the next
available render tick clears the display. Source-liveness detection retains the
existing capture timer interval. These are not atomic zero-stale-frame claims.

`DrawCompletionOwnershipTests.m`, run by `test.sh`, is a headless regression test for the completion ownership fix in 74dfa73. It includes the production `Display.m`, retires the source generation from a fake view's `currentDrawable`, and asserts that an invalid draw releases its frame lease exactly once. It opens no window and no NSApplication; its GPU work is offscreen.

The headless compile path retains its original owned buffer and encoder logic.
The display compile path reuses all original screenshot, codec, source generation,
stop and terminal handling rather than maintaining a second protocol implementation.

## Evidence and remaining acceptance

On 2026-10-09, stage3 compiled and passed strict ad-hoc signature verification.
Its executable SHA-256 is
`04e5282792841dff5e5348186b56546afe477152d415f10948db191b4262f03b`.
The real CVPixelBuffer offline self-test exercises pool exhaustion, a retained
in-flight slot surviving cache and generation invalidation, safe free-slot
resize, replacement publication, and closed publication refusing to revive a
frame. The separate headless source also compiles and reports false native-window
capability. Rebuilding onto the existing staged app is refused without changing
its hash.

These checks open no GUI and connect to no VM. They do not establish actual
Guest frame orientation, draw success, closing/reopening while remote capture
continues, producer crash behavior with a visible window, or frame-time / CPU /
memory performance. Those require the root agent's real Linux VM and broker
integration run. The initial stage3/4 source cap was 30 fps. The later stage6 limit is detailed
below; neither version claims the product's 1440p/60 Hz target or performance
against Parallels.

### First real window attempt and shader correction

The root agent's first stage3 `window:true` launch failed before QMP continuation
with `native_display_metal_unavailable`; no native display pass was claimed.
A separate no-window compilation of the exact embedded shader reproduced
`MTLLibraryErrorDomain`, code `3`: `quad` conflicts with Metal's reserved typedef
of the same name. Stage4 uses prefixed shader entrypoints. The shared
`prepareMetal` path is now exercised by `--display-metal-self-test`, which creates
the real library/functions/render pipeline and command queue but submits no GPU
command and opens no window. That check passes on the host; it is now part of
`test.sh`, so Objective-C compilation alone cannot hide an invalid embedded shader.

Initialization errors identify device, command queue, library compilation,
function lookup or render pipeline phase. Stderr JSON includes bounded NSError
domain/code/localizedDescription from this fixed shader setup, never arbitrary
NSError userInfo or Guest image data. The control error also identifies the
failed phase. Stage4 SHA-256:
`dcf3d17f55bd0bbf2a6c950e71117a2ce408aa8c589d5efc4be92161a65728c9`.
Stage3 remains unchanged as the failed historical artifact. Actual stage4
window rendering remains for the root agent to verify.

## Stage6: visible capture cadence and presentation timestamp probe

Stage6 SHA-256:
`503dc9883eec41344943b170ac6f826c761d4822c15d164e8871a37883a0a1f6`.
Stage4 remains frozen; root separately reported actual native show/close/reopen,
continued browser streaming, and process cleanup on producer death. Stage6 itself
has only build / offline tests at this checkpoint, not a VM performance result.

The AppKit main thread publishes visible state under the existing lock. A window
must be open, visible, not miniaturized, and have the public visible occlusion
flag. While visible, source draw admission uses a 60 Hz rational deadline cadence;
otherwise it uses 30 Hz. Closing clears visibility synchronously before the ACK.
Generation replacement resets pacing. Delayed events skip missed slots without a
catch-up queue. Source event jitter, GPU copy/encode cost and draw availability
can lower the actual rate; changing the bound does not prove achieved 60 fps.
The renderer's refresh preference is also 60 Hz and still submits at most one GPU
command at a time, skipping unchanged sequence/size. The owned slot limit stays
three.

Remote encoding is deliberately independent. The current `start` protocol has
no fps field and retains its existing fixed 30 fps gate, encoder properties and
one-second idle refresh. Visible-source admission does not increase encoded
frames or static repeats. This change does not advertise remote 60 fps support.

Additional capabilities (all limits/preferences, not observed performance):

```json
{
  "capture_limit_fps": 60,
  "native_window_capture_limit_fps": 60,
  "background_capture_limit_fps": 30,
  "encoder_limit_fps": 60,
  "idle_encode_interval_ms": 1000,
  "display_refresh_preference_hz": 60,
  "presented_time_sample_limit": 120,
  "presented_time_source": "CAMetalDrawable.addPresentedHandler/presentedTime",
  "physical_presentation_measurement": "unavailable"
}
```

`window_status` adds `capture_current_limit_fps`, `capture_native_visible`,
`source_draw_events`, `source_copies`, `capture_pacing_drops`, `capture_pool_drops`,
`visible_source_copies`, `background_source_copies`, `sampled_monotonic_us`,
`encoder_active`, `encoder_output_frames_current_epoch`, and
`encoder_idle_frames_current_epoch`. Source counters are process-cumulative;
encoder counts reset at an encoding epoch start and retain their last values
after stop. Existing `display_gpu_inflight` remains a JSON **boolean**, with
`display_gpu_inflight_limit: 1` the separate numeric limit.

The real `CAMetalDrawable` presented handler captures its `presentedTime`, then
updates a main-thread-owned ring of at most 120 metadata samples. Each sample
includes `metal_presented_time_seconds`, `source_generation`, `source_sequence`,
`source_frame_available` and `window_epoch`; it contains no image data.
`presented_callback_count`, `presented_time_valid_count`,
`presented_time_invalid_count`, and `presented_time_evicted_count` are cumulative.
Only finite, positive, strictly increasing times enter the ring. Zero, duplicate,
nonfinite or out-of-order values increase the invalid count. Blank clearing
frames are distinguishable by `source_frame_available: false`. Historical samples
from a closed window retain their epoch. GPU completed/submitted counters remain
separate. `physical_presentations` is null: this probe is not a photon detector,
input-to-display latency measure or a validated benchmark.

Public API evidence reviewed on 2026-10-09:

- SDK: `/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/System/Library/Frameworks/Metal.framework/Headers/MTLDrawable.h`,
  SHA-256 `6ded45a7ca00e7ff0e8cfbe9799aedc0d84bcb65c35c679da2eadf0c5e777eaa`.
  The header documents the presented callback and host timestamp, explicitly
  including zero for unpresented/skipped frames. It warns that scheduled
  presentation delay can affect intervals derived from this timestamp.
- [Apple presentedTime](https://developer.apple.com/documentation/metal/mtldrawable/presentedtime)
  describes seconds in the host clock and zero for unpresented/dropped drawables.
  The official Markdown response was retrieved over normal verified TLS after
  the web reader rejected its content type.
- [Apple addPresentedHandler](https://developer.apple.com/documentation/metal/mtldrawable/addpresentedhandler(_:))
  is the public callback API, also declared in the SDK above.

Offline tests cover 600 rational 60 Hz admissions without integer rounding drift,
30 Hz background admission on that source, same-timestamp/late-event catch-up
rejection, visibility policy transitions, and bounded timestamp filtering with
synthetic values. Those values are explicitly **not** evidence of actual
presentation callbacks. Existing real buffer ownership, headless compile,
shader/pipeline creation, signatures and no-overwrite checks also pass. Root must
now measure actual visible/background cadence, remote count isolation and whether
this host returns useful nonzero presentation timestamps.

## Stage7: HEVC aspect-fit and explicit display geometry

The root agent's real stage6 2560×1440 source test exposed an early HEVC size
rejection followed by H264 fallback. Stage7 fits that owned source to at most
1920×1080 for HEVC, using the same separate owned scaling buffer and synchronous
Metal copy already used by H264. It does not crop, stretch or upscale. The fit
rounds coded dimensions down to even values. H264 retains its 1280×720 box.
Actual HEVC SPS Main / tier0 / level≤120 verification and required-hardware
VideoToolbox verification are unchanged; selecting a valid geometry cannot bypass
either gate. Source cache/native display remain at Guest resolution. Codec pacing,
frame duration, idle refresh, pool leases, encoder completion and queue limits
are unchanged.

Geometry tests include 2560×1440→1920×1080, 1080×1920→606×1080,
3840×2160→1920×1080, unchanged 640×480 / 1280×800 / 2×2, H264
1280×800→1152×720, and invalid zero/odd/oversized/too-thin sources. Source bounds
remain 3840×2160, so a 1440×2560 portrait source is still outside this capture
contract. These are geometry tests, not a substitute for the root agent's actual
HEVC encoding/browser decode retry.

Window status now reports:

- `drawable_width`, `drawable_height`: current Metal drawable pixel dimensions.
- `backing_scale`: current NSWindow backing scale, zero when closed.
- `window_content_width_points`, `window_content_height_points`: AppKit content size.
- `display_source_width`, `display_source_height`: current published display frame,
  zero when the display has no frame (including closed).
- `viewport_width_pixels`, `viewport_height_pixels`: aspect-fit viewport inside
  the drawable; black letterboxing does not count as displayed Guest pixels.
- `source_width`, `source_height`, `source_frame_available`: independent valid
  capture cache, available even with the window closed.
- `encoder_width`, `encoder_height`: actual configured active encoder dimensions,
  zero before encoder creation / after close.

These fields distinguish a 2560×1440 Guest from a smaller Retina drawable or
letterboxed viewport. A Guest resolution alone does not prove 1:1 native 1440p.
Window geometry and capture metadata are sampled on their respective owner
threads, not an atomic cross-thread input coordinate authorization token.

Frozen stage7 executable SHA-256:
`a722ac90be8684da56b2ba7195e2080ae896547942e22a15a430a99ea564336c`.
The complete offline suite passes, including unchanged strict headless build,
real shader/pipeline creation, buffer leases and pacing/presentation policies.
No VM or window was started by this implementation task.


## Stage8: immutable displayed-frame precursor

See [component schema, mapping and verification](../../archive/docs-evidence/qemu-displayed-frame-component.md).
Stage8r3 adds read-only actual-presented provenance and pure backing-to-Guest
coordinate conversion. No native input/authority RPC is implemented or enabled.

## Stage 9: focus-driven native control through the controller

The optional trusted broker arguments are `SOCKET --persistent --control-socket
ABSOLUTE_CONTROL_SOCKET --vm-id ID --worker-instance 64_LOWERCASE_HEX`. With no
arguments, the native window remains view-only. Connection/attachment happens on
one dedicated background thread and never blocks SPICE's ready/boot handshake.
The direct parent and socket must be private, owned by the current UID, non-link
objects, with matching pinned identities and a same-UID Unix peer. This is the
existing local trusted-user boundary, not isolation from another process of that
user. No public listener is added.

`native_window_protocol:2`, `native_input_protocol:1`, and
`native_input_supported:true` describe the compiled path. `native_input_available`
requires an attached healthy controller connection. Input follows focus: when the
window is the key window of the active application, visible and not miniaturized,
with no modifier keys held, the view becomes first responder and the client sends
`begin`. `input_enabled` becomes true after a successful `begin` ACK. Focus loss
sends `suspend`. No global key hook, Accessibility API, host CGEvent synthesis,
SPICE input channel, or direct QMP input is used.

The native wire messages are `begin`, `heartbeat`, `event`, `suspend` and
`detach`. Each `begin` ACK carries a new activation `epoch`, a per-connection
counter that rejects stale frames and events from an earlier activation; it is
not a lease. States are `connecting`, `view_only`, `beginning`, `active`,
`suspending`, `suspended` and `cleanup_unconfirmed`.

`window_status` separates `native_input_state`, `native_input_ready`,
`native_release_confirmed`, and `native_input_epoch` from window visibility.
These are last confirmed local observations; when state is
`cleanup_unconfirmed`, they cannot prove the controller's current state.
Closing the window synchronously disables local input and queues release-only
suspension; its window ACK **does not confirm input release**. The independently
received `suspend` ACK confirms release and echoes the current activation epoch.
Focus loss, miniaturization, geometry/source invalidation, input queue overflow,
and semantic event denial likewise suspend. Protocol/connection/cleanup failure
permanently fences this client connection; no implicit retry occurs. Helper process EOF
lets the controller perform its connection cleanup, and does not itself attest
release success.

One request is in flight, <=8 KiB JSONL each, with an 8-second socket operation
bound; idle heartbeats are scheduled every 500 ms. This is not a hard release
latency guarantee. At most 32 transition items and one coalesced move are pending.
Buttons/scroll carry their own position, so dropping an obsolete pending move
cannot relocate a click. Overflow clears pending work, disables input and requests
release; it does not silently drop an up event while leaving input active.
Queued events older than 200 ms are rejected locally. A late `begin` ACK after
focus loss is immediately suspended instead of rearming.

The event captures the immutable presented record at AppKit event receipt and
maps its backing coordinate through that record's viewport. Letterbox/outside
coordinates reject (an outside drag suspends to release held buttons). Historical
validation keeps at most eight records for at most 250 ms, plus the current static
record without inventing a new capture time. Any source/window/layout invalidation
clears all of them. `validate_displayed_frame` returns `{valid:true,frame,current}`
only for an exact eight-field token (display instance, presentation serial,
window/invalidation epoch, source generation/sequence, source width/height).
This check supplies provenance; the controller still validates the activation
epoch. Static frames do not expire merely because the desktop is unchanged.

Supported physical key codes are the existing controller subset (letters, digits,
navigation, Return/Escape/Tab/Space, modifiers and F1–F12). Repeat downs are ignored;
Guest typematic behavior is not a Unicode/text-input guarantee. Side-specific
modifier flags use public SDK `IOKit/hidsystem/IOLLEvent.h` definitions
(`NX_DEVICEL/R*KEYMASK`), rather than conflating left/right aggregate modifiers.
Only presses tracked by this view can generate corresponding releases. Input does
not begin while modifier keys are held. OS-reserved shortcuts and broader
keyboard layouts remain compatibility work.

`test.sh` includes real private Unix sockets with a scripted controller peer:
blocked event ACK, 1,000 move coalescing, transition overflow, suspend ACK epoch
echo, late `begin` cancellation, and malformed sequence fencing.
These tests do not establish Guest input delivery. Actual Guest button/key receipt,
and window-close held-button cleanup require the root Agent's separate disposable-VM acceptance. CUA's complete-key API cannot establish
an independently held physical key; no such actual held-key claim is made here.

Runtime JSON booleans are explicit `@YES`/`@NO`. In Objective-C, boxing a C
logical/comparison expression (for example `@(_ready && !_fatal)`) can produce
an NSNumber integer, even though the condition is logically Boolean. The pure
probe fallback did not exercise the configured client and therefore missed this
preboot integration failure. Regression tests serialize actual client statuses
through Foundation JSON and check CFBoolean types after deserialization in
connecting, attached, active, pending, suspended and failed states. The same
check covers both current and historical `validate_displayed_frame` results.
Window visibility/source-availability fields retain explicit Boolean literals;
typed BOOL variables for GPU in-flight/visibility remain Boolean fields. Numeric
0/1 must not be accepted by the Rust capability validator.

### Application activation and input focus

`begin` requires both an active application and its key window. Event
admission additionally requires the image view to be first responder, which the
window sets before `begin` and again on the first ready update; it does not
repeatedly steal focus back. Subsequent first-responder loss suspends, as do
`applicationDidResignActive:` and the existing key-window/visibility callbacks.
The 50 ms common-mode UI timer and status query recheck the same state. These are
local event-loop checks, not a hard OS scheduling/remote release deadline.

`window_status.native_input_focus` exposes strict JSON booleans
`application_active`, `window_key`, `first_responder` (specifically the image
view), and `input_target_focused`. Root's earlier target-window AX Raise is not
proof that the native application actually deactivated; the new fields permit a
separate actual check. Returning focus to the window begins a new activation
(at most once per second).

Public references: [Apple NSWindow.isKeyWindow](https://developer.apple.com/documentation/appkit/nswindow/iskeywindow)
defines the key window relative to the application;
[NSApplication.keyWindow](https://developer.apple.com/documentation/appkit/nsapplication/keywindow)
describes the current keyboard-receiving window and notes inactivity may yield
nil. The installed macOS SDK `AppKit.framework/Headers/NSApplication.h` provides
`isActive` and `applicationDidResignActive:`, and `NSWindow.h` provides
`firstResponder`. Pure policy tests cover inactive-but-key and active/key with
wrong first responder; existing real Unix cancellation tests cover queued and
in-flight input cancellation. They are not an actual GUI deactivation test.

### Passive copy timing (native helper only)

`window_status.copy_timing` version 1 reports two independent rolling windows,
`source_copy` and `encoder_resize`, each retaining at most 120 attempts. Lifetime
`attempts/succeeded/failed` counters are separate from `window_samples`; they
saturate at the JSON safe integer limit and set `counters_saturated`.

Each window exposes `cpu_total_us`, `commit_wait_us`, and `gpu_execution_us` with
`valid_count`, `invalid_count`, `min`, nearest-rank `p50/p95/p99`, and `max`.
No valid samples yields null statistics. CPU time uses GLib's monotonic clock:
entry to the post-wait/pre-statistics timestamp (including first-use shader/pipeline
setup), and immediately before commit to that same post-wait timestamp. Recording
lock/aggregate overhead and final return/ARC destruction are excluded. Commit-wait
includes API submission and CPU scheduling/synchronization; subtracting GPU time
from it does not establish pure GPU queue wait. GPU duration uses the completed command's public
`GPUStartTime`/`GPUEndTime`, requiring finite positive ordered timestamps; missing,
zero, invalid or unsuccessful-command timestamps are counted invalid, never as
zero-duration work. CPU/GPU clocks are not subtracted from each other.

These are aggregate copy measurements, not source capture age, ACK latency,
window drawable presentation, display scanout, input-to-photon, or physical FPS.
Windows can span source generations and geometry changes; no per-frame identity
or pixels are recorded. Multiple status windows may overlap: **do not combine
their percentiles into a whole-run percentile**. There is no raw trace mode,
encoder/scheduling change, or altered producer ACK ordering. Headless compilation
excludes this module and instrumentation.

Native input diagnostics expose `native_input_suspension_count` and the latest
at most eight `native_input_suspensions` through `window_status`. Each `begin`
starts a new first-cause record: subsequent release/focus callbacks
do not overwrite that cause. Fixed reason labels distinguish focus, source,
layout, queue overflow/age, unsupported key and remote rejection. The optional
`queue_age_ms` is sampled at dequeue, before RPC, and is not end-to-end latency.
An optional `remote_diagnostic` contains only version 1, an allowlisted stage and
bounded integer worker elapsed milliseconds. Neither record contains keys,
coordinates, frame payloads, identities or arbitrary peer messages. Invalid
remote diagnostics are omitted; the original rejection and cleanup still occur.
These observations do not alter the 200ms queue age, frame validation, focus or
release-ACK rules and do not prove guest delivery.
