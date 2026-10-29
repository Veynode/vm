#!/usr/bin/env python3
"""Write the local Venus runtime registration the daemon admits (--qemu-venus-runtime).

Roles: loader, host_library and render_server from
runtime/linux-vulkan-build-host-runtime.py; spice_library from
runtime/build-spice-reconnect-server.py; qemu_library, icd and firmware from the
pinned UTM 5.0.8 app. Every role must match crates/control/src/qemu_venus_catalog.json.

--repin rewrites the catalog hashes of the four locally built roles to this
machine's build (compiler output is not reproducible across toolchains). It never
re-pins vendor roles or frameworks: those must still match UTM 5.0.8. A re-pin is
a review decision; commit it only after inspecting the build logs and manifests.
"""
import argparse, hashlib, json, os, pathlib, sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
CATALOG = ROOT / 'crates/control/src/qemu_venus_catalog.json'
APP = ROOT / '.local/runtime/utm/UTM.app/Contents'
BUILT = ('loader', 'host_library', 'render_server', 'spice_library')


def sha(path):
    with open(path, 'rb') as f:
        return hashlib.file_digest(f, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--host-dir', type=pathlib.Path, required=True, help='output of linux-vulkan-build-host-runtime.py')
    parser.add_argument('--output', type=pathlib.Path, default=ROOT / '.local/runtime/venus/registration.json')
    parser.add_argument('--repin', action='store_true', help='update catalog hashes of the locally built roles')
    args = parser.parse_args()
    host = args.host_dir.resolve()
    frameworks = (APP / 'Frameworks').resolve()
    roles = {
        'loader': host / 'probe-loader',
        'host_library': host / 'build/src/libvirglrenderer.1.dylib',
        'render_server': host / 'build/server/virgl_render_server',
        'qemu_library': frameworks / 'qemu-aarch64-softmmu.framework/Versions/A/qemu-aarch64-softmmu',
        'spice_library': ROOT / '.local/spice-reconnect-build/runtime/libspice-server-fixed.dylib',
        'icd': APP / 'Resources/vulkan/icd.d/MoltenVK_icd.json',
        'firmware': APP / 'Resources/qemu/edk2-aarch64-code.fd',
    }
    roles = {name: path.resolve(strict=True) for name, path in roles.items()}
    actual = {name: sha(path) for name, path in roles.items()}
    catalog = json.loads(CATALOG.read_text())
    if set(roles) != set(catalog['roles']):
        raise SystemExit('catalog role contract changed')
    vendor = [n for n in roles if n not in BUILT and actual[n] != catalog['roles'][n]]
    stale = [f for f, h in catalog['frameworks'].items() if sha(frameworks / f) != h]
    if vendor or stale:
        raise SystemExit(f'UTM 5.0.8 vendor pins do not match: {vendor + stale}')
    changed = {n: (catalog['roles'][n], actual[n]) for n in BUILT if actual[n] != catalog['roles'][n]}
    if changed and not args.repin:
        raise SystemExit('built roles differ from the reviewed catalog (use --repin after review): ' + ', '.join(changed))
    if changed:
        for name, (_, new) in changed.items():
            catalog['roles'][name] = new
        CATALOG.write_text(json.dumps(catalog, indent=2) + '\n')
    registration = {
        'schema': catalog['schema'],
        'profile_revision': catalog['profile_revision'],
        'frameworks_dir': str(frameworks),
        **{name: {'path': str(path), 'sha256': actual[name]} for name, path in roles.items()},
    }
    args.output.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    tmp = args.output.with_suffix('.tmp')
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, 'w') as f:
        f.write(json.dumps(registration, indent=2) + '\n')
    tmp.replace(args.output)
    json.dump({'registration': str(args.output), 'repinned': {n: {'old': o, 'new': w} for n, (o, w) in changed.items()}}, sys.stdout, indent=2)
    print()


if __name__ == '__main__':
    main()
