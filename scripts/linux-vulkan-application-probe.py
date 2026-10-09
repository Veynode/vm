#!/usr/bin/env python3
"""Bounded, diskless upstream vkmark workload. Preparation only unless --run."""
import argparse
import gzip
import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
APK_URL = 'https://dl-cdn.alpinelinux.org/alpine/v3.24/community/aarch64/vkmark-2025.01-r2.apk'
APK_SHA = '8b6cea3d5135efaa6c5f445f8f4a5635f96e9d60b35818b5062a7cf88a2c9921'
UPSTREAM = '2bf2ca7f1623db7ad7840a4dd626444d11830815'
CASES = [('vertex-interleaved', 'vertex:duration=4.0:interleave=true'),
         ('vertex-separated', 'vertex:duration=4.0:interleave=false'),
         ('texture-linear', 'texture:duration=4.0:texture-filter=linear:anisotropy=16'),
         ('shading-phong', 'shading:duration=4.0:shading=phong'),
         ('shading-cel', 'shading:duration=4.0:shading=cel')]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


base = load('venus_init', ROOT/'scripts/linux-vulkan-probe.py')
ORIGINAL_INIT = base.INIT


def guest_script():
    script = ORIGINAL_INIT.replace(
        "printf '\\nVULKAN_PACKAGES_BEGIN\\n'", 
        "apk add --force-non-repository --no-cache --no-progress /veynode/vkmark.apk || fail vkmark_package\n"
        "printf '\\nVULKAN_PACKAGES_BEGIN\\n'")
    extra = '''[ "$rc" -eq 0 ] || fail instance
[ -f /usr/lib/vkmark/headless.so ] || fail headless_plugin
# Full physical-device API/features remain available for later diagnosis.
timeout -s KILL 15 vulkaninfo > /tmp/vulkan-features.txt 2>&1 || fail features
printf 'VKMARK_FEATURES_BEGIN\\n'
grep -E 'deviceName|deviceType|driverName|driverID|apiVersion|samplerAnisotropy|VK_EXT_headless_surface|VK_KHR_swapchain' /tmp/vulkan-features.txt
printf 'VKMARK_FEATURES_END\\n'
'''
    for ident, scene in CASES:
        extra += f'''printf 'VKMARK_CASE_BEGIN {ident}\\n'
timeout -s KILL 20 vkmark --winsys headless --size 800x600 --present-mode fifo -b '{scene}'
status=$?
printf '\\nVKMARK_CASE_EXIT {ident} %s\\n' "$status"
'''
    extra += "printf 'VKMARK_APPLICATION_DONE\\n'\nsync\npoweroff -f"
    return script.replace('sync\npoweroff -f', extra)


def classify(text, code):
    text = text.replace('\r', '')
    result = base.classify(text, code)
    results = []
    for ident, scene in CASES:
        sections = re.findall(r'^VKMARK_CASE_BEGIN ' + ident + r'\n(.*?)^VKMARK_CASE_EXIT ' + ident + r' (\d+)\n', text, re.M | re.S)
        item = {'case': ident, 'passed': False}
        if len(sections) == 1:
            body, status = sections[0]
            name, *options = scene.split(':')
            rows = re.findall(r'^\[' + name + r'\] (.*?) FPS: (\d+) FrameTime: ([0-9.]+) ms$', body, re.M)
            identity = (re.findall(r'^\s*Device Name:\s*(.*?)\s*$', body, re.M) == ['Virtio-GPU Venus (Apple M1 Pro)']
                        and re.findall(r'^\s*Vendor ID:\s*(.*?)\s*$', body, re.M) == ['0x106B'])
            valid = len(rows) == 1 and all(o in rows[0][0].split(':') for o in options) and int(rows[0][1]) > 0
            item.update(exit_code=int(status), hardware_identity=identity, completed_scene=valid,
                        passed=status == '0' and identity and valid and not re.search(
                            r'llvmpipe|lavapipe|SwiftShader|Failed with exception|Skipping benchmark', body, re.I))
        results.append(item)
    features = re.findall(r'^VKMARK_FEATURES_BEGIN\n(.*?)^VKMARK_FEATURES_END$', text, re.M | re.S)
    features_ok = len(features) == 1 and bool(re.search(r'samplerAnisotropy\s*=\s*true', features[0]))
    markers_ok = (re.findall(r'^VKMARK_CASE_BEGIN (\S+)$', text, re.M) == [c[0] for c in CASES] and
                  re.findall(r'^VKMARK_CASE_EXIT (\S+) \d+$', text, re.M) == [c[0] for c in CASES])
    result.update(cases=results, api_feature_guard=features_ok, markers_valid=markers_ok,
                  workload_completed=(result['initialization_succeeded'] and features_ok and markers_ok and
                      all(c['passed'] for c in results) and text.splitlines().count('VKMARK_APPLICATION_DONE') == 1),
                  visual_correctness_validated=False, presentation_validated=False, performance_measured=False)
    return result


