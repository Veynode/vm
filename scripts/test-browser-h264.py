#!/usr/bin/env python3
"""Serve one explicit disposable desktop recording to an actual browser locally.

Open the printed localhost URL in the browser under test. This is not a remote
desktop server. No directory listing or arbitrary host file routes are exposed.
"""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import re
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--sample', type=Path, required=True)
parser.add_argument('--output', type=Path, default=ROOT / '.local/evidence/browser-h264.json')
args = parser.parse_args()
sample = args.sample.resolve()
if not sample.is_file() or not sample.is_relative_to(ROOT / '.local'):
    raise SystemExit('Use only an existing repository-created diagnostic recording under .local')
if sample.stat().st_size > 100 * 1024 * 1024:
    raise SystemExit('Diagnostic sample exceeds 100MiB cap')
page = (ROOT / 'tests/browser/h264.html').read_bytes()
done = threading.Event()
received = []


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def headers_for(self, status, kind, length):
        self.send_response(status)
        self.send_header('Content-Type', kind)
        self.send_header('Content-Length', str(length))
        self.send_header('Cache-Control', 'no-store')
        self.send_header('X-Content-Type-Options', 'nosniff')

    def do_GET(self):
        if self.path == '/':
            self.headers_for(200, 'text/html; charset=utf-8', len(page))
            self.end_headers()
            self.wfile.write(page)
        elif self.path == '/sample.mp4':
            total = sample.stat().st_size
            start, end, status = 0, total - 1, 200
            if self.headers.get('Range'):
                match = re.fullmatch(r'bytes=(\d+)-(\d*)', self.headers['Range'])
                if not match:
                    self.send_error(416)
                    return
                start = int(match[1])
                end = min(int(match[2]) if match[2] else end, total - 1)
                if not 0 <= start <= end < total:
                    self.send_error(416)
                    return
                status = 206
            self.headers_for(status, 'video/mp4', end - start + 1)
            self.send_header('Accept-Ranges', 'bytes')
            if status == 206:
                self.send_header('Content-Range', f'bytes {start}-{end}/{total}')
            self.end_headers()
            with sample.open('rb') as stream:
                stream.seek(start)
                remaining = end - start + 1
                while remaining:
                    chunk = stream.read(min(65536, remaining))
                    if not chunk:
                        break
                    self.wfile.write(chunk)
                    remaining -= len(chunk)
        else:
            self.send_error(404)

    def do_POST(self):
        if self.path != '/result' or self.headers.get('Origin') != origin:
            self.send_error(403)
            return
        try:
            length = int(self.headers.get('Content-Length', '0'))
            if not 0 < length <= 8192:
                raise ValueError('invalid length')
            result = json.loads(self.rfile.read(length))
            if not isinstance(result, dict) or not isinstance(result.get('passed'), bool):
                raise ValueError('invalid report')
        except (ValueError, TypeError):
            self.send_error(400)
            return
        received.append(result)
        self.headers_for(200, 'application/json', 2)
        self.end_headers()
        self.wfile.write(b'{}')
        done.set()


server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
server.daemon_threads = True
origin = f'http://127.0.0.1:{server.server_port}'
threading.Thread(target=server.serve_forever, daemon=True).start()
print(json.dumps({'open_browser_url': origin + '/', 'sample': str(sample)}), flush=True)
try:
    if not done.wait(120):
        raise TimeoutError('No actual browser decode result within 120 seconds')
    result = received[0]
    result.update(sample=str(sample), collected_at_unix=time.time())
    args.output.parent.mkdir(parents=True, exist_ok=True)
    output = args.output if result['passed'] else args.output.with_suffix('.failed.json')
    output.write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result), flush=True)
    if not result['passed']:
        raise SystemExit(1)
finally:
    server.shutdown()
    server.server_close()
