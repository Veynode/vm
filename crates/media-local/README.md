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

The default bare diagnostic binds HTTP and WebRTC UDP to127.0.0.1 and uses no
external ICE servers. Explicit paired mode can supply the bounded trusted ICE
configuration described in [the security model](../../docs/security.md). The server
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
use explicit source-frame geometry instead of treating encoded dimensions as
control coordinates. The bare diagnostic grants no input authority; the paired
control path below separately checks scope and frame identity.

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
browser code are trusted by this diagnostic. The bare mode does not authenticate
remote users. The explicitly configured paired path implements signed
offer/answer fingerprint binding and scoped grants; see pairing evidence (`vm-archive/docs-evidence/session-auth/`) and paired control below. Trusted
short-lived ICE configuration and local TURN tests are separate from default
host-only behavior; see TURN cleanup evidence (`vm-archive/docs-evidence/turn-allocation-cleanup.md`). Internet deployment/E2EE acceptance remains unverified.
No Parallels/performance or release-readiness claim follows from these local tests.

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
broker protocol evidence (`vm-archive/docs-evidence/console-broker.md`),
persistent control/browser evidence (`vm-archive/docs-evidence/control-qemu-persistent.md`)
and native capture evidence (`vm-archive/docs-evidence/apple-spice-console.md`) for the
separate accepted paths and preserved failures.

## Paired encrypted console control

With production `--console SOCKET` plus `--auth-state DIR --auth-namespace NAME --vm ID --control-state DIR`, confirmed paired sessions may use the exact reliable ordered `veynode-control-v1` / `veynode-control/v1` DataChannel on the signed peer. Observe-only grants receive fresh snapshots; input additionally requires explicit scope and a fresh screenshot's frame identity. `/auth/control` is absent. Native browser code rejects a server that advertises control without this transport version. See real acceptance and reproduction (`vm-archive/docs-evidence/browser-control-channel/`).

No command, clipboard or MCP protocol is exposed by this proprietary channel. The separate explicitly enabled `--allow-files` path requires strict Guest binding and a `files` grant; it grants no input authority. See actual file-channel evidence (`vm-archive/docs-evidence/browser-files-live/`). HTTP signaling remains loopback-only. Limits apply to application messages and RTC queues separately; this is not a claim of Internet or total-memory acceptance. Freeze a single binary for Keychain-backed pairing administration and serving; executable identity changes are a separate upgrade/signing gate.

Control replies have three forms. `{"sequence","result"}` is success. `{"sequence","error":"control rejected"}` is a refusal that fences the session's control; the service prints `control channel failure category=<word>`, where the word is a fixed sub-reason (for example `request_replay`, `input_authorization`, `vm_status_mismatch`, `frame_anchor`, `controller_unavailable`, `controller_reply_invalid`, `screenshot_reply_too_large`, `screenshot_reply_invalid`, `png_decode_corrupt`, `frame_verification`) and `dispatch_rejected` only for what has no word yet. `{"sequence","error":"frame unavailable","reason":<word>}` answers `observe_frame` alone and fences nothing: the request was authentic and in sequence, but the controller had no usable picture (`controller_refused`, `controller_deadline`, `png_decode_unsupported`, `scaled_frame_over_cap`). The session and the video stay as they were; the service prints `control frame unavailable reason=<word>`. No log line or reply carries controller or Guest error text. Tested offline only (2026-10-12); the browser page does not accept the third form yet.

### Guest capture cleanup reconciliation

With strict Guest observation binding and a receipt-v1 encoder/service, a failed
encoder may preserve its original random capture receipt for a subsequent signed
close. Only positively reaped host children and successful input/channel/peer
cleanup are eligible. Unconfirmed completed sessions block new offers, never age
into success, and cause shutdown failure. The independent Guest query must return
exact-instance confirmed cleanup with terminal EOF.

