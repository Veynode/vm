# Local native desktop WebRTC integration

`vm-media-local` is a **loopback-only integration diagnostic**, separate from the
control daemon and its authorization boundary. It receives browser offers,
selects a genuinely available native hardware encoder, and relays framed Annex B
access units from the actual SPICE console over a real DTLS/SRTP WebRTC connection.
It serves only the explicitly listed browser receiver assets. No recording is
made by the native live encoder or Rust sender.

```sh
cargo build -p vm-media-local
native/tools/build-spice-video-stream.sh
npm --prefix browser ci
npm --prefix browser run build
# SPICE_SOCKET must belong to the independently started diagnostic VM.
target/debug/vm-media-local .local/bin/spice-video-stream SPICE_SOCKET browser 8787
```

The bundled fixture wrapper in `native/tests/test-spice-video-stream.py
--live-browser` starts an owned corrected Linux graphics fixture and server. Its
`.ready.json` states the URL and exact `.stop` marker; the wrapper has an180second
limit and reaps its children. Neither server shutdown nor browser disconnect
terminates a user VM: the fixture owns its disposable VM separately.

The server binds HTTP and WebRTC UDP to127.0.0.1, uses no external ICE servers,
checks exact Host and Origin for mutations, caps JSON bodies at256KiB and admits
one session. The browser supplies a random request ID. Close requests cancel
pending negotiation as well as established streaming; a bounded60second cancelled
ID table prevents a delayed offer from resurrecting a cancelled session. Full
admission is refused when that table is full. Unconnected sessions have a12second
connection deadline; connected streams have a180second limit. Closed sessions
reap only their own native child. These safeguards do not constitute remote user
authentication. **Do not expose this diagnostic through a proxy or public tunnel.**

At startup `--probe-codecs` actually creates, encodes and drains one small frame
with VideoToolbox required-hardware sessions; every live stream again verifies
hardware at its actual dimensions. Browser capabilities and decoded RTP frames
control selection; HTML video support is irrelevant. H265 is Main8-bit and H264
uses Constrained Baseline level3.1, scaling larger source surfaces proportionally
within1280×720 in the native live path. Browser control coordinates must later
map encoded dimensions to source dimensions; this diagnostic has no input API.

The Rust sender rejects oversized/malformed/nonmonotonic access-unit headers and
requires the first frame to be a keyframe starting at timestamp0. RTP timestamps
are calculated from the actual source microsecond timestamp using90kHz, with
32-bit wrap. It never guesses frame boundaries from individual NAL units. Frame
writes have a500ms deadline and the native pipe writer also has a deadline. These
bounds fail a stalled connection rather than queueing unlimited frames. They do
not yet implement the required production latency scheduler, adaptive bitrate,
seamless dynamic resolution, concurrent encoder budgeting or sleep/wake. Both
native helpers refresh an already captured, valid idle frame at 1 fps; this
bounded correctness mechanism is not adaptive idle/resource management.

RTCP PLI/FIR requests addressed to this bound sender are forwarded to native
`keyframe` commands. Feedback is rate-limited and bounded, unrelated SSRCs are
ignored, and stale FIR sequences do not repeatedly force encoding. The receiver
feedback future runs alongside the complete frame reader; cancelling a partial
`read_exact` on every feedback event would corrupt framing and is avoided.
Actual first-keyframe loss recovered before the periodic keyframe in Chromium
and Safari. The legacy per-stream helper still needs an initial SPICE draw: idle
refresh cannot manufacture a first frame for a new consumer on a static Guest.
The persistent broker below avoids that cold-consumer dependency by retaining
its capture connection from VM startup.

Explicit diagnostic-only fault options are disabled by default:
`--fault-drop-first-keyframe` omits the first complete access unit to test real
receiver recovery, and `--fault-suppress-hevc-frames` continues native hardware
HEVC encoding but suppresses its RTP output to test bounded first-frame fallback.
Neither option substitutes a mock source. Never enable them for normal sessions.

