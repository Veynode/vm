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
    share/guest/  the Linux guest agent for cloud-image machines, when built
    share/runtime.json  where everything above is, relative to share/: the
                  daemon reads it from bin/../share, so the unpacked directory
                  needs no registration (`bin/veynodevm daemon start`)
    libexec/qemu/ the QEMU/GPU runtime (with --qemu-runtime): the QEMU loader,
                  UTM's QEMU and the frameworks it loads, the SPICE server
                  with VeynodeVM's reconnect fix, QEMU's firmware and the
                  native console app
    LICENSES/     the runtime's licences and NOTICE (with --qemu-runtime)
    web/          the web UI server and its built app (with --web)
    install.py    registers this directory in <home>/runtime.json
    MANIFEST.json version, commit, every file's SHA-256, signing

Nothing is built, downloaded, notarized or published here; signing is ad hoc
unless --sign names an identity. The QEMU/GPU runtime is included only with
--qemu-runtime, as this Mac uses it today: UTM 5.0.8's binaries
(runtime/fetch-utm.sh), the SPICE reconnect build
(runtime/build-spice-reconnect-server.py) and the console app
(runtime/build-qemu-console.sh). Venus and Neptune are left out: their
registration pins the unsigned bytes (runtime/register-venus.py).
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile

ROOT = Path(__file__).resolve().parents[1]
# The daemon finds vm-console-stream, and the CLI vm-control, next to itself.
BINARIES = ('veynodevm', 'vm-control', 'vm-image', 'vm-mcp-http', 'vm-qemu', 'vm-console', 'vm-console-stream')
HELPERS = {'vz_desktop_stream': 'vz-desktop-stream', 'vision': 'vm-vision', 'macos_desktop': 'vm-macos-desktop',
           'macos_relay': 'veynode-macos-relay'}
BOOT = {'kernel': 'Image', 'initrd': 'initramfs.gz', 'modloop': 'modloop'}
WEB = ('server.mjs', 'auth.mjs', 'daemon.mjs', 'desktop.mjs', 'terminal.mjs')
EXCLUDED = ['QEMU/GPU runtime (UTM, virglrenderer, Venus): redistribution not reviewed',
            'cua-driver: third-party binaries fetched with runtime/fetch-cua-driver.sh, not reviewed for redistribution']
RUNTIME_EXCLUDED = ['Venus and Neptune (virgl_render_server, MoltenVK, DXMT): their registration pins unsigned bytes',
                    'D3DMetal: proprietary, no redistribution grant',
                    'cua-driver: third-party binaries fetched with runtime/fetch-cua-driver.sh, not reviewed for redistribution']

# The QEMU/GPU runtime, as the development home registers it today
# (scripts/install-dev-runtime.py --qemu).
QEMU_LIBRARY = 'qemu-aarch64-softmmu.framework/Versions/A/qemu-aarch64-softmmu'
SPICE_LIBRARY = 'libspice-server-fixed.dylib'
CONSOLE_APP = 'VeynodeQEMUConsole.app'
# Loaded by name rather than linked: libepoxy opens ANGLE from its own
# Frameworks directory for virgl's `gl=es`.
OPENED = ('EGL.framework/Versions/Current/EGL', 'GLESv2.framework/Versions/Current/GLESv2')
# Replaced by libspice-server-fixed.dylib, which takes its install name; the
# loader refuses to start when two SPICE servers are loaded.
REPLACED = ('spice-server.1.framework',)
# The QEMU process: HVF, QEMU's code generator, the Guest's microphone.
LOADER_ENTITLEMENTS = ('com.apple.security.hypervisor', 'com.apple.security.cs.allow-jit',
                       'com.apple.security.device.audio-input')
