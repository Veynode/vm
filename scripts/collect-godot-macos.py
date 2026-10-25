#!/usr/bin/env python3
"""Collect an existing explicit Godot export through files-scoped MCP only.

Never starts Godot or operates a VM. Exit zero means collection integrity and
MCP cleanup, not rendering/GPU acceptance. Output directory must be private and
already exist; all files are created exclusively. Requires Pillow for host decode.
"""
import argparse
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import select
import stat
import struct
import zlib
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('godot_blender_collect', ROOT/'archive/benchmarks/blender-macos/collect.py')
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)
CASES = ('OmniLightNoShadows', 'OmniLightWithShadows', 'OmniLightWithPCSSShadows',
         'SpotLightWithShadows', 'SpotLightWithPCSSShadows', 'SpotLightWithProjector')
MAX_FILE = 8*1024*1024
JSON_LIMIT = 1024*1024


def require(value, message):
    if not value:
        raise ValueError(message)


def strict_json(data):
    def obj(pairs):
        result = {}
        for key, value in pairs:
            require(key not in result, 'duplicate JSON field')
            result[key] = value
        return result
    def nonfinite(_):
        raise ValueError('nonfinite JSON number')
    require(len(data) <= JSON_LIMIT, 'JSON size bound')
    try:
        return json.loads(data, object_pairs_hook=obj, parse_constant=nonfinite)
    except (UnicodeError, json.JSONDecodeError, RecursionError):
        raise ValueError('invalid artifact JSON') from None


def allowed_names(stem):
    return {stem+'-'+x for x in ('result.json', 'probe.json', 'import.log', 'render.log')} | {
        f'{stem}-{case}-{sample}.png' for case in CASES for sample in range(2)}


def manifest(data, stem):
    require(len(data) <= 65536, 'manifest size bound')
    value = strict_json(data)
    require(isinstance(value, dict) and set(value) == {'schema', 'files', 'render_passed', 'fixture_manifest_sha256'}, 'manifest schema')
    require(type(value['schema']) is int and value['schema'] == 1 and type(value['render_passed']) is bool, 'manifest version/status')
    require(isinstance(value['fixture_manifest_sha256'], str) and re.fullmatch('[0-9a-f]{64}', value['fixture_manifest_sha256']), 'fixture digest')
    require(isinstance(value['files'], list) and 1 <= len(value['files']) <= 16, 'manifest file count')
    names = set()
    for item in value['files']:
        require(isinstance(item, dict) and set(item) == {'name', 'bytes', 'sha256'}, 'manifest entry schema')
        name = item['name']
        require(isinstance(name, str) and name in allowed_names(stem) and name not in names, 'artifact name/duplicate')
        names.add(name)
        require(type(item['bytes']) is int and 0 <= item['bytes'] <= MAX_FILE, 'artifact size')
        require(isinstance(item['sha256'], str) and re.fullmatch('[0-9a-f]{64}', item['sha256']), 'artifact digest')
    require(stem+'-result.json' in names, 'missing result artifact')
    return value


def decode_png(data):
    from PIL import Image, ImageFile
    require(not ImageFile.LOAD_TRUNCATED_IMAGES, 'Pillow truncated decoding enabled')
    require(len(data) <= MAX_FILE and data[:8] == b'\x89PNG\r\n\x1a\n', 'PNG size/signature')
    offset, ended = 8, False
    while offset < len(data):
        require(offset+12 <= len(data), 'PNG truncated chunk')
        size = struct.unpack('>I', data[offset:offset+4])[0]
        end = offset+12+size
        require(end <= len(data), 'PNG truncated data')
        kind, payload = data[offset+4:offset+8], data[offset+8:end-4]
        require(zlib.crc32(kind+payload)&0xffffffff == struct.unpack('>I', data[end-4:end])[0], 'PNG checksum')
        if kind == b'IEND':
            require(size == 0 and end == len(data), 'PNG trailing data')
            ended = True
        offset = end
    require(ended, 'PNG missing end')
    try:
        with Image.open(io.BytesIO(data)) as im:
            require(im.format == 'PNG' and im.size == (1280, 720) and im.mode in ('RGB', 'RGBA')
                    and getattr(im, 'n_frames', 1) == 1, 'PNG geometry/format')
            im.verify()
        with Image.open(io.BytesIO(data)) as im:
            im.load()
            pixels = im.convert('RGB').tobytes()
            require(len(pixels) == 1280*720*3, 'PNG decoded size')
        return {'width':1280, 'height':720, 'decoded_rgb_sha256':hashlib.sha256(pixels).hexdigest()}
    except ValueError:
        raise
    except Exception:
        raise ValueError('PNG full decode failed') from None


