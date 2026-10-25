# Linux graphics: measured backend boundary

2026-10-08, Apple M1 Pro / 16 GB / macOS 26.4.1. Decision: retain Apple VZ for the existing Linux lifecycle/control path, but **do not advertise accelerated Linux graphics for that backend**. Evaluate an independent QEMU/HVF graphics backend using the existing UTM implementation. macOS guests remain on Apple VZ; Windows has a separate GPU gate in [0001](0001-windows-gpu.md). This is a measured capability split, not removal of the Linux graphics requirement.

## Actual results

| Configuration | Observed guest result | Meaning |
|---|---|---|
| Project Apple VZ Linux boot, kernel fixture | `-virgl -edid -resource_blob -host_visible`, `-context_init`, capsets 0 | Current exposed virtio device has no accelerated virgl/Venus protocol. Boot/framebuffer output is not a GPU result. |
| UTM 5.0.5 QEMU 10.0.12, HVF, `virtio-gpu-gl-pci`, `gl=es`, `ANGLE_DEFAULT_PLATFORM=metal` | `+virgl +edid`, `+context_init`, capsets 1 and 2 | Actual independent guest sees virgl; not merely compiled device help. |
| Same QEMU + Alpine 3.24.2 userspace / Mesa 26.1.6 | `virtio_gpu: driver missing`, EGL falls back to llvmpipe | Failed acceleration test. Alpine's exact Mesa package omits virgl; a successful EGL initialization alone would hide software rendering. |
| Same QEMU + Alpine 3.23.6 userspace / Mesa 25.2.7, same 3.24 kernel | EGL surfaceless/device reports `virgl`, desktop GL 2.1, GLES 3.0 | Actual accelerated-protocol context. The other enumerated EGL device is explicitly llvmpipe; do not confuse its higher advertised GL level with the accelerated device. |
| Same 3.23.6 guest, compiled GLES3 shader triangle, 64×64 pbuffer, 120 draw iterations | Renderer guard `virgl`; center `(255,0,0,255)`, corner `(0,0,255,255)`; guest powers off and QEMU exits 0 | Real rendering and pixel readback pass. No local video encoding involved. This does not measure 1440p/60, frame pacing, desktop compositing, CAD, or games. |
| QEMU Venus blob mode + MoltenVK, Alpine 3.24.2 Mesa Venus/Zink | Capsets 3, but actual `vkCreateInstance` fails `ERROR_OUT_OF_HOST_MEMORY` with virtio control errors | Vulkan **failed/unverified**. Error name does not establish RAM exhaustion; render-server/context/transport needs investigation. Not marked supported. |

Successful recorded evidence: [JSON](../../archive/docs-evidence/windows-linux-gpu-2026-10-08.json). Raw local logs `.local/runtime/utm/verified-linux-gpu.serial.log`, failed Venus log `.local/runtime/utm/linux-venus-failed.log`. Apple VZ raw baseline is `.local/evidence/control-smoke-serial.log`. These are separate guest runs. No matched performance comparison was performed.

## Reproduction

Prerequisites: the repository's existing Alpine fixture (`python3 scripts/prepare-linux.py` then `python3 scripts/build-linux-fixture.py`) supplies `.local/media/alpine-3.24.2/{Image,veynode-initramfs.gz}`. Then:

```sh
./runtime/fetch-utm.sh
./runtime/build-utm-loader.sh
python3 archive/scripts/windows-probe-linux-gpu.py --output .local/runtime/utm/verified-linux-gpu.json
```

The probe downloads pinned Alpine 3.23.6 minirootfs and verifies SHA-256, overlays it in a temporary cpio in RAM, keeps the existing kernel/module fixture read-only, boots a new diskless 1 GiB/2-vCPU guest, installs Mesa and a compiler from Alpine's signed repositories inside the guest, builds the project's GLES test, reads pixels and powers off. No persistent guest disk, host share, or forwarded network port is created. The SPICE Unix socket resides inside a private temporary directory. Dependencies' actual versions appear in the serial log; repositories can update, so exact package versions and original logs must accompany comparisons. This is a reproducible diagnostic command, not a hermetic release image.

