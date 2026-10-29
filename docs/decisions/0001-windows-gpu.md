# Windows ARM GPU decision gate

Status: **open, release-blocking**, evidence checked 2026-10-08 (Australia/Adelaide).

Update 2026-10-29: the runtime pin moved to UTM 5.0.8, whose Neptune speaks wire format 2, so the installer now pins guest tools 0.1.274; D3D11 passed the pixel test on it ([known issues](../known-issues.md#windows-11-arm64-machines)). The 5.0.5 / 0.1.273 notes below stay as they were measured.

Update 2026-10-26: Windows 11 ARM64 now installs and boots as a VeynodeVM QEMU/HVF machine (`machine create --os windows`, see [known issues](../known-issues.md#windows-11-arm64-machines)) with a basic `virtio-ramfb` display and no GPU acceleration. That is the CPU/installation baseline only; this GPU gate is unchanged.

Update 2026-10-26 (Triton attempt, not passing): `machine create --os windows --gpus neptune` selects the `neptune-dxmt-experimental` QEMU profile, UTM 5.0.5's device (`virtio-ramfb-gl,hostmem=8G,blob=on,neptune=on`) with `NPT_BACKEND=dxmt`, the three `NPT_*_LIBRARY_PATH` variables pointing at `dxmt-native.framework`, and `RENDER_SERVER_EXEC_PATH` pointing at UTM's `QEMURenderServer`. The guest driver came from UTM's guest tools 0.1.274 (`Drivers/viogpu3d/w11/ARM64`, SHA-256 of the ISO `ab6add47a4ed992f5920f14e96f68d60566e6f731e2cf816c3d38ab2b78f407a`). In one disposable install, the driver bound as "Red Hat VirtIO GPU 3D controller" but did not work: Code 43 without the render server, and a boot hang with it. The acceptance test (Triton driver working, not Basic Display, plus a D3D11 pixel check) has **not** passed. Details and the recovery step are in [known issues](../known-issues.md#windows-11-arm64-machines). D3DMetal (`NPT_CAPSET_D3D12`) was not tried, because of its redistribution terms.

Update 2026-10-26 (second Triton pass, still not passing): after matching UTM's default arguments (Venus on the same device, MoltenVK ICD, IPA granule) and using guest tools **0.1.273** (0.1.274 speaks Neptune wire format 2, the 5.0.5 host format 1), viogpu3d runs as the only display adapter with no problem code. A D3D11 device still can't be created, because UTM's `QEMURenderServer` is App-Sandbox-signed and crashes at launch outside UTM's own XPC helper. An unsandboxed, ad-hoc-signed copy would have needed a maintainer decision and added a re-signed third-party binary to the redistribution question; the third pass avoids it. The pixel test is `guest/windows/d3d11-pixel-test.c`.

Update 2026-10-26 (third pass, D3D11 accelerated): the profile now starts the reviewed Venus registration's `virgl_render_server`, built from the virglrenderer commit UTM 5.0.5 ships (`482f9d8b`) with Neptune, instead of UTM's sandboxed `QEMURenderServer`, so no third-party binary is re-signed. With it the acceptance test passes: viogpu3d is the only adapter, dxdiag names it with feature level 11_1, and the D3D11 pixel test renders exactly on that adapter (WARP refused). At 1080p an ALU-heavy full-screen frame runs at about 500 FPS pipelined, about 100 times WARP in the same guest; each synchronous round trip costs about 1.4 ms. More vCPUs, a smaller `hostmem` and `virtio-gpu-gl-pci` measured no better, so UTM's arguments stay. D3DMetal would need an x86_64 render server under Rosetta and is still not used. Numbers and the Fast Startup crash are in [known issues](../known-issues.md#windows-11-arm64-machines). D3D12 and redistribution remain open.

Windows ARM remains required for the first public release. CPU virtualization is feasible on this host, but Windows installation, its guest service, GPU correctness/performance, and publication rights for the complete graphics stack are not validated. No Parallels comparison has been run.

## Decision

Evaluate the existing **UTM 5.0.5 beta Triton → Neptune → DXMT → Metal** implementation in a fresh, disposable Windows ARM VM before considering any original Windows GPU driver. Do not use Homebrew QEMU's 2D framebuffer as evidence of acceleration. Do not claim that UTM currently has no Windows acceleration: that statement is obsolete for its 2026 experimental releases.

Keep this candidate an independent backend experiment until it passes the gate below. The upstream binary was downloaded to `.local/runtime/utm/UTM.app`, verified by SHA-256 and `codesign --verify --deep --strict`, and its CLI help ran. A separate developer loader now runs its QEMU dylib; a diskless HVF instance and a real Linux virgl shader/pixel test passed ([Linux evidence](0002-linux-graphics.md)). These facts do **not** establish Windows boot or D3D correctness.

UTM 5.0.5 was chosen because 5.0.6's release currently warns that its matching Windows 3D driver has not been released. Both are prereleases. Stable 4.7.5 is not an equivalent GPU candidate. [5.0.5 release](https://github.com/utmapp/UTM/releases/tag/v5.0.5), [5.0.6 warning](https://github.com/utmapp/UTM/releases/tag/v5.0.6).

## Component ownership and gaps

| Route | Guest driver / virtual device / API translation / host GPU | Assessment |
|---|---|---|
| Installed QEMU 11.1.2 + HVF | virtio display-only driver / QEMU virtio-gpu / software 3D in guest / no accelerated device compiled | CPU lifecycle baseline only. Actual device inventory has `virtio-gpu-device` and `virtio-gpu-pci`, no GL or rutabaga variants. |
| UTM 5.0.5 + DXMT | Triton UMD and forked virtio KMD / patched QEMU virtio-gpu / Neptune in Mesa + virglrenderer, native DXMT / Metal | Best concrete open-source D3D11 experiment. Experimental; no local guest result yet. D3D12 acceptance is not established by DXMT. |
| UTM + D3DMetal | same guest and virtual GPU / Neptune + d3dmetal-native wrapper / Apple's proprietary D3DMetal / Metal | Upstream advertises experimental D3D12 in 5.0.5. The wrapper is not a redistribution license for Apple's framework. Commercial general-purpose VM use/distribution remains unresolved. |
| virgl | Mesa Gallium guest / virtio-gpu / virglrenderer / host GL or ANGLE→Metal | Mature Linux direction does not supply a production Windows D3D11/12 driver. The Windows `viogpu3d` PR remains open/unmerged as checked. |
| Venus | Mesa Vulkan guest + suitable kernel driver / virtio-gpu blob transport / virglrenderer / MoltenVK or KosmicKrisp→Metal | Vulkan transport alone does not provide Windows DWM/D3D integration. Driver/host extension compatibility needs tests. |
| gfxstream | guest Vulkan/GLES frontend + kernel transport / rutabaga virtio-gpu / gfxstream / host graphics implementation | Available QEMU integration does not establish a complete signed Windows ARM driver or D3D11/12→Metal product path. |
| Parallels / VMware Fusion | vendor guest drivers / vendor GPU / vendor translation / vendor host backend | Useful external comparison candidates. Their installed products and management APIs do not prove embeddable, redistributable SDK availability. |
| Original driver | project must provide WDDM KMD/UMD, synchronization, resource sharing, protocol hardening, API translation and Metal implementation | Not selected. Large maintenance, signing, compatibility, security and validation surface; existing code must be evaluated first. |

Primary technical references: [QEMU virtio-gpu documentation](https://www.qemu.org/docs/master/system/devices/virtio-gpu.html), [Mesa Venus](https://docs.mesa3d.org/drivers/venus.html), [gfxstream source](https://github.com/google/gfxstream), [virtio-win PR 943](https://github.com/virtio-win/kvm-guest-drivers-windows/pull/943), [Neptune introduction](https://blog.getutm.app/2026/introducing-neptune-direct3d-virtualization-for-qemu/), [Triton implementation](https://blog.getutm.app/2026/introducing-triton-directx-11-driver-for-qemu/).

Triton's source describes real costs requiring measurement: rendering contexts in separate processes; shared linear textures; CPU polling for shared fence completion; scanout via IOSurface with an extra GPU blit. Thus this is neither verified zero-copy nor evidence that it beats Parallels. Per-context isolation also needs stress testing. Capture of the final DWM console after closing the local window is a separate test from observing an application render target.

## Reproducible artifacts and source pins

Run `./runtime/fetch-utm.sh`, then `vm-archive/scripts/probe-windows.sh --output .local/runtime/utm/host-evidence.json`.

The fetch helper pins [UTM 5.0.5](https://github.com/utmapp/UTM/releases/tag/v5.0.5), 302,621,893 bytes, SHA-256 `713afe73c711f01344b8766654be531cd391ed2e30931206f43b5159f143764f`. It leaves installed applications alone, never removes quarantine, and does not create/start a VM. QEMU in this bundle is a dylib with an XPC launcher, not a drop-in standalone CLI. Directly invoking `QEMULauncher --help` exited 133; do not repeat or mistake it for a supported command-line backend.

The release's [dependency manifest](https://github.com/utmapp/UTM/blob/v5.0.5/patches/sources) pins:

| Component | Source version |
|---|---|
| UTM | tag `v5.0.5`, object `b6f7475be54f9cb542c46b131319454b83489ced` |
| QEMU | `v10.0.12-utm` source archive |
| virglrenderer | `482f9d8b8c2d2288efa10a027116216909d2c226` |
| DXMT | `bcdcf6da89af6c9fedeb2fc92b4367b23d5d6ed6` |
| ANGLE/WebKit | `ed78ab6e1a37f4f11583a0bd038f22ec91f3ff10` |
| libepoxy | `bf98587477fe68d07b93319ece7b40a7d0e2eabe` |
| Mesa host Vulkan implementation | `79bc850d884a1307356ff61c017e58901b90c7e2` |
| MoltenVK | `ef1c5461774f5fbd224ddcfd91fd2c0ea23f0384` |
| LLVM | `15.0.7` |

The guest driver source is separate: [Triton UMD](https://github.com/osy/virtio-win-mesa/tree/neptune), [KMD](https://github.com/osy/kvm-guest-drivers-windows/tree/neptune), [Windows build scripts](https://github.com/osy/build-mesa). Observed branch heads were `694ea5e21c9b9461c59301727bca889a5e17e972` and `3f58ca3efd264831afef8b41f3060dff5fce824b`; **these are not verified as compatible with 5.0.5**. Pin the exact matched guest-tools release and digest before installation; never combine moving branches blindly.

For source reproduction use the pinned UTM checkout's `scripts/build_dependencies.sh -p macos -a arm64`, then its documented app build, or the upstream Triton post's standalone Cocoa QEMU build. Neither build has run here: only Command Line Tools are selected and `xcrun --find metal` failed; full Xcode with Metal toolchain is required. A native DXMT-only build can omit the optional x86_64 D3DMetal renderer. Standalone QEMU needs the UTM patches, ANGLE, libepoxy, virglrenderer with Neptune and a working `libdxmt-native` renderer; Homebrew QEMU alone cannot substitute.


### Matching experimental guest tools

`vm-archive/scripts/windows-fetch-guest-tools.sh` pins the official [QEMU v10.0.12-utm asset](https://github.com/utmapp/qemu/releases/tag/v10.0.12-utm) `utm-guest-tools-0.1.273.iso`, 140,195,840 bytes, SHA-256 `7d2c0343e92358ad5e65078b08ec1dad873eb91b3c154aef60531bf6c2f04601`. UTM 5.0.5's [download task](https://github.com/utmapp/UTM/blob/v5.0.5/Platform/UTMDownloadSupportToolsTask.swift) selects `utm-guest-tools-3d-latest.iso`; on this date it redirected to that exact release asset. This is provenance for the selected match, not a claim that a Windows guest has loaded it.

Read-only ISO inspection found `Drivers/viogpu3d/w11/ARM64/viogpu3d.sys`, ARM64/ARM64EC/x86 UMD DLLs and `vulkan_virtio.dll`. INF version is `08/30/2026,100.6.101.58000`. Extracting the catalog's certificate subjects shows Microsoft Windows Hardware Compatibility Publisher / Microsoft Windows Third Party Component CA 2014 / Microsoft Root Certificate Authority 2010. **Certificate presence alone does not validate the full chain, catalog membership, Windows policy acceptance, or actual signed driver loading.** No driver was installed and no test-signing/security setting was enabled.

The ISO's `virtio-win_license.txt` is a BSD-style three-clause notice. That file does not independently establish licensing/source-compliance for every bundled UMD, SPICE component or renderer. Pin the corresponding KMD/UMD source commit, preserve all required notices and assess the complete dependency set before redistributing. Guest building requires Windows MSVC/WDK and ARM64X/EC outputs ([upstream build instructions](https://github.com/osy/kvm-guest-drivers-windows/blob/neptune/viogpu/viogpu3d/BUILDING.md)). No source-built guest binary was produced here.

The developer adapter is reproducible with `./runtime/build-utm-loader.sh`. Only our own executable is ad-hoc signed with the normal Hypervisor entitlement; no upstream binary or system security policy is modified. It establishes an isolated test route while full-source Xcode build remains unverified. It is not a production sandbox or product distribution design.

## Licensing and commercial alternatives

The pinned [DXMT license](https://github.com/utmapp/dxmt/blob/bcdcf6da89af6c9fedeb2fc92b4367b23d5d6ed6/LICENSE) is LGPL 2.1-or-later. Distribution requires a complete per-component license/source/relinking review, including QEMU, guest drivers, Mesa, ANGLE and renderer dependencies; one top-level license is insufficient. No binary has been shipped by this project.

The July Triton article reports restrictive D3DMetal game-evaluation and noncommercial distribution terms. **The downloaded September UTM 5.0.5 binary actually includes D3DMetal.framework 3.0**, alongside both `dxmt-native` and `d3dmetal-native`. The framework's `Resources/LICENSE` contains an xxHash notice, not an overall Apple distribution grant. This later artifact fact supersedes any assumption that the binary omits D3DMetal, but does not establish our commercial rights. We have not loaded or tested that renderer, accepted a separate Apple agreement, reverse-engineered it, or treated CrossOver's reported arrangement as a license available to us. The applicable Apple agreement must be obtained and checked before using it in a commercial product; D3D12 remains gated.

Parallels' [official SDK deprecation notice](https://kb.parallels.com/en/130091) says the Virtualization SDK is unsupported from Desktop 19.3.0; CLI automation remains available. Its [current EULA](https://www.parallels.com/about/legal/eula/) includes benchmark-testing restrictions. Before a real comparison, establish the applicable agreement and any required vendor permission. Locally Desktop 20.1.0 (55732) reports an expired license and two `invalid` VM statuses; no existing VM was opened or modified.

Broadcom's [Apple Silicon feature matrix](https://knowledge.broadcom.com/external/article/315609) documents Windows ARM D3D11 and management APIs in Fusion. We have not verified any supported embeddable GPU SDK, headless-console texture/capture API, or commercial redistribution grant for either vendor. Absence of verified public terms is **not proof no private agreement exists**. Required vendor answers: supported ARM driver/API levels; embedding and redistribution; unattended lifecycle; independent native console display; headless capture; automation; signing; maintenance/security updates; pricing. No outreach or purchase is authorized or performed.

## Installation medium and current blockers

Microsoft provides the [Windows 11 ARM64 ISO download](https://www.microsoft.com/en-us/software-download/windows11arm64). A direct download API initially returned a generic error; the ordinary Safari webpage successfully generated an official English ISO link for **26H2, build 26300.9457**. Download is in progress in Safari: `Windows11_Client_arm64_en-us_26300_9457.iso`, browser-reported 8.53 GB. The live page publishes English SHA-256 `DBF606CEB4CE1390F9F21AD1D96331848BC6B701CBAB0012785881A1A1C0EC63`; this must match after completion before use. The temporary download token is not persisted here. Reserve at least 9 GB for media and 64 GB virtual disk for standard Windows 11; a downloadable retail ISO is not a free evaluation license. No EULA was accepted or registration submitted.

The official [Windows 11 IoT Enterprise LTSC evaluation](https://www.microsoft.com/en-us/evalcenter/evaluate-windows-11-iot-enterprise-ltsc) explicitly offers ARM64, 90 days, no product key, but requires registration and is scoped to evaluating fixed-purpose devices. It is not a silent replacement for the product's general Windows desktop target. We did not submit registration or personal details. A properly entitled Windows ARM test image or an applicable evaluation agreement is needed before installing/running it.

## Required gate evidence

1. Fresh Windows ARM installation under the selected backend, repeatable start/normal shutdown/stop, guest service handshake; driver versions and signatures recorded.
2. Adapter name and D3D feature levels from real device creation, with WARP/software explicitly detected. D3D11 textured rendering, shader/compute, DWM overlapping windows and representative CAD/game workload; D3D12 separately, with unsupported cases recorded as failures.
3. ARM-native and Windows-emulated x64 builds of the same public workload, identical resolution/resources, recorded separately. Candidate sources: [Microsoft DirectX samples](https://github.com/microsoft/DirectX-Graphics-Samples), [DirectXTK](https://github.com/microsoft/DirectXTK), and a pinned [FreeCAD](https://github.com/FreeCAD/FreeCAD) release. Sample success is not broad compatibility.
4. Windowed native 2560×1440 at 60 Hz; trace frame-time distributions, stalls, input latency, process tree CPU/physical memory, GPU copies and synchronization, warm-up and at least five measured runs per scenario. No speed claim from a single FPS score.
5. Same console remains remotely capturable after closing the native window. H.264 hardware encoding, reconnect and sleep/wake must be measured independently of GPU rendering.
6. One foreground GPU load with two background VMs, with memory pressure and renderer crashes. The proposed 8+8+6 GB guest allocation exceeds this host's 16 GB physical RAM before host overhead: measure pressure first, never present it as a sustainable validated baseline.
7. Matched hardware/software/resource/resolution comparison with Parallels only after lawful test entitlement; separate local, remote direct, and forced TURN runs with network conditions and sample count.

Passing one stage does not waive later stages or authorize public release.

## Windows boot prerequisites exercised

`python3 vm-archive/scripts/windows-boot-probe.py --seconds 8` ran the pinned QEMU/HVF with bundled EDK2 secure-capable firmware, a fresh private NVRAM copy, virtual TPM 2.0 (`tpm-crb`), USB keyboard/tablet and no network. QMP reports `running`, exposes the TPM, saves a screenshot and quits cleanly. Serial output reaches EDK2's UEFI shell. NVRAM asset is **QCOW2 despite its `.fd` extension**; treating it as raw initially failed the required 64 MiB flash-size check and was corrected. This verifies the boot prerequisites only; Secure Boot policy enforcement and Windows installation are not verified. The firmware's QMP image is still an uninitialized display placeholder, so that screenshot is not a working Windows desktop.

After the official ISO download completes and its SHA-256 is verified, `python3 vm-archive/scripts/windows-boot-probe.py --iso /absolute/path/Windows11_Client_arm64_en-us_26300_9457.iso --seconds 60` exercises the install medium in an isolated 4 GiB VM. This command has not yet run with completed media. It creates no Windows disk, never sends keyboard input or accepts a license, preserves logs under `.local/runtime/utm/windows-boot-*`, then shuts down both QEMU and the software TPM. Installer progress and a licensed persistent VM remain necessary before driver/D3D tests.

Latest bounded media checkpoint: 2026-10-08T08:09:05.360277+00:00; Safari still has an incomplete **8,169,291,196-byte** download. See checkpoint (`vm-archive/docs-evidence/windows-media-download-2026-10-08.json`). No ISO hash verification or installer boot occurred. Resume only once Safari publishes the completed file:

```sh
python3 vm-archive/scripts/windows-boot-probe.py --iso "$HOME/Downloads/Windows11_Client_arm64_en-us_26300_9457.iso" --seconds 60
```

The probe itself verifies the official SHA-256 before creating its VM. It sends no keyboard input: a boot-media prompt or UEFI screen must be recorded as pre-installation state, not successful Windows installation.

## Completed download: unresolved media identity (supersedes download-pending state)

The Safari file completed at **8,528,523,264 bytes**. Full-file SHA-256 from an independent `shasum -a 256` is `8992f2d2cfd2fa647e579e9eb31fc21f34da8333c29b6170fde17d2ed6623e9c`. This **does not match the expected English hash**. The prepared probe correctly rejected it before creating a VM.

The live Microsoft ARM64 page was rechecked using both its rendered text and a direct HTTPS HTML fetch with table rows parsed by `<tr>/<td>`. Both associate `8992…` with **Arabic 64-bit** and `dbf606…` with **English 64-bit**. Raw HTML SHA-256: `e71f1caef3fb20eb70daabebbd2de381c44a961c23fe4858f85b3a452655dc8e` (local copy `.local/runtime/utm/windows11arm64-official-page.html`). This excludes a mere flattened-text row association error. Meanwhile, the filename contains `en-us`, and a read-only ISO9660 header inspection gives volume `CCCOMA_A64FRE_EN-US_DV9`. Matching another published row does not resolve this inconsistency or establish correct English media identity.

One short follow-up mistakenly treated the alternate published hash as enough for an isolated boot experiment. It was interrupted during its wait period, the alternate-hash option was removed, and the probe is restored to the original strict English pin. This is **not accepted validation**. Its retained serial log (`vm-archive/docs-evidence/windows-media-aborted-serial-2026-10-08.txt`) shows USB boot attempted, a timeout, then UEFI shell. No Windows installer screen was captured or proven, no keyboard input was sent, no Windows OS disk was created, and no agreement was accepted. Both QEMU and swtpm were stopped. The explicit record prevents this aborted run being confused with a successful installer test.

Next prerequisite: obtain the correctly identified official English ARM64 ISO whose full hash matches the published English entry, or resolve the publisher's filename/content/hash-table inconsistency with authoritative provenance. Do not alter the pin to the observed hash, weaken verification, or boot the current file again while that identity remains unresolved. Structured media checkpoint (`vm-archive/docs-evidence/windows-media-download-2026-10-08.json`) preserves both the earlier download observations and this newer result.

### Read-only WIM identity follow-up

`vm-archive/scripts/inspect-windows-media.py` mounts only the supplied ISO read-only, reads `sources/lang.ini` and WIM metadata using wimlib1.14.5, verifies the source file identity is unchanged and detaches its own mount. It never boots Windows or accepts the media identity. The current en-US artifact contains ARM64 Windows PE/setup26100.9457 and Home/Home Single Language/Pro26300.9457 images, all defaulting to en-US. This strengthens the observed language/hash conflict; it does not resolve the publisher's contradictory English hash. Full sanitized output: `vm-archive/docs-evidence/windows-media-identity.json`; mount cleanup succeeded and `hdiutil info` confirmed no mounted test image.

An independent official **English International / English (United Kingdom)** download was selected through the same Microsoft page and started in Safari, filename `Windows11_Client_arm64_en-gb_26300_9457.iso`,8.54GB. Its independently published expected SHA256 is `23d5d42e8cd37fe1bacc37920eaa1ab0bb044aae0d41eeeb6a77e82a8ca9a483`. This is a separate artifact with its own expected identity, not an alternate hash allowed for the failed en-US file. At this checkpoint the download is in progress, no hash result or boot acceptance yet. Temporary signed download URLs are not persisted in repository evidence. Preserve the original en-US failure and strict pin.

### Independent en-GB download also fails its own published identity

The second official Safari artifact completed at8,543,594,496bytes. Its full SHA256 is `500bcb6f272b7c455156470ea06816f558f13b885723f84ee9eab7534c7dee61`, not the independently recorded English International pin `23d5d42e8cd37fe1bacc37920eaa1ab0bb044aae0d41eeeb6a77e82a8ca9a483`. The current official table associates the observed hash with Dutch64-bit, while read-only `lang.ini` and WIM metadata identify en-GB. This second artifact was **never booted**. Its own mount was successfully detached. The scripts retain distinct strict en-US/en-GB profiles; neither failed artifact is allowed through another language's row.

See en-GB identity evidence (`vm-archive/docs-evidence/windows-media-en-gb-identity.json`). Do not continue downloading languages looking for a hash that passes. Windows installation is now blocked on an independently verifiable matching official artifact or authoritative publisher correction of this provenance conflict, followed by applicable test entitlement/acceptance. The rest of product development continues; this does not make the entire project externally blocked.

### 2026-10-09 independent official-media recheck

The retail English language/hash conflict remains unchanged. The public ordinary
Enterprise evaluation link checked is x64 despite an overview mentioning ARM64.
A distinct IoT LTSC2024 ARM64 en-US candidate has an official5,042,194,432-byte
HEAD response and an independent Microsoft PDF assigning SHA256
`ccec358a760c3c581249f091ed42d04f37b2b99c347b7a58257c3cc272d7982c`.
It has **not been downloaded or booted**. Required registration, evaluation-scope
and agreement acceptance are unresolved; fixed-function IoT evaluation must not
silently replace general Windows desktop acceptance. No strict pin was changed.
Individual PE/catalog signatures and WIM metadata do not resolve whole-media
identity. See the bounded review and primary sources (`vm-archive/docs-evidence/windows-media-review-20261009/`).
