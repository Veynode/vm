import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { spawnSync } from 'node:child_process';
import { AuthenticatedReceiver } from '../dist/native-auth.js';
import { DeviceIdentity, SessionAuthError, grantBytes, offerRejectionBytes, verifyOfferRejection, hex } from '../dist/session-auth.js';
const sdp = `v=0\r\na=fingerprint:sha-256 ${Array(32).fill('AA').join(':')}\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n`;
async function fixture(transform = async value => value) {
  const host = await DeviceIdentity.generate(), device = await DeviceIdentity.generate();
  const receiver = await AuthenticatedReceiver.fromIdentity(device, { pinnedHostPublicKey: host.publicKey, vmId: 'fixture-vm' });
  const grant = { host_id: host.id, device_id: device.id, device_public_key: device.publicKey, vm_id: 'fixture-vm', scopes: ['observe'], version: 1 };
  const signedGrant = { grant, signature: await host.sign(await grantBytes(grant)) }; await receiver.acceptGrant(signedGrant);
  let nonce = 0, latest; const calls = [];
  const fetcher = async (path, options) => {
    calls.push(path); const body = JSON.parse(options.body);
    if (path === '/auth/challenge') { const now = Math.floor(Date.now() / 1000); return Response.json({ host_id: host.id, device_id: device.id, vm_id: grant.vm_id, scopes: grant.scopes, grant_version: 1, instance_nonce: '11'.repeat(32), nonce: (++nonce).toString(16).padStart(64, '0'), issued_at: now, expires_at: now + 60 }); }
    assert.equal(path, '/auth/offer', 'Non-admission sends neither confirm nor close');
    const rejection = { intent: body.signed_offer.intent, code: 'guest_display_asleep', outcome: 'not_admitted', version: 1 };
    latest = { rejection, signature: await host.sign(offerRejectionBytes(rejection)) };
    return Response.json(await transform({ signed_rejection: structuredClone(latest) }, host));
  };
  return { receiver, host, device, signedGrant, fetcher, calls, latest: () => latest };
}
test('verified non-admission stays fenced until close, cleans local channel, returns fixed source failure, then permits explicit offer', async () => {
  const f = await fixture(), original = globalThis.fetch; globalThis.fetch = f.fetcher; let closed = 0;
  f.receiver.bindControlTransport(async () => assert.fail('no control'), () => closed++);
  try {
    await assert.rejects(f.receiver.offer(sdp, 'request-1', 'h265'), error => error instanceof SessionAuthError && /not admit/.test(error.message));
    assert.equal(f.receiver.offerTransmissionState, 'rejected'); assert.equal(f.receiver.controlConfirmed, false);
    await assert.rejects(f.receiver.confirm(), /No verified/); await assert.rejects(f.receiver.control({ kind: 'acquire' }), /confirmed/);
    await assert.rejects(f.receiver.signedClose(), /No verified/);
    await assert.rejects(f.receiver.acceptGrant(f.signedGrant), /uncertain/);
    await assert.rejects(f.receiver.offer(sdp, 'request-2', 'h264'), /uncertain/);
    await assert.rejects(f.receiver.iceConfiguration(), /no pending/);
    await assert.rejects(f.receiver.unpair(), /Close/); assert.throws(() => f.receiver.dispose(), /Close/);
    assert.throws(() => f.receiver.clearUnopenedControlTransport(), /admission/);
    assert.deepEqual(await f.receiver.close(), { code: 'guest_display_asleep' }); assert.equal(closed, 1);
    assert.equal(f.receiver.offerTransmissionState, 'not-sent'); assert.equal(await f.receiver.close(), null);
    assert.equal(closed, 1); await assert.rejects(f.receiver.control({ kind: 'acquire' }), /confirmed/);
    await assert.rejects(f.receiver.confirm(), /No verified/); await f.receiver.acceptGrant(f.signedGrant);
    await assert.rejects(f.receiver.offer(sdp, 'request-2', 'h264'), /not admit/);
    assert.deepEqual(await f.receiver.close(), { code: 'guest_display_asleep' }); f.receiver.dispose();
    assert.deepEqual(f.calls, ['/auth/challenge', '/auth/offer', '/auth/challenge', '/auth/offer']);
  } finally { globalThis.fetch = original; }
});
test('local channel close failure retains rejection and fences until explicit successful close', async () => {
  const f = await fixture(), original = globalThis.fetch; globalThis.fetch = f.fetcher; let fail = true;
  f.receiver.bindControlTransport(async () => assert.fail('no control'), () => { if (fail) throw Error('local close failed'); });
  try {
    await assert.rejects(f.receiver.offer(sdp, 'local-close', 'h265'));
    await assert.rejects(f.receiver.close(), /local close failed/); assert.equal(f.receiver.offerTransmissionState, 'rejected');
    await assert.rejects(f.receiver.offer(sdp, 'new', 'h264'), /uncertain/);
    fail = false; assert.deepEqual(await f.receiver.close(), { code: 'guest_display_asleep' }); f.receiver.dispose();
  } finally { globalThis.fetch = original; }
});
const mutations = {
  'plaintext error': () => ({ error: 'guest_display_asleep', closed: true }),
  'missing signature': value => { delete value.signed_rejection.signature; return value; },
  'forged signature': value => { value.signed_rejection.signature = '00'.repeat(64); return value; },
  'unknown code': value => { value.signed_rejection.rejection.code = 'everything_clean'; return value; },
  'wrong version': value => { value.signed_rejection.rejection.version = 2; return value; },
  'wrong outcome': value => { value.signed_rejection.rejection.outcome = 'admitted'; return value; },
  'extra rejection field': value => { value.signed_rejection.rejection.closed = true; return value; },
  'extra signed field': value => { value.signed_rejection.closed = true; return value; },
  'ambiguous answer and rejection': value => ({ ...value, type: 'answer' }),
  'null rejection': () => ({ signed_rejection: null }),
  'foreign signed intent': async (value, host) => { value.signed_rejection.rejection.intent.request_id = 'different'; value.signed_rejection.signature = await host.sign(offerRejectionBytes(value.signed_rejection.rejection)); return value; },
  'different codec signed intent': async (value, host) => { value.signed_rejection.rejection.intent.codec = 'h264'; value.signed_rejection.signature = await host.sign(offerRejectionBytes(value.signed_rejection.rejection)); return value; },
};
for (const [name, transform] of Object.entries(mutations)) test(`invalid rejection remains uncertain: ${name}`, async () => {
  const f = await fixture(transform), original = globalThis.fetch; globalThis.fetch = f.fetcher;
  try {
    await assert.rejects(f.receiver.offer(sdp, 'uncertain', 'h265'));
    assert.equal(f.receiver.offerTransmissionState, 'uncertain'); await assert.rejects(f.receiver.close(), /unconfirmed/);
    await assert.rejects(f.receiver.offer(sdp, 'retry', 'h264'), /uncertain/); await assert.rejects(f.receiver.acceptGrant(f.signedGrant), /uncertain/);
    await assert.rejects(f.receiver.confirm(), /No verified/); await assert.rejects(f.receiver.control({ kind: 'acquire' }), /confirmed/);
    assert.throws(() => f.receiver.dispose(), /Close/); assert.equal(f.calls.length, 2);
  } finally { globalThis.fetch = original; }
});
test('canonical rejection covers every intent field; genuine foreign signatures and stale different requests fail', async () => {
  const f = await fixture(), original = globalThis.fetch; globalThis.fetch = f.fetcher;
  try {
    await assert.rejects(f.receiver.offer(sdp, 'binding', 'h265'));
    const signed = f.latest(), expected = structuredClone(signed.rejection.intent);
    assert.deepEqual(await verifyOfferRejection(signed, f.host.publicKey, expected), signed.rejection);
    assert.deepEqual(await verifyOfferRejection(signed, f.host.publicKey, expected), signed.rejection, 'Exact duplicate verifies consistently');
    for (const mutate of [i => i.request_id = 'other', i => i.codec = 'h264', i => i.offer.sha256 = 'ab'.repeat(32), i => i.offer.fingerprint_sha256 = 'ab'.repeat(32), ...['host_id','device_id','instance_nonce','nonce'].map(key => i => i.challenge[key] = 'ab'.repeat(32)), i => i.challenge.vm_id = 'other', i => i.challenge.scopes = ['input','observe'], i => i.challenge.grant_version++, i => i.challenge.issued_at++, i => i.challenge.expires_at--]) {
      const changed = structuredClone(signed); mutate(changed.rejection.intent); changed.signature = await f.host.sign(offerRejectionBytes(changed.rejection));
      await assert.rejects(verifyOfferRejection(changed, f.host.publicKey, expected), /substituted/);
    }
    const foreign = await DeviceIdentity.generate();
    await assert.rejects(verifyOfferRejection({ ...signed, signature: await foreign.sign(offerRejectionBytes(signed.rejection)) }, f.host.publicKey, expected), /signature/);
    await assert.rejects(verifyOfferRejection(signed, foreign.publicKey, expected), /Pinned/);
    await f.receiver.close();
    globalThis.fetch = async (path, options) => path === '/auth/offer' ? Response.json({ signed_rejection: signed }) : f.fetcher(path, options);
    await assert.rejects(f.receiver.offer(sdp, 'binding', 'h265'), /substituted/, 'New challenge cannot reuse old receipt');
    assert.equal(f.receiver.offerTransmissionState, 'uncertain'); await assert.rejects(f.receiver.close(), /unconfirmed/);
  } finally { globalThis.fetch = original; }
});

