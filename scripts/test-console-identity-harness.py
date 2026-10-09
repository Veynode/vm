#!/usr/bin/env python3
"""Synthetic negative tests of the macOS identity acceptance script itself.

No VM, real Guest service, permissions, screen capture or desktop input is used.
The real acceptance script runs against an owned fake MCP child executable. Its
reports live only in each temporary test directory and are never VM evidence.
"""
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shlex
import signal
import struct
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
TARGET = ROOT / 'scripts/test-macos-console-identity.py'
JPEG = ROOT / 'browser/test/fixtures/screenshot-1000x500.jpg'
SENTINEL = 'DO_NOT_PERSIST_GUEST_IMAGE_OR_CREDENTIAL_aec31'


def fake_mcp(argv):
    """Only called in a child explicitly launched by this harness's private shim."""
    assert argv[0] == 'mcp'
    state = Path(argv[argv.index('--state-dir') + 1])
    config = json.loads((state / 'config.json').read_text())
    (state / 'pid').write_text(str(os.getpid()))
    # Independent failsafe; a harness regression cannot leave a permanent child.
    signal.alarm(25)
    scenario = config['scenario']
    width, height, display = config['width'], config['height'], 1
    bounds = {'x': 0.0, 'y': 0.0, 'width': float(width), 'height': float(height)}
    canonical = b'Veynode/GuestDisplay/v1\0' + struct.pack('>III', display, width, height) + struct.pack('>dddd', *bounds.values())
    identity = {'version': 1, 'security_session_id': 42, 'uid': 501, 'console_set': None,
                'display_id': display, 'physical_width': width, 'physical_height': height,
                'display_geometry_sha256': hashlib.sha256(canonical).hexdigest()}
    status = {'identity_eligible': True, 'identity_rejection': None, 'probe_grants_input': False,
              'lock_state': 'unknown', 'transition_race_excluded': False}
    guest = {'os': 'darwin', 'machine': 'VirtualMac-fixture-only', 'console_identity': identity,
             'console_identity_status': status, 'capabilities': {'screenshot': True, 'input': False},
             'input_policy': {'enabled': False, 'available': False}}
    data = base64.b64decode(config['image'])
    frame = {'frame_id': '12345678-1234-4234-8234-123456789abc', 'width': width, 'height': height,
             'source_width': width, 'source_height': height, 'display_id': display,
             'console_identity': identity, 'desktop_bounds_points': bounds,
             'console_identity_verified_at': 'capture_pre_enumeration_and_post_encode',
             'point_pixel_scale': 1, 'source_generation': 1, 'input_anchor_available': False,
             'captured_at_us': 1, 'sha256': hashlib.sha256(data).hexdigest(), 'byte_length': len(data)}
    if scenario.startswith('observer'):
        frame.pop('source_generation'); frame.pop('input_anchor_available')
    if scenario == 'observer-anchor': frame['source_generation'] = 1
    if scenario == 'observer-input': guest['capabilities']['input'] = True
    for line in sys.stdin:
        request = json.loads(line)
        if 'id' not in request:
            continue
        if request['method'] == 'initialize':
            if scenario == 'malicious-initialize':
                print(json.dumps({'jsonrpc': '2.0', 'id': request['id'] + 1,
                                  'error': {'code': -32603, 'message': SENTINEL},
                                  'result': {'content': [{'type': 'image', 'data': SENTINEL}]}}), flush=True)
                continue
            result = {'protocolVersion': '2025-06-18', 'capabilities': {}, 'serverInfo': {'name': 'synthetic-harness', 'version': '1'}}
        elif request['params']['name'] == 'vm_guest_info':
            result = {'content': [{'type': 'text', 'text': json.dumps({'result': guest})}]}
        elif request['params']['name'] == 'vm_screenshot':
            result = {'content': [{'type': 'image', 'mimeType': 'image/jpeg', 'data': config['image']},
                                  {'type': 'text', 'text': json.dumps(frame)}]}
        else:
            raise AssertionError('unexpected_fixture_method')
        print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}), flush=True)
    (state / 'eof').write_text('stdin EOF observed')
    return 7 if scenario == 'nonzero-exit' else 0


