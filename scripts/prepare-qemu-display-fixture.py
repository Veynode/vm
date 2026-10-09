#!/usr/bin/env python3
"""Prepare a private RAM-only 1440p display fixture; never launch a VM.

Uses the pinned fixed-glamor module already accepted by the Linux pixel probes.
Animated gears are a limited display workload, not a games/CAD benchmark.
"""
import argparse
import base64
import hashlib
import importlib.util
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
GLAMOR_SHA = 'ba64c24cb70c953d26ac032633b6bca98577ea415f7abdc32f7f451d9a115f67'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    output = args.output.absolute()
    glamor = ROOT / '.local/windows-gpu/glamor-causal-artifacts/modules-fixed/libglamoregl.so'
    module_bytes = glamor.read_bytes()
    if hashlib.sha256(module_bytes).hexdigest() != GLAMOR_SHA:
        raise ValueError('wrong pinned fixed-glamor module')
    output.mkdir(mode=0o700, exist_ok=False)
    spec = importlib.util.spec_from_file_location('qemu_fixture', ROOT / 'scripts/test-control-qemu.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)

    def transform(init):
        for anchor in ['udevd --daemon', 'Xorg :0 vt1', 'glxgears -geometry 640x400+20+20']:
            if init.count(anchor) != 1:
                raise ValueError('fixture integration anchor changed')
        payload = base64.b64encode(module_bytes).decode('ascii')
        install = ("mkdir -p /veynode/modules-fixed\nprintf '%s' '" + payload +
                   "' | base64 -d > /veynode/modules-fixed/libglamoregl.so\n" +
                   f"echo '{GLAMOR_SHA}  /veynode/modules-fixed/libglamoregl.so' | sha256sum -c -\n")
        init = init.replace('udevd --daemon', install + 'udevd --daemon')
        init = init.replace('Xorg :0 vt1', 'Xorg :0 vt1 -modulepath /veynode/modules-fixed,/usr/lib/xorg/modules')
        return init.replace('glxgears -geometry 640x400+20+20', 'glxgears -geometry 2400x1300+20+20')

    initrd, metadata = module.fixture(output, init_transform=transform)
    config = {'id': 'native-display-1440p', 'backend': 'qemu-hvf', 'guest': 'linux',
              'cpus': 2, 'memory_mib': 1536, 'window': True, 'width': 2560, 'height': 1440,
              'kernel': str(module.MEDIA / 'Image'), 'initrd': str(initrd),
              'command_line': 'console=ttyAMA0 rdinit=/veynode/gpu-init',
              'serial_log': str(output / 'serial.log')}
    metadata.update({'fixed_glamor_sha256': GLAMOR_SHA, 'requested_display': [2560, 1440],
                     'workload': 'glxgears2400x1300; xterm updates1Hz',
                     'actual_guest_geometry_verified': False, 'performance_measured': False})
    (output / 'config.json').write_text(json.dumps(config, indent=2) + '\n')
    (output / 'fixture.json').write_text(json.dumps(metadata, indent=2) + '\n')
    print(json.dumps({'prepared': str(output), 'initrd_sha256': metadata['initrd_sha256'], 'vm_started': False}))


if __name__ == '__main__':
    main()
