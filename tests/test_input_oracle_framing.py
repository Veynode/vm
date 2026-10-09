#!/usr/bin/env python3
"""Regress the actual input harness failure from reading concurrently appended UART."""
import importlib.util
import json
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('input_harness', Path(__file__).resolve().parents[1] / 'scripts/test-control-input.py')
harness = importlib.util.module_from_spec(spec)
spec.loader.exec_module(harness)

class Framing(unittest.TestCase):
    def test_every_partial_read_preserves_complete_records_only(self):
        records = ['boot output\n', 'X_INPUT {"type":4,"button":1}\n',
                   'RAW_INPUT {"type":1,"code":272,"value":1}\n',
                   'X_INPUT {"type":5,"button":1}\n']
        data = ''.join(records)
        for length in range(len(data) + 1):
            with self.subTest(length=length):
                x, raw = harness.parse_events(data[:length])
                self.assertEqual(len(x), sum(data[:length].count(record) for record in [records[1], records[3]]))
                self.assertEqual(len(raw), data[:length].count(records[2]))

    def test_malformed_complete_record_is_not_ignored(self):
        with self.assertRaises(json.JSONDecodeError):
            harness.parse_events('X_INPUT {"type":\n')

    def test_crlf_and_unterminated_valid_json(self):
        self.assertEqual(harness.parse_events('X_INPUT {"type":5}\r\n'), ([{'type':5}], []))
        self.assertEqual(harness.parse_events('X_INPUT {"type":5}'), ([], []))

if __name__ == '__main__':
    unittest.main()
