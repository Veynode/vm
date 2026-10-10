import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { AuthenticatedReceiver } from '../dist/native-auth.js';
import { DeviceIdentity, grantBytes, verifyGrant, verifyOffer, answerBytes, sdpBinding, verifyConfirmation, verifyClose, offerRejectionBytes } from '../dist/session-auth.js';
const sdp = byte => `v=0\r\na=fingerprint:sha-256 ${Array(32).fill(byte).join(':')}\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n`;
async function setup(mode = 'normal') {
  const host = await DeviceIdentity.generate(), device = await DeviceIdentity.generate();
  const receiver = await AuthenticatedReceiver.fromIdentity(device, { pinnedHostPublicKey: host.publicKey, vmId: 'fixture-vm' });
  const grant = { host_id: host.id, device_id: device.id, device_public_key: device.publicKey, vm_id: 'fixture-vm', scopes: ['observe'], version: 1 };
  const signedGrant = { grant, signature: await host.sign(await grantBytes(grant)) };
  await receiver.acceptGrant(signedGrant);
  const now = Math.floor(Date.now() / 1000);
  const challenge = { host_id: host.id, device_id: device.id, vm_id: grant.vm_id, scopes: ['observe'], grant_version: 1, instance_nonce: '11'.repeat(32), nonce: '22'.repeat(32), issued_at: now, expires_at: now + 60 };
  let transcript, closeCount = 0; const calls = [];
  const fetcher = async (path, options) => {
    calls.push(path); const body = JSON.parse(options.body); assert.equal(options.redirect, 'error'); assert.equal(options.credentials, 'omit');
    if (path === '/auth/challenge') return Response.json(challenge);
    if (path === '/auth/offer') {
      await verifyOffer(body.signed_offer, grant, challenge, body.sdp, now);
      transcript = { intent: body.signed_offer.intent, session_id: 'fixture-session', answer: await sdpBinding(sdp('BB')) };
      const signed_answer = { transcript, signature: await host.sign(answerBytes(transcript)) };
      return Response.json({ type: 'answer', sdp: sdp(mode === 'substitute' ? 'CC' : 'BB'), signed_answer });
    }
    if (path === '/auth/confirm') { await verifyConfirmation(body.confirmation, transcript, device.publicKey, now); return Response.json({ confirmed: true }); }
    if (path === '/auth/close') { await verifyClose(body, transcript, device.publicKey); closeCount++; return Response.json(mode === 'cleanup-failure' && closeCount === 1 ? { closed: false } : { closed: true }); }
    throw new Error('Unexpected endpoint');
  };
  return { receiver, host, device, fetcher, calls, signedGrant };
}
test('receiver verifies answer before caller can install it; confirms and signs scoped close', async () => {
  const f = await setup(), original = globalThis.fetch; globalThis.fetch = f.fetcher;
  try {
    const pair = await f.receiver.pairingRequest(); assert.equal(pair.pairing.host_public_key, f.host.publicKey); assert.deepEqual(pair.pairing.scopes, ['observe']);
    const result = await f.receiver.offer(sdp('AA'), 'fixture-request', 'h264'); assert.equal(result.answer.sdp, sdp('BB'));
    await assert.rejects(f.receiver.offer(sdp('AA'), 'another', 'h264'), /active session/);
    // Returned mutable object must not change the internal close transcript.
    result.transcript.session_id = 'substituted';
    await f.receiver.confirm(); await f.receiver.close(); f.receiver.dispose();
    assert.deepEqual(f.calls, ['/auth/challenge', '/auth/offer', '/auth/confirm', '/auth/close']);
  } finally { globalThis.fetch = original; }
});
test('receiver rejects substituted answer and sends no confirmation', async () => {
  const f = await setup('substitute'), original = globalThis.fetch; globalThis.fetch = f.fetcher;
  try { await assert.rejects(f.receiver.offer(sdp('AA'), 'fixture-request', 'h264'), /SDP substituted/); await assert.rejects(f.receiver.confirm(), /No verified/); assert.deepEqual(f.calls, ['/auth/challenge', '/auth/offer']); assert.throws(() => f.receiver.dispose(), /Close the session/); }
  finally { globalThis.fetch = original; }
});
test('receiver retains session on missing cleanup attestation, rejects broader or foreign grant', async () => {
  const f = await setup('cleanup-failure'), original = globalThis.fetch; globalThis.fetch = f.fetcher;
  try {
    const grant = { host_id: f.host.id, device_id: f.device.id, device_public_key: f.device.publicKey, vm_id: 'fixture-vm', scopes: ['input', 'observe'], version: 1 };
    await assert.rejects(f.receiver.acceptGrant({ grant, signature: await f.host.sign(await grantBytes(grant)) }), /pairing grant/);
    await f.receiver.offer(sdp('AA'), 'fixture-request', 'h264'); await assert.rejects(f.receiver.close(), /not acknowledged/);
    assert.throws(() => f.receiver.dispose(), /Close the session/); await f.receiver.close(); f.receiver.dispose();
  } finally { globalThis.fetch = original; }
});

