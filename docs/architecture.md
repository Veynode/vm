# Architecture and current boundaries

The product contract is in product-spec.md. This document distinguishes implemented paths from intended design.

## Implemented path

MCP stdio client → Rust `vm-control mcp` → private Unix socket → independent Rust `vm-control daemon` → one backend worker process per VM → real ARM Guest. Apple Virtualization.framework remains the default, through Swift `vm-apple`. Its native console attaches directly through VZVirtualMachineView without video encoding. Closing that console or disconnecting MCP leaves the daemon/VM alive. Daemon loss closes worker stdin and stops workers rather than leaving unmanaged processes; transparent daemon upgrade/reconnection is not implemented.

An opt-in `qemu-hvf` backend is implemented in the same controller: Rust `vm-qemu` owns trusted local QEMU/HVF, virtio-gpu-gl and private QMP/SPICE sockets. Its accepted Linux ARM direct-kernel path supports RAM boot or a pinned raw installed disk with an inherited inode lock, plus default observer-only serial v1 integration and explicit serial v2 execution described below. Installed clone persistence, isolation, actual accelerated pixel checks, MCP info/capture and normal shutdown are accepted in [the real clone test](../archive/docs-evidence/installed-qemu-clones-live/README.md). File transfer over serial, host shares and Windows/macOS Guests remain unsupported by this worker. The opt-in serial v2 path has real installed-Linux command execution, cancellation and recovery evidence; it does not enable those missing capabilities. Real MCP lifecycle, ACPI normal shutdown, screenshots, native console windows and worker/daemon crash cleanup have passed. This is a product control path with a limited implementation scope, not release-ready QEMU packaging or acceptance of all Guest graphics. See [persistent QEMU evidence](../archive/docs-evidence/control-qemu-persistent.md).

When registered, the QEMU worker starts its private `vm-console` broker before resuming Guest CPUs. The broker owns one native SPICE connection independently of MCP and browser clients. Capture-connected readiness does not promise a Guest frame. Screenshots preserve actual source capture time, generation and sequence while assigning a new observation UUID; static cached observations do not pretend a new draw occurred. Unexpected broker loss leaves the Guest running and screenshots unavailable; recovery currently requires VM restart. Scoped QEMU desktop input now uses daemon leases and fresh screenshot anchors; see the signed browser input section below.

The same persistent SPICE owner now presents an AppKit/MetalKit console through its owned BGRA surface pool, without video encoding or a second SPICE consumer. Lifecycle-scoped controller/MCP show, close and reopen operations have passed against the real Guest; closing the window leaves capture and the same remote WebRTC session running. Actual screenshots were independently decoded after these transitions, and the disposable backend-crash check removed the owned worker/broker/native window while the concurrent macOS Guest remained responsive. That crash test had no active browser session. See [native window acceptance](../archive/docs-evidence/qemu-native-window-live/README.md).

The QEMU native stage9r4 window now supports explicit Take Control/Return to Agent and physical keyboard/mouse events through an ordered private native channel. Actual AppKit→worker/QMP→Guest Xorg+evdev takeover/click/Return/drag-close/release checks pass, as does sampled window-focus loss/restore without automatic rearming; see [native input evidence](../archive/docs-evidence/qemu-native-input-fixed-live/README.md). Application deactivation and held-key loss remain component-tested only. Unicode/IME input is not implemented. Window resizing scales the view; it does not change Guest resolution. The tested stage7 path uses a configurable initial2560×1440 source, visible60Hz/background30Hz source-copy ceilings and bounded owned copying. A short API presentation-callback sample and real browser HEVC1080p scaling passed; these do not prove sustained1440p/60Hz smoothness, zero-copy, physical presentation or superiority to Parallels. See [1440p measurement](../archive/docs-evidence/qemu-native-1440-live/README.md) and [HEVC scaling](../archive/docs-evidence/qemu-hevc-scale-live/README.md). Dynamic Guest resolution remains unfinished.

