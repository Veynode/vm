#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""Build isolated matching host virgl + server, preserving owned process group.
No VM/GPU execution. Original vendor artifacts are read-only dependencies.
"""
import argparse
import difflib
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
ROOT = Path(__file__).resolve().parents[1]
INPUTS = {'virglrenderer.tar.gz': '92a96c668a2ddc8a7e6846586554c4d45cba11cbf75557a39169ca58d71c81d2',
          'libepoxy.tar.gz': '0f99f3f4313b8e74efe18cea61874b07c8a9f9e504d1aa496ca4d7825e70c39d',
          'eglplatform.h': '25b5391655effcf363a38fa00c3154f356a3eab3d35dc067847875513cf1e441',
          'khrplatform.h': '7b1e01aaa7ad8f6fc34b5c7bdf79ebf5189bb09e2c4d2e79fc5d350623d11e83'}

PROXY_SHA = '305a3347cdef1a321830472d30be6caf7ce3ab5dbbf7e13ac0500daec3b4bf2e'
FRAMEWORKS = {
 'epoxy.0': 'e0e23f0943b43dd975eaf02d1884338e53c418dd420ca709e56cd6b57ee8bcde',
 'vulkan.1': 'fa5eb7539ca3c399e05a5ed93214bcd1cc00bf60eab5e8890564bd305438fe41',
 'virglrenderer.1': '091551234b618c11eac168b91b2a64166330cb73555e83ef649c891e0870dc82',
 'MoltenVK': 'ba5aaa1d356261cf3cb2f9d89cc8f1c9f1bd56adac5e85833d8083d4bbb0b26f',
}

def patch_proxy(data):
    if hashlib.sha256(data).hexdigest() != PROXY_SHA:
        raise ValueError('proxy source pin mismatch')
    text = data.decode()
    old = 'POSIX_SPAWN_CLOEXEC_DEFAULT | POSIX_SPAWN_SETPGROUP);\n   posix_spawnattr_setpgroup(&attr, 0);'
    if text.count(old) != 1: raise ValueError('spawn patch anchor mismatch')
    text = text.replace(old, 'POSIX_SPAWN_CLOEXEC_DEFAULT);')
    text = text.replace("Detaching the\n * process group (POSIX_SPAWN_SETPGROUP, pgroup 0) keeps the server from\n * receiving terminal signals, matching the fork() path's setpgid(0, 0).",
        'Veynode: inherit the\n * already-owned worker process group so forced group retirement includes\n * the server and its context workers. Non-Apple fork behavior is unchanged.')
    return text.encode()


def sha(p): return hashlib.sha256(p.read_bytes()).hexdigest()

def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--source-cache', type=Path, default=ROOT/'.local/linux-vulkan-source-review')
    p.add_argument('--output-dir', type=Path, required=True)
    a = p.parse_args(); out = a.output_dir.absolute()
    frameworks = ROOT/'.local/runtime/utm/UTM.app/Contents/Frameworks'
    for name, expected in FRAMEWORKS.items():
        if sha(frameworks/(name+'.framework')/'Versions/A'/name) != expected:
            raise ValueError('vendor framework pin mismatch')
    for name, expected in INPUTS.items():
        if sha(a.source_cache/name) != expected: raise ValueError('source archive hash mismatch')
    out.mkdir(parents=True, mode=0o700, exist_ok=False)
    for name in ['virglrenderer.tar.gz', 'libepoxy.tar.gz']:
        with tarfile.open(a.source_cache/name) as tar: tar.extractall(out, filter='data')
    source = next(out.glob('virglrenderer-*')); epoxy = next(out.glob('utmapp-libepoxy-*'))
    proxy = source/'src/proxy/proxy_server.c'
    before = proxy.read_bytes(); after = patch_proxy(before); proxy.write_bytes(after)
    (out/'owned-group.patch').write_text(''.join(difflib.unified_diff(before.decode().splitlines(True), after.decode().splitlines(True), fromfile='a/src/proxy/proxy_server.c', tofile='b/src/proxy/proxy_server.c')))
    inc = out/'include/epoxy'; inc.mkdir(parents=True)
    for sub, name in [('EGL','eglplatform.h'),('KHR','khrplatform.h')]:
        (out/'include'/sub).mkdir()
        shutil.copyfile(a.source_cache/name, out/'include'/sub/name)
    for name in ['common.h','gl.h','egl.h']: shutil.copyfile(epoxy/'include/epoxy'/name, inc/name)
    log = (out/'build.log').open('x'); commands = []
    def run(cmd, env=None):
        commands.append([str(x) for x in cmd]); log.write('$ '+str(commands[-1])+'\n'); log.flush()
        subprocess.run(cmd, stdout=log, stderr=subprocess.STDOUT, check=True, env=env)
    for name in ['gl','gl_angle_ext','egl','egl_angle_ext']:
        run(['python3',epoxy/'src/gen_dispatch.py','--header','--no-source','--outputdir='+str(inc),epoxy/'registry'/f'{name}.xml'])
    frameworks = ROOT/'.local/runtime/utm/UTM.app/Contents/Frameworks'
    pc = out/'pkgconfig'; pc.mkdir()
    for name, version, lib in [('epoxy','1.5.10','epoxy.0'),('vulkan','1.4.0','vulkan.1')]:
        (pc/f'{name}.pc').write_text(f'Name: {name}\nDescription: pinned UTM framework build dependency\nVersion: {version}\nLibs: {frameworks}/{lib}.framework/Versions/A/{lib} -Wl,-rpath,{frameworks}\nCflags: -I{out}/include\nepoxy_has_egl=1\n')
    env = os.environ.copy(); env['PKG_CONFIG_LIBDIR'] = str(pc); env['PKG_CONFIG_PATH'] = str(pc)
    run(['meson','setup',out/'build',source,'-Dvenus=true','-Dneptune=true','-Dvulkan-dload=false',
         '-Drender-server-mode=process','-Dtests=false','-Dvtest=false','-Dplatforms=egl',
         '-Dcheck-gl-errors=false'], env)
    run(['ninja','-C',out/'build','-j2','src/libvirglrenderer.1.dylib','server/virgl_render_server'], env)
    binary=out/'build/server/virgl_render_server'
    library=out/'build/src/libvirglrenderer.1.dylib'
    run(['install_name_tool','-id','@rpath/virglrenderer.1.framework/Versions/A/virglrenderer.1',library])
    run(['codesign','--force','--sign','-',library])
    run(['codesign','--verify','--strict',library])
    loader_source=ROOT/'runtime/linux-vulkan-host-loader.c'
    shutil.copyfile(loader_source,out/'loader.c')
    loader=out/'probe-loader'
    run(['clang','-Wall','-Wextra','-Werror','-I',ROOT/'native/tools',out/'loader.c','-Wl,-rpath,'+str(frameworks),'-o',loader])
    entitlements=out/'hypervisor.plist'
    entitlements.write_text('<?xml version="1.0"?><plist version="1.0"><dict><key>com.apple.security.hypervisor</key><true/></dict></plist>')
    run(['codesign','--force','--sign','-','--entitlements',entitlements,loader])
    for path in [loader,library,binary]:run(['otool','-L',path])
    # Our own developer binary, no inherited app-sandbox identity or entitlements.
    run(['codesign','--force','--sign','-',binary])
    run(['codesign','--verify','--strict',binary])
    for name, expected in FRAMEWORKS.items():
        if sha(frameworks/(name+'.framework')/'Versions/A'/name) != expected:
            raise ValueError('vendor input changed during build')
    log.close()
    manifest={'schema':1,'source_archives':INPUTS,'commands':commands,'binary':str(binary),
              'binary_sha256':sha(binary),'build_log_sha256':sha(out/'build.log'),
              'script_sha256':sha(Path(__file__)), 'official_app_modified':False,
              'signing':'local ad-hoc developer signature; no sandbox-inherit entitlement',
              'scope':'independent host framework+server diagnostic; runtime lifecycle not yet verified; not a product distribution',
              'host_library':str(library),'host_library_sha256':sha(library),
              'loader':str(loader),'loader_sha256':sha(loader),'loader_source_sha256':sha(loader_source),
              'proxy_original_sha256':PROXY_SHA,'proxy_patched_sha256':sha(proxy),'patch_sha256':sha(out/'owned-group.patch'),
              'original_frameworks':FRAMEWORKS,
              'vm_run':False,'gpu_run':False,'renderer_lifecycle_verified':False,
              'frameworks':{name:sha(frameworks/name/'Versions/A'/name.removesuffix('.framework')) for name in ['epoxy.0.framework','vulkan.1.framework']}}
    (out/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
    print(json.dumps({'binary':str(binary),'sha256':sha(binary)}))
if __name__=='__main__':main()