GPL = 'licenses/GPL-2.0.txt'
LGPL = 'licenses/LGPL-2.1.txt'
APACHE = 'LICENSE'
# Every framework the runtime may carry: component, version or commit,
# source, SPDX licence, and the licence text in this checkout
# (licenses/runtime/README.md says where each came from). UTM 5.0.8's pins (patches/sources in its
# source archive; docs/decisions/0026-gpu-runtime-source-build.md).
COMPONENTS = {
    'qemu-aarch64-softmmu': ('QEMU (UTM fork)', '10.0.12-utm', 'https://github.com/utmapp/qemu/releases/download/v10.0.12-utm/qemu-10.0.12-utm.tar.xz', 'GPL-2.0-only', GPL),
    'virglrenderer.1': ('virglrenderer (UTM fork)', '451505ad0aa449290f2c0672da6877d57ee770e5', 'https://github.com/utmapp/virglrenderer', 'MIT', 'licenses/runtime/virglrenderer.txt'),
    'epoxy.0': ('libepoxy (UTM fork)', 'bf98587477fe68d07b93319ece7b40a7d0e2eabe', 'https://github.com/utmapp/libepoxy', 'MIT', 'licenses/runtime/libepoxy.txt'),
    'EGL': ('ANGLE (UTM WebKit fork, Source/ThirdParty/ANGLE)', 'ed78ab6e1a37f4f11583a0bd038f22ec91f3ff10', 'https://github.com/utmapp/WebKit', 'BSD-3-Clause', 'licenses/runtime/angle.txt'),
    'GLESv2': ('ANGLE (UTM WebKit fork, Source/ThirdParty/ANGLE)', 'ed78ab6e1a37f4f11583a0bd038f22ec91f3ff10', 'https://github.com/utmapp/WebKit', 'BSD-3-Clause', 'licenses/runtime/angle.txt'),
    'vulkan.1': ('Vulkan-Loader', '6df86907be582927507637f3f6003ea58bc34517', 'https://github.com/osy/Vulkan-Loader', 'Apache-2.0', APACHE),
    'slirp.0': ('libslirp (UTM mirror)', '4.9.1', 'https://github.com/utmapp/libslirp/releases/download/v4.9.1-release-mirror/libslirp-v4.9.1.tar.gz', 'BSD-3-Clause AND MIT', 'licenses/runtime/libslirp.txt'),
    'zstd.1': ('zstd', '1.5.2', 'https://github.com/facebook/zstd/releases/download/v1.5.2/zstd-1.5.2.tar.gz', 'BSD-3-Clause OR GPL-2.0-only', 'licenses/runtime/zstd.txt'),
    'usb-1.0.0': ('libusb', '1.0.25', 'https://github.com/libusb/libusb/releases/download/v1.0.25/libusb-1.0.25.tar.bz2', 'LGPL-2.1-or-later', LGPL),
    'usbredirparser.1': ('usbredir', '0.14.0', 'https://www.spice-space.org/download/usbredir/usbredir-0.14.0.tar.xz', 'LGPL-2.1-or-later', LGPL),
    'usbredirhost.1': ('usbredir', '0.14.0', 'https://www.spice-space.org/download/usbredir/usbredir-0.14.0.tar.xz', 'LGPL-2.1-or-later', LGPL),
    'glib-2.0.0': ('GLib', '2.83.0', 'https://download.gnome.org/sources/glib/2.83/glib-2.83.0.tar.xz', 'LGPL-2.1-or-later', LGPL),
    'gio-2.0.0': ('GLib', '2.83.0', 'https://download.gnome.org/sources/glib/2.83/glib-2.83.0.tar.xz', 'LGPL-2.1-or-later', LGPL),
    'gobject-2.0.0': ('GLib', '2.83.0', 'https://download.gnome.org/sources/glib/2.83/glib-2.83.0.tar.xz', 'LGPL-2.1-or-later', LGPL),
    'gmodule-2.0.0': ('GLib', '2.83.0', 'https://download.gnome.org/sources/glib/2.83/glib-2.83.0.tar.xz', 'LGPL-2.1-or-later', LGPL),
    'pixman-1.0': ('pixman', '0.38.0', 'https://www.cairographics.org/releases/pixman-0.38.0.tar.gz', 'MIT', 'licenses/runtime/pixman.txt'),
    'jpeg.62': ('libjpeg-turbo', '1.5.3', 'https://ftp.osuosl.org/pub/blfs/conglomeration/libjpeg-turbo/libjpeg-turbo-1.5.3.tar.gz', 'IJG AND BSD-3-Clause AND Zlib', 'licenses/runtime/libjpeg-turbo.txt'),
    'ssl.1.1': ('OpenSSL', '1.1.1b', 'https://github.com/openssl/openssl/releases/download/OpenSSL_1_1_1b/openssl-1.1.1b.tar.gz', 'OpenSSL', 'licenses/runtime/openssl.txt'),
    'crypto.1.1': ('OpenSSL', '1.1.1b', 'https://github.com/openssl/openssl/releases/download/OpenSSL_1_1_1b/openssl-1.1.1b.tar.gz', 'OpenSSL', 'licenses/runtime/openssl.txt'),
    'ffi.8': ('libffi', '3.5.0', 'https://github.com/libffi/libffi/releases/download/v3.5.0/libffi-3.5.0.tar.gz', 'MIT', 'licenses/runtime/libffi.txt'),
    'intl.8': ('gettext (libintl)', '0.22.5', 'http://ftp.gnu.org/gnu/gettext/gettext-0.22.5.tar.gz', 'LGPL-2.1-or-later', LGPL),
    'iconv.2': ('libiconv', '1.16', 'http://ftp.gnu.org/gnu/libiconv/libiconv-1.16.tar.gz', 'LGPL-2.1-or-later', LGPL),
    'spice-client-glib-2.0.8': ('spice-gtk (spice-client-glib)', '0.42', 'https://www.spice-space.org/download/gtk/spice-gtk-0.42.tar.xz', 'LGPL-2.1-or-later', LGPL),
    'json-glib-1.0.0': ('json-glib', '1.10.0', 'https://download.gnome.org/sources/json-glib/1.10/json-glib-1.10.0.tar.xz', 'LGPL-2.1-or-later', LGPL),
    'soup-3.0.0': ('libsoup', '3.6.0', 'https://download.gnome.org/sources/libsoup/3.6/libsoup-3.6.0.tar.xz', 'LGPL-2.0-or-later', LGPL),
    'phodav-3.0.0': ('phodav', '3.0', 'https://download.gnome.org/sources/phodav/3.0/phodav-3.0.tar.xz', 'LGPL-2.1-or-later', LGPL),
    'opus.0': ('Opus', '1.3', 'https://archive.mozilla.org/pub/opus/opus-1.3.tar.gz', 'BSD-3-Clause', 'licenses/runtime/opus.txt'),
    'gstreamer-1.0.0': ('GStreamer', '1.19.1', 'https://gstreamer.freedesktop.org/src/gstreamer/gstreamer-1.19.1.tar.xz', 'LGPL-2.0-or-later', LGPL),
    'gstbase-1.0.0': ('GStreamer', '1.19.1', 'https://gstreamer.freedesktop.org/src/gstreamer/gstreamer-1.19.1.tar.xz', 'LGPL-2.0-or-later', LGPL),
    'gstapp-1.0.0': ('gst-plugins-base', '1.19.1', 'https://gstreamer.freedesktop.org/src/gst-plugins-base/gst-plugins-base-1.19.1.tar.xz', 'LGPL-2.0-or-later', LGPL),
    'gstaudio-1.0.0': ('gst-plugins-base', '1.19.1', 'https://gstreamer.freedesktop.org/src/gst-plugins-base/gst-plugins-base-1.19.1.tar.xz', 'LGPL-2.0-or-later', LGPL),
    'gsttag-1.0.0': ('gst-plugins-base', '1.19.1', 'https://gstreamer.freedesktop.org/src/gst-plugins-base/gst-plugins-base-1.19.1.tar.xz', 'LGPL-2.0-or-later', LGPL),
    'gstvideo-1.0.0': ('gst-plugins-base', '1.19.1', 'https://gstreamer.freedesktop.org/src/gst-plugins-base/gst-plugins-base-1.19.1.tar.xz', 'LGPL-2.0-or-later', LGPL),
}
# What the runtime carries besides UTM's frameworks.
OWN_RUNTIME = [
    ('SPICE server, with UTM\'s spice-0.14.3.patch and VeynodeVM\'s reconnect and retired-publication fixes '
     f'({SPICE_LIBRARY}; runtime/build-spice-reconnect-server.py, runtime/apply-spice-*.py, '
     'native/tools/spice-scanout-generation.c)', '0.14.3', 'https://www.spice-space.org/download/releases/spice-server/spice-0.14.3.tar.bz2', 'LGPL-2.1-or-later', LGPL),
    ('QEMU firmware and data (share/: EDK2, option ROMs, keymaps), as bundled with QEMU', '10.0.12-utm',
     'https://github.com/utmapp/qemu/releases/download/v10.0.12-utm/qemu-10.0.12-utm.tar.xz',
     'GPL-2.0-only AND BSD-2-Clause-Patent (EDK2, see share/edk2-licenses.txt)', GPL),
    ('QEMU loader (probe-loader; native/tools/spice-reconnect-loader.c)', 'this release',
     'https://github.com/Veynode/vm', 'GPL-2.0-or-later', GPL),
    ('UTM (the frameworks above are its 5.0.8 release build)', '5.0.8',
     'https://github.com/utmapp/UTM/releases/tag/v5.0.8', 'Apache-2.0', APACHE),
]

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