The registry is atomically written with private filesystem permissions. Config registration is not OS installation or disk creation. The Apple adapter supports Linux direct kernel/initrd and optional disk; macOS uses an Apple-restored disk and persistent hardware/machine/auxiliary identity. Windows isn't advertised by this backend. Guest capabilities are explicit. Read-only virtiofs mounts exist only when an explicit shared directory is configured. VM state `running` means hypervisor state, not Guest readiness.

Linux integration: a small statically linked Go binary accepts host-CID2-only virtio-vsock port5000, with bounded newline JSON requests. Using Go here is a reversible implementation choice for reliable arm64 Linux cross-compilation without a guest sysroot; Rust remains the public controller. Command execution is disabled unless explicitly enabled at guest service launch, uses argv (shell only when explicitly requested), enforces deadline/output caps and uses one root-owned cgroup v2 subtree per production command. Go clone3 atomically places the child into its group before executing user code; cancellation, timeout and direct-parent completion kill the subtree and confirm it is empty. Unsupported containment fails startup. No TCP guest listener. The disposable initramfs selects an explicit legacy root/process-group diagnostic flag and advertises descendant containment false. New installed fixtures run the service as root but all command children use a startup-fixed non-root account (UID/GID1000), cleared supplementary groups, fixed HOME/cwd and an actual startup credential probe. Normal shutdown is an independently enabled root service operation. This reduces command privilege; it is not a command sandbox, separate-process privilege boundary or finished desktop agent.

Scopes are set by the local MCP launcher; observation is default. Lifecycle and execution are independent opt-ins. VM control leases use daemon-side owner/epoch checks and local human takeover. Asynchronous command jobs bind VM, originating MCP session owner and original lease epoch. Explicit cancellation and human takeover request full job-cgroup cleanup on the production Linux service; old owners cannot start further jobs. Owners can still poll/cancel their own revoked jobs, but other clients cannot access them. Session EOF/read/write failure, explicit lease release and HTTPS DELETE/expiry revoke authority and attempt cancellation; when the host cannot observe a disconnect, lease TTL and the Guest deadline remain separate bounds while the Guest service is alive. Finished ownership records may be evicted early at bounded capacity, never active jobs or in-flight reservations. Cancellation acknowledgements are distinct from actual finished results, and cannot undo prior side effects. Synchronous commands now compose the same registered asynchronous job path and cancellation; Guest services without that path explicitly fail. Actual production cgroup tests cover setsid/background-grandchild escape and MCP stdio EOF; legacy process-group diagnostics do not provide that containment. The Unix socket trusts the local OS account, which can bypass individual MCP launcher's restrictions by launching a new instance; this is an explicit local trust boundary, not multi-user isolation.

The earlier VZ/vsock execution-service SIGKILL test leaves a distinct recovery window: no in-process handler can run, and a real test observed its orphaned job still populated. A service-lifetime lock prevents competing cleanup owners; restart reaps only root-protected stale job directories before opening the listener. Actual same-binary SIGKILL/restart testing confirms stale group removal and command/files-worker recovery. The mechanism does not promise immediate termination before restart or undo effects performed meanwhile. A cleanup failure stops command admission, cancels peer jobs and prevents successful shutdown reporting merely because the active count reached zero. See [cgroup validation](../archive/docs-evidence/linux-cgroup-2026-10-08/README.md). This lifecycle boundary does not impose filesystem/network isolation, per-job CPU/memory limits, or a sandbox against Guest administrators.

Installed QEMU Linux execution instead requires explicit serial v2 opt-in. Its host
heartbeat and independent guardian fail-stop the original VM when the service
cannot safely acknowledge execution; serial v1 stays observer-only. Actual
service-only SIGKILL testing observed the original worker/guardian/QEMU absent
within1.679s after the arm request, with new execution denied and MCP EOF reporting
cleanup unconfirmed. This terminates the Guest incarnation, rather than proving
that its dead service acknowledged empty cgroups. The earlier failed serial-proof
parser test is retained. See [serial execution decision](decisions/0015-qemu-serial-execution-v2.md).

