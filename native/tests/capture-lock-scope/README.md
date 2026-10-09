# Capture state lock and authority freshness regression

The actual relay-v6 media failure was `console_identity_verification_stale`
after5259ms (source sequence99, encoded89); the Guest subsequently attested
`stop_capture_completed`. It does not identify which check expired. This change
removes a demonstrated scheduling obstruction, not a proven sole cause of that
Guest failure. No new Guest capture, UI action, permission change or live binary
replacement was performed here.

Previously the sample callback held `condition` across CVPixelBuffer base-address
locking, allocation and the full NV12 row copy. The same condition gates identity
read accounting/publication, writer admission and stop. A blocked copy therefore
blocked these independent operations. The controlled before test failed with
`copy_blocks_identity_condition`, exit1. Its first barrier establishes that the
actual receive path is inside copy before the control queue tries the condition;
it releases the barrier and drains both queues before reporting failure.

The new receive path discards the old latest slot, captures immutable dimensions,
PTS/sequence and generation under the condition, copies on the existing serial
sample queue without that lock, then rechecks generation, accepting/stopping,
connection state, current750ms identity freshness and monotonic PTS before
publishing. There is still only one copy and one socket writer; no extra frame
queue or detached per-frame task. Each client accepts one capture lifetime;
generation starts1 and advances once to2 at idempotent stop. A stopped copy's
error also cannot overwrite the terminal outcome. Actual SCK detach/sample-drain
cleanup proof remains required; the synthetic tests do not attest SCK cleanup.

Authority observations retain their actual completion timestamp, before waiting
to record metrics. Publishing never stamps a later lock-acquisition time. A
query that took more than750ms or a result already older than750ms is rejected.
The500ms timer,25ms leeway, queue QoS and750ms freshness bound are unchanged.
The pre-existing observation is non-atomic; this does not eliminate a subsequent
console transition or bound OS-call scheduling latency. A record already admitted
to a socket write remains subject to the existing bounded write/stop protocol.

## Reproduction (host only, no capture APIs)

Build an unused absolute output with:

```sh
bash native/tools/build-macos-capture-stream.sh --output "$PWD/.local/NEW-capture-probe"
.local/NEW-capture-probe --copy-lock-test
.local/NEW-capture-probe --self-test
```

Both passed on the frozen stage5 recorded in [verification.json](verification.json).
The existing compiler warning about the weak self-test variable was retained;
build success is not a new Guest acceptance result.

The before source is commit `7437efe9a5e7c37224942736a62e66300f19e4ca`'s
`native/tools/macos-capture-stream.swift`. For the controlled failure, extract that
source into a scratch directory and apply [before-instrumentation.patch](before-instrumentation.patch).
It only extracts the same admission/copy body into a callable function and injects
a copy closure, without changing the old condition scope. Compile the extracted
source with [before-test.swift](before-test.swift) and the unchanged shared
`native/guest-macos/console-binding.swift`, using the frameworks/target in the
build script. Invoke `--copy-lock-test`; exit1 is the expected regression failure.
The instrumented source, test and binary hashes and original failure output are
retained here; full scratch files also remain in `.local/capture-lock-stage1`.

The after tests cover blocked-copy authority renewal, stop, changed generation,
newer PTS, expiry and late copy error; delayed metrics publication cannot renew
an old authority timestamp. A real Unix socket receives only error/terminal
records when freshness expires during the copy, never the stale NV12 frame.
Original self-tests retain padding, headers, bounded writes, cleanup ledger,
duplicate-stop/start/drain, quarantine, TTL and management-query checks.

## Additive non-sensitive diagnostics

`error`, `terminated`, and `stopped` carry `console_identity_diagnostic`:

- `stale_site`: null or fixed `authority_read`, `authority_publish`,
  `sample_admission`, `sample_publish`, `writer_admission`.
- `verification_age_us`: null or a nonnegative JS-safe integer. At
  `authority_read` this is the read duration, at other sites the verification age.
- `authority_read_max_us`, `identity_publish_lock_wait_max_us`,
  `sample_copy_max_us`, `sample_lock_wait_max_us`,
  `identity_timer_lateness_max_us`: saturated JS-safe integer monotonic durations.
- `sample_generation`: per-client lifecycle generation, never authority.

The first stale site is retained. Timer lateness measures dispatch invocation
against the nominal500ms grid, not OS-query duration; missed firings are coalesced
without catch-up queries. `timer_read` was discussed as a reserved receiving-side
allowlist value but is not emitted: authority timing and publication are now
separate. No pixel data, identity, username, path or arbitrary error description
is added. Stop-related cancellation is silently discarded rather than falsely
labeled stale. Normal diagnostics do not grant input or weaken cleanup proof.