def load_target():
    spec = importlib.util.spec_from_file_location('identity_acceptance_under_test', TARGET)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class IdentityHarness(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.target = load_target()
        cls.jpeg = JPEG.read_bytes()

    def run_script(self, scenario='valid', data=None, width=1000, height=500, observer_only=False):
        with tempfile.TemporaryDirectory(prefix='identity-harness-') as directory:
            root = Path(directory)
            config = {'scenario': scenario, 'width': width, 'height': height,
                      'image': base64.b64encode(self.jpeg if data is None else data).decode()}
            (root / 'config.json').write_text(json.dumps(config))
            binary = root / 'synthetic-mcp'
            binary.write_text('#!/bin/sh\nexec ' + shlex.quote(sys.executable) + ' ' + shlex.quote(str(Path(__file__).resolve())) + ' --fake-mcp "$@"\n')
            binary.chmod(0o700)
            output = root / 'report.json'
            process = subprocess.Popen([sys.executable, str(TARGET), '--binary', str(binary),
                                        '--state-dir', str(root), '--id', 'synthetic-only', '--output', str(output)] + (['--observer-only'] if observer_only else []),
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
            try:
                stdout, stderr = process.communicate(timeout=18)
                self.assertTrue(output.is_file(), 'Acceptance script must preserve a failure report')
                encoded = output.read_bytes()
                self.assertNotIn(SENTINEL.encode(), encoded + stdout + stderr, 'Raw initialize reply leaked')
                report = json.loads(encoded)
                self.assertEqual(process.returncode, 0 if report['passed'] else 1)
                self.assertTrue((root / 'eof').is_file(), 'Owned fake child did not receive cleanup EOF')
                pid = int((root / 'pid').read_text())
                with self.assertRaises(ProcessLookupError, msg='Owned MCP process remains alive or unreaped'):
                    os.kill(pid, 0)
                return report
            finally:
                # Only the fresh, owned process group is eligible for cleanup.
                # Never inspect, signal or interact with a real daemon or VM.
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait(timeout=3)

    def test_positive_fixture_and_sof_independent_dimensions(self):
        self.assertEqual(self.target.jpeg_dimensions(self.jpeg), (1000, 500))
        report = self.run_script()
        self.assertTrue(report['passed'])
        self.assertEqual(report['frame']['width'], 1000)
        self.assertFalse(report['input_performed'])
        self.assertFalse(report['browser_binding_validated'])

    def test_observer_contract_is_explicit_and_default_still_requires_anchors(self):
        report = self.run_script('observer', observer_only=True)
        self.assertTrue(report['passed'])
        self.assertEqual(report['screenshot_contract'], 'observer_only_no_input_anchor')
        self.assertNotIn('source_generation', report['frame'])
        self.assertNotIn('input_anchor_available', report['frame'])
        default = self.run_script('observer')
        self.assertFalse(default['passed'])
        self.assertEqual(default['error'], 'required_input_anchor_fields_missing')

    def test_observer_rejects_present_anchor_even_without_input_authority(self):
        for scenario in ['valid', 'observer-anchor']:
            with self.subTest(scenario=scenario):
                report = self.run_script(scenario, observer_only=True)
                self.assertFalse(report['passed'])
                self.assertEqual(report['error'], 'observer_input_anchor_fields_present')

    def test_observer_rejects_enabled_input_and_preserves_geometry_validation(self):
        enabled = self.run_script('observer-input', observer_only=True)
        self.assertFalse(enabled['passed'])
        self.assertEqual(enabled['error'], 'observer_input_capability_enabled')
        geometry = self.run_script('observer', observer_only=True, width=999)
        self.assertFalse(geometry['passed'])
        self.assertEqual(geometry['error'], 'jpeg_encoded_dimensions_mismatch')

    def test_encoded_dimensions_mismatch_fails_despite_matching_identity_and_hash(self):
        report = self.run_script(width=999)
        self.assertFalse(report['passed'])
        self.assertEqual(report['error'], 'jpeg_encoded_dimensions_mismatch')

    def test_missing_eoi_fails_despite_matching_length_and_hash(self):
        report = self.run_script(data=self.jpeg[:-2])
        self.assertFalse(report['passed'])
        self.assertEqual(report['error'], 'invalid_jpeg_envelope')

    def test_segment_overrun_fails_despite_matching_length_and_hash(self):
        report = self.run_script(data=b'\xff\xd8\xff\xe0\xff\xff\x00\xff\xd9')
        self.assertFalse(report['passed'])
        self.assertEqual(report['error'], 'invalid_jpeg_segment_length')

    def test_malicious_initialize_reply_is_redacted_and_constructor_child_reaped(self):
        report = self.run_script('malicious-initialize')
        self.assertFalse(report['passed'])
        self.assertEqual(report['error'], 'AssertionError')
        self.assertNotIn('before', report)

    def test_nonzero_mcp_exit_invalidates_otherwise_successful_observation(self):
        report = self.run_script('nonzero-exit')
        self.assertIn('after', report, 'Fixture must reach successful info/capture before its exit failure')
        self.assertFalse(report['passed'])
        self.assertEqual(report['cleanup_error'], 'mcp_close_failed')

    def test_jpeg_parser_rejects_invalid_structure_with_bounded_segments(self):
        def segment(marker, payload):
            return bytes([0xff, marker]) + struct.pack('>H', len(payload) + 2) + payload
        sof = segment(0xc0, struct.pack('>BHHB', 8, 500, 1000, 1) + b'\x01\x11\x00')
        sos = segment(0xda, b'\x01\x01\x00\x00\x3f\x00')
        # Header structure only. These synthesized records are not pixel images.
        for name, body in {
            'missing SOI': b'\x00\xd8' + sof + sos + b'\xff\xd9',
            'missing EOI': b'\xff\xd8' + sof + sos,
            'zero segment': b'\xff\xd8\xff\xe0\x00\x00\xff\xd9',
            'short segment': b'\xff\xd8\xff\xe0\x00\x01\xff\xd9',
            'segment overrun': b'\xff\xd8\xff\xe0\xff\xff\xff\xd9',
            'SOS without SOF': b'\xff\xd8' + sos + b'\xff\xd9',
            'duplicate SOF': b'\xff\xd8' + sof + sof + sos + b'\xff\xd9',
            'bad SOF component layout': b'\xff\xd8' + segment(0xc0, struct.pack('>BHHB', 8, 500, 1000, 2) + b'\x01\x11\x00') + sos + b'\xff\xd9',
            'empty dimensions': b'\xff\xd8' + segment(0xc0, struct.pack('>BHHB', 8, 0, 1000, 1) + b'\x01\x11\x00') + sos + b'\xff\xd9',
            'marker count bound': b'\xff\xd8' + segment(0xe0, b'') * 257 + sof + sos + b'\xff\xd9',
        }.items():
            with self.subTest(name=name):
                with self.assertRaises(AssertionError):
                    self.target.jpeg_dimensions(body)


if __name__ == '__main__':
    if len(sys.argv) > 1 and sys.argv[1] == '--fake-mcp':
        sys.exit(fake_mcp(sys.argv[2:]))
    print('SYNTHETIC ACCEPTANCE-HARNESS TESTS; NO REAL VM VALIDATION', flush=True)
    unittest.main(verbosity=2)
