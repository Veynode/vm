#!/usr/bin/env python3
"""Run only the checks that the changed files need.

Changed files are those that differ from the merge-base with main, plus staged,
unstaged and untracked files; positional paths replace that list. --dry-run
prints the plan, --quick narrows Rust tests to the touched modules for the edit
loop, and --all runs the full gate (scripts/check.sh).
"""
import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CRATES = {
    'api': 'veynodevm-api',
    'cli': 'veynodevm',
    'control': 'vm-control',
    'mcp': 'veynodevm-mcp',
    'media-local': 'vm-media-local',
    'oci': 'veynodevm-oci',
    'remote-proto': 'vm-remote-proto',
    'gateway': 'vm-gateway',
    'session-auth': 'vm-session-auth',
}
WORKSPACE_FILES = {'Cargo.toml', 'Cargo.lock'}
GO_AGENT = 'guest/veynode-agent'
# Native helpers that check.sh builds, with the arguments that send their output to DIR.
NATIVE_BUILDS = {
    'native/worker/': ('scripts/build-native.sh', ['--output-app', '{dir}/vm-apple.app']),
    'native/tools/vz-desktop-stream.': ('native/tools/build-vz-desktop-stream.sh', ['--output', '{dir}/vz-desktop-stream']),
    'native/tools/vm-vision.': ('native/tools/build-vm-vision.sh', ['--output', '{dir}/vm-vision']),
    'native/tools/vm-macos-desktop.': ('native/tools/build-vm-macos-desktop.sh', ['--output', '{dir}/vm-macos-desktop']),
    'native/guest-metal-fix/': ('native/guest-metal-fix/build.sh', ['{dir}/metal-fix']),
}
WEB_BUILD_INPUTS = ('web/package.json', 'web/package-lock.json', 'web/vite.config.ts', 'web/server.mjs',
                    'web/src/routes/', 'web/src/router.tsx', 'web/src/start.ts', 'web/src/routeTree.gen.ts')
# Paths with no offline check, and the live command that covers them.
LIVE_ONLY = {
    'guest/kernel/': 'python3 guest/kernel/build.py && python3 scripts/test-containers.py',
    'guest/images/': 'veynodevm build -t <image> guest/images/<name> && veynodevm run --rm <image>',
    'runtime/': 'the matching runtime/*.sh or *.py build (see docs/guides/building.md)',
    'native/': 'the build or test script next to the file (no offline check is mapped for it)',
    'tests/mcp-client/': 'the scripts/test-macos-*.py harness that loads it (needs a macOS guest)',
}
NOT_SOURCE = ('target/', '.local/', 'vendor/', '.claude/', '.gitignore', '.ignore', 'licenses/')
ORDER = {'fmt': 0, 'lint': 1, 'test': 2, 'build': 3}


@dataclass
class Stage:
    key: str
    kind: str
    argv: list
    triggers: set = field(default_factory=set)
    note: str = ''
    needs_dir: bool = False


@dataclass
class Plan:
    stages: dict = field(default_factory=dict)
    notes: list = field(default_factory=list)
    unmapped: list = field(default_factory=list)

    def add(self, key, kind, argv, trigger, note='', needs_dir=False):
        stage = self.stages.setdefault(key, Stage(key, kind, argv, note=note, needs_dir=needs_dir))
        stage.triggers.add(trigger)
        return stage

    def ordered(self):
        return sorted(self.stages.values(), key=lambda s: (ORDER[s.kind], s.key))


def crate_dependents(root=ROOT):
    """Map each crate directory to the crate directories that depend on it by path."""
    dependents = {name: set() for name in CRATES}
    for name in CRATES:
        text = (root / 'crates' / name / 'Cargo.toml').read_text()
        for dep in re.findall(r'path\s*=\s*"\.\./([^"]+)"', text):
            if dep in dependents:
                dependents[dep].add(name)
    return dependents


# Decision 0028: the core is a daemon with one interface, API 1. Its clients
# (the CLI, the web UI, the core plugin) never link it; only the core's own
# media side (the console broker) does.
CORE = 'control'
# Crates whose tests run another crate's binary, which cargo does not build for them:
# vm-mcp-http's end-to-end tests run vm-control's stdio MCP server per session.
RUNS = {'mcp': ('vm-control', 'vm-control')}
CORE_PARTS = {'media-local'}


