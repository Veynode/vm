#!/usr/bin/env python3
"""Host-only preparation checks; synthetic output is not Guest acceptance."""
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class Tests(unittest.TestCase):
    def test_shells_parse_and_fixed_info_only_boot(self):
        for name in ['boot', 'install', 'provision']:
            subprocess.run(['/bin/sh', '-n', str(ROOT / f'scripts/installed-qemu-desktop-{name}.sh')], check=True)
        install = (ROOT / 'scripts/installed-qemu-desktop-install.sh').read_text()
        self.assertIn('command_args="--virtio-serial"', install)
        self.assertNotIn('--allow-', install)
        self.assertIn('devfs dmesg udev udev-trigger udev-settle', install)
        self.assertNotIn('mdev hwdrivers', install)
        self.assertIn('xorg-server=21.1.23-r0', install)
        self.assertIn('mesa-dri-gallium=25.2.7-r1', install)
        self.assertNotIn('/v3.24/', install)

    def test_marker_accepts_only_small_identifier(self):
        spec = importlib.util.spec_from_file_location('marker', ROOT / 'scripts/installed-qemu-boot-marker.py')
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        for item in ['abc-123_X', 'a' * 64]:
            self.assertEqual(module.value(item), item)
        for item in ['', 'a' * 65, '../x', 'x\n', 'é', 'a b', '$(id)', 'x=y']:
            with self.subTest(item=item), self.assertRaises(ValueError):
                module.value(item)

    def test_acpi_marker_only_for_power_button(self):
        source = (ROOT / 'scripts/installed-qemu-acpi-handler.sh').read_text()
        for device in ['PWRF', 'PBTN', 'LID', 'arbitrary']:
            with self.subTest(device=device), tempfile.TemporaryDirectory() as temporary:
                console = Path(temporary) / 'console'
                script = source.replace('/dev/console', str(console)).replace('/sbin/poweroff', 'echo REQUEST_NORMAL_POWEROFF')
                result = subprocess.run(['/bin/sh', '-c', script, 'test', 'button/power', device, '00000080', '1'], capture_output=True, text=True, check=True)
                if device in ['PWRF', 'PBTN']:
                    self.assertEqual(console.read_text(), 'VEYNODE_INSTALLED_ACPI_POWER_BUTTON\n')
                    self.assertIn('REQUEST_NORMAL_POWEROFF', result.stdout)
                else:
                    self.assertFalse(console.exists())
                    self.assertEqual(result.stdout, '')

    def gate(self, count=15, failed=False, status=0, trace=True):
        source = (ROOT / 'scripts/installed-qemu-desktop-boot.sh').read_text()
        block = source[source.index('set +e\n'):source.index('apk info -v')]
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            lines = ['XRENDER_CASE ' + json.dumps(dict(name=str(n), passed=not(failed and n == 0)), separators=(',', ':')) for n in range(count)]
            lines.append('XRENDER_SUMMARY {"cases":15,"failed":0,"passed":true}')
            probe = directory / 'xrender-probe'
            probe.write_text("#!/bin/sh\ncat <<'END'\n" + '\n'.join(lines) + f'\nEND\nexit {status}\n')
            (directory / 'veynode-Xorg.log').write_text('VEYNODE_GLAMOR_GLYPH variant=explicit-one\n' if trace else '')
            block = block.replace('/opt/veynode-fixture/', temporary + '/').replace('/run/', temporary + '/').replace('timeout 30 ', '/bin/sh ')
            return subprocess.run(['/bin/sh'], input=block + '\necho PIXELS_ACCEPTED\n', text=True, capture_output=True, timeout=2)

    def test_real_shell_pixel_gate_rejects_failure(self):
        okay = self.gate()
        self.assertEqual(okay.returncode, 0, okay.stderr)
        self.assertIn('PIXELS_ACCEPTED', okay.stdout)
        for variant in [dict(count=14), dict(count=16), dict(failed=True), dict(status=1), dict(trace=False)]:
            with self.subTest(variant=variant):
                result = self.gate(**variant)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn('PIXELS_ACCEPTED', result.stdout)


if __name__ == '__main__':
    unittest.main()
