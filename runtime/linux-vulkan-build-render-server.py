#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""Build matching standalone Venus server; never modify/re-sign official UTM app."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
ROOT = Path(__file__).resolve().parents[1]
INPUTS = {'virglrenderer.tar.gz': '47f2c2fd490486a133467c24edf1682bafb832111ca7c98a0f4448ad60f5293e',
          'libepoxy.tar.gz': '0f99f3f4313b8e74efe18cea61874b07c8a9f9e504d1aa496ca4d7825e70c39d',
          'eglplatform.h': '25b5391655effcf363a38fa00c3154f356a3eab3d35dc067847875513cf1e441',
          'khrplatform.h': '7b1e01aaa7ad8f6fc34b5c7bdf79ebf5189bb09e2c4d2e79fc5d350623d11e83'}

def sha(p): return hashlib.sha256(p.read_bytes()).hexdigest()

def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--source-cache', type=Path, default=ROOT/'.local/linux-vulkan-source-review')
    p.add_argument('--output-dir', type=Path, required=True)
    a = p.parse_args(); out = a.output_dir.absolute()
    for name, expected in INPUTS.items():
        if sha(a.source_cache/name) != expected: raise ValueError('source archive hash mismatch')
    out.mkdir(parents=True, mode=0o700, exist_ok=False)
    for name in ['virglrenderer.tar.gz', 'libepoxy.tar.gz']:
        with tarfile.open(a.source_cache/name) as tar: tar.extractall(out, filter='data')
    source = next(out.glob('virglrenderer-*')); epoxy = next(out.glob('utmapp-libepoxy-*'))
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
    run(['ninja','-C',out/'build','-j2','server/virgl_render_server'], env)
    binary=out/'build/server/virgl_render_server'
    # Our own developer binary, no inherited app-sandbox identity or entitlements.
    run(['codesign','--force','--sign','-',binary])
    run(['codesign','--verify','--strict',binary])
    log.close()
    manifest={'schema':1,'source_archives':INPUTS,'commands':commands,'binary':str(binary),
              'binary_sha256':sha(binary),'build_log_sha256':sha(out/'build.log'),
              'script_sha256':sha(Path(__file__)), 'official_app_modified':False,
              'signing':'local ad-hoc developer signature; no sandbox-inherit entitlement',
              'scope':'standalone diagnostic host process, not a sandboxed product distribution',
              'frameworks':{name:sha(frameworks/name/'Versions/A'/name.removesuffix('.framework')) for name in ['epoxy.0.framework','vulkan.1.framework']}}
    (out/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
    print(json.dumps({'binary':str(binary),'sha256':sha(binary)}))
if __name__=='__main__':main()