// These cases distinguish local pre-transmission failure from an offer whose
// delivery/answer is uncertain. Host signatures are real; HTTP is controlled.
for (const mode of ['challenge-denied', 'invalid-challenge', 'invalid-sdp', 'already-aborted', 'abort-during-signing', 'signing-failed']) test(`safe local cleanup before offer transmission: ${mode}`, async () => {
  const f = await setup(), original = globalThis.fetch, controller = new AbortController();
  let channelClosed = 0, offerCalls = 0;
  f.receiver.bindControlTransport(async () => { throw Error('unused'); }, () => channelClosed++);
  globalThis.fetch = async (path, options) => {
    if (path === '/auth/offer') offerCalls++;
    if (path === '/auth/challenge' && mode === 'challenge-denied') return Response.json({ error: 'denied' }, { status: 403 });
    const response = await f.fetcher(path, options);
    if (path === '/auth/challenge' && mode === 'invalid-challenge') { const challenge = await response.json(); challenge.vm_id = 'substituted'; return Response.json(challenge); }
    return response;
  };
  const sign = f.device.sign;
  if (mode === 'abort-during-signing') f.device.sign = async function (...args) { const value = await sign.apply(this, args); controller.abort(); return value; };
  if (mode === 'signing-failed') f.device.sign = async () => { throw Error('signing failed'); };
  if (mode === 'already-aborted') controller.abort();
  try {
    assert.equal(f.receiver.offerTransmissionState, 'not-sent');
    await assert.rejects(f.receiver.offer(mode === 'invalid-sdp' ? 'invalid SDP' : sdp('AA'), 'fixture-request', 'h264', controller.signal));
    assert.equal(offerCalls, 0); assert.equal(f.receiver.offerTransmissionState, 'not-sent');
    await f.receiver.close(); assert.equal(channelClosed, 1); assert.equal(f.calls.includes('/auth/close'), false);
    f.receiver.clearUnopenedControlTransport();
    f.device.sign = sign; globalThis.fetch = f.fetcher;
    await f.receiver.acceptGrant(f.signedGrant); await f.receiver.pairingRequest();
    f.receiver.bindControlTransport(async () => { throw Error('unused'); });
    await f.receiver.offer(sdp('AA'), 'retry-request', 'h264');
    assert.equal(f.receiver.offerTransmissionState, 'verified'); await f.receiver.close();
    assert.equal(f.receiver.offerTransmissionState, 'not-sent');
  } finally { f.device.sign = sign; f.receiver.dispose(); globalThis.fetch = original; }
});
for (const mode of ['lost-answer', 'tampered-answer', 'offer-denied']) test(`transmitted offer cannot claim local cleanup or replace authority: ${mode}`, async () => {
  const f = await setup(mode === 'tampered-answer' ? 'substitute' : 'normal'), original = globalThis.fetch;
  globalThis.fetch = async (path, options) => {
    const response = await f.fetcher(path, options);
    if (path === '/auth/offer' && mode === 'lost-answer') throw Error('answer lost after host acceptance');
    if (path === '/auth/offer' && mode === 'offer-denied') return Response.json({ error: 'denied' }, { status: 403 });
    return response;
  };
  try {
    await assert.rejects(f.receiver.offer(sdp('AA'), 'fixture-request', 'h264'));
    assert.equal(f.receiver.offerTransmissionState, 'uncertain');
    await assert.rejects(f.receiver.close(), /cleanup is unconfirmed/);
    assert.throws(() => f.receiver.clearUnopenedControlTransport(), /admission/);
    await assert.rejects(f.receiver.acceptGrant(f.signedGrant), /uncertain session/);
    await assert.rejects(f.receiver.offer(sdp('AA'), 'retry', 'h264'), /uncertain/);
    await assert.rejects(f.receiver.unpair(), /Close the session/);
    assert.equal(f.calls.filter(path => path === '/auth/offer').length, 1);
    assert.equal(f.calls.includes('/auth/close'), false);
  } finally { assert.throws(() => f.receiver.dispose(), /Close the session/); globalThis.fetch = original; }
});
test('pending challenge cannot be cleared while signaling is still busy', async () => {
  const f = await setup(), original = globalThis.fetch; let release, entered;
  const reached = new Promise(resolve => { entered = resolve; }), blocked = new Promise(resolve => { release = resolve; });
  globalThis.fetch = async () => { entered(); await blocked; return Response.json({}, { status: 403 }); };
  try {
    const offered = f.receiver.offer(sdp('AA'), 'pending-request', 'h264'); await reached;
    assert.throws(() => f.receiver.clearUnopenedControlTransport(), /admission/);
    await assert.rejects(f.receiver.close(), /pending/); await assert.rejects(f.receiver.acceptGrant(f.signedGrant), /session/);
    release(); await assert.rejects(offered, /403/); await f.receiver.close();
    assert.equal(f.receiver.offerTransmissionState, 'not-sent');
  } finally { release(); f.receiver.dispose(); globalThis.fetch = original; }
});