Before a v2 worker can launch, the daemon durably records its unique incarnation
and a private locked witness descriptor. That same open file description remains
held through worker, guardian and QEMU; QMP verifies its fdset before vCPU start.
Scopes are durably recorded before command dispatch. A restarted daemon loads
these records as a recovery fence and cannot resurrect the original lease. Local
explicit recovery requires the original witness inode/content and an exclusive
lock, then a durable exact-instance completion record. Repeated old completion
receipts cannot clear a newer instance. Actual two-boot recovery and live-lock
refusal passed; PID absence, elapsed time or a shutdown request alone never supply
this proof. See [actual witness validation](../archive/docs-evidence/installed-qemu-witness-live/README.md).
The subsequent v7r3 implementation performs the same proof automatically only
for its own matching tracked worker after exit0. Actual two-boot normal restart
passes20checks, including old-authority denial and independently decoded captures;
the earlier failed terminal observation remains archived. Startup provenance is
published before cont and retained only within the same worker incarnation, while
top-level terminal state requires the owned child to have exited. Restart-loaded obligations remain explicit. Journal v3 separates256 pending
launches/256 scopes from FIFO64 completed receipts. Exact completed witness files
are collected only after durable completion; expired receipt retries are refused,
never treated as pending-cleanup proof. Actual v8 normal restart verifies completed
file GC; actual restart recovery verifies that old receipts after GC still cannot
mutate a running new instance. Component tests cover300 completions and eviction,
not300 real Guest boots. Process-crash evidence is not a power-loss, sleep/wake or
transparent-upgrade claim.

Actual same-daemon two-VM acceptance now covers separate installed Linux desktops:
B service-only SIGKILL retires B while A's same rootless job keeps writing and a
new command/capture succeeds; A then normally exits and only its exact witness is
collected. B's unknown scope/launch/witness is retained. This is functional failure
isolation for two Linux Guests, not three-system, sustained load or performance
acceptance. See [peer evidence](../archive/docs-evidence/installed-qemu-peer-isolation-live/README.md)
and [v8 recovery](../archive/docs-evidence/installed-qemu-retention-recovery-live/README.md).

## Graphics evidence

Apple Linux VZ scanout exposes virtio-gpu with no virgl/resource_blob/context_init capabilities and zero capability sets in the actual Guest kernel probe. Host Metal availability does not turn that device into accelerated Linux GL/Vulkan. Native scanout works; hardware Linux 3D through this VZ device is not established and requires a different backend path. macOS VZ and UTM Windows experimental D3D11 have separate validation gates. See decisions/.

The separate QEMU Linux graphics fixture has actual virgl hardware-renderer and pixel-test evidence. A narrowly tested Xorg fallback font-shader workaround fixes its reproduced 2D glyph corruption. A selectively rebuilt SPICE server supplies fresh scanout descriptors per client and handles retired generations; reconnect, resize and producer-death recovery diagnostics passed. The controller and corrected graphics fixtures have distinct evidence: lifecycle success does not certify pixel correctness, and these patches do not establish general game/CAD compatibility or Windows GPU support.

The actual macOS Guest now completes the pinned Godot4.5.2 native Metal Forward+
six-case lighting/shadow/PCSS/projector sample. Twelve independently decoded
1280×720 images and separate same-console capture support functional acceptance.
The initial geometry-guard failure is preserved; explicit keep aspect prevents
window geometry from changing the internal render size. This does not establish
1440p/60Hz, physical presentation timing, visual conformance or broad game/CAD
compatibility. Blender Cycles remains a separate failed Guest workload. See
[Godot evidence](../archive/docs-evidence/godot-macos-live/README.md). A subsequent six-run cadence
diagnostic measured720p callback p50 about16.6ms and1440p about19.6ms, with a
207.7ms maximum in the first1440p run. This does not support stable1440p60;
callbacks are not physical presentation. [Raw cadence evidence](../archive/docs-evidence/godot-cadence-live/README.md).

## macOS observation and capture path

The opted-in non-root macOS Guest service implements full-resolution SCK screenshots for production MCP observation with bounded JPEG quality selection, frame UUID/time, physical pixel dimensions and UI scale. Existing Guest TCC is required; the service never auto-grants it. Actual Retina2× open/closed-window results are in [MCP evidence](../archive/docs-evidence/control-macos-screenshot.md). macOS async execution remains unsupported. Opt-in files now have actual MCP and browser acceptance described below; input has permission-denial coverage but positive delivery remains unverified.

