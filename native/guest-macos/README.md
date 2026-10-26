# macOS Guest service

## Read-only application window observation

The staged `windows` method uses the existing `--allow-observe` switch and
existing Guest Screen Recording permission. It never prompts for permission,
requires no Accessibility or command-execution enablement, and does not grant
input authority. `info.capabilities.windows` and `info.window_policy` report
current preflight availability; enumeration can still fail if the desktop or
permission changes after that query.

The exact request is `{"id":"windows-1","method":"windows"}`. Its result lists
on-screen, layer-zero application windows, excluding desktop elements, for the
verified current Guest console. Window IDs are observations local to the returned
snapshot UUID, not durable handles or input anchors. Titles, document paths,
Accessibility nodes, focus and z-order are not returned. Application names and
bundle identifiers are bounded to256 UTF-8 bytes each; they remain potentially
identifying data within the observation permission.

Window/display bounds are global screen-space points, including negative origins
and cross-display rectangles. The SDK `CGWindow.h` documents `kCGWindowBounds`
relative to the main display's upper-left corner. Each SCK window must match the
same ID's CoreGraphics bounds; moving/mismatched windows are omitted explicitly.
The SDK `SCShareableContent.h` documents display width/height as points, not
physical pixels. The response separately reports raw display-mode pixel dimensions,
rotation and the actual `SCContentFilter.pointPixelScale` from `SCStream.h`;
unavailable scale is null, never an assumed1 or2. No global pixel coordinate plane
is constructed from mixed-scale displays. Positive-area intersections include only
active, awake displays. Main-display sleep rejects through the existing console
identity policy; a secondary sleeping display is represented without capture scale.

At most128 windows and16 online displays are returned, with a48KiB compact JSON
result budget (the wrapped method reply remains below64KiB). Windows are sorted
by ID, with explicit `truncated`, `eligible_window_count` and
`omitted_invalid_count`. The latter includes malformed geometry or unencodable
owner metadata; these counters are not a strict partition. Whole windows are
omitted rather than cutting UTF-8 strings or silently changing coordinates.
More than16 displays fails. Framework enumeration is capped at8192 metadata
entries before our dictionaries/candidate arrays are built; this does not bound
memory already allocated internally by Apple's APIs.

One OS enumeration is outstanding at a time, with an8s caller deadline. A late
callback retains the slot until it completes. Console identity, display topology,
geometry/state and Screen Recording permission are checked again before publishing;
a mismatch discards the entire result. This is sequential observation, not an
atomic desktop snapshot; transitions between public API reads cannot be excluded.
Wall-clock requested/completed timestamps may move backward after clock adjustment;
the separate completion monotonic timestamp is not presentation time.

Build a fresh output with `build.sh --output /absolute/new/path`, then run
`--windows-self-test` for pure geometry, metadata/wire limits, authorization
denials and callback ownership checks. These tests do not enumerate host windows
or establish Guest acceptance. The new controller must expose observe-only
`vm_windows`; an older daemon cannot forward the new method merely because a new
MCP client advertises it. Actual Guest migration and MCP validation are separate.

Component checkpoint: `.local/guest-share/macos-guest-agent-windows-stage2`,
SHA256 `8328097f75a8f89f4668625780e189df89dec171f647623d8d7e3b902c664b96`,
built without warnings. Its `--windows-self-test` and all nine groups of the
existing `--self-test` passed; logs are in `.local/evidence/macos-windows-stage2/`.
This includes existing real filesystem/pipe/subprocess regressions, but no actual
Guest window enumeration, permission change, app input or host desktop observation.

Build on the Apple Silicon host with `native/guest-macos/build.sh`.
The standalone ARM64 binary is copied to `.local/guest-share/macos-guest-agent`.
Host `--self-test` exercises handlers and real child processes only; it is not
proof that the service works inside a Guest or that Guest vsock connects.

After the user completes macOS Setup Assistant and signs in, open Guest Terminal:

```sh
mkdir -p "$HOME/veynode"
mount -t virtiofs veynode "$HOME/veynode"
"$HOME/veynode/macos-guest-agent" --allow-execute
```

