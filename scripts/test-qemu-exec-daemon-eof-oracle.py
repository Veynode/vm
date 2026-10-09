#!/usr/bin/env python3
"""Pure oracle tests: no process query, signals, subprocesses or VM startup."""
import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('daemon_eof', Path(__file__).with_name('test-qemu-exec-daemon-eof.py'))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def identities():
    return {name: {'pid': index + 1000, 'uid': 501, 'start_sec': 100, 'start_usec': index,
        'state': 4 if name == 'worker' else 2, 'executable': '/fixture/bin'}
        for index, name in enumerate(('daemon', 'worker', 'guardian', 'backend'))}


class OracleTests(unittest.TestCase):
    def test_fast_group_exit_is_retirement_not_guardian_timeout(self):
        original = identities()
        oracle = module.RetirementOracle(original)
        first = copy.deepcopy(original)
        first['daemon'] = None
        self.assertFalse(oracle.observe(first)['all_vm_absent_twice'])
        gone = dict.fromkeys(original)
        self.assertFalse(oracle.observe(gone)['all_vm_absent_twice'])
        last = oracle.observe(gone)
        self.assertTrue(last['all_vm_absent_twice'])
        self.assertTrue(last['backend_absent_twice'])
        self.assertFalse(last['resume_worker'])
        self.assertNotIn('guardian_timeout_proven', last)

    def test_stopped_worker_may_resume_only_after_daemon_and_backend_end(self):
        original = identities()
        oracle = module.RetirementOracle(original)
        values = copy.deepcopy(original)
        values['daemon'] = None
        values['backend'] = None
        values['guardian']['state'] = 5
        self.assertFalse(oracle.observe(values)['resume_worker'])
        self.assertTrue(oracle.observe(values)['resume_worker'])
        values['worker']['state'] = 2
        self.assertFalse(oracle.observe(values)['resume_worker'])
        self.assertFalse(oracle.observe(values)['all_vm_absent_twice'])
        gone = dict.fromkeys(original)
        oracle.observe(gone)
        self.assertTrue(oracle.observe(gone)['all_vm_absent_twice'])

    def test_zombie_is_not_reaped_and_one_absence_is_insufficient(self):
        original = identities()
        oracle = module.RetirementOracle(original)
        zombie = {name: {**value, 'state': 5} for name, value in original.items()}
        for _ in range(3):
            decision = oracle.observe(zombie)
            self.assertFalse(decision['backend_absent_twice'])
            self.assertFalse(decision['all_vm_absent_twice'])
        gone = dict.fromkeys(original)
        self.assertFalse(oracle.observe(gone)['all_vm_absent_twice'])

    def test_reused_pid_or_missing_sample_never_proves_retirement(self):
        for key in ('pid', 'uid', 'start_sec', 'start_usec'):
            original = identities()
            oracle = module.RetirementOracle(original)
            changed = copy.deepcopy(original)
            changed['backend'][key] += 1
            with self.assertRaisesRegex(AssertionError, 'PID was reused'):
                oracle.observe(changed)
        oracle = module.RetirementOracle(identities())
        with self.assertRaisesRegex(AssertionError, 'incomplete'):
            oracle.observe({'worker': None})

    def test_daemon_alive_cannot_authorize_resume(self):
        original = identities()
        oracle = module.RetirementOracle(original)
        values = copy.deepcopy(original)
        values['backend'] = None
        oracle.observe(values)
        result = oracle.observe(values)
        self.assertTrue(result['backend_absent_twice'])
        self.assertFalse(result['daemon_ended'])
        self.assertFalse(result['resume_worker'])


if __name__ == '__main__':
    unittest.main()
