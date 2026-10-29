# 0026 — GPU runtime: build from source, ship as a separate signed artifact with its corresponding source

Date: 2026-10-27. Status: **accepted direction**; nothing is implemented yet. The QEMU/HVF GPU backend (`--gpus virgl`,
`--gpus venus`) is a 1.0 requirement. This decision does not replace the legal review: it names what the release must
contain so that review has something complete to look at. [licenses/experimental-runtime.json](../../licenses/experimental-runtime.json)
stays `blocked` until a reviewed manifest from this pipeline replaces it.

## Context

Development runs the GPU backend from the official UTM 5.0.5 app ([runtime/fetch-utm.sh](../../runtime/fetch-utm.sh)): the
QEMU library, its frameworks and firmware are taken from `UTM.app` unchanged, and three pieces are built locally on top
of them (a SPICE server with our fixes, a Venus virglrenderer and render server, our loaders and helpers).
[scripts/install-dev-runtime.py](../../scripts/install-dev-runtime.py) points the daemon at those paths.

That cannot be released as is ([license gate](../../licenses/README.md)):

- We cannot redistribute UTM's binaries without the matching source and build of each one, which we do not have; the
  UTM app tag identifies a candidate, not the exact corresponding source.
- Downloading UTM on the user's Mac at install time was considered and rejected. It ties us to UTM's internal layout and
  exported symbols (`qemu_init`, `qemu_main_loop`), leaves QEMU security fixes on UTM's schedule, costs a 300 MB download
  for a few frameworks, and loading frameworks signed by another Team ID under the hardened runtime needs
  `com.apple.security.cs.disable-library-validation`. Our modified SPICE and render server would still be ours to
  distribute.
- `UTM.app` also carries components we have no right to ship (D3DMetal) or no use for (other QEMU targets, Windows
  graphics, GStreamer).

## Decision