def core_dependents(root=ROOT, dependents=None):
    """Crates outside the core that depend on the core crate by path, which decision 0028 forbids."""
    dependents = crate_dependents(root) if dependents is None else dependents
    return sorted(dependents[CORE] - CORE_PARTS)


def unit_tests_for_scripts(root=ROOT):
    """Map scripts/<file> to the tests/test_*.py modules that load it."""
    found = {}
    for test in sorted((root / 'tests').glob('test_*.py')):
        for script in set(re.findall(r"scripts/([A-Za-z0-9_.-]+\.py)", test.read_text())):
            found.setdefault(f'scripts/{script}', set()).add(test.name)
    return found


def quick_filter(rel):
    """Test-name filter for a crate source file, or None when only the whole package covers it."""
    parts = Path(rel).parts
    if 'bin' in parts:
        return None
    stem = Path(rel).stem
    if stem == 'mod':
        stem = parts[-2]
    if stem in ('lib', 'main'):
        return None
    for suffix in ('_tests', '_test_support', '_test'):
        if stem.endswith(suffix) and stem != suffix:
            return stem[:-len(suffix)]
    return stem


def unit_test_argv(modules):
    return [sys.executable, '-m', 'unittest', 'discover', '-s', 'tests', '-p', modules]


def npm_install_needed(prefix, root=ROOT):
    lock = root / prefix / 'package-lock.json'
    marker = root / prefix / 'node_modules' / '.package-lock.json'
    return not marker.exists() or (lock.exists() and lock.stat().st_mtime > marker.stat().st_mtime)