A separate Guest SCStream diagnostic sends latest bounded NV12 frames over host-CID2-only vsock5001. The VZ worker bridges only that fixed port to one same-user private Unix client. A host VideoToolbox encoder explicitly requires hardware H264/HEVC, preserves BT.709 SDR metadata and feeds the existing loopback WebRTC sender. Actual macOS browser decode with native window closed and capture-stop ACKs passed; [evidence](../archive/docs-evidence/macos-webrtc.md) records CPU copies, static refresh and scope. This is not the authenticated Internet endpoint. Local bridge start/stop are trusted-owner administration calls and are absent from MCP/HTTP tool lists.

## Implemented local media and remaining remote path

Actual Linux console → SPICE IOSurface → Metal-owned frame cache → required-hardware VideoToolbox → Rust RTP/WebRTC → browser is implemented and locally tested. `vm-media-local` binds HTTP and WebRTC UDP to loopback, checks Host/Origin and bounds session admission/lifetime; it is separate from the authorized control service. Chromium and Safari have live codec/profile, decoded-frame, feedback and cleanup evidence in [native WebRTC validation](../archive/docs-evidence/native-webrtc.md). The persistent broker also passed actual static-source Chromium HEVC/H264 sessions with confirmed disconnect, separately from MCP screenshot/lifecycle tests. Browser sampled pixel changes cannot establish source motion when the source cache is unchanged.

The native capture retains the latest owned surface independently of browser encoding and invalidates it on producer loss or source replacement. Active encoding refreshes an idle valid cache at 1 fps. Copies, synchronous encoder drain and bounded queues remain correctness mechanisms. One encoder owner is allowed per broker; adaptive bitrate/frame-rate/resolution, multi-VM encoder budgets and sleep/wake recovery remain unfinished. A source-generation change ends its encoding epoch and requires a fresh start. The legacy per-stream capture helper remains a separate diagnostic; a cold connection to a completely static Guest can lack an initial draw.

The Rust sender now admits complete encoded AUs against a bounded **relative sender-lag** budget before RTP submission. It compares local arrival elapsed time with unchanged native PTS using the lowest observed offset, rejects lag of 250 ms or more, and suppresses dependent frames until a fresh keyframe arrives. Recovery is bounded to 2 seconds or 120 dropped AUs. Keyframe requests are coalesced and rate-limited; packet submission and recovery reads have absolute deadlines. This cannot measure initial constant delay, Guest capture age, network delay or physical presentation. Actual direct-loopback HEVC/H264 regression passes ordinary decoding and confirmed cleanup, but both runs recorded zero stale/dependency drops and zero recovery-keyframe admissions. The recovery decisions have deterministic component coverage, not actual congestion-recovery acceptance. See [scheduler contract](../archive/docs-evidence/relative-sender-lag.md) and [actual two-codec evidence](../archive/docs-evidence/relative-sender-lag-live/README.md).

The private broker uses connection-owned encoder epochs, start-ACK-before-AU ordering and bounded nonblocking Unix I/O. A stop error, missing/malformed acknowledgement or timeout fences and reaps capture rather than falsely releasing ownership. A session-ended event is trusted only when its nonzero wire/body epochs agree and identify the current owner. Eleven real process/socket tests use an explicitly synthetic native protocol fixture; they are not GPU, VideoToolbox or VM acceptance. See [broker protocol evidence](../archive/docs-evidence/console-broker.md).

The new optional loopback signed-session path implements local pairing, authenticated SDP/fingerprint binding and revocation against actual macOS video (see evidence/session-auth/README.md). The intended Internet path still requires production integration and hostile-relay verification of these controls, trusted browser-code delivery, direct-first ICE with restricted short-lived TURN credentials, and authorized same-console input. Cloud TURN is relay only; no SFU is selected. Browser file/input/MCP controls must enforce scope, frame coordinates and current control epoch on the Mac. Loopback DTLS/SRTP tests do not establish this Internet E2EE design. A separate `vm-mcp-http` endpoint implements direct-host rustls HTTPS with provisioned bearer principals, VM allowlists and bounded Streamable HTTP sessions; TLS terminates on the Mac. It does not authenticate the loopback media diagnostic or implement hosted discovery/signaling/TURN. See [control endpoint contract](../crates/control/README.md).

