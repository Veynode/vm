# Product and release acceptance

Status: engineering preview (phases 0–5 of the reorganization implemented; see README for what is verified). Product direction updated by the user on 2026-10-25; the original 2026-10-08 requirements that still apply are kept under "Inherited requirements".

## Product: VeynodeVM

VeynodeVM is a self-hosted, Docker-like runtime for Apple Silicon Macs. It runs OCI images, and each workload can have **hardware-accelerated graphics**, an optional **guest desktop**, and an **MCP control surface** for external AI agents. GPU access is the main difference from Docker on macOS: containers here get virtio-gpu with virgl (OpenGL/GLES) or Venus (Vulkan → MoltenVK → Metal) through the QEMU/HVF backend.

CLI and product name: `veynodevm`.

### Run modes (both required)

1. **Micro-VM mode** (`veynodevm run IMAGE`): one lightweight VM per container. The image layers are converted once per digest into a read-only root disk; each container gets a private APFS clone as its writable layer; a pinned Veynode kernel and the guest agent as PID 1 start the OCI entrypoint. This mode gives strong isolation, and each container has its own GPU, desktop, MCP observation/input/exec/files and control lease. `--gpus` selects QEMU/HVF; otherwise Apple Virtualization.framework is used, including for `--desktop` (2D virtio-gpu scanout, software rendering).
2. **Shared-VM mode** (`veynodevm run --shared`, or the stock `docker` CLI through `docker context veynodevm`): one long-running engine Linux VM runs containerd/dockerd. Containers share its GPU through a CDI device (`veynodevm.dev/gpu=all`). This mode aims for the broadest OCI and docker CLI compatibility.

Both modes need: registry pull (`linux/arm64`), `-p` port publishing, read-write `-v` volumes, streaming `logs`/`attach`/`exec -it`, `rm` and image garbage collection. If an image has no GPU userspace driver, the product must report that it fell back to software rendering (llvmpipe); it must never fall back silently.

### Guests

The architecture keeps three guest operating systems: Linux ARM, macOS ARM and Windows ARM. **Only Linux is implemented in the current phase.** macOS and Windows stay in the config/capability model and must fail with an explicit unsupported error. Existing macOS guest integration code (`native/guest-macos`, `native/guest-launcher`) is kept but is not on the main path.

### User interfaces

- `veynodevm` CLI (Docker-style verbs) driving the persistent daemon. The daemon is managed by launchd and outlives terminals and UI windows.
- **Web management UI** (`web/`, TanStack Start) served on localhost by `veynodevm ui`. It covers containers/VMs, images, run forms, logs, exec, files and GPU status, and embeds the WebRTC desktop console.
- **AppKit is used only for native desktop control**: the direct console window (`native/console`) with Take Control / Return to Agent. There is no AppKit management list.
- **Guest desktop**: an optional Linux graphical session, shown in the native window or in the browser through hardware H.264/HEVC WebRTC.
- **MCP**: local stdio and authenticated Streamable HTTPS, with container tools (`container_*`, `image_*`) added alongside the existing `vm_*` tools.

## Inherited requirements (2026-10-08, still binding)

- No embedded agent/model/harness. Explicit observe/input/execute/files/lifecycle scopes; destructive actions require separate authorization; unsupported features fail explicitly. Control leases and immediate human takeover.
- Screenshots carry frame/time/coordinates; input is Unicode/layout/Retina-correct where implemented.
- Browser console: prefer HEVC only when host hardware encoding and the browser's WebRTC receive capability intersect; keep H.264 with a bounded retry; record the codec actually received and the fallback reason; never silently use software encoding. Device-key pairing, revocation, media fingerprint binding and end-to-end encryption of sensitive control/clipboard/file/MCP data before any Internet exposure. Guest data stays on the Mac; no default cloud persistence.
- Swift for the native backends and AppKit console; Rust for the platform-neutral control/image/OCI/security/MCP/session service; TypeScript for browser and web UI; guest-specific integrations. Explicit backend capability queries; no assumed GPU/snapshot/balloon parity. Swift types never form the public wire contract.
- Out of scope: anti-cheat, special hardware drivers, x86 OS guests, nested virtualization.
- Performance claims only from agreed, reproducible measurements. No superiority over Parallels or Docker Desktop is claimed.
- Not authorized: public release, purchases, third-party communications, protected-branch pushes and destructive actions.

## Release gates

- Real pull → run → logs → exec → stop → rm in both modes on Linux images; port and volume checks.
- Actual guest GPU evidence: `vulkaninfo` reports the Venus device in micro-VM and shared modes; GL/GLES pixel checks through virgl.
- Desktop: native window and browser WebRTC with input and takeover; window closure does not end the VM or remote session.
- Concurrent stability, crash, disk-full, sleep and upgrade recovery.
- **Redistribution review of the QEMU/virglrenderer/ANGLE/MoltenVK/SPICE runtime** (`scripts/package-reviewed-runtime.py`, `licenses/`). This is currently blocked; development fetches the runtime locally through `runtime/`.
- Security and packaging review.
