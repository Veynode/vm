#!/usr/bin/env python3
"""Shorten the Neptune guest UMD's synchronous reply wait (UTM guest tools 0.1.273).

  neptune-umd-patch.py IN OUT [--spin N] [--floor US]

Every synchronous reply wait in neptune_umd_arm64.dll (native ARM64 processes)
and neptune_umd_ec.dll (x64 processes under emulation) yields 256 times, then
sleeps at least 1000 us on a high-resolution timer (npt_ring_relax and
npt_relax_sleep_us in osy/virtio-win-mesa). The host's reply takes ~0.5 ms, so
almost every wait pays a ~1.5 ms timer sleep. This rewrites the two immediates
at each of the two inlined call sites: the yield count and the sleep floor.
Defaults are the live-measured variant (640 yields, 200 us floor; see
docs/known-issues.md, Windows GPU). The Authenticode directory entry is
cleared, because the signature no longer matches.

Only the known 0.1.273 builds are accepted (by SHA-256), and the result is
checked against the known patched hash for the default arguments.
neptune_umd_x86.dll (32-bit x86 processes) is not handled.
"""
import argparse, hashlib, pathlib, struct, sys

# Per file: stock SHA-256, SHA-256 with the defaults, and per call site the
# virtual addresses of `cmp wN,#256` (yield count), `cmp w9,#1000` and
# `mov w8,#1000` (sleep floor).
BUILDS = {
    'neptune_umd_arm64.dll': {
        'stock': '8fbb509b3b5e2a06b67392fe6b711832227e1ab802e096bf7d723ace6c66b437',
        'patched': 'bbb14f104a9684159d691a50860d2edf641f88dd97d156c240e361bb4fe20fb5',
        'sites': [(0x180006690, 0x1800066cc, 0x1800066d0), (0x1800077d0, 0x180007814, 0x180007810)],
    },
    'neptune_umd_ec.dll': {
        'stock': '1171ecf0c7725425589f997202cfcc7cbfb7338886b87e71f34b4335e9ced941',
        'patched': '31b3c728e96cb3faa1344ad5e255663217ec7972f7e727fbc3974bc656b9f725',
        'sites': [(0x1800074dc, 0x180007510, 0x180007514), (0x18000862c, 0x180008664, 0x180008668)],
    },
}
DEFAULT_SPIN, DEFAULT_FLOOR = 640, 200
STOCK_SPIN, STOCK_FLOOR = 256, 1000
CMP_W9_1000 = 0x710fa13f
MOV_W8_1000 = 0x52807d08


class PE:
    def __init__(self, data):
        self.data = data
        pe = struct.unpack_from('<I', data, 0x3c)[0]
        if data[pe:pe + 4] != b'PE\0\0':
            raise ValueError('not a PE file')
        sections = struct.unpack_from('<H', data, pe + 6)[0]
        optional_size = struct.unpack_from('<H', data, pe + 20)[0]
        self.optional = pe + 24
        if struct.unpack_from('<H', data, self.optional)[0] != 0x20b:
            raise ValueError('not PE32+')
        self.base = struct.unpack_from('<Q', data, self.optional + 24)[0]
        self.sections = []
        for i in range(sections):
            header = self.optional + optional_size + 40 * i
            virtual_size, va, raw_size, raw = struct.unpack_from('<IIII', data, header + 8)
            self.sections.append((va, min(virtual_size, raw_size) or raw_size, raw))

    def offset(self, va):
        rva = va - self.base
        for start, size, raw in self.sections:
            if start <= rva < start + size:
                return raw + rva - start
        raise ValueError(f'address {va:#x} is outside every section')

    def word(self, va):
        return struct.unpack_from('<I', self.data, self.offset(va))[0]

    def put(self, va, word):
        struct.pack_into('<I', self.data, self.offset(va), word)

    def clear_security_directory(self):
        # Data directory 4 (IMAGE_DIRECTORY_ENTRY_SECURITY) of a PE32+ header.
        struct.pack_into('<II', self.data, self.optional + 112 + 4 * 8, 0, 0)


def patch_bytes(data, sites, spin=DEFAULT_SPIN, floor=DEFAULT_FLOOR):
    """Return a patched copy of data; every site must hold the stock instructions."""
    if not STOCK_SPIN <= spin < 4096:
        raise ValueError(f'spin must be in [{STOCK_SPIN}, 4095] (fewer yields reopens the blob page-staleness race)')
    if not 50 <= floor <= STOCK_FLOOR:
        raise ValueError(f'floor must be in [50, {STOCK_FLOOR}] us')
    pe = PE(bytearray(data))
    for spin_va, cmp_va, mov_va in sites:
        count = pe.word(spin_va)
        # cmp wN,#imm12 is subs wzr,wN,#imm12: 0x7100001f with imm12 in bits 10-21.
        if count & 0xffc0001f != 0x7100001f or (count >> 10) & 0xfff != STOCK_SPIN:
            raise ValueError(f'{spin_va:#x}: expected cmp wN,#{STOCK_SPIN}, found {count:#010x}')
        if pe.word(cmp_va) != CMP_W9_1000 or pe.word(mov_va) != MOV_W8_1000:
            raise ValueError(f'{cmp_va:#x}/{mov_va:#x}: expected the 1000 us floor')
        pe.put(spin_va, (count & ~(0xfff << 10)) | (spin << 10))
        pe.put(cmp_va, (CMP_W9_1000 & ~(0xfff << 10)) | (floor << 10))
        pe.put(mov_va, 0x52800008 | (floor << 5))
    pe.clear_security_directory()
    return bytes(pe.data)


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument('input', type=pathlib.Path)
    p.add_argument('output', type=pathlib.Path)
    p.add_argument('--spin', type=int, default=DEFAULT_SPIN)
    p.add_argument('--floor', type=int, default=DEFAULT_FLOOR)
    a = p.parse_args(argv)
    build = BUILDS.get(a.input.name)
    if not build:
        raise SystemExit(f'{a.input.name}: not one of {", ".join(BUILDS)}')
    data = a.input.read_bytes()
    if hashlib.sha256(data).hexdigest() != build['stock']:
        raise SystemExit(f'{a.input}: not the UTM guest tools 0.1.273 build')
    out = patch_bytes(data, build['sites'], a.spin, a.floor)
    digest = hashlib.sha256(out).hexdigest()
    if (a.spin, a.floor) == (DEFAULT_SPIN, DEFAULT_FLOOR) and digest != build['patched']:
        raise SystemExit(f'{a.input}: patched hash {digest} differs from the measured build')
    a.output.write_bytes(out)
    print(f'{a.output}: spin {a.spin}, floor {a.floor} us, sha256 {digest}')


if __name__ == '__main__':
    sys.exit(main())
