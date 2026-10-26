# Computer use: cua's driver on VeynodeVM's system-level VM

Status: **proposed plan**, not implemented. It follows [where agent capabilities live](architecture.md#where-agent-capabilities-live).

Sources:
- github.com/trycua/cua at `de783ee0a6` (2026-10-26), driver workspace 0.38.0, contract 0.8.0. Read statically; nothing was built or run.
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
- Linux needs `libx11-6 libxi6 libxtst6 libxext6 libwayland-client0 libxkbcommon0`, `at-spi-bus-launcher`, and `/dev/uinput` with a udev rule plus group `input`. Without uinput only foreground delivery works, which is fine in a VM.
- The driver runs as the desktop user, never root. It is pinned to a release built from source, its checksum is recorded in `runtime/`, and it is updated on purpose.
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

- **QEMU desktops:** desktop and a11y tools are refused by the daemon (`lib.rs:2684`), and QEMU has only the serial transport. Fixed by the driver port.
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
