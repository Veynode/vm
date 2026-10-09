#!/usr/bin/env python3
"""Prepare a private diskless Venus diagnostic; --run explicitly boots one VM.

This preserves the original 256M host aperture and 1GiB guest allocation. It adds
QEMU guest_errors and virgl debug logging, and pins the old Vulkan APK versions.
It is an initialization diagnostic, never graphical/performance acceptance.
"""
import argparse
import gzip
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]
BASE = ROOT / '.local/windows-gpu'
MEDIA = ROOT / '.local/media/alpine-3.24.2'
ROOTFS_SHA = '9bf70a7f18ea44094cbb5f70c58f9af129c8214745743db0e68e5502cc2ce773'
INIT = '''#!/bin/sh
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
fail() { printf '\\nVULKAN_SETUP_FAIL %s\\n' "$1"; poweroff -f; exit 1; }
mount -t proc proc /proc || fail proc
mount -t sysfs sysfs /sys || fail sysfs
mount -t devtmpfs devtmpfs /dev || fail devtmpfs
mkdir -p /run /tmp /root
mount -t tmpfs tmpfs /tmp || fail tmpfs
ln -s /usr/lib/modules /lib/modules
modprobe virtio_net || fail virtio_net
modprobe virtio_gpu || fail virtio_gpu
ip link set lo up
ip link set eth0 up || fail link
udhcpc -i eth0 -n -q -s /veynode/dhcp || fail dhcp
apk add --no-cache --no-progress mesa-vulkan-virtio=26.1.6-r0 vulkan-tools=1.4.347-r0 vulkan-loader=1.4.347-r0 || fail packages
printf '\\nVULKAN_PACKAGES_BEGIN\\n'
apk info -v
printf 'VULKAN_PACKAGES_END\\n'
export XDG_RUNTIME_DIR=/run
export VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json
[ -f "$VK_DRIVER_FILES" ] || fail virtio_icd
printf 'VULKAN_INSTANCE_BEGIN\\n'
vulkaninfo --summary
rc=$?
printf '\\nVULKAN_INSTANCE_EXIT %s\\n' "$rc"
sync
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
PIXEL_RUN = '''[ "$rc" -eq 0 ] || fail instance
for stage in vert frag; do
 glslangValidator -V --target-env vulkan1.1 -S "$stage" /veynode/triangle."$stage" -o /veynode/triangle."$stage".spv || fail shader_compiler
