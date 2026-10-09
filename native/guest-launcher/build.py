#!/usr/bin/env python3
"""Build a new, self-contained Guest-only foreground service bundle. Never install it."""
import argparse, ctypes, hashlib, json, os, pathlib, shutil, stat, subprocess, tempfile

HERE = pathlib.Path(__file__).resolve().parent

def regular(path):
    path = pathlib.Path(path).absolute()
    s = path.lstat()
    if not stat.S_ISREG(s.st_mode) or s.st_nlink != 1 or s.st_uid != os.getuid() or s.st_size <= 0 or s.st_size > 64 * 1024 * 1024:
        raise ValueError('input must be owned single-link regular file, 1..64MiB')
    if path.resolve() != path:
        raise ValueError('input path must not contain symlinks')
    attributes = subprocess.run(['/usr/bin/xattr', str(path)], check=True, capture_output=True).stdout.splitlines()
    if b'com.apple.quarantine' in attributes:
        raise ValueError('quarantined input must not be repackaged to bypass its review')
    return path

def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--integration', required=True, type=regular)
    p.add_argument('--capture', required=True, type=regular)
    p.add_argument('--output', required=True, type=pathlib.Path)
    args = p.parse_args()
    output = args.output
    if not output.is_absolute() or output.exists() or output.is_symlink() or output.parent.resolve() != output.parent:
        p.error('output must be new absolute path with an existing non-symlink parent')
    temporary = pathlib.Path(tempfile.mkdtemp(prefix='.guest-bundle-', dir=output.parent))
    try:
        subprocess.run(['xcrun', 'swiftc', '-swift-version', '5', '-O', '-target', 'arm64-apple-macosx14.0',
                        '-framework', 'CoreGraphics', str(HERE / 'main.swift'), '-o', str(temporary / 'launcher')], check=True)
        for name, source in [('integration', args.integration), ('capture', args.capture)]:
            shutil.copyfile(source, temporary / name)
        records = {}
        for name in ['launcher', 'integration', 'capture']:
            path = temporary / name
            path.chmod(0o700)
            data = path.read_bytes()
            records[name] = {'sha256': hashlib.sha256(data).hexdigest(), 'bytes': len(data)}
        manifest = temporary / 'manifest.json'
        manifest.write_text(json.dumps({'version': 1, 'files': records}, sort_keys=True, separators=(',', ':')) + '\n')
        manifest.chmod(0o600)
        subprocess.run([str(temporary / 'launcher'), 'verify'], check=True)
        # Darwin's public renamex_np(RENAME_EXCL=4) publishes the entire verified
        # directory atomically and cannot overwrite a concurrent publisher.
        libc = ctypes.CDLL('/usr/lib/libSystem.B.dylib', use_errno=True)
        libc.renamex_np.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint]
        libc.renamex_np.restype = ctypes.c_int
        if libc.renamex_np(os.fsencode(temporary), os.fsencode(output), 4) != 0:
            error = ctypes.get_errno()
            raise OSError(error, os.strerror(error))
        print(json.dumps({'bundle': str(output), 'source': {'integration': str(args.integration), 'capture': str(args.capture)},
                          'files': records, 'installed_in_guest': False, 'service_started': False}, sort_keys=True))
    finally:
        if temporary.exists():
            shutil.rmtree(temporary)

if __name__ == '__main__':
    main()