// Exercise the actual native.ts event handlers and production AuthenticatedReceiver.
// DOM/RTCPeerConnection and delayed HTTP transport are synthetic; signatures are
// real P-256. This tests cancellation ordering, not a real browser or VM.
for (const scenario of [{ name: 'successful confirmation', confirmFails: false }, { name: 'rejected confirmation', confirmFails: true }, { name: 'lost close response', retry: 'lost' }, { name: 'false retry receipt', retry: 'false' }, { name: 'lost answer stays fenced', offerFails: true }, { name: 'signed source rejection cleans locally without codec fallback', offerRejected: true }]) test(`native page cleanup: ${scenario.name}`, async () => {
  const { confirmFails, retry, offerFails, offerRejected } = scenario;
  const f = await setup(); const saved = new Map();
  const replace = (name, value) => { saved.set(name, Object.getOwnPropertyDescriptor(globalThis, name)); Object.defineProperty(globalThis, name, { value, writable: true, configurable: true }); };
  class Element {
    listeners = new Map(); disabled = false; hidden = false; value = ''; textContent = ''; style = {}; srcObject = null;
    addEventListener(type, callback) { const list = this.listeners.get(type) ?? []; list.push(callback); this.listeners.set(type, list); }
    click() { return Promise.all((this.listeners.get('click') ?? []).map(callback => callback())); }
    setAttribute() {}
    getBoundingClientRect() { return { left: 0, top: 0, right: 640, bottom: 360, width: 640, height: 360 }; }
    getContext() { return {}; }
    requestVideoFrameCallback() { return 1; }
    cancelVideoFrameCallback() {}
  }
  const elements = new Map(); const element = id => { if (!elements.has(id)) elements.set(id, new Element()); return elements.get(id); };
  const caps = [{ mimeType: 'video/h264', clockRate: 90000, sdpFmtpLine: 'profile-level-id=42e01f;packetization-mode=1' }];
  if (offerRejected) caps.unshift({ mimeType: 'video/h265', clockRate: 90000, sdpFmtpLine: 'profile-id=1;tier-flag=0;level-id=120' });
  class Peer extends EventTarget {
    iceGatheringState = 'complete'; connectionState = 'new'; localDescription = null;
    addTransceiver() { return { setCodecPreferences() {} }; }
    async createOffer() { return { type: 'offer', sdp: sdp('AA') }; }
    async setLocalDescription(value) { this.localDescription = value; }
    async setRemoteDescription() {}
    close() { this.connectionState = 'closed'; }
  }
  let release, reached;
  const waiting = new Promise(resolve => { reached = resolve; });
  const blocked = new Promise(resolve => { release = resolve; });
  const oldOpen = AuthenticatedReceiver.open;
  const poll = async predicate => { for (let i = 0; i < 100; i++) { if (predicate()) return; await new Promise(r => setTimeout(r, 5)); } throw new Error('UI did not settle'); };
  let connectTask, disconnectTask; const closeBodies = [];
  try {
    replace('document', { querySelector: element, createElement: () => new Element() }); replace('window', new EventTarget());
    replace('MediaStream', class {}); replace('RTCPeerConnection', Peer); replace('RTCRtpReceiver', { getCapabilities: () => ({ codecs: caps }) });
    replace('fetch', async (path, options) => {
      if (path === '/capabilities') return Response.json({ h264: 'verified-hardware', h265: offerRejected ? 'verified-hardware' : 'unavailable', authentication: { required: true, vm_id: 'fixture-vm' } });
      if (path === '/auth/offer' && offerRejected) {
        await f.fetcher(path, options); reached(); await blocked;
        const rejection = { intent: JSON.parse(options.body).signed_offer.intent, code: 'guest_display_inactive', outcome: 'not_admitted', version: 1 };
        assert.equal(rejection.intent.codec, 'h265');
        return Response.json({ signed_rejection: { rejection, signature: await f.host.sign(offerRejectionBytes(rejection)) } });
      }
      if (path === '/auth/offer' && offerFails) { await f.fetcher(path, options); reached(); throw new TypeError('Synthetic lost signed answer'); }
      if (path === '/auth/confirm') { reached(); await blocked; if (confirmFails) throw new TypeError('Synthetic lost confirmation response'); }
      if (path === '/auth/close' && retry) {
        closeBodies.push(JSON.parse(options.body)); const receipt = await f.fetcher(path, options);
        if (closeBodies.length === 1) throw new TypeError('Synthetic response lost after server accepted close');
        if (closeBodies.length === 2 && retry === 'false') return Response.json({ closed: false });
        return receipt;
      }
      return f.fetcher(path, options);
    });
    AuthenticatedReceiver.open = async () => f.receiver;
    element('#host-pin').value = f.host.publicKey;
    const grant = { host_id: f.host.id, device_id: f.device.id, device_public_key: f.device.publicKey, vm_id: 'fixture-vm', scopes: ['observe'], version: 1 };
    element('#signed-grant').value = JSON.stringify({ grant, signature: await f.host.sign(await grantBytes(grant)) });
    await import(`../dist/native.js?cleanup-case=${encodeURIComponent(scenario.name)}`);
    await poll(() => element('#status').textContent.includes('verified pairing'));
    await element('#prepare-pairing').click(); await poll(() => element('#pairing-request').textContent.includes('digest'));
    await element('#accept-grant').click(); await poll(() => element('#pairing-status').textContent.includes('Ready'));
    connectTask = element('#connect').click(); await waiting;
    if (!offerRejected) disconnectTask = element('#disconnect').click();
    await new Promise(r => setTimeout(r, 10)); assert.equal(f.calls.filter(path => path === '/auth/close').length, 0);
    release(); await Promise.all([connectTask, disconnectTask]);
    assert.equal(f.calls.filter(path => path === '/auth/close').length, offerFails || offerRejected ? 0 : 1, 'Only a verified answer permits signed close');
    assert.equal(element('#result').textContent.includes('Signaling is pending'), false);
    if (offerRejected) {
      assert.match(element('#result').textContent, /signed-offer-rejection/);
      assert.match(element('#result').textContent, /guest_display_inactive/);
      assert.equal(f.calls.filter(path => path === '/auth/offer').length, 1, 'Source rejection must not fall back to H264');
      assert.equal(f.calls.includes('/auth/confirm'), false);
      assert.equal(element('#connect').disabled, false);
      assert.equal(element('#retry-cleanup').hidden, true);
      assert.equal(f.receiver.offerTransmissionState, 'not-sent');
    }
    if (retry || offerFails) {
      assert.equal(element('#connect').disabled, true); assert.equal(element('#retry-cleanup').hidden, false);
      assert.equal(element('#retry-cleanup').disabled, false);
      const offers = f.calls.filter(path => path === '/auth/offer').length;
      await element('#connect').click(); assert.equal(f.calls.filter(path => path === '/auth/offer').length, offers, 'Programmatic Connect also respects fence');
      await element('#retry-cleanup').click();
      if (offerFails) {
        assert.equal(element('#connect').disabled, true); assert.equal(f.calls.filter(path => path === '/auth/close').length, 0);
        assert.equal(element('#retry-cleanup').hidden, false); assert.match(element('#status').textContent, /still unconfirmed/);
      } else {
        assert.equal(closeBodies.length, 2);
        assert.equal(closeBodies[0].session_id, closeBodies[1].session_id);
        assert.equal(closeBodies[0].transcript_sha256, closeBodies[1].transcript_sha256);
        if (retry === 'false') {
          assert.equal(element('#connect').disabled, true); assert.equal(element('#retry-cleanup').hidden, false);
          assert.equal(element('#retry-cleanup').disabled, false); assert.match(element('#status').textContent, /still unconfirmed/);
          await element('#connect').click(); assert.equal(f.calls.filter(path => path === '/auth/offer').length, offers);
          await element('#retry-cleanup').click(); assert.equal(closeBodies.length, 3);
          assert.equal(closeBodies[0].session_id, closeBodies[2].session_id);
          assert.equal(closeBodies[0].transcript_sha256, closeBodies[2].transcript_sha256);
        }
        assert.equal(element('#connect').disabled, false); assert.equal(element('#retry-cleanup').hidden, true);
        assert.match(element('#status').textContent, /cleanup acknowledged/);
        assert.equal(f.calls.filter(path => path === '/auth/offer').length, offers, 'Retry never creates another offer');
        assert.match(element('#result').textContent, /Synthetic response lost/, 'Historical cleanup error remains in evidence');
      }
    }
    if (offerFails) assert.throws(() => f.receiver.dispose(), /Close the session/); else f.receiver.dispose();
  } finally {
    release(); await Promise.allSettled([connectTask, disconnectTask].filter(Boolean)); AuthenticatedReceiver.open = oldOpen;
    for (const [name, descriptor] of saved) { if (descriptor) Object.defineProperty(globalThis, name, descriptor); else delete globalThis[name]; }
  }
});

