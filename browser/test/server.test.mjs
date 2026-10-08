import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { createInterface } from 'node:readline';
import http from 'node:http';

test('local probe serves only explicit assets and rejects DNS rebinding hosts', { timeout: 5000 }, async () => {
  const child = spawn(process.execPath, ['serve.mjs'], { cwd: new URL('../', import.meta.url), env: { ...process.env, PORT: '0' }, stdio: ['ignore', 'pipe', 'pipe'] });
  const exit = once(child, 'exit');
  const lines = createInterface({ input: child.stdout });
  try {
    const [line] = await once(lines, 'line');
    const { url } = JSON.parse(line);
    assert.equal((await fetch(url)).status, 200);
    const script = await fetch(new URL('dist/probe.js', url));
    assert.equal(script.status, 200); assert.match(script.headers.get('content-type'), /text\/javascript/);
    assert.equal((await fetch(new URL('package.json', url))).status, 404);
    assert.equal((await fetch(url, { method: 'POST', body: 'not an evidence endpoint' })).status, 403);
    const badHost = await new Promise((resolve, reject) => {
      const req = http.get(url, { headers: { Host: 'attacker.example' } }, res => { res.resume(); resolve(res.statusCode); }); req.on('error', reject);
    });
    assert.equal(badHost, 403);
  } finally { lines.close(); child.kill('SIGTERM'); await exit; }
});
