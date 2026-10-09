#!/usr/bin/env python3
"""Harness safety guards only; no daemon, process signal or VM is invoked."""
import importlib.util
from pathlib import Path
import unittest
import tempfile

spec = importlib.util.spec_from_file_location('mixed', Path(__file__).with_name('test-control-mixed.py'))
mixed = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mixed)


class Guards(unittest.TestCase):
    def test_mac_and_foreign_vm_mutations_always_reject(self):
        for method in ['create', 'start', 'shutdown', 'force_stop', 'delete', 'human_takeover']:
            for target in ['macos-smoke', 'another-linux', None]:
                with self.subTest(method=method, target=target), self.assertRaises(ValueError):
                    mixed.authorize(method, {'id': target, 'confirm': target}, 'mixed-owned')

    def test_only_diskless_qemu_linux_creation(self):
        config = {'id': 'mixed-owned', 'backend': 'qemu-hvf', 'guest': 'linux'}
        mixed.authorize('create', config, 'mixed-owned')
        for change in [{'backend': 'apple-virtualization'}, {'guest': 'macos'}, {'disk': '/existing.img'}, {'machine_identity_dir': '/existing-identity'}]:
            with self.subTest(change=change), self.assertRaises(ValueError):
                mixed.authorize('create', config | change, 'mixed-owned')
        for method in ['start', 'shutdown']:
            mixed.authorize(method, {'id': 'mixed-owned'}, 'mixed-owned')
        with self.assertRaises(ValueError):
            mixed.authorize('force_stop', {'id': 'mixed-owned', 'confirm': 'other'}, 'mixed-owned')
        mixed.authorize('force_stop', {'id': 'mixed-owned', 'confirm': 'mixed-owned'}, 'mixed-owned')

    def test_read_only_mac_observation_is_allowed_but_no_exec_or_files(self):
        for method in mixed.READ_ONLY:
            mixed.authorize(method, {'id': 'macos-smoke'}, 'mixed-owned')
        for method in ['exec', 'file_upload_begin', 'input_step', 'stop_guest_stream_bridge']:
            with self.subTest(method=method), self.assertRaises(ValueError):
                mixed.authorize(method, {'id': 'mixed-owned'}, 'mixed-owned')


class EvidenceChecks(unittest.TestCase):
    def test_actual_fixture_decodes_and_dimensions_must_match(self):
        body = (Path(__file__).resolve().parents[1]/'browser/test/fixtures/screenshot-1000x500.jpg').read_bytes()
        self.assertEqual(mixed.decode_image(body, 1000, 500)['decoded_frames'], 1)
        with self.assertRaises(AssertionError):
            mixed.decode_image(body, 999, 500)

    def test_jpeg_magic_without_decodable_pixels_is_rejected(self):
        with self.assertRaises(AssertionError):
            mixed.decode_image(bytes.fromhex('ffd8ffd9'), 1000, 500)

    def test_previous_boot_marker_cannot_satisfy_new_boot(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'serial.log'
            ready = b'Device: virgl\nAccelerated: yes\nVEYNODE_DESKTOP_READY\n'
            path.write_bytes(ready)
            offset = path.stat().st_size
            with self.assertRaises(TimeoutError):
                mixed.wait_desktop(path, offset, timeout=.001)
            with path.open('ab') as output:
                output.write(ready)
            self.assertIn('VEYNODE_DESKTOP_READY', mixed.wait_desktop(path, offset, timeout=.1))

    def test_truncated_restart_log_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'serial.log'
            path.write_bytes(b'new')
            with self.assertRaises(AssertionError):
                mixed.read_boot_log(path, 100)


if __name__ == '__main__':
    unittest.main()