def inputs(root, profile, web, boot=None):
    """What goes where: (source, destination relative to the release), the
    helpers present, and what is missing."""
    boot = boot or root / '.local/boot'
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
        need(boot / file, f'share/boot/{file}')
    # The Linux guest agent the daemon puts on a cloud image's seed.
    agent = root / '.local/bin/veynode-guest'
    if agent.exists():
        files.append((agent, 'share/guest/veynode-agent'))
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


def shipped_runtime(helpers, web, qemu):
    """share/runtime.json: the layout, relative to share/, which the daemon
    reads from bin/../share (vm_control::home::Runtime::bundled)."""
    runtime = {
        'backend': '../VeynodeVM.app/Contents/MacOS/vm-apple',
        'boot': {key: f'boot/{file}' for key, file in BOOT.items()},
        **{key: f'../{value}' for key, value in helpers.items()},
    }
    if web:
        runtime['web'] = '../web/server.mjs'
    if qemu:
        runtime['qemu'] = {
            'worker': '../bin/vm-qemu',
            'launcher': '../libexec/qemu/probe-loader',
            'library': f'../libexec/qemu/Frameworks/{QEMU_LIBRARY}',
            'resources': '../libexec/qemu/share',
            'spice_library': f'../libexec/qemu/Frameworks/{SPICE_LIBRARY}',
            'console_broker': '../bin/vm-console',
            'console_helper': f'../libexec/qemu/{CONSOLE_APP}/Contents/MacOS/spice-console',
        }
    return runtime