This mount was verified as the signed-in Guest user (UID 501), without sudo,
at `/Users/veynodetest/veynode`. macOS requested permission for Terminal's first
access to the shared directory. An attempted Guest write failed with
`Operation not permitted`, consistent with the host's read-only share. The
service must run as the signed-in Guest user, without sudo; it refuses UID 0. Omit
`--allow-execute` to disable command execution. `--allow-observe` independently enables the bounded screenshot method after existing Guest Screen Recording permission; omitting both leaves read-only `info` access. Keep that terminal running for this
validation. A managed login service and installer are not implemented yet.

The current worker config exports only the explicit `.local/guest-share`
directory read-only with the virtiofs tag `veynode`. If macOS has automatically
mounted it at a different location, use that mount instead. Do not attempt to
skip initial setup or pre-create Guest user accounts offline.

Transport: Darwin `AF_VSOCK` (40), port 5000, accept only peer CID 2 (host),
maximum 16 concurrent requests. There is no TCP/UDP listener. The host worker
must enforce user/Agent authorization before forwarding any request; trusting
host CID does not replace those permission checks.

Requests are one JSON line, limited to 64 KiB, with a nonempty string `id` of at
most 128 UTF-8 bytes and a `method`. Unknown fields are rejected. Methods:

- `info`: reports actual OS, kernel, machine, Metal device, user identity and
  implemented capabilities (`exec`, independently available `screenshot`, opt-in `input`/`desktop`, and default `files:false`) and an `observation` policy with current preflight permission status.
- `exec`: `argv` array (1–128 arguments, no NUL) and integer `timeout_ms` (1–30000).
  Requires explicit `--allow-execute`. Uses `posix_spawnp` with a new process
  group, current Guest user, sanitized environment, and closed stdin. Captures
  stdout/stderr independently up to 64 KiB each, drains excess output, and
  reports truncation, exit code, elapsed time, and timeout. The process group is
  killed on timeout and after the direct child exits so normal background
  descendants do not remain. This is process lifecycle control, not a sandbox
  against intentionally malicious commands that detach from the group.

Read/write framing deadlines and bounded output prevent stalled peers from
holding unbounded memory. The native host worker additionally imposes a 35 s
request deadline. No credentials, command bodies or command outputs are logged
by the service. Accessibility trees and a signed installer remain unimplemented; opt-in input and files are described below. The independently authorized screenshot method is described below; the separate PNG diagnostic remains available for local diagnosis.

Run Guest diagnostics separately:

```sh
"$HOME/veynode/metal-probe"
```

Only output obtained **inside the actual Guest** establishes Guest Metal
capability. Existing `apple-host-metal.json` remains host-only evidence.

The macOS service reports `async_exec:false` and `shutdown:false` explicitly;
Linux job cancellation and privileged shutdown have not been ported to it.

Command cleanup now observes the leader with `waitid(WEXITED | WNOHANG |
WNOWAIT)`, keeping its PID reserved until the private process group has been
signalled. Only then does `waitpid` reap the leader; no later group signal is
issued. This avoids signalling a reused PGID after an ordinary exit. Process
group setup is checked before spawning. Darwin's zombie-only `killpg` EPERM is
accepted only when the same leader remains waitable and `proc_listpids` reports
exactly that PID as the group's sole member; other cleanup failures are errors.

`--self-test` now also exercises natural parent exit with a delayed background
writer and32 short-lived groups, in addition to the timed-out descendant,
Unicode/nonzero exit and output-limit checks. These are host real-process tests,
not Guest/vsock acceptance. The existing command deadline and250ms output-drain
budget remain; synchronous OS reaping after SIGKILL is not a hard real-time
kernel guarantee. Deliberate process-group escape is still outside this helper's
containment guarantees, and no sandbox claim is made.

Build the separate screenshot diagnostic on the host with
`native/tools/build-macos-desktop-probe.sh`. Run it only inside the signed-in
Guest console session, directly or through this service's `exec`:

```sh
"$HOME/veynode/macos-desktop-probe" --output /tmp/veynode-desktop-first.png --timeout-ms 8000
```

It checks Screen Recording permission without requesting it by default. If
permission is missing, the JSON result explicitly fails. The optional
`--request-permission` requests approval in the Guest; the user must handle the
Guest permission UI. A restart of the responsible application may be needed.
The diagnostic refuses physical Mac models and UID 0 before calling capture or
permission APIs. Use a new absolute output filename each time: existing files
and symlinks are never overwritten. A failed write or deadline may leave an
incomplete newly created file, which must not be treated as a valid screenshot.
Only a success JSON with matching file hash is evidence of completed capture.

