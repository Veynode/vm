import importlib.util, pathlib, struct, unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('neptune_umd_patch', ROOT / 'scripts/neptune-umd-patch.py')
umd = importlib.util.module_from_spec(spec)
spec.loader.exec_module(umd)

BASE = 0x180000000
TEXT_VA, TEXT_RAW, TEXT_SIZE = 0x1000, 0x400, 0x200
SITES = [(BASE + 0x1010, BASE + 0x1020, BASE + 0x1024), (BASE + 0x1100, BASE + 0x1110, BASE + 0x1114)]


def fake_pe(spin_word=0x7104013f):
    """A PE32+ image with one .text section holding the stock instructions at SITES."""
    data = bytearray(TEXT_RAW + TEXT_SIZE)
    data[0:2] = b'MZ'
    pe = 0x80
    struct.pack_into('<I', data, 0x3c, pe)
    data[pe:pe + 4] = b'PE\0\0'
    struct.pack_into('<HH', data, pe + 4, 0xaa64, 1)
    optional_size = 240
    struct.pack_into('<H', data, pe + 20, optional_size)
    optional = pe + 24
    struct.pack_into('<H', data, optional, 0x20b)
    struct.pack_into('<Q', data, optional + 24, BASE)
    struct.pack_into('<II', data, optional + 112 + 32, 0x300, 0x80)  # a signature entry
    section = optional + optional_size
    data[section:section + 5] = b'.text'
    struct.pack_into('<IIII', data, section + 8, TEXT_SIZE, TEXT_VA, TEXT_SIZE, TEXT_RAW)
    for spin_va, cmp_va, mov_va in SITES:
        for va, word in ((spin_va, spin_word), (cmp_va, umd.CMP_W9_1000), (mov_va, umd.MOV_W8_1000)):
            struct.pack_into('<I', data, TEXT_RAW + va - BASE - TEXT_VA, word)
    return bytes(data)


def word(data, va):
    return struct.unpack_from('<I', data, TEXT_RAW + va - BASE - TEXT_VA)[0]


class NeptuneUmdPatchTests(unittest.TestCase):
    def test_rewrites_yield_count_and_floor_and_drops_signature(self):
        out = umd.patch_bytes(fake_pe(), SITES)
        for spin_va, cmp_va, mov_va in SITES:
            self.assertEqual(word(out, spin_va), 0x710a013f)  # cmp w9,#640
            self.assertEqual(word(out, cmp_va), 0x7103213f)   # cmp w9,#200
            self.assertEqual(word(out, mov_va), 0x52801908)   # mov w8,#200
        pe = struct.unpack_from('<I', out, 0x3c)[0]
        self.assertEqual(struct.unpack_from('<II', out, pe + 24 + 112 + 32), (0, 0))

    def test_stock_arguments_restore_the_instructions(self):
        stock = fake_pe()
        out = umd.patch_bytes(stock, SITES, spin=256, floor=1000)
        self.assertEqual([word(out, va) for site in SITES for va in site],
                         [word(stock, va) for site in SITES for va in site])

    def test_refuses_an_unexpected_instruction(self):
        with self.assertRaisesRegex(ValueError, 'expected cmp'):
            umd.patch_bytes(fake_pe(spin_word=0x7104213f), SITES)  # cmp w9,#264

    def test_refuses_fewer_yields_than_stock(self):
        with self.assertRaisesRegex(ValueError, 'spin'):
            umd.patch_bytes(fake_pe(), SITES, spin=128)

    def test_cli_rejects_unknown_build(self):
        with self.assertRaises(SystemExit):
            import tempfile
            with tempfile.TemporaryDirectory() as d:
                src = pathlib.Path(d) / 'neptune_umd_arm64.dll'
                src.write_bytes(fake_pe())
                umd.main([str(src), str(pathlib.Path(d) / 'out.dll')])


if __name__ == '__main__':
    unittest.main()
