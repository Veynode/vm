# Exact input/frame diagnostic (opt-in)

This diagnostic observes the existing current-to-history rejection. It does not grant input, refresh a timestamp, retain extra presented frames, rebind a token, replay an event, or change the 8-record history, 250 ms historical age, 200 ms queue, focus, authority or release rules.

Append `--input-frame-diagnostic` to the native helper's existing `SOCKET --persistent [--control-socket PATH --vm-id ID --worker-instance HEX]` arguments. It is disabled by default. The prepared wrapper appends this flag only when its second argument is `--persistent`; capability probes pass through unchanged. It requires the controller/broker's optional diagnostic-ID forwarding implementation; do not enable it with an old controller that rejects the extra event field.

`window_status.input_frame_diagnostic` is a version-1 object with `enabled`, `authority:false`, `capacity:16`, `issued`, `evicted`, `unmatched_validations`, `duplicate_validations`, and `records`. Default-off records stay empty, and native events contain no diagnostic ID. There is no new status polling, RPC, log file, or permission.

Each record contains:

- `diagnostic_id`: process-local monotonically issued integer 1..9007199254740991; never reused. Exhaustion omits further IDs, without changing input policy.
- `token`: the original eight-field token, including full unsigned integer values; it is not an authorization receipt.
- `acquisition`: `monotonic_seconds`, `record_found`, `current`, `callback_received_monotonic_seconds`, `callback_age_ms`. The actual snapshot and observation are read under the existing publication lock during the AppKit callback. Original callback time is preserved. Ages are signed observations, not clamped or used to authorize.
- `enqueue_epoch`: authority epoch observed under NativeControl's existing lock at queue admission.
- Optional `dispatch`: actual request `sequence`, `epoch`, local monotonic time, and existing pre-exchange `queue_age_ms`. This precedes socket exchange and does not prove receipt or Guest delivery.
- Optional `validation`: the same five observation fields at the unchanged helper validator, plus `accepted` and a fixed `reason` string (null for success). Record association requires both exact ID and exact token. A duplicate validation cannot overwrite the first; unknown/foreign matches only increment a counter.

The top-level optional `diagnostic_id` travels with the event and through `validate_displayed_frame`; the frame token remains exactly eight fields. The Rust parser and native helper accept only JSON integers in the ID range (no null, bool, float, string or zero). The ID has no role in authority or validation decisions. No key code, text, button, pointer coordinate, or event body is recorded.

A missing dispatch/validation is meaningful: move coalescing, overflow/suspension, pre-validation rejection, disconnect, or ring eviction can leave an incomplete record. Sixteen records are intentionally a small rolling window; query soon after rejection. Validation after history invalidation/eviction reports `record_found:false`, with null current/time/age. It does not reconstruct missing data from the latest snapshot. Records do not identify the exact instant of replacement, only currentness at the two observations.

## Verification and build

The component regression uses the real `VNDisplayedFrameStore`: acquire an aged but current frame, commit a new frame, and validate the original. The original `display_historical_expired` rejection remains, while the diagnostic records current→historical and original ages. Additional cases cover ID isolation, replay preservation, invalidation, capacity, default-off wire shape, strict ID types, and full-width token values.

The real private Unix NativeControl fixture verifies the optional field, exact sequence/epoch association, and unchanged rejection→suspend/release behavior. No VM/input is dispatched by that test. The no-application status selftest populates 16 complete diagnostic records and 120 presentation samples in the actual `VNDisplay` status, requiring less than 56 KiB to reserve at least 8 KiB for existing GLib-owner aggregates within the conservative 64 KiB envelope. No response limit is increased.

Build to a fresh path with `native/console/build.sh --output-app ABSOLUTE_NEW.app`. Run only the selected Foundation and headless selftests for a no-GPU check; the general `test.sh` also contains pre-existing Metal capability tests and is not a no-device test suite. This change has component/build verification; actual CAD association requires a separate runtime acceptance.
