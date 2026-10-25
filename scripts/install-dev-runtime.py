#!/usr/bin/env python3
"""Register this checkout's development runtime in VeynodeVM runtime.json.

Writes <VEYNODEVM_HOME or ~/Library/Application Support/VeynodeVM>/runtime.json:
  backend   .local/bin/vm-apple            (scripts/build-native.sh)
  boot      .local/boot/{Image,initramfs.gz,modloop}  (guest/kernel/build.py)
  qemu      UTM-derived QEMU/HVF GPU runtime, only with --qemu and when present
            (runtime/fetch-utm.sh, runtime/build-utm-loader.sh; the SPICE reconnect
            build's loader is preferred when present)
  web       web/server.mjs when web/dist is built (cd web && npm ci && npm run build)

Only paths that exist are written; the daemon reads this file when started
without flags. Existing keys not managed here (memory_budget_mib) are kept.
"""
import argparse, json, os, pathlib, sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
UTM = ROOT / '.local/runtime/utm'


def home():
    custom = os.environ.get('VEYNODEVM_HOME')
    if custom:
        return pathlib.Path(custom)
    return pathlib.Path.home() / 'Library/Application Support/VeynodeVM'


def require(path):
    if not path.exists():
        raise SystemExit(f'missing {path}')
    return str(path.resolve())


def qemu_paths():
    paths = {
        'worker': ROOT / 'target/debug/vm-qemu',
        'launcher': next((p for p in (ROOT / '.local/spice-reconnect-build/runtime/probe-loader', UTM / 'probe-loader') if p.exists()),
                         UTM / 'probe-loader'),
        'library': UTM / 'UTM.app/Contents/Frameworks/qemu-aarch64-softmmu.framework/Versions/A/qemu-aarch64-softmmu',
        'resources': UTM / 'UTM.app/Contents/Resources/qemu',
    }
    optional = {
        'spice_library': ROOT / '.local/spice-reconnect-build/runtime/libspice-server-fixed.dylib',
        'screenshot_helper': ROOT / '.local/bin/spice-screenshot',
        'console_broker': ROOT / 'target/debug/vm-console',
        'console_helper': ROOT / '.local/bin/spice-console',
        'venus_runtime': ROOT / '.local/runtime/venus/registration.json',
    }
    missing = [str(p) for p in paths.values() if not p.exists()]
    if missing:
        raise SystemExit('QEMU runtime incomplete; run runtime/fetch-utm.sh and runtime/build-utm-loader.sh first:\n  ' + '\n  '.join(missing))
    result = {k: str(v.resolve()) for k, v in paths.items()}
    present = {k: v for k, v in optional.items() if v.exists()}
    if not ('console_broker' in present and 'console_helper' in present):
        # The daemon accepts the console broker and native helper only as a pair.
        present.pop('console_broker', None)
        present.pop('console_helper', None)
    result.update({k: str(v.resolve()) for k, v in present.items()})
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--qemu', action='store_true', help='also register the QEMU/HVF GPU runtime')
    parser.add_argument('--memory-budget-mib', type=int)
    parser.add_argument('--dns', action='append', help='default container DNS server (repeatable); needed when host resolvers listen only on loopback')
    args = parser.parse_args()
    root = home()
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    path = root / 'runtime.json'
    runtime = json.loads(path.read_text()) if path.exists() else {}
    runtime['backend'] = require(ROOT / '.local/bin/vm-apple')
    boot = ROOT / '.local/boot'
    runtime['boot'] = {name: require(boot / file) for name, file in
                       (('kernel', 'Image'), ('initrd', 'initramfs.gz'), ('modloop', 'modloop'))}
    if args.qemu:
        runtime['qemu'] = qemu_paths()
    web = ROOT / 'web/server.mjs'
    if (ROOT / 'web/dist/server/server.js').exists():
        runtime['web'] = str(web.resolve())
    if args.dns:
        runtime['dns'] = args.dns
    if args.memory_budget_mib:
        runtime['memory_budget_mib'] = args.memory_budget_mib
    tmp = path.with_suffix('.tmp')
    tmp.write_text(json.dumps(runtime, indent=2) + '\n')
    os.chmod(tmp, 0o600)
    tmp.replace(path)
    json.dump({'runtime': str(path), **runtime}, sys.stdout, indent=2)
    print()


if __name__ == '__main__':
    main()
