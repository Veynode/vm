# Ship a Neptune guest UMD with a shorter reply wait

Status: **proposed**, 2026-10-28. Not implemented in the installer; see "Open questions".
Update 2026-10-29: the runtime moved to UTM 5.0.8 and the installer to guest tools 0.1.274 (Neptune wire format 2).
The measurements and `scripts/neptune-umd-patch.py` below are for 0.1.273's UMDs; 0.1.274's have not been examined.

## Context

Every synchronous D3D11 reply wait in UTM's Neptune guest UMD (guest tools
0.1.273, `neptune_umd_arm64.dll` and `neptune_umd_ec.dll`) yields 256 times,
then sleeps at least 1 ms on a high-resolution timer. That timer wakes after
1.5 ms. The host answers a staging `Map` in about 0.5 ms, which is too late for
the 0.26 ms yield window, so every such wait costs about 1.4 ms
([known issues](../known-issues.md#windows-11-arm64-machines), "Where the
1.4 ms goes"). Live measurements on Windows 11 26300 changed the policy to 640
yields followed by 200 µs sleeps. With that change the 64x64 wait-every-frame
case went from 1.35–1.57 ms to 0.41–0.49 ms, and 1080p `fill` waiting every
frame from 3.1–4.6 ms to 2.3–3.0 ms. Both a binary patch and a source build
gave this result, and the pixel test passed. The cost is guest CPU while
waiting: CPU time per wall time rose from about 0.2 to about 0.7 of a core in
that loop.

No VeynodeVM host change removes the wait. The host's extra time is the GPU
waking from idle, and even a warm host (about 260 µs, or about 145 µs with
[`runtime/dxmt-gpu-completed-event.patch`](../../runtime/dxmt-gpu-completed-event.patch))
leaves little margin under the stock window.

## Decision (proposed)

Install the guest tools' viogpu3d package unchanged, then replace its native
ARM64 and x64 (ARM64EC) UMDs in the driver store with versions whose wait is
shorter. The installer does this, so the user never sees it.

- **ARM64 (`neptune_umd_arm64.dll`).** Prefer a source build of
  osy/virtio-win-mesa at the commit that matches the tools release
  (`44d3259f888` for 0.1.273). It carries a patch that makes the policy
  tunable with `NPT_RELAX_YIELDS`, `NPT_RELAX_FAST_SLEEPS` and
  `NPT_RELAX_FAST_US`. It cross-compiles on the Mac with clang-cl, the xwin
  Windows SDK and the WDK headers. Until that build runs in release CI, use
  the binary patch.
- **ARM64EC (`neptune_umd_ec.dll`).** Use the binary patch
  ([`scripts/neptune-umd-patch.py`](../../scripts/neptune-umd-patch.py)). It
  accepts only the 0.1.273 files, identified by SHA-256, and checks that the
  default output matches the live-measured build. ARM64EC and ARM64X need
  MSVC, which the Mac build doesn't have.
- **x86 (`neptune_umd_x86.dll`, 32-bit apps)** has the same loop. It isn't
  patched yet; its call sites haven't been located.

## Licence

Neptune's guest code lives in a Mesa fork; its files carry
`Copyright 2026 Turing Software LLC` and `SPDX-License-Identifier: MIT`
(checked in `src/virtio/neptune/npt_ring.c` at `44d3259f888`). The viogpu3d driver repositories are BSD 3-Clause. A modified
UMD must keep the copyright and permission notices. Ship them beside the DLL
in the guest tools' licence file and in VeynodeVM's third-party notices. The
binary patch modifies a file the user supplied (the guest tools ISO) on the
user's own machine, so VeynodeVM distributes only the patch tool. A
source-built DLL would be a redistributed Mesa derivative. Either way, the
Authenticode signature is dropped: Windows 11 26300 loaded the unsigned UMD
from the driver store, but that is not documented behaviour (not legal
advice).

## Maintenance

- Each new guest tools release needs new call-site addresses and hashes in
  `scripts/neptune-umd-patch.py`. Find them by disassembly or by building
  the matching Mesa commit. The script refuses unknown builds instead of
  guessing. The host's Neptune wire format must match: 0.1.274 speaks
  format 2, and UTM 5.0.5's host speaks format 1.
- Propose the tunable relax policy upstream (osy/virtio-win-mesa
  `npt_ring.c`). Once a release ships it, drop the replacement and set the
  environment variables, or rely on its defaults.
- Don't lower the yield count below 256. The upstream comment ties it to a
  blob page-staleness race.

## Open questions (maintainer decision)

1. **Repairing a restored UMD.** If Windows reinstalls the stock files
   (driver update, `pnputil` repair, System File Checker on the driver
   store), a boot-time task can detect it by hash. Swapping files while
   dwm already has the stock UMD loaded produced a mixed state that hung
   the guest once. A clean swap needs a reboot, and a guest-initiated
   restart trips QEMU's hvf `isv` assertion (the host must stop and start
   the machine). Options:
   - a boot task that swaps the files and asks the host, through the
     machine's stop/start, for a restart;
   - checking at install time only;
   - waiting for an upstream release.
2. **Release build of the ARM64 UMD.** Building needs the Microsoft SDK and
   WDK (their licences accepted on the build machine), plus Mesa at the
   matching commit. Should release CI own it, or should the binary patch
   stay the only shipped form?
3. **CPU budget.** The 640-yield spin costs guest CPU that dwm also pays,
   since it loads the same UMD. The `NPT_RELAX_*` knobs allow a cheaper
   default (for example 384 yields), but idle dwm CPU per setting hasn't
   been measured.
4. **Installer scope.** `install-distro.py windows` doesn't install the
   guest tools today. The Neptune driver is installed by hand (see known
   issues). Automating the tools install is a prerequisite.
