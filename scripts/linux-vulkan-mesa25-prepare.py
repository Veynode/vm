#!/usr/bin/env python3
"""Fresh RAM same-source Mesa25 Venus build + virgl/XCB WSI; preparation only."""
import argparse,base64,gzip,hashlib,json,os,subprocess,tarfile
from pathlib import Path
import importlib.util
ROOT=Path(__file__).resolve().parents[1]
def load(n,p):
 s=importlib.util.spec_from_file_location(n,p);m=importlib.util.module_from_spec(s);s.loader.exec_module(m);return m
W=load('mesa25_wsi',ROOT/'scripts/linux-vulkan-wsi-prepare.py');C=W.C
Q=load('mesa25_rootfs',ROOT/'scripts/test-control-qemu.py')
SOURCE=ROOT/'.local/linux-vulkan-mesa25-source/mesa-25.2.7.tar.xz'
SHA512='87dd815e0d11d6ec0eb969ee93d3f376103bb899d90599e0b7902394e41c58139384df79f89633e132ca969348d3320f55308a74651d409b454d51f1bcda27bc'
GLAMOR=ROOT/'.local/windows-gpu/glamor-causal-artifacts/modules-fixed/libglamoregl.so'
BUILT=ROOT/'.local/linux-vulkan-mesa25-built-v1/venus25.tgz'
BUILT_SHA='007f0f4f2e59f4b25c836a1d48c5b32557c1f515766ff6ba353419f3b7dea668'
GLAMOR_SHA='ba64c24cb70c953d26ac032633b6bca98577ea415f7abdc32f7f451d9a115f67'
BUILD=r'''
mount -t tmpfs -o remount,size=1500M rootfs / || fail ram_build_limit
printf 'https://dl-cdn.alpinelinux.org/alpine/v3.23/main\nhttps://dl-cdn.alpinelinux.org/alpine/v3.23/community\n' > /etc/apk/repositories
apk add --no-cache --no-progress build-base bison flex meson ninja python3 py3-mako py3-yaml py3-packaging py3-cparser libdrm-dev libx11-dev libxshmfence-dev libxcb-dev libxrandr-dev expat-dev zstd-dev zlib-dev vulkan-loader-dev vulkan-tools glslang || fail build_packages
printf 'MESA25_TOOLCHAIN_BEGIN\n'; apk info -v; cc --version; meson --version; ninja --version; sha256sum /usr/bin/cc /usr/bin/ld /usr/bin/python3 /usr/bin/meson /usr/bin/ninja /lib/ld-musl-aarch64.so.1; printf 'MESA25_TOOLCHAIN_END\n'
mkdir -p /build
# Official full source archive verified on host and again in this RAM Guest.
echo '@SHA@  /veynode/mesa-25.2.7.tar.xz' | sha512sum -c - || fail source_hash
tar -xJf /veynode/mesa-25.2.7.tar.xz -C /build || fail source_extract
printf 'MESA25_BUILD_BEGIN\n'
cat > /build/compile.sh <<'COMPILE'
#!/bin/sh
set -eu
export CFLAGS='-O2 -g0' CXXFLAGS='-O2 -g0'
meson setup /build/output /build/mesa-25.2.7 --prefix=/opt/venus25 --libdir=lib --buildtype=release --wrap-mode=nofallback -Db_lto=false -Db_ndebug=true -Dgallium-drivers=[] -Dvulkan-drivers=virtio -Dplatforms=x11 -Dllvm=disabled -Dglx=disabled -Degl=disabled -Dgbm=disabled -Dgles1=disabled -Dgles2=disabled -Dopengl=false -Dvideo-codecs=[] -Dvulkan-layers=[] -Dbuild-tests=false -Dvalgrind=disabled
ninja -C /build/output -j2
meson install -C /build/output --no-rebuild
strip /opt/venus25/lib/libvulkan_virtio.so
COMPILE
python3 /veynode/mesa25-build.py
rc=$?
cat /run/mesa25-build.log
printf 'MESA25_BUILD_EXIT %s\n' "$rc"
[ "$rc" -eq 0 ] || fail mesa_build
sha256sum /opt/venus25/lib/libvulkan_virtio.so
ldd /opt/venus25/lib/libvulkan_virtio.so > /run/venus25-ldd.txt
cat /run/venus25-ldd.txt
for lib in $(awk '/=> \/usr\/lib\// {print $3}' /run/venus25-ldd.txt); do sha256sum "$lib" || fail runtime_hash; done
cat /opt/venus25/share/vulkan/icd.d/virtio_icd.aarch64.json
# Export compact artifact for later fresh RAM fixtures; no production install.
tar -czf /run/venus25.tgz -C /opt/venus25 .
[ "$(stat -c %s /run/venus25.tgz)" -le 4194304 ] || fail artifact_bound
printf '\nMESA25_ARTIFACT_BEGIN\n'; base64 /run/venus25.tgz; printf 'MESA25_ARTIFACT_END\n'
export VK_DRIVER_FILES=/opt/venus25/share/vulkan/icd.d/virtio_icd.aarch64.json
timeout -s KILL 15 vulkaninfo --summary || fail instance
'''.replace('@SHA@',SHA512)