`--guest-cleanup-bridge ABSOLUTE_PRIVATE_JSON` optionally supplies a repaired
listener's exact controller `binding` object (same VM and worker) solely for that
query. Require owner0600/single-link/no-symlink file in owner0700 directory; replace
atomically. Controller proof is checked before and after, one total3s deadline,
no parallel query. This never repins media or authorizes input. Omit the option
for recovery through the originally pinned listener. Ledger retention is120s/32
terminal captures; no proof on expiry, service restart, missing receipt or failed
host cleanup. Durable restart recovery remains unfinished.

Actual HEVC bridge loss, H264 encoder crash and same-server reconnect (`vm-archive/docs-evidence/browser-cleanup-recovery-live/`).

### Optional local encoder rate actuator diagnostic

The default sender does not open a rate channel or change encoder rate targets.
For either the direct Guest encoder or `--console` (with a compatible sibling
`vm-console-stream` and broker/helper), `--encoder-rate-control` enables a private
inherited Unix socketpair. After the first complete AU, the sender verifies a
hardware-backed typed status. This does not add an HTTP endpoint or grant browser
control of encoder properties.

Alternatively, `--encoder-rate-trial 2000000,250000,2000000` runs exactly three
local target updates with a six-second dwell after each acknowledgement, beginning
after the first complete AU.
The final target remains active for that media session. Each new media session
gets a new encoder identity and repeats the explicitly requested trial. The
three decimal targets must be high/low/same-high within 250,000–40,000,000 bps.
The two CLI modes are mutually exclusive. This finite diagnostic is **not ABR**,
network estimation, an output bitrate guarantee, or a congestion algorithm.

The existing capability probe must advertise rate protocol 1 and the matching
bounds; no extra encoder probe is spawned. The owning media process serializes
requests, binds every update to the acknowledged encoder instance/epoch and next
sequence, checks readback and nondecreasing submission boundaries, and logs only
fixed operation metadata. The internal `Client::status`/`update` methods are the
future scheduler integration point; they are not externally accessible.
Replies are strict bounded JSONL (1,024 bytes), with one absolute three-second
write/read deadline. Malformed, stale, partial, disconnected or cancelled exchanges
fence that channel and end streaming through the existing cleanup path; raw reply
or parse errors are not logged. Rate status/readback is not measured bitrate or
proof of frame delivery. The private endpoint remains owned while the encoder is
sent `stop` and reaped, so normal cleanup does not first trigger actuator EOF.

The focused `encoder_rate::tests` cover actual local Unix streams (stale identity,
sequence/readback/submission regression, malformed/duplicate/oversized/partial
records, EOF and cancellation) and an owned Python subprocess with the real FD
inheritance and encoder stop/drain helper. That subprocess deliberately supplies
synthetic receipts; it proves process/pipe lifetime, not VideoToolbox, SPICE,
Guest capture or browser acceptance. Actual hardware/browser validation is a
separate gate using immutable compatible encoder and media binaries.

### Optional bounded receiver-loss response

`--encoder-loss-response` is a separate opt-in mode, mutually exclusive with
`--encoder-rate-control` and `--encoder-rate-trial`. After the first available
access unit and status receipt, it requests an initial 2 Mbps target, with a
2 Mbps ceiling and 250 kbps floor. Initial access units can precede this ACK
and use the encoder's existing default target; this is not a first-frame cap.
Each confirmed peer
gets its own observer and private actuator channel. Defaults, paired grants,
HTTP routes, codec negotiation and capture-cleanup proofs are unchanged.

The observer is below retransmission generation in the pinned interceptor chain.
It observes actual outgoing Sender Reports and incoming Receiver Reports for the
bound local media SSRC. It rejects invalid/replayed/ambiguous sequence or signed
24-bit cumulative-loss data and requires a known fresh LSR/SR association. A
sufficient window covers at least 128 intended packets: loss≥10% requests half
the last acknowledged target; three consecutive sufficient windows at≤1% permit
an incremental 10% increase. Decisions have a 2-second cooldown and remain within
the fixed target range. Missing/stale feedback freezes recovery; it does not
terminate a valid static desktop or invent low loss. The bounded shared observer
continues collecting while one private rate request is in flight. Only an actual
correlated native ACK updates the driver's target. Private-channel failure ends
streaming through existing cleanup; it is never retried as a successful update.

