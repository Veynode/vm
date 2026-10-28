# Guest Metal presentation instrument

Build a fresh staged executable and run only inside a non-root VirtualMac Guest:

```sh
native/tools/build-guest-metal-presentation.sh --output "$PWD/.local/guest-share/guest-metal-presentation-new"
.local/guest-share/guest-metal-presentation-new --self-test
# Guest Terminal, while the native VM window is visible:
./veynode/guest-metal-presentation-new --log /Users/veynodetest/presentation-new.jsonl --seconds 60
```

The private JSONL path must be absolute and new. No network, capture, input, TCC,
model or Agent harness is included. It draws a deterministic fullscreen sine-field
shader through one triangle in an MTKView at requested60fps, with5seconds warmup.
The actual drawable pixel dimensions and scale are recorded. Requested rate is
not measured rate. This simple workload is instrumentation, not representative
CAD/gaming compatibility or performance acceptance.

The source records [MTLDrawable.presentedTime](https://developer.apple.com/documentation/metal/mtldrawable/presentedtime)
inside [addPresentedHandler](https://developer.apple.com/documentation/metal/mtldrawable/addpresentedhandler(_:)).
Apple documents zero as not presented/dropped. Zero/invalid values are explicitly
counted and excluded from frame-interval statistics; command completion or handler
invocation never substitutes for a valid presentation timestamp. In a Guest,
these APIs report the Guest graphics system. They are not proof of physical host
scanout, native-window presentation, end-to-end input latency, or remote presentation.
No automatic conversion to the comparator's `kind:frame` records is provided.

At most2 GPU command buffers are in flight. This does not bound unpresented
frames. Missing drawable/backpressure ticks, missing callbacks, invalid times,
nonmonotonic intervals, sequence gaps and command failures are explicit. Callback
writes are synchronized and sealed after a2second final drain; missing/late samples
cannot become invented frames. The instrument records the real measurement start/
end and first/last presented time, exposing unobserved leading/trailing duration.
Normal interval percentiles do not hide those gaps. Visible/occluded observations
are recorded but do not prove that no overlay covered any pixels. A separate global
watchdog exits124 after warmup+requested duration+10seconds, including when the
main runloop stalls. Abrupt termination has no complete-log guarantee.

Up to8000 samples/4MiB are retained; logging occurs after measurement, then fsync.
The JSONL summary's `measurement_complete` describes callback/timestamp coverage,
not disk durability, smoothness or meeting60fps. Final stdout additionally reports
`log_write_confirmed`; require it and exit0 before treating the run as complete.
Serialization/write/fsync failures cannot silently pass. Retain failed runs. The
collector does not supply the full comparison manifest, host resources or input
latency. See vm-archive/benchmarks/README.md for those separate requirements.
