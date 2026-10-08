#!/usr/bin/env python3
"""Actual RAM-only Xorg desktop with virgl, plus windowless SPICE capture.

This diagnostic does not establish correct desktop rendering or performance.

Uses the existing Alpine kernel/module fixture read-only. Packages are fetched
inside a fresh RAM-only guest with user-mode NAT and no forwarded ports. No
existing VM is touched. This is a pixel correctness test, not a benchmark.
"""
import argparse
import datetime
import gzip
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import time
import re
import plistlib
import uuid
import shutil
import stat
import struct
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]
BASE = ROOT / '.local/windows-gpu'
MEDIA = ROOT / '.local/media/alpine-3.24.2'
ROOTFS_SHA = 'b17a57958e29735ff0e6e64254d958e65903687a70ca30cc430b33a965ad49d7'
INIT = '''#!/bin/sh
set -eu
trap 'echo VEYNODE_DESKTOP_FAIL; poweroff -f' EXIT
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /tmp /run /root /dev/pts
mount -t devpts devpts /dev/pts
mount -t tmpfs tmpfs /tmp
test -e /lib/modules || ln -s /usr/lib/modules /lib/modules
modprobe virtio_net
modprobe virtio_gpu
ip link set lo up
ip link set eth0 up
udhcpc -i eth0 -n -q -s /veynode/dhcp
printf 'https://dl-cdn.alpinelinux.org/alpine/v3.23/main\nhttps://dl-cdn.alpinelinux.org/alpine/v3.23/community\n' > /etc/apk/repositories
apk add --no-cache --no-progress mesa-utils mesa-dri-gallium mesa-egl mesa-gles mesa-demos xwd xorg-server xf86-input-libinput xterm twm font-dejavu eudev
udevd --daemon
udevadm trigger
udevadm settle
cat > /veynode/xorg.conf <<'XORG'
Section "Device"
 Identifier "Virtio"
 Driver "modesetting"
 Option "AccelMethod" "glamor"
EndSection
Section "ServerFlags"
 Option "AutoAddDevices" "false"
EndSection
XORG
export DISPLAY=:0 XDG_RUNTIME_DIR=/run
Xorg :0 vt1 -config /veynode/xorg.conf -ac -noreset -nolisten tcp -logfile /veynode/Xorg.log &
sleep 4
cat /veynode/Xorg.log
glxinfo -B > /veynode/glxinfo
cat /veynode/glxinfo
grep -q "Device: virgl" /veynode/glxinfo
grep -q "Accelerated: yes" /veynode/glxinfo
twm &
xterm -geometry 72x12+20+420 -title 'Veynode GPU desktop console' -e sh -c 'while true; do date; sleep 1; done' &
glxgears -geometry 640x400+20+20 &
gears=$!
sleep 2
kill -0 "$gears"
xwd -root -silent -out /veynode/desktop.xwd
printf 'VEYNODE_XWD_BEGIN\\n'
gzip -c /veynode/desktop.xwd | base64
printf 'VEYNODE_XWD_END\\n'
echo VEYNODE_DESKTOP_READY
sleep 300
trap - EXIT
poweroff -f
'''
DHCP = '''#!/bin/sh
case "$1" in
 bound|renew)
  ip addr replace "$ip/24" dev "$interface"
  ip link set "$interface" up
  ip route replace default via "$router"
  : > /etc/resolv.conf
  for server in $dns; do echo "nameserver $server" >> /etc/resolv.conf; done;;
esac
'''


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b''):
            h.update(chunk)
    return h.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--ready-timeout', type=int, default=180, help='Seconds to wait for owned Guest diagnostic setup (1..900)')
    parser.add_argument('--output', type=Path, default=BASE / 'linux-desktop-evidence.json')
    parser.add_argument('--xorg-disable-shader4', action='store_true', help='Diagnostic only: hide GL_EXT_gpu_shader4 from Xorg to isolate its accelerated core-font path')
    parser.add_argument('--gl-font-api', choices=['glx', 'gles', 'glx-points'], default='glx', help='Actual GL API for isolated font probe; never force unsupported extensions')
    parser.add_argument('--gl-font-probe', action='store_true', help='Run isolated integer-atlas/bit-operation/instanced-GL pixel cases instead of XRender cases')
    parser.add_argument('--xrender-probe', action='store_true', help='Run deterministic actual Guest X11/XRender pixel cases; no desktop capture acceptance')
    parser.add_argument('--xorg-accel', choices=['glamor', 'none'], default='glamor')
    parser.add_argument('--capture-cycles', type=int, choices=[0, 1, 2], default=2, help='Default verifies separate consumer reconnect; 0 requires extra consumer')
    parser.add_argument('--extra-consumer', type=Path, help='Run executable UNIX_SOCKET OUTPUT_PREFIX after screenshot consumers')
    parser.add_argument('--extra-consumer-timeout', type=int, default=90, help='Owned extra consumer timeout in seconds (1..300)')
    parser.add_argument('--export-utm', type=Path, help='Create new native-viewer fixture and exit; refuses overwrite')
    args = parser.parse_args()
    if not 1 <= args.extra_consumer_timeout <= 300:
        parser.error('extra-consumer-timeout must be 1..300')
    if not 1 <= args.ready_timeout <= 900:
        parser.error('--ready-timeout must be 1..900')
    if args.gl_font_probe:
        args.xrender_probe = True
    if args.capture_cycles == 0 and not args.extra_consumer and not args.export_utm and not args.xrender_probe:
        parser.error('capture-cycles 0 requires extra-consumer')
    BASE.mkdir(parents=True, exist_ok=True)
    source = BASE / 'alpine-minirootfs-3.23.6-aarch64.tar.gz'
    if not source.exists():
        part = source.with_suffix('.part')
        subprocess.run(['curl', '-fL', '--retry', '2', 'https://dl-cdn.alpinelinux.org/alpine/v3.23/releases/aarch64/' + source.name, '-o', str(part)], check=True)
        if digest(part) != ROOTFS_SHA:
            raise SystemExit('Alpine rootfs SHA-256 mismatch')
        part.rename(source)
    if digest(source) != ROOTFS_SHA:
        raise SystemExit('Alpine rootfs SHA-256 mismatch')
    for path in (MEDIA / 'Image', MEDIA / 'veynode-initramfs.gz', BASE / 'probe-loader'):
        if not path.is_file():
            raise SystemExit(f'Missing prerequisite: {path}; see docs/decisions/0002-linux-graphics.md')
    spec = importlib.util.spec_from_file_location('fixture', ROOT / 'scripts/build-linux-fixture.py')
    fixture = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixture)
    entries = []
    with tarfile.open(source) as archive:
        for member in archive.getmembers():
            name = member.name.removeprefix('./')
            if not name or name == '.':
                continue
            if name.startswith('/') or '..' in Path(name).parts:
                raise ValueError('Unsafe rootfs member path')
            if member.isdir():
                mode, data = stat.S_IFDIR | member.mode, b''
            elif member.issym():
                mode, data = stat.S_IFLNK | member.mode, member.linkname.encode()
            elif member.isfile():
                mode, data = stat.S_IFREG | member.mode, archive.extractfile(member).read()
            else:
                raise ValueError('Unexpected rootfs member type')
            entries.append(fixture.entry(name, data, mode))
    init = INIT
    if args.xorg_disable_shader4:
        init = init.replace('Xorg :0 vt1', 'MESA_EXTENSION_OVERRIDE=-GL_EXT_gpu_shader4 Xorg :0 vt1')
    if args.xrender_probe:
        init = init.replace('apk add --no-cache --no-progress', 'cat /proc/mounts\nmount -t tmpfs -o remount,size=1100M rootfs /\ncat /proc/mounts\napk add --no-cache --no-progress')
        init = init.replace('font-dejavu eudev', 'font-dejavu eudev build-base libx11-dev libxrender-dev mesa-dev')
        init = init[:init.index('twm &')] + '''mkdir -p /veynode/xrender
cc -O2 -Wall -Wextra /veynode/xrender-probe.c -o /veynode/xrender-probe -lX11 -lXrender -lGL
set +e
/veynode/xrender-probe /veynode/xrender
status=$?
set -e
echo XRENDER_EXIT=$status
tar -czf /veynode/xrender.tgz -C /veynode/xrender .
printf 'VEYNODE_XRENDER_ARCHIVE_BEGIN\\n'
base64 /veynode/xrender.tgz
printf 'VEYNODE_XRENDER_ARCHIVE_END\\n'
echo VEYNODE_DESKTOP_READY
sleep 300
trap - EXIT
poweroff -f
'''
    if args.gl_font_probe:
        init = init.replace('/veynode/xrender-probe.c', '/veynode/gl-font-probe.c')
        if args.gl_font_api == 'glx-points':
            init = init.replace('-lX11 -lXrender -lGL', '-DPOINTS_ONLY -lX11 -lXrender -lGL')
        if args.gl_font_api == 'gles':
            init = init.replace('-lX11 -lXrender -lGL', '-DUSE_GLES -lEGL -lGLESv2')
            init = init.replace('/veynode/xrender-probe /veynode/xrender', 'EGL_PLATFORM=surfaceless /veynode/xrender-probe /veynode/xrender')
    for name, data, mode in [('gpu-init', init.replace('\"glamor\"', '\"' + args.xorg_accel + '\"').encode(), 0o100755), ('dhcp', DHCP.encode(), 0o100755), ('egl-test.c', (ROOT / 'scripts/windows-linux-egl-test.c').read_bytes(), 0o100644), ('xrender-probe.c', (ROOT / 'scripts/linux-xrender-probe.c').read_bytes(), 0o100644), ('gl-font-probe.c', (ROOT / 'scripts/linux-gl-font-probe.c').read_bytes(), 0o100644)]:
        entries.append(fixture.entry('veynode/' + name, data, mode))
    entries.append(fixture.entry('TRAILER!!!', b''))
    app = BASE / 'UTM.app/Contents'
    log_path = args.output.with_suffix('.serial.log').resolve()
    log_path.parent.mkdir(parents=True, exist_ok=True)
    initrd_bytes = (MEDIA / 'veynode-initramfs.gz').read_bytes() + gzip.compress(b''.join(entries), mtime=0)
    if args.export_utm:
        export_utm(args.export_utm, initrd_bytes)
        return 0
    if not args.xrender_probe and not (BASE / 'spice-capture').is_file():
        raise SystemExit('Run scripts/windows-build-spice-capture.sh first')
    result = {'timestamp_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
              'backend': 'UTM 5.0.5 QEMU 10.0.12 HVF virtio-gpu-gl ANGLE Metal',
              'kernel_sha256': digest(MEDIA / 'Image'), 'rootfs_sha256': ROOTFS_SHA,
              'ram_mib': 1536, 'vcpus': 2, 'xorg_accel': args.xorg_accel,
              'xorg_extension_override': '-GL_EXT_gpu_shader4' if args.xorg_disable_shader4 else None,
              'gl_font_api': args.gl_font_api if args.gl_font_probe else None,
              'pixel_probe_kind': 'gl-font' if args.gl_font_probe else ('xrender' if args.xrender_probe else None),
              'xrender_probe_source_sha256': digest(ROOT / ('scripts/linux-gl-font-probe.c' if args.gl_font_probe else 'scripts/linux-xrender-probe.c')) if args.xrender_probe else None,
              'native_view_opened': False, 'performance_measured': False,
              'desktop_visual_correctness': 'requires image inspection; not inferred from capture success'}
    prefix = args.output.resolve().with_suffix('')
    with tempfile.TemporaryDirectory(prefix='veynode-desktop-') as temp:
        temp = Path(temp)
        initrd = temp / 'desktop-initramfs.gz'
        initrd.write_bytes(initrd_bytes)
        qmp_path = temp / 'qmp.sock'
        command = [str(BASE / 'probe-loader'), str(app / 'Frameworks/qemu-aarch64-softmmu.framework/Versions/A/qemu-aarch64-softmmu'), '-L', str(app / 'Resources/qemu'), '-machine', 'virt,accel=hvf', '-cpu', 'host', '-m', '1536M', '-smp', '2', '-nodefaults', '-nic', 'user,model=virtio-net-pci', '-display', 'none', '-serial', 'stdio', '-monitor', 'none', '-qmp', f'unix:{qmp_path},server=on,wait=off', '-spice', f'unix=on,addr={temp}/spice.sock,disable-ticketing=on,gl=es', '-device', 'virtio-gpu-gl-pci', '-kernel', str(MEDIA / 'Image'), '-initrd', str(initrd), '-append', 'console=ttyAMA0 rdinit=/veynode/gpu-init']
        env = os.environ.copy()
        env['ANGLE_DEFAULT_PLATFORM'] = 'metal'
        with log_path.open('w') as log:
            proc = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=log, stderr=log, env=env, start_new_session=True)
            try:
                deadline = time.monotonic() + args.ready_timeout
                while time.monotonic() < deadline:
                    serial = log_path.read_text(errors='replace')
                    if 'VEYNODE_DESKTOP_FAIL' in serial or proc.poll() is not None:
                        raise RuntimeError('Guest setup failed; see serial log')
                    if 'VEYNODE_DESKTOP_READY' in serial:
                        break
                    time.sleep(1)
                else:
                    raise TimeoutError('Guest desktop readiness timed out')
                result['desktop_ready'] = True
                if args.xrender_probe:
                    result['xrender_cases'] = [json.loads(line.split('XRENDER_CASE ', 1)[1]) for line in serial.splitlines() if 'XRENDER_CASE ' in line]
                    summary_lines = [line.split('XRENDER_SUMMARY ', 1)[1] for line in serial.splitlines() if 'XRENDER_SUMMARY ' in line]
                    result['xrender_summary'] = json.loads(summary_lines[-1]) if summary_lines else None
                    result['xrender_exit_code'] = int(re.search(r'XRENDER_EXIT=(\d+)', serial)[1])
                    archive = base64.b64decode(serial.split('VEYNODE_XRENDER_ARCHIVE_BEGIN', 1)[1].split('VEYNODE_XRENDER_ARCHIVE_END', 1)[0])
                    archive_path = Path(str(prefix) + '-cases.tar.gz')
                    archive_path.write_bytes(archive)
                    case_dir = Path(str(prefix) + '-cases')
                    case_dir.mkdir(exist_ok=True)
                    with tarfile.open(archive_path) as case_archive:
                        for member in case_archive.getmembers():
                            name = member.name.removeprefix('./')
                            if member.isdir(): continue
                            if '/' in name or not name.endswith('.ppm') or not member.isfile():
                                raise ValueError('Unexpected XRender artifact member')
                            ppm = case_dir / name
                            ppm.write_bytes(case_archive.extractfile(member).read())
                            subprocess.run(['/usr/bin/sips', '-s', 'format', 'png', str(ppm), '--out', str(ppm.with_suffix('.png'))], check=True, stdout=subprocess.DEVNULL)
                    qmp = QMP(qmp_path)
                    result['qmp_status_after_pixel_cases'] = qmp.run('query-status')
                    qmp.run('quit'); qmp.close()
                    result['qemu_exit_code'] = proc.wait(timeout=10)
                    result['xrender_pixel_tests_passed'] = result['xrender_exit_code'] == 0 and bool(result['xrender_summary']) and result['xrender_summary']['passed']
                else:
                    encoded_xwd = serial.split('VEYNODE_XWD_BEGIN', 1)[1].split('VEYNODE_XWD_END', 1)[0]
                    xwd = gzip.decompress(base64.b64decode(encoded_xwd))
                    Path(str(prefix) + '-guest.xwd').write_bytes(xwd)
                    guest_ppm = Path(str(prefix) + '-guest.ppm')
                    result['guest_xwd'] = xwd_to_ppm(xwd, guest_ppm)
                    subprocess.run(['/usr/bin/sips', '-s', 'format', 'png', str(guest_ppm), '--out', str(prefix) + '-guest.png'], check=True, stdout=subprocess.DEVNULL)
                    qmp = QMP(qmp_path)
                    result['qmp_status_before'] = qmp.run('query-status')
                    ppm = Path(str(prefix) + '-qmp.ppm')
                    qmp.run('screendump', {'filename': str(ppm)})
                    raw = ppm.read_bytes()
                    header = re.match(rb'P6\s+(\d+)\s+(\d+)\s+255\s', raw)
                    if not header:
                        raise ValueError('Unexpected QMP PPM encoding')
                    result['qmp_frame'] = {'width': int(header[1]), 'height': int(header[2]),
                                           'any_rgb_nonzero': any(raw[header.end():]), 'sha256': digest(ppm)}
                    captures = []
                    for cycle in range(args.capture_cycles):
                        # Disconnect then reconnect a completely separate consumer to the same live VM.
                        capture = subprocess.run([str(BASE / 'spice-capture'), str(temp / 'spice.sock'), str(prefix) + f'-spice-{cycle}'], capture_output=True, text=True, timeout=55)
                        Path(str(prefix) + f'-spice-{cycle}.log').write_text(capture.stdout + capture.stderr)
                        frames = []
                        for line in capture.stdout.splitlines():
                            if line.startswith('FRAME '):
                                frames.append(dict(part.split('=', 1) for part in line.split()[1:]))
                        valid = capture.returncode == 0 and len(frames) == 2 and all(int(f['nonblack']) > 1000 for f in frames) and frames[0]['rgb_hash'] != frames[1]['rgb_hash']
                        result['spice_capture'] = captures
                        captures.append({'cycle': cycle, 'exit_code': capture.returncode, 'changing_nonblack_frames': valid, 'frames': frames})
                        if not valid:
                            raise RuntimeError('SPICE capture missing nonblack/changing frames')
                    result['spice_capture'] = captures
                    result['consumer_reconnect_tested'] = args.capture_cycles == 2
                    if args.extra_consumer:
                        extra = subprocess.run([str(args.extra_consumer.resolve()), str(temp / 'spice.sock'), str(prefix) + '-extra'], capture_output=True, text=True, timeout=args.extra_consumer_timeout)
                        Path(str(prefix) + '-extra.log').write_text(extra.stdout + extra.stderr)
                        result['extra_consumer_exit_code'] = extra.returncode
                        if extra.returncode != 0:
                            raise RuntimeError('Extra SPICE consumer failed; see extra log')
                    result['qmp_status_after_reconnect'] = qmp.run('query-status')
                    result['capture_path_passed'] = result['qmp_status_after_reconnect']['status'] == 'running'
                    qmp.run('quit')
                    qmp.close()
                    result['qemu_exit_code'] = proc.wait(timeout=10)
            except (Exception, KeyboardInterrupt) as error:
                result['error'] = str(error)
                result['capture_path_passed'] = False
            finally:
                if proc.poll() is None:
                    os.killpg(proc.pid, signal.SIGTERM)
                    try: proc.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(proc.pid, signal.SIGKILL)
                        proc.wait()
    serial = log_path.read_text(errors='replace')
    result['renderer_evidence'] = [line.strip() for line in serial.splitlines() if any(key in line for key in ('Device: virgl', 'Accelerated:', 'OpenGL', '[drm] features:', 'cap sets:', 'VEYNODE_DESKTOP', 'XRENDER_GL_RENDERER', 'XRENDER_VERSION', 'GLFONT_'))]
    result['serial_log'] = str(log_path.relative_to(ROOT)) if log_path.is_relative_to(ROOT) else str(log_path)
    args.output.write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result, indent=2))
    return 0 if result.get('xrender_pixel_tests_passed' if args.xrender_probe else 'capture_path_passed') else 1


