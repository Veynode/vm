#!/usr/bin/env python3
"""Fail-closed packaging of an exact, reviewed local runtime inventory.

This mechanical gate is not legal advice or a license verifier. The checked-in
experiment is blocked. Review evidence is required even when bytes are pinned.
No download, VM, signing, notarization or publication operation is performed.
"""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import re
import stat
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]
REQUIRED = ('component_inventory', 'license_notices', 'corresponding_source',
            'build_and_patch_recipe', 'relink_and_installation',
            'redistribution_review', 'product_license')
MAX_FILES = 20000
MAX_BYTES = 8 * 1024**3


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()


def pinned_file(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as source:
        before = os.fstat(source.fileno())
        if not stat.S_ISREG(before.st_mode) or before.st_size > MAX_BYTES:
            raise ValueError('not a bounded regular file')
        digest = hashlib.sha256(); consumed = 0
        while chunk := source.read(min(1024 * 1024, before.st_size + 1 - consumed)):
            consumed += len(chunk)
            if consumed > before.st_size or consumed > MAX_BYTES: raise ValueError('file grew during hashing')
            digest.update(chunk)
        after = os.fstat(source.fileno())
        if (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns):
            raise ValueError('file changed during hashing')
        return {'bytes': before.st_size, 'sha256': digest.hexdigest()}


def local_path(base, relative):
    if not isinstance(relative, str) or not relative or Path(relative).is_absolute() or '..' in Path(relative).parts:
        raise ValueError('invalid relative path')
    path = base / relative
    # Reject symlinked parents; bundle-internal symlinks are handled by inventory.
    cur = base
    for part in Path(relative).parts:
        cur = cur / part
        if cur.is_symlink(): raise ValueError('input path crosses a symlink')
    return path


def inventory(path):
    if path.is_symlink() or not path.is_dir(): raise ValueError('runtime root is not a real directory')
    records = []; problems = []; total = 0
    def walk(directory):
        nonlocal total
        for item in sorted(directory.iterdir()):
            rel = item.relative_to(path).as_posix(); st = item.lstat()
            if len(records) >= MAX_FILES: raise ValueError('inventory file limit exceeded')
            if stat.S_ISLNK(st.st_mode):
                target = os.readlink(item)
                # Absolute link bytes are hashed but never exposed in public reports.
                records.append({'path': rel, 'type': 'symlink', 'target_sha256': hashlib.sha256(os.fsencode(target)).hexdigest()})
                try: internal = not os.path.isabs(target) and item.resolve(strict=True).is_relative_to(path.resolve())
                except (OSError, RuntimeError): internal = False
                if not internal: problems.append('nonportable_or_external_symlink:' + rel)
            elif stat.S_ISDIR(st.st_mode):
                records.append({'path': rel, 'type': 'directory', 'mode': stat.S_IMODE(st.st_mode)})
                walk(item)
            elif stat.S_ISREG(st.st_mode):
                info = pinned_file(item);total += info['bytes']
                if total > MAX_BYTES: raise ValueError('inventory byte limit exceeded')
                records.append({'path': rel, 'type': 'file', 'mode': stat.S_IMODE(st.st_mode), **info})
            else: raise ValueError('special file in runtime')
    walk(path)
    return {'tree_sha256': hashlib.sha256(canonical(records)).hexdigest(),
            'regular_files': sum(x['type'] == 'file' for x in records),
            'symlinks': sum(x['type'] == 'symlink' for x in records),
            'bytes': total, 'problems': problems}


def check(manifest, base):
    if set(manifest) != {'schema', 'status', 'scope', 'roots', 'evidence', 'blockers'} or manifest['schema'] != 1:
        raise ValueError('unsupported manifest schema')
    if manifest['status'] not in ('blocked', 'reviewed'): raise ValueError('invalid review status')
    if not isinstance(manifest['blockers'], list): raise ValueError('invalid blockers')
    roots = manifest['roots']
    if not isinstance(roots, list) or not 1 <= len(roots) <= 8: raise ValueError('invalid runtime roots')
    errors = []; observed = {}; paths = []; ids = set()
    if manifest['status'] != 'reviewed': errors.append('release_review_not_complete')
    if manifest['blockers']: errors.append('unresolved_release_blockers')
    for entry in roots:
        if set(entry) != {'id', 'path', 'tree_sha256'} or not re.fullmatch('[a-z0-9-]{1,64}', entry['id']) or entry['id'] in ids:
            raise ValueError('invalid root entry')
        ids.add(entry['id']);path = local_path(base, entry['path'])
        if any(path.is_relative_to(p) or p.is_relative_to(path) for p in paths): raise ValueError('overlapping roots')
        paths.append(path); actual = inventory(path); observed[entry['id']] = actual
        if actual['tree_sha256'] != entry['tree_sha256']: errors.append('inventory_mismatch:' + entry['id'])
        errors.extend(entry['id'] + ':' + p for p in actual['problems'])
    evidence = manifest['evidence']
    if not isinstance(evidence, dict) or set(evidence) != set(REQUIRED): raise ValueError('invalid evidence schema')
    for kind in REQUIRED:
        item = evidence[kind]
        if item is None: errors.append('missing_evidence:' + kind);continue
        if set(item) != {'path', 'sha256'}: raise ValueError('invalid evidence record')
        path = local_path(base, item['path']);actual = pinned_file(path)
        if not actual['bytes'] or actual['sha256'] != item['sha256']: errors.append('evidence_mismatch:' + kind)
    return {'release_allowed': not errors, 'errors': errors, 'observed_roots': observed}


def package(manifest, base, output):
    result = check(manifest, base)
    if not result['release_allowed']: return result
    if output.exists() or output.is_symlink(): raise ValueError('output already exists')
    output = output.parent.resolve(strict=True) / output.name
    for entry in manifest['roots']:
        if output.is_relative_to(local_path(base, entry['path']).resolve()): raise ValueError('output inside input root')
    # Quiescent trusted build directory required. Recheck after tar, before atomic publication.
    with tempfile.TemporaryDirectory(prefix='.runtime-package-', dir=output.parent) as temp:
        stage = Path(temp) / 'runtime.tar.gz'
        with tarfile.open(stage, 'w:gz', dereference=False) as archive:
            payload = canonical(manifest) + b'\n'
            record = tarfile.TarInfo('release-evidence/manifest.json')
            record.size = len(payload); record.mode = 0o600
            archive.addfile(record, io.BytesIO(payload))
            for entry in manifest['roots']:
                archive.add(local_path(base, entry['path']), arcname='runtime/' + entry['id'])
            for kind, item in manifest['evidence'].items():
                archive.add(local_path(base, item['path']), arcname='release-evidence/' + kind + '/' + Path(item['path']).name)
        if check(manifest, base) != result: raise ValueError('inputs changed while packaging')
        os.chmod(stage, 0o600); os.link(stage, output)  # Atomic no-overwrite publication.
    return {**result, 'archive': pinned_file(output)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest', type=Path, default=ROOT/'licenses/experimental-runtime.json')
    parser.add_argument('--output', type=Path, help='Optional fresh archive; never created if review is blocked')
    args = parser.parse_args()
    try:
        fd = os.open(args.manifest, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, 'rb') as source:
            before = os.fstat(source.fileno())
            if not stat.S_ISREG(before.st_mode): raise ValueError('manifest is not regular')
            if before.st_size > 1024*1024: raise ValueError('manifest too large')
            raw = source.read(1024*1024 + 1)
            after = os.fstat(source.fileno())
            if (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns) or len(raw) != before.st_size:
                raise ValueError('manifest changed during read')
        if len(raw) > 1024*1024: raise ValueError('manifest too large')
        manifest = json.loads(raw)
        result = package(manifest, ROOT, args.output) if args.output else check(manifest, ROOT)
    except (OSError, ValueError, TypeError, KeyError) as exc:
        print(json.dumps({'release_allowed': False, 'error': type(exc).__name__}));return 2
    print(json.dumps(result, indent=2));return 0 if result['release_allowed'] else 2

if __name__ == '__main__': raise SystemExit(main())
