#!/usr/bin/env python3
"""Offline classification guards, not Vulkan/Guest acceptance."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('probe', Path(__file__).with_name('linux-vulkan-probe.py'))
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


class Classification(unittest.TestCase):
    def test_first_blob_failure_preserved_before_cascading_guest_oom(self):
        text = '\n'.join(['virgl blob create error: Operation not permitted',
                          'virtio_gpu_virgl_process_cmd: ctrl 0x10c, error 0x1200',
                          'vkCreateInstance failed with ERROR_OUT_OF_HOST_MEMORY',
                          'VULKAN_INSTANCE_EXIT 1'])
        result = probe.classify(text, 0)
        self.assertFalse(result['initialization_succeeded'])
        self.assertEqual(result['first_backend_error'], 'virgl blob create error: Operation not permitted')
        self.assertEqual(result['instance_exit'], 1)

    def test_proxy_context_failure_precedes_blob_failure(self):
        result = probe.classify('proxy: failed to send message: Broken pipe\n'
                                'proxy: failed to create a context\n'
                                'virgl blob create error: Invalid argument', 0)
        self.assertEqual(result['first_backend_error'], 'proxy: failed to send message: Broken pipe')
        self.assertFalse(result['initialization_succeeded'])

    def test_software_and_incomplete_output_cannot_succeed(self):
        for text, code in [('deviceName = llvmpipe\nVULKAN_INSTANCE_EXIT 0', 0),
                           ('deviceName = lavapipe\nVULKAN_INSTANCE_EXIT 0', 0),
                           ('deviceName = Apple\nVULKAN_INSTANCE_EXIT 0', -9),
                           ('deviceName = Apple', 0),
                           ('VULKAN_SETUP_FAIL packages\ndeviceName = Apple\nVULKAN_INSTANCE_EXIT 0', 0),
                           ('deviceName = Apple\nVULKAN_INSTANCE_EXIT 0\nVULKAN_INSTANCE_EXIT 0', 0)]:
            with self.subTest(text=text, code=code):
                self.assertFalse(probe.classify(text, code)['initialization_succeeded'])

    def test_initialization_never_implies_graphics(self):
        result = probe.classify('deviceName = Apple M1 Pro\nVULKAN_INSTANCE_EXIT 0', 0)
        self.assertTrue(result['initialization_succeeded'])
        self.assertFalse(result['graphics_application_validated'])
        self.assertFalse(result['performance_measured'])

    def test_diagnostics_are_bounded(self):
        result = probe.classify('\n'.join('virtio ctrl 0x10c' for _ in range(2000)), 1)
        self.assertEqual(len(result['backend_errors']), 32)

    def test_raster_failures_cannot_be_hidden_by_instance_success(self):
        good = '\n'.join([f'VK_PIXEL_CASE frame={i} tested=3522 mismatches=0' for i in range(3)] +
                         ['VK_PIXEL_PASS frames=3 size=64x64'])
        for text in [good.replace('mismatches=0', 'mismatches=1', 1),
                     good.replace('frame=1', 'frame=0'),
                     good.replace('tested=3522', 'tested=10', 1),
                     good.replace('VK_PIXEL_PASS frames=3 size=64x64', ''),
                     good + '\nVK_PIXEL_PASS frames=3 size=64x64']:
            with self.subTest(text=text):
                result = probe.classify('deviceName = Apple\nVULKAN_INSTANCE_EXIT 0', 0)
                self.assertFalse(probe.classify_pixels(text, result)['raster_pixel_oracle_passed'])
        result = probe.classify('deviceName = Apple\nVULKAN_INSTANCE_EXIT 0', 0)
        self.assertTrue(probe.classify_pixels(good, result)['raster_pixel_oracle_passed'])
        self.assertFalse(result['graphics_application_validated'])


if __name__ == '__main__':
    unittest.main()
