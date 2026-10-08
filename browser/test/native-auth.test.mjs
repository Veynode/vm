import test from 'node:test';
import assert from 'node:assert/strict';
import { AuthenticatedReceiver } from '../dist/native-auth.js';
import { DeviceIdentity, grantBytes, verifyOffer, answerBytes, sdpBinding, verifyConfirmation, verifyClose } from '../dist/session-auth.js';
const sdp = byte => `v=0\r\na=fingerprint:sha-256 ${Array(32).fill(byte).join(':')}\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n`;
async function setup(mode = 'normal') {
  const host = await DeviceIdentity.generate(), device = await DeviceIdentity.generate();
  const receiver = await AuthenticatedReceiver.fromIdentity(device, { pinnedHostPublicKey: host.publicKey, vmId: 'fixture-vm' });
  const grant = { host_id: host.id, device_id: device.id, device_public_key: device.publicKey, vm_id: 'fixture-vm', scopes: ['observe'], version: 1 };
  await receiver.acceptGrant({ grant, signature: await host.sign(await grantBytes(grant)) });
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
  return { receiver, host, device, fetcher, calls };
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
  try { await assert.rejects(f.receiver.offer(sdp('AA'), 'fixture-request', 'h264'), /SDP substituted/); await assert.rejects(f.receiver.confirm(), /No verified/); assert.deepEqual(f.calls, ['/auth/challenge', '/auth/offer']); f.receiver.dispose(); }
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

// Exercise the actual native.ts event handlers and production AuthenticatedReceiver.
// DOM/RTCPeerConnection and delayed HTTP transport are synthetic; signatures are
// real P-256. This tests cancellation ordering, not a real browser or VM.
for (const scenario of [{ name: 'successful confirmation', confirmFails: false }, { name: 'rejected confirmation', confirmFails: true }, { name: 'lost close response', retry: 'lost' }, { name: 'false retry receipt', retry: 'false' }, { name: 'lost answer stays fenced', offerFails: true }]) test(`native page cleanup: ${scenario.name}`, async () => {
  const { confirmFails, retry, offerFails } = scenario;
  const f = await setup(); const saved = new Map();
  const replace = (name, value) => { saved.set(name, Object.getOwnPropertyDescriptor(globalThis, name)); Object.defineProperty(globalThis, name, { value, writable: true, configurable: true }); };
  class Element {
    listeners = new Map(); disabled = false; hidden = false; value = ''; textContent = ''; style = {}; srcObject = null;
    addEventListener(type, callback) { const list = this.listeners.get(type) ?? []; list.push(callback); this.listeners.set(type, list); }
    click() { return Promise.all((this.listeners.get('click') ?? []).map(callback => callback())); }
    getContext() { return {}; }
    requestVideoFrameCallback() { return 1; }
    cancelVideoFrameCallback() {}
  }
  const elements = new Map(); const element = id => { if (!elements.has(id)) elements.set(id, new Element()); return elements.get(id); };
  const caps = [{ mimeType: 'video/h264', clockRate: 90000, sdpFmtpLine: 'profile-level-id=42e01f;packetization-mode=1' }];
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
      if (path === '/capabilities') return Response.json({ h264: 'verified-hardware', h265: 'unavailable', authentication: { required: true, vm_id: 'fixture-vm' } });
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
    disconnectTask = element('#disconnect').click();
    await new Promise(r => setTimeout(r, 10)); assert.equal(f.calls.filter(path => path === '/auth/close').length, 0);
    release(); await Promise.all([connectTask, disconnectTask]);
    assert.equal(f.calls.filter(path => path === '/auth/close').length, offerFails ? 0 : 1, 'Only a verified answer permits signed close');
    assert.equal(element('#result').textContent.includes('Signaling is pending'), false);
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
    f.receiver.dispose();
  } finally {
    release(); await Promise.allSettled([connectTask, disconnectTask].filter(Boolean)); AuthenticatedReceiver.open = oldOpen;
    for (const [name, descriptor] of saved) { if (descriptor) Object.defineProperty(globalThis, name, descriptor); else delete globalThis[name]; }
  }
});