`--fault-rtp-loss` is legal **only** with `--encoder-loss-response`. It runs once
per local diagnostic peer: 5 seconds clean from the first RTP packet, 8 seconds
dropping each selected tenth sequence (including its retransmissions), then
restores forwarding automatically. It has hard packet/sequence budgets and does
not delay or retain media payloads. Exceeding the bounded fault budget fails the
diagnostic. These counters measure interceptor admission/forwarding, not actual
UDP delivery, bandwidth estimation or a wire-rate cap. Do not enable this fault
for ordinary sessions.

Loss diagnostics are version 1 single-line JSON: `receiver_loss_feedback` at most
once per second during ordinary polling, with extra records on decisions or
failure; `receiver_loss_rate_request` before transmission; and
`receiver_loss_rate_ack` only after validated readback. They include session-
relative monotonic `elapsed_ms`, fixed reason/observation IDs, actual triggering
RR deltas in `cause` (null only for the initial target),
freshness/rejection counters, local fault totals and requested/acknowledged
encoder instance/epoch/sequence/target. `diagnostics` is the latest observer
snapshot and may describe a newer report than the retained decision cause.
Failure records use fixed categories,
never packet bodies, SDP, credentials or raw native errors. A property target or
readback is not measured output bitrate. This bounded loss response is not a
complete congestion controller or Internet bandwidth guarantee.

### Selective media stop at local UDP submission

Each sender enables a per-Peer media egress gate after codec negotiation and
before spawning its encoder. On stream completion, cancellation, source failure
or detected authorization loss, main irreversibly fences RTP **before** awaiting
input, file or native encoder cleanup. This covers queued original packets and
NACK retransmissions at the final local socket poll, including homogeneous GSO
batches and TURN ChannelData/Send Indication output already queued for a known
TURN server. Pending media sends wake on fencing. A synchronous send poll already
in progress finishes before the fence returns; previously accepted kernel or
remote bytes cannot be recalled.

