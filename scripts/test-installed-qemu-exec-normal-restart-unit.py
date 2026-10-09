#!/usr/bin/env python3
"""Offline oracle tests only. Never invokes the actual Guest harness main()."""
import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('normal_restart', Path(__file__).with_name('test-installed-qemu-exec-normal-restart.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


def identity(letter, inode):
    return {'vm': 'restart-test', 'worker_instance': letter * 64, 'device': 1, 'inode': inode}


def snapshot(completed, launches=None, scopes=None):
    return {'version': 2, 'launches': launches or [], 'scopes': scopes or [], 'completed': completed}


class Oracles(unittest.TestCase):
    def setUp(self):
        self.one, self.two = identity('a', 10), identity('b', 11)

    def test_explicit_v3_accepts_collected_inode_reuse_not_instance_reuse(self):
        same_inode={**self.two,'inode':self.one['inode']}
        m.distinct_witnesses(self.one,same_inode,3)
        with self.assertRaises(AssertionError):m.distinct_witnesses(self.one,same_inode)
        for bad in [self.one,{**same_inode,'vm':'other'},{**same_inode,'inode':True}]:
            with self.assertRaises(AssertionError):m.distinct_witnesses(self.one,bad,3)
        value={**snapshot([self.one,same_inode]),'version':3}
        m.journal_matches(value,[],[self.one,same_inode],version=3)
        with self.assertRaises(AssertionError):m.journal_matches(value,[],[self.one,same_inode])
        with self.assertRaises(AssertionError):m.journal_matches(value,[],[same_inode,self.one],version=3)

    def test_v3_exit_proof_still_rejects_pending_or_unconfirmed(self):
        pins=[{'pid':i,'uid':1,'start_sec':20,'start_usec':30} for i in (100,101,102)]
        status={'state':'stopped','exit_code':0}
        done={**snapshot([self.one]),'version':3}
        m.normal_exit_proof(status,done,[self.one],pins,[None]*3,3)
        for bad in [{**status,'execution_reconciliation_status':'unconfirmed'},{**status,'state':'stopping'}]:
            with self.assertRaises(AssertionError):m.normal_exit_proof(bad,done,[self.one],pins,[None]*3,3)
        with self.assertRaises(AssertionError):m.normal_exit_proof(status,{**snapshot([],[self.one]),'version':3},[self.one],pins,[None]*3,3)

    def test_exact_two_completed_not_merely_counts(self):
        good = snapshot([self.two, self.one])
        m.journal_matches(good, [], [self.one, self.two])
        for wrong in (identity('c', 11), {**self.two, 'vm': 'another'}, {**self.two, 'inode': 12}):
            with self.assertRaises(AssertionError):
                m.journal_matches(snapshot([self.one, wrong]), [], [self.one, self.two])
        for wrong in (snapshot([self.one]), snapshot([self.one, self.one]), snapshot([], [self.one, self.two])):
            with self.assertRaises(AssertionError):
                m.journal_matches(wrong, [], [self.one, self.two])

    def test_remaining_scope_or_pending_launch_not_completed(self):
        scope = {'vm': 'restart-test', 'worker_instance': 'b'*64, 'owner_sha256': 'c'*64, 'epoch': 1}
        with self.assertRaises(AssertionError):
            m.journal_matches(snapshot([self.one], [self.two], [scope]), [], [self.one, self.two])

    def test_boot_and_witness_reuse_rejected(self):
        m.distinct_boots({'boot': {'boot_id': 'one'}}, {'boot': {'boot_id': 'two'}})
        with self.assertRaises(AssertionError):
            m.distinct_boots({'boot': {'boot_id': 'one'}}, {'boot': {'boot_id': 'one'}})
        m.distinct_witnesses(self.one, self.two)
        for wrong in (self.one, {**self.two, 'inode': 10}, {**self.two, 'vm': 'another'}):
            with self.assertRaises(AssertionError):
                m.distinct_witnesses(self.one, wrong)

    def test_exit_zero_alone_never_proves_completion(self):
        pins = [{'pid': i, 'uid': 1, 'start_sec': 20, 'start_usec': 30} for i in (100,101,102)]
        status = {'state': 'stopped', 'exit_code': 0}
        journal = snapshot([self.one])
        m.normal_exit_proof(status, journal, [self.one], pins, [None]*3)
        for bad in ({**status, 'exit_code': False}, {**status, 'exit_code': 1},
                    {**status, 'state': 'crashed'}, {**status, 'execution_reconciliation_required': True},
                    {**status, 'execution_reconciliation_status': 'unconfirmed'},
                    {**status, 'execution_recovery_required': True}):
            with self.assertRaises(AssertionError):
                m.normal_exit_proof(bad, journal, [self.one], pins, [None]*3)
        with self.assertRaises(AssertionError):
            m.normal_exit_proof(status, snapshot([], [self.one]), [self.one], pins, [None]*3)
        with self.assertRaises(AssertionError):
            m.normal_exit_proof(status, journal, [self.one], pins, [None, None, pins[2]])

    def test_pending_terminal_saved_before_failure_and_not_promoted_by_cleanup(self):
        pins = [{'pid': i, 'uid': 1, 'start_sec': 20, 'start_usec': 30} for i in (100,101,102)]
        pending = {'state': 'stopped', 'exit_code': 0, 'execution_reconciliation_required': True}
        journal = snapshot([], [self.one])
        with tempfile.TemporaryDirectory() as directory:
            trace = Path(directory) / 'shutdown.jsonl'
            trace.touch(mode=0o600)
            with self.assertRaisesRegex(AssertionError, 'automatic normal termination unconfirmed'):
                m.evaluate_shutdown_observation(trace, pending, journal, [self.one], pins, [None]*3, 1.25)
            preserved = trace.read_bytes()
            record = json.loads(preserved)
            self.assertEqual(record['status'], pending)
            self.assertEqual(record['journal'], journal)
            self.assertEqual(record['elapsed_seconds'], 1.25)
            self.assertEqual(record['process_observations'], [None]*3)
            # Simulate a later finally observation. It cannot rewrite the first
            # failed proof or make that failed invocation return success.
            pending.pop('execution_reconciliation_required')
            m.append_shutdown_observation(trace, {'event': 'later_cleanup', 'status': pending,
                                                  'journal': snapshot([self.one])})
            self.assertTrue(trace.read_bytes().startswith(preserved))
            self.assertTrue(json.loads(trace.read_text().splitlines()[0])['status']['execution_reconciliation_required'])

    def test_complete_journal_with_unconfirmed_status_still_fails_and_is_saved(self):
        pins = [{'pid': i, 'uid': 1, 'start_sec': 20, 'start_usec': 30} for i in (100,101,102)]
        with tempfile.TemporaryDirectory() as directory:
            trace = Path(directory) / 'shutdown.jsonl'; trace.touch(mode=0o600)
            bad = {'state': 'stopped', 'exit_code': 0, 'execution_reconciliation_status': 'unconfirmed'}
            with self.assertRaises(AssertionError):
                m.evaluate_shutdown_observation(trace, bad, snapshot([self.one]), [self.one], pins, [None]*3, 2)
            self.assertEqual(json.loads(trace.read_text())['status'], bad)

    def test_generic_error_not_stale_authority_proof(self):
        def reply(text, failed=True):
            return {'result': {'isError': failed, 'content': [{'type': 'text', 'text': text}]}}
        m.authority_denied(reply('control lease expired, revoked or belongs to another client'))
        for bad in (reply('unsupported'), reply('serial execution unavailable or fenced'),
                    reply('control lease expired, revoked or belongs to another client', False),
                    {'error': {'code': -32602, 'message': 'Invalid tool arguments'}}):
            with self.assertRaises(AssertionError):
                m.authority_denied(bad)

    def test_real_private_journal_read_and_replacement_detected(self):
        with tempfile.TemporaryDirectory() as directory:
            state = Path(directory)
            store = state / 'exec-journal'
            store.mkdir(mode=0o700)
            for file, data in [('exec-scopes.required', b'v1\n'),
                               ('exec-scopes.json', json.dumps(snapshot([self.one, self.two])).encode())]:
                (store / file).write_bytes(data)
                (store / file).chmod(0o600)
            journal, _ = m.witness.read_journal(state)
            m.journal_matches(journal, [], [self.one, self.two])
            path = store / 'exec-scopes.json'
            original = store / 'original'
            path.rename(original)
            path.symlink_to(original)
            with self.assertRaises(OSError):
                m.witness.read_journal(state)

    def test_fifteen_pixels_gate_not_readiness_only(self):
        boot = {'boot_id': 'b'*8+'-'+ 'b'*4+'-'+ 'b'*4+'-'+ 'b'*4+'-'+ 'b'*12,
                'root_device': '/dev/vda', 'root_fstype': 'ext4'}
        cases = [{'name': 'case'+str(i), 'passed': True, 'mismatched_pixels': 0} for i in range(15)]
        def raw(cs):
            return ('VEYNODE_INSTALLED_BOOT '+json.dumps(boot)+'\n'+
                    ''.join('XRENDER_CASE '+json.dumps(c)+'\n' for c in cs)+
                    'XRENDER_SUMMARY {"cases":15,"failed":0,"passed":true}\nVEYNODE_INSTALLED_GRAPHICS_READY=15\n').encode()
        self.assertIsNotNone(m.base.desktop.boot_evidence(raw(cases)))
        bad = copy.deepcopy(cases); bad[0]['mismatched_pixels'] = 1
        for cs in (bad, cases[:-1], cases+[cases[0]]):
            with self.assertRaises(AssertionError):
                m.base.desktop.boot_evidence(raw(cs))


if __name__ == '__main__':
    unittest.main()