The JSON includes display dimensions in points and pixels, scale, selected
display ID, a frame UUID, PNG SHA-256 and capture request/completion times. The
completion time is not an exact display presentation timestamp. PNG data stays
in the Guest; the diagnostic does not transmit it or log image bytes. Runtime is
limited to 1–25 seconds (8 seconds by default), dimensions to 4096 on either axis
and 8,388,608 pixels total, and PNG output to 16 MiB. This one-shot diagnostic
does not implement continuous capture, encoding, input or remote streaming.

## Independently authorized console observation

Build a new staged executable without replacing a running service or its share:

```sh
native/guest-macos/build.sh --output "$PWD/.local/guest-share/macos-guest-agent-observe-stage-new"
```

After explicitly coordinating replacement of the old foreground service, run the
staged binary as the signed-in Guest user with `--allow-observe`. This does not
require `--allow-execute`. The service never requests or grants Screen Recording
permission: missing existing permission returns `screen_recording_permission_required`.
Permission belongs to the responsible Guest application/code identity; moving to a
packaged login service will require separate permission validation. The service
refuses actual capture outside a non-root VirtualMac Guest, before host TCC APIs.

Request `{"id":"shot","method":"screenshot"}` over the same host-only vsock
port5000. Only `id` and `method` are accepted for this method. It selects the active
main Guest display through public ScreenCaptureKit `SCScreenshotManager`, preserves
its full physical-pixel dimensions and returns JPEG using the explicit quality ladder0.80→0.65→0.50→0.35, returning the actual selected quality. Images
exceeding700KiB,4096 pixels on either axis or8,388,608 total pixels explicitly fail;
there is no downscale, unreported quality selection or host-screen fallback.

The image response includes MIME/base64/byte length/SHA256, a frame UUID, actual
source and encoded dimensions, display identity, point-space bounds and content
rect, point-to-pixel scale, requested/completed times and Guest monotonic completion
time. Completion time is not an exact display presentation timestamp. JPEG is lossy,
so it is not an exact pixel-comparison artifact. SDR and cursor inclusion are explicit.

One screenshot can be outstanding. The caller waits at most8s for capture; native
callbacks have no cancellation API. If a callback outlives that wait, the capture
slot stays busy until it finishes, preventing accumulated asynchronous captures.
The encoded reply stays below1MiB; request and command-output limits are unchanged.
The main thread runs the dispatch loop; vsock acceptance and requests run on bounded
background workers. Closing the host's VM window does not select a different Guest
console, though actual headless capture must be verified for each integration path.

The controller maps `vm_screenshot` directly to this method, independently of
command execution. It validates the declared image type/base64/size/signature and
emits the actual `image/jpeg` MIME in MCP image content. Apple Linux remains
unsupported for screenshots in this adapter; QEMU keeps its existing PNG path.

Host `--self-test` now checks disabled/physical-host capture guards and encodes and
independently decodes synthetic32×16 and textured256×256 images, including actual quality fallback without resizing and an enforced size failure.
It never captures the host desktop. Actual Guest acceptance uses:

```sh
python3 scripts/test-control-macos-screenshot.py --state-dir /absolute/private/state --id macos-smoke --output .local/evidence/macos-mcp-new.json
```

That test requires an already running registered VM and never starts or stops it.
It checks observe-only MCP tools, actual Guest permission state, two fresh frames,
full-resolution metadata/SHA256, and an independent FFmpeg JPEG decode.

