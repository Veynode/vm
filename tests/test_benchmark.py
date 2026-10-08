import copy
import importlib.util
import json
import pathlib
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('bench', pathlib.Path(__file__).parents[1]/'scripts/summarize-benchmark.py')
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)


def manifest():
    # Deliberately synthetic validator fixture, never product performance evidence.
    return {
        'implementation': {'name': 'unit-test-only', 'version': 'test', 'build': 'test'},
        'host': {'model': 'fixture', 'chip': 'fixture', 'ram_bytes': 16*1024**3,
                 'os_build': 'test', 'power_mode': 'test', 'power_source': 'test', 'thermal_state': 'test'},
        'guest': {'os': 'fixture', 'build': 'test'},
        'application': {'name': 'fixture', 'version': 'test', 'workload': 'test', 'architecture': 'arm64'},
        'resources': {'vcpus': 2, 'memory_bytes': 1024**3},
        'display': {'width': 2560, 'height': 1440, 'refresh_hz': 60, 'scale': 1},
        'measurement': {'tool': 'fixture', 'version': 'test', 'frame_method': 'test',
                        'input_method': 'test', 'resource_method': 'test', 'process_scope': ['worker', 'helper']},
        'scenario': 'local', 'background': [], 'network': {'path': 'local'},
        'sampling': {'warmup_seconds': 10, 'duration_seconds': 60},
    }


def remote(m):
    m['scenario'] = 'remote'
    m['network'] = {'path': 'forced-turn', 'client_hardware': 'test', 'browser_version': 'test',
                    'codec': 'h264', 'rate_policy': 'test', 'network_shaping': 'test',
                    'turn_provider': 'test', 'turn_region': 'test', 'rtt_ms': 20, 'jitter_ms': 1,
                    'loss_percent': 0, 'bandwidth_mbps': 50,
                    'selected_candidate_pair': {'local_type': 'relay', 'remote_type': 'host'}}


class BenchTests(unittest.TestCase):
    def summarize(self, rows, m=None):
        with tempfile.TemporaryDirectory() as d:
            p = pathlib.Path(d)
            (p/'manifest.json').write_text(json.dumps(m or manifest()))
            (p/'samples.jsonl').write_text(''.join(json.dumps(row)+'\n' for row in rows))
            return bench.summarize(p)

    def test_tail_and_mismatch(self):
        self.assertEqual(bench.percentile([10, 10, 10, 100], .5), 10)
        a = {'manifest': manifest()}
        b = copy.deepcopy(a)
        b['manifest']['application']['architecture'] = 'x86_64-translated'
        with self.assertRaisesRegex(ValueError, 'application'):
            bench.compare(a, b)
        b = copy.deepcopy(a)
        b['manifest']['resources']['memory_bytes'] *= 2
        with self.assertRaisesRegex(ValueError, 'resources'):
            bench.compare(a, b)
        b = copy.deepcopy(a)
        remote(b['manifest'])
        with self.assertRaisesRegex(ValueError, 'scenario'):
            bench.compare(a, b)

    def test_empty_metadata_cannot_match(self):
        m = manifest()
        m['host'] = {}
        with self.assertRaisesRegex(ValueError, 'host'):
            bench.compare({'manifest': m}, {'manifest': m})
        del m['implementation']
        with self.assertRaisesRegex(ValueError, 'implementation'):
            bench.validate_manifest(m)

    def test_duplicate_frame_timestamps_remain_rejected(self):
        with self.assertRaisesRegex(ValueError, 'increasing'):
            self.summarize([{'kind': 'frame', 'present_time_ns': 1}]*2)

    def test_missing_category_is_not_zero(self):
        report = self.summarize([])
        self.assertEqual(report['frame_interval_ms']['samples'], 0)
        self.assertIsNone(report['host_cpu_percent']['p99'])
        self.assertIsNone(bench.stats([])['p99'])

    def test_turn_requires_actual_matching_candidate_pair(self):
        m = manifest()
        remote(m)
        bench.validate_manifest(m)
        m['network']['selected_candidate_pair']['local_type'] = 'srflx'
        with self.assertRaisesRegex(ValueError, 'contradicts'):
            bench.validate_manifest(m)
        del m['network']['selected_candidate_pair']
        with self.assertRaisesRegex(ValueError, 'actual selected'):
            bench.validate_manifest(m)

    def test_translated_architecture_and_boolean_resources_rejected(self):
        m = manifest()
        m['application']['architecture'] = 'x86-any'
        with self.assertRaisesRegex(ValueError, 'distinguish'):
            bench.validate_manifest(m)
        m = manifest()
        m['resources']['vcpus'] = True
        with self.assertRaisesRegex(ValueError, 'vcpus'):
            bench.validate_manifest(m)

    def test_sample_consistency(self):
        row = {'kind': 'resource', 'host_cpu_percent': 100, 'host_physical_footprint_bytes': 1000,
               'guest_assigned_bytes': 1024**3, 'timestamp_unix_ns': 1}
        report = self.summarize([row])
        self.assertEqual(report['host_cpu_percent']['p50'], 100)
        m = manifest()
        m['measurement']['resource_method'] = 'not-measured'
        with self.assertRaisesRegex(ValueError, 'contradict'):
            self.summarize([row], m)
        m = manifest()
        m['measurement']['process_scope'] = []
        with self.assertRaisesRegex(ValueError, 'process scope'):
            self.summarize([row], m)
        row['guest_assigned_bytes'] *= 2
        with self.assertRaisesRegex(ValueError, 'assigned memory'):
            self.summarize([row])

    def test_matched_declarations_still_do_not_produce_a_winner(self):
        a = {'manifest': manifest()}
        b = copy.deepcopy(a)
        b['manifest']['implementation']['name'] = 'other-unit-test-only'
        result = bench.compare(a, b)
        self.assertTrue(result['configuration_matches'])
        self.assertIsNone(result['winner'])


if __name__ == '__main__':
    unittest.main()