def prepare(out, server_manifest, cache):
    cache.mkdir(parents=True, exist_ok=True)
    apk = cache/'vkmark-2025.01-r2.apk'
    if not apk.exists():
        with urllib.request.urlopen(APK_URL, timeout=30) as response:
            data = response.read(2 * 1024 * 1024 + 1)
        if len(data) > 2 * 1024 * 1024:
            raise ValueError('APK exceeds download bound')
        with apk.open('xb') as f:
            f.write(data)
    if base.digest(apk) != APK_SHA:
        raise ValueError('official APK hash mismatch')
    # Private imported module; no modification of shared diagnostic source.
    base.INIT = guest_script()
    cmd, env = base.prepare(out, server_manifest, 4096)
    fixture = load('cpio_fixture', ROOT/'scripts/build-linux-fixture.py')
    with (out/'initramfs.gz').open('ab') as f:
        f.write(gzip.compress(fixture.entry('veynode/vkmark.apk', apk.read_bytes(), 0o100644) +
                              fixture.entry('TRAILER!!!', b''), mtime=0))
    (out/'guest-init.sh').write_text(base.INIT)
    manifest = json.loads((out/'prepare.json').read_text())
    manifest.update(workload='vkmark-2025.01-r2', upstream_commit=UPSTREAM,
                    apk_url=APK_URL, apk_sha256=APK_SHA, license='LGPL-2.1-or-later',
                    cases=CASES, winsys='headless', size=[800, 600],
                    application_timeout_seconds=20, host_timeout_seconds=180,
                    initrd_sha256=base.digest(out/'initramfs.gz'),
                    guest_init_sha256=base.digest(out/'guest-init.sh'),
                    workload_source_sha256=base.digest(Path(__file__)),
                    change_from_old_probe='independent server + upstream 4 KiB IPA; pinned headless vkmark workload')
    (out/'prepare.json').write_text(json.dumps(manifest, indent=2)+'\n')
    return cmd, env


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--output-dir', required=True, type=Path)
    p.add_argument('--render-server-manifest', required=True, type=Path)
    p.add_argument('--cache', type=Path, default=ROOT/'.local/linux-vulkan-application-source')
    p.add_argument('--run', action='store_true')
    a = p.parse_args()
    os.umask(0o077)
    out = a.output_dir.absolute()
    cmd, extra = prepare(out, a.render_server_manifest, a.cache)
    if not a.run:
        print(json.dumps({'prepared': True, 'run_performed': False, 'path': str(out)}))
        return 0
    timed_out = False
    with tempfile.TemporaryDirectory(prefix='vn-vkmark-') as temp, (out/'serial.log').open('x') as log:
        cmd = [x.replace('@SOCKET@', temp+'/spice.sock') for x in cmd]
        env = os.environ.copy()
        for key in ['APP_SANDBOX_GROUP_ID', 'VIRGL_LOG_FILE']:
            env.pop(key, None)
        env.update(extra)
        proc = subprocess.Popen(cmd, env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
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
    result.update(exit_code=code, timed_out=timed_out, owned_child_reaped=True, serial_sha256=base.digest(out/'serial.log'))
    if timed_out:
        result['workload_completed'] = False
    (out/'result.json').write_text(json.dumps(result, indent=2)+'\n')
    print(json.dumps(result, indent=2))
    return 0 if result['workload_completed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