API references checked against Apple's public documentation:
[SCScreenshotManager.captureImage](https://developer.apple.com/documentation/screencapturekit/scscreenshotmanager/captureimage(contentfilter:configuration:completionhandler:)),
[pointPixelScale](https://developer.apple.com/documentation/screencapturekit/sccontentfilter/pointpixelscale),
and [CGPreflightScreenCaptureAccess](https://developer.apple.com/documentation/coregraphics/cgpreflightscreencaptureaccess()).
The capture and point-scale APIs used here are available from macOS14.

Actual production observation acceptance is recorded in
[control-macos-screenshot](../../archive/docs-evidence/control-macos-screenshot.md): two
fresh frames with the native window open and two after closing it, all2560×1440,
Guest point scale2, selected JPEG quality0.65, command execution disabled. The
same installed Guest remained running. This does not establish continuous capture,
browser operation or unattended shutdown.


## Opt-in physical desktop input (staged; actual Guest acceptance pending)

Input is independent of command execution. Build a new artifact, preserving any
running service binary:

```sh
native/guest-macos/build.sh --output "$PWD/.local/guest-share/macos-guest-agent-input-stage5"
"$PWD/.local/guest-share/macos-guest-agent-input-stage5" --self-test
cargo build -p vm-control --bins -j 2 --target-dir .local/macos-input-target
```

The build refuses an existing `--output` path; choose a new stage name for another
build. The checked stage5 binary already exists. No existing Guest service is
restarted by these commands. After explicit user approval of **Guest** Accessibility
permission for the responsible application (normally the Guest Terminal used to
launch this binary), the operator can replace only the Guest service process and run:

```sh
"$HOME/veynode/macos-guest-agent-input-stage5" --allow-observe --allow-input
```

Screen Recording remains separately required for screenshot anchors. The service
uses `CGPreflightPostEventAccess` only and never requests or grants either permission.
Check `vm_guest_info.input_policy` to determine the actual permission state; TCC
attribution can depend on launch identity. No host permission is needed. The service
rejects root and physical-host input. Do not add `--allow-execute`: macOS asynchronous
cancellation/escaped-descendant containment is not implemented, and the controller
continues to reject the legacy synchronous command path.

Use the new controller daemon only in an operator-controlled VM restart; replacing
its binary does not hot-upgrade an existing daemon. The existing state/registry can
be retained, but do not start a second daemon or attach a writable Guest disk twice.
For a daemon already running the updated code, start MCP with:

```sh
.local/macos-input-target/debug/vm-control mcp --state-dir /absolute/private/state --allow-input
```

Take a fresh screenshot and pass its `frame_id`,
`source_generation`, `width`, and `height` to `vm_input_click`, `vm_input_drag`,
`vm_input_scroll`, or `vm_input_key`. A screenshot has an input anchor only when
`input_anchor_available` is true. Observation still succeeds when input registration
is unavailable. There is no input anchored to an arbitrary WebRTC frame: the browser
session's existing QEMU-only authorization binding is unchanged by this MCP adapter.

The service caches at most16 screenshot metadata records (not image bodies), each
valid for5s from the original capture completion. It verifies current main display
identity, actual display-mode physical dimensions, point-space size and global
origin. Retina coordinates map from each physical pixel centre into those captured
bounds. Display-reconfiguration notifications invalidate cached anchors, including
a transient configuration change that returns to the same geometry. A capture that
crossed such a notification cannot become an input anchor.

Internal Guest methods are `input_step` and `input_cleanup`, with
nested `arguments`. Only one operation is active, with16 events per step, a4s total
deadline and a1s inter-step watchdog checked every100ms. Absolute movement must be
paired x/y and precede mouse buttons or scrolling within the same operation. The
physical key map is explicit; `insert`, text and Unicode/layout translation remain
unsupported. Whole batches validate before any event is posted.

Only states posted by this service are released; it does not issue an all-physical-key reset.
Normal completion, failed continuations, failed response writes, the watchdog,
display changes, SIGTERM/SIGINT and service errors run cleanup. A successful
per-request vsock response is followed by normal socket EOF and is not itself a
session cancellation. An incomplete
request that disconnects before identifying its operation is bounded by the1s
watchdog (plus its100ms check interval), not an immediate global key reset. A failed release
latches input disabled, and requires explicit service restart after permission is
restored. SIGKILL/Guest crash cannot execute cleanup and is not claimed to do so.

`dispatched` and `synthetic_release_dispatched` mean public CGEvent posting completed;
CGEventPost has no application-delivery receipt. `guest_delivery_confirmed` remains
false. Permission/transport failures remain explicit errors.

Host self-tests use a synthetic sink and never post host input. They cover2x Retina
and negative-origin pixel-centre mapping, batch denial, stale/cache/generation
fencing, different-operation denial, synthetic release, failed-release latch,
watchdog, display invalidation and disabled/host guards. Controller tests cover
strict ACKs and actual child-pipe routing with a synthetic Guest
permission denial. These are not real Guest input acceptance; actual permission,
click/key/drag/scroll behavior and Retina positioning remain to be tested in Guest.

## Own-window AppKit input oracle

`input-oracle.swift` is a separate Guest-only validation program. It never sends
input, installs event taps/global monitors, captures screens, requests TCC, or
observes another application's keys. It implements ordinary NSView callbacks for
its own key window. It records physical keycodes/flags and mouse coordinates, not
character strings. CGEvent source PID/state/user-data are included only as
**untrusted metadata**, not proof that an event is synthetic or came from our service.

The final staged oracle is `.local/guest-share/macos-input-oracle-stage2`, SHA256
`e8bf29a92106e2188b3f4e874590dcd2788ad476add3fe64cbbf77586b0d6876`.
For another build choose a new absolute output path:

```sh
native/guest-macos/build-input-oracle.sh --output "$PWD/.local/guest-share/macos-input-oracle-next"
.local/guest-share/macos-input-oracle-stage2 --self-test
```

The build refuses existing outputs. Self-test does not launch an app/window; it
checks own-window counters, observed down/up tracking, focus-loss uncertainty,
Retina/global-origin coordinate conversion, overflow marking, bounded JSONL logging,
0600 mode, and existing-file/symlink refusal. The physical-host runtime guard was
also tested: exit1, no window and no log created. Local reports:
`.local/evidence/macos-input-oracle-stage2.json` and
`.local/evidence/macos-input-oracle-selftest.log`.

After Guest login and the read-only share mount, launch from a **separate Guest
Terminal tab** while the input service remains running:

```sh
"$HOME/veynode/macos-input-oracle-stage2" --log "$HOME/veynode-input-oracle-01.jsonl" --seconds 600
```

No sudo/password or additional permission is needed for receiving its own window's
events. Choose a new log filename each run. The app creates an800×530 point view
on the main display, with a green left and red right target. The displayed counters
and crosshair can be verified through the product screenshot path. Leave this
window focused; an observation gap is marked permanently if it loses key-window
status, and held states are not falsely cleared. If it moves or changes backing
scale, a fresh target-geometry record is written. Keep it on the main display for
this first adapter's main-display-only screenshot/input contract.

Suggested actual acceptance sequence:

1. Verify `vm_guest_info.input_policy.available`, capture a new
   screenshot with `input_anchor_available:true`, and choose coordinates inside
   the green target using that screenshot's physical-pixel dimensions. Use only
   that capture's frame UUID/generation; do not use a video frame as an anchor.
2. Click the target. Require observed mouse-down/up counts to increase and the
   displayed held-button set to return empty. Cross-check the log's Quartz point
   and physical-pixel coordinates against the requested pixel centre.
3. Send physical `a`, then a supported modifier chord such as `ctrl+a`. Require
   key-down/up observations, no held keycodes after completion, and final modifier
   flags zero. Avoid OS-reserved switching/quit shortcuts during this oracle run.
4. Drag across the target and scroll. Require own-window drag/scroll events and
   released buttons; AppKit may coalesce motion, so event count need not equal
   the number of transmitted movement steps.
5. After a screenshot is older than5s, verify input rejection and no corresponding
   new event.
6. Close the oracle's window or wait its configured timeout. The terminal regains
   its prompt. Inspect the private log inside Guest with
   `tail -n 40 "$HOME/veynode-input-oracle-01.jsonl"` if needed. Do not expand Guest
   command/file permissions merely to retrieve this diagnostic.

Logging is capped at512 ordinary records and256KiB, with reserved terminal-summary
space. Mouse-move log entries are sampled to30Hz; counters still observe every
own-window callback. `ended` reports discarded records and any write failure;
tracking overflow is explicit, never interpreted as a successful release. Runtime
is30–900s (default300s). Timeout/window close are the orderly finish paths; abrupt
process death may omit the final summary. No raw screenshot or text is logged.

The AppKit UI and real Guest input observations have not been accepted by the
implementation agent; the main operator performs that actual validation. This
fixture is an independent receiver, not evidence that an input request succeeded
merely because the sender returned `dispatched:true`.

## Public console identity staging

The new service links the shared `console-binding.swift` implementation. `info`
returns `console_identity` (canonical v1 or null) plus `console_identity_status`.
Unavailable public identity leaves diagnostic `info` usable but marks screenshot
and input capabilities unavailable, preserving the separate actual TCC result.
`lock_state:unknown` means unmeasured; it does not revoke otherwise authorized
same-session input or claim the display is unlocked.

Successful screenshots carry `console_identity`. Capture reads identity before
enumeration and after JPEG encoding, checks SCK display ID/physical size/global
point bounds against the canonical geometry hash, and discards changed/unavailable
results. Input anchors retain that exact identity. Each input step and each batch
element checks the current identity; synthetic releases check their original login
UID/session/console set before every release. Unverifiable releases return an error
and latch the existing cleanup fault; no release is deliberately posted into a
different login. The watchdog polls identity at most2Hz while an anchor/operation
exists; there is no identity polling after all anchors are cleared/expire.

Build and host self-test without replacing any running executable:

```sh
bash native/guest-macos/build.sh --output "$PWD/.local/guest-share/macos-guest-agent-input-stage6r3"
.local/guest-share/macos-guest-agent-input-stage6r3 --self-test
```

Choose a fresh output name if that path already exists. Final staged SHA256 is
`c5266a7cc95b56322bfbc2edb436e18a6f8e62e5a61b4aa9ce02c95b11f04754`.
The main operator must first stop the owned old Guest service through an authorized
controlled path; never run two listeners or overwrite the active binary. After
that controlled restart, the Guest command is:

```sh
"$HOME/veynode/macos-guest-agent-input-stage6r3" --allow-observe --allow-input
```

No sudo/password or automatic TCC prompt is added. Responsible-app permission must
still be verified by the operator. No upgrade or permission change was performed
by this implementation task. Earlier stage6 variants are intermediate artifacts;
use stage6r3 for subsequent actual acceptance.

Host tests use synthetic identity/geometry/event sinks and existing real subprocess
tests; they do not capture or inject host events. Actual Guest `info`/screenshot
identity agreement, user switching, logout/relogin, display sleep and input remain
pending for this build. Periodic/per-action reads are not an atomic transaction
with CoreGraphics; switching between final check and event submission remains a
documented race. There is no long-running public session-notification watcher,
cross-process capture-stream identity, host bridge-incarnation binding or macOS
browser-input enablement in this change. See
`archive/docs-evidence/apple-console-service-integration.md` for exact evidence.

## Opt-in macOS file transfer (staged implementation)

The Guest service now implements the existing bounded Linux file wire protocol
using a separate, fixed-purpose **same-UID** worker. This does not enable execution,
Accessibility, screen recording, or new permissions. Without `--allow-files`,
`info.capabilities.files` remains false and every file method is unsupported.
The worker is not an isolation boundary against other programs running as the
same Guest user. In particular, this is not the Linux dedicated files-account
separation. `async_exec` remains false; this change does not expose legacy macOS
command execution through MCP.

Build to a fresh path, preserving running services:

```sh
bash native/guest-macos/build.sh --output "$PWD/.local/guest-share/macos-guest-agent-files-stage6"
.local/guest-share/macos-guest-agent-files-stage6 --self-test
```

The builder now compiles privately and publishes using a no-replace hard link;
it refuses existing binaries and shared outputs, including the default paths.
Use another unique basename on subsequent builds. Only after an operator has
stopped the old owned integration service, start the staged service inside the
Guest as the same ordinary user, preserving any already-authorized observation
flags:

```sh
# Root must already exist, be owned by this user, mode0700, and have no ACL.
# Choose an ordinary local Guest directory, outside TCC-protected folders.
"$HOME/veynode/macos-guest-agent-files-stage6" --allow-observe --allow-input \
  --allow-files --files-root "$HOME/veynode-files-fixture-stage1/files"
```

Omit `--allow-input` if input is not desired; files do not depend on it. Do not add
`--allow-execute`. No command here grants TCC permission or changes an existing
folder's permissions. A missing, symlinked, nonprivate, ACL-bearing, foreign-owned,
nonlocal, or concurrently used root refuses startup. The root, staging directory
and exclusive lock are pinned by descriptors/inodes. Path replacement invalidates
future operations. Root paths containing any symlink are rejected rather than
silently canonicalized.

The existing `file_upload_begin/write/commit`, `file_download_begin/read`,
`file_cancel`, and `file_session_end` shapes are unchanged. The controller injects
the64-hex session owner; random32-hex transfer tokens are bound to that owner.
Limits are8MiB per file,32KiB per chunk,16 transfers,60-second idle expiry and
300-second absolute lifetime. Session end removes incomplete uploads and retires
the owner for300 seconds, including owners with no transfer. It does not delete
published files. Exhausted retirement storage fences new admissions; it does not
evict valid retirement records. The worker prunes expired transfers while idle.

Names are single UTF-8 filenames, at most255 bytes; slash, backslash and NUL are
rejected by **byte**, including separators followed by Unicode combining marks.
Dot/dotdot and the `.veynode-` namespace are reserved. Downloads require a private,
owned, single-link regular file and no ACL. Symlinks, hard links, FIFOs and
subdirectories are refused. Source metadata and the streamed digest are checked;
a final changed/integrity error requires discarding partially downloaded data.

Uploads create private0600 staging files, enforce sequential offsets and the
announced size/hash, sync the file, and publish through public Darwin
`renameatx_np(RENAME_EXCL)`. Destination races never overwrite existing files.
After publication the directories are synced. A failure at that point returns
`durability_unknown` and explicitly says the file is already published; it does
not delete it or claim rollback. Unsupported filesystem operations fail.
Startup recovery only removes safe owned single-link files matching the exact
reserved temporary-name grammar; unexpected staging entries refuse startup.
Same-user malicious races are outside this worker's security boundary.

Worker RPC uses private inherited pipes, closed unrelated descriptors, one
in-flight request and a5-second deadline. A worker failure or timeout permanently
fences that bridge until service restart. Only its positively spawned, unreaped
child PID may be signalled; failed initialization cannot touch another child or
process group. EOF attempts staging cleanup and a graceful exit. Forced worker
termination is not reported as confirmed staging cleanup; a fresh worker must
validate and remove retained private staging before admitting work. Idle service
lifetime is unlimited; an incomplete incoming worker request has a5-second limit.

The installed SDK provides `O_NOFOLLOW_ANY`, `fstatx_np`,
`filesec_query_property`, `renameatx_np` and `RENAME_EXCL`. ACL absence is queried
explicitly from `fstatx_np` metadata: a generic `acl_get_fd_np` nil/error is **not**
accepted as absence. Apple's [Libc ACL implementation](https://github.com/apple-oss-distributions/Libc/blob/main/posix1e/acl_file.c)
explains that retrieval delegates to a file-security property lookup.

Host-only acceptance: `--files-self-test` uses real private filesystem fixtures,
real worker pipes and real subprocess failure, with no Guest, TCC, screenshots or
input. It covers0-byte/Unicode/8MiB roundtrips, integrity/offset/owner limits,
no-replace publication races, slash-plus-combining traversal rejection,
symlink/hardlink/FIFO/ACL refusal, source/root/lock changes, stale staging refusal,
owner retirement, idle/absolute expiry, transfer capacity, startup-failure child
ownership, EOF cleanup, SIGKILL recovery, and SIGSTOP timeout with bounded reaping.
These tests do not establish actual Guest/MCP acceptance, disk-full recovery or
crash durability. `scripts/test-macos-files.py` is the separate actual Guest
acceptance path; only its recorded real results can establish that integration.

Final staged host checkpoint: `.local/guest-share/macos-guest-agent-files-stage6`,
SHA256 `8c72b73c3ba76535b8b2e0ef59f2e17b325ee9d2dc9712bf39c278dd9238ee7e`.
Full `--self-test` passed, including the existing command/capture/input/console
checks and new real files/subprocess tests; log
`.local/evidence/macos-files-stage6-selftest.log`. Building to that existing path
was refused without replacement. Stage1–5 artifacts are intermediate development
checkpoints, not deployment candidates. Earlier failures are preserved in local
logs: test temporary-path symlink and ambiguous nil ACL lookup were corrected,
not waived. No Guest service was changed by this implementation task.

### Real bounded APFS ENOSPC acceptance

`test-files-disk-full.py` creates its own256MiB APFS disk image and verifies the
image/device/mount/volume-size boundary before any fill. It never fills the host
volume or accepts an arbitrary existing mount. It runs the unchanged file worker
through bounded private-pipe requests. Reproduce with a fresh evidence filename:

```sh
python3 native/guest-macos/test-files-disk-full.py \
  --binary .local/guest-share/macos-guest-agent-files-stage6 \
  --output .local/evidence/macos-files-enospc-stage6-final.json
```

Actual host acceptance passed in
`.local/evidence/macos-files-enospc-stage6-final.json` using the stage6 SHA above:

- Published a sentinel, then accepted32KiB of a separate8MiB upload.
- Wrote260,964,352 filler bytes only inside the image until real errno28
  (`ENOSPC`). APFS still reported5,844,992 available bytes; that statistic is not
  proof that another allocation can succeed. The worker itself also returned
  `file_error: write failed (errno 28)` on the next upload chunk.
- The unfinished staging file and upload token were removed, with no destination
  published. Policy reported zero active transfers and zero staged uploads.
- The previously published sentinel's SHA256 remained unchanged.
- Removing the test filler allowed a new64KiB upload to succeed through the same
  worker. The sentinel remained intact. Worker EOF exited0, the exact owned image
  was detached, and private scratch was removed. A final `hdiutil info` confirmed
  no test image remained attached.

The initial CLI invocation failed before image creation because `hdiutil create`
uses `-type UDIF` for blank images rather than `-format UDRW`; its failure report
is retained as `macos-files-enospc-stage6-first.json`. The corrected runner passed
before and after adding nonblocking deadline handling to its request writer;
`macos-files-enospc-stage6-second.json` preserves that first successful run.
No production source or binary changed for this test. This validates the real
partial-write ENOSPC cleanup path on a mounted host APFS image, not Guest disk
exhaustion, power-loss durability, post-rename fsync failure, or all filesystem
failure modes. Unexpected detach failure retains scratch and reports failure;
it does not force-detach unrelated devices or claim cleanup succeeded.

## Structured graphics capabilities (stage8)

`info` now includes `graphics` without requiring an opt-in flag or additional
permissions. It reads the public properties of the same single system-default
Metal device used for the retained legacy `metal_device` string. It does not
compile shaders, create pipelines, render, capture the desktop or grant input.
Each info request samples current device properties once; no stale capability
cache or guessed Guest/host GPU equivalence is used.

The graphics envelope is schema1, api `metal`, status `reported` or `unavailable`,
device_selection `system-default`, evidence `public-device-properties`, a query
timestamp and a structured `device`. No Metal device returns `device:null`,
`reason:no_metal_device`, and legacy `metal_device:none`; it does not synthesize
false feature values. The `assessment` explicitly says shader rendering is
`not-tested-by-this-query`, performance `not-measured`, and per-kernel limits
`unavailable-without-pipeline`.

The device contains architecture/name, lossless string registryID, queried OS,
family support booleans, per-axis thread maxima, threadgroup memory/buffer/sampler
limits, argument-buffer and read-write-texture tiers, and compute/render function
pointer, dynamic-library and raytracing support. An unqueried family is absent,
not false; unknown future tier raw values retain their number and `unknown` name.
Thread axes are never multiplied into an implied total. Device-advertised support
does not establish that an individual shader compiles or an application renders.

`native/shared/MetalCapabilities.swift` is shared with the independent Blender
pipeline diagnostic. The diagnostic retains its prior `host_os` spelling through
an adapter; Guest info uses `os_version` inside the device record. Frozen old
binaries do not change. No media, console identity, cleanup or input permission
policy changes accompany these fields.

```sh
native/guest-macos/build.sh --output "$PWD/.local/guest-share/macos-guest-agent-files-stage8"
.local/guest-share/macos-guest-agent-files-stage8 --graphics-self-test
```

The self-test injects synthetic values into the actual info handler and exercises
JSON precision, exact axis values, unsupported/unknown features, no-device output,
same-record legacy naming and unchanged disabled permissions. It performs no
Metal device query or Guest operation. Actual stage8 Guest/MCP acceptance remains
separate from these host-only format tests and is coordinated by the operator.
