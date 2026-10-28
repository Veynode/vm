#!/usr/bin/env python3
"""Assemble a VeynodeVM release directory from this checkout's built pieces.

One directory serves both forms of the product: the standalone VM/Docker
product (CLI, daemon, web UI) and the Mac engine plugin of veynode/core, which
unpacks it and uses the daemon and CLI over API 1 (docs/api.md):

  VeynodeVM/
    bin/          veynodevm, vm-control and the daemon's siblings
    libexec/      host helpers registered in runtime.json, when built
    VeynodeVM.app the Apple Virtualization worker (vm-apple)
    share/boot/   the micro-VM kernel, initramfs and module squashfs
    web/          the web UI server and its built app (with --web)
    install.py    registers this directory in <home>/runtime.json
    MANIFEST.json version, commit, every file's SHA-256, signing

Nothing is built, downloaded, notarized or published here; signing is ad hoc
unless --sign names an identity. The QEMU/GPU runtime (UTM) is never included:
its redistribution has not been reviewed (scripts/package-reviewed-runtime.py).
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
# The daemon finds vm-console-stream, and the CLI vm-control, next to itself.
BINARIES = ('veynodevm', 'vm-control', 'vm-image', 'vm-mcp-http', 'vm-qemu', 'vm-console', 'vm-console-stream')
HELPERS = {'vz_desktop_stream': 'vz-desktop-stream', 'vision': 'vm-vision', 'macos_desktop': 'vm-macos-desktop'}
BOOT = {'kernel': 'Image', 'initrd': 'initramfs.gz', 'modloop': 'modloop'}
WEB = ('server.mjs', 'auth.mjs', 'daemon.mjs', 'desktop.mjs', 'terminal.mjs')
EXCLUDED = ['QEMU/GPU runtime (UTM, virglrenderer, Venus): redistribution not reviewed',
            'cua-driver: third-party binaries fetched with runtime/fetch-cua-driver.sh, not reviewed for redistribution']

INSTALL = '''#!/usr/bin/env python3
"""Register this VeynodeVM directory in <VEYNODEVM_HOME or ~/Library/Application Support/VeynodeVM>/runtime.json.

Keeps what runtime.json already says about the QEMU runtime, DNS, MTU and the
memory budget; points the worker, boot files, web UI and helpers here. Then:
  bin/veynodevm daemon start
