# macOS Guest service

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
  implemented capabilities (`exec`, independently available `screenshot`, `desktop:false`, `files:false`) and an `observation` policy with current preflight permission status.
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
by the service. Guest input, accessibility, files and a signed installer remain unimplemented. The independently authorized screenshot method is described below; the separate PNG diagnostic remains available for local diagnosis.

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
[control-macos-screenshot](../../docs/evidence/control-macos-screenshot.md): two
fresh frames with the native window open and two after closing it, all2560×1440,
Guest point scale2, selected JPEG quality0.65, command execution disabled. The
same installed Guest remained running. This does not establish continuous capture,
browser operation or unattended shutdown.