def xwd_to_ppm(data, output):
    # XWD header is network endian; pixel byte order and row stride are independent.
    header = struct.unpack('>25I', data[:100])
    size, version, fmt, depth, width, height, xoff, order, unit, bitorder, pad, bpp, stride, visual, red, green, blue, bits, cmap, colors, *_ = header
    if version != 7 or fmt != 2 or bpp not in (24, 32) or (red, green, blue) != (0xff0000, 0xff00, 0xff):
        raise ValueError('Unsupported XWD pixel layout')
    step = bpp // 8
    pixels = data[size + 12 * colors:]
    if len(pixels) < height * stride or stride < width * step:
        raise ValueError('Truncated XWD')
    rgb = bytearray()
    for y in range(height):
        for x in range(width):
            offset = y * stride + x * step
            pixel = int.from_bytes(pixels[offset:offset + step], 'little' if order == 0 else 'big')
            rgb.extend(((pixel >> 16) & 255, (pixel >> 8) & 255, pixel & 255))
    output.write_bytes(f'P6\n{width} {height}\n255\n'.encode() + rgb)
    return {'width': width, 'height': height, 'bits_per_pixel': bpp, 'bytes_per_row': stride,
            'rgb_sha256': hashlib.sha256(rgb).hexdigest()}


class QMP:
    def __init__(self, path):
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(10)
        self.socket.connect(str(path))
        self.file = self.socket.makefile('rwb')
        json.loads(self.file.readline())
        self.run('qmp_capabilities')

    def run(self, command, arguments=None):
        request = {'execute': command}
        if arguments: request['arguments'] = arguments
        self.file.write(json.dumps(request).encode() + b'\n')
        self.file.flush()
        while True:
            response = json.loads(self.file.readline())
            if 'error' in response: raise RuntimeError(response['error'])
            if 'return' in response: return response['return']

    def close(self):
        self.file.close()
        self.socket.close()


