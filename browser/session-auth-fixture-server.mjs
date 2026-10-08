// Loopback synthetic-crypto fixture only. No signing endpoint, Keychain action,
// WebRTC, VM or arbitrary file/command access. Rust verify receives bounded stdin.
import http from 'node:http';
import { readFile } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
const root = new URL('./', import.meta.url);
const binary = process.env.VEYNODE_PAIR_BIN ?? fileURLToPath(new URL('../target/debug/vm-pair', root));
const port = Number(process.env.PORT ?? 8768);
if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('Invalid PORT');
const origin = `http://127.0.0.1:${port}`;
const assets = new Map([['/', ['session-auth-fixture.html', 'text/html']], ['/dist/session-auth.js', ['dist/session-auth.js', 'text/javascript']], ['/dist/session-auth-fixture.js', ['dist/session-auth-fixture.js', 'text/javascript']], ['/golden.json', ['../crates/session-auth/tests/golden.json', 'application/json']]]);
let active = 0;
const server = http.createServer(async (req, res) => {
  res.setHeader('Cache-Control', 'no-store'); res.setHeader('X-Content-Type-Options', 'nosniff'); res.setHeader('Referrer-Policy', 'no-referrer');
  res.setHeader('Content-Security-Policy', "default-src 'none'; script-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'");
  if (req.headers.host !== `127.0.0.1:${port}`) { res.writeHead(403).end(); return; }
  if (req.method === 'POST' && req.url === '/verify') {
    if (req.headers.origin !== origin || req.headers['content-type'] !== 'application/json') { res.writeHead(403).end(); return; }
    if (active >= 4) { res.writeHead(429).end(); return; }
    active++; let child;
    try {
      let size = 0; const chunks = [];
      for await (const chunk of req) { size += chunk.length; if (size > 512 * 1024) { res.writeHead(413).end(); return; } chunks.push(chunk); }
      const body = Buffer.concat(chunks); JSON.parse(body.toString('utf8'));
      const result = await new Promise((resolve, reject) => {
        child = spawn(binary, ['verify'], { stdio: ['pipe', 'pipe', 'pipe'] });
        const out = [], err = []; let bytes = 0;
        const timer = setTimeout(() => { child.kill('SIGKILL'); reject(new Error('Rust verification deadline')); }, 5000);
        child.on('error', error => { clearTimeout(timer); reject(error); });
        for (const [pipe, destination] of [[child.stdout, out], [child.stderr, err]]) pipe.on('data', chunk => { bytes += chunk.length; if (bytes > 16384) { child.kill('SIGKILL'); reject(new Error('Rust output exceeded fixture bound')); } else destination.push(chunk); });
        child.on('close', code => { clearTimeout(timer); if (code !== 0) reject(new Error(Buffer.concat(err).toString('utf8').slice(0, 1024))); else { try { resolve(JSON.parse(Buffer.concat(out).toString('utf8'))); } catch { reject(new Error('Malformed Rust verification response')); } } });
        child.stdin.on('error', () => {}); child.stdin.end(body);
      });
      res.writeHead(200, { 'Content-Type': 'application/json' }).end(JSON.stringify(result));
    } catch (error) { if (!res.headersSent) res.writeHead(400, { 'Content-Type': 'application/json' }).end(JSON.stringify({ error: String(error) })); }
    finally { active--; if (child && child.exitCode === null) child.kill('SIGKILL'); }
    return;
  }
  if (!['GET', 'HEAD'].includes(req.method ?? '') || !assets.has(req.url)) { res.writeHead(404).end(); return; }
  const [file, type] = assets.get(req.url);
  try { const body = await readFile(new URL(file, root)); res.writeHead(200, { 'Content-Type': `${type}; charset=utf-8` }).end(req.method === 'HEAD' ? undefined : body); }
  catch { res.writeHead(500).end('Build browser assets and Rust verification CLI first.'); }
});
server.requestTimeout = 10000; server.headersTimeout = 10000;
server.listen(port, '127.0.0.1', () => console.log(JSON.stringify({ url: `${origin}/`, scope: 'synthetic session-auth/IndexedDB fixture; no VM or external network', database: 'veynode-session-auth-fixture-v1' })));