Public platform boundary is capabilities + lifecycle + console/guest RPC. Swift types never form the public wire contract. Future Linux/Windows host adapters are not being implemented. Offline image storage uses private read-only base images and APFS fclonefileat writable clones, atomic publication, SHA256 integrity and free-space preflight. Active writable source disks are refused. Actual installed clone boots and private writes have been checked against unchanged complete base/source hashes. These are offline disk clones; no live disk snapshot or memory restore is advertised.

Codec selection is per connection: intersect verified host hardware and browser profile/level capabilities, prefer HEVC while retaining H.264, verify negotiated RTP and decoded/presented frames, and retry H.264 once only after failed-attempt cleanup is confirmed. Reconnection starts a new selection; seamless codec renegotiation is not implemented. MP4 decoding is separate evidence from the actual localhost WebRTC runs. Authorized QEMU browser input now passes localhost acceptance; macOS browser input and Internet transport remain unfinished.

Linux file transfer now uses an explicitly enabled separate fixed non-root worker and private directory, independent files scope, owner-bound bounded transfers, atomic no-overwrite uploads and final transmitted-byte hash verification. The validated command and files accounts differ; upload-then-execute is not implicitly authorized. See evidence/control-files.md for actual Guest and TLS MCP validation and limits.

macOS opt-in file transfer uses the same scoped protocol, with a separate
same-UID non-root worker and pinned private local directory. It allows a single
filename, rejects symlinks/hardlinks/extended ACLs, limits transfers to8MiB and
chunks to32KiB, and publishes without overwrite after hash verification. Worker
RPC has a5second deadline; failure fences files until service restart. The
worker is not a sandbox against other programs running as that Guest user.
Actual stdio MCP/official SDK roundtrips, EOF and idle-expiry cleanup pass; see
[macOS file evidence](../archive/docs-evidence/macos-files-live/README.md). This does not enable
legacy command execution, which still lacks reliable descendant cancellation.

## Signed browser input checkpoint

The production QEMU console can now receive scoped browser control via the existing daemon lease/epoch path. An explicit fresh screenshot supplies frame identity, source generation and physical dimensions; input never invents correspondence with a video RTP frame. Each signed request binds the verified session transcript, monotonic sequence and short expiry. Human takeover, revoke and close fence the prior owner; actual Linux evdev/Xorg acceptance and denied stale input are recorded in evidence/browser-input-live/README.md. The original loopback HTTP control transport has been replaced by the authenticated WebRTC DataChannel; no plaintext control endpoint/fallback remains. The same signed envelopes and daemon checks are retained. macOS/Windows input and full Internet transport remain unfinished.

The channel shares the signed media peer and its DTLS association. Peer-driver callbacks only schedule bounded work; they never await commands to the same driver. One fragmented request/response is allowed, with no queued second operation. Session close first revokes authority, cancels controller RPCs and stops encoding; channel worker retirement and final peer close must be confirmed. The sender uses hardware HEVC or H264 independently of the control channel. Actual two-codec browser/Guest and failure-recovery evidence is in evidence/browser-control-channel/README.md.

Authenticated Guest-bound offers may now receive a host-signed `not_admitted` rejection for source validation failure before any host session/peer/encoder allocation. It binds the complete current offer intent, uses an allowlisted code, and is not evidence of cleanup for any admitted session. A non-queued preflight guard plus pre/post session-book checks prevent signing over existing cleanup uncertainty; the request is tombstoned before publication. Browser verification requires the pinned key and exact outstanding intent, then successful local peer cleanup before reconnect. Plaintext errors and later failures retain the conservative fence. See [actual refusal evidence](../archive/docs-evidence/browser-offer-rejection-live/README.md).