"""
import json, os, pathlib, sys

HERE = pathlib.Path(__file__).resolve().parent
LAYOUT = json.loads(%(layout)r)


def main():
    custom = os.environ.get('VEYNODEVM_HOME')
    home = pathlib.Path(custom) if custom else pathlib.Path.home() / 'Library/Application Support/VeynodeVM'
    home.mkdir(mode=0o700, parents=True, exist_ok=True)
    path = home / 'runtime.json'
    runtime = json.loads(path.read_text()) if path.exists() else {}
    for key in ('backend', 'web', *LAYOUT['known_helpers']):
        runtime.pop(key, None)
    runtime['backend'] = str(HERE / LAYOUT['backend'])
    runtime['boot'] = {key: str(HERE / value) for key, value in LAYOUT['boot'].items()}
    if LAYOUT.get('web'):
        runtime['web'] = str(HERE / LAYOUT['web'])
    for key, value in LAYOUT['helpers'].items():
        runtime[key] = str(HERE / value)
    tmp = path.with_suffix('.tmp')
    tmp.write_text(json.dumps(runtime, indent=2) + '\\n')
    os.chmod(tmp, 0o600)
    tmp.replace(path)
    print(f'registered {HERE} in {path}; start the daemon with {HERE / "bin/veynodevm"} daemon start')


if __name__ == '__main__':
    sys.exit(main())
'''


def inputs(root, profile, web):
    """What goes where: (source, destination relative to the release), the
    helpers present, and what is missing."""
    files, missing = [], []

    def need(source, dest):
        if source.exists():
            files.append((source, dest))
        else:
            missing.append(str(source))

    for name in BINARIES:
        need(root / 'target' / profile / name, f'bin/{name}')
    need(root / 'native/.build/vm-apple', 'VeynodeVM.app/Contents/MacOS/vm-apple')
    for file in BOOT.values():
        need(root / '.local/boot' / file, f'share/boot/{file}')
    helpers = {}
    for key, name in HELPERS.items():
        source = root / '.local/bin' / name
        if source.exists():
            files.append((source, f'libexec/{name}'))
            helpers[key] = f'libexec/{name}'
    if web:
        for name in WEB:
            need(root / 'web' / name, f'web/{name}')
        for part in ('dist/server', 'dist/client'):
            need(root / 'web' / part, f'web/{part}')
    return files, helpers, missing


def layout(helpers, web):
    return {
        'backend': 'VeynodeVM.app/Contents/MacOS/vm-apple',
        'boot': {key: f'share/boot/{file}' for key, file in BOOT.items()},
        'web': 'web/server.mjs' if web else None,
        'helpers': helpers,
        # A helper this release lacks must not keep pointing at another build.
        'known_helpers': sorted(HELPERS),
    }


def info_plist(version):
    return f'''<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>com.veynode.vmworker</string>
<key>CFBundleExecutable</key><string>vm-apple</string>
<key>CFBundleName</key><string>Veynode VM Worker</string>
<key>CFBundleDisplayName</key><string>Veynode VM Worker</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
<key>CFBundleVersion</key><string>1</string>
<key>LSMinimumSystemVersion</key><string>14.0</string>
<key>NSHighResolutionCapable</key><true/>
<key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
'''


def copy(source, dest):
    dest.parent.mkdir(parents=True, exist_ok=True)
    if source.is_dir():
        shutil.copytree(source, dest, symlinks=False)
    else:
        shutil.copy2(source, dest, follow_symlinks=True)


def signing_argv(identity, path, entitlements=None):
    argv = ['codesign', '--force', '--sign', identity]
    if identity != '-':
        # A Developer ID signature for notarization: hardened runtime and a secure timestamp.
        argv += ['--options', 'runtime', '--timestamp']
    if entitlements:
        argv += ['--entitlements', str(entitlements)]
    return argv + [str(path)]


def digest(path):
    hasher = hashlib.sha256()
    with open(path, 'rb') as source:
        while chunk := source.read(1 << 20):
            hasher.update(chunk)
    return hasher.hexdigest()


def manifest(out, version, commit, profile, identity, helpers, web):
    entries = []
    for path in sorted(p for p in out.rglob('*') if p.is_file() and p.name != 'MANIFEST.json'):
        mode = path.stat().st_mode & 0o777
        entries.append({'path': str(path.relative_to(out)), 'sha256': digest(path), 'mode': f'{mode:o}'})
    return {
        'product': 'VeynodeVM',
        'version': version,
        'commit': commit,
        'profile': profile,
        'api': 1,
        'signing': 'ad hoc' if identity == '-' else identity,
        'notarized': False,
        'includes': {'web': web, 'helpers': sorted(helpers)},
        'excludes': EXCLUDED,
        'files': entries,
    }


def run(argv, dry):
    if dry:
        print(' '.join(argv))
    else:
        subprocess.run(argv, check=True)


def package(root, out, profile, identity, web, version, commit, dry=False):
    files, helpers, missing = inputs(root, profile, web)
    if missing:
        raise SystemExit('missing built pieces (see README "Quick start"):\n  ' + '\n  '.join(missing))
    out.mkdir(parents=True)
    for source, dest in files:
        copy(source, out / dest)
    (out / 'VeynodeVM.app/Contents/Info.plist').write_text(info_plist(version))
    if web:
        # dist/server/server.js is an ES module.
        (out / 'web/package.json').write_text(json.dumps({'type': 'module', 'private': True}) + '\n')
    (out / 'install.py').write_text(INSTALL % {'layout': json.dumps(layout(helpers, web))})
    os.chmod(out / 'install.py', 0o755)
    for name in BINARIES:
        run(signing_argv(identity, out / 'bin' / name), dry)
    for dest in helpers.values():
        run(signing_argv(identity, out / dest), dry)
    run(signing_argv(identity, out / 'VeynodeVM.app', root / 'native/entitlements.plist'), dry)
    run(['codesign', '--verify', '--strict', '--deep', str(out / 'VeynodeVM.app')], dry)
    record = manifest(out, version, commit, profile, identity, helpers, web)
    (out / 'MANIFEST.json').write_text(json.dumps(record, indent=2) + '\n')
    return record


def crate_version(root):
    for line in (root / 'crates/cli/Cargo.toml').read_text().splitlines():
        if line.startswith('version'):
            return line.split('=', 1)[1].strip().strip('"')
    raise SystemExit('no version in crates/cli/Cargo.toml')


def git_commit(root):
    result = subprocess.run(['git', '-C', str(root), 'rev-parse', 'HEAD'], capture_output=True, text=True)
    dirty = subprocess.run(['git', '-C', str(root), 'status', '--porcelain', '--untracked-files=no'], capture_output=True, text=True)
    commit = result.stdout.strip() or 'unknown'
    return commit + ('-dirty' if dirty.stdout.strip() else '')


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--output', required=True, type=Path, help='a fresh absolute directory, such as /tmp/VeynodeVM')
    parser.add_argument('--profile', default='release', choices=('release', 'debug'), help='which target/ build to take (default release)')
    parser.add_argument('--sign', default='-', metavar='IDENTITY', help='codesign identity, such as "Developer ID Application: …" (default ad hoc)')
    parser.add_argument('--web', action='store_true', help='include the web UI (cd web && npm ci && npm run build first)')
    parser.add_argument('--dry-run', action='store_true', help='list the inputs and what is missing; copy and sign nothing')
    args = parser.parse_args()
    if not args.output.is_absolute() or args.output.exists():
        raise SystemExit('--output must be an absolute path that does not exist yet')
    if args.dry_run:
        files, helpers, missing = inputs(ROOT, args.profile, args.web)
        for source, dest in files:
            print(f'{dest}  <- {source}')
        for path in missing:
            print(f'MISSING {path}')
        return 1 if missing else 0
    record = package(ROOT, args.output, args.profile, args.sign, args.web, crate_version(ROOT), git_commit(ROOT))
    print(f'{args.output}: VeynodeVM {record["version"]} ({record["commit"]}), {len(record["files"])} files, signing {record["signing"]}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
