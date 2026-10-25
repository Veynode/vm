#!/usr/bin/env python3
"""Selective source build using exact UTM source headers and unchanged official libraries."""
import os,pathlib,subprocess
ROOT=pathlib.Path(__file__).resolve().parents[1];BASE=ROOT/'.local/spice-reconnect-build';PC=BASE/'pkgconfig';PC.mkdir(exist_ok=True)
FRAMEWORKS=ROOT/'.local/runtime/utm/UTM.app/Contents/Frameworks'
framework=lambda name:f'-F{FRAMEWORKS} -framework {name} -Wl,-rpath,{FRAMEWORKS}'
glib=[BASE/'glib-2.83.0',BASE/'glib-2.83.0/glib',BASE/'glib-config',BASE/'glib-config/glib',BASE/'glib-2.83.0/gmodule',BASE/'glib-config/gmodule']
def pc(name,version,includes,libs,requires=''):
 text=f'Name: {name}\nDescription: Isolated verified UTM source headers and official frameworks\nVersion: {version}\nCflags: '+ ' '.join('-I'+str(p) for p in includes)+'\nLibs: '+libs+'\nRequires: '+requires+'\n'
 if name=='glib-2.0':text=f'glib_mkenums={BASE}/glib-config/gobject/glib-mkenums\nglib_genmarshal={BASE}/glib-config/gobject/glib-genmarshal\n'+text
 (PC/(name+'.pc')).write_text(text)
pc('glib-2.0','2.83.0',glib,framework('glib-2.0.0'))
pc('gobject-2.0','2.83.0',glib,framework('gobject-2.0.0'),'glib-2.0')
pc('gio-2.0','2.83.0',glib,framework('gio-2.0.0'),'gobject-2.0')
pc('gio-unix-2.0','2.83.0',glib+[BASE/'glib-2.83.0/gio'],framework('gio-2.0.0'),'gio-2.0')
pc('gthread-2.0','2.83.0',glib,framework('gthread-2.0.0'),'glib-2.0')
pc('pixman-1','0.38.0',[BASE/'pixman-0.38.0/pixman'],framework('pixman-1.0'))
pc('openssl','1.1.1b',[BASE/'openssl-1.1.1b/include'],framework('ssl.1.1')+' '+framework('crypto.1.1'))
pc('libjpeg','1.5.3',[BASE/'libjpeg-turbo-1.5.3'],framework('jpeg.62'))
pc('spice-protocol','0.14.4',[BASE/'spice-protocol-0.14.4'],'')
pc('zlib','1.2.12',[],'-lz')
env=os.environ.copy();env['PATH']=str(BASE/'venv/bin')+os.pathsep+env['PATH'];env.update(PKG_CONFIG_LIBDIR=str(PC),PKG_CONFIG_PATH=str(PC))
subprocess.run(['meson','setup',*(['--clearcache','--reconfigure'] if (BASE/'spice-build/meson-private/coredata.dat').exists() else []),str(BASE/'spice-build'),str(BASE/'spice-0.14.3'),'-Dgstreamer=no','-Dlz4=false','-Dsasl=false','-Dopus=disabled','-Dsmartcard=disabled','-Dmanual=false','-Dspice-common:opus=disabled','--buildtype=debugoptimized'],env=env,check=True)