The gate preserves valid DTLS/SCTP, SRTCP and ICE/TURN control traffic, including
allocation deletion, so existing cleanup acknowledgements retain their meaning.
The existing IPv4 mDNS socket has a separate transport-bound, strict single-A
query/answer validator; a peer merely using port5353 receives no exception.
Unknown DNS shapes, missing socket identity and unsupported IPv6 mDNS output
fail closed.
Unknown framing and TCP output in this enabled mode fail closed. This is the
current UDP media product path, not a promise that arbitrary RTC transports or
RTP/RTCP payload collisions are supported. Every bound RTP/RTX payload and original driver submission must avoid
64–95, where marker-bit RTP is ambiguous with RTCP multiplexing
([RFC 5761 §4](https://www.rfc-editor.org/rfc/rfc5761#section-4)).
Enabling after the first RTP admission is rejected. Classification applies to
this trusted RTC core output, not arbitrary externally supplied datagrams; the
SRTCP type range is a mux distinction, not a replacement for authentication.
TURN UDP ChannelData accepts the exact unpadded or four-byte-aligned length
([RFC 8656 §12.5](https://www.rfc-editor.org/rfc/rfc8656#section-12.5));
channel numbers outside the current 0x4000–0x4fff implementation fail closed. The default
upstream wrapper remains unchanged unless its gate is explicitly enabled.

Version 1 `media_egress_fence` JSON records use an independent non-authority
`fence_id`, `phase` (`before_cleanup` or `after_peer_close`), `elapsed_ms` and
`counters`. `media_submitted_datagrams` and `control_submitted_datagrams` count
completed local socket polls; `blocked_media_datagrams` counts deliberate
post-fence suppression. `pending_media_send` is a Boolean for a locally pending
media future; `rejected_datagrams`, `enabled`, `fenced`, and `failed` expose the
remaining fixed state. `rejection_reason` is null or the first fixed failure
category; it never contains the offending bytes. `mdns_send_errors` counts
actual mDNS socket polls returning an I/O error. These definite non-submissions
do not invalidate an RTP stop proof; identity, framing and partial-send failures
remain sticky failures. Cancelling a pending send is not counted as an I/O error.
No packet/address/credential is included. A normal close
may block zero packets and does not by itself validate the pending-send race.
The stop latch alone is not a frame-age mechanism. The local AU deadline extension
below adds an original sender-budget check, not a pacer/BWE/output cap. Native cleanup failure, Peer close failure or TURN deletion failure still
prevents a successful cleanup receipt.


### Original AU deadlines at final local submission

The media application now also enables a bounded deadline table for the one
negotiated H264 or H265 encoding before encoder startup. It checks the actual
sender coding SSRC and payload under the same core lock used for mutation, and associates an optional RTX tuple only when
both coding RTX SSRC and a matching negotiated `apt` codec exist. FEC and multiple
encodings/senders are refused in this mode. Sender parameter/track changes and
renegotiation are explicitly unsupported once enabled; even a no-op setter is
rejected before touching the core, without falsely invalidating cleanup proof. The upstream wrapper remains opt-in.

After packetization and before the first RTP enqueue, an exclusive metadata
writer registers the AU timestamp with the **unchanged** deadline from the
relative sender-lag scheduler (at most 250 ms remaining). No capture wall clock
is inferred, no PTS is rewritten, and an AU already expired before enqueue is a
media policy failure. The 128-entry table keeps immutable deadlines. It retires
only expired entries, only when capacity is needed; it never evicts a live AU.
Forward serial timestamp arithmetic and a cumulative span below 2^31 ticks
prevent wrap/reuse aliases within the Peer incarnation. Duplicate timestamps
cannot refresh eligibility. Idle expired entries do not themselves stop media.

The pinned ordinary NACK responder clones the original timestamp; RTX preserves
it while changing SSRC/PT/sequence and prepending the original sequence to its
payload. Final SRTP header lookup normalizes only the configured primary/RTX
tuples to their original AU. It never uses the retransmission's new enqueue
instant as its age. UDP/TURN and every GSO segment share the same lookup. The
final socket mutex checks the earliest original deadline immediately before each
poll. This is eligibility at the nonblocking poll, not a known kernel acceptance
timestamp: preemption can occur between checking and completion. A full poll
returning after the deadline remains counted as a submission, increments
`completion_overrun_datagrams`, and causes a policy stop. A pending media send arms one timer for that absolute deadline and can
also be awakened by explicit stopping. Kernel-accepted bytes cannot be recalled;
this is a local submission budget, not measured receiver latency or a wire cap.

Known expiry sets `policy_stop_reason:"au_deadline_expired"`, fences all further
media, and makes the media session fail. It does **not** invent unconfirmed
cleanup when native, authority, Peer and TURN cleanup actually succeed. Even an
expired retransmission of a formerly complete AU stops the session; seamless
recovery is not claimed. Unknown/evicted identities, binding changes, unsafe
framing and partial UDP writes remain conservative `failed` outcomes. There is
no automatic IDR recovery after partial-AU expiry. Control traffic remains
available for real cleanup. NACK expansion and cache bytes are bounded by the product responder described below;
all downstream RTC queue memory remains a separate boundary that this table does not solve.

The existing fixed JSON counters add `au_deadlines_enabled`,
`registered_access_units`, `au_records`, `au_records_high_water`,
`expired_datagrams`, `completion_overrun_datagrams`, `unknown_au_datagrams`, and
`policy_stop_reason`. `expired_datagrams` counts all suppressed batch members
when its earliest deadline expires, including siblings whose own deadline has
not yet passed; it is not an individual packet-age measurement.
`--fault-au-egress-hold` is an explicit finite localhost diagnostic: ten seconds
after the first successful full media socket submission, the next final media
poll is deliberately held Pending, while its original deadline keeps running.
It affects neither control polls nor default behavior, creates no queued fault
traffic, and does not delay main's RTP enqueue to simulate the missing boundary.
`fault_au_hold_armed`, `fault_au_hold_triggered`, and
`fault_au_hold_after_first_submit_ms` record that exact relative trigger. A known
expiry remains a failed playback result even if the signed close receipt can
truthfully acknowledge cleanup.

### Bounded NACK repair work

The single-stream media path replaces the default NACK responder with the
product `BoundedNack` interceptor. It deduplicates within each compound request
and the responder's pending queue before constructing repair packets. Cache
limits are 1024 packets/1 MiB, with at most 2048 serialized bytes per cacheable
packet; pending repair limits are 128 packets/128 KiB including RTX's two-byte
original sequence. Cache byte pressure evicts old entries; oversized originals
still forward uncached. Excess NACK work is explicitly suppressed and counted,
not treated as repaired/delivered media. Original RTP never silently drops on a
forward-queue overflow: an independent sticky failure synchronously fences the
Peer even though the pinned interceptor chain swallows handler errors.

Ordinary and inbound passthrough each have 32-record/256 KiB limits. Compact
copies sever large backing allocations; unknown custom attributes are refused.
These copies add CPU/allocation work, not a measured performance improvement.
This bounds this responder's owned storage and per-compound generation work,
not upstream parsing or all subsequent RTC/kernel/network queues. No pacing,
BWE, network rate cap or seamless recovery is introduced. The original AU
final-poll deadline remains authoritative for all repairs.

`nack_budget_snapshot` version 1 logs fixed caps, current/peak cache and queue
wire bytes/counts, dedup/suppression/failure counts and actual unbind/close
state, tied only to the independent Peer `fence_id`. Main requires the actual
close callback and empty resources for confirmed cleanup. See
component boundaries and tests (`vm-archive/docs-evidence/media-nack-budget-component-v1/`).

## Status codes (offer admission)

These are the interface contract for the two offer routes. Owner decision 0021 kept both
behaviours as they are. A change to any row is a public contract change and must be decided on
purpose.

| Condition | `POST /offer` (loopback, unauthenticated, only when auth is unset) | `POST /auth/offer` (signed) |
|---|---|---|
| Codec is not `h264` or `h265` | **400** `invalid offer` (`main.rs` `offer_inner`, the codec test at ~396, reached when auth is unset, ~381-383) | **403** `{"error":"authorization rejected"}` (session-auth `OfferIntent::canonical`, `lib.rs` ~329-332, refused before admission) |
| Hardware encoder for the requested codec unavailable, absent from the capability table, or its entry malformed | **409** `required hardware encoder unavailable` | **409** `required hardware encoder unavailable` (same shared admission path, after signature admission) |

- The 400 versus 403 split is deliberate: the signed route's 403 is part of its authenticated
  contract and does not say why a signed offer was refused, while the unauthenticated route
  names a malformed offer. The 400 also covers a bad request ID, a type other than `offer` and an
  SDP over 200000 bytes. When authentication is configured, `POST /offer` returns **403**
  `signed offer required` for every body.
- The three capability cases (encoder **unavailable**, codec **missing** from the capability
  table, entry **malformed** or any value other than the exact string `verified-hardware`) all
  return the same 409 and the same body. **They cannot be told apart by status code or body.**
  The 409 fails closed: no session slot, peer or encoder exists after it. Other 409 responses on
  this route (`request already cancelled`, `previous session cleanup unconfirmed`, `one
  diagnostic session already active`) have different bodies and are separate conditions.
- Do not build client logic that branches on telling "codec unsupported by this host" from
  "capability entry malformed". Read `GET /capabilities` for the advertised state instead.
- The 180 s media cap on a connected session is the named constant
  `ice_config::MEDIA_SESSION_CAP_SECONDS`, consumed by `run_session` through
  `ice_config::media_session_cap_elapsed`; `src/remote_bounds_tests.rs` fails on any drift of the
  value or its wiring.
