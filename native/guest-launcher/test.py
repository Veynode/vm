#!/usr/bin/env python3
"""Real host temporary-filesystem/process tests; never starts Guest services or requests TCC."""
import argparse, hashlib, json, os, pathlib, shutil, subprocess, tempfile, unittest

ARGS = None

class LauncherTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='launcher-test-', dir=ARGS.directory)
        self.root = pathlib.Path(self.temp.name)
        self.source = self.root / 'source'
        shutil.copytree(ARGS.bundle, self.source)
        self.source.chmod(0o700)
        self.launcher = self.source / 'launcher'

    def tearDown(self):
        self.temp.cleanup()

    def call(self, *args, success=True, executable=None):
        result = subprocess.run([str(executable or self.launcher), *map(str, args)], capture_output=True, timeout=8)
        self.assertEqual(result.returncode == 0, success, (result.returncode, result.stdout, result.stderr))
        self.assertLess(len(result.stdout), 4096)
        value = json.loads(result.stdout)
        if not success:
            self.assertIs(value['ready'], False)
            self.assertIsInstance(value['error'], str)
        return value

    def test_verified_then_local_install_without_source(self):
        original = self.call('verify')
        self.assertFalse(original['service_started'])
        self.assertFalse(original['permission_checked'])
        destination = self.root / 'local'
        installed = self.call('install', '--destination', destination)
        self.assertTrue(installed['installed'])
        self.assertFalse(installed['launchd_installed'])
        self.assertEqual(installed['hashes'], original['hashes'])
        for name in ['launcher', 'integration', 'capture', 'manifest.json']:
            self.assertEqual((destination / name).read_bytes(), (self.source / name).read_bytes())
            self.assertEqual((destination / name).stat().st_nlink, 1)
            self.assertEqual((destination / name).stat().st_mode & 0o777, 0o600 if name == 'manifest.json' else 0o700)
        shutil.rmtree(self.source)
        independent = self.call('verify', executable=destination / 'launcher')
        self.assertEqual(independent['hashes'], original['hashes'])

    def test_existing_destination_unchanged(self):
        destination = self.root / 'local'
        self.call('install', '--destination', destination)
        before = {p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in destination.iterdir()}
        self.assertEqual(self.call('install', '--destination', destination, success=False)['error'], 'destination_exists')
        self.assertEqual(before, {p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in destination.iterdir()})

    def test_destination_symlink_and_parent_symlink(self):
        target = self.root / 'target'; target.mkdir(mode=0o700)
        link = self.root / 'link'; link.symlink_to(target)
        self.assertEqual(self.call('install', '--destination', link, success=False)['error'], 'destination_exists')
        self.assertEqual(self.call('install', '--destination', link / 'new', success=False)['error'], 'directory_symlink_or_unavailable')
        self.assertEqual(list(target.iterdir()), [])

    def test_raw_destination_dotdot_and_repeated_separators(self):
        for suffix in ['/nested/../local', '/./local', '//local']:
            self.assertEqual(self.call('install', '--destination', str(self.root) + suffix, success=False)['error'], 'unsafe_destination_path')
        self.assertFalse((self.root / 'local').exists())

    def test_source_symlink_is_rejected(self):
        original = self.source / 'capture'; target = self.root / 'target'; original.rename(target); original.symlink_to(target)
        self.assertEqual(self.call('verify', success=False)['error'], 'bundle_file_unavailable_or_symlink')

    def test_source_fifo_is_rejected_without_blocking(self):
        path = self.source / 'capture'; path.unlink(); os.mkfifo(path, 0o600)
        self.assertEqual(self.call('verify', success=False)['error'], 'bundle_file_not_private_regular')

    def test_source_hardlink_is_rejected(self):
        os.link(self.source / 'capture', self.root / 'second-link')
        self.assertEqual(self.call('verify', success=False)['error'], 'bundle_file_not_private_regular')

    def test_source_hash_and_mode_are_verified(self):
        path = self.source / 'capture'; path.write_bytes(path.read_bytes() + b'changed')
        self.assertEqual(self.call('verify', success=False)['error'], 'binary_hash_mismatch')
        path.chmod(0o722)
        self.assertEqual(self.call('verify', success=False)['error'], 'bundle_file_not_private_regular')

    def test_extended_acl_is_rejected(self):
        path = self.source / 'capture'
        subprocess.run(['/bin/chmod', '+a', 'everyone allow read', str(path)], check=True)
        self.assertEqual(self.call('verify', success=False)['error'], 'extended_acl_not_supported')

    def test_manifest_strict_types_and_members(self):
        path = self.source / 'manifest.json'; value = json.loads(path.read_bytes())
        value['version'] = True; path.write_text(json.dumps(value))
        self.assertEqual(self.call('verify', success=False)['error'], 'invalid_manifest')
        value['version'] = 1; value['files']['extra'] = value['files']['capture']; path.write_text(json.dumps(value))
        self.assertEqual(self.call('verify', success=False)['error'], 'invalid_manifest')

    def test_quarantine_is_not_stripped_or_bypassed(self):
        path = self.source / 'capture'
        subprocess.run(['/usr/bin/xattr', '-w', 'com.apple.quarantine', '0081;00000000;VeynodeFixture;', str(path)], check=True)
        self.assertEqual(self.call('verify', success=False)['error'], 'quarantined_bundle_file_requires_separate_review')
        self.assertIn(b'com.apple.quarantine', subprocess.check_output(['/usr/bin/xattr', str(path)]))

    def test_nonprivate_destination_parent_rejected(self):
        directory = self.root / 'public'; directory.mkdir(mode=0o755)
        self.assertEqual(self.call('install', '--destination', directory / 'local', success=False)['error'], 'directory_owner_or_mode')

    def test_physical_host_denial_before_permission(self):
        state = pathlib.Path.home() / '.veynode-services'
        before = state.lstat() if state.exists() else None
        for action in ['preflight', 'start']:
            for role in ['integration', 'capture']:
                self.assertEqual(self.call(action, role, success=False)['error'], 'nonroot_virtual_mac_guest_required')
        after = state.lstat() if state.exists() else None
        self.assertEqual(before, after, 'Host guard must not create/change service lock state')

    def test_actual_exec_lock_inheritance_and_release(self):
        result = self.call('self-test', '--directory', self.root)
        self.assertTrue(result['passed'])
        self.assertTrue(result['duplicate_lock_rejected'])
        self.assertTrue(result['exec_child_retains_lock_after_parent_close'])
        self.assertTrue(result['lock_available_after_exact_child_reap'])
        self.assertEqual(sorted(p.name for p in self.root.iterdir()), ['source'])

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bundle', required=True, type=pathlib.Path)
    parser.add_argument('--directory', required=True, type=pathlib.Path)
    ARGS = parser.parse_args()
    if subprocess.check_output(['/usr/sbin/sysctl', '-n', 'hw.model']).strip().startswith(b'VirtualMac'):
        raise SystemExit('These are physical-host tests; do not run them in Guest')
    unittest.main(argv=['launcher-tests'], verbosity=2)
