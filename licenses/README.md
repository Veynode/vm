# Experimental runtime release gate

2026-10-09. This is an engineering inventory and packaging gate, **not legal
advice, a redistribution grant, or completed license compliance**. The actual
UTM/QEMU/SPICE experiment is blocked. No binary was distributed by this task.

## Actual artifact findings

[Runtime audit](runtime-audit.json) records hashes of local artifacts, source
archives, build/patch inputs and license files. The UTM5.0.5 DMG still matches
its fetch pin `713afe73c711f01344b8766654be531cd391ed2e30931206f43b5159f143764f`.
The extracted app contains552regular files/243symlinks, about1.39GB. Its exact
content/mode/link inventory is pinned by a tree hash, not inferred from a version
string. A tag and verified download identify a candidate; they do not prove a
complete corresponding-source or redistribution package.

| Inspected component | Evidence actually present | Remaining release work |
| --- | --- | --- |
| UTM app/source | Unchanged downloaded app; source `UTM-5.0.5/LICENSE` is Apache2.0; Renderer has its own Apple copyright/permission notice. | UTM's source license cannot stand in for every embedded dependency or the separately licensed framework. Select the runtime closure actually intended for the product. |
| QEMU/graphics dependency family | UTM `patches/sources` pins QEMU10.0.12-utm archive, SPICE0.14.3, ANGLE/WebKit, virglrenderer, Mesa, DXMT and other revisions. | No complete per-binary matching source/build/license delivery has been established here. Do not call the UTM app tag an exact proof for all framework bytes. Preserve each component's actual notices and applicable delivery requirements after review. |
| Modified SPICE server | Six pinned source archives independently match `sources.json`; `spice-0.14.3/COPYING` contains LGPL2.1, and `server/reds.c` explicitly says2.1-or-later. UTM patch, our scripts and generation helper are available with hashes. | Assemble the exact modified source, upstream/project patches, generated inputs, build instructions, license/notice and modification record. Review replacement/relinking and installation obligations for the intended packaging. This set has not been prepared as a delivery artifact. |
| SPICE dependencies | Actual `otool -L` names unchanged UTM GLib/GIO/GObject, pixman, SSL/crypto and JPEG frameworks plus system frameworks. `configure-spice-reconnect-build.py` supplies source headers but links those official binaries. | Header/source-archive availability and ABI tests do not establish corresponding source of reused vendor binaries. The selective SPICE build is not a from-source rebuild of the full dependency closure. |
| D3DMetal | Framework is physically present. Its ordinary `Resources/LICENSE` is an xxHash BSD2 notice. | That notice is not an Apple D3DMetal commercial redistribution grant. Permission/terms are unresolved; no vendor SDK rights are assumed. |
| EDK2/other notices | App includes `Resources/qemu/edk2-licenses.txt`; source tree has an iOS `License.plist`. | Literal filename scan finds only EDK2 licenses and D3DMetal's notice in the app. This is **not** proof that no other notice exists: embedded UI/data resources were not exhaustively decoded. An actual distributable notice inventory is still needed. |
| Project code | Apache-2.0 ([LICENSE](../LICENSE)); the loaders, `spice-scanout-generation.c` and the runtime patches are GPL-2.0-or-later ([GPL-2.0.txt](GPL-2.0.txt)) and the DXMT patch LGPL-2.1-or-later ([LGPL-2.1.txt](LGPL-2.1.txt)); [NOTICE](../NOTICE) lists them. | None for the source license. The runtime artifact still needs its own notices and corresponding source. |

The scratch SPICE runtime has three absolute symlinks to assets outside its own
directory (`UTM.app`, an Alpine rootfs and `spice-capture`). A plain copy/tar of
that directory is not a self-contained portable product. Packaging with automatic
symlink dereferencing would also silently enlarge its dependency/license scope.
The new gate rejects those links before creating an archive.