def build_plan(paths, quick=False, root=ROOT, dependents=None, script_tests=None):
    plan = Plan()
    dependents = crate_dependents(root) if dependents is None else dependents
    script_tests = unit_tests_for_scripts(root) if script_tests is None else script_tests
    changed_crates, lint_crates, quick_filters, full_crates, integration = set(), set(), {}, set(), {}
    rust_files, all_rust = [], False
    md_files = []

    for path in sorted(set(paths)):
        exists = (root / path).exists()
        if path.endswith('.md'):
            if exists and not path.startswith(NOT_SOURCE):
                md_files.append(path)
            continue
        if path in WORKSPACE_FILES or path.startswith('vendor/'):
            all_rust = True
            continue
        if path.startswith(NOT_SOURCE):
            continue
        if path.startswith('crates/') and path.endswith('/Cargo.toml'):
            plan.add('core-deps', 'lint', [sys.executable, 'scripts/check-changed.py', '--check-deps'], path)
        if path.startswith('crates/'):
            crate = path.split('/')[1]
            if crate not in CRATES:
                plan.unmapped.append(path)
                continue
            changed_crates.add(crate)
            if path.endswith('.rs') and exists:
                rust_files.append(path)
            rel = path.split('/', 2)[2]
            if rel.startswith('src/') and rel.endswith('.rs'):
                f = quick_filter(rel[4:])
                if f is None:
                    full_crates.add(crate)
                else:
                    quick_filters.setdefault(crate, set()).add(f)
            elif rel.startswith('tests/') and rel.count('/') == 1 and rel.endswith('.rs'):
                integration.setdefault(crate, set()).add(Path(rel).stem)
            else:
                full_crates.add(crate)
            continue
        if path.startswith(GO_AGENT + '/'):
            plan.add('go-test', 'test', ['go', '-C', GO_AGENT, 'test', '-race', './...'], path)
            plan.add('go-vet', 'lint', ['env', 'GOOS=linux', 'GOARCH=arm64', 'go', '-C', GO_AGENT, 'vet', './...'], path)
            continue
        if path.startswith('web/'):
            if npm_install_needed('web', root):
                plan.add('web-install', 'lint', ['npm', '--prefix', 'web', 'ci', '--no-audit', '--no-fund'], path)
            plan.add('web-typecheck', 'lint', ['npm', '--prefix', 'web', 'run', 'typecheck'], path)
            plan.add('web-test', 'test', ['npm', '--prefix', 'web', 'test'], path)
            if path.startswith(WEB_BUILD_INPUTS):
                plan.add('web-build', 'build', ['npm', '--prefix', 'web', 'run', 'build'], path)
            continue
        if path.startswith('browser/'):
            if npm_install_needed('browser', root):
                plan.add('browser-install', 'lint', ['npm', '--prefix', 'browser', 'ci', '--no-audit', '--no-fund'], path)
            plan.add('browser-test', 'test', ['npm', '--prefix', 'browser', 'test'], path)
            continue
        native = next((b for p, b in NATIVE_BUILDS.items() if path.startswith(p) or path == b[0]), None)
        if native:
            script, args = native
            plan.add(f'native:{script}', 'build', [script, *args], path, needs_dir=True)
            continue
        if path.startswith('tests/') and re.fullmatch(r'tests/test_[^/]+\.py', path):
            if exists:
                plan.add(f'py:{path}', 'test', unit_test_argv(Path(path).name), path)
            continue
        if path.startswith('scripts/') and path.endswith('.py'):
            if exists:
                plan.add('py-compile', 'lint', [sys.executable, '-m', 'py_compile'], path)
            for test in sorted(script_tests.get(path, ())):
                plan.add(f'py:tests/{test}', 'test', unit_test_argv(test), path)
            if re.match(r'scripts/test-', path):
                plan.notes.append(f'{path}: live harness, needs a VM; run only when asked')
            continue
        if path.startswith('scripts/') and path.endswith('.sh') and exists:
            plan.add('sh-syntax', 'lint', ['bash', '-n'], path)
            continue
        live = next((p for p in LIVE_ONLY if path.startswith(p)), None)
        if live:
            plan.notes.append(f'{path}: no offline check; live: {LIVE_ONLY[live]}')
            continue
        plan.unmapped.append(path)

    if all_rust:
        plan.add('cargo-fmt', 'fmt', ['cargo', 'fmt', '--check'], 'workspace')
        plan.add('cargo-clippy', 'lint', ['cargo', 'clippy', '--locked', '--workspace', '--all-targets', '--', '-D', 'warnings'], 'workspace')
        plan.add('cargo-test', 'test', ['cargo', 'test', '--locked', '--workspace'], 'workspace')
    elif changed_crates:
        if rust_files:
            plan.add('rustfmt', 'fmt', ['rustfmt', '--check', '--edition', '2021'], 'rust')
            plan.stages['rustfmt'].triggers = set(rust_files)
        lint_crates = set(changed_crates)
        if not quick:
            for crate in changed_crates:
                lint_crates |= dependents.get(crate, set())
        clippy = ['cargo', 'clippy', '--locked']
        for crate in sorted(lint_crates):
            clippy += ['-p', CRATES[crate]]
        plan.add('cargo-clippy', 'lint', clippy + ['--all-targets', '--', '-D', 'warnings'], 'rust')
        plan.stages['cargo-clippy'].triggers = {f'crates/{c}' for c in sorted(changed_crates)}
        tested = set(changed_crates)
        if not quick:
            # A change to a binary those tests run is a change to what they test.
            tested |= {c for c, (pkg, _) in RUNS.items() if any(CRATES[d] == pkg for d in changed_crates)}
        for crate in sorted(tested):
            package = CRATES[crate]
            if crate in RUNS:
                pkg, binary = RUNS[crate]
                plan.add(f'cargo-build:{binary}', 'test', ['cargo', 'build', '--locked', '-p', pkg, '--bin', binary], f'crates/{crate}')
            if not quick or crate in full_crates or crate not in changed_crates:
                plan.add(f'cargo-test:{crate}', 'test', ['cargo', 'test', '--locked', '-p', package], f'crates/{crate}')
                continue
            for test in sorted(integration.get(crate, ())):
                plan.add(f'cargo-test:{crate}:{test}', 'test', ['cargo', 'test', '--locked', '-p', package, '--test', test], f'crates/{crate}/tests/{test}.rs')
            filters = sorted(quick_filters.get(crate, ()))
            if filters:
                plan.add(f'cargo-test:{crate}:quick', 'test', ['cargo', 'test', '--locked', '-p', package, '--', *filters], f'crates/{crate}',
                         note='quick filter; "running 0 tests" everywhere means the filter missed, rerun without --quick')

    if md_files:
        plan.add('md-links', 'lint', [sys.executable, 'scripts/check-changed.py', '--check-links'], 'md')
        plan.stages['md-links'].triggers = set(md_files)
    return plan


def stage_argv(stage, temp_dir):
    """Commands whose arguments are the trigger files get them appended; output paths go to TEMP_DIR."""
    argv = [a.replace('{dir}', str(temp_dir)) for a in stage.argv]
    if stage.key in ('rustfmt', 'py-compile', 'sh-syntax', 'md-links'):
        argv += sorted(stage.triggers)
    return argv


