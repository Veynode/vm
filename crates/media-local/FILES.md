# Files on the authenticated console channel

This opt-in implementation carries bounded file operations over the existing
signed-session DTLS/SCTP DataChannel. It is not an MCP transport. Standard MCP
stdio and HTTPS retain their existing independent permissions and behavior.

`vm-media-local ... --guest-stream --allow-files` requires the existing
`--auth-state`, `--auth-namespace`, `--vm`, and complete strict
`--guest-binding-state/--guest-binding-vm/--guest-worker-instance/--guest-bridge-instance`
configuration. The private controller and media bridge must match the selected
VM and their pinned incarnations. This initial browser path uses macOS Guest
binding; it does not advertise QEMU, Linux or Windows browser files support.
Existing observe-only grants and default scopes do not change. A local admin
must explicitly approve the device for sorted scopes `files,observe`; requesting
files is also explicit in the browser. No input lease, command permission or
macOS input capability is granted by this option.

Each file request is a signed `ControlRequest` bound to the exact confirmed
session, answer transcript, monotonically increasing sequence and short expiry.
Its action is `{kind:"file",operation:...}`. Seven operations are supported:
info, upload_begin, upload_write, upload_commit, download_begin, download_read,
and cancel. See `session-auth/src/files.rs` and `tests/files-golden.json` for
strict fields and canonical bytes. Upload chunks use the `data` field with
canonical standard base64, at most 32KiB decoded; total files are at most 8MiB.
Only one non-reserved filename inside the explicitly configured Guest files
root is accepted. Guest implementation determines atomic no-overwrite
publication and final SHA256. The macOS root belongs to the non-root service
UID, and is not isolation from other Guest programs running as that same UID.

The adapter assigns a fresh owner distinct from desktop input, checks current
grant and pinned VM/worker/bridge before dispatch and again before returning a
result, and has one operation in flight with no queue. Source checks and Guest
file execution are different RPCs; they are not an atomic VM-incarnation
transaction. A detected change fails closed and retires the owner. The request
has a 10s overall deadline, including source checks. A valid Guest business
error becomes only `{ok:false,error:{code}}` from a fixed allowlist, without
paths or raw diagnostics. Invalid replies, transport, source, signature and
replay errors fence the channel. Info excludes the Guest root path.

Close immediately fences authority and cancels the waiting operation. Its
independent cleanup task survives cancellation of a close waiter, sends real
controller `session_end`, validates owner retirement and every Guest file
cancellation result, and retains its first outcome, including failure. The
files owner cannot have jobs or held input; those appearing in its receipt
invalidate proof. Cleanup waits at most 10s for the daemon's 8s file cleanup
budget. HTTP close, channel retirement and service shutdown wait at most 12s;
missing or failed cleanup never becomes success on a repeated close. Native
capture recovery cannot override failed file cleanup. A cancelled upload is
not rollback of a publication already completed by the Guest.

Validation on 2026-10-09: 7 private Unix protocol/real-P256 adapter tests passed
(owner-scoped dispatch, no input lease, sanitized errors, replay, registry
revocation, changed worker, close/in-flight cancellation and sticky cleanup).
One opt-in/scope route test and 2 canonical/boundary tests passed. Regression:
media binary 105 passed, 8 explicit external-fixture tests ignored; media
library 12 passed; session-auth 23 passed, 1 explicit Keychain fixture ignored.
Clippy all targets passed with warnings denied. Unix fixtures simulate the
controller replies and do not prove Guest filesystem or browser delivery;
actual browser-to-Guest acceptance is a separate checkpoint.

Reproduce without touching a running binary:

```sh
CARGO_TARGET_DIR=.local/browser-files-target cargo test -p vm-media-local --bin vm-media-local -p vm-session-auth --lib -j2
CARGO_TARGET_DIR=.local/browser-files-target cargo test -p vm-session-auth -j2
CARGO_TARGET_DIR=.local/browser-files-target cargo clippy -p vm-media-local -p vm-session-auth --all-targets -j2 -- -D warnings
```

## Sequential-response and registry-read correction

Two browser uploads ended unexpectedly in the first actual integration run;
cleanup was acknowledged, but that version did not report a useful channel
failure category. These observations alone do not establish their cause.
Subsequent focused testing found and corrected two independently reproducible
races:

* `DataChannel::send` enqueues bytes before its cooperative completion tail.
  A real localhost DTLS/SCTP peer received a response and immediately submitted
  the next signed request while a test-only barrier held that tail pending.
  The previous loop incorrectly fenced this sequential request as concurrent.
  The loop now rejects overlap during dispatcher execution and separately
  completes the bounded 4s response send. Incoming transport fragments can
  remain in the library's bounded queue while sending; they are not dispatched
  concurrently. Existing malicious concurrent-dispatch rejection still passes.
* A strict existing 8-reader registry test failed with `registry busy`. In
  edition2021 the tail-expression temporary `Store` could be destroyed after
  the local mutex guard, briefly releasing the mutex before the file lock.
  The function now explicitly drops `Store` while holding the mutex, including
  a registry parse failure. Ten focused runs (8 threads, 32 reads each) passed;
  each also verified an actual external file lock is still refused.

Failure logging contains only fixed categories (request binding/signature,
replay, concurrency, response send, authorization, source, backend and timing),
never underlying errors, paths, file bytes or tokens. At most the first fixed
Guest business-error code is logged per file adapter. Actual browser failures
must be distinguished using these categories; the regression fixes alone do
not prove which race caused a previous live-session failure.

After these changes the media binary suite passed 107 tests with 8 explicit
external fixtures ignored, and Clippy all targets passed with warnings denied.
The original failure and fixed-run logs are retained locally as
`control-response-tail-before.txt`, `control-response-tail-after.txt`,
`browser-files-v2-rust-tests.txt`, `browser-files-v2-rust-tests-fixed.txt`, and
`control-registry-read-fixed.json` under `.local/evidence/`.