The developer loader only adapts the official QEMU dylib lifecycle. Its own ad-hoc signature has the Hypervisor entitlement. It does not patch/re-sign the UTM bundle, disable system protections, or install an application. UTM's native XPC launcher's inherited sandbox makes a raw invocation unsuitable; the separate loader is a diagnostic, not a validated production sandbox design.

The current C test deliberately checks a tiny rendered image, not FPS. It checks shader compile/link, renderer identity and exact pixel colors. It exits nonzero on software fallback or incorrect pixels, and the host probe fails unless the success marker and clean process exit both occur.

## Source evidence and implications

Apple's [VZVirtioGraphicsDeviceConfiguration](https://developer.apple.com/documentation/virtualization/vzvirtiographicsdeviceconfiguration) describes scanout configuration; it does not establish Linux GL/Vulkan acceleration. Our conclusion is scoped to the current host/device/guest configuration, not all possible future Apple APIs. A future custom virtio device still needs a guest driver, host renderer, resource sharing and synchronization; it is not automatic GPU acceleration.

UTM's [graphics design](https://github.com/utmapp/UTM/blob/v5.0.5/Documentation/Graphics.md) describes the guest Mesa → virtio → virglrenderer → ANGLE → Metal path and SPICE display. Source pins and distribution gaps are recorded in 0001. Alpine's [exact Mesa 26.1.6 package definition](https://github.com/alpinelinux/aports/blob/b94e73658216f4e91c701bfdeb420e6874910a74/main/mesa/APKBUILD) lists no virgl Gallium driver, explaining the observed package failure. The [3.23 branch](https://github.com/alpinelinux/aports/blob/3.23-stable/main/mesa/APKBUILD) includes it; the measured version was 25.2.7. Do not silently downgrade a user's guest distribution; the older userspace is a controlled experiment.

Next gates: resolve Venus and newer Mesa compatibility; native console presentation with actual scaling/input and frame-time traces; compositor/application workloads; capture after native window close; multi-VM GPU/context isolation and resource budgets; packaging/license/source compliance. UTM's existing local renderer is promising evidence, not a product integration or a release pass.

## Real console and windowless capture experiment (2026-10-08)

A second, **actual Xorg desktop** experiment uses the same virgl/ANGLE/Metal path, 1.5 GiB RAM and 2 vCPUs, Alpine 3.23.6, Xorg modesetting/glamor, twm, xterm and glxgears. `glxinfo -B` reports direct rendering, `Device: virgl`, `Accelerated: yes`, GL 2.1 and GLES 3.0. This is distinct from the earlier 64×64 offscreen test. glxgears' printed unsynchronized FPS is **not** a measured display frame rate or comparative benchmark.

Evidence: [structured results](../../archive/docs-evidence/windows-linux-desktop-2026-10-08.json), [native UTM console](../../archive/docs-evidence/windows-linux-desktop-native-2026-10-08.png), [windowless SPICE frame](../../archive/docs-evidence/windows-linux-desktop-host-2026-10-08.png), [guest X11 readback](../../archive/docs-evidence/windows-linux-desktop-guest-2026-10-08.png). The native image is from a separate native UTM run of the same fixture, not a claim that both clients were attached simultaneously. The last two are from the same standalone VM run, at different instants.

Measured outcomes:

- **First-consumer capture works without a native window.** A project-owned Objective-C diagnostic connects over a private local SPICE Unix socket, receives the GL scanout IOSurface, copies it with Metal before acknowledging the producer, and exports two different, nonblack desktop PNGs. The real VM remains running after this consumer exits. Actual IOSurface: 1280×800, `BGRA` (`0x42475241`), stride 5120, allocation 4,096,000 bytes, `y0top=false`. This diagnostic makes an explicit copy for safe ownership; it does not establish a zero-copy product pipeline.
- **QMP screendump fails for this accelerated desktop.** It returns success and a 1280×800 all-black PPM while the GL scanout shows animated gears. A successful QMP response does not establish a usable capture capability.
- **Desktop rendering quality fails.** The native viewer and SPICE PNG both show substantial white/solid-block glyph and window-decoration corruption. Guest `xwd -root` XGetImage pixels show the same defect, narrowing it to a point upstream of host capture, potentially glamor/virgl/ANGLE rendering. This does not identify the responsible component. The XWD is 24-bit pixels with a 5120-byte row stride; parsing it as 32-bit pixels adds a separate artificial color/geometry error. The script honors its header.
- **Disabling glamor is rejected.** `AccelMethod=none` produces llvmpipe, `Accelerated: no`, and the experiment exits nonzero. This is not a graphics solution.
- **Independent SPICE consumer reconnect fails.** After the first consumer exits, a fresh second client receives 23,444 draw callbacks over 45 seconds but obtains no usable scanout texture and exports no frames. The VM keeps running. The upstream QEMU patch writes one IOSurface ID to a pipe and avoids resending unchanged scanout geometry; a consumed pipe is a plausible mechanism, not a confirmed root cause. A persistent per-VM capture consumer can outlive local views and browser connections, avoiding routine new-consumer connections; its own crash/restart remains a required recovery gate. No browser reconnect has been tested by this experiment.
- **UTM's stock native-window close fails the requested lifecycle.** In the actual disposable native run, closing the GPU window displayed “Suspend is not supported when GPU acceleration is enabled. Closing this window will kill the VM.” Confirming close stopped that VM. [UTM's window controller](https://github.com/utmapp/UTM/blob/v5.0.5/Platform/macOS/Display/VMDisplayWindowController.swift) attempts suspension on close and force-stops in `windowWillClose`. A separately owned VM process/capture service is still necessary; the stock UTM application is not a product implementation.

The diagnostic GL scanout API follows the [public CocoaSpice source](https://github.com/utmapp/CocoaSpice/blob/127033fa3e59cd49678f49ed54f8adfc060afb56/Sources/CocoaSpice/CSDisplay.m), commit `127033fa3e59cd49678f49ed54f8adfc060afb56`, Apache-2.0. The build fetches public headers from a hash-checked source archive (`4e9f787d6edc21287f5648570ecd7fca07640c559b00c269503798f592783373`) and links the unchanged official UTM libraries. Its upstream `POLLHUP` handling treats a scanout pipe whose producer closed it as stale. The QEMU scanout optimization is in the [pinned UTM patch](https://github.com/utmapp/UTM/blob/v5.0.5/patches/qemu-10.0.12-utm.patch). These source observations do not establish distribution rights for the entire bundled dependency graph; 0001 remains applicable.

Reproduce the capture and its failures:

```sh
./runtime/build-spice-capture.sh
# Default: first capture succeeds, independent-consumer reconnect currently fails.
python3 scripts/qemu-linux-desktop-probe.py --output .local/runtime/utm/desktop-reconnect.json
# Scope explicitly limited to first-consumer capture; not reconnect acceptance.
python3 scripts/qemu-linux-desktop-probe.py --capture-cycles 1 --output .local/runtime/utm/desktop-first.json
# Must fail the hardware renderer guard, rather than accept llvmpipe.
python3 scripts/qemu-linux-desktop-probe.py --xorg-accel none --output .local/runtime/utm/desktop-software-rejected.json
```

Each run saves serial logs, QMP PPM, guest XWD/decoded PNG, host SPICE PNGs and frame metadata. `--extra-consumer EXECUTABLE --capture-cycles 0` runs an independently owned consumer as the first connection using `EXECUTABLE UNIX_SOCKET OUTPUT_PREFIX`; its exit code and logs are preserved. It does not bypass the guest hardware-renderer guard. This supports a separately validated encoder experiment, not a claim that H.264/WebRTC were tested here.

To reproduce the native console, export a new disposable bundle (existing paths are refused):

```sh
python3 scripts/qemu-linux-desktop-probe.py --export-utm .local/runtime/utm/new-desktop.utm
```

Open this bundle in the isolated `.local/runtime/utm/UTM.app`; in that app's QEMU graphics preferences select ANGLE (Metal), disable Vulkan and DirectX for this Linux experiment, and start only the named project probe. No Windows drivers, D3DMetal agreement, host shares, clipboard sharing, guest disk or forwarded network ports are needed. The guest auto-powers off after five minutes. The actual native run used the same settings and a ten-minute timeout. When testing close, expect the destructive-close warning for this disposable VM; do not apply it to existing user VMs. All test VMs were stopped after evidence collection.

## Narrowed 2D reproduction: core X11 text (2026-10-08)

The actual guest pixel probe in [`scripts/linux-xrender-probe.c`](../../scripts/linux-xrender-probe.c) now separates core X11, XRender and GLX. With the same real QEMU/HVF/virgl/ANGLE Metal configuration, **13 of 15 cases pass; the two core-font drawing cases fail**. This changes the next investigation from a broad “A8/compositing is broken” hypothesis to the specialized core-font rendering path. It is not a driver fix or a claim that all other operations work.

| Case group | Actual pixel result |
|---|---|
| XFillRectangle, XCopyArea into a mapped window, XRender fill/Solid Src | Exact pixels pass |
| Uploaded A8 mask and XRender-filled A8 mask, Over composition at alpha 0/64/128/255 | Exact pixels pass; tolerance permitted 1 but measured error 0 |
| Independently specified 8×8 A8 glyph via XRenderAddGlyphs/CompositeString8 | Exact pixels pass |
| A1 bitmap mask, XCopyPlane of deterministic bitmap | Exact pixels pass |
| Red-to-blue linear gradient | Measured error 0; permitted rounding tolerance 2 |
| GLX triangle and scissor rectangles, renderer explicitly `virgl` | Exact pixels pass; triangle's 64 mathematically edge-aligned diagonal centers excluded before running, 4,032 interior/exterior pixels checked |
| XDrawString `Hi` versus depth-one core-font bitmap | Fails. Initial 11-case run omitted all 31 glyph pixels. Expanded 15-case run differs at 2,028 pixels, with spurious solid areas; exact erroneous content is not stable across runs. |
| Same depth-one font bitmap copied to depth 24 with XCopyPlane | Exact pixels pass, validating the bitmap oracle/foreground mapping independently of PolyText |
| XDrawImageString `Hi` versus bitmap plus independently computed font background rectangle | Fails consistently: background is correct but all 31 foreground glyph pixels are missing |

The normal [baseline JSON](../../archive/docs-evidence/windows-xrender-2026-10-08/baseline.json) and [filtered package/renderer/case log](../../archive/docs-evidence/windows-xrender-2026-10-08/baseline.log) accompany per-case actual and expected PNGs. In particular: [actual PolyText](../../archive/docs-evidence/windows-xrender-2026-10-08/baseline/core_text_vs_depth1.png), [expected PolyText](../../archive/docs-evidence/windows-xrender-2026-10-08/baseline/core_text_vs_depth1.expected.png), [actual ImageText](../../archive/docs-evidence/windows-xrender-2026-10-08/baseline/core_image_text_vs_depth1.png), [expected ImageText](../../archive/docs-evidence/windows-xrender-2026-10-08/baseline/core_image_text_vs_depth1.expected.png). These small 64×64 pictures are diagnostic samples, not scaled screenshots of the desktop.

Most oracles are analytic colors/geometry from known requests, not copies of implementation output. The core-font oracle deliberately uses the same selected `fixed` font drawn through a separate **depth-one bitmap path**, then compared with the accelerated depth-24 text output; it is not an independent font rasterizer. The passing bitmap copy and independently supplied A8 glyph help distinguish font loading, bitmap upload and composition from direct core-font drawing. Pixel extraction uses `XGetImage`/`XGetPixel` within the guest, avoiding the host scanout/capture path. Both preflight `glxinfo` and the probe's own GLX context reject software fallback.

Source-level lead, **not a proven faulty component**: installed Xorg is 21.1.23-r0. The official [Xorg 21.1.23 source archive](https://www.x.org/releases/individual/xserver/xorg-server-21.1.23.tar.xz), SHA-256 `e39832e5617dadaf072fdf9f0e19e5d2e1c2a13607ac280bac1aba9f8fe14634`, shows `glamor_font.c:glamor_font_get` storing core fonts in `GL_R8UI` integer textures; `glamor_text.c:glamor_text` uses instanced vertex attributes and core-font shader integer texel/bit operations. `glamor_program.c` can select an EXT_gpu_shader4 compatibility path for GLSL 1.20. A8 XRender glyphs are a different path. The [XRender protocol](https://www.x.org/releases/current/doc/renderproto/renderproto.txt) defines the mask/composition behavior used by the independent expected-color calculations.

A bounded control set `MESA_EXTENSION_OVERRIDE=-GL_EXT_gpu_shader4` **only on the Xorg process**. It **did not fix the two text cases**: 2,231 mismatches for PolyText and 31 for ImageText; the other 13 cases still passed and the separate application GLX context remained virgl. [Control JSON](../../archive/docs-evidence/windows-xrender-2026-10-08/extension-control.json). We have not instrumented the actual Xorg GL context to prove which internal path it selected after that override, so this is an ineffective control, not proof that a particular extension, Mesa, virglrenderer or ANGLE is responsible. No workaround was applied to the product.

Reproduction (expected nonzero exit until the genuine text defect is fixed):

```sh
python3 scripts/qemu-linux-desktop-probe.py --xrender-probe --output .local/runtime/utm/xrender-baseline.json
python3 scripts/qemu-linux-desktop-probe.py --xrender-probe --xorg-disable-shader4 --output .local/runtime/utm/xrender-extension-control.json
```

The mode retains the 1,536 MiB / 2-vCPU limit, builds the diagnostic in the disposable guest using public Alpine development packages, runs all cases even when pixels fail, saves each case plus expected pixels, and quits QEMU cleanly. It does not run or alter the normal desktop/reconnect acceptance path. Only this mode raises the RAM-root filesystem's capacity ceiling from the kernel's approximately 725 MiB default to 1,100 MiB: the extra compiler/development packages exhausted the default rootfs in the first attempt. This is a filesystem ceiling within the same guest memory allocation, not additional guest RAM. The failed setup logs remain at `.local/runtime/utm/xrender-probe.serial.log` and `.local/runtime/utm/xrender-probe-expanded-root.serial.log`; the latter exposed a `df`/mount-table diagnostic issue before the explicit remount was corrected.

Both final runs compiled the exact C source SHA-256 `688d57f7bfe3f47d7e90db15b726b925bc8ab0b54b1d657ad8c1982e9034cf25`; Mesa 25.2.7-r1, Xorg 21.1.23-r0, XRender protocol 0.11. All owned QEMU processes exited 0 after collecting the **failing** pixel result. No GPU timing/performance was measured. Next targeted work is tracing the actual core-font GL requests/context and comparing integer-atlas/instancing behavior; do not replace the accelerated desktop with llvmpipe or infer a GPU-driver fix from these unit-sized cases.

## GL font isolation and actual context boundary (2026-10-08)

The next diagnostic is [`scripts/linux-gl-font-probe.c`](../../scripts/linux-gl-font-probe.c), run in the same guarded real virgl fixture. The application GLX context reports **OpenGL 2.1, GLSL 1.20, no GL_EXT_gpu_shader4**. Its integer-atlas test correctly refuses to proceed; it does not force an unsupported extension. The source observation about integer fonts above therefore does **not** prove the actual Xorg server uses that path. The server's own context has not yet been instrumented.

A separate real **GLES 3 / GLSL 300 ES** context permits isolated integer-atlas tests. In the fully state-checked run, 16 of 34 pixel cases pass and 18 fail. R8UI upload/readback, direct texel fetch, all ten independent integer/coordinate expressions, vertex-ID geometry, GL_SHORT instanced rectangles, and **both GL_SHORT and GL_FLOAT instanced glyphs with a single-purpose font/discard shader pass exactly**. This establishes that those individual APIs and the combined straightforward instanced-font operation can work on this stack. It is not evidence that the Xorg GL2.1 path is correct.

The larger uniform-mode diagnostic shader still produces wrong pixels even though each draw verifies the actual current program, `glGetUniformiv` mode/sampler values, active texture/unit/binding, framebuffer, viewport, completeness and GL errors. Its separate non-instanced glyph case also fails. These failures are preserved, but no component has been assigned blame: there is no reduction proving whether shader translation, a particular control-flow interaction, generic vertex attributes, or an unmeasured diagnostic state causes them. The float `exp2`/`floor` bit surrogate is **not** a reliable exact arithmetic oracle at integer boundaries, so its failure is not evidence of an invalid GL implementation. No release claim counts this exploratory suite as passing.

The state-checked source is saved independently with SHA-256 `e05e8061e01f1a504bf7e3b020c6b2f495e346e0c71a276fca36b7356755a1c3` in [the evidence directory](../../archive/docs-evidence/windows-gl-font-2026-10-08/verified-state-source.c), along with [results](../../archive/docs-evidence/windows-gl-font-2026-10-08/gl-font-verified-state.json), actual/expected PNGs and filtered state/package logs. Earlier expression experiments remain in `.local/runtime/utm/gl-font-{gles,integer-ops,straightline}.*`; their progression must not be read as a confirmed integer-driver defect. Reproduction:

```sh
# Refuses missing GLX integer extension; this is the expected capability result.
python3 scripts/qemu-linux-desktop-probe.py --gl-font-probe --gl-font-api glx --output .local/runtime/utm/font-glx.json
# Exploratory actual GLES integer pipeline; preserves failing cases and exits nonzero.
python3 scripts/qemu-linux-desktop-probe.py --gl-font-probe --gl-font-api gles --output .local/runtime/utm/font-gles.json
```

The inspected official Xorg source offers a closer GL2.1 lead: `glamor_font_get` rejects integer atlases without integer GLSL support; `glamor_poly_text8` can fall back through `miPolyText8` to `glamor_poly_glyph_blt`. That implementation enumerates bitmap pixels into GL_SHORT point coordinates and draws `GL_POINTS`. `GLAMOR_DEFAULT_POINT_SIZE` writes `gl_PointSize=1.0` only for GL_ES; desktop fixed point-size state should still define valid behavior when program point sizing is disabled. This is a reason to measure a small actual point-list case, **not** evidence that an unwritten point size is itself erroneous.

### Measured GL2.1 point-list reduction

The bounded point-list comparison now reproduces a failure without integer textures, instancing or mode-dispatch fragment shaders. All five cases use a 64×64 RGBA8 FBO, the same 12 independently specified point centers, a constant-red fragment shader and exact readback of all 4,096 pixels. The actual context is virgl GL2.1/GLSL120; no unsupported extension is enabled. The renderer guard, API errors, current program, attribute type/size/buffer, framebuffer and viewport are checked.

| Actual point configuration | Pixel result |
|---|---|
| Initial fixed size 1, program point-size disabled, GL_SHORT positions, vertex shader does not write point size | **Fails**, 11 differing pixels |
| Explicit `glPointSize(1)`, program point-size disabled, same shader/positions | **Fails**, 4,084 differing pixels (almost the entire framebuffer red) |
| Same fixed-size state and positions; vertex shader additionally writes `gl_PointSize=1.0` | **Passes**, zero differing pixels |
| Program point-size enabled, shader writes 1, same GL_SHORT positions | **Passes**, zero differing pixels |
| Program point-size enabled, shader writes 1, GL_FLOAT positions control | **Passes**, zero differing pixels |

Before every draw, queried `GL_POINT_SIZE` is 1.0. `GL_VERTEX_PROGRAM_POINT_SIZE` is false for the first three cases and true for the last two; the reported range is 1–511. The [OpenGL 2.1 specification](https://registry.khronos.org/OpenGL/specs/gl/glspec21.pdf), section 3.3, specifies fixed `PointSize` state when a vertex shader is active and vertex-program point sizing is disabled. Thus the first two tests do not intentionally rely on an undefined unwritten shader output. We **do not** test the undefined combination of enabled program sizing and unwritten shader size.

This is a concrete narrow graphics-stack failure and a useful next driver investigation target. It does not locate the faulty layer among guest Mesa, virglrenderer and ANGLE/Metal, nor prove that changing Xorg's macro repairs every desktop defect. The explicit shader assignment is a **diagnostic control**, not a product workaround or completed Xorg patch. Actual Xorg GL requests/state still need instrumentation or a controlled source build to connect this reduction causally to the core-font failure.

[JSON](../../archive/docs-evidence/windows-gl-font-2026-10-08/gl-font-points.json), [state log](../../archive/docs-evidence/windows-gl-font-2026-10-08/gl-font-points.log), [exact tested source](../../archive/docs-evidence/windows-gl-font-2026-10-08/points-source.c), and actual/expected PNGs in `archive/docs-evidence/windows-gl-font-2026-10-08/points-pixels/` are retained. Source SHA-256: `2876eb3d4698355545ac9f0211deb0eedc5bd28e853f1a0ea3771fa1c0361aad`. The 1,536 MiB VM quit normally (QEMU exit 0) after preserving the failing diagnostic exit 1. No performance measured.

```sh
python3 scripts/qemu-linux-desktop-probe.py --gl-font-probe --gl-font-api glx-points --output .local/runtime/utm/font-points.json
```

## Actual Xorg causal comparison: bounded glyph workaround (2026-10-08)

The next experiment has connected the point-list reduction to the **actual Xorg server**, and a narrowly scoped workaround now passes the existing pixel suite. This supersedes the earlier statement that actual server tracing remained undone for this path. It does not repair or identify the underlying guest/host graphics-driver defect.

[`archive/scripts/windows-probe-glamor.py`](../../archive/scripts/windows-probe-glamor.py) builds only `libglamoregl.so` from the hash-verified Xorg21.1.23 source inside the disposable 1,536MiB Guest, using at most two compiler jobs. The saved [Alpine21.1.23-r0 packaging recipe](../../archive/docs-evidence/windows-glamor-causal-2026-10-08/alpine-APKBUILD) declares the same source and a single `termio.h`→`termios.h` preparation change, which the build applies. The new binary is not asserted identical to Alpine's build: unused server targets/docs and LTO are omitted. Both experiment variants use the same configuration and source objects.

The first module adds only bounded logging immediately before the real core-glyph point draws. The second changes only that facet's vertex shader to write `gl_PointSize=1.0` unconditionally, plus its diagnostic label. It leaves the shared macro and other shader facets unchanged. Separate module directories are selected with `Xorg -modulepath`; the host and existing VMs receive no installation. The full build command and two small patches are checked in as `archive/scripts/windows-build-glamor.sh`, `windows-glamor-trace.patch` and `windows-glamor-size.patch`.

**Actual result:**

- Source-built traced baseline reproduces precisely the two core-font failures (4,065 incorrect pixels each in this run); all other13cases pass.
- Baseline Xorg logs prove `poly_glyph_blt` executed both failing requests as31points, actual renderer `virgl`, GL2.1/Mesa25.2.7, fixed point size1.0 and program point sizing disabled.
- Restarting only Xorg with the controlled module produces the same actual fallback/state traces and **all15cases pass**, with zero measured pixel error. Expectations and tolerances were not changed.
- `/proc/<Xorg>/maps` confirms loading each selected module. Baseline module SHA-256 `100c25a7a929fbe49a466f9d0aae4b30356fc1a6eba6f692a07d810a0a8173fd`; explicit-size module `ba64c24cb70c953d26ac032633b6bca98577ea415f7abdc32f7f451d9a115f67`.
- The controlled server then runs actual twm/xterm/glxgears. Guest XWD and independently captured SPICE frames show readable window titles/date text and a clean xterm background, with changing gear frames. The earlier broad blocks/glyph corruption is absent in these inspected images. This is a bounded visual observation, not proof that all desktop operations are correct.
- SPICE capture remains independent of a local window; no native viewer was opened for this run. QMP CPU framebuffer stays black, as before. Guest XWD and host CoreImage PNG colors differ, so color fidelity is not accepted by this experiment.
- QEMU quits with exit0. The source-built baseline diagnostic exits1, the controlled15case suite exits0, and the causal comparison/actual capture wrapper exits0. No performance measurement or software-renderer fallback was used.

Evidence: [result and traces](../../archive/docs-evidence/windows-glamor-causal-2026-10-08/result.json), [build/run log](../../archive/docs-evidence/windows-glamor-causal-2026-10-08/build-and-run.log), [Guest desktop](../../archive/docs-evidence/windows-glamor-causal-2026-10-08/guest-desktop.png), [host desktop](../../archive/docs-evidence/windows-glamor-causal-2026-10-08/host-desktop.png), and per-case actual/expected PNGs in the same evidence directory. Full module binaries, configuration headers, Meson logs and compile commands remain in `.local/runtime/utm/glamor-causal-artifacts/`; their hashes are recorded in the repository.

```sh
python3 archive/scripts/windows-probe-glamor.py --output .local/runtime/utm/glamor-causal.json
# Reparse saved actual-run evidence without starting another VM:
python3 archive/scripts/windows-probe-glamor.py --analyze-only --output .local/runtime/utm/glamor-causal.json
```

Acceptance requires exactly the original15uniquecases, precisely the original two baseline failures, all15fixed cases passing, actual31point/state traces for both variants, and both selected module paths in the real process maps. The ordinary desktop fixture retains its default180s readiness bound; this explicit source-build experiment uses a bounded600s setup allowance. This is not a relaxation of a rendering assertion.

This provides a reproducible **Xorg core-glyph workaround on the tested stack**. It has not yet been packaged into an installed Guest integration service, validated across other font/layout/rendering paths, or tested with game/CAD workloads. The independent composite GLES diagnostic still contains unexplained failures; the workaround does not fix them. A lower-layer fix, full graphics regression matrix, color management, measured performance, supported guest packaging and redistribution audit remain necessary before treating this backend as release-ready. No upstream message or publication was sent.

## Actual upstream glmark2 GLES workload (2026-10-09)

The bounded next step beyond the15pixel diagnostic now passes in a real independent
1536MiB/2-vCPU QEMU/HVF RAM Guest with the same fixed SPICE/glamor path. Unmodified
upstream glmark2 2023.01 was built inside the actual Alpine3.23.6 userspace with
Meson, only `x11-glesv2` and two compiler jobs. The source archive SHA256 is
`8fece3fc323b643644a525be163dc4931a4189971eda1de8ad4c1712c5db3d67`;
actual Guest binary SHA256 is
`9319c38cfa78cc3f3e607123c4acaf4579cf20df45dae3e24e99234da29d8acd`.
Package versions, build/runtime/source hashes, filtered logs and images are saved
in [the actual-run record](../../archive/docs-evidence/linux-glmark2/README.md).

All six unchanged upstream pixel validations pass: client-array and VBO model
submission, mipmapped texture sampling, Phong lighting, normal-map bump and
height-map bump. The parser requires the actual `Validation: Success` records
because upstream may exit0 even on pixel Failure/Unknown. Each validation process
reports `virgl` and GLES3.0/Mesa25.2.7, independently of the accelerated virgl
`glxinfo` guard. No software renderer fallback or assertion relaxation was used.

Three more complex public GLES window scenes—terrain, shadow and refract—complete
their requested2seconds and exit0. A subsequent live terrain window produces
actual Guest XWD plus two changing nonblack SPICE frames; the owned QEMU quits
normally with exit0. Both inspected1280×800 console images show the800×600 terrain
window with visible relief, texture and lighting and black unused background.
They are captured at different animation times and are not a pixel-match oracle.
The separate QMP CPU screendump remains black and does not count as display success.

This expands actual functional coverage to a public multi-scene GLES workload,
not a game/CAD application certification. The complex scenes have no asserted
full-frame reference here; color fidelity and native presentation remain unaccepted.
No native viewer was opened, and raw glmark2 FPS is not a presentation/frame-time
measurement or a comparison with Parallels. This single run does not resolve the
other exploratory GLES failures, installed-Guest packaging, redistribution, broad
compatibility, or performance gates documented above.

```sh
python3 archive/scripts/test-linux-glmark2.py --output-dir .local/evidence/linux-glmark2-next
```