## Signed browser file transfer checkpoint

Browser uploads/downloads now use the authenticated DTLS DataChannel with explicit files scope, independent of desktop-input permission and its lease. The host rechecks grant and VM worker/bridge identity around each bounded Guest RPC. Uploads never overwrite; both directions verify declared size, sequential32KiB chunks and final SHA256 within8MiB. Closing/revoking a session retires its file owner and requires actual Guest cleanup before acknowledgment. Unsupported or uncertain cleanup stays an error. Swift encoders omit optional slash escaping so base64 worst-case chunks remain within the unchanged64KiB line bound.

Actual macOS8MiB upload/download, full0xFF chunks and browser active-upload disconnect passed after correcting this framing bug; older failures are retained in [browser file evidence](../archive/docs-evidence/browser-files-live/README.md) and [framing diagnosis](../archive/docs-evidence/browser-files-slash-framing.md). This is the same-console local authenticated path, not an Internet endpoint or a replacement for standard MCP. End-to-end browser code delivery, cloud connectivity and broader transfer/resource scheduling remain unfinished.


## Local media stop boundary

The product UDP sender now latches a per-Peer media fence before awaiting native,
input or file cleanup. A synchronous gate covers each final nonblocking UDP send
poll, including GSO fallback and already-wrapped TURN media; pending sends wake
on revocation. The linearization point follows any already-running send poll.
SRTP is stopped while validated SRTCP, DTLS/SCTP, STUN/TURN and narrowly validated
owned-socket mDNS control remain usable. Unsupported framing and partial UDP
submission fail closed. TCP is explicitly unsupported when this fence is enabled.
This cannot recall packets already accepted by the kernel or remote queues.

Original cleanup obligations remain: native/Guest retirement, control shutdown,
Peer close and TURN allocation removal require their own evidence. Final failed
or pending-media state forbids a successful cleanup receipt. Local submission
counters are not receiver delivery. The actual two-codec direct/local-TURN
regression and preceding mDNS failures are preserved in
[stop-fence evidence](../archive/docs-evidence/media-egress-fence-live/v3-accepted/README.md).
The original250ms relative sender-lag deadline now also governs final UDP sends.
A non-cloneable AU writer binds the actual negotiated H264/H265 primary and RTX
parameters under the Peer core lock. At most128 monotonic RTP timestamp records
track original deadlines; a newly reproduced defect means cached or queued packets
can outlive a retired record and incorrectly fail as unknown. Packet-lifetime
provenance is being repaired; the old table is not sufficient lifetime protection
across NACK/RTX and TURN wrapping. Changing the bound
sender configuration is explicitly unsupported. Pending final sends wake at the
original deadline. A known expiry latches a policy stop while preserving truthful
control/Guest/Peer cleanup; unknown identity or malformed transport remains a
failure. A full successful send whose completion is late remains counted as
submitted, with a separate overrun diagnostic. This does not reveal the exact
kernel acceptance instant or initial capture delay.

Actual normal hardware H264/HEVC direct and forced local TURN cases passed with
signed close confirmation. Deliberate downstream holds separately verified known
expiry termination in both codecs. See [normal AU evidence](../archive/docs-evidence/media-au-deadline-live/v1-normal/README.md)
and [intentional expiry evidence](../archive/docs-evidence/media-au-deadline-live/v1-expiry/README.md).
The product replaces the exact default NACK responder with a bounded responder:
1MiB/1024 cached packets, 128KiB/128 pending repairs and independent 256KiB/32
forward/read queues. Compact serialization owns retained bytes; expansion checks
256-pair/object budgets and generation-aware dedup before cloning. Fatal state
synchronously fences media and wakes main independently of swallowed interceptor
errors. Actual close clears resources. These are owned wire-byte/packet limits,
not bounds on every RTC queue, RSS or processing rate. Real Peer tests exercise
repair and expiry; four actual normal Guest/browser sessions contain no repairs.
[Evidence](../archive/docs-evidence/media-nack-budget-live/v1-normal/README.md). Pacing and bandwidth
estimation remain open; compact serialization cost has not been benchmarked.