class BoundedMCP(shared.MCP):
    """Reuse the existing MCP/file helper, with absolute read/write deadlines."""
    def __init__(self, binary, state, deadline):
        self.deadline = deadline
        self.rpc_deadline = min(deadline, time.monotonic()+12)
        super().__init__(binary, state, files=True)
        os.set_blocking(self.process.stdin.fileno(), False)
    def send(self, value):
        data = (json.dumps(value)+'\n').encode()
        require(len(data) <= 65536, 'MCP request size')
        fd = self.process.stdin.fileno()
        os.set_blocking(fd, False)
        while data:
            left = min(self.deadline, self.rpc_deadline)-time.monotonic()
            require(left > 0 and select.select([], [fd], [], left)[1], 'MCP write deadline')
            try:
                n = os.write(fd, data)
            except BlockingIOError:
                continue
            require(n > 0, 'MCP short write'); data = data[n:]
    def rpc(self, method, params):
        self.rpc_deadline = min(self.deadline, time.monotonic()+12)
        self.number += 1
        self.send({'jsonrpc':'2.0', 'id':self.number, 'method':method, 'params':params})
        while b'\n' not in self.buffer:
            left = self.rpc_deadline-time.monotonic()
            require(left > 0 and select.select([self.process.stdout], [], [], left)[0], 'MCP read deadline')
            block = os.read(self.process.stdout.fileno(), 65536)
            require(block, 'MCP EOF before response')
            self.buffer.extend(block)
            require(len(self.buffer) <= JSON_LIMIT, 'MCP response size')
        line, _, self.buffer = self.buffer.partition(b'\n')
        value = strict_json(line)
        require(isinstance(value, dict) and type(value.get('id')) is int and value['id'] == self.number, 'MCP response identity')
        return value


class LimitedOutput:
    def __init__(self, stream, limit):
        self.stream, self.limit, self.size = stream, limit, 0
    def write(self, block):
        require(self.size+len(block) <= self.limit, 'artifact exceeds collection bound')
        n = self.stream.write(block); self.size += n
        require(n == len(block), 'artifact short write')
        return n
    def flush(self): self.stream.flush()
    def fileno(self): return self.stream.fileno()


def close_client(client, timeout=45):
    result = {'confirmed':False, 'forced':False}
    if not hasattr(client, 'process'):
        return result
    child = client.process
    try:
        if not child.stdin.closed: child.stdin.close()
        deadline = time.monotonic()+timeout
        streams = [s for s in (child.stdout, child.stderr) if s and not s.closed]
        # Drain bounded chunks without retaining any response/token. A pending
        # response must not deadlock the child's EOF cleanup on a full stdout.
        while child.poll() is None and time.monotonic() < deadline:
            ready = select.select(streams, [], [], min(.05, max(0, deadline-time.monotonic())))[0]
            for stream in ready:
                if not os.read(stream.fileno(), 65536): streams.remove(stream)
        code = child.poll()
        if code is None:
            result['forced'] = True
            child.kill()
            child.wait(timeout=2)
        else:
            result['confirmed'] = code == 0
        result['exit_code'] = child.returncode
    except Exception:
        result['error'] = 'MCP close unconfirmed'
        if child.poll() is None:
            result['forced'] = True
            child.kill()
            try: child.wait(timeout=2)
            except subprocess.TimeoutExpired: result['owned_child_reap_failed'] = True
    finally:
        for stream in (child.stdin, child.stdout, child.stderr):
            if stream and not stream.closed: stream.close()
    return result


def producer_report_valid(doc, probe, files, stem):
    """Validate producer assertions separately; never infer GPU correctness."""
    require(type(doc.get('schema')) is int and doc['schema'] == 1 and type(doc.get('passed')) is bool, 'result schema')
    if doc['passed'] is not True: return False
    require(set(files) == allowed_names(stem), 'successful report requires complete artifact set')
    for name in ('import', 'render'):
        value = doc.get(name, {})
        require(value.get('passed') is True and value.get('owned_child_reaped') is True
                and value.get('forced_kill') is False and type(value.get('exit_code')) is int
                and value['exit_code'] == 0 and value.get('stdout_eof') is True
                and type(value.get('deadline_seconds')) is int and value['deadline_seconds'] == 120, 'producer process claim mismatch')
    require(isinstance(probe, dict) and type(probe.get('schema')) is int and probe['schema'] == 1 and probe.get('passed') is True
            and probe.get('error') == '' and probe.get('driver') == 'metal'
            and probe.get('method') == 'forward_plus', 'producer probe claim mismatch')
    require(isinstance(probe.get('adapter'), str) and probe['adapter'].startswith('Apple Paravirtual device'), 'Guest adapter claim mismatch')
    engine = probe.get('engine', {})
    require(tuple(engine.get(k) for k in ('major','minor','patch','status')) == (4,5,2,'stable'), 'engine claim mismatch')
    require([c.get('name') for c in probe.get('cases', [])] == list(CASES), 'probe cases')
    for case in probe['cases']:
        require(len(case.get('samples', [])) == 2, 'probe samples')
        for i, sample in enumerate(case['samples']):
            name = f'{case["name"]}-{i}.png'
            require(sample.get('file') == name and sample.get('width') == 1280 and sample.get('height') == 720
                    and sample.get('sha256') == files.get(stem+'-'+name, {}).get('sha256'), 'probe image binding')
    return True