done
gcc -std=c11 -Wall -Wextra -Werror /veynode/triangle.c -o /veynode/triangle -l:libvulkan.so.1 || fail c_compiler
/veynode/triangle || fail pixel_oracle
printf 'VK_PIXEL_IMAGE_BEGIN\\n'
base64 /veynode/vulkan-triangle.ppm
printf 'VK_PIXEL_IMAGE_END\\n'
sync
poweroff -f'''


def digest(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b''):
            h.update(chunk)
    return h.hexdigest()


def classify(text, code):
    setup = re.findall(r'^VULKAN_SETUP_FAIL (\w+)$', text, re.M)
    exits = re.findall(r'^VULKAN_INSTANCE_EXIT (\d+)$', text, re.M)
    errors = [line for line in text.splitlines() if any(x in line for x in
              ('proxy: failed', 'blob create error', 'resource does not have info', 'ctrl 0x',
               'failed to get blob', 'failed to get reply', 'invalid reply for blob',
               'invalid fd type', 'shm size(', 'vkCreateInstance failed'))]
    return {'setup_failure': setup, 'instance_exit': int(exits[0]) if len(exits) == 1 else None,
            'initialization_succeeded': code == 0 and not setup and exits == ['0']
            and 'deviceName' in text and not re.search(r'llvmpipe|lavapipe|SwiftShader', text, re.I),
            'first_backend_error': errors[0] if errors else None, 'backend_errors': errors[:32],
            'graphics_application_validated': False, 'performance_measured': False}


def classify_pixels(serial, result):
    cases = re.findall(r'^VK_PIXEL_CASE frame=(\d+) tested=(\d+) mismatches=(\d+)$', serial, re.M)
    result['pixel_cases'] = cases
    result['raster_pixel_oracle_passed'] = (result['initialization_succeeded'] and
        serial.splitlines().count('VK_PIXEL_PASS frames=3 size=64x64') == 1 and len(cases) == 3 and
        [c[0] for c in cases] == ['0', '1', '2'] and
        all(int(c[1]) == 3522 and c[2] == '0' for c in cases))
    return result


def prepare(out, server_manifest=None, ipa_granule=0, pixels=False):
    rootfs = MEDIA / 'alpine-minirootfs-3.24.2-aarch64.tar.gz'
    if digest(rootfs) != ROOTFS_SHA:
        raise ValueError('rootfs pin mismatch')
    app = BASE / 'UTM.app/Contents'
    qemu = app / 'Frameworks/qemu-aarch64-softmmu.framework/Versions/A/qemu-aarch64-softmmu'
    server = app / 'XPCServices/QEMUHelper.xpc/Contents/MacOS/QEMURenderServer.app/Contents/MacOS/QEMURenderServer'
    if server_manifest is not None:
        metadata = json.loads(server_manifest.read_text())
        server = Path(metadata['binary'])
        if digest(server) != metadata['binary_sha256']:
            raise ValueError('standalone render-server hash mismatch')
        for name, expected in metadata['frameworks'].items():
            library = app/'Frameworks'/name/'Versions/A'/name.removesuffix('.framework')
            if digest(library) != expected:
                raise ValueError('standalone render-server framework mismatch')
    icd = app / 'Resources/vulkan/icd.d/MoltenVK_icd.json'
    inputs = [rootfs, MEDIA/'Image', MEDIA/'veynode-initramfs.gz', BASE/'probe-loader', qemu, server,
              app/'Frameworks/virglrenderer.1.framework/Versions/A/virglrenderer.1',
              app/'Frameworks/vulkan.1.framework/Versions/A/vulkan.1',
              app/'Frameworks/MoltenVK.framework/Versions/A/MoltenVK', icd,
              Path(__file__), ROOT/'scripts/build-linux-fixture.py']
    if pixels:
        inputs += [ROOT/'scripts'/('linux-vulkan-triangle.' + ext) for ext in ['c', 'vert', 'frag']]
    hashes = {str(p.relative_to(ROOT)): digest(p) for p in inputs}
    out.mkdir(mode=0o700, parents=True, exist_ok=False)
    spec = importlib.util.spec_from_file_location('fixture', ROOT/'scripts/build-linux-fixture.py')
    fixture = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixture)
    entries = []
    with tarfile.open(rootfs) as archive:
        for member in archive.getmembers():
            name = member.name.removeprefix('./')
            if not name or name == '.':
                continue
            if name.startswith('/') or '..' in Path(name).parts:
                raise ValueError('unsafe rootfs path')
            if member.isdir():
                mode, data = stat.S_IFDIR | member.mode, b''
            elif member.issym():
                mode, data = stat.S_IFLNK | member.mode, member.linkname.encode()
            elif member.isfile():
                mode, data = stat.S_IFREG | member.mode, archive.extractfile(member).read()
            else:
                raise ValueError('unsupported rootfs entry')
            entries.append(fixture.entry(name, data, mode))
    init = INIT
    if pixels:
        init = init.replace('vulkan-loader=1.4.347-r0 ||', 'vulkan-loader=1.4.347-r0 gcc musl-dev vulkan-headers glslang ||')
        init = init.replace('sync\npoweroff -f', PIXEL_RUN)
        for ext in ['c', 'vert', 'frag']:
            entries.append(fixture.entry('veynode/triangle.'+ext,
                (ROOT/'scripts'/('linux-vulkan-triangle.'+ext)).read_bytes(), 0o100644))
    entries += [fixture.entry('veynode/vulkan-init', init.encode(), 0o100755),
                fixture.entry('veynode/dhcp', DHCP.encode(), 0o100755), fixture.entry('TRAILER!!!', b'')]
    initrd = out/'initramfs.gz'
    initrd.write_bytes((MEDIA/'veynode-initramfs.gz').read_bytes() + gzip.compress(b''.join(entries), mtime=0))
    # Short private socket directory avoids macOS sockaddr_un path truncation.
    cmd = [str(BASE/'probe-loader'), str(qemu), '-L', str(app/'Resources/qemu'),
           '-machine', 'virt,accel=hvf', '-cpu', 'host', '-m', '1024M', '-smp', '2', '-nodefaults',
           '-nic', 'user,model=virtio-net-pci', '-display', 'none', '-serial', 'stdio', '-monitor', 'none',
           '-d', 'guest_errors', '-spice', 'unix=on,addr=@SOCKET@,disable-ticketing=on,gl=es',
           '-device', 'virtio-gpu-gl-pci,blob=on,hostmem=256M,venus=on', '-kernel', str(MEDIA/'Image'),
           '-initrd', str(initrd), '-append', 'console=ttyAMA0 rdinit=/veynode/vulkan-init']
    if ipa_granule:
        cmd[cmd.index('virt,accel=hvf')] = 'virt'
        cmd += ['-accel', f'hvf,ipa-granule-size={ipa_granule}']
    env = {'ANGLE_DEFAULT_PLATFORM': 'metal', 'RENDER_SERVER_EXEC_PATH': str(server),
           'VK_DRIVER_FILES': str(icd), 'VIRGL_LOG_LEVEL': 'debug'}
    manifest = {'schema': 1, 'command_template': cmd, 'environment': env, 'inputs': hashes,
                'initrd_sha256': digest(initrd), 'disk_attached': False, 'run_performed': False, 'ipa_granule_bytes': ipa_granule or 'host-default', 'pixel_oracle': pixels,
                'change_from_old_probe': 'enable guest_errors/debug; Vulkan only; pin original APK versions/virtio ICD'}
    (out/'prepare.json').write_text(json.dumps(manifest, indent=2)+'\n')
    return cmd, env


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--output-dir', type=Path, required=True)
    p.add_argument('--run', action='store_true')
    p.add_argument('--pixels', action='store_true')
    p.add_argument('--render-server-manifest', type=Path)
    p.add_argument('--ipa-granule', type=int, choices=[0, 4096], default=0,
                   help='4096 matches official UTM Vulkan config; public API requires macOS 26')
    args = p.parse_args()
    os.umask(0o077)
    out = args.output_dir.absolute()
    cmd, extra = prepare(out, args.render_server_manifest, args.ipa_granule, args.pixels)
    if not args.run:
        print(json.dumps({'prepared': True, 'run_performed': False, 'path': str(out)}))
        return 0
    timed_out = False
    with tempfile.TemporaryDirectory(prefix='vn-vulkan-') as temp, (out/'serial.log').open('x') as log:
        cmd = [x.replace('@SOCKET@', temp+'/spice.sock') for x in cmd]
        env = os.environ.copy()
        # Avoid an inherited unrelated application's sandbox group altering SHM naming.
        env.pop('APP_SANDBOX_GROUP_ID', None)
        env.pop('VIRGL_LOG_FILE', None)
        env.update(extra)
        proc = subprocess.Popen(cmd, env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                start_new_session=True)
        try:
            code = proc.wait(timeout=180)
        except (subprocess.TimeoutExpired, KeyboardInterrupt):
            timed_out = True
            os.killpg(proc.pid, signal.SIGTERM)
            try:
                code = proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
                code = proc.wait(timeout=5)
    result = classify((out/'serial.log').read_text(errors='replace'), code)
    if args.pixels:
        serial = (out/'serial.log').read_text(errors='replace')
        result = classify_pixels(serial, result)
    result.update({'exit_code': code, 'timed_out': timed_out, 'owned_child_reaped': True,
                   'serial_sha256': digest(out/'serial.log')})
    (out/'result.json').write_text(json.dumps(result, indent=2)+'\n')
    print(json.dumps(result, indent=2))
    return 0 if (result['raster_pixel_oracle_passed'] if args.pixels else result['initialization_succeeded']) else 1


if __name__ == '__main__':
    raise SystemExit(main())