Existing documentation is not replaced: [Windows graphics licensing](../docs/decisions/0001-windows-gpu.md#licensing-and-commercial-alternatives)
and [Linux graphics limits](../docs/decisions/0002-linux-graphics.md) retain the
D3DMetal/DXMT/guest-driver distinctions. No commercial SDK inquiry, purchase,
license acceptance or outside message was made. Signing an executable, verifying
Apple's signature, or passing a VM test does not establish redistribution rights.
The release runtime is to be built from source and shipped with its corresponding
source; [decision 0026](../docs/decisions/0026-gpu-runtime-source-build.md) lists the components, licences and
our patches.

## Packaging boundary

The existing `native/tools/package-app.sh` is a **development Swift worker**
packager. It currently copies only our executable/plist and applies an ad-hoc
signature. It does not bundle UTM, so this audit does not claim it is already
shipping the unreviewed runtime. There is no existing production QEMU/UTM
runtime distribution workflow to retrofit. Do not casually extend that dev
script with `cp -R UTM.app` or copy the scratch runtime.

The explicit runtime packaging entry point is now:

```sh
# Read-only check; expected exit2 for the current experimental manifest.
python3 scripts/package-reviewed-runtime.py

# Same gate runs before any output archive is created.
# Current actual manifest must reject this and leave the output absent.
python3 scripts/package-reviewed-runtime.py \
  --output "$PWD/.local/reviewed-runtime-new.tar.gz"
```

[experimental-runtime.json](experimental-runtime.json) deliberately has
`status: blocked`, unresolved blockers, and seven missing evidence records. It
pins both current runtime trees. Every file's relative name, mode, size and hash,
and every link's name/target hash participate in the inventory. Extra, removed,
changed or renamed files require a fresh review. Symlinked input paths, absolute
or escaping/dangling links, FIFOs and other special files fail closed.

A future separately reviewed release manifest must have no blockers, an exact
quiescent staging inventory, and nonempty hash-pinned artifacts for all of:

- Complete component/dependency inventory, including dynamically loaded plugins,
  guest drivers, firmware and any assets actually included.
- Complete license/copyright/NOTICE material for that inventory.
- Corresponding-source delivery covering applicable components and modifications.
- Build, patch and generated-input recipe corresponding to the shipped binaries.
- Reviewed replacement/relinking/installation plan for the intended product.
- Redistribution review, including any proprietary component's actual grant or
  an explicitly reviewed exclusion of that component from the inventory.
- The project's product-source license decision.

The gate only verifies that these evidence artifacts exist and match their
reviewed hashes; it cannot determine their legal sufficiency. Do not generate
empty placeholder documents, flip a status bit or re-pin a new tree and call
that a license review. The repository/reviewer and build staging owner remain
trusted; this is not protection against a malicious person editing the gate.

On a reviewed manifest the packager includes all seven evidence artifacts and
the manifest in the archive, preserves internal symlinks without dereferencing,
rechecks inputs after tar creation, and atomically publishes only to a new path.
It refuses to overwrite an archive or put it inside an input runtime tree.
Input limits are20,000entries/8GiB per tree. It runs no dependency code, download,
VM, signing, notarization, installer or network publication. Archive success is
therefore a mechanical staging result, never a product release pass.

Any future CI/installer/release workflow must use this gate before its own
packaging/signing/publication stage and preserve the reviewed manifest. It cannot
stop someone deliberately bypassing it with an unrelated shell command. Current
Swift development builds remain usable and unchanged.

## Validation performed

Ten actual filesystem/archive tests passed:

```sh
python3 scripts/test-runtime-release.py
```

They cover blocked review/no output, missing or changed evidence, changed/added
runtime bytes, external links even with a refreshed inventory hash, preserved
internal links and inclusion of every evidence artifact, existing-output
preservation, symlinked review files, special files, path traversal, input mutation during archive creation, and real CLI
rejection of oversized/malformed/directory/symlink/FIFO manifests within2seconds. The
successful archive fixture contains synthetic text only; it licenses nothing.

The actual1.39GB experiment was inventoried and checked. The requested output
remained absent and the gate exited2 due to unresolved review, three nonportable
links and all seven missing delivery-evidence artifacts. The sanitized actual
result is [gate-result.json](gate-result.json). No VM was run and no live runtime
or existing package was changed. This closes the accidental unreviewed packaging
path through the new entry point, not the underlying license/source gaps.