def linked(binary):
    """The @rpath frameworks a Mach-O links: (framework, path inside it)."""
    result = subprocess.run(['otool', '-L', '-arch', 'arm64', str(binary)], capture_output=True, text=True)
    return re.findall(r'@rpath/([^/\s]+\.framework)/(\S+)', result.stdout)


def rpaths(binary):
    result = subprocess.run(['otool', '-l', str(binary)], capture_output=True, text=True)
    return re.findall(r'cmd LC_RPATH\n\s+cmdsize \d+\n\s+path (.+?) \(offset', result.stdout)


def frameworks(utm, seeds):
    """Every UTM framework the seeds load, transitively, plus OPENED."""
    found, todo = set(), [*seeds, *(utm / 'Contents/Frameworks' / name for name in OPENED)]
    found.update(name.split('/')[0] for name in OPENED)
    seen = set()
    while todo:
        binary = Path(os.path.realpath(todo.pop()))
        if binary in seen or not binary.is_file():
            continue
        seen.add(binary)
        for framework, inner in linked(binary):
            if framework not in REPLACED:
                found.add(framework)
                todo.append(utm / 'Contents/Frameworks' / framework / inner)
    return sorted(found)


def runtime_inputs(utm, spice, console):
    """The QEMU/GPU runtime's pieces and what is missing of them."""
    pieces = {
        'loader': spice / 'probe-loader',
        'spice': spice / SPICE_LIBRARY,
        'console': console,
        'qemu': utm / 'Contents/Frameworks' / QEMU_LIBRARY,
        'share': utm / 'Contents/Resources/qemu',
    }
    missing = [str(path) for path in pieces.values() if not path.exists()]
    return pieces, missing


def notice(names):
    """LICENSES/NOTICE: every shipped framework's component, version, source
    and licence; texts alongside."""
    lines = ['VeynodeVM QEMU/GPU runtime', '==========================', '',
             'libexec/qemu/ carries the third-party software below, unmodified unless',
             'stated, as UTM 5.0.8 built it. Each licence text is in this directory;',
             'the corresponding source is at the URL given, and the patches and build',
             'recipes of the parts VeynodeVM changed are in its source repository',
             '(https://github.com/Veynode/vm: runtime/, native/tools/,',
             'docs/decisions/0026-gpu-runtime-source-build.md). UTM\'s own patches to',
             'these sources are in its source archive, patches/.', '']
    texts = {}
    by_component = {}
    for name in names:
        key = name.removesuffix('.framework')
        component, version, source, spdx, text = COMPONENTS[key]
        by_component.setdefault((component, version, source, spdx, text), []).append(name)
    entries = [(*component, files) for component, files in by_component.items()]
    entries += [(*own, []) for own in OWN_RUNTIME]
    for component, version, source, spdx, text, files in entries:
        texts[text] = Path(text).name
        lines += [f'{component}', f'  version: {version}', f'  source:  {source}', f'  licence: {spdx} (LICENSES/{Path(text).name})']
        if files:
            lines.append(f'  files:   {", ".join(f"Frameworks/{name}" for name in files)}')
        lines.append('')
    return '\n'.join(lines), texts


