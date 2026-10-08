import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:net';
import { request } from 'node:http';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

// Real loopback HTTP and Rust verifier, no fake cryptographic implementation.
test('synthetic fixture server restricts host/origin/files and invokes bounded Rust verification', async () => {
  const reservation = createServer(); await new Promise(resolve => reservation.listen(0, '127.0.0.1', resolve));
  const port = reservation.address().port; await new Promise(resolve => reservation.close(resolve));
  const child = spawn(process.execPath, [fileURLToPath(new URL('../session-auth-fixture-server.mjs', import.meta.url))], { env: { ...process.env, PORT: String(port) }, stdio: ['ignore', 'pipe', 'pipe'] });
  const exited = new Promise(resolve => child.once('exit', resolve)); let stderr = ''; child.stderr.on('data', x => { stderr += x; });
  try {
    await new Promise((resolve, reject) => { const deadline = setTimeout(() => reject(new Error('Server readiness timeout: ' + stderr)), 5000); child.stdout.once('data', () => { clearTimeout(deadline); resolve(); }); child.once('error', reject); child.once('exit', () => { clearTimeout(deadline); reject(new Error('Server exited: ' + stderr)); }); });
    const base = `http://127.0.0.1:${port}`;
    const get = await fetch(base); assert.equal(get.status, 200); assert.match(get.headers.get('content-security-policy'), /default-src 'none'/);
    assert.equal(await new Promise((resolve, reject) => { const req = request(base + '/golden.json', { headers: { Host: 'attacker.invalid' } }, res => { res.resume(); resolve(res.statusCode); }); req.on('error', reject); req.end(); }), 403);
    assert.equal((await fetch(base + '/src/session-auth.ts')).status, 404);
    const golden = JSON.parse(await readFile(new URL('../../crates/session-auth/tests/golden.json', import.meta.url), 'utf8'));
    const payload = { kind: 'grant', host_public_key: golden.host_public_key, signed: golden.signed_grant };
    const post = (body, origin = base) => fetch(base + '/verify', { method: 'POST', headers: { 'Content-Type': 'application/json', Origin: origin }, body: typeof body === 'string' ? body : JSON.stringify(body) });
    assert.equal((await post(payload, 'https://attacker.invalid')).status, 403);
    const valid = await post(payload); assert.equal(valid.status, 200, await valid.clone().text()); assert.deepEqual(await valid.json(), { verified: true, authorization_checked: false });
    const changed = structuredClone(payload); changed.signed.grant.vm_id = 'other-vm'; assert.equal((await post(changed)).status, 400);
    assert.equal((await post('{invalid')).status, 400);
    assert.equal((await post(' '.repeat(512 * 1024 + 1))).status, 413);
    assert.equal((await fetch(base)).status, 200);
  } finally { child.kill('SIGTERM'); await exited; }
});
