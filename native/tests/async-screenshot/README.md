# Native screenshot off-loop component checks

The native console now takes a real frame-pool lease on its owner loop, freezes the source metadata, and performs the CPU pixel snapshot, PNG encoding and base64/JSON serialization on one background job. The source-copy, SPICE draw ACK and display publication code is unchanged. Headless builds retain their existing path.

The owner permits one screenshot until both its worker and output receipt settle. Before output admission it rechecks live connection, surface/generation and geometry. A newer source sequence alone does not invalidate an immutable older snapshot. Generation/connection loss rejects the old success; shutdown suppresses it and cannot acknowledge before the worker releases its lease. The worker completion timestamp must meet the two-second absolute admission deadline, including when the owner tick arrives late. ImageIO is not forcibly interruptible: an unretired worker during final teardown forces process exit 1, never a clean cleanup claim.

All native framed records use one serial nonblocking writer: at most 16 records and 8 MiB including in-flight storage and 13-byte headers; at most 4 MiB per body, except one JSON (kind 1) record of up to 12 MiB, queued beside that budget, which carries a full-resolution screenshot reply. Reservation, compact copy and dispatch share one ordering lock. Each record has a 500 ms absolute admission deadline, including queue time. Partial writes, backpressure timeout and budget exhaustion permanently fence the writer; subsequent capture-error output cannot append a new frame into a partial one. The shutdown receipt follows earlier admitted records. A record already admitted before source invalidation remains subject to this bounded write contract; this is not atomic whole-record freshness or zero post-invalidation bytes.

PNG output uses an 8 MiB bounded data consumer (the documented PNG contract; it was 2 MiB before decision 0024's 2026-10-11 raise reached the native side). Maximum pixel geometry remains 3840×2160; one CPU snapshot and ImageIO internals are additional memory, so the wire budget is not a whole-process RSS bound. The fixed screenshot diagnostic schema remains; overflow reports unknown final encoded length as null rather than inventing a full PNG size. Caller-owned mutable wire bodies are copied. Frame leases release after CPU copying (or its failure), not after a timeout flag.

`window_status.screenshot_timing` is null before the first settled job, then the latest job only. Schema version 1 includes `timing_clock: CLOCK_MONOTONIC`, frozen `source_generation`/`source_sequence`, `success`, and these microsecond fields:

- `admitted_monotonic_us`, `worker_started_monotonic_us`, `copy_completed_monotonic_us`, `png_completed_monotonic_us`, `worker_completed_monotonic_us`, `glib_completion_monotonic_us`;
- when output is attempted: `output_admitted_monotonic_us`, and after the actual write receipt, `output_completed_monotonic_us`;
- `copy_us`, `png_us`, `serialize_us`, `worker_total_us`, plus `output_queue_us` and `output_write_us` when applicable.

Original `captured_at_us` and `captured_monotonic_us` remain unchanged. Failed/cancelled jobs can omit output fields. `success` means the screenshot passed publication checks and its full output record was written; it is not proof of client decoding or physical display. A status request arriving before the next owner tick can still see the preceding settled job: associate samples by source identity, not request order alone. These clocks are not automatically calibrated to CAMetalDrawable presentedTime.

## Reproduction and actual scope

```sh
native/console/test-screenshot.sh
native/console/build.sh --output-app "$PWD/.local/NEW-screenshot-console.app"
native/console/test.sh "$PWD/.local/NEW-screenshot-console.app"
```

The focused test uses eight real pipe groups (mixed framing/FIFO, concurrent producers, mutable-copy isolation, actual partial write timeout, in-flight count and byte bounds, invalidation/drain/EPIPE, invalid admission); actual CPU ImageIO PNG decode and incompressible overflow; real three-slot lease retention and exception/cancellation; and six cases running the production owner command/tick path: newer sequence, retired generation, real socket EOF, shutdown barrier, closing fence and completed-after-deadline rejection. Test-only hooks are absent from the production build.

The staged v2 build and focused/full existing native checks passed; hashes and sanitized logs are alongside this file. Existing native checks include actual Metal pipeline creation, with no window or GPU command submission; no VM, SPICE server, encoder, screenshot workload or cadence comparison ran in this component step. The full existing suite also independently builds the unchanged headless path, checks typed capabilities and refuses overwriting the staged app.

An initial writer test compile failed because an untyped Objective-C array made `status` ambiguous; explicit VNWriteReceipt generics fixed the fixture. An initial screenshot test compile needed an explicit CGBitmapInfo enum cast. A first syntax-check command incorrectly passed link-framework flags to compile-only `-c` under Werror; no source compile was attempted by that command. These command/fixture failures are not GPU failures. The v1 staged app is retained but superseded by v2, which adds the independently reviewed delayed-tick deadline check. Actual same-workload screenshot-neighborhood validation is a separate subsequent acceptance and is not claimed here.
