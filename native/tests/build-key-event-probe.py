#!/usr/bin/env python3
"""Build a fresh own-window key event probe; never launch it."""
import argparse,ctypes,hashlib,json,os,pathlib,plistlib,shutil,subprocess,tempfile
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--output-app',required=True,type=pathlib.Path)
p.add_argument('--guest-output',required=True,type=pathlib.Path)
a=p.parse_args()
for path in [a.output_app,a.guest_output]:
    if not path.is_absolute() or path.exists() or path.is_symlink() or path.parent.resolve()!=path.parent:
        p.error('fresh absolute output paths with existing non-symlink parents required')
temporary=pathlib.Path(tempfile.mkdtemp(prefix='.key-probe-',dir=a.output_app.parent))
try:
    binary=temporary/'Contents/MacOS/key-event-probe';binary.parent.mkdir(parents=True)
    subprocess.run(['xcrun','swiftc','-swift-version','5','-O','-target','arm64-apple-macosx14.0','-framework','AppKit',str(pathlib.Path(__file__).with_name('key-event-probe.swift')),'-o',str(binary)],check=True)
    subprocess.run([str(binary),'--self-test'],check=True)
    (temporary/'Contents/Info.plist').write_bytes(plistlib.dumps({'CFBundleIdentifier':'com.veynode.key-event-probe.dev','CFBundleExecutable':'key-event-probe','CFBundleName':'Veynode Key Probe','CFBundlePackageType':'APPL','CFBundleVersion':'1','LSMinimumSystemVersion':'14.0','NSPrincipalClass':'VeynodeKeyApplication'}))
    subprocess.run(['/usr/bin/codesign','--force','--sign','-',str(temporary)],check=True)
    subprocess.run(['/usr/bin/codesign','--verify','--strict',str(temporary)],check=True)
    libc=ctypes.CDLL('/usr/lib/libSystem.B.dylib',use_errno=True)
    libc.renamex_np.argtypes=[ctypes.c_char_p,ctypes.c_char_p,ctypes.c_uint]
    if libc.renamex_np(os.fsencode(temporary),os.fsencode(a.output_app),4)!=0:raise OSError(ctypes.get_errno(),'exclusive app publish failed')
    binary=a.output_app/'Contents/MacOS/key-event-probe'
    fd=os.open(a.guest_output,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o700)
    with os.fdopen(fd,'wb') as f:f.write(binary.read_bytes())
    print(json.dumps({'app':str(a.output_app),'guest_binary':str(a.guest_output),'sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'launched':False}))
finally:
    if temporary.exists():shutil.rmtree(temporary)
