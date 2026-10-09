#!/usr/bin/env python3
"""Prepare a fresh RAM-only Linux input oracle with verified accelerated pixels.

No VM starts. The generic lifecycle fixture retains its intentional stock defect;
this input fixture applies the previously accepted, narrowly scoped glamor patch.
"""
import argparse
import base64
import hashlib
import importlib.util
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
GLAMOR_SHA = 'ba64c24cb70c953d26ac032633b6bca98577ea415f7abdc32f7f451d9a115f67'
GLAMOR = ROOT / '.local/windows-gpu/glamor-causal-artifacts/modules-fixed/libglamoregl.so'
PROBE = ROOT / 'scripts/linux-xrender-probe.c'
XORG = 'Xorg :0 vt1 -config /veynode/xorg.conf -ac -noreset -nolisten tcp -logfile /veynode/Xorg.log &'


def graphics_transform(init, module_bytes, probe_bytes):
    if hashlib.sha256(module_bytes).hexdigest() != GLAMOR_SHA:
        raise ValueError('wrong pinned fixed-glamor module')
    for anchor in ['libx11-dev linux-headers', 'udevd --daemon', XORG, 'twm &']:
        if init.count(anchor) != 1:
            raise ValueError('fixture integration anchor changed: ' + anchor)
    init = init.replace('libx11-dev linux-headers', 'libx11-dev linux-headers libxrender-dev mesa-dev')
    # The private module was built against this exact Xorg release/ABI.
    init = init.replace('xorg-server ', 'xorg-server=21.1.23-r0 ')
    install = "mkdir -p /veynode/modules-fixed /veynode/input-pixels\n"
    for path, data in [('/veynode/modules-fixed/libglamoregl.so', module_bytes),
                       ('/veynode/input-xrender-probe.c', probe_bytes)]:
        install += "printf '%s' '" + base64.b64encode(data).decode('ascii') + "' | base64 -d > " + path + '\n'
        install += f"echo '{hashlib.sha256(data).hexdigest()}  {path}' | sha256sum -c -\n"
    install += 'timeout 30 cc -O2 -Wall -Wextra /veynode/input-xrender-probe.c -o /veynode/input-xrender-probe -lX11 -lXrender -lGL\n'
    init = init.replace('udevd --daemon', install + 'udevd --daemon')
    init = init.replace(XORG, XORG.replace('Xorg :0 vt1', 'Xorg :0 vt1 -modulepath /veynode/modules-fixed,/usr/lib/xorg/modules') + '\nxorg_pid=$!')
    guard = r'''kill -0 "$xorg_pid"
echo VEYNODE_INPUT_MODULE_MAP_BEGIN
grep '/veynode/modules-fixed/libglamoregl.so' /proc/$xorg_pid/maps
echo VEYNODE_INPUT_MODULE_MAP_END
if grep -q '/usr/lib/xorg/modules/libglamoregl.so' /proc/$xorg_pid/maps; then exit 1; fi
apk info -v xorg-server mesa-dri-gallium
set +e
timeout 30 /veynode/input-xrender-probe /veynode/input-pixels > /veynode/input-xrender.log 2>&1
pixel_status=$?
set -e
cat /veynode/input-xrender.log
echo VEYNODE_INPUT_PIXEL_EXIT=$pixel_status
test "$pixel_status" = 0
test "$(grep -c '^XRENDER_CASE ' /veynode/input-xrender.log)" = 15
test "$(grep '^XRENDER_CASE ' /veynode/input-xrender.log | grep -c '"passed":true')" = 15
grep -Fx 'XRENDER_SUMMARY {"cases":15,"failed":0,"passed":true}' /veynode/input-xrender.log
grep 'VEYNODE_GLAMOR_GLYPH variant=explicit-one' /veynode/Xorg.log
echo VEYNODE_INPUT_GRAPHICS_READY=15
'''
    return init.replace('twm &', guard + 'twm &')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--directory', type=Path, required=True)
    parser.add_argument('--id', default='browser-input')
    parser.add_argument('--window', action='store_true')
    parser.add_argument('--width', type=int, default=1280)
    parser.add_argument('--height', type=int, default=800)
    args = parser.parse_args()
    if not args.id or len(args.id) > 64 or not all(c.isascii() and (c.isalnum() or c in '-_') for c in args.id):
        parser.error('invalid fixture VM ID')
    if not (640 <= args.width <= 3840 and 480 <= args.height <= 2160):
        parser.error('display dimensions outside fixture bounds')
    module_bytes, probe_bytes = GLAMOR.read_bytes(), PROBE.read_bytes()
    if hashlib.sha256(module_bytes).hexdigest() != GLAMOR_SHA:
        raise ValueError('wrong pinned fixed-glamor module')
    directory = args.directory.resolve()
    directory.mkdir(mode=0o700, parents=True, exist_ok=False)
    spec = importlib.util.spec_from_file_location('qemu_fixture', ROOT / 'scripts/test-control-qemu.py')
    fixture = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixture)
    generated = []

    def transform(init):
        init = graphics_transform(init, module_bytes, probe_bytes)
        generated.append(init.encode())
        return init

    initrd, metadata = fixture.fixture(directory, input_oracle=ROOT / 'scripts/linux-control-input-oracle.c',
                                       input_oracle_seconds=1800, init_transform=transform)
    config = dict(id=args.id, backend='qemu-hvf', guest='linux', cpus=2, memory_mib=1536,
                  kernel=str(fixture.MEDIA / 'Image'), initrd=str(initrd), window=args.window,
                  width=args.width, height=args.height,
                  command_line='console=ttyAMA0 rdinit=/veynode/gpu-init', serial_log=str(directory / 'serial.log'))
    config_bytes = (json.dumps(config, indent=2) + '\n').encode()
    metadata.update({'fixed_glamor_sha256': GLAMOR_SHA,
                     'pixel_probe_sha256': hashlib.sha256(probe_bytes).hexdigest(),
                     'init_sha256': hashlib.sha256(generated[0]).hexdigest(),
                     'config_sha256': hashlib.sha256(config_bytes).hexdigest(),
                     'preparer_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                     'input_oracle_sha256': hashlib.sha256((ROOT / 'scripts/linux-control-input-oracle.c').read_bytes()).hexdigest(),
                     'pixel_cases_required': 15, 'oracle_seconds': 1800,
                     'graphics_ready_marker': 'VEYNODE_INPUT_GRAPHICS_READY=15',
                     'actual_pixels_verified': False, 'vm_started': False})
    (directory / 'config.json').write_bytes(config_bytes)
    (directory / 'fixture.json').write_text(json.dumps(metadata, indent=2) + '\n')
    print(directory / 'config.json')


if __name__ == '__main__':
    main()
