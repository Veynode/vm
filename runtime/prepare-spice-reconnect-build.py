#!/usr/bin/env python3
"""Fetch locked UTM dependency snapshots into isolated scratch; no app edits.

Pins identify archives used for this diagnostic and are checked BEFORE extraction.
They are snapshot integrity pins, not claims of independent vendor signatures.
"""
import concurrent.futures
import hashlib
import json
import os
import pathlib
import subprocess
import tarfile
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
BASE = ROOT / '.local/spice-reconnect-build'
BASE.mkdir(exist_ok=True)
SOURCES = [
    ('glib-2.83.0.tar.xz', 'glib-2.83.0', 'https://download.gnome.org/sources/glib/2.83/glib-2.83.0.tar.xz', 'a07d9e1a57a4279c5ece71c26dc44eea12bd518ea9ff695d53e722997032b614'),
    ('pixman-0.38.0.tar.gz', 'pixman-0.38.0', 'https://www.cairographics.org/releases/pixman-0.38.0.tar.gz', 'a7592bef0156d7c27545487a52245669b00cf7e70054505381cff2136d890ca8'),
    ('libjpeg-turbo-1.5.3.tar.gz', 'libjpeg-turbo-1.5.3', 'https://ftp.osuosl.org/pub/blfs/conglomeration/libjpeg-turbo/libjpeg-turbo-1.5.3.tar.gz', 'b24890e2bb46e12e72a79f7e965f409f4e16466d00e1dd15d93d73ee6b592523'),
    ('openssl-1.1.1b.tar.gz', 'openssl-1.1.1b', 'https://github.com/openssl/openssl/releases/download/OpenSSL_1_1_1b/openssl-1.1.1b.tar.gz', '5c557b023230413dfb0756f3137a13e6d726838ccd1430888ad15bfb2b43ea4b'),
    ('spice-protocol-0.14.4.tar.xz', 'spice-protocol-0.14.4', 'https://www.spice-space.org/download/releases/spice-protocol-0.14.4.tar.xz', '04ffba610d9fd441cfc47dfaa135d70096e60b1046d2119d8db2f8ea0d17d912'),
    ('spice-0.14.3.tar.bz2', 'spice-0.14.3', 'https://www.spice-space.org/download/releases/spice-server/spice-0.14.3.tar.bz2', '551d4be4a07667cf0543f3c895beb6da8a93ef5a9829f2ae47817be5e616a114'),
]


def digest(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def fetch(source):
    name, top, url, expected = source
    path = BASE / name
    if not path.exists():
        with tempfile.TemporaryDirectory(prefix='.download-', dir=BASE) as temp:
            download = pathlib.Path(temp) / name
            subprocess.run(['curl', '-fL', '--retry', '1', '--max-time', '120', url, '-o', str(download)],
                           check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            if digest(download) != expected:
                raise RuntimeError('Downloaded archive checksum mismatch: ' + name)
            try:
                os.link(download, path)  # Atomic no-replace cache publication.
            except FileExistsError:
                pass  # Verify the competing publication below, never overwrite it.
    if digest(path) != expected:
        raise RuntimeError('Cached archive checksum mismatch; refusing extraction: ' + name)
    if not (BASE / top).exists():
        # A killed/failed extraction cannot leave a half-populated final source directory.
        with tempfile.TemporaryDirectory(prefix='.extract-', dir=BASE) as temp:
            with tarfile.open(path) as archive:
                if any(pathlib.PurePosixPath(member.name).parts[0] != top for member in archive.getmembers()):
                    raise RuntimeError('Archive has unexpected source roots: ' + name)
                archive.extractall(temp, filter='data')
            (pathlib.Path(temp) / top).rename(BASE / top)
    return {'file': name, 'url': url, 'sha256': expected, 'source_directory': top,
            'archive_verified_before_extraction': True}


if __name__ == '__main__':
    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
        results = list(pool.map(fetch, SOURCES))
    (BASE / 'sources.json').write_text(json.dumps(results, indent=2) + '\n')
    print(json.dumps(results, indent=2))