The implementation uses pinned `webrtc`/`rtc`0.21.0 APIs from the
[upstream Rust implementation](https://github.com/webrtc-rs/webrtc), specifically
its `play-from-disk-h26x` sample for codec/track/negotiation API usage. It does not
replay a disk recording: the live source is an owned child reading the VM's public
SPICE IOSurface, using one Metal copy before hardware compression. Dependency
licensing/security audit and distribution rights remain release gates.

DTLS/SRTP encrypts this peer transport, but the localhost HTTP signaling and served
browser code are trusted by this diagnostic. Device pairing, authenticated
fingerprint/key binding, remote grants, TURN and Internet E2EE acceptance are not
implemented here. Initial real-browser evidence and profile-limit corrections
are retained separately; no Parallels/performance or release-readiness claim.

## Persistent VM console

The production QEMU worker can now own `vm-console` independently of any browser
connection. It starts QEMU paused, waits for the capture helper's ready event,
then resumes Guest CPUs. A ready event means the capture connection exists;
Guest pixels may still be unavailable. The broker keeps one native SPICE
connection and one owned latest surface cache. Browser encoding starts and
stops against that cache, so a static desktop does not depend on a new SPICE
draw after reconnect. Genuine producer loss invalidates the cache.

```sh
cargo build -p vm-media-local --bins
native/tools/build-spice-console.sh
# QEMU worker normally owns this broker. PRIVATE_DIR must already be mode0700.
target/debug/vm-console .local/bin/spice-console SPICE_SOCKET PRIVATE_DIR/console.sock
# In a separate process, using the worker's existing private socket:
target/debug/vm-media-local --console PRIVATE_DIR/console.sock browser 8787
```

The socket is mode0600 and trusts the local account; it is not a remote
permission boundary. Requests are JSON lines, max4096 bytes, with a string
`request_id` (max64 bytes), `method`, and optional `codec` (`hevc` or `h264`).
Methods are `capabilities`, `status`, `frame_status`, `screenshot`, `start`, `keyframe`, and
`stop`. Unknown fields/methods fail explicitly. Responses are framed:4-byte
big-endian body length,1-byte kind,8-byte big-endian epoch, then body. Kind1 is
JSON containing request_id and result/error, or an asynchronous event. Kind2
body is8-byte big-endian PTS in microseconds,1-byte keyframe flag and Annex B.
Bodies are bounded to4MiB. Encoder epochs are broker-assigned, monotonically
increasing, and scoped to the connection that started them. Start acknowledgement
precedes access units; another connection cannot stop or force its keyframe.

`frame_status` reads current capture connectivity, cache validity, source generation/sequence and physical source dimensions from the native capture loop. Invalid cache dimensions are zero. It supports freshness checks but is not atomic with a separate QMP input request.

Eight clients, sixteen pending native calls, one screenshot and one encoder are
allowed. Per-client output queues hold three records; a slow consumer is closed
instead of accumulating unbounded latency. Unix sockets are explicitly
nonblocking and use poll against absolute deadlines: on Darwin, MSG_DONTWAIT
alone did not reliably bound a blocking socket send. Native command writes and
client output writes have500ms deadlines. Native replies have a12second deadline
(shutdown:1second). A rejected, missing or malformed native stop acknowledgement
fences the capture owner and is never converted into successful cleanup. A stop
reply must explicitly report `state:stopped`; a session-ended event must carry
matching, nonzero wire/body epochs for the current owner. Old-epoch events and
access units cannot release or feed the new owner. Native stop must drain before
ownership is released. The pipe shim `vm-console-stream` preserves
the original sender framing and confirms its stop before exiting successfully.
An explicit stdin stop followed by EOF does not bypass that acknowledgement wait.
HTTP close also requires explicit cleanup attestation before codec fallback.

Static video can retain a single pixel hash while decoded/presented frame counts
advance. The browser gate records pixel changes independently and accepts five
actual decoded/presented frames, with a6second progress window after first frame.
The native helper refreshes idle encoding at1fps; this is a bounded correctness
path, not complete production network adaptation or multi-VM encoder budgeting.
Real static-source Chromium HEVC/H264 sessions and confirmed disconnect now pass
through the persistent path; source sequence and capture timestamps remain fixed.
Source replacement ends the current encoding epoch and requires a new start;
producer loss invalidates the cache. Broker loss leaves QEMU running but capture
unavailable, with recovery currently requiring VM restart.

Eleven real broker/shim process and Unix-socket tests cover ownership races,
start ordering, EOF, truncated native output, native stop errors/timeouts,
malformed acknowledgements and inconsistent session-error epochs. Their native
fixture is synthetic and proves no graphics or hardware encoding. Reproduce with
`cargo test -p vm-media-local --test console_protocol`. See
[broker protocol evidence](../../docs/evidence/console-broker.md),
[persistent control/browser evidence](../../docs/evidence/control-qemu-persistent.md)
and [native capture evidence](../../docs/evidence/apple-spice-console.md) for the
separate accepted paths and preserved failures.