test('source failure is returned only after cleanup ACK; false receipt keeps the same signed-close binding', async () => {
  const f = await setup(), original = globalThis.fetch, bodies = []; let clean = false;
  globalThis.fetch = async (path, options) => {
    const result = await f.fetcher(path, options);
    if (path !== '/auth/close') return result;
    bodies.push(JSON.parse(options.body));
    return Response.json({ closed: clean, media_failure: { code: 'guest_display_asleep' } });
  };
  try {
    await f.receiver.offer(sdp('AA'), 'fixture-source-request', 'h264'); await f.receiver.confirm();
    await assert.rejects(f.receiver.close(), /not acknowledged/);
    assert.throws(() => f.receiver.dispose(), /Close the session/);
    clean = true; assert.deepEqual(await f.receiver.close(), { code: 'guest_display_asleep' });
    assert.equal(bodies[0].session_id, bodies[1].session_id); assert.equal(bodies[0].transcript_sha256, bodies[1].transcript_sha256);
    f.receiver.dispose();
  } finally { globalThis.fetch = original; }
});

// Minimal in-memory IndexedDB covering only the calls BrowserIdentityStore makes.
function installFakeIndexedDB() {
  const rows = new Map();
  const transaction = () => {
    const queue = []; const tx = { oncomplete: null, onabort: null, onerror: null, aborted: false, abort() { tx.aborted = true; }, objectStore: () => store };
    const request = operation => { const pending = { result: undefined, onsuccess: null }; queue.push(() => { pending.result = operation(); pending.onsuccess?.(); }); return pending; };
    const store = { get: key => request(() => rows.get(key)), add: (value, key) => request(() => { rows.set(key, value); }), delete: key => request(() => { rows.delete(key); }) };
    setTimeout(() => { while (queue.length && !tx.aborted) queue.shift()(); if (tx.aborted) tx.onabort?.(); else tx.oncomplete?.(); }, 0);
    return tx;
  };
  const database = { close() {}, createObjectStore() { return {}; }, transaction };
  const fake = { open() { const request = { result: undefined, onupgradeneeded: null, onsuccess: null, onerror: null, onblocked: null }; setTimeout(() => { request.result = database; request.onupgradeneeded?.(); request.onsuccess?.(); }, 0); return request; } };
  const original = Object.getOwnPropertyDescriptor(globalThis, 'indexedDB');
  Object.defineProperty(globalThis, 'indexedDB', { value: fake, configurable: true, writable: true });
  return { rows, restore() { if (original) Object.defineProperty(globalThis, 'indexedDB', original); else delete globalThis.indexedDB; } };
}
// Pins current behaviour: local unpair removes only the browser identity record. It
// makes no host request, so the host-side grant stays active until the operator revokes it.
test('local unpair removes only this browser identity, never contacts the host, and leaves the host grant unrevoked', async () => {
  const fake = installFakeIndexedDB(), original = globalThis.fetch; let networkCalls = 0;
  globalThis.fetch = async () => { networkCalls++; throw new Error('unexpected network call'); };
  try {
    const host = await DeviceIdentity.generate();
    const receiver = await AuthenticatedReceiver.open({ pinnedHostPublicKey: host.publicKey, vmId: 'fixture-vm', databaseName: 'veynode-session-auth-unpair-test' });
    const grant = { host_id: host.id, device_id: receiver.deviceId, device_public_key: receiver.publicKey, vm_id: 'fixture-vm', scopes: ['observe'], version: 1 };
    const signedGrant = { grant, signature: await host.sign(await grantBytes(grant)) };
    await receiver.acceptGrant(signedGrant);
    assert.equal(fake.rows.size, 1, 'identity record exists before unpair');
    const identity = receiver.identity;
    await receiver.unpair();
    assert.equal(networkCalls, 0, 'local unpair must not contact the host');
    assert.equal(fake.rows.size, 0, 'only this browser identity record is removed');
    await assert.rejects(identity.sign(new Uint8Array([1])), /unpaired/);
    // Nothing in the browser revokes the host-side grant: the host-signed artifact still verifies.
    await verifyGrant(signedGrant, host.publicKey, { device_public_key: grant.device_public_key, vm_id: 'fixture-vm', scopes: ['observe'] });
  } finally { fake.restore(); globalThis.fetch = original; }
});
// Pins current behaviour: the native page has no unpair control and no operator revoke text.
// The `vm-pair revoke` instruction exists only in crates/session-auth/README.md.
test('native page exposes no unpair control and no operator revoke instruction', () => {
  const html = readFileSync(new URL('../native.html', import.meta.url), 'utf8');
  assert.doesNotMatch(html, /vm-pair revoke/);
  assert.doesNotMatch(html, /unpair/i);
});
