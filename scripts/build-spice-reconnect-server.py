#!/usr/bin/env python3
"""Build isolated Darwin SPICE reconnect experiment, reusing unchanged UTM libraries.

Requires official UTM and CocoaSpice probe prerequisites already downloaded.
Never rewrites the app. Builds with at most two compiler jobs. Saves baseline
before applying the project-owned fresh-client patch; not a release build.
"""
import json,os,pathlib,shutil,subprocess,sys
ROOT=pathlib.Path(__file__).resolve().parents[1];BASE=ROOT/'.local/spice-reconnect-build';BASE.mkdir(exist_ok=True);RUNTIME=BASE/'runtime';RUNTIME.mkdir(exist_ok=True);PATCHES=ROOT/'.local/windows-gpu/UTM-5.0.5/patches';FW=ROOT/'.local/windows-gpu/UTM.app/Contents/Frameworks'
def run(args,cwd=ROOT):subprocess.run([str(x) for x in args],cwd=cwd,check=True)
run([sys.executable,ROOT/'scripts/prepare-spice-reconnect-build.py'])
server=BASE/'spice-0.14.3'
if not server.is_dir():raise SystemExit('Pinned SPICE source preparation failed')
for name,patch in [('openssl-1.1.1b','openssl-1.1.1b.patch'),('spice-0.14.3','spice-0.14.3.patch')]:
 path=BASE/name;marker=path/'.veynode-upstream-patched'
 if not marker.exists():run(['patch','--batch','-p1','-i',PATCHES/patch],path);marker.touch()
venv=BASE/'venv'
if not (venv/'bin/python').exists():run([sys.executable,'-m','venv',venv])
run([venv/'bin/pip','install','meson==1.12.1','six==1.17.0','pyparsing==2.4.7'])
if not (BASE/'glib-config/meson-private/coredata.dat').exists():run(['meson','setup',BASE/'glib-config',BASE/'glib-2.83.0','-Dtests=false','-Dinstalled_tests=false','-Ddocumentation=false','-Dintrospection=disabled','-Dlibmount=disabled','-Dselinux=disabled','-Dsysprof=disabled','-Ddtrace=false','-Dsystemtap=false'])
run(['ninja','-C',BASE/'glib-config','-j2','glib/gversionmacros.h','glib/glib-visibility.h','gobject/glib-enumtypes.h','gobject/gobject-visibility.h','gio/gioenumtypes.h','gio/gio-visibility.h','gmodule/gmodule-visibility.h'])
for name,flags,marker in [('pixman-0.38.0',['--disable-gtk','--disable-libpng','--disable-dependency-tracking'],'pixman/pixman-version.h'),('libjpeg-turbo-1.5.3',['--without-simd','--disable-dependency-tracking'],'jconfig.h')]:
 if not (BASE/name/marker).exists():run(['./configure',*flags],BASE/name)
ssl=BASE/'openssl-1.1.1b'
if not (ssl/'Makefile').exists():run(['./Configure','darwin64-arm64-cc','no-shared','no-tests'],ssl)
run(['make','-j2','build_generated'],ssl)
def build():
 run([sys.executable,ROOT/'scripts/configure-spice-reconnect-build.py']);run(['ninja','-C',BASE/'spice-build','-j2','server/libspice-server.1.dylib'])
def save_library(name):
 dest=RUNTIME/name;temporary=dest.with_suffix('.new.dylib');shutil.copyfile(BASE/'spice-build/server/libspice-server.1.dylib',temporary)
 run(['install_name_tool','-id','@rpath/spice-server.1.framework/Versions/A/spice-server.1',temporary]);run(['codesign','-f','-s','-',temporary]);temporary.replace(dest)
if not (RUNTIME/'libspice-server-baseline.dylib').exists():
 if (server/'.veynode-reconnect-fixed').exists():raise SystemExit('Baseline missing from modified scratch; use a fresh isolated checkout/build directory')
 build();save_library('libspice-server-baseline.dylib')
if not (server/'.veynode-reconnect-fixed').exists():run([sys.executable,ROOT/'scripts/apply-spice-reconnect-fix.py'])
run([sys.executable,ROOT/'scripts/apply-spice-retired-publication-fix.py'])
# Keep project-owned helper source synchronized; integration patch is guarded above.
for name in ['spice-scanout-generation.c','spice-scanout-generation.h']:shutil.copyfile(ROOT/'native/tools'/name,server/'server'/name)
build();save_library('libspice-server-fixed.dylib')
run(['clang',ROOT/'native/tools/spice-reconnect-loader.c','-Wl,-rpath,'+str(FW),'-o',RUNTIME/'probe-loader'])
entitlements=RUNTIME/'hypervisor.plist';entitlements.write_text('<?xml version="1.0"?><plist version="1.0"><dict><key>com.apple.security.hypervisor</key><true/></dict></plist>')
run(['codesign','-f','-s','-','--entitlements',entitlements,RUNTIME/'probe-loader'])
env=os.environ.copy();env.update(PKG_CONFIG_LIBDIR=str(BASE/'pkgconfig'),PKG_CONFIG_PATH=str(BASE/'pkgconfig'))
flags=subprocess.check_output(['pkg-config','--cflags','--libs','glib-2.0','pixman-1','openssl','libjpeg','spice-protocol'],env=env,text=True).split()
run(['clang',ROOT/'native/tools/spice-reconnect-abi-probe.c','-I'+str(server/'server'),*flags,'-F'+str(FW),'-framework','spice-server.1','-o',BASE/'abi-probe'])
abi=subprocess.check_output([str(BASE/'abi-probe')],text=True);(BASE/'abi-probe.json').write_text(abi)
print(json.dumps({'baseline':str(RUNTIME/'libspice-server-baseline.dylib'),'fixed':str(RUNTIME/'libspice-server-fixed.dylib'),'scope':'Independent diagnostic library and loader; unchanged official app; no release claim'},indent=2))
