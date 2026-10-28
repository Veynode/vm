# VeynodeVM development

VeynodeVM is a Docker-like runtime for Apple Silicon Macs: each container is a micro-VM on QEMU/HVF (with a virgl or Venus GPU) or Apple Virtualization, managed by a control daemon that also serves MCP to AI agents, with a Linux guest agent, a native console and a web UI. Rust workspace (`crates/`), Go guest agent, Swift/Objective-C native helpers, TypeScript web UI, Python harnesses. Current source establishes actual behavior; [README.md](README.md) and [docs/](docs/) describe intended behavior and what has been verified.

## Start here

1. Find the area below, read only the doc section it names, then the code. Don't read whole long READMEs (`crates/control/README.md` is 800 lines); jump to the heading.
2. Before "fixing" a limit, check [docs/known-issues.md](docs/known-issues.md); before changing a boundary, check [docs/architecture.md](docs/architecture.md) and the [decisions](docs/decisions/).
3. Stop exploring once the entry point, callers and the check are clear.

| Area | Path | Read | Check |
|---|---|---|---|
| `veynodevm` CLI (an API 1 client: flags, output, terminal, launchd) | `crates/cli` | [usage guide](docs/guides/usage.md) | `check-changed.py` |
| MCP HTTPS endpoint (`vm-mcp-http`), tool groups, cua-driver scopes, resource URIs | `crates/mcp` | the "HTTPS endpoint" section of [crates/control/README.md](crates/control/README.md) | `check-changed.py` |
| Local API 1: wire, codes, frames, client | `crates/api` | [docs/api.md](docs/api.md), [decision 0028](docs/decisions/0028-core-split-and-api-1.md) | `check-changed.py` |
| Control daemon, its services (`src/services/`: machines, containers, images, engine, sessions, desktop relay), MCP, QEMU worker (`bin/vm-qemu.rs`) | `crates/control` | the matching section of [crates/control/README.md](crates/control/README.md) | `check-changed.py` |
| OCI registry, blob store, layers, Tart images | `crates/oci` | — | `check-changed.py` |
| Console broker, WebRTC, H.264/HEVC | `crates/media-local`, `browser/` | [crates/media-local/README.md](crates/media-local/README.md) | `check-changed.py` |
| Pairing and signed sessions | `crates/session-auth` | [crates/session-auth/README.md](crates/session-auth/README.md) | `check-changed.py` |
| Linux guest agent (PID 1 in micro-VMs) | `guest/veynode-agent` | [guest/veynode-agent/README.md](guest/veynode-agent/README.md) | `check-changed.py` |
| Web UI (TanStack Start, xterm.js, WebCodecs desktop; `web/daemon.mjs` speaks API 1) | `web/` | [docs/design/DESIGN.md](docs/design/DESIGN.md) (core's design system), then the file it names | `check-changed.py` |
| Apple Virtualization worker, native tools | `native/worker`, `native/tools` | [native/README.md](native/README.md) | `check-changed.py` (builds to a temp dir) |
| QEMU native console (needs the UTM runtime) | `native/console` | [building guide](docs/guides/building.md#gpu-runtime) | `native/console/test.sh` on a staged app |
| Kernel, initramfs, official images | `guest/kernel`, `guest/images` | [guest/images/README.md](guest/images/README.md) | live only |
| GPU runtime (UTM QEMU, virglrenderer, Venus, SPICE) | `runtime/` | [building guide](docs/guides/building.md#gpu-runtime) | live only |

## Rules

- Search with `rg`. `target/` (15 GB), `vendor/`, `.local/` and `node_modules/` are not source: never search or read them in bulk. [.ignore](.ignore) hides `vendor/`, licenses, lockfiles and generated routes from `rg` and Claude Code's search; reach them with `rg --no-ignore-dot` (never `rg -u`, which also walks `target/`). History and evidence from the earlier phase live in the separate, private Veynode/vm-archive repository (paths written `vm-archive/...`); don't cite it as current behavior. `vendor/webrtc-*` and `vendor/rtc-*` are patched copies wired through `[patch.crates-io]`; change them only on purpose.
- Rust: always use the default `target/`. Never pass `--target-dir` or `-j`; a new target dir is a cold build of every dependency.
- Live VMs: don't start VMs or the daemon, and don't run `scripts/check.sh --real` or `scripts/test-*.py`, unless the task needs live evidence or the user asks. Anything that touches runtime state uses a disposable `VEYNODEVM_HOME` or an explicit `--state-dir`, never `~/Library/Application Support/VeynodeVM`.
- Disk: VM disks, images and clones fill the host fast. Check `df -h /System/Volumes/Data` before a large operation (install, image pull, clone, download) and stay above 20 GiB free. Clean up what you created once it's no longer needed (superseded clones, failed machine disks, temporary state dirs, staged builds, screenshots no doc references); keep the ISOs in `.local/iso/` and evidence that docs cite; never remove what you didn't create.
- Processes: manage only processes you started. A running `vm-apple` worker blocks `scripts/build-native.sh` without `--output-app`; never kill it to get a build through.
- Agent capabilities: put an MCP capability on the host if it can live there, else in the Guest OS's own services and APIs; add a Guest helper (like VMware Tools) only for what the OS can't do, invisible to the user (no prompts, windows or manual install), and never put one in a driver; drivers are for devices only. See [architecture](docs/architecture.md#where-agent-capabilities-live).
- Host: Apple Silicon, macOS 14+. Code paths gated on a macOS version or a downloaded runtime (`.local/runtime`) can't be checked without it; say so instead of guessing.
- Git: check `git status` and the diff first; keep unrelated changes; stage only your paths. Commit messages and PR descriptions carry no Claude/Anthropic attribution.

## Verification

`scripts/check-changed.py` reads the files changed against `main` (committed, staged, unstaged and untracked) and runs only the checks they need: `rustfmt` on the changed `.rs` files, clippy and tests for the changed crates (clippy only for crates that depend on them), Go tests for the guest agent, typecheck and tests for `web/` or `browser/`, the matching native build, the Python unit tests that load a changed script, and a link check for changed Markdown. Paths with no offline check are printed, never skipped silently.

- While editing: `scripts/check-changed.py --quick` (Rust tests filtered to the touched modules, no dependent crates).
- Before finishing: `scripts/check-changed.py`, once.
- `--dry-run` shows the plan; pass paths to check something specific; `--base REF` for another base.
- Full gate, `scripts/check.sh` (or `check-changed.py --all`): only when the user asks or the change is workspace-wide (root `Cargo.toml`, `Cargo.lock`, `vendor/`, toolchain). `--real` only when asked.
- Don't rerun a stage that passed unless something it covers changed. Open a test file only where a failure points or when the change alters what it asserts. Never weaken an assertion to pass.
- Report what ran and what kind of evidence it is: static/unit, fixture, or live VM. Unit tests don't prove guest, GPU or desktop behavior.
