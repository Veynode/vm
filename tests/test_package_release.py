import contextlib
import importlib.util
import io
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('package_release', ROOT / 'scripts/package-release.py')
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


def checkout(root, web=True, helpers=('vm-vision',)):
    """A checkout with every built piece as a small file."""
    for name in release.BINARIES:
        touch(root / 'target/release' / name)
    touch(root / 'native/.build/vm-apple')
    touch(root / 'native/entitlements.plist')
    for file in release.BOOT.values():
        touch(root / '.local/boot' / file)
    for name in helpers:
        touch(root / '.local/bin' / name)
    if web:
        for name in release.WEB:
            touch(root / 'web' / name)
        touch(root / 'web/dist/server/server.js')
        touch(root / 'web/dist/client/assets/app.js')


def touch(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(path.name)
    path.chmod(0o755)


class PackageReleaseTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.temp.name) / 'repo'
        self.out = pathlib.Path(self.temp.name) / 'VeynodeVM'

    def tearDown(self):
        self.temp.cleanup()

    def package(self, web=True, identity='-'):
        with contextlib.redirect_stdout(io.StringIO()) as printed:
            record = release.package(self.root, self.out, 'release', identity, web, '0.1.0', 'abc', dry=True)
        return record, printed.getvalue()

    def test_missing_pieces_are_named_and_nothing_is_written(self):
        checkout(self.root)
        (self.root / '.local/boot/modloop').unlink()
        (self.root / 'target/release/vm-control').unlink()
        with self.assertRaises(SystemExit) as refused:
            self.package()
        self.assertIn('target/release/vm-control', str(refused.exception))
        self.assertIn('.local/boot/modloop', str(refused.exception))
        self.assertFalse(self.out.exists())

    def test_the_layout_manifest_and_signing(self):
        checkout(self.root)
        record, printed = self.package()
        for path in ('bin/veynodevm', 'bin/vm-control', 'bin/vm-console-stream', 'libexec/vm-vision',
                     'VeynodeVM.app/Contents/MacOS/vm-apple', 'VeynodeVM.app/Contents/Info.plist',
                     'share/boot/Image', 'web/server.mjs', 'web/dist/server/server.js', 'web/package.json', 'install.py'):
            self.assertTrue((self.out / path).is_file(), path)
        listed = {entry['path']: entry for entry in record['files']}
        self.assertNotIn('MANIFEST.json', listed)
        self.assertEqual(listed['bin/veynodevm']['mode'], '755')
        self.assertEqual(len(listed['bin/veynodevm']['sha256']), 64)
        self.assertEqual(json.loads((self.out / 'MANIFEST.json').read_text()), record)
        self.assertEqual((record['signing'], record['notarized'], record['api']), ('ad hoc', False, 1))
        self.assertTrue(any('UTM' in item for item in record['excludes']))
        # Ad hoc: no hardened runtime; the worker app carries its entitlements.
        self.assertIn('codesign --force --sign - ' + str(self.out / 'bin/vm-control'), printed)
        self.assertNotIn('--options runtime', printed)
        self.assertIn('--entitlements', [line for line in printed.splitlines() if line.endswith('VeynodeVM.app')][0])

    def test_an_identity_signs_for_notarization(self):
        argv = release.signing_argv('Developer ID Application: X', pathlib.Path('/b'))
        self.assertEqual(argv, ['codesign', '--force', '--sign', 'Developer ID Application: X',
                                '--options', 'runtime', '--timestamp', '/b'])

    def test_without_web_and_helpers(self):
        checkout(self.root, web=False, helpers=())
        record, _ = self.package(web=False)
        self.assertFalse((self.out / 'web').exists())
        self.assertEqual(record['includes'], {'web': False, 'helpers': []})

    def test_the_release_ships_its_layout_relative_to_share(self):
        checkout(self.root)
        self.package()
        shipped = json.loads((self.out / 'share/runtime.json').read_text())
        self.assertEqual(shipped['backend'], '../VeynodeVM.app/Contents/MacOS/vm-apple')
        self.assertEqual(shipped['boot']['kernel'], 'boot/Image')
        self.assertEqual(shipped['vision'], '../libexec/vm-vision')
        self.assertNotIn('qemu', shipped)
        for value in [shipped['backend'], shipped['vision'], *shipped['boot'].values()]:
            self.assertTrue((self.out / 'share' / value).is_file(), value)
        runtime = release.shipped_runtime({}, False, True)['qemu']
        self.assertEqual(runtime['launcher'], '../libexec/qemu/probe-loader')
        self.assertEqual(runtime['library'], '../libexec/qemu/Frameworks/' + release.QEMU_LIBRARY)
        self.assertEqual(runtime['venus_runtime'], '../libexec/qemu/venus/registration.json')

    def test_the_venus_registration_is_relative_private_and_signed(self):
        qemu = pathlib.Path(self.temp.name) / 'libexec/qemu'
        for path in ('venus/probe-loader', 'venus/libvirglrenderer.1.dylib', 'venus/virgl_render_server',
                     'Frameworks/' + release.QEMU_LIBRARY, 'Frameworks/' + release.SPICE_LIBRARY,
                     release.ICD, 'share/edk2-aarch64-code.fd'):
            touch(qemu / path)
        release.write_registration(qemu)
        path = qemu / 'venus/registration.json'
        registration = json.loads(path.read_text())
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        self.assertEqual((registration['schema'], registration['profile_revision']), (1, 'owned-group-v1'))
        self.assertEqual(registration['frameworks_dir'], '../Frameworks')
        self.assertEqual(registration['render_server']['path'], 'virgl_render_server')
        self.assertEqual(registration['icd']['path'], '../Resources/vulkan/icd.d/MoltenVK_icd.json')
        self.assertEqual(registration['loader']['sha256'], release.digest(qemu / 'venus/probe-loader'))
        self.assertNotIn('Venus', ' '.join(release.RUNTIME_EXCLUDED))

    def test_the_runtime_notice_names_every_framework(self):
        text, texts = release.notice(['qemu-aarch64-softmmu.framework', 'glib-2.0.0.framework', 'gio-2.0.0.framework'])
        self.assertIn('QEMU (UTM fork)\n  version: 10.0.12-utm', text)
        self.assertIn('files:   Frameworks/glib-2.0.0.framework, Frameworks/gio-2.0.0.framework', text)
        self.assertIn('LGPL-2.1-or-later (LICENSES/LGPL-2.1.txt)', text)
        for source in texts:
            self.assertTrue((ROOT / source).is_file(), source)
        for key, (_, _, _, _, source) in release.COMPONENTS.items():
            self.assertTrue((ROOT / source).is_file(), key)
        with self.assertRaises(KeyError):
            release.notice(['unknown.framework'])

    def test_the_loader_gets_its_entitlements(self):
        plist = release.entitlements_plist(release.LOADER_ENTITLEMENTS)
        self.assertIn('<key>com.apple.security.hypervisor</key><true/>', plist)
        argv = release.signing_argv('Apple Development: X', pathlib.Path('/l'), pathlib.Path('/e'))
        self.assertEqual(argv[-3:], ['--entitlements', '/e', '/l'])

    def test_install_registers_the_directory_and_keeps_the_rest(self):
        checkout(self.root)
        self.package()
        home = pathlib.Path(self.temp.name) / 'home'
        home.mkdir()
        (home / 'runtime.json').write_text(json.dumps({'dns': ['1.1.1.1'], 'qemu': {'worker': '/q'}, 'vz_desktop_stream': '/old'}))
        env = {**os.environ, 'VEYNODEVM_HOME': str(home)}
        subprocess.run([sys.executable, '-I', str(self.out / 'install.py')], env=env, check=True, capture_output=True)
        runtime = json.loads((home / 'runtime.json').read_text())
        out = self.out.resolve()
        self.assertEqual(runtime['backend'], str(out / 'VeynodeVM.app/Contents/MacOS/vm-apple'))
        self.assertEqual(runtime['boot']['initrd'], str(out / 'share/boot/initramfs.gz'))
        self.assertEqual(runtime['web'], str(out / 'web/server.mjs'))
        self.assertEqual(runtime['vision'], str(out / 'libexec/vm-vision'))
        # A helper this release lacks no longer points at another build.
        self.assertNotIn('vz_desktop_stream', runtime)
        self.assertEqual((runtime['dns'], runtime['qemu']), (['1.1.1.1'], {'worker': '/q'}))
        self.assertEqual((home / 'runtime.json').stat().st_mode & 0o777, 0o600)


if __name__ == '__main__':
    unittest.main()