LINK = re.compile(r'\[[^\]]*\]\(([^)\s]+)(?:\s+"[^"]*")?\)')


def check_links(files, root=ROOT):
    """Report relative Markdown links whose target file does not exist."""
    broken = []
    for name in files:
        path = root / name
        in_fence = False
        for number, line in enumerate(path.read_text(errors='replace').splitlines(), 1):
            if line.lstrip().startswith('```'):
                in_fence = not in_fence
            if in_fence:
                continue
            for target in LINK.findall(line):
                if re.match(r'^[a-z][a-z0-9+.-]*:|^#|^<', target, re.I):
                    continue
                target = target.split('#', 1)[0]
                if target and not (path.parent / target).exists():
                    broken.append(f'{name}:{number}: {target}')
    return broken


def git(*args):
    return subprocess.run(['git', *args], cwd=ROOT, check=True, capture_output=True, text=True).stdout


def changed_files(base):
    merge_base = git('merge-base', base, 'HEAD').strip()
    files = set(git('diff', '--name-only', merge_base).split())
    files |= set(git('diff', '--name-only', '--cached').split())
    files |= set(git('ls-files', '-o', '--exclude-standard').split())
    return sorted(files)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('paths', nargs='*', help='check these repository paths instead of the git changes')
    parser.add_argument('--base', default='main', help='compare against the merge-base with this ref (default: main)')
    parser.add_argument('--quick', action='store_true', help='edit loop: Rust tests of the touched modules only, no dependent crates')
    parser.add_argument('--dry-run', action='store_true', help='print the plan without running it')
    parser.add_argument('--keep-going', action='store_true', help='run every stage even after a failure')
    parser.add_argument('--all', action='store_true', help='run the full gate, scripts/check.sh')
    parser.add_argument('--check-links', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--check-deps', action='store_true', help=argparse.SUPPRESS)
    args = parser.parse_args()
    os.chdir(ROOT)

    if args.check_links:
        broken = check_links(args.paths)
        for line in broken:
            print(f'broken link: {line}', file=sys.stderr)
        return 1 if broken else 0
    if args.check_deps:
        users = core_dependents()
        for name in users:
            print(f'crates/{name} depends on crates/{CORE}; use the API (crates/api) instead', file=sys.stderr)
        return 1 if users else 0
    if args.all:
        return subprocess.call(['scripts/check.sh'])

    paths = [os.path.relpath(Path(p).resolve(), ROOT) for p in args.paths] if args.paths else changed_files(args.base)
    if not paths:
        print('No changed files.')
        return 0
    plan = build_plan(paths, quick=args.quick)
    stages = plan.ordered()
    for note in plan.notes:
        print(f'note: {note}')
    for path in plan.unmapped:
        print(f'no check mapped: {path}')
    if not stages:
        print('Nothing to run.')
        return 0

    env = dict(os.environ, PATH=f'/opt/homebrew/bin:{os.environ.get("PATH", "")}')
    temp_dir = None
    results = []
    try:
        for stage in stages:
            if stage.needs_dir and temp_dir is None and not args.dry_run:
                (ROOT / '.local').mkdir(exist_ok=True)
                temp_dir = Path(tempfile.mkdtemp(prefix='check-changed.', dir=ROOT / '.local'))
            argv = stage_argv(stage, temp_dir or Path('.local/check-changed.TMP'))
            triggers = sorted(stage.triggers)
            shown = ', '.join(triggers[:4]) + (f' (+{len(triggers) - 4})' if len(triggers) > 4 else '')
            print(f'\n==> {stage.key}  [{shown}]\n    {" ".join(argv)}', flush=True)
            if stage.note:
                print(f'    {stage.note}')
            if args.dry_run:
                continue
            start = time.monotonic()
            code = subprocess.call(argv, env=env)
            results.append((stage.key, code, time.monotonic() - start))
            if code and not args.keep_going:
                break
    finally:
        if temp_dir:
            shutil.rmtree(temp_dir, ignore_errors=True)

    if args.dry_run:
        return 0
    print('\nSummary:')
    for key, code, seconds in results:
        print(f'  {"ok  " if code == 0 else "FAIL"} {key} ({seconds:.1f}s)')
    skipped = [s.key for s in stages][len(results):]
    for key in skipped:
        print(f'  skip {key} (after a failure)')
    return 1 if any(code for _, code, _ in results) else 0


if __name__ == '__main__':
    sys.exit(main())