def collect(args, client_factory=BoundedMCP):
    require(re.fullmatch('[a-z0-9][a-z0-9-]{0,47}', args.run_id), 'invalid run ID')
    require(re.fullmatch('[a-zA-Z0-9_-]{1,64}', args.id), 'invalid VM ID')
    require(30 <= args.timeout_seconds <= 300, 'collection timeout must be 30..300 seconds')
    path = args.output_dir.absolute()
    require(path.resolve(strict=True) == path, 'output directory must be canonical')
    directory = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    st = os.fstat(directory)
    if st.st_uid != os.getuid() or stat.S_IMODE(st.st_mode) != 0o700:
        os.close(directory)
        raise ValueError('output directory must be private')
    stem = 'godot-'+args.run_id
    outputs = {}
    def reserve(name):
        fd = os.open(name, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=directory)
        outputs[name] = os.fdopen(fd, 'w+b')
        return outputs[name]
    def data(name):
        stream = outputs[name]; stream.flush(); stream.seek(0); return stream.read(MAX_FILE+1)
    report = {'schema':1, 'run_id':args.run_id, 'vm_id':args.id, 'collection_integrity':False,
              'producer_render_report_valid':False, 'render_acceptance':False, 'files':[],
              'scope':'artifact integrity, independent host PNG decoding and MCP cleanup only; no new rendering or GPU acceptance'}
    client = None
    stage = 'reserve_outputs'
    try:
        result = reserve(stem+'-collection.json')
        export = reserve(stem+'-export.json')
        deadline = time.monotonic()+args.timeout_seconds
        stage = 'mcp_initialize'
        client = client_factory.__new__(client_factory)
        client_factory.__init__(client, args.binary.resolve(), args.state_dir.resolve(), deadline)
        stage = 'manifest_download'
        item = shared.download(client, args.id, stem+'-export.json', LimitedOutput(export,65536), deadline)
        report['files'].append(item)
        require(item['integrity_verified'], 'manifest download failed')
        stage = 'manifest_validation'
        listing = manifest(data(stem+'-export.json'), stem)
        report['fixture_manifest_sha256'] = listing['fixture_manifest_sha256']
        report['producer_reported_render_passed'] = listing['render_passed']
        stage = 'reserve_artifacts'
        for entry in listing['files']: reserve(entry['name'])
        docs, verified = {}, {}
        for entry in listing['files']:
            name = entry['name']; limit = JSON_LIMIT if name.endswith('.json') else MAX_FILE
            stage = 'artifact_download'
            item = shared.download(client,args.id,name,LimitedOutput(outputs[name],limit),deadline)
            report['files'].append(item)
            require(item['integrity_verified'] and item['bytes_written'] == entry['bytes'] and item['sha256'] == entry['sha256'], 'artifact manifest integrity mismatch')
            verified[name] = item
            stage = 'artifact_decode'
            if name.endswith('.json'): docs[name] = strict_json(data(name))
            if name.endswith('.png'): item['decoded'] = decode_png(data(name))
            require(time.monotonic() < deadline, 'collection deadline exceeded')
        stage = 'producer_report_validation'
        doc = docs[stem+'-result.json']
        require(isinstance(doc,dict), 'result JSON object required')
        if doc.get('passed') is True:
            for name in ('import.log','render.log'):
                text = data(stem+'-'+name).decode('utf-8',errors='replace')
                require(not any(line.lstrip().startswith(('ERROR:','SCRIPT ERROR:')) for line in text.splitlines()), 'producer log contains engine errors')
            require(doc.get('fixture_manifest_sha256') == listing['fixture_manifest_sha256'], 'fixture/result binding mismatch')
        report['producer_render_report_valid'] = producer_report_valid(doc,docs.get(stem+'-probe.json'),verified,stem)
        require(listing['render_passed'] is doc['passed'], 'manifest/result status mismatch')
        report['collection_integrity'] = True
    except Exception as error:
        # Never persist arbitrary exception/backend response text or transport tokens.
        report['failure_stage'] = stage
        report['error_type'] = type(error).__name__
        report['error'] = 'collection or artifact validation failed'
    finally:
        report['mcp_cleanup'] = close_client(client) if client is not None else {'confirmed':False,'forced':False}
        report['collection_integrity'] &= report['mcp_cleanup']['confirmed']
        if stem+'-collection.json' in outputs:
            outputs[stem+'-collection.json'].write((json.dumps(report,indent=2)+'\n').encode())
        for stream in outputs.values():
            try: stream.flush(); os.fsync(stream.fileno())
            finally: stream.close()
        os.close(directory)
    return report


def main():
    p=argparse.ArgumentParser(description=__doc__)
    for key in ('binary','state-dir','output-dir'): p.add_argument('--'+key,type=Path,required=True)
    p.add_argument('--id',required=True);p.add_argument('--run-id',required=True)
    p.add_argument('--timeout-seconds',type=int,default=180)
    args=p.parse_args();report=collect(args);print(json.dumps(report,indent=2))
    return 0 if report['collection_integrity'] else 1

if __name__=='__main__':raise SystemExit(main())
