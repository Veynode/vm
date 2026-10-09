#!/usr/bin/env python3
"""Acceptance-harness logic tests with protocol doubles, NOT Guest acceptance."""
import base64
import copy
import hashlib
import importlib.util
import pathlib
import unittest

spec = importlib.util.spec_from_file_location('probe', pathlib.Path(__file__).with_name('probe-browser-file-backend.py'))
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


def receipt():
    return {'result': {'closed': True, 'retired_owner': True, 'retired_owner_ttl_seconds': 120,
                       'job_cancellation': {'attempted': 0, 'unconfirmed_requests': 0, 'outcomes': []},
                       'file_cancellation': {'attempted': 1, 'unconfirmed_requests': 0,
                                             'outcomes': [{'result': {'result': {'cancelled_transfers': 0}}}]}}}


class DownloadFixture:
    def __init__(self, body, mutation=None):
        self.body = body
        self.mutation = mutation
        self.calls = 0

    def guest(self, method, owner, **args):
        digest = hashlib.sha256(self.body).hexdigest()
        if method == 'file_download_begin':
            return {'size': len(self.body), 'sha256': digest, 'chunk_bytes': probe.CHUNK, 'token': 'test'}
        self.calls += 1
        offset = args['offset']
        part = self.body[offset:offset + probe.CHUNK]
        value = {'data': base64.b64encode(part).decode(), 'offset': offset + len(part),
                 'sha256': digest, 'eof': offset + len(part) == len(self.body)}
        if self.mutation:
            self.mutation(value)
        return value


class ProbeTests(unittest.TestCase):
    def test_exact_byte_and_hash_verification_for_full_ff_chunk(self):
        body = bytes([255]) * probe.CHUNK
        fixture = DownloadFixture(body)
        report = {}
        probe.download_verified(fixture, 'test-owner', 'test-file', body, report)
        self.assertTrue(report['byte_comparison_passed'])
        self.assertEqual(report['download_chunks'], 1)
        self.assertEqual(report['downloaded_sha256'], hashlib.sha256(body).hexdigest())

    def test_corruption_short_chunks_and_forged_metadata_are_rejected(self):
        body = bytes([255]) * probe.CHUNK
        mutations = [
            lambda v: v.update(data=base64.b64encode(bytes(probe.CHUNK)).decode()),
            lambda v: v.update(data=v['data'][:-1]),
            lambda v: v.update(offset=True),
            lambda v: v.update(eof=1),
            lambda v: v.update(eof=False),
            lambda v: v.update(sha256='0' * 64),
            lambda v: v.update(data=base64.b64encode(body[:100]).decode(), offset=100, eof=True),
        ]
        for mutate in mutations:
            with self.subTest(mutate=mutate), self.assertRaises(probe.CheckFailure):
                probe.download_verified(DownloadFixture(body, mutate), 'test-owner', 'test-file', body, {})

    def test_multi_chunk_download_requires_exact_final_eof(self):
        body = bytes(probe.CHUNK) + bytes([255]) * probe.CHUNK
        report = {}
        fixture = DownloadFixture(body)
        probe.download_verified(fixture, 'test-owner', 'test-file', body, report)
        self.assertEqual(fixture.calls, 2)
        self.assertEqual(report['downloaded_bytes'], len(body))

    def test_cleanup_requires_numeric_ack_and_no_other_authority(self):
        self.assertEqual(probe.validate_cleanup(receipt()), 0)
        mutations = [
            lambda r: r['result'].update(closed=1),
            lambda r: r['result']['file_cancellation'].update(attempted=True),
            lambda r: r['result']['file_cancellation'].update(unconfirmed_requests=1),
            lambda r: r['result']['file_cancellation']['outcomes'][0]['result']['result'].update(cancelled_transfers=True),
            lambda r: r['result']['file_cancellation']['outcomes'][0].update(error='no receipt'),
            lambda r: r['result'].update(input_release={'confirmed': True}),
            lambda r: r['result']['job_cancellation'].update(attempted=1),
        ]
        for mutate in mutations:
            item = copy.deepcopy(receipt())
            mutate(item)
            with self.subTest(mutate=mutate), self.assertRaises(probe.CheckFailure):
                probe.validate_cleanup(item)


if __name__ == '__main__':
    unittest.main()
