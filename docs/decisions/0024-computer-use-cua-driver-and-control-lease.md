# 0024 — Computer use: cua-driver as the Guest's semantic helper, one action contract, a control lease

Date: 2026-10-26. Status: **accepted**; phases 1–3 of [computer use](../computer-use.md) are implemented with static,
unit and fixture evidence, and live on Linux (Apple Virtualization and QEMU/Venus), a Windows 11 machine and a macOS container.

## Context

Agents drive VM desktops two ways. The host plane (screenshots from the console, input through virtual HID devices) works
for every Guest and needs nothing in it, but sees only pixels. The Guest's own accessibility APIs (AT-SPI, UI Automation,
the macOS Accessibility API) see elements, and our scripts reach them per call (`desktop.ps1`, the agent's AT-SPI helper),
which was slow on Windows and thin on Linux. Phase 0 ran cua-driver 0.38 (MIT, `trycua/cua`) in `ubuntu-desktop` on both
backends and on a Windows 11 ARM machine: it idles at zero CPU, its semantic tools mostly work, and on Windows it is 10–30
times faster than `desktop.ps1`. Its known defects are bounded: background AT-SPI typing truncates multibyte text
(upstream #4754), Chromium's trusted background click is refused on Linux, and XAML apps can report a hotkey as delivered
when it was not.

## Decision

1. **cua-driver is the Guest's semantic helper**, in the third layer of
   [where agent capabilities live](../architecture.md#where-agent-capabilities-live). It is pinned to one official release
   (`runtime/cua-driver.json`: version, commit, archive and binary SHA-256 per platform) and updated only on purpose with
   a new live acceptance. It runs as the desktop user, never root, started on the first call, with no overlay, window or
   prompt, and telemetry and update checks off by environment (and by config on Windows, before its first run). Tools that
   change the driver itself or download code (`install_extension`, `check_for_update`, `install_ffmpeg`, `set_config`) are
   never exposed.
2. **The host owns the contract.** Agents reach the driver only through the daemon's bridge (`cua_bridge`), which maps
   every tool to an MCP scope and refuses unknown tools, so a newer driver adds nothing until it is mapped; applies the
   phase-0 delivery policies; bounds deadlines and frames; and passes results through unchanged — `unverifiable` and
   `unknown` are never turned into success. Two faces: a curated `cua` tool group on the main MCP, and a per-VM endpoint
   that serves the driver's own schemas so agents and skills written for cua-driver work unchanged. Replacing the driver
   per Guest later means replacing what sits behind the bridge, not the faces.
3. **One action result.** Host input tools return cua's `ActionResult` with the host's own route `virtual_hid`. Its effect
   comes only from evidence the host saw after the input (a changed region at the target, a text readback); dispatch alone
   is `unverifiable`. A Guest without the driver still answers `cua_get_window_state` with OCR rows marked untrusted, which
   `verify_state` reports as `unknown` and actions turn into host pixel input.
4. **A person's input wins.** A control lease per VM: while a viewer is open and its person gave input within 5 s, agent
   input on every surface is refused with `person_in_control` and resumes on its own; closing the viewer returns control
   at once. The lease orders a person and agents on one host; it is not an authorization boundary between agents, which
   remains the MCP scopes and the local trust boundary of the daemon socket.

## Consequences

- A Guest helper ships in images (`ubuntu-desktop`) and is installed on first use in Windows machines and macOS
  containers, from the pinned binaries the dev runtime registers. A missing or altered binary is refused before anything
  is copied.
- Windows needs a user logged on to the console: the driver serves its pipe from a hidden logon task with that user's
  elevated interactive token, because a less-privileged token rejects the administrator SSH user (upstream #4957).
- Only the Apple Virtualization console window reports a person's input now; the QEMU console and browser desktops do not
  pause the agent until they report it too.
- QEMU Linux desktops without the driver keep the host plane only; with it, the `cua_*` tools replace the window and
  accessibility tools that QEMU refuses.
