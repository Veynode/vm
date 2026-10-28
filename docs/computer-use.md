# Computer use: cua's driver on VeynodeVM's system-level VM

Status: **proposed plan**, not implemented. It follows [where agent capabilities live](architecture.md#where-agent-capabilities-live). The Linux part of phase 0 has run; see [Phase 0 findings (Linux)](#phase-0-findings-linux).

Sources:
- github.com/trycua/cua at `de783ee0a6` (2026-10-26), driver workspace 0.38.0, contract 0.8.0. The plan was written from a static read. Phase 0 then built the driver for Linux arm64 and ran it in live Guests.
- This repo's `main` at `46126aec`, plus the unmerged MCP branches `goofy-moser` (agent view), `elated-meninsky` (events), `upbeat-sanderson` (tool groups) and `elated-faraday` (checkpoints and forks).

## Goal

An agent that drives a VeynodeVM Guest should get cua-driver's computer-use model unchanged:
- window-scoped snapshots with element tokens
- an honest `ActionResult` for every action
- `verify_state`, menus by path, browser tools through CDP, and batching

On top of that it gets what only a VM host can give:
- isolation from the user's own desktop
- GPU
- frame-bound host capture and input that work in every app, including apps without an accessibility tree
- sub-second checkpoints, rollback and forks
- scoped multi-principal access

## What cua is now

cua retired its Python `computer-server`/`cua-agent` stack in October 2026. The current pieces are:

| Piece | Runs | License | Our use |
|---|---|---|---|
| cua-driver (`libs/cua-driver/rust`): core, contract, `platform-linux`/`-windows`/`-macos` | on the OS it drives | MIT | **Embed as the Guest helper**, pinned and vendored like `vendor/webrtc-*` |
| Contract 0.8.0 (`cua-driver-contract`, `contract/manifest.json`, `recovery_hint.rs`) | — | MIT | **Adopt as our public computer-use contract** |
| cua-spacesd: in-guest daemon, `cua.env.v1` gRPC, `/mcp`, media | Guest | FSL-1.1-MIT | **Design only.** Our daemon and Guest transports fill this role |
| `cua` SDK/VMM: gVisor, QEMU/KVM, Lume, kubevirt | host | MIT | Reference; Linux hosts later |
| cua-bench (OSWorld-Verified, WinArena, ScreenSpot-Pro) | host | MIT, with OSWorld Apache-2.0 | **Use for evals** |
| cua-perception, cua-som (OmniParser) | — | AGPL | **Not used** |

## Architecture

```
agent ──MCP──▶ vm-control (host plane)
   │              • lifecycle, checkpoints/forks, events, scopes, recording
   │              • frame-bound capture + virtual-HID input (every Guest, no helper)
   │              • OCR, screen_wait
   │              └─ per-VM driver bridge ──Guest transport──▶ cua-driver (Guest semantic plane)
   │                                                            • AT-SPI / UIA / AX, CDP
   └── per-VM endpoint /vm/{id}/mcp: cua-driver tool names, unchanged
```

### Host plane (exists; extended)

The daemon keeps what it does today, so every Guest works with no helper:
- screenshots from the console (SPICE, VZ agent, Screen Sharing)
- virtual-HID input
- OCR, events, checkpoints and forks, auth and scopes

What changes is that host actions adopt the driver's result shape, so agents see one model whether or not the helper exists:
- Host input returns an `ActionResult` with a new `route: "virtual_hid"`.
- Its evidence comes from the screen change (`window_change`) or from OCR text (`value_readback` when the text matches).
- A helper-less `get_window_state` with `target:{kind:"desktop"}` returns the screenshot plus OCR text boxes as elements with tokens. The elements carry `in_web_content`-style trust marks, so `verify_state` returns `unknown` with `untrusted_source` rather than guessing.

### Guest semantic plane (new)

cua-driver runs as a **stdio MCP child process** in the console user's session. The alternative, linking `cua-driver-core` in-process, would pull about 300 crates into an agent we write in Go. Linux, Windows and macOS all support the stdio form; on macOS it uses `CUA_DRIVER_EMBEDDED=1` so it inherits its parent's TCC grants.

| Guest | Spawned by | Host transport | Notes |
|---|---|---|---|
| Linux on Apple Virtualization | Go agent, as the desktop user (`DISPLAY` set; D-Bus and XAUTHORITY discovered by the driver) | vsock 5000, new agent method `driver_rpc` carrying JSON-RPC frames (64 KiB per request, 1 MiB per reply today) | The a11y helper subprocess is the existing pattern |
| Linux on QEMU/HVF | Go agent | **a second, dedicated virtio-serial port** `veynode.driver`, so the fenced serial-v2 exec protocol stays unchanged; there is no vsock on HVF | Also lifts the daemon's refusal of desktop tools on QEMU |
| Windows | interactive-logon scheduled task (`cua-driver autostart` pattern) serving `\\.\pipe\cua-driver` | one persistent SSH connection per VM running `cua-driver mcp --socket`, which avoids 1–2 s of SSH/PowerShell startup per call | Session 0 is refused; open issue #4957 rejects an admin SSH session, so the bridge may need a non-admin account |
| macOS container | our signed `vm-macos-desktop`, which already holds the Accessibility grant | persistent SSH | Screen Recording can't be pre-granted by MDM; first boot already grants it through Screen Sharing |

**Invisible-helper rules:**
- The image (Linux), the answer file (Windows) or first boot (macOS) installs the driver. Nothing is installed by hand.
- No window, tray icon, prompt or console flash. The driver's agent-cursor overlay stays off unless a person is watching.
- Linux needs `libx11-6 libxi6 libxext6 libxkbcommon0` (the build links no libXtst or libwayland), `at-spi2-core`, and `/dev/uinput` with a udev rule plus group `input`. Without uinput only foreground delivery works, which is fine in a VM. A browser also needs `chromium-sandbox`. The full list is in [phase 0](#what-a-linux-image-needs).
- The driver starts with `--no-overlay` and `DO_NOT_TRACK=1 CUA_DRIVER_RS_UPDATE_CHECK=0`. The cursor overlay is on by default, and the binary contains a PostHog telemetry sender.
- The driver runs as the desktop user, never root. `ubuntu-desktop` runs its XFCE session as root today, so the image must move the session to a user. It is pinned to an official release, which cua's release job builds from the tagged commit and attests with sigstore. [`runtime/cua-driver.json`](../runtime/cua-driver.json) records the tag, commit and the archive and binary checksums per platform; the image and `runtime/fetch-cua-driver.sh` check them, and the pin is updated on purpose.
- Idle CPU and memory are measured in phase 0 and are part of acceptance.

### Two MCP faces

1. **The existing VM tools** (`vm_*`, `container_*`) remain the VM-level face: lifecycle, checkpoints, host capture and input, events. The `goofy-moser` agent-view work maps onto the contract:
   - The compact tree becomes `get_window_state`'s `tree_markdown` and tokens.
   - `observe` deltas follow the `since:"latest"` `+ ~ -` rules, and `invalidated_snapshot_ids` and `stale_element_token` are adopted.
   - `vm_input_batch` aligns with `run_steps` (64 steps, `expect` checks, one `observe`).
   - Frame re-anchoring stays; it is stricter than the driver's `screenshot_context`.
2. **A per-VM cua-compatible face.** It is served both as a `cua` tool group on the main MCP (with an added `vm` argument) and as an MCP endpoint per VM (`/vm/{id}/mcp`, on stdio through `vm-control mcp --vm ID`) that carries cua-driver's own tool names and schemas unchanged. An agent or skill written for cua-driver then works against a VeynodeVM Guest without change.

**Scope mapping**, enforced by the daemon before the driver sees a call (the driver runs its own `bounded` profile as a second fence):

| Scope | Tools |
|---|---|
| observe | `list_apps`, `list_windows`, `get_window_state`, `get_accessibility_tree`, `get_desktop_state`, `zoom`, `verify_state`, `get_browser_state` |
| input | the click, type, key, scroll and drag tools, `set_value`, `invoke_menu`, `set_window_frame`, `run_steps` |
| browser (new) | `browser_*` tools that mutate |
| clipboard (new) | `clipboard_read`/`clipboard_write` (privacy) |
| execute | `run_script`, `launch_app`, `kill_app` |
| never exposed | `install_extension`, `check_for_update`, `install_ffmpeg`, `set_config` |

### Delivery policy in a VM

cua-driver works hard to stay background-only, because it shares the user's desktop. In a VM the agent owns the desktop, so:
- **No person attached:** prefer the semantic route (accessibility or DOM), then `foreground`, then host `virtual_hid`. Escalation stays explicit, as in cua (`escalation` plus `next_call`), but our `next_call` may name the host route.
- **A person attached** (native console or browser desktop session, which the daemon already knows): agent input pauses behind a **control lease**, and the person's input always wins. This is new, because the architecture has no leases today. The agent cursor overlay is shown while the agent holds the lease.

### Combined with VM capabilities

- **`verify_state` plus rollback:** an `unsatisfied` result can end in `container_rollback`. Warm rollback takes 0.4–0.6 s for a 512 MiB Guest.
- **Forks for parallel attempts:** run up to 16 copies from one checkpoint (about 1 s for 3), each driven independently.
- **Events instead of polling:** screen `changed`/`settled` and job events from `elated-meninsky` drive `wait_for` without loops.
- **GPU desktops:** browsers, Electron and 3D apps render on Venus/Zink or Neptune, so the driver sees real apps at full speed.
- **Recording:** the host records every call in the driver's `turn-NNNNN/` layout (action, before/after state and png, evidence), so it also captures helper-less host actions. It exports ATIF-v1.8 for cua-bench and training.

## Evals

Use cua-bench in client-only mode against our VMs:
- **The legacy `/cmd` adapter:** a small host-side shim on our MCP.
- **OSWorld's Flask server on :5000**, plus 9222 (CDP) and 8080 (VLC), in the Guest.

Reset between tasks is a warm rollback to a checkpoint, instead of cua's fresh sandbox.

Gaps:
- Upstream's OSWorld image is amd64 only, and amd64 on Apple Silicon is TCG, which is unusable. We build an **arm64 Ubuntu 22.04 OSWorld Guest**. Chromium stands in for Google Chrome, which has no Linux arm64 build; tasks that depend on Chrome itself are reported separately.
- WinArena runs on our Windows 11 ARM64 Guest later.

Report pass@k, steps, tokens and wall time beside cua's published numbers, and say which tasks differ in fidelity.

## Gaps found on main that this plan closes

- **QEMU desktops:** desktop and a11y tools are refused by the daemon (`crates/control/src/manager.rs`), and QEMU has only the serial transport. Fixed by the driver port.
- **Windows:** no screenshots or input. Check the console-broker path, then add the driver.
- **Clipboard:** none anywhere. Fixed by the driver's clipboard tools, under the new `clipboard` scope.
- **Resolution:** fixed everywhere. Add `set_resolution` (VZ display reconfiguration; QEMU virtio-gpu EDID plus SPICE resize).
- **Container init flags:** container-init omits `--allow-text-input` (QEMU) and `--allow-files` (Apple Linux), so `vm_input_text` and `vm_file_*` are likely unavailable in containers. Fix the flags or document the omission.

## Phases

| Phase | Work | Done when |
|---|---|---|
| 0. Spike | Build pinned cua-driver for `aarch64-unknown-linux-gnu` and `aarch64-pc-windows-msvc`. Run it by hand in `ubuntu-desktop` (VZ and QEMU/Venus) and on win11v | We know binary size, idle CPU and memory, which tools work per Guest, whether known bugs bite (#4874 tree degrades on X11, #4754 UTF-8 typing, #4906 slow Windows `list_windows`), and that it runs with nothing visible |
| 1. Linux bridge | Agent `driver_rpc` over vsock; dedicated QEMU virtio-serial port; daemon bridge; `cua` group and per-VM endpoint; scope mapping; image packaging | `get_window_state` → `click` → `verify_state` in an XFCE terminal and in Chromium, on both backends, with recorded evidence |
| 2. Windows and macOS bridge | Persistent SSH bridge, interactive-task autostart from the answer file; macOS embedded spawn | The same flow in Notepad/Explorer and in TextEdit/Finder |
| 3. One contract | `ActionResult` on host tools (`virtual_hid` route), OCR elements for helper-less Guests, align the `goofy-moser` tools, control lease | Every input tool returns a valid `ActionResult`; the contract invariants are unit tested |
| 4. Evals | arm64 OSWorld image, cua-bench client adapter, checkpoint reset | A full OSWorld-Verified run with a published comparison |
| 5. Recording and gaps | `turn-NNNNN` recording, ATIF export, clipboard scope, `set_resolution`, init flags | Recorded runs replay; gaps above closed |

Phases 0 and 1 also merge the four MCP branches first, because the bridge builds on tool groups, events and the agent view.

## Progress (phases 1–3)

Implemented with static, unit and fixture evidence, and run live on Linux, Windows and macOS (below).
The decision is [0024](decisions/0024-computer-use-cua-driver-and-control-lease.md).

**Guest half (phase 1):**
- **Pin:** cua-driver 0.38.0 (`85e2b1318b`), one official release for Linux, Windows and macOS (`runtime/cua-driver.json`;
  `runtime/fetch-cua-driver.sh` fetches and checks a platform). Telemetry stays off through the environment, which every
  start sets.
- **Supervisor** (`guest/veynode-agent/driver.go`): started on the first call as the desktop session's user with the
  session's groups and environment, never as root; outside every command cgroup; restarted after an exit (at most 5 starts
  a minute). It owns the MCP handshake and rewrites request ids, so the host sends only `tools/*`, `resources/*` and `ping`.
- **Contract with the host:**
  - Apple Virtualization: agent method `driver_rpc {frame, deadline_ms}` on vsock 5000, through the worker's
    `guest_request`; the reply is `{frame}`. This method alone takes up to 1 MiB per request and a deadline up to 10
    minutes; replies are at most 1 MiB.
  - QEMU: newline JSON-RPC frames, without an envelope, on the virtio port `veynode.driver`; failures are JSON-RPC errors
    with `data.veynode_code`.
  - Error codes: `unsupported`, `deadline_exceeded`, `driver_exited`, `driver_unavailable`, `reply_too_large`,
    `invalid_request`, `busy`.
- **Image** (`ubuntu-desktop`): XFCE runs as `veynode` (uid 1000, groups `input video render`); `at-spi2-core`, the
  uinput udev rule and the pinned driver are installed. Chromium stays out of the base image.
- **Fixed on the way:** the `virtio_balloon`/`virtio_rng` blacklist is written for every container, and
  `veynodevm exec -u/-e/-w` now applies without `-i`/`-t`.

**Host half (phase 1):** the worker limits, the QEMU driver port, the bridge with its scope mapping and policies, the
`cua` group and the per-VM endpoint. See [cua-driver tools](../crates/control/README.md#cua-driver-tools).

**Windows and macOS (phase 2):** a persistent link per VM (`driver_link`): on Windows the driver's own `mcp --socket`
proxy over one SSH session, after `guest/windows/cua.ps1` installed the pinned binary and a hidden logon task serving
`\\.\pipe\cua-driver`; in macOS containers the embedded driver over SSH. Both install on first use and need no step
from the user beyond a logged-on Windows console. Not done yet: routing the Windows `vm_desktop_*`/`vm_accessibility_*`
tools through the driver (they still use `desktop.ps1`), and the XAML keyboard policy (host input for keys without an
element token).

**One contract (phase 3):**
- `ActionResult` on host input (`action_result.rs`): route `virtual_hid`, effect from the observation only. Contract
  invariants are unit tested (`confirmed` needs evidence, `refused` says why, an unconfirmed effect names what to do next).
- OCR rows for Guests without the driver (`cua_get_window_state`), untrusted.
- Agent view: the compact rows and `+ ~ -` deltas were already the driver's shape; `vm_input_batch` keeps its 50 steps
  and `wait` steps, and takes OCR tokens as click refs. Not done: `invalidated_snapshot_ids` and `stale_element_token`
  for the host's own trees, and routing `vm_window*`/`vm_accessibility_*` on QEMU Linux through the driver (QEMU still
  refuses them; the `cua_*` tools are the path there).
- Control lease (`control_lease.rs`), `vm_control_lease`, and `control` events. The Apple Virtualization console window
  reports the person's input; the QEMU console and browser desktops do not yet.

### Live acceptance (2026-10-26)

`ubuntu-desktop` built with the pinned driver and run from a disposable `VEYNODEVM_HOME` by a foreground daemon; evidence
in `.local/evidence/computer-use-live/` (`*.jsonl` frames, `*.txt` summaries). Live VM evidence for Linux only.

| Check | Apple Virtualization | QEMU/Venus |
|---|---|---|
| Session: Xorg root, XFCE and the driver as `veynode`, no oops | passed | passed |
| `cua_launch_app` → `list_windows` → `get_window_state` (tokens) → `type_text` → `verify_state` in xfce4-terminal | passed; the typed command ran | passed; the typed command ran |
| `set_window_frame`, `set_value` | confirmed by readback | not run |
| Chromium: `browser_prepare` (isolated) → `navigate` → `browser_type` `Zoë 世界` → `browser_click` → page reads `clicked:Zoë 世界` | passed | not run |
| Per-VM endpoint: the driver's own schemas (47 tools, 160 KB at observe+input), refusals before the driver | — | passed |
| Driver network use: one loopback socket, the CDP connection to its own Chromium | passed | — |
| `vm_input_click` `ActionResult` (`virtual_hid`, `unverifiable` with `next_call`) | — | passed |
| OCR fallback with the driver removed: `ocr1:N` rows `trusted:false`; token click refused with the pixel `next_call`; `verify_state` unknown; the driver back on the next call once restored | — | passed |
| `exec -u veynode id` with supplementary groups, `-w`, `-e` | — | passed after the fixes below |

Found and fixed during the run:
- **QEMU killed the VM on `exec -u/-e/-w`.** The serial v2 preflight allowed only `argv` and `timeout_ms` on
  `exec_start`, so the identity fields were a protocol violation that fenced the incarnation. It now takes them.
- **QEMU killed the VM on `vm_input_text` in any container.** The host's `info` contract required a rootless service,
  while an OCI micro-VM runs as its container root (which the Guest admits since `a1bf1521`). The host now accepts either.
  Text input itself is then refused honestly: container-init does not start the service with `--allow-text-input`, so
  on QEMU desktop containers `cua_type_text` is the way to type.
- A fenced incarnation now writes its reason to the worker's stderr; it was discarded.
- `exec -u` set only the primary group; it now adds the user's `/etc/group` memberships, like `docker exec -u`.
- The curated `cua_browser_prepare` schema took `strategy` as a string and had no `profile`.
- `cua_verify_state` on a Guest without the driver was an error; it is now `unknown` with `driver_unavailable`.

Findings that stand:
- **Non-ASCII through an element token is lost even in the foreground.** `type_text` with `element_token` and
  `Zoë 世界` typed `Zoë ` (the bridge's foreground policy applied), and `verify_state` correctly reported unsatisfied.
  Without a token the foreground route is exact; `set_value` and `browser_type` are exact. Upstream #4754 reaches the token
  path; the policy should send token plus non-ASCII to `set_value`.
- **Attaching to the user's Chromium profile** (`browser_prepare` `existing_profile`) is refused with
  `browser_consent_required` unless the driver runs with `--grant existing-profile`; the bridge does not grant it. A
  driver-owned isolated browser (`allow_launch`, `profile.mode: isolated_new`) needs no grant and ends with the session.
- The QEMU console window registers no lease viewer, so only the Apple Virtualization window pauses the agent; the lease
  read `holder: agent` with the QEMU window open. Pausing on a person's typing was not run live (it needs real input in the
  console window); the unit tests cover it.
- The first host OCR call after `vm-vision` is built takes about 17 s (Vision's first run); later calls take 0.4 s.
- `veynodevm daemon start` with a custom `VEYNODEVM_HOME` reuses the fixed launchd label, so it replaces the default
  daemon's agent and points it at that home.

**Windows and macOS** (same day, APFS clones of the `win11v` machine and of the `macos-tahoe-vanilla` image in a
disposable home, so neither original was touched):

| Check | Windows 11 ARM (win11v, QEMU) | macOS Tahoe container |
|---|---|---|
| Install on the first call, from nothing | passed: 10.4 s (copy, telemetry off, hidden logon task, start); later calls 16 ms over the link | passed: about 1 s (copy to `~/.veynodevm`, embedded start) |
| Invisible | passed: daemon in the console session and SSH proxy in session 0, no window handle, task `Hidden`, nothing on the desktop screenshot | passed: one process as `admin`, no window |
| Network | no TCP or UDP endpoint | no socket |
| Tree with tokens | Notepad (32 elements), Explorer | TextEdit, Finder |
| Typing `Zoë 世界` into an element | `confirmed` by UIA value readback, exact | delivered complete through the foreground policy; TextEdit auto-capitalized the first letter and `verify_state` reported `unsatisfied` |
| Element actions | Notepad's Add New Tab invoked; selecting an Explorer tree item `unverifiable` (it selects, it does not navigate) | `invoke_menu` File > New; a Finder sidebar click `confirmed` as the row selection it was, without navigating |
| `verify_state` | window and element predicates satisfied; a Document's `value_equals` is `unknown` (`untrusted_source`) | window predicates satisfied; elements of an incomplete tree are `unknown` (`observation_unavailable`) |

Found and fixed:
- **Windows install could never start.** `cua.ps1` queried the not-yet-registered task with stderr redirected, which
  Windows PowerShell turns into a terminating error under `Stop`. `schtasks` now runs through one helper that trusts only
  its exit code.
- **Windows OpenSSH refused the script** ("exec request failed on channel 0") once it passed about 17 KB as UTF-16
  base64. Every SSH PowerShell script now travels gzipped (cua.ps1: 18 KB → 4 KB).
- **The macOS driver exited at once**: embedded, `mcp` requires a host-provided service socket. It now runs
  `mcp --direct`, owning its runtime in the SSH session that holds the Accessibility grant.
- The curated `cua_launch_app` offers `bundle_id`, which macOS needs (it has no `launch_path`).

Limits seen on macOS: the driver's own screenshots fail, because the SSH session has no Screen Recording grant (host
screenshots go through Screen Sharing, as before); `launch_app` with a folder URL times out handing the folder to Finder.

Not run: a console with real person input (the lease pause); Chromium on QEMU and Windows; Edge on Windows.

## Phase 0 findings (Linux)

Run on 2026-10-26 on an M3 Pro (11 cores, macOS 27). VeynodeVM was built from `main` at `d78b5843` with a private `VEYNODEVM_HOME` and its own boot assets. No product code changed.

**Kinds of evidence:**
- **Live:** one run per backend unless a row says otherwise. The backends are Apple Virtualization (`--desktop`, llvmpipe) and QEMU/HVF (`--gpus venus`, GL on virgl, Vulkan on Venus).
- **Static:** reading the cua source or the binary.

Raw logs, the harness and the spike image are in `.local/evidence/cua-spike-linux/` of the main checkout (not tracked).

**Setup:**
- The Guest image was `veynodevm/ubuntu-desktop`, built from `guest/images/ubuntu-desktop`, with one spike layer on top:
  - `at-spi2-core`
  - a user `veynode` (uid 1000, groups `input video render`) that runs the XFCE session
  - the uinput udev rule
  - Mousepad (GTK3), FeatherPad (Qt5) and deb Chromium 156 from the xtradeb PPA
- The driver ran as `veynode` through `veynodevm exec -i CONTAINER runuser -u veynode -- cua-driver mcp --no-overlay`. It had the session's `DISPLAY`, `DBUS_SESSION_BUS_ADDRESS` and `XDG_RUNTIME_DIR`.
- A host-side harness spoke newline-delimited JSON-RPC to it and logged every frame's size and latency. On QEMU, `exec -i` runs over the serial-v2 exec channel, so the QEMU run also exercised a serial transport.

### Binary

- **Build (live):** built from source with `cargo build --locked -p cua-driver --release --features cua-driver/portal-input --target aarch64-unknown-linux-gnu`, in `debian:11` arm64. This is the same base, flags and Rust 1.97.1 as cua's release job.
  - The build took about 4 minutes on 11 cores.
  - The sparse checkout also needs `libs/cua/crates/cua-telemetry`, `libs/cua/crates/cua-teleport/src/ux` and `libs/images`, because the telemetry crate embeds two JSON files at build time.
  - No release download was needed.
- **Size and sha256:** 56.6 MB as built, 40.6 MB stripped, 16.1 MB with `gzip -9`. The sha256 is `546c051c…f7d`; the full hash is in the evidence directory.
- **Dynamic libraries (`ldd`):** libX11, libXi, libxkbcommon, libXext, libxcb (with libXau, libXdmcp, libbsd, libmd) and glibc 2.31 or newer.
  - It does not link libXtst or libwayland-client: XTEST and Wayland go through pure-Rust protocol crates.
  - It names libgtk-3 and libgtk-4 as strings, apparently for an optional `dlopen`.
- **Telemetry (static and live):** the binary contains the PostHog sender (`eu.i.posthog.com`), even though `cua-driver` depends on `cua-telemetry` with `default-features = false`. Telemetry defaults to on. With `DO_NOT_TRACK=1 CUA_TELEMETRY=0 CUA_DRIVER_RS_UPDATE_CHECK=0`, the running driver held only Unix sockets: no TCP or UDP. The driver was not run without these variables.

### Tool matrix

`tools/list` returns 66 tools. Cells give the result and its latency in ms (VZ / QEMU). The route and effect are as the driver reported them.

| Tool | Case | VZ | QEMU/Venus | Notes |
|---|---|---|---|---|
| `list_apps` | all | degraded 10 | degraded 13 | Lists every process in the Guest, kernel threads included (107 rows, 30 KB), because the container sees the agent's PID space |
| `list_windows` | all | works 4 | works 5 | Correct bounds and z-order |
| `get_desktop_state` | 1280×800 screenshot and windows | works 15 | works 42 | `agent_overlay_capture: not_present` |
| `get_window_state` | Mousepad, markdown | works 45 | works 93 | 19 elements, menus with item counts. **#4874 did not reproduce**: GTK3, Qt5 and Chromium trees were complete |
| `get_window_state` | xfce4-terminal | works 40 | works 75 | 20 elements. **Degraded:** the VTE terminal's text is not in the tree, so host OCR is the readback |
| `get_window_state` | FeatherPad (Qt5) | works 185 | works 378 | 21 elements. **#4894 (Qt hang) did not reproduce** |
| `get_window_state` | `elements`, `both`, with screenshot | works 47 | works 81 | PNG of the window; `since:"latest"` diffs (`+ ~ -`) and `invalidated_snapshot_ids` behave as documented |
| `click` | `element_token` (GTK3 and Qt) | works 335 | works 371 | Route `accessibility`, effect `unverifiable`; the next read shows the change. It also dismissed a Mousepad modal dialog |
| `type_text` | ASCII, by token, background | works 7 | works 13 | AT-SPI; effect `confirmed` by value readback |
| `type_text` | non-ASCII, by token, background | **fails** 6 | **fails** 9 | **#4754 reproduces.** The AT-SPI insert keeps as many bytes as the text has characters: `héllo 世界 ✓ naïve` (16 characters) reads back `héllo 世界 ` (16 bytes). The driver reports `unverifiable` and says the readback differs |
| `type_text` | non-ASCII, `delivery_mode:"foreground"` | works ~1050 (3 of 3, fresh container) | works 1426 | Real key events are exact (`kb ✓ naïve 世界`). See the foreground row below |
| `type_text` / `press_key` | terminal by token, then `return` | works 1263 / 304 | works 1300 / 314 | Route `global_input`; the command ran |
| `hotkey` / `press_key` | `ctrl+a`, `backspace` in a field | works 266 / 234 | works 488 / 303 | Route `global_input` (virtual master keyboard) |
| `scroll` | by `element_token` | works | works | The vertical scrollbar went from 0 to 764 (VZ) and to 528 (QEMU) |
| `scroll` | by window x,y | degraded 502 | works 1463 | On VZ the point was covered by the terminal, so the background pointer scrolled the window on top |
| `set_value` | 300-line document; empty a field | works 7 | works 12 | Effect `confirmed` by readback |
| `invoke_menu` | Mousepad `File > New` | works 743 | works 1256 | A tab was added |
| `verify_state` | element exists; window bounds | works 234 / 110 | works 330 / 114 | `satisfied after 2 sample(s)` |
| `run_steps` | Qt `type_text` with `expect` and `observe` | works 542 | works 1147 | An `expect` without `role`, `name` or `text` is refused before anything runs (`invalid_batch`) |
| `clipboard_write` / `clipboard_read` | `clip ✓ 世界` | works 56 / 161 | works 63 / 108 | Exact round trip |
| `set_window_frame` | FeatherPad to 600×400 | works 8 | works 22 | Route `system_api`, effect `confirmed` |
| `launch_app`, `bring_to_front` | FeatherPad, Mousepad | works | — | The launched app is the driver's child, so it dies with the driver |
| foreground delivery | any tool with `delivery_mode:"foreground"` | **degraded** | works | When xfwm4 publishes no `_NET_ACTIVE_WINDOW`, the driver refuses with `foreground_unavailable` and sends nothing; `xdotool windowactivate` could not set one either. That happened on VZ after the session's apps were killed and restarted; a fresh VZ boot worked 3 of 3 times. The WM is the cause, not the backend |
| `browser_prepare` | `isolated_new`, `allow_launch` | works 605 | works 1603 | Without `chromium-sandbox` it **fails** with `browser_route_unavailable` (Chromium exits). The container root is a chroot, so the kernel refuses unprivileged user namespaces (`unshare -U` gets EPERM) |
| `get_browser_state` | bind; `dom_refs_v1`; `semantic_v2` | works 10 / 6 / 6 | works 11 / 10 / 14 | Exact CDP binding to the native window |
| `browser_navigate` | local page; Wikipedia | works 26 / 776 | works 67 / 702 | |
| `browser_type` | `Zoë 世界` | works 22 | works 30 | CDP `insertText` is exact, so it works around #4754 inside browsers |
| `browser_click` | default (trusted, background) | refused 9 | refused 16 | `browser_input_trust_unavailable` by design on Linux: trusted CDP input would activate the window |
| `browser_click` | `foreground` or `input_route:"dom_event"` | works 44 | works 447 | The page showed `clicked:Zoë 世界` |

Not tried: `drag`, `zoom`, recording, `run_script`, Wayland, and the uinput background pointer under load. The uinput device node and group were in place, but nothing proved which pointer route each click took beyond the reported `route`.

**The VZ desktop crash:**
- In the first VZ run, Xorg died mid-session with a guest kernel oops ("Undefined instruction" in `drm_atomic_check_only`, from a DIRTYFB ioctl).
- `virtio_balloon` and `virtio_rng` were loaded at the time. The desktop entrypoint's `udevadm trigger` loads them.
- These are the two modules [known issues](known-issues.md) blames for guest memory corruption on Apple Virtualization. The agent writes the blacklist only for `--init systemd` containers.
- With the same blacklist in the container's `/etc/modprobe.d`, the later VZ runs showed no oops, including foreground typing three times.
- This is a product bug in the desktop images, independent of the driver.

### Numbers

| | VZ | QEMU/Venus |
|---|---|---|
| Spawn to first `tools/list`, inside the Guest (5 runs) | 100–134 ms | 108–114 ms |
| The same through `veynodevm exec -i` from the host | 0.34 s | 0.34 s |
| RSS when fresh | 27 MB | 27 MB |
| RSS after the desktop matrix | 48 MB, 11 threads | — |
| RSS after Chromium/Wikipedia reads | — | 268 MB; it plateaus and does not grow over 6 more reads |
| Idle CPU over 60 s with no calls | 0 ticks (under 0.02 % of one core) | 0 ticks |
| `get_window_state`, Mousepad with 300 lines (5 runs) | 53–83 ms | 100–112 ms |
| `get_window_state`, Chromium on Wikipedia "Linux", default 250 elements | 545–564 ms, 71 KB | 1.03–1.19 s, 64–71 KB |
| The same with `max_elements: 2000` | 1.03 s, 479 elements, 157 KB | — |
| `get_browser_state` `semantic_v2`, Wikipedia | 4.85 s, 53 KB | 5.0 s, 53 KB |

**Token cost of `tree_markdown`** (characters ÷ 4, an estimate):
- A typical small window costs 370–560 tokens: Mousepad 2,244 characters, the terminal 1,468, FeatherPad 1,835.
- A text widget's content appears twice (label and `value`), so a 300-line document costs about 2,000 tokens.
- Chromium on Wikipedia costs about 8,200 tokens at the default cap and 18,500 at 2,000 elements.

### Transport sizes

Measured over 300 calls; the full table is `vz-frames.txt` and `qm-frames.txt`.
- The largest request was 3 KB: `set_value` with 300 lines. Every other request was under 350 B.
- `tools/list` is 230 KB.

The largest replies:

| Reply | Size |
|---|---|
| `get_window_state`, 2,000 elements | 157 KB |
| `get_desktop_state`, 1280×800 PNG | 118–157 KB |
| `get_window_state` with screenshot | up to 119 KB |
| `get_browser_state` `semantic_v2` | 53 KB |

The slowest calls: a foreground timeout took 12.5 s, `get_browser_state` took 5 s, and `verify_state` can be told to wait 10 s.

Against today's vsock limits:
- **1 MiB per reply:** every observed reply fits, `tools/list` included. A desktop screenshot is scaled to at most 1,568 px (`max_image_dimension`), which keeps it in the low hundreds of KB.
- **64 KiB per request:** typical calls fit. But `type_text`, `set_value` and `clipboard_write` carry user text, and above 64 KiB they would be refused by our transport rather than by the driver. The bridge needs a larger request limit for `driver_rpc` (1 MiB, like replies), or it must refuse with the driver's own error shape.
- **35 s per call:** this fits single calls. A `run_steps` batch can exceed it (64 steps, up to 2 s of delay each, plus `wait_for` and `expect` timeouts). The bridge should derive the deadline from the call, or cap `run_steps` and say so.

On QEMU, a full driver session ran over `exec -i` on the serial-v2 channel: 230 KB `tools/list` frames and screenshots, with no fence. The dedicated `veynode.driver` port is still the right design:
- It keeps driver traffic out of the fenced exec protocol and its heartbeat.
- That heartbeat ([decision 0015](decisions/0015-qemu-serial-execution-v2.md), 500 ms with a 1 s budget) is fenced, and QEMU is killed, when a long stall on QEMU's main thread delays it. The parallel Zink session saw this during virgl shader compiles. The driver channel must not share that failure mode.

### What a Linux image needs

To make the driver invisible and automatic in `ubuntu-desktop`:
- **Packages:** `at-spi2-core` and `chromium-sandbox` (with deb Chromium, not snap). The X libraries the binary links are already in the image; `libxtst6` and `libwayland-client0` are not needed by this build.
- **Session:** XFCE runs as a desktop user (uid 1000, groups `input video render`) instead of root. Xorg stays root, as today. `at-spi-bus-launcher` starts from XFCE's autostart and publishes `AT_SPI_BUS` on the root window, which the driver found without extra environment.
- **uinput:** the rule `KERNEL=="uinput", GROUP="input", MODE="0660"` gave `/dev/uinput` mode `root:input 0660` on both backends. `uinput.ko` is in the modloop.
- **Agent spawn:** the Go agent starts `/usr/local/bin/cua-driver mcp --no-overlay` as that user once the session is up. It passes:
  - the session's `DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`, `XDG_RUNTIME_DIR` and `HOME`, read from the `xfce4-session` process
  - `DO_NOT_TRACK=1 CUA_TELEMETRY=0 CUA_DRIVER_RS_UPDATE_CHECK=0`

  It keeps the driver outside per-exec cgroup kill, so `exec` timeouts don't take the driver and its launched apps with them. It restarts the driver if it exits.
- **Visible footprint:** none from the driver. There was no window owned by its pid, no overlay or notification, and the screenshot showed nothing extra. The isolated Chromium window that `browser_prepare` launches is visible, as any app the agent opens would be.
- **Separate fix:** write the `virtio_balloon`/`virtio_rng` blacklist for every Apple Virtualization container that runs udev, not only for systemd ones.

### What changes in the plan

- **Phase 0 (Linux) is done.**
  - Idle cost is zero CPU and 27–50 MB, rising to about 270 MB after heavy browser reads.
  - The semantic tools work on both backends.
  - No blocker was found for phase 1.
- **#4754 is real on the AT-SPI typing route.** Until upstream fixes it, the bridge should:
  - send non-ASCII `type_text` with `delivery_mode:"foreground"` when no person is attached, which matches the delivery policy above
  - or use `set_value`, or `browser_type` in browsers
  - otherwise rely on the driver's `unverifiable` readback rather than reporting success
- **Foreground and trusted browser input depend on the window manager's active-window state.** The host `virtual_hid` route stays the last fallback, as planned. With no person attached, the bridge should pass `delivery_mode:"foreground"` to `browser_click`, because the background trusted route is refused on Linux.
- **`list_apps` should be filtered by the bridge** to the desktop user's processes, or left out of the `cua` group. Today it returns the whole Guest's process table.
- **Transport:** raise the `driver_rpc` request limit and derive per-call deadlines, as described under transport sizes.
- **Other follow-ups found on the way:**
  - `veynodevm exec -u USER` ran as root in a Linux container.
  - The desktop images run XFCE as root.
  - The VZ module blacklist covers systemd containers only.

## Decisions needed

1. **Embed the cua-driver binary** (recommended) or reimplement its contract ourselves. Embedding gets Windows UIA, menus, CDP and `verify_state` now, at the cost of tracking a fast-moving upstream and its open bugs.
2. **Expose the per-VM cua-compatible endpoint** next to `vm_*` (recommended), or only the `cua` group.
3. **`cua.env.v1` compatibility**, so cua's SDK talks to our VMs directly. It needs a license check, because the protos have no separate license file and spacesd itself is FSL. Deferred until that check.
4. **Control leases and pausing the agent when a person attaches**: this changes today's "no leases" boundary in [architecture](architecture.md).
5. **The arm64 OSWorld image**: building and hosting it (size, Chromium substitution).

## Phase 0 findings (Windows)

Run on 2026-10-26 against win11v (Windows 11 Pro 26300 ARM64, QEMU/HVF, 1024x768 `virtio-ramfb`, no GPU profile), by hand over SSH. Evidence is live-VM, n=1, in `.local/evidence/cua-spike-windows/` (`calls.jsonl` has every MCP request and reply). The driver was removed from the Guest afterwards.

**Binary.**
- The official `cua-driver-rs-0.38.0-windows-arm64.zip` (SHA-256 `c442872360cbb819e288b9f30daa7e3d2693d5945495d1400bcd90af5bd16402`, matching the release's `SHA256SUMS` and GitHub's asset digest).
- The release was built from `85e2b1318b`, not `de783ee0a6` (the commit this plan read).
- `cua-driver.exe` is 32.6 MB. The zip also ships `cua-driver-uia.exe` (22.0 MB), `cua_driver_sdk.dll` (26.5 MB), a Node addon and `cua-cursor-theme.exe`: 84 MB unpacked. Only `cua-driver.exe` ran in this spike.
- No source build was tried.

**Telemetry is on by default** (PostHog EU). The first CLI calls in the Guest created an installation ID and recorded "registration" and "current release" events before `cua-driver telemetry disable` ran, so one install event was probably sent. The image must disable it before the first run:
- set `DO_NOT_TRACK=1` (or `CUA_DRIVER_RS_TELEMETRY_ENABLED=0`) machine-wide, or write `{"telemetry_enabled": false}` to `%USERPROFILE%\.cua-driver\config.json`
- also set `CUA_DRIVER_RS_UPDATE_CHECK`. The variable exists in the binary; its effect was not tested.

**Session and #4957.**
- `cua-driver autostart enable` registers `\cua-driver-serve`: an InteractiveToken logon task with RunLevel HighestAvailable, running `powershell -WindowStyle Hidden -Command Start-Process cua-driver.exe serve`.
- `autostart kick` to a ready `status` takes 1.9 s. The daemon runs in session 1 with about 20 MB of working set.
- #4957 reproduces: a daemon started by a LeastPrivilege task rejects the admin SSH session (`os error 233`, "SIDs must match"). `cua-driver stop` over SSH cannot reach that daemon either.
- A HighestAvailable task works with the existing admin SSH account, as upstream's autostart does, so **no non-admin account is needed**. A non-admin account was not tried; it would need its own logged-on desktop.

**Bridge.** One SSH connection running `cua-driver mcp --socket \\.\pipe\cua-driver` connects and initializes in 0.9–1.0 s and then stays open. It exposes 63 tools.
- A named `session` ends when the connection closes. The next connection must call `start_session` again; until then, calls fail with "session has ended".
- The permission mode was `standard`. No approval prompt appeared for anything tried, including `browser_prepare` with an isolated profile.

| Tool | Result | Warm latency |
|---|---|---|
| `list_apps` | works: 119 apps (running and Start menu) | 1.85 s |
| `list_windows` | works; **#4906 did not reproduce** with 1–4 windows | 18–46 ms (median of 10: 18 ms) |
| `get_window_state` (Notepad, markdown) | works: 30–32 elements, 2.3 KB, tokens `snapshot:N`, `invalidated_snapshot_ids` | 210–320 ms (first 556–592 ms) |
| `get_window_state` (Explorer) | works: 152 elements, 14.1 KB | 170–240 ms |
| `get_window_state` screenshot | **degraded**: the window capture (Windows Graphics Capture) is blank white on this display; `get_desktop_state`'s desktop screenshot is correct | 220 ms |
| `click` by token | works: UIA Invoke (Notepad "Add New Tab", Explorer "Documents" navigated) | 15–41 ms |
| `set_value` | works (ValuePattern); without an `element_token` it fails with "requires an admitted element target" | 16 ms |
| `type_text` with `element_token` | works, including non-ASCII (`äöü é 你好 🙂`), verified by read-back | 153 ms |
| `type_text` without token (background) | **degraded**: PostMessage lands at the caret, reported "not verified" | 54–83 ms |
| `type_text`/`press_key` foreground | **reports success but nothing lands** unless a foreground `click` put focus in the editor first; then it works. `bring_to_front` alone was not enough | 17–59 ms |
| `hotkey` on modern Notepad | **fails** (`tool_invocation_failed`): no UIA accelerator for `ctrl+a`/`ctrl+end`, and PostMessage is ignored by XAML; also with `delivery_mode:"foreground"` | 44–47 ms |
| `hotkey` on Explorer (Win32) | reported success through PostMessage (`alt+up`), but the title had not changed by the next `list_windows` call (one sample, no wait) | 9 ms |
| `invoke_menu` | **fails** in Notepad (`menu path segment 1 was not found` for Edit > Time/date, File > New tab, View > Zoom) and in Explorer (segment 0); neither has a native menu bar | 110–400 ms |
| `verify_state` | works: `satisfied` in 232 ms; an absent element is `unknown`, by design; a `Window` role selector on Explorer's title stayed `unknown` | 0.24–2.2 s |
| `run_steps` with `expect` and `observe` | works: stops at the first failing step (the hotkey above), reports which `expect` failed and with what value, and `observe` returns the `+ ~ -` delta | 244–396 ms |
| `clipboard_write`/`clipboard_read` | works with Unicode; `include_text:true` is needed to get the text back | 3–13 ms |
| `set_window_frame` | works: `effect: confirmed`, DWM frame read-back, visible on the desktop screenshot | 15 ms |
| Edge: `browser_prepare` isolated profile | works: launches a driver-owned Edge with a fresh profile (2.4–5.7 s). `get_browser_state` binds it exactly (1.4–1.6 s), then `browser_navigate` (78–147 ms), DOM and `semantic_v2` snapshots (5–33 ms) and `browser_click` on a link (1.8 s, navigated) work. `data:` URLs are refused; navigation is http/https/about only. **The three isolated profiles (35.8 MB) stayed in `%LOCALAPPDATA%\CuaDriver\BrowserProfiles`** although the reply said `profile_persists_after_session: false` | |

**Compared with this repo's PowerShell UIA tools** (`guest/windows/desktop.ps1`, through the daemon, same Notepad window):

| | cua-driver over the persistent bridge | `desktop.ps1`, per call (SSH, task, PowerShell) |
|---|---|---|
| Window or desktop state | `list_windows` 18 ms | `vm_desktop_state` 3.8–4.9 s |
| Notepad tree | 0.27–0.32 s, 32 elements, 2.3 KB markdown | 2.9–3.6 s, 39 nodes, 8.3 KB JSON |
| Explorer tree | 0.17–0.24 s, 152 elements, 14.1 KB | 3.6 s, 75 nodes |
| An action | 10–150 ms | 2.6–2.8 s |

- The trees carry the same information: roles, names, values and actions. cua's markdown is about 3.5x smaller per element. Its Explorer walk goes deeper: 152 elements against 75, because `desktop.ps1` uses the UIA control view.
- **Bug found in `desktop.ps1`'s transport, not in cua:** `session.ps1` writes its reply through `[Console]::Out`, which is the OEM code page (437) over SSH. "ä" becomes byte 0x84 and CJK characters become `?`, so a desktop call fails ("exited 0 without a result") whenever an element's text is not ASCII. The cua-typed Notepad tab triggered it. Reading the request through `[Console]::In` has the same problem, so `set_text` with non-ASCII text would be mangled. Fixed after the spike: `session.ps1`, `exec.ps1` and `files.ps1` now set UTF-8 console encoding, and the same Notepad tree (37 nodes with that tab) returns live.
- The earlier cold-start timeout did not recur: the first desktop call 30 s after boot took 4.6 s this time.

**Resources.**
- Idle `serve`: 0.08% of one core over 60 s, 19.8 MB working set, 22.3 MB private, 18 threads.
- Start: 1.6–1.9 s from task start to a reachable pipe. The bridge connects in about 1 s, and the first tree read takes about 0.6 s.

**Invisibility.**
- **Upstream's autostart is not invisible:**
  - `serve` creates a visible `Cua.AgentCursorOverlay` top-level window, because the overlay is on by default. Turning off the session's agent cursor does not remove the window. No cursor showed in the desktop screenshot.
  - Its console window is hidden, titled "Administrator: …cua-driver.exe".
  - The task runs `powershell.exe -WindowStyle Hidden`, which usually shows a console briefly; this was not captured.
- **A task of our own was invisible:** InteractiveToken, HighestAvailable, Hidden, `ExecutionTimeLimit PT0S`, action `conhost.exe --headless cua-driver.exe serve --no-overlay`. The process owned only a hidden `PseudoConsoleWindow`: no overlay, no console, no tray icon.
- The answer file needs to install:
  - the pinned `cua-driver.exe` (for example `C:\Program Files\VeynodeVM\cua\`; `cua-driver-uia.exe` and the DLL were not needed by anything tried)
  - telemetry off before the first run
  - that task, registered for the autologon user with a logon trigger
- The desktop helper's per-call task in `session.ps1` would then not be needed for these tools.

**Consequences for phase 2.**
- The persistent bridge is 10–30x faster than `desktop.ps1` per call, and it adds tokens, `verify_state`, `run_steps`, the clipboard and Edge over CDP. Embedding the driver is worth it on Windows.
- Before relying on it, keep our host-plane input for modern XAML apps: driver hotkeys fail and foreground typing can report false success there. Treat `invoke_menu` as Win32-only.
- File the profile-leak and false-success findings upstream.
