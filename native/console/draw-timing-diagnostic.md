# Passive native draw timing diagnostic (opt-in, version 1)

Purpose: attribute where the native display cadence observations come from, without changing any decision. It is observation metadata only. It does not change pacing, source-copy admission, frame leases, ACK, inflight, control authority, input authority, the 120-record presentation ring, or the 16-record input diagnostic.

Implementation: `native/console/DrawTiming.h`, `native/console/DrawTiming.m`, the call sites in `native/console/Display.m` (`drawInMTKView:`, completion and presented handlers, `status`, `main`), component tests `native/console/DrawTimingTests.m`, and the saturated-status self-test `--draw-timing-status-self-test`.

## Enablement

- Helper flag: `--draw-timing-diagnostic`, default absent (off).
- Parsed in the trailing-flag loop of `Display.m` `main` with `--input-frame-diagnostic` and `--native-frame-acquisition`. It may follow `SOCKET --persistent [--control-socket PATH --vm-id ID --worker-instance HEX]`.
- Planned: the wrapper appends the flag at the end only when the second argument is `--persistent`. The broker's `--probe-capabilities` call passes through unchanged. Rust argument protocol is unchanged.
- **Pending:** no wrapper is wired into the repository or any run yet. The generator is being built under `benchmarks/linux-stk-native-stage-v1/`. Until it exists and is pinned, the flag is parsed but cannot be reached through the broker.
- When off, the object is `{"version":1,"enabled":false,"authority":false,"capacity":32}`. The diagnostic adds no timestamp reads, stores no records, and changes no draw decision. The presented handler's existing `NSProcessInfo.systemUptime` read (`received`, passed to `commitCandidate`) predates this change at `b5d6e8d` and is unchanged; it is not a diagnostic read.

## Export

`window_status.draw_timing` in the existing `VNDisplay -status` result, reached through the unchanged `window_status` RPC. No new RPC, file, log, stdout line, permission, or wire-limit change. Rust does not validate this field; the Rust window-status reply limit is 1 MiB.

## Clocks (never joined without a documented calibration)

- `cpu`: `NSProcessInfo.systemUptime`, used for every new CPU timestamp.
- `metal_gpu`: `MTLCommandBuffer.GPUStartTime` / `GPUEndTime`, Metal's clock. Raw values are kept, including zero.
- `metal_presented`: `CAMetalDrawable.presentedTime`, Core Animation's clock. Raw values are kept, including zero.
- The existing GLib source clock, `callback_received_monotonic_seconds`, POSIX screenshot time, and Guest loop clocks are never joined to these.

## Object shape (enabled)

```json
{
  "version": 1, "enabled": true, "authority": false, "capacity": 32,
  "clock": {"cpu": "NSProcessInfo.systemUptime", "metal_gpu": "MTLCommandBuffer.GPUStartTime/GPUEndTime", "metal_presented": "CAMetalDrawable.presentedTime"},
  "callbacks": 0,
  "branches": {
    "inflight": 0, "no_window": 0, "invisible": 0, "unchanged_source": 0,
    "no_drawable_or_pass": 0, "command_or_encoder_failed": 0, "texture_failed": 0,
    "retired_generation": 0, "submitted_frame": 0, "submitted_clear": 0
  },
  "lease_lost": 0,
  "attempts_issued": 0,
  "submissions_issued": 0,
  "evicted": 0,
  "pending_evicted": 0,
  "late_updates": 0,
  "invalid_clock": {"cpu_stage": 0, "metal_gpu": 0, "metal_presented": 0},
  "completions": {"completed": 0, "failed": 0, "gate_applied": 0, "gate_epoch_changed": 0, "gate_revision_changed": 0},
  "records": []
}
```

### Branch accounting (exact)

- Every `drawInMTKView:` call increments `callbacks` and exactly one of the ten `branches` keys:
  `callbacks = inflight + no_window + invisible + unchanged_source + no_drawable_or_pass + command_or_encoder_failed + texture_failed + retired_generation + submitted_frame + submitted_clear`.
- Precedence is the original code order: `inflight`, `no_window`, `invisible`, `unchanged_source` (same generation, sequence and drawable size as the last applied submission), then drawable/pass acquisition, command/encoder creation, texture creation, the retired-generation check before commit, then commit as `submitted_frame` (source frame) or `submitted_clear` (no source frame).
- `lease_lost` (top level and per record) is an annotation: the source frame lease could not be retained, so the attempt draws clear-only. Its terminal is still one of the branches above.
- Only callbacks that pass `unchanged_source` create attempt records and IDs.
- `recordCallbackBranch:` accepts only the four callback-only branches (`inflight`, `no_window`, `invisible`, `unchanged_source`). Attempt terminals use `attempt:terminal:` or `attempt:committedAt:clear:`.

### Attempt records (ring of 32, oldest evicted first)

`attempt_id` is a process-local integer from 1, never reused. `submission_id` is a process-local integer from 1, in commit order, for committed attempts only.