def sign(identity, path, entitlements=None, dry=False):
    run(signing_argv(identity, path, entitlements), dry)


def entitlements_plist(keys):
    body = ''.join(f'<key>{key}</key><true/>' for key in keys)
    return ('<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" '
            f'"http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n<plist version="1.0"><dict>{body}</dict></plist>\n')


def package_runtime(root, out, pieces, utm, identity, dry):
    """Copies the QEMU/GPU runtime into libexec/qemu, points its run paths
    inside the release, signs it and writes LICENSES/."""
    qemu = out / 'libexec/qemu'
    names = frameworks(utm, [pieces['qemu'], pieces['spice'], pieces['console'] / 'Contents/MacOS/spice-console'])
    for name in names:
        shutil.copytree(utm / 'Contents/Frameworks' / name, qemu / 'Frameworks' / name, symlinks=True)
    copy(pieces['spice'], qemu / 'Frameworks' / SPICE_LIBRARY)
    copy(pieces['loader'], qemu / 'probe-loader')
    shutil.copytree(pieces['share'], qemu / 'share', symlinks=False)
    shutil.copytree(pieces['console'], qemu / CONSOLE_APP, symlinks=True)
    # Run paths into the release instead of the build's absolute ones.
    for binary, rpath in ((qemu / 'probe-loader', '@executable_path/Frameworks'),
                          (qemu / 'Frameworks' / SPICE_LIBRARY, '@loader_path'),
                          (qemu / CONSOLE_APP / 'Contents/MacOS/spice-console', '@executable_path/../../../Frameworks')):
        for old in rpaths(binary):
            run(['install_name_tool', '-delete_rpath', old, str(binary)], dry)
        run(['install_name_tool', '-add_rpath', rpath, str(binary)], dry)
    # Inside out: the frameworks and the SPICE server before what loads them,
    # all with one Team ID so the hardened loader's library validation passes.
    for name in names:
        sign(identity, qemu / 'Frameworks' / name, dry=dry)
    sign(identity, qemu / 'Frameworks' / SPICE_LIBRARY, dry=dry)
    plist = out / 'libexec/qemu/probe-loader.entitlements'
    plist.write_text(entitlements_plist(LOADER_ENTITLEMENTS))
    sign(identity, qemu / 'probe-loader', plist, dry)
    plist.unlink()
    sign(identity, qemu / CONSOLE_APP, dry=dry)
    text, texts = notice(names)
    licenses = out / 'LICENSES'
    licenses.mkdir()
    (licenses / 'NOTICE').write_text(text)
    for source, name in texts.items():
        shutil.copy2(root / source, licenses / name)
    return names


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


def manifest(out, version, commit, profile, identity, helpers, web, runtime=None):
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
        'includes': {'web': web, 'helpers': sorted(helpers), **({'qemu_runtime': runtime} if runtime else {})},
        'excludes': RUNTIME_EXCLUDED if runtime else EXCLUDED,
        'files': entries,
    }


def run(argv, dry):
    if dry:
        print(' '.join(argv))
    else:
        subprocess.run(argv, check=True)


