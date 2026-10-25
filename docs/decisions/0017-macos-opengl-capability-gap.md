# 0017 — Treat macOS Guest OpenGL acceleration as a separate release gap

Date:2026-10-10. Decision: preserve the macOS ARM product requirement and direct
VZ native display; do not label Metal device availability as OpenGL acceleration
or pass FreeCAD on a software renderer. This is a measured capability boundary
for the current host/Guest combination, not a claim about all future Apple releases.

## Evidence

Official native ARM FreeCAD1.1.4 on the existing4CPU/4GiB macOS26.6.2 Guest
constructed the first constrained solid, but its selected Qt OpenGL context
reported Apple Software Renderer2.1. Strict GPU acceptance failed before capture
and the edit. [Original actual failure](../../archive/docs-evidence/freecad-macos-live/cad1-software-renderer/README.md).

An independent identical binary enumerated only one online CGL renderer in the
Guest, acceleration0. Legacy,3.2-core and4.1-core required-accelerated choices all
returned kCGLBadPixelFormat10002/no matches. Unrestricted choices were software,
including4.1. On the host, the same binary found Apple M1 Pro accelerated and all
three required-accelerated choices succeeded. [Host/Guest evidence](../../archive/docs-evidence/freecad-macos-live/cgl1-no-accelerated-renderer/README.md).
No shader, draw, performance or presentation was measured by this enumeration.
The v1 FreeCAD widget ownership was not recorded, but this independent result
means simply selecting a different current CGL profile is not an evidenced fix.

The earlier real Blender EEVEE Metal render remains valid and distinct; full
Cycles pipeline remains failed. Apple's [framework description](https://developer.apple.com/documentation/paravirtualizedgraphics)
and [WWDC explanation](https://developer.apple.com/videos/play/wwdc2022/10002/)
describe Metal acceleration. They do not establish that this observed Guest
exposes accelerated CGL. Our exact running Guest evidence is decisive for this
configuration. No new preference or security permission was changed.

## Consequences and next decisions

- Do not repeat identical FreeCAD runs or change2.1 to4.1 hoping for acceleration.
  Keep the prepared Qt ownership diagnostic unrun unless new contradictory evidence
  justifies it. Geometry-only or software-rendered results cannot pass the CAD gate.
- Classify current missing public accelerated CGL as a backend/API capability gap.
  FreeCAD/Coin's desktop OpenGL dependency is a compatibility requirement. The
  independent Metal/Cycles compiler issue remains a separate failure.
- A graphics translation or alternative licensed component would need to provide
  the application's desktop OpenGL/context integration, not merely decode video
  or advertise OpenGL ES. No viable drop-in component has yet been demonstrated.
  Do not remove macOS, Windows or CAD from the release scope to hide this gap.
- Keep ordinary game/CAD release readiness open. Evaluate any replacement with
  the same application, pinned model, actual hardware renderer, visible correct
  pixels, input, bounded execution and performance methodology. Do not start a
  large custom driver effort without a separately evidenced route decision.

The root UI launch also exposed a separate modifier-delivery problem: requested
Python -I became -i. This did not alter the recorded CGL child's binary hash or
minimal environment; outer-interpreter isolation was not achieved. Its idle
Terminal was explicitly closed after collection, and root observed its absence
at1791554593812ms while both service terminals remained. No PID/wait status for
that outer interpreter was measured. Future test launches use checked lowercase
shell-wrapper paths with sensitive option spelling inside the file.
