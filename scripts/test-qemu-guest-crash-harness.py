#!/usr/bin/env python3
"""Host-only strict crash-evidence parser tests, not a live Guest fault test."""
import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec=importlib.util.spec_from_file_location('guest_crash',Path(__file__).with_name('test-qemu-guest-crash.py'))
fixture=importlib.util.module_from_spec(spec);spec.loader.exec_module(fixture)
SHA='a'*64


def valid():
    identity={'pid':42,'start_ticks':123,'executable_inode':55,'executable_device':99,'executable_sha256':SHA}
    return [{'event':event,'data':dict(identity)} for event in
            ('watcher_ready','target_verified_before_signal','sigkill_sent_to_pinned_service')]


class Harness(unittest.TestCase):
    def test_exact_unchanged_target_sequence(self):
        self.assertEqual(fixture.crash_proof(valid(),SHA),valid()[-1])

    def test_mismatch_duplicate_refusal_missing_and_reordered_fail(self):
        cases=[valid()[:-1],valid()+[valid()[-1]],list(reversed(valid())),valid()+[{'event':'refused'}]]
        for key,value in [('pid',1),('pid',True),('start_ticks',0),('executable_sha256','b'*64)]:
            changed=valid()
            for row in changed:row['data'][key]=value
            cases.append(changed)
        changed=valid();changed[-1]['data']['pid']=43;cases.append(changed)
        changed=valid();changed[0]['data']['unknown']='secret';cases.append(changed)
        for value in cases:
            with self.assertRaises(AssertionError):fixture.crash_proof(value,SHA)

    def test_complete_records_only_and_no_arbitrary_boot_text_is_proof(self):
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'serial'
            rows=valid()
            wire=b'noise: sigkill happened\n'+b''.join(b'VEYNODE_GUEST_CRASH '+json.dumps(v).encode()+b'\r\n' for v in rows)
            path.write_bytes(wire+b'VEYNODE_GUEST_CRASH {"event":')
            self.assertEqual(fixture.events(path),rows)
            path.write_bytes(b'noise: sigkill happened\n')
            self.assertEqual(fixture.events(path),[])

    def test_prompt_prefix_still_rejected_but_explicit_newline_is_valid(self):
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'serial'
            row=b'VEYNODE_GUEST_CRASH '+json.dumps(valid()[1]).encode()+b'\n'
            path.write_bytes(b'veynode-desktop login: '+row)
            self.assertEqual(fixture.events(path),[])
            path.write_bytes(b'veynode-desktop login: \n'+row)
            self.assertEqual(fixture.events(path),[valid()[1]])


if __name__=='__main__':unittest.main(verbosity=2)