1. **Build the runtime from pinned source in CI.** One lock file lists every component: source URL, SHA-256, upstream
   commit or tag, and declared license. Our changes are kept as patch files per component (today they are Python
   anchor edits in `runtime/`; converting them to plain patches is part of this work), applied in a fixed order.
   [UTM's dependency build](https://github.com/utmapp/UTM/blob/main/scripts/build_dependencies.sh) (Apache-2.0) is the
   reference recipe; we start from the revisions UTM 5.0.5 pins, because those are what the live GPU evidence used.
2. **Ship only the closure the product loads.** It is measured with `otool -L` from the shipped executables, not taken
   from UTM's framework list. Excluded are D3DMetal, DXMT, `d3dmetal-native`, swtpm/libtpms, every QEMU target
   except `aarch64-softmmu`, KosmicKrisp, the Mesa and LLVM builds UTM uses for it, and GStreamer with its plugins.
3. **Keep the validated process shape for 1.0.** QEMU stays a dynamic library started by our small loader in its own
   process, as today. The daemon and CLI never load GPL code; they talk to QEMU over QMP, sockets and pipes. Whether to
   switch to a plain `qemu-system-aarch64` executable is decided later, with its own live evidence.
4. **Sign everything with our Developer ID under the hardened runtime.** Every framework gets the same Team ID, so
   library validation stays on. Only the processes that need them get entitlements: `com.apple.security.hypervisor`
   for the QEMU loader. There is no JIT entitlement, because HVF needs none and TCG is not a supported path.
   The runtime is notarized as part of the release.
5. **Distribute it as its own artifact, versioned on its own.** Each release publishes the following, side by side
   on the project's GitHub Releases:
   - `veynodevm-gpu-runtime-<ver>-darwin-arm64.tar.zst`: the signed binaries and firmware.
   - `veynodevm-gpu-runtime-<ver>-source.tar.zst`: the source of every component as built, our patches, the lock file
     and the build scripts.
   - `NOTICE` and an SPDX SBOM.
   - A signed manifest of hashes.

   The CLI, Homebrew and npm packages do not embed the runtime. `veynodevm runtime install` (also run on first
   `--gpus`) downloads it, verifies the manifest signature, the hashes and `codesign`, and installs it under the state
   directory by version. The CLI declares the runtime versions it accepts. A QEMU or virglrenderer security fix ships
   as a new runtime version without a CLI release.
6. **Meet the copyleft obligations by shipping the source with the binaries.** Each runtime version has its full
   corresponding source published next to it, under the same URL scheme and for as long as that binary is offered.
   This covers GPL-2.0 §3(a) for QEMU, and the LGPL corresponding-source terms. LGPL libraries stay separate dylibs,
   so a user can replace them; the install layout and the rpaths needed to do that are documented. `scripts/package-reviewed-runtime.py`
   runs before signing, and its seven evidence records (inventory, notices, corresponding source, build and patch
   recipe, relinking and installation, redistribution review, product license) come from this pipeline.
7. **Our glue that runs inside the QEMU or SPICE process is licensed separately from the rest of the project.** This
   covers the loaders, `spice-scanout-generation.c` and the patches. It is offered as GPL-2.0-or-later, so it can be
   combined with GPL-2.0-only QEMU whatever license the rest of the project takes. The rest of the project is
   Apache-2.0; [NOTICE](../../NOTICE) lists the GPL files, which carry an SPDX header.

## Component inventory

The pins below are UTM 5.0.5's (`patches/sources` in its source archive), the ones the live GPU evidence used.
Update 2026-10-29: development moved to UTM 5.0.8, which changes virglrenderer to `451505ad`, Mesa to `eed3e1d9`,
MoltenVK to `0790efd5`, DXMT to `66e27f37` and D3DMetal to `fd09f2ed`, adds UTM's in-kernel GICv3 patches to QEMU
10.0.12 and one patch to SPICE 0.14.3; the other pins are unchanged.
"Licence" is what the upstream project declares. The review confirms it against the files in each source archive.
**Ships** is the proposal for 1.0.

### Emulator and GPU path

| Component | Pinned revision | Licence | Source | Used for | Ships | Our patches |
|---|---|---|---|---|---|---|
| QEMU (UTM fork) | `10.0.12-utm` | GPL-2.0-only (files under GPL-2.0+, LGPL, BSD, MIT) | github.com/utmapp/qemu release tarball | the emulator: `aarch64-softmmu` only, as a library | yes | none (UTM's patches are in the fork) |
| EDK2 firmware (`edk2-aarch64-code.fd`) | as bundled with QEMU 10.0.12-utm | BSD-2-Clause-Patent | QEMU `pc-bios` | UEFI machines ([qemu_uefi.rs](../../crates/control/src/qemu_uefi.rs)) | yes, if UEFI machines are in 1.0 | none |
| virglrenderer (UTM fork) | `482f9d8b8c2d2288efa10a027116216909d2c226` | MIT | github.com/utmapp/virglrenderer | virgl (GL) and Venus (Vulkan) host side | yes | **owned-group**: `src/proxy/proxy_server.c` drops `POSIX_SPAWN_SETPGROUP`, so the render server stays in the worker's process group and goes away with it ([runtime/linux-vulkan-build-host-runtime.py](../../runtime/linux-vulkan-build-host-runtime.py)) |
| virglrenderer render server | same source | MIT | built from the patched source above | Venus contexts | yes | same patch ([runtime/linux-vulkan-build-render-server.py](../../runtime/linux-vulkan-build-render-server.py)) |
| libepoxy (UTM fork) | `bf98587477fe68d07b93319ece7b40a7d0e2eabe` | MIT | github.com/utmapp/libepoxy | GL dispatch for QEMU and virglrenderer | yes | none |
| ANGLE (EGL, GLESv2) | WebKit fork `ed78ab6e1a37f4f11583a0bd038f22ec91f3ff10`, `Source/ThirdParty/ANGLE` | BSD-3-Clause | github.com/utmapp/WebKit | GLES on Metal under virgl `gl=es` | yes | none |
| Vulkan-Loader | `6df86907be582927507637f3f6003ea58bc34517` | Apache-2.0 | github.com/osy/Vulkan-Loader | Venus | yes | none |
| MoltenVK (UTM fork) | `ef1c5461774f5fbd224ddcfd91fd2c0ea23f0384` (MoltenVK 1.4.2) | Apache-2.0 | github.com/utmapp/MoltenVK | Vulkan on Metal for Venus | yes | none |
| SPICE server | `0.14.3` | LGPL-2.1-or-later | spice-space.org release tarball | display and input channel between QEMU and the console | yes | UTM's `spice-0.14.3.patch`, then ours: **reconnect fix** (Darwin FIFO scanout generation, adds IOSurface and CoreFoundation, compiles in `native/tools/spice-scanout-generation.c`; [runtime/apply-spice-reconnect-fix.py](../../runtime/apply-spice-reconnect-fix.py)) and **retired publication** (a retired FIFO generation is not a client failure; [runtime/apply-spice-retired-publication-fix.py](../../runtime/apply-spice-retired-publication-fix.py)). Built without GStreamer and Opus. |
| spice-protocol | `0.14.4` | BSD-3-Clause | spice-space.org | headers only | headers only | none |
| libslirp (UTM mirror) | `4.9.1` | BSD-3-Clause (some files MIT) | github.com/utmapp/libslirp | user-mode networking in QEMU | yes | none |
| zstd | `1.5.2` | BSD-3-Clause or GPL-2.0 | github.com/facebook/zstd | QEMU | yes | none |
| libusb, usbredir | `1.0.25`, `0.14.0` | LGPL-2.1-or-later | libusb.info, spice-space.org | host USB passthrough, not used | **no**: build QEMU without them, then re-run the UEFI keyboard and tablet checks | — |

### Shared libraries

| Component | Pinned revision | Licence | Source | Needed by | Ships | Note |
|---|---|---|---|---|---|---|
| GLib (glib, gio, gobject, gmodule) | `2.83.0` | LGPL-2.1-or-later | download.gnome.org | QEMU, SPICE, spice-client-glib | yes | 2.83 is an unstable (odd) series: move to a stable 2.84.x |
| pixman | `0.38.0` | MIT | cairographics.org | QEMU, SPICE | yes | from 2019: update |
| libjpeg-turbo | `1.5.3` | IJG, BSD-3-Clause, zlib | libjpeg-turbo | QEMU, SPICE | yes | from 2018: update to 3.x |
| OpenSSL | `1.1.1b` | OpenSSL/SSLeay | openssl.org | SPICE, spice-client-glib | yes | **1.1.1 is end of life (2023) and 1.1.1b has known CVEs**: move to a supported 3.x (Apache-2.0) |
| libffi | `3.5.0` | MIT | github.com/libffi | GLib | yes | |
| gettext (libintl) | `0.22.5` | LGPL-2.1-or-later | ftp.gnu.org | GLib | yes | |
| libiconv | `1.16` | LGPL-2.1-or-later | ftp.gnu.org | GLib | yes | the macOS system iconv may do; check |
| zlib, bzip2, SASL, PAM | system | — | macOS | QEMU | no (system libraries) | |

### Native console client

[native/console](../../native/console/build.sh) links spice-client-glib. In UTM's build that pulls in GStreamer, libsoup,
phodav, json-glib, Opus and usbredir.

| Component | Pinned revision | Licence | Source | Ships | Note |
|---|---|---|---|---|---|
| spice-gtk (spice-client-glib) | `0.42` | LGPL-2.1-or-later | spice-space.org | yes | build without GStreamer, WebDAV (phodav, libsoup) and USB redirection; the console uses its own VideoToolbox path |
| CocoaSpice | `127033fa3e59cd49678f49ed54f8adfc060afb56` | Apache-2.0 | github.com/utmapp/CocoaSpice | headers or sources, as used by `spice-capture.m` | |
| GStreamer, libsoup, phodav, json-glib, Opus | 1.19.1, 3.6.0, 3.0, 1.10.0, 1.3 | LGPL / BSD-3-Clause (Opus) | — | **no**, once spice-gtk is built without them | |

### Our code in the runtime

| Item | Path | Runs in | Licence (point 7) |
|---|---|---|---|
| QEMU loader | [runtime/utm-loader.c](../../runtime/utm-loader.c) | QEMU process | GPL-2.0-or-later |
| SPICE reconnect loader | [native/tools/spice-reconnect-loader.c](../../native/tools/spice-reconnect-loader.c) | QEMU process | GPL-2.0-or-later |
| Venus host loader | [runtime/linux-vulkan-host-loader.c](../../runtime/linux-vulkan-host-loader.c) | QEMU process | GPL-2.0-or-later |
| Scanout generation | `native/tools/spice-scanout-generation.{c,h}` | inside the SPICE server | GPL-2.0-or-later (LGPL-compatible) |
| SPICE screenshot and capture helper | [runtime/spice-capture.m](../../runtime/spice-capture.m) | its own process | project licence |
| QEMU console app (`spice-console`) | [native/console](../../native/console) | its own process | project licence |

### Excluded from `UTM.app`

D3DMetal and `d3dmetal-native`: these are proprietary and we have no redistribution grant. The following are not
used on macOS 1.0 paths: DXMT, swtpm and libtpms, KosmicKrisp (`vulkan_kosmickrisp`), the 27 other QEMU targets,
`qemu-img`, libgcrypt, libgpg-error, libpng, `spice-client-glib`'s GStreamer plugins, and the decompiled iOS Hypervisor.

## Consequences

- **Self-hosted Apple Silicon CI.** Hosted macOS runners cannot run these VMs, and a full runtime build takes hours.
  Each runtime version reruns the GPU release checks before it is published: `vulkaninfo` reports Venus, the
  virgl and Zink pixel oracles pass, the native and browser desktops work, and so do reconnect and teardown.
- **Version changes are real changes.** Moving off the UTM pins (GLib, OpenSSL, libjpeg-turbo, pixman) changes what
  the GPU evidence covers. Do it in one runtime version and re-verify, not piecemeal.
- **We take on QEMU, virglrenderer and SPICE security maintenance.** Track their advisories, because the guest can
  reach host GPU code through virgl and Venus.
- **Requirements by feature.** Venus needs macOS 26 or newer
  ([qemu_graphics.rs](../../crates/control/src/qemu_graphics.rs)); virgl does not. The release documents both, and the
  CLI refuses Venus below 26 with that message.
- **Development paths.** `runtime/fetch-utm.sh` and the `.local/runtime` paths remain until the built runtime replaces
  them in `install-dev-runtime.py`; they are never packaged.

## Open questions

- Legal sufficiency of the corresponding-source and relinking material (counsel, not this pipeline).
- The project licence for everything outside point 7.
- Whether UEFI machines (EDK2) are in 1.0.
- Whether the guest kernel and initramfs (GPL-2.0, BusyBox) follow the same source-with-binary scheme. That is a separate
  artifact and a separate decision.
