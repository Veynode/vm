#!/usr/bin/env python3
"""Offline maintenance proof/evidence tests; no daemon or Guest is started."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('maintenance', Path(__file__).with_name('test-qemu-guest-crash-maintenance.py'))
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)


class Proof(unittest.TestCase):
    def test_terminal_exit_zero_and_guest_pass_cannot_replace_disk_proof(self):
        good = {'state': 'stopped', 'exit_code': 0}
        marker = {'pass_count': 1, 'fail_count': 0}
        m.validate_completion(True, good, marker)
        for bound, status, evidence, field in (
            (False, good, marker, 'startup_binding_observed'),
            (True, {'state': 'error', 'exit_code': 0}, marker, 'state_stopped'),
            (True, {'state': 'stopped', 'exit_code': 1}, marker, 'worker_exit_zero'),
            (True, {'state': 'stopped', 'exit_code': False}, marker, 'worker_exit_zero'),
            (True, good, {'pass_count': 0, 'fail_count': 0}, 'one_complete_pass_marker'),
            (True, good, {'pass_count': 2, 'fail_count': 0}, 'one_complete_pass_marker'),
            (True, good, {'pass_count': 1, 'fail_count': 1}, 'no_failure_marker')):
            with self.assertRaisesRegex(AssertionError, field):
                m.validate_completion(bound, status, evidence)

    def test_exact_fd_proof_required(self):
        proof = {'device': 2, 'inode': 7, 'descriptor_backed': True, 'qmp_fdsets_verified_at_start': True}
        m.validate_binding(proof, 2, 7)
        for key, value in [('device', 3), ('inode', 8), ('descriptor_backed', 1), ('qmp_fdsets_verified_at_start', False)]:
            with self.assertRaises(AssertionError):
                m.validate_binding({**proof, key: value}, 2, 7)

    def test_original_missing_proof_saved_before_failure_and_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'trace'; path.touch(mode=0o600)
            status = {'state': 'stopped', 'exit_code': 0, 'runtime': {'state': 'stopped'}}
            with self.assertRaisesRegex(AssertionError, 'startup_binding_observed'):
                m.final_proof(path, False, status, {'pass_count': 1, 'fail_count': 0}, .7)
            original = path.read_bytes()
            first = json.loads(original)
            self.assertEqual(first['status'], status)
            self.assertIs(first['bound'], False)
            self.assertEqual(first['elapsed_seconds'], .7)
            m.append_trace(path, {'phase': 'failure_cleanup', 'state': 'stopped', 'exit_code': 0})
            self.assertTrue(path.read_bytes().startswith(original))
            self.assertFalse(json.loads(path.read_text().splitlines()[0])['bound'])

    def test_transient_verified_proof_counts_but_cannot_be_invented(self):
        # Synthetic timing example only: an already observed exact startup proof
        # remains valid when a terminal event omits it. An omitted proof is never
        # reconstructed from Guest success, exit0, a path or requested config.
        p = {'device': 2, 'inode': 7, 'descriptor_backed': True, 'qmp_fdsets_verified_at_start': True}
        m.validate_binding(p, 2, 7)
        m.validate_completion(True, {'state': 'stopped', 'exit_code': 0}, {'pass_count': 1, 'fail_count': 0})
        with self.assertRaises(AssertionError):
            m.validate_completion(False, {'state': 'stopped', 'exit_code': 0}, {'pass_count': 1, 'fail_count': 0})

    def test_log_marker_requires_complete_exact_line_and_no_fail(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'serial'
            path.write_bytes(b'prefixVEYNODE_CRASH_MAINTENANCE_PASS\nVEYNODE_CRASH_MAINTENANCE_PASS')
            self.assertEqual(m.marker_evidence(path)['pass_count'], 0)
            path.write_bytes(b'VEYNODE_CRASH_MAINTENANCE_PASS\r\n')
            self.assertEqual(m.marker_evidence(path)['pass_count'], 1)
            path.write_bytes(b'VEYNODE_CRASH_MAINTENANCE_PASS\nVEYNODE_CRASH_MAINTENANCE_FAIL stage=x\n')
            self.assertEqual(m.marker_evidence(path)['fail_count'], 1)

    def test_trace_symlink_is_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / 'target'; target.touch(mode=0o600)
            link = Path(directory) / 'link'; link.symlink_to(target)
            with self.assertRaises(OSError):
                m.append_trace(link, {'event': 'test'})
            self.assertEqual(target.read_bytes(), b'')


if __name__ == '__main__':
    unittest.main()