| key | meaning |
| --- | --- |
| `attempt_id`, `submission_id` | stable identities; `submission_id` null if not committed |
| `terminal` | `pending`, or a branch name for an attempt terminal |
| `final` | true when the record cannot change (see Coverage) |
| `window_epoch`, `generation`, `sequence` | `window_epoch` is read at callback entry exactly as the original code does. `generation` and `sequence` are read under the publication lock. For clear-only attempts `generation` and `sequence` are recorded as 0 |
| `revision` | `VNDisplayedFrameStore.revision`, read under the publication lock before drawable acquisition. It is the same value the completion gate compares |
| `drawable` | `[width, height]` integer pair, or null when no drawable was acquired |
| `lease_lost` | boolean |
| `entry_s` | `systemUptime` at `drawInMTKView:` entry (µs-rounded seconds) |
| `pre_ms` | entry → start of drawable/pass acquisition (visibility, layout, early checks) |
| `wait_ms` | `currentDrawable` + `currentRenderPassDescriptor` duration |
| `submit_ms` | drawable acquired → `commit` returned |
| `gpu_entry_ms` | commit returned → Metal completion handler entry. Not GPU-only time; may be negative if the Metal thread enters completion before the main-thread commit call returns |
| `inflight_ms` | Metal completion handler entry → main block that clears `inflight` |
| `pres_entry_ms` | commit returned → Metal presented handler entry (signed) |
| `pres_main_ms` | Metal presented handler entry → main block that records the sample |
| `status` | integer `MTLCommandBufferStatus` once completed, else null |
| `gate` | `applied`, `epoch_changed`, `revision_changed`, `command_failed`, or null while pending |
| `gpu_start_s`, `gpu_end_s` | raw Metal GPU seconds, kept as given (0 kept), null if non-finite or never completed |
| `pres_s` | raw `presentedTime` seconds (0 kept), null if non-finite or never presented |

Durations are millisecond values rounded to 1 µs. A stage not reached is null, never zero. Records hold numbers and fixed strings only: no pixels, no drawable or command references, no input bodies, no Guest data.

### Counters

- `attempts_issued`: total attempt IDs issued. `submissions_issued`: total committed submission IDs.
- `evicted`: records dropped from the ring. `pending_evicted`: the subset that were not final when dropped.
- `late_updates`: stage callbacks whose attempt had already left the ring. They are counted, never reconstructed.
- `invalid_clock`: `cpu_stage` counts non-finite or non-positive CPU stage values that were not recorded. `metal_gpu` counts zero or non-finite GPU start/end values. `metal_presented` counts zero, non-finite, or non-increasing presented values, using the existing `VNPresentedSamples` monotonic policy. Raw values are still kept in the record.
- `completions`: `completed` and `failed` count command status, and the `gate_*` keys count the unchanged completion gate per completed command. `gate_applied` means `lastGeneration`/`lastSequence`/`lastDrawableSize` were updated, as before. The failed-command gate is counted in `failed`, not in a `gate_*` key.

## Coverage contract (for consumers)

- `final` for a non-submitted attempt means its terminal is set. For a submitted attempt it means the inflight-clear stage is recorded and either the presented-main stage is recorded or the gate is `command_failed`.
- Across polls, `attempt_id` must form a contiguous sequence with no gap unless the consumer records the gap as a coverage failure. Records are updated in place by `attempt_id`.
- A submitted attempt whose presented handler never fires remains non-final and is counted in `pending_evicted` if evicted. Incomplete tails are reported as incomplete, never backfilled from the latest ring.
- Presented stages that arrive after a command failure (`gate` = `command_failed`) are counted in `late_updates` and do not change the record. A final record cannot change.
- The monotonic presented verdict in `invalid_clock.metal_presented` is taken on the Metal presented thread. `VNPresentedSamples` takes its verdict on the main thread. The two verdicts could differ only if presented handlers complete out of order, which has not been observed. Raw values are authoritative.

## Size budget (measured)

- Existing saturated VNDisplay status, 16 input diagnostics + 120 presentation samples, with the minimal disabled draw_timing object: 38121 bytes.
- Saturated status with 32 fully populated draw timing records (worst case: 16-digit identifiers), 16 input diagnostics and 120 presentation samples: 55357 bytes. That leaves 1987 bytes below the 57344-byte limit. It is asserted by `--draw-timing-status-self-test`.
- Real production run (STK v2 archive for comparison): archived `window_status` rows averaged 24040 bytes including 120 presentation samples (18360 bytes). A 32-record draw timing object adds about 12–16 KB at realistic identifier sizes, which keeps a production reply near 40 KB, below 56 KiB. The input diagnostic is off in the STK run.

## Verification

1. `DrawTimingTests.m` (Foundation only): disabled no-op and minimal shape, exact branch identity, admission precedence, stable IDs, saturated bounded ring, pending eviction, late updates, null-not-zero stages, invalid CPU/Metal clocks, raw Metal zero, monotonic presented policy and completion gates. Passing.
2. Real `VNDisplay` saturated status self-test with 32 populated draw timing records: 8 evicted, 0 pending, wire below 56 KiB. Passing.
3. `native/console/test.sh` on a fresh staged app: 14 PASS lines, exit 0. Passing.
4. Independent read-only review of the native diff (decision preservation, locking, counts, size). No blocker or major findings. Its minor findings are fixed: clear-only identity is 0, presented stages after command failure are late, status copies the ring under the lock before building records, and blocks capture a local `timing`. Re-verified with the component suite and the staged app's `test.sh`.
5. Frozen private build and wrapper pins, then runtime only through `benchmarks/linux-stk-native-stage-v1`. Pending.

This is a diagnostic contract only. It is not a smoothness, latency, or photon measurement.