test('Rust rejection golden canonical bytes and all fixed-code signatures verify in WebCrypto', async () => {
  const golden = JSON.parse(await readFile(new URL('../../crates/session-auth/tests/offer-rejection-golden.json', import.meta.url), 'utf8'));
  assert.equal(golden.cases.length, 3);
  for (const value of golden.cases) {
    assert.equal(hex(offerRejectionBytes(value.signed.rejection)), value.canonical_hex);
    assert.deepEqual(await verifyOfferRejection(value.signed, golden.host_public_key, golden.expected_intent), value.signed.rejection);
  }
});

test('WebCrypto-generated signed rejection verifies in real Rust CLI, substituted intent and forged signature fail', async () => {
  const binary = process.env.VEYNODE_PAIR_BIN ?? new URL('../../target/debug/vm-pair', import.meta.url).pathname;
  const f = await fixture(), original = globalThis.fetch; globalThis.fetch = f.fetcher;
  try {
    await assert.rejects(f.receiver.offer(sdp, 'rust-interop', 'h265'));
    const request = { kind: 'offer_rejection', host_public_key: f.host.publicKey, expected_intent: f.latest().rejection.intent, signed: f.latest() };
    const run = value => spawnSync(binary, ['verify'], { input: JSON.stringify(value), encoding: 'utf8', timeout: 5000 });
    const valid = run(request); assert.equal(valid.status, 0, valid.error?.message ?? valid.stderr);
    assert.equal(JSON.parse(valid.stdout).verified, true); assert.equal(JSON.parse(valid.stdout).authorization_checked, false);
    const changed = structuredClone(request); changed.expected_intent.request_id = 'other'; assert.notEqual(run(changed).status, 0);
    const forged = structuredClone(request); forged.signed.signature = '00'.repeat(64); assert.notEqual(run(forged).status, 0);
    await f.receiver.close(); f.receiver.dispose();
  } finally { globalThis.fetch = original; }
});
