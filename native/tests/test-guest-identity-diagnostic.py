#!/usr/bin/env python3
"""Actual host parser over private synthetic Guest sockets; no VM/SCK claim."""
import argparse
import copy
import importlib.util
import json
import pathlib

ROOT = pathlib.Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location(
    'cleanup_fixture', ROOT / 'native/tests/test-guest-video-cleanup.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=pathlib.Path, required=True)
    parser.add_argument('--output', type=pathlib.Path, required=True)
    args = parser.parse_args()
    valid = dict(stale_site='sample_publish', verification_age_us=750001,
                 authority_read_max_us=170, identity_publish_lock_wait_max_us=20,
                 sample_copy_max_us=900, sample_lock_wait_max_us=12,
                 identity_timer_lateness_max_us=30, sample_generation=2)
    cases = []
    for site in ['authority_read', 'authority_publish', 'timer_read',
                 'sample_admission', 'sample_publish', 'writer_admission', None]:
        item = dict(valid, stale_site=site)
        if site is None:
            item['verification_age_us'] = None
        cases.append((str(site), item, True))
    for key, bad in [('stale_site', '/private/SECRET'), ('sample_copy_max_us', True),
                     ('sample_lock_wait_max_us', -1), ('sample_generation', 1.5),
                     ('sample_generation', 9007199254740992),
                     ('verification_age_us', 'SECRET'), ('extra', 'SECRET')]:
        item = copy.deepcopy(valid)
        item[key] = bad
        cases.append((f'invalid-{key}-{type(bad).__name__}', item, False))
    item = dict(valid)
    del item['sample_copy_max_us']
    cases.append(('missing-field', item, False))
    report = {'scope': __doc__, 'passed': False, 'cases': []}
    with args.output.open('x') as output:
        try:
            for name, value, accepted in cases:
                result = fixture.run(args.binary, 'early-clean', identity_diagnostic=value)
                reduced = result['summary']['guest_error']
                assert 'SECRET' not in json.dumps(reduced), name
                if accepted:
                    assert reduced['console_identity_diagnostic'] == value, name
                else:
                    assert 'console_identity_diagnostic' not in reduced, name
                report['cases'].append({'name': name, 'passed': True,
                                        'diagnostic_accepted': accepted})
            # Timing metadata is never sufficient cleanup evidence.
            result = fixture.run(args.binary, 'missing-terminal', identity_diagnostic=valid)
            assert result['exit_code'] == 1
            assert not result['summary']['guest_cleanup_confirmed']
            assert result['summary']['guest_error']['console_identity_diagnostic'] == valid
            report['cases'].append({'name': 'no-cleanup-attestation', 'passed': True})
            report['passed'] = True
        finally:
            json.dump(report, output, indent=2)
            output.write('\n')
    print(json.dumps({'passed': True, 'cases': len(report['cases'])}))


if __name__ == '__main__':
    main()
