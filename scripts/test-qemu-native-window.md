# Existing QEMU VM native window acceptance

This script is attach-only. Supply a running, explicitly chosen QEMU VM on the new window-capable controller/broker/native helper. It never creates, starts, stops or deletes a VM; never changes permissions; and signals only its own bounded MCP client children if they fail to exit. Root/CUA coordinates actual runtime and visual inspection separately.

```
python3 scripts/test-qemu-native-window.py \
  --binary /absolute/path/to/vm-control \
  --state /absolute/private/controller-state \
  --vm explicitly-chosen-qemu-id \
  --output /absolute/path/to/new-evidence-directory
```

The output directory must not exist. It is created with mode0700; reports and PNGs are mode0600. `ffmpeg` must be available for independent PNG decoding. Default MCP/local RPC timeout is20 seconds and post-show GPU-progress wait15 seconds, each configurable only within bounded limits. Each phase checkpoints `report.json`; failure and failed restoration remain explicit, rather than returning a successful exit.

The sequence is:

1. Verify the specified existing QEMU worker and private controller socket identity. Read native window status.
2. Initialize an observation-only MCP client, verify window mutation tools are absent, and explicitly call both show/close to prove they are rejected without changing the initial open/closed state.
3. Initialize a lifecycle-authorized MCP client. If initially open, close first so even a static desktop must produce a newly completed GPU command after the following show.
4. Show, poll bounded native status until frame available and `gpu_commands_completed` increases without new GPU failures, and collect/decode a complete MCP PNG, matching dimensions and one decoded frame.
5. Close, require `window_open:false`, and independently decode another MCP screenshot to demonstrate capture continues.
6. Reopen, require another new GPU completion and collect/decode a third screenshot.
7. Finally restore the pre-test open/closed state, require the same worker/controller identity, and close MCP clients with successful cleanup exits. A failed restoration or client cleanup fails the report.

RPC window state and Metal command completion are not physical presentation proof. `window_visible` is recorded but is not a claim that the pixels are visible, unobstructed, correctly oriented or free of artifacts. Root must inspect the native window using CUA. The script also makes no frame-rate, latency, zero-copy or Parallels comparison claim. It restores open/closed state only; it does not restore focus, minimization or window position.

Development-only tests can be run with `python3 scripts/test-qemu-native-window-unit.py`. They validate evidence parsing, real independent image decode, denied-tool handling and owned-client deadlines using local subprocess fixtures; they are not VM/UI acceptance. The actual script must only be run after root has coordinated the named VM.

The first actual stage4 invocation (`.local/evidence/native-window-stage4-harness/report.json`) failed before any mutation because the original harness incorrectly required `display_gpu_inflight` to be an integer. Read-only inspection confirmed the native field is intentionally a JSON boolean (`BOOL` in `Display.m`), separate from integer `display_gpu_inflight_limit:1`. The corrected harness strictly requires that boolean and separately validates integer counters/limit; integer0/1 substitutes are rejected. Only arithmetic that excludes an older pending command explicitly converts the validated flag to0/1. The original failure is retained; rerun into a new evidence directory.

For a fixture explicitly configured with initial2560×1440 dimensions, add `--expect-width 2560 --expect-height 1440`. Every decoded screenshot must then match those actual dimensions; passing QEMU's initial EDID preference alone is insufficient. Optional native pacing fields are strictly validated as configured ceilings, not measuredfps: capture/window/background/encoder limits must be coherent and the current capture limit must be one of the configured states. The remote encoder start protocol currently has no requestedfps parameter; capture60 does not imply remote60.