def package(root, out, profile, identity, web, version, commit, dry=False, boot=None, runtime=None):
    """`runtime`: (UTM.app, SPICE reconnect runtime dir, console app) to ship
    the QEMU/GPU runtime."""
    files, helpers, missing = inputs(root, profile, web, boot)
    if runtime:
        pieces, absent = runtime_inputs(*runtime)
        missing += absent
    if missing:
        raise SystemExit('missing built pieces (see docs/guides/building.md):\n  ' + '\n  '.join(missing))
    out.mkdir(parents=True)
    for source, dest in files:
        copy(source, out / dest)
    (out / 'VeynodeVM.app/Contents/Info.plist').write_text(info_plist(version))
    if web:
        # dist/server/server.js is an ES module.
        (out / 'web/package.json').write_text(json.dumps({'type': 'module', 'private': True}) + '\n')
    (out / 'install.py').write_text(INSTALL % {'layout': json.dumps(layout(helpers, web))})
    os.chmod(out / 'install.py', 0o755)
    (out / 'share').mkdir(exist_ok=True)
    (out / 'share/runtime.json').write_text(json.dumps(shipped_runtime(helpers, web, bool(runtime)), indent=2) + '\n')
    for name in BINARIES:
        run(signing_argv(identity, out / 'bin' / name), dry)
    for dest in helpers.values():
        run(signing_argv(identity, out / dest), dry)
    run(signing_argv(identity, out / 'VeynodeVM.app', root / 'native/entitlements.plist'), dry)
    run(['codesign', '--verify', '--strict', '--deep', str(out / 'VeynodeVM.app')], dry)
    shipped = package_runtime(root, out, pieces, runtime[0], identity, dry) if runtime else None
    if runtime:
        for path in (out / 'libexec/qemu/probe-loader', out / 'libexec/qemu' / CONSOLE_APP):
            run(['codesign', '--verify', '--strict', str(path)], dry)
    record = manifest(out, version, commit, profile, identity, helpers, web, shipped)
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


def archive(out):
    """OUT.tar.gz with OUT's contents under VeynodeVM/, and its SHA-256 in
    OUT.tar.gz.sha256."""
    path = out.with_name(out.name + '.tar.gz')
    with tarfile.open(path, 'w:gz') as tar:
        tar.add(out, arcname='VeynodeVM')
    checksum = digest(path)
    path.with_name(path.name + '.sha256').write_text(f'{checksum}  {path.name}\n')
    return path, checksum


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--output', required=True, type=Path, help='a fresh absolute directory, such as /tmp/VeynodeVM')
    parser.add_argument('--profile', default='release', choices=('release', 'debug'), help='which target/ build to take (default release)')
    parser.add_argument('--sign', default='-', metavar='IDENTITY', help='codesign identity, such as "Developer ID Application: …" (default ad hoc)')
    parser.add_argument('--web', action='store_true', help='include the web UI (cd web && npm ci && npm run build first)')
    parser.add_argument('--boot', type=Path, default=ROOT / '.local/boot', help='the micro-VM boot files (default .local/boot)')
    parser.add_argument('--qemu-runtime', action='store_true', help='ship the QEMU/GPU runtime (libexec/qemu, LICENSES/)')
    parser.add_argument('--utm', type=Path, default=ROOT / '.local/runtime/utm/UTM.app', help='UTM.app whose frameworks ship (runtime/fetch-utm.sh)')
    parser.add_argument('--spice-runtime', type=Path, default=ROOT / '.local/spice-reconnect-build/runtime',
                        help='probe-loader and libspice-server-fixed.dylib (runtime/build-spice-reconnect-server.py)')
    parser.add_argument('--console-app', type=Path, default=ROOT / '.local/bin' / CONSOLE_APP, help='the native console app (runtime/build-qemu-console.sh)')
    parser.add_argument('--archive', action='store_true', help='also write OUTPUT.tar.gz and OUTPUT.tar.gz.sha256')
    parser.add_argument('--dry-run', action='store_true', help='list the inputs and what is missing; copy and sign nothing')
    args = parser.parse_args()
    runtime = (args.utm, args.spice_runtime, args.console_app) if args.qemu_runtime else None
    if not args.output.is_absolute() or args.output.exists():
        raise SystemExit('--output must be an absolute path that does not exist yet')
    if args.dry_run:
        files, helpers, missing = inputs(ROOT, args.profile, args.web, args.boot)
        if runtime:
            pieces, absent = runtime_inputs(*runtime)
            files += [(path, f'libexec/qemu ({key})') for key, path in pieces.items()]
            missing += absent
        for source, dest in files:
            print(f'{dest}  <- {source}')
        for path in missing:
            print(f'MISSING {path}')
        return 1 if missing else 0
    record = package(ROOT, args.output, args.profile, args.sign, args.web, crate_version(ROOT), git_commit(ROOT),
                     boot=args.boot, runtime=runtime)
    print(f'{args.output}: VeynodeVM {record["version"]} ({record["commit"]}), {len(record["files"])} files, signing {record["signing"]}')
    if args.archive:
        path, checksum = archive(args.output)
        print(f'{path}: {path.stat().st_size} bytes, sha256 {checksum}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
