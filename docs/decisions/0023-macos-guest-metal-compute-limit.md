# 0023 — macOS Guest Metal compute: large pipelines are a backend limit; first-release statement

Date: 2026-10-11. Status: **classification by the technical lead from retained evidence; not a scope reduction.** The macOS
Guest, Blender and GPU acceleration stay first-release requirements. What changes is how Cycles on the Guest's Metal device is
described and what is offered instead.

## Evidence (records are on the `remote-ice-admission-binding` branch, not in this tree)

1. Guest Metal pipeline creation succeeds for small kernels (the Cycles control kernel, synthetic kernels up to a host cold
   compile of 9.8 s) and does not for the large Cycles kernels `shade_background`, `shade_surface`, `shade_shadow`
   (no answer within 120 s on the async path, 600 s tested for one; instant nil without an error on sync and plain paths in a
   fresh process) and `shade_volume` (`CompilerError` after 70 s on every path in a fresh process).
   `metal-cost-ladder-v1` (record on the `remote-ice-admission-binding` branch, `docs/evidence/metal-cost-ladder-v1/README.md`).
2. Synthetic non-Cycles kernels reproduce the failing region; the host compiles the same sources in seconds. So it is not
   Blender and not a defect in the Cycles source. `dominators-pairs-v1` (record on the `remote-ice-admission-binding` branch, `docs/evidence/blender-cycles-kernel-inventory-v1/dominators-pairs-v1/README.md`).
3. During a failing request the host's `MTLCompilerService` processes used +0.04 s CPU over 130 s and the Virtualization VM
   service used CPU at its idle rate (positive control: a Guest busy loop gave 1.3 s/s). Neither side works on the request.
4. The Guest's Metal device reports architecture `air64_v27` (`Apple Paravirtual device`); host-built binary archives
   (`applegpu_g13s`) open but are not used (miss, under 0.2 ms), so they cannot provision the Guest.
   `metal-binary-archive-v1` (record on the `remote-ice-admission-binding` branch, `docs/evidence/metal-binary-archive-v1/README.md`).
5. The worker's macOS graphics device is `VZMacGraphicsDeviceConfiguration` with one display; it has no compile-related
   option. The only variables within our reach are Guest CPU count and memory, not yet varied (low expected value after 3).
6. The Guest lost its Metal device twice after runs of such requests; each time a normal Guest restart recovered it. Trigger not
   established: a check of the leading hypothesis (async abandoned at its limit, then sync and plain creation in the same
   process) did not reproduce it in three processes. This is a robustness gap of the Guest's Metal path under heavy failed
   requests, not isolated.
7. Cycles on the Guest's virtual CPUs works (Blender 5.2.2, 0.20 s on the control scene, statistics equal to the host's).
   `blender-cycles-cpu-guest-v1` (record on the `remote-ice-admission-binding` branch, `docs/evidence/blender-cycles-cpu-guest-v1/README.md`).

## Classification

**Backend capability limit of the Apple paravirtual graphics stack** for large compute pipelines (with a stall instead of an
error for the largest), reproduced without Cycles. Not our implementation; not a Blender defect. Not a fault we can fix in
`vm-apple` with the options Virtualization.framework exposes.

## Decision

- **First-release statement for the macOS Guest:** Metal graphics and small compute pipelines work (Godot Metal Forward+
  six-case sample, Blender UI, the compute 1M and triangle checks). **Cycles on the Guest's Metal device is not available**;
  Cycles runs on the Guest's CPUs. No Parallels or performance comparison is made from this.
- **Do not ship a workaround that hides the limit:** no silent CPU fallback in a product path that was asked for GPU; a request
  for Metal Cycles in the Guest reports the limit.
- **Keep the reproducer** (`native/tests/metal-pso-gate` and the cost ladder, both on the `remote-ice-admission-binding` branch) as the regression check: it is the test to rerun
  on each new macOS Guest version or host OS, because the limit is Apple's and may move.
- **Robustness item (ours to track):** a lost Guest Metal device (`MTLCreateSystemDefaultDevice` nil in a fresh process) is
  already reported by the Guest agent's `info` as `graphics.status: "unavailable"`, `reason: "no_metal_device"` and
  `metal_device: "none"` (`native/shared/MetalCapabilities.swift`, asserted in `native/guest-macos/graphics-tests.swift`).
  Not observed live: the agent was not queried during either loss, so whether a long-running agent process sees the same
  nil as a fresh process is untested. Open: surface this in the daemon's health output and in operator guidance, and test
  it the next time the device is lost (recovery found: a normal Guest restart).

## Health-path correction, 2026-10-16

The daemon alert described as open above had already been implemented. This continuation fixes its unsafe inference:
an older/malformed successful `guest_info` without a valid graphics report no longer clears a previous explicit device
loss. A separate last-successful observation reports unknown/unavailable/reported; only a schema 1 public Metal device
report with the expected device identity/query timestamp clears the loss alert. Failed queries retain both last records,
worker events preserve them, and a new worker has no inherited device observation. Operator guidance requests saving
Guest work, normal shutdown, waiting for stopped, restarting and querying again; no automatic recovery or CPU fallback.
The 250-test control library run passed with 3 existing ignored tests, including archived real Swift/MCP wire compatibility.
This does not resolve the important live gap: a long-running Guest agent was not observed during an actual loss/recovery.
The presence report also does not validate rendering, large pipelines or performance.

## What would change this

A macOS or Virtualization.framework update that lets the large pipelines create (rerun the ladder and the three-path check);
an Apple statement of the limit; Guest CPU/memory variation showing a dependence; a different Guest macOS version.

## What this does not do

It does not claim the limit is permanent, does not drop Blender or macOS from scope, and does not say anything about
Linux or Windows GPU paths.
