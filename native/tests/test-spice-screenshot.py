#!/usr/bin/env python3
"""Actual independent SPICE screenshots; no host-screen or mocked acceptance."""
import argparse
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import uuid

ROOT = Path(__file__).resolve().parents[2]
SELF = Path(__file__).resolve()
BINARY = ROOT / '.local/bin/spice-screenshot'
os.umask(0o077)


def blue_centroid(rgb, width, height):
    xs = ys = count = 0
    for y in range(0, height, 2):
        for x in range(0, width, 2):
            offset = (y * width + x) * 3
            r, g, b = rgb[offset:offset + 3]
            if b > 80 and b > 1.3 * r and b > 1.3 * g:
                xs += x; ys += y; count += 1
    if count < 100:
        raise RuntimeError('Independent Guest blue gear marker absent')
    return (xs / count / width, ys / count / height)


def consumer(socket, prefix):
    evidence = {'success': False, 'scope': 'actual windowless SPICE screenshot, not MCP or performance acceptance'}
    try:
        captures = []
        for index in range(2):
            process = subprocess.run([str(BINARY), socket], capture_output=True, timeout=15)
            Path(prefix + f'.{index}.stderr').write_bytes(process.stderr)
            if process.returncode or len(process.stdout) > 1024 * 1024:
                raise RuntimeError(f'Screenshot failed: exit={process.returncode}, bounded response={process.stdout[:150]!r}')
            frame = json.loads(process.stdout)
            uuid.UUID(frame['frame_id'])
            png = base64.b64decode(frame.pop('data_base64'), validate=True)
            if not png.startswith(b'\x89PNG\r\n\x1a\n') or len(png) != frame['byte_length'] or len(png) > 700 * 1024:
                raise RuntimeError('Invalid image representation/size')
            if (frame['width'], frame['height']) != (1280, 800) or frame['coordinate_space'] != 'guest-physical-pixels' or frame['guest_ui_scale'] is not None:
                raise RuntimeError('Unexpected source coordinates or guessed UI scale')
            image = Path(prefix + f'.{index}.png')
            image.write_bytes(png)
            pixels = subprocess.check_output(['ffmpeg', '-v', 'error', '-i', str(image), '-f', 'rawvideo', '-pix_fmt', 'rgb24', '-'], timeout=15)
            if len(pixels) != 1280 * 800 * 3:
                raise RuntimeError('Independent decoded dimensions differ')
            frame['blue_centroid'] = blue_centroid(pixels, 1280, 800)
            frame['png_sha256'] = hashlib.sha256(png).hexdigest()
            frame['decoded_rgb_sha256'] = hashlib.sha256(pixels).hexdigest()
            captures.append(frame)
        if captures[0]['frame_id'] == captures[1]['frame_id'] or captures[0]['decoded_rgb_sha256'] == captures[1]['decoded_rgb_sha256']:
            raise RuntimeError('Independent reconnect did not capture a new changing frame')
        if captures[1]['captured_monotonic_us'] <= captures[0]['captured_monotonic_us']:
            raise RuntimeError('Capture timeline regressed')
        guest = Path(prefix.removesuffix('-extra') + '-guest.png')
        pixels = subprocess.check_output(['ffmpeg', '-v', 'error', '-i', str(guest), '-f', 'rawvideo', '-pix_fmt', 'rgb24', '-'], timeout=15)
        oracle = blue_centroid(pixels, 1280, 800)
        for frame in captures:
            if max(abs(a-b) for a, b in zip(oracle, frame['blue_centroid'])) > .05:
                raise RuntimeError('Screenshot orientation/crop differs from Guest XWD')
        missing = subprocess.run([str(BINARY), socket + '.missing'], capture_output=True, timeout=15)
        if missing.returncode != 1 or 'error' not in json.loads(missing.stdout):
            raise RuntimeError('Unavailable console did not fail explicitly')
        evidence.update(success=True, captures=captures, guest_blue_centroid=oracle,
                        missing_socket_error=json.loads(missing.stdout)['error'],
                        source_sha256=hashlib.sha256((ROOT/'native/tools/spice-screenshot.m').read_bytes()).hexdigest())
    except Exception as error:
        evidence['error'] = str(error)
    Path(prefix + '.screenshot.json').write_text(json.dumps(evidence, indent=2) + '\n')
    print(json.dumps(evidence, indent=2))
    return 0 if evidence['success'] else 1


if len(sys.argv) == 3 and not sys.argv[1].startswith('-'):
    raise SystemExit(consumer(*sys.argv[1:]))
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--output', type=Path, required=True)
args = parser.parse_args()
if args.output.exists():
    raise SystemExit('Refusing to overwrite evidence')
subprocess.run([str(ROOT/'native/tools/build-spice-screenshot.sh')], check=True)
os.environ['VEYNODE_SPICE_LIBRARY'] = str(ROOT/'.local/spice-reconnect-build/runtime/libspice-server-fixed.dylib')
spec = importlib.util.spec_from_file_location('desktop', ROOT/'scripts/windows-probe-linux-desktop.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
module.BASE = ROOT/'.local/spice-reconnect-build/runtime'
blob = (ROOT/'.local/windows-gpu/glamor-causal-artifacts/modules-fixed/libglamoregl.so').read_bytes()
digest = hashlib.sha256(blob).hexdigest()
if digest != 'ba64c24cb70c953d26ac032633b6bca98577ea415f7abdc32f7f451d9a115f67':
    raise SystemExit('Wrong validated Guest module')
assert module.INIT.count('udevd --daemon') == 1 and module.INIT.count('Xorg :0 vt1') == 1
inject = "mkdir -p /veynode/modules-fixed\nprintf '%s' '" + base64.b64encode(blob).decode() + "' | base64 -d > /veynode/modules-fixed/libglamoregl.so\nprintf '%s\\n' '" + digest + "  /veynode/modules-fixed/libglamoregl.so' | sha256sum -c -\n"
module.INIT = module.INIT.replace('udevd --daemon', inject+'udevd --daemon').replace('Xorg :0 vt1', 'Xorg :0 vt1 -modulepath /veynode/modules-fixed,/usr/lib/xorg/modules')
sys.argv = [str(SELF), '--output', str(args.output), '--capture-cycles', '0', '--extra-consumer', str(SELF)]
raise SystemExit(module.main())
