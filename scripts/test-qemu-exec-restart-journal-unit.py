#!/usr/bin/env python3
"""Offline parser/retention tests only: no processes, signals, Guest or daemon."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import struct
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('restart', Path(__file__).with_name('test-qemu-exec-restart-journal.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


def journal():
    return {'version': 1, 'scopes': [{'vm': 'test-vm', 'worker_instance': 'a' * 64,
        'owner_sha256': 'b' * 64, 'epoch': 3}]}


class OfflineTests(unittest.TestCase):
    def test_exact_argv_spaces_and_environment_not_returned(self):
        argv = ['/fixture/vm-control', 'daemon', '--state-dir', '/fixture/a b', '--backend', '/fixture/引擎']
        raw = struct.pack('=i', len(argv)) + b'/fixture/vm-control\0\0\0' + b'\0'.join(os.fsencode(x) for x in argv) + b'\0SECRET=not-argv\0'
        self.assertEqual(m.parse_procargs(raw), argv)
        m.validate_daemon_argv(argv, Path(argv[0]), Path('/fixture/a b'))
        self.assertNotIn('SECRET', repr(m.parse_procargs(raw)))

    def test_truncated_and_oversize_argv_refused(self):
        for raw in [b'', struct.pack('=i', 200) + b'/exe\0', struct.pack('=i', 2) + b'/exe\0/exe\0no-terminator', b'x' * (1024 * 1024 + 1)]:
            with self.assertRaises(AssertionError):
                m.parse_procargs(raw)

    def test_daemon_replay_rejects_wrong_action_relative_and_duplicate_paths(self):
        good = ['/fixture/bin', 'daemon', '--state-dir', '/fixture/state', '--backend', '/fixture/native']
        variants = [good[:1] + ['mcp'] + good[2:], good + ['--state-dir', '/other'],
            good + ['--secret', 'value'], good[:-1] + ['relative/native'], good[:3] + ['/other'] + good[4:]]
        for argv in variants:
            with self.assertRaises(AssertionError):
                m.validate_daemon_argv(argv, Path('/fixture/bin'), Path('/fixture/state'))

    def test_journal_schema_excludes_command_data_and_duplicates(self):
        self.assertEqual(m.validate_journal(json.dumps(journal()).encode()), journal())
        variants = []
        for key, value in [('argv', ['secret']), ('output', 'body'), ('owner', 'c' * 64), ('job_id', 'x')]:
            item = journal(); item['scopes'][0][key] = value; variants.append(json.dumps(item).encode())
        for epoch in [True, 0, 9007199254740992]:
            item = journal(); item['scopes'][0]['epoch'] = epoch; variants.append(json.dumps(item).encode())
        item = journal(); item['scopes'] *= 2; variants.append(json.dumps(item).encode())
        variants += [b'{"version":1,"version":1,"scopes":[]}', b'{"version":2,"scopes":[]}', b' ' * 131073]
        for raw in variants:
            with self.assertRaises(AssertionError):
                m.validate_journal(raw)

    def test_same_record_must_survive_not_just_nonempty(self):
        before = journal()
        m.require_retained(before, copy.deepcopy(before))
        for value in [{'version': 1, 'scopes': []}, {'version': 1, 'scopes': before['scopes'] * 2}]:
            with self.assertRaises(AssertionError):
                m.require_retained(before, value)
        after = journal(); after['scopes'][0]['owner_sha256'] = 'c' * 64
        with self.assertRaises(AssertionError):
            m.require_retained(before, after)

    def test_real_private_file_reads_and_unsafe_objects_rejected(self):
        for kind in ['valid', 'symlink', 'hardlink', 'mode', 'fifo']:
            with tempfile.TemporaryDirectory() as temp:
                state = Path(temp); directory = state / 'exec-journal'; directory.mkdir(mode=0o700)
                path = directory / 'exec-scopes.json'
                m.private_write(directory / 'exec-scopes.required', b'v1\n')
                m.private_write(path, json.dumps(journal()).encode())
                if kind == 'symlink':
                    path.rename(directory / 'real'); path.symlink_to('real')
                elif kind == 'hardlink': os.link(path, directory / 'alias')
                elif kind == 'mode': path.chmod(0o644)
                elif kind == 'fifo': path.unlink(); os.mkfifo(path, 0o600)
                if kind == 'valid': self.assertEqual(m.read_journal(state)[0], journal())
                else:
                    with self.assertRaises((AssertionError, OSError)):
                        m.read_journal(state)


if __name__ == '__main__':
    unittest.main()