PREBUILT=r'''printf 'https://dl-cdn.alpinelinux.org/alpine/v3.23/main\nhttps://dl-cdn.alpinelinux.org/alpine/v3.23/community\n' > /etc/apk/repositories
apk add --no-cache --no-progress build-base libdrm=2.4.131-r0 libx11=1.8.12-r1 libxcb=1.17.0-r1 libxshmfence=1.3.3-r0 libexpat=2.8.5-r0 zstd-libs=1.5.7-r2 zlib=1.3.2-r1 vulkan-loader=1.4.321.0-r0 vulkan-tools=1.4.321.0-r0 || fail runtime_packages
echo '@PIN@  /veynode/venus25.tgz' | sha256sum -c - || fail prebuilt_hash
mkdir -p /opt/venus25
tar -xzf /veynode/venus25.tgz -C /opt/venus25 || fail prebuilt_extract
sha256sum /opt/venus25/lib/libvulkan_virtio.so
export VK_DRIVER_FILES=/opt/venus25/share/vulkan/icd.d/virtio_icd.aarch64.json
timeout -s KILL 15 vulkaninfo --summary || fail instance
'''.replace('@PIN@',BUILT_SHA)
PREBUILT=PREBUILT.replace('timeout -s KILL 15 vulkaninfo',"sha256sum -c - <<'RUNTIME_HASHES'\n"+'52f010ad74dc519d050c57dd85ecb1264673a23365b3b57dfe184874bb0a8981  /usr/lib/libz.so.1\n68b829b7761e9bd28dc7f17367d9a82174f0be110bbfbe9cd14066849a64fdf1  /usr/lib/libzstd.so.1\nd1e313a701dc97b766ea17f6110125347e03cc1c5cd4a91340420382d30e5d8a  /usr/lib/libdrm.so.2\n3e9d56414761bea3eb353b96e755cd59688af59d3a4ca2f79e46134f07466ddc  /usr/lib/libxcb.so.1\n376bd6bd0318f28f4fffd7db3039fca3c5ab01e48f308ee25ad26aa5f0613fc0  /usr/lib/libX11-xcb.so.1\nd024c95aad8c114eec901ed00ec98174f4b2e719880df51e905a4d588c12923a  /usr/lib/libxcb-dri3.so.0\n3a59b66214e61d47aa0ca960bc1bd958cdd882f5cb833c1b48330c15556fcf4f  /usr/lib/libxcb-present.so.0\n8e345d08738aafa798133f4a46c5e6706cb6f127350887f1f27ad018b04fee05  /usr/lib/libxcb-xfixes.so.0\n83057092e64ef9362282466a4b634ab86a17a7adc54f7e44c0047b4c57c987c2  /usr/lib/libxcb-sync.so.1\n08181b12992b6ae3072605230a496753a3776e3412a3d2330965686ddc8476da  /usr/lib/libxcb-randr.so.0\na042ea717a104be459d99177c70ef7159dfc9e9bea2e385eee6f09d594a95d0e  /usr/lib/libxcb-shm.so.0\n97d7aa57d81e848b81304d88a3fb11329339248ba4d39104a0301eed7f12903c  /usr/lib/libxshmfence.so.1\ne6f8be13631b0bf4019a2744cda2646b9b9c423e1341105e824b4f0968ca1546  /usr/lib/libexpat.so.1\ne8ac1c2fa534bbfc97b97819c642e4dd1029b6ff1487506799d054ddf3345da7  /usr/lib/libXau.so.6\n04760cbd29fe7478d596bf14a4fa06ea558f5ef66fd0c2d57d98edd706d0e2d6  /usr/lib/libXdmcp.so.6\nd382fb1aa8f844b4d26082615557ea7838bba09d60f9cd6c3fd583da0c4e42ef  /usr/lib/libbsd.so.0\n88abe567e4224c6d400bb74aac8a458f71b29b087bd1ddfa873850358d959014  /usr/lib/libmd.so.0'+"\nRUNTIME_HASHES\n[ $? -eq 0 ] || fail runtime_dependency_hash\ntimeout -s KILL 15 vulkaninfo")

