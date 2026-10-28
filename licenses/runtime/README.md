# Licences of the shipped QEMU/GPU runtime

`scripts/package-release.py --qemu-runtime` copies these into the release's `LICENSES/` with its `NOTICE`, which maps
each shipped framework to its component, version, source and licence. The GPL and LGPL texts are
[../GPL-2.0.txt](../GPL-2.0.txt) and [../LGPL-2.1.txt](../LGPL-2.1.txt); Apache-2.0 is the project's
[LICENSE](../../LICENSE).

Copied from the upstream source at the pinned version: `virglrenderer.txt` and `libepoxy.txt` (the commits UTM 5.0.5
pins), `pixman.txt`, `libjpeg-turbo.txt` (`LICENSE.md` and `README.ijg`) and `openssl.txt` (the SPICE reconnect
build's sources). Copied from a later release of the same project, whose licence text is unchanged: `zstd.txt`
(1.5.7) and `opus.txt` (1.6.1; its copyright years run to 2023). Written from the upstream project's declared licence,
not copied from the pinned source, and to be checked against it: `libffi.txt`, `angle.txt` and `libslirp.txt`.