def export_utm(bundle, initrd_bytes):
    bundle = bundle.resolve()
    if bundle.exists(): raise SystemExit('Refusing to overwrite an existing UTM fixture')
    data = bundle / 'Data'
    data.mkdir(parents=True)
    (data / 'desktop-initramfs.gz').write_bytes(initrd_bytes)
    shutil.copyfile(MEDIA / 'Image', data / 'Image')
    config = {
        'Backend': 'QEMU', 'ConfigurationVersion': 4,
        'Information': {'Name': 'Veynode Linux GPU Desktop Probe', 'IconCustom': False, 'UUID': str(uuid.uuid4()), 'Notes': 'Disposable RAM-only project diagnostic'},
        'System': {'Architecture': 'aarch64', 'Target': 'virt', 'CPU': 'default', 'CPUFlagsAdd': [], 'CPUFlagsRemove': [], 'CPUCount': 2, 'MemorySize': 1536},
        'QEMU': {'DebugLog': True, 'UEFIBoot': False, 'RNGDevice': True, 'BalloonDevice': False, 'TPMDevice': False, 'Hypervisor': True, 'TSO': False, 'AdditionalArguments': ['-append', '\"console=ttyAMA0 rdinit=/veynode/gpu-init\"', '-serial', 'file:veynode-desktop-serial.log']},
        'Input': {'UsbBusSupport': '3.0', 'UsbSharing': False, 'MaximumUsbShare': 0},
        'Sharing': {'DirectoryShareMode': 'None', 'DirectoryShareReadOnly': True, 'ClipboardSharing': False},
        'Display': [{'Hardware': 'virtio-gpu-gl-pci', 'DynamicResolution': False, 'UpscalingFilter': 'Linear', 'DownscalingFilter': 'Linear', 'NativeResolution': False}],
        'Drive': [{'ImageName': name, 'ImageType': kind, 'Interface': 'None', 'InterfaceVersion': 1, 'Identifier': str(uuid.uuid4()), 'ReadOnly': True} for name, kind in [('Image', 'LinuxKernel'), ('desktop-initramfs.gz', 'LinuxInitrd')]],
        'Network': [{'Mode': 'Emulated', 'Hardware': 'virtio-net-pci', 'MacAddress': '52:54:00:51:de:01', 'IsolateFromHost': False, 'PortForward': []}], 'Serial': [], 'Sound': []}
    with (bundle / 'config.plist').open('wb') as output: plistlib.dump(config, output)
    print(bundle)


if __name__ == '__main__':
    raise SystemExit(main())
