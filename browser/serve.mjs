import http from 'node:http';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
const root = new URL('./', import.meta.url);
const port = Number(process.env.PORT ?? 8765);
if (!Number.isInteger(port) || port < 0 || port > 65535) throw new Error('Invalid PORT');
const allowed = new Map([['/', ['index.html', 'text/html']], ['/probe.css', ['probe.css', 'text/css']], ...['probe', 'codec-policy', 'loopback'].map(name => [`/dist/${name}.js`, [`dist/${name}.js`, 'text/javascript']])]);
const server = http.createServer(async (req, res) => {
  const address = server.address();
  if (!['GET', 'HEAD'].includes(req.method ?? '') || ![`127.0.0.1:${address.port}`, `localhost:${address.port}`].includes(req.headers.host)) { res.writeHead(403).end(); return; }
  const route = allowed.get(req.url);
  if (!route) { res.writeHead(404).end(); return; }
  try {
    const body = await readFile(new URL(route[0], root));
    res.writeHead(200, { 'Content-Type': `${route[1]}; charset=utf-8`, 'Cache-Control': 'no-store', 'X-Content-Type-Options': 'nosniff', 'Content-Security-Policy': "default-src 'self'; script-src 'self'; style-src 'self'; media-src 'self' blob:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'", 'Referrer-Policy': 'no-referrer' });
    res.end(req.method === 'HEAD' ? undefined : body);
  } catch { res.writeHead(500).end('Build first: npm run build'); }
});
server.listen(port, '127.0.0.1', () => console.log(JSON.stringify({ url: `http://127.0.0.1:${server.address().port}/`, root: fileURLToPath(root), scope: 'local WebRTC protocol probe only' })));