def init_script(reuse=False):
 start=W.guest_script().split('apk add --no-cache --no-progress mesa-vulkan-virtio=')[0]
 wsi=W.WSI.replace('26.1.6-r0','25.2.7-r1').replace('21.1.25-r0','21.1.23-r0')
 wsi=wsi.replace('eudev xwininfo','eudev libxrender-dev mesa-dev xwininfo')
 wsi=wsi.replace('export MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink','export MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu GALLIUM_DRIVER=virgl')
 wsi=wsi.replace('Xorg :0 vt1','Xorg :0 vt1 -modulepath /veynode/modules-fixed,/usr/lib/xorg/modules')
 wsi=wsi.replace("grep -qi 'zink' /run/glxinfo || fail gl_not_zink", "grep -q 'Device: virgl' /run/glxinfo || fail gl_not_virgl")
 wsi=wsi.replace("grep -q 'Apple M1 Pro' /run/glxinfo || fail gl_not_apple", "# virgl GL identity is virtual; actual Apple identity is separately required from Venus")
 gate=r'''grep '/veynode/modules-fixed/libglamoregl.so' /proc/$xorg/maps || fail fixed_module_map
if grep -q '/usr/lib/xorg/modules/libglamoregl.so' /proc/$xorg/maps; then fail stock_module_map; fi
mkdir -p /run/xrender
timeout -s KILL 30 cc -O2 -Wall -Wextra /veynode/xrender.c -o /run/xrender-probe -lX11 -lXrender -lGL || fail xrender_compile
timeout -s KILL 30 /run/xrender-probe /run/xrender > /run/xrender.log 2>&1
rc=$?; cat /run/xrender.log
[ "$rc" -eq 0 ] || fail xrender_pixels
[ "$(grep -c '^XRENDER_CASE ' /run/xrender.log)" -eq 15 ] || fail xrender_count
grep -Fxq 'XRENDER_SUMMARY {"cases":15,"failed":0,"passed":true}' /run/xrender.log || fail xrender_summary
'''
 wsi=wsi.replace("[ -f /usr/lib/vkmark/xcb.so ]",gate+"[ -f /usr/lib/vkmark/xcb.so ]")
 # No 3.24 APK reuse: same Alpine3.23 vkmark package with normal signature checks.
 return start+(PREBUILT if reuse else BUILD)+'\napk add --no-cache --no-progress vkmark=2025.01-r1 || fail vkmark_package\n'+wsi+'\n'+C.END.replace('VEYNODE_CONTROL_VENUS_READY','VEYNODE_WSI_READY')

