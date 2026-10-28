import importlib.util
import pathlib
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('check_changed', ROOT / 'scripts/check-changed.py')
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)

DEPENDENTS = {'api': {'cli', 'control', 'mcp', 'media-local'}, 'cli': set(), 'control': {'media-local'}, 'mcp': {'control'}, 'gateway': set(), 'media-local': set(), 'oci': {'control'}, 'remote-proto': {'gateway', 'media-local'}, 'session-auth': {'media-local'}}
SCRIPT_TESTS = {'scripts/summarize-benchmark.py': {'test_benchmark.py'}}


def plan(*paths, quick=False):
    return check.build_plan(paths, quick=quick, dependents=DEPENDENTS, script_tests=SCRIPT_TESTS)


def keys(p):
    return {k for k in p.stages if not k.endswith('-install')}


class PlanTest(unittest.TestCase):
    def test_only_the_core_links_the_core(self):
        linked = dict(DEPENDENTS, control={'cli', 'media-local'})
        self.assertEqual(check.core_dependents(dependents=linked), ['cli'])
        self.assertEqual(check.core_dependents(dependents=DEPENDENTS), [])
        self.assertEqual(check.core_dependents(), [])

    def test_a_manifest_change_checks_the_core_rule(self):
        self.assertIn('core-deps', plan('crates/cli/Cargo.toml').stages)

    def test_crate_change_tests_the_crate_and_lints_dependents(self):
        p = plan('crates/oci/src/tart.rs')
        self.assertEqual(keys(p), {'rustfmt', 'cargo-clippy', 'cargo-test:oci'})
        self.assertEqual(p.stages['cargo-clippy'].argv, ['cargo', 'clippy', '--locked', '-p', 'vm-control', '-p', 'veynodevm-oci', '--all-targets', '--', '-D', 'warnings'])
        self.assertEqual(p.stages['rustfmt'].triggers, {'crates/oci/src/tart.rs'})

    def test_control_change_reaches_its_dependent(self):
        argv = plan('crates/control/src/jobs.rs').stages['cargo-clippy'].argv
        self.assertEqual([argv[i + 1] for i, a in enumerate(argv) if a == '-p'], ['vm-control', 'vm-media-local'])

    def test_api_change_reaches_its_clients_and_the_core(self):
        argv = plan('crates/api/src/frame.rs').stages['cargo-clippy'].argv
        self.assertEqual([argv[i + 1] for i, a in enumerate(argv) if a == '-p'], ['veynodevm-api', 'veynodevm', 'vm-control', 'veynodevm-mcp', 'vm-media-local'])

    def test_workspace_files_run_every_crate(self):
        for path in ('Cargo.lock', 'Cargo.toml', 'vendor/webrtc-0.21.0/src/lib.rs'):
            self.assertEqual(keys(plan(path)), {'cargo-fmt', 'cargo-clippy', 'cargo-test'}, path)

    def test_quick_filters_by_module_and_skips_dependents(self):
        p = plan('crates/control/src/qemu_graphics_tests.rs', 'crates/control/tests/images.rs', quick=True)
        self.assertEqual(p.stages['cargo-test:control:quick'].argv[-2:], ['--', 'qemu_graphics'])
        self.assertIn('cargo-test:control:images', p.stages)
        self.assertNotIn('cargo-test:control', p.stages)
        self.assertNotIn('veynodevm', p.stages['cargo-clippy'].argv)

    def test_quick_falls_back_to_the_package_for_roots_and_bins(self):
        for path in ('crates/control/src/lib.rs', 'crates/control/src/bin/vm-qemu.rs', 'crates/control/Cargo.toml'):
            self.assertIn('cargo-test:control', plan(path, quick=True).stages, path)

    def test_quick_filter_names(self):
        self.assertEqual(check.quick_filter('jobs.rs'), 'jobs')
        self.assertEqual(check.quick_filter('ice_transport_tests.rs'), 'ice_transport')
        self.assertEqual(check.quick_filter('desktop/mod.rs'), 'desktop')
        self.assertIsNone(check.quick_filter('main.rs'))
        self.assertIsNone(check.quick_filter('bin/vm_mcp_http_tests/mod.rs'))

    def test_crate_readme_is_only_a_link_check(self):
        self.assertEqual(keys(plan('crates/control/README.md')), {'md-links'})

    def test_go_agent(self):
        self.assertEqual(keys(plan('guest/veynode-agent/jobs.go')), {'go-test', 'go-vet'})

    def test_web_builds_only_for_build_inputs(self):
        self.assertEqual(keys(plan('web/src/ui.tsx')), {'web-typecheck', 'web-test'})
        self.assertIn('web-build', plan('web/src/routes/index.tsx').stages)

    def test_native_builds_go_to_a_temp_dir(self):
        p = plan('native/worker/main.swift', 'native/tools/vm-vision.swift', 'scripts/build-native.sh')
        self.assertEqual(keys(p), {'native:scripts/build-native.sh', 'native:native/tools/build-vm-vision.sh'})
        self.assertEqual(check.stage_argv(p.stages['native:scripts/build-native.sh'], pathlib.Path('/t')), ['scripts/build-native.sh', '--output-app', '/t/vm-apple.app'])

    def test_python_scripts_run_their_unit_tests(self):
        p = plan('scripts/summarize-benchmark.py', 'tests/test_linux_fixture.py')
        self.assertEqual(keys(p), {'py-compile', 'py:tests/test_benchmark.py', 'py:tests/test_linux_fixture.py'})

    def test_live_harness_is_compiled_and_noted(self):
        p = plan('scripts/test-multivm.py')
        self.assertEqual(keys(p), {'py-compile'})
        self.assertTrue(any('live harness' in n for n in p.notes))

    def test_live_only_and_unmapped_paths_are_reported(self):
        p = plan('guest/kernel/build.py', 'native/console/Display.m', 'nowhere/file')
        self.assertEqual(p.stages, {})
        self.assertEqual(len(p.notes), 2)
        self.assertEqual(p.unmapped, ['nowhere/file'])

    def test_generated_and_archived_paths_are_ignored(self):
        self.assertEqual(plan('target/debug/x', 'archive/old.rs', '.local/run/a.json').stages, {})

    def test_stages_run_cheapest_first(self):
        kinds = [s.kind for s in plan('crates/oci/src/tart.rs', 'native/worker/main.swift').ordered()]
        self.assertEqual(kinds, sorted(kinds, key=check.ORDER.get))

    def test_real_crate_dependents(self):
        self.assertEqual(check.crate_dependents(), DEPENDENTS)


class LinkTest(unittest.TestCase):
    def test_broken_relative_links(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / 'there.md').write_text('x')
            (root / 'a.md').write_text('[ok](there.md#part) [web](https://x.y) [anchor](#a)\n```\n[skip](gone.md)\n```\n[bad](gone.md)\n')
            self.assertEqual(check.check_links(['a.md'], root), ['a.md:5: gone.md'])


    def test_mcp_tests_build_the_core_binary_they_run(self):
        for path in ('crates/mcp/src/lib.rs', 'crates/control/src/mcp.rs'):
            p = plan(path)
            self.assertIn('cargo-test:mcp', p.stages, path)
            self.assertEqual(p.stages['cargo-build:vm-control'].argv[-4:], ['-p', 'vm-control', '--bin', 'vm-control'])
            order = list(p.stages)
            self.assertLess(order.index('cargo-build:vm-control'), order.index('cargo-test:mcp'))
        # The edit loop tests only what changed.
        self.assertNotIn('cargo-test:mcp', plan('crates/control/src/mcp.rs', quick=True).stages)


if __name__ == '__main__':
    unittest.main()
