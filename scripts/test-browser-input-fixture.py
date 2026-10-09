#!/usr/bin/env python3
"""Host-only fixture gate tests. Synthetic probe output is not graphics acceptance."""
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / 'scripts' / (name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

prepare = load('prepare-browser-input-fixture')
desktop = load('windows-probe-linux-desktop')

class Tests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.init = desktop.INIT.replace('font-dejavu eudev', 'font-dejavu eudev build-base libx11-dev linux-headers')
        cls.transformed = prepare.graphics_transform(cls.init, prepare.GLAMOR.read_bytes(), prepare.PROBE.read_bytes())

    def test_module_hash_and_anchor_fail_closed(self):
        with self.assertRaisesRegex(ValueError, 'pinned'):
            prepare.graphics_transform(self.init, b'wrong module', prepare.PROBE.read_bytes())
        with self.assertRaisesRegex(ValueError, 'anchor'):
            prepare.graphics_transform(self.init.replace('twm &', 'twm'), prepare.GLAMOR.read_bytes(), prepare.PROBE.read_bytes())

    def test_hardware_and_loaded_module_guards_precede_oracle_desktop(self):
        subprocess.run(['/bin/sh', '-n'], input=self.transformed, text=True, check=True)
        self.assertIn('xorg-server=21.1.23-r0 ', self.transformed)
        self.assertIn('-modulepath /veynode/modules-fixed,/usr/lib/xorg/modules', self.transformed)
        self.assertIn('grep -q "Device: virgl"', self.transformed)
        self.assertIn('grep -q "Accelerated: yes"', self.transformed)
        self.assertIn('grep \'/veynode/modules-fixed/libglamoregl.so\' /proc/$xorg_pid/maps', self.transformed)
        self.assertLess(self.transformed.index('VEYNODE_INPUT_GRAPHICS_READY=15'), self.transformed.index('twm &'))
        self.assertEqual(desktop.INIT, self.init.replace('font-dejavu eudev build-base libx11-dev linux-headers', 'font-dejavu eudev'))

    def gate(self, case_count=15, failed=False, code=0, trace=True):
        # Execute exactly the generated acceptance block; replace the bounded
        # Guest executable with a short synthetic shell and enforce a host timeout.
        guard = self.transformed.split('set +e\n', 1)[1].split('twm &', 1)[0]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            lines = ['XRENDER_CASE ' + json.dumps({'name':str(n),'passed':not(failed and n==0)}, separators=(',', ':')) for n in range(case_count)]
            lines += ['XRENDER_SUMMARY {"cases":15,"failed":0,"passed":true}']
            (root/'input-xrender-probe').write_text("#!/bin/sh\ncat <<'OUTPUT'\n" + '\n'.join(lines) + f'\nOUTPUT\nexit {code}\n')
            (root/'Xorg.log').write_text('VEYNODE_GLAMOR_GLYPH variant=explicit-one\n' if trace else '')
            guard = 'set +e\n' + guard.replace('/veynode/', directory + '/').replace('timeout 30 ', '/bin/sh ')
            return subprocess.run(['/bin/sh'], input=guard, text=True, capture_output=True, timeout=2)

    def test_exact_fifteen_success_cases_allow_readiness(self):
        result = self.gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('VEYNODE_INPUT_GRAPHICS_READY=15', result.stdout)

    def test_failed_missing_extra_nonzero_or_wrong_module_trace_never_ready(self):
        for params in [{'failed':True}, {'case_count':14}, {'case_count':16}, {'code':1}, {'trace':False}]:
            with self.subTest(params=params):
                result = self.gate(**params)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn('VEYNODE_INPUT_GRAPHICS_READY=15', result.stdout)

if __name__ == '__main__':
    unittest.main()