def prepare(out,vm,reuse=False):
 if not vm or len(vm)>48 or not all(c.isascii() and(c.isalnum() or c in '-_')for c in vm):raise ValueError('ID')
 if hashlib.sha512(SOURCE.read_bytes()).hexdigest()!=SHA512:raise ValueError('official source hash')
 if C.A.base.digest(GLAMOR)!=GLAMOR_SHA:raise ValueError('fixed glamor hash')
 C.pinned(ROOT/'.local/qemu-observer-guest-v1/veynode-agent-r3',C.AGENT_SHA,8*1024*1024)
 if reuse:
  C.pinned(BUILT,BUILT_SHA,4*1024*1024)
  with tarfile.open(BUILT)as t:
   for m in t.getmembers():
    if m.name.startswith('/') or '..'in Path(m.name).parts or not(m.isdir()or m.isfile()):raise ValueError('unsafe built artifact')
 out.mkdir(mode=0o700,parents=True,exist_ok=False)
 init=init_script(reuse);subprocess.run(['/bin/sh','-n'],input=init,text=True,check=True)
 initrd,meta=Q.fixture(out,init_transform=lambda _:init)
 entries=[]
 for n,p in [(('veynode/venus25.tgz',BUILT) if reuse else ('veynode/mesa-25.2.7.tar.xz',SOURCE)),('veynode/mesa25-build.py',ROOT/'scripts/linux-vulkan-mesa25-build.py'),('veynode/observer-agent',ROOT/'.local/qemu-observer-guest-v1/veynode-agent-r3'),('veynode/modules-fixed/libglamoregl.so',GLAMOR),('veynode/xrender.c',ROOT/'scripts/linux-xrender-probe.c')]:
  if n.endswith('libglamoregl.so'):entries.append(C.F.entry('veynode/modules-fixed',b'',0o40755))
  entries.append(C.F.entry(n,p.read_bytes(),0o100755 if n.endswith('observer-agent') else 0o100644))
 entries.append(C.F.entry('TRAILER!!!',b''))
 with initrd.open('ab')as f:f.write(gzip.compress(b''.join(entries),mtime=0))
 (out/'guest-init.sh').write_text(init)
 reg=C.registration(json.loads((ROOT/'.local/linux-vulkan-host-runtime-v1/manifest.json').read_text()),json.loads((ROOT/'crates/control/src/qemu_venus_catalog.json').read_text()))
 (out/'venus-runtime.json').write_text(json.dumps(reg,indent=2)+'\n');os.chmod(out/'venus-runtime.json',0o600)
 config={'id':vm,'backend':'qemu-hvf','guest':'linux','cpus':2,'memory_mib':2048,'window':True,'width':1280,'height':800,'qemu_graphics_profile':'venus-moltenvk-experimental','kernel':str(Q.MEDIA/'Image'),'initrd':str(initrd),'command_line':'console=ttyAMA0 rdinit=/veynode/gpu-init','serial_log':str(out/'serial.log')}
 (out/'config.json').write_text(json.dumps(config,indent=2)+'\n')
 paths=[*([BUILT] if reuse else []),Path(__file__),ROOT/'scripts/linux-vulkan-mesa25-build.py',Q.MEDIA/'Image',ROOT/'scripts/test-control-qemu.py',ROOT/'scripts/linux-vulkan-wsi-prepare.py',ROOT/'scripts/linux-vulkan-wsi-test.py',ROOT/'scripts/linux-xrender-probe.c',SOURCE,GLAMOR,initrd,out/'guest-init.sh',out/'config.json',out/'venus-runtime.json',ROOT/'.local/qemu-observer-guest-v1/veynode-agent-r3']
 report={'schema':1,'vm_run':False,'disk_attached':False,'desktop_gl':'virgl','normal_markers':['VEYNODE_ACPI_POWER_BUTTON','VEYNODE_INIT_NORMAL_SHUTDOWN'],'mesa_source_sha512':SHA512,'source_url':'https://archive.mesa3d.org/mesa-25.2.7.tar.xz','reuse_actual_built_icd':reuse,'build_seconds_limit':0 if reuse else 600,'build_jobs':2,'rootfs':meta,'inputs':[{'path':str(p),'sha256':C.A.base.digest(p)}for p in paths], 'limits':['No mixed Mesa packages; standalone Venus25 built from official source, same-version packaged virgl','Build/toolchain transitive APK versions recorded, not every dependency pinned','No source patches or LLVM; runtime app/WSI remains to be tested']}
 (out/'fixture.json').write_text(json.dumps(report,indent=2)+'\n');return report
if __name__=='__main__':
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('--output-dir',type=Path,required=True);p.add_argument('--id',required=True);p.add_argument('--reuse-built-icd',action='store_true');a=p.parse_args();os.umask(0o077);prepare(a.output_dir.absolute(),a.id,a.reuse_built_icd);print('prepared; no VM started')
