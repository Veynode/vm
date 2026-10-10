import test from 'node:test';
import assert from 'node:assert/strict';
import { selectedIcePath } from '../dist/ice-config.js';
function stats(localType = 'relay', remoteType = 'host') {
  return new Map([
    ['transport', { type: 'transport', dtlsState: 'connected', selectedCandidatePairId: 'pair' }],
    ['pair', { type: 'candidate-pair', state: 'succeeded', nominated: true, localCandidateId: 'local', remoteCandidateId: 'remote', currentRoundTripTime: .025 }],
    ['local', { type: 'local-candidate', candidateType: localType, protocol: 'udp', relayProtocol: 'tls', address: 'private-host', port: 44444, url: 'turn:secret-server', usernameFragment: 'secret-ufrag', credential: 'secret-password' }],
    ['remote', { type: 'remote-candidate', candidateType: remoteType, protocol: 'udp', address: 'private-remote' }],
  ]);
}
test('selected ICE path proves active pair and reports only an allowlist without credentials or addresses', () => {
  assert.deepEqual(selectedIcePath(stats()), { localCandidateType: 'relay', remoteCandidateType: 'host', protocol: 'udp', relayProtocol: 'tls', roundTripTimeSeconds: .025, relayed: true });
  assert.deepEqual(selectedIcePath(stats('host', 'host')), { localCandidateType: 'host', remoteCandidateType: 'host', protocol: 'udp', roundTripTimeSeconds: .025, relayed: false });
  assert.equal(selectedIcePath(stats('host', 'relay')).relayed, true);
  assert.doesNotMatch(JSON.stringify(selectedIcePath(stats())), /private|secret|address|credential|port|url|username/i);
});
test('policy, nominated stale pairs, missing reports and ambiguous transports never establish actual relay', () => {
  for (const change of [s => s.delete('transport'), s => s.get('transport').dtlsState = 'connecting',
    s => s.get('transport').selectedCandidatePairId = 'missing', s => s.get('pair').state = 'in-progress',
    s => s.get('pair').nominated = false, s => s.delete('local'), s => s.get('local').candidateType = 'unknown',
    s => s.get('remote').protocol = 'tcp', s => s.set('extra', { type: 'transport', dtlsState: 'connected', selectedCandidatePairId: 'other' })]) {
    const report = stats(); change(report); assert.equal(selectedIcePath(report), null);
  }
  const report = stats(); report.get('pair').currentRoundTripTime = Infinity;
  assert.equal('roundTripTimeSeconds' in selectedIcePath(report), false);
});

import { DeviceIdentity, grantBytes, hex, verifySignature } from '../dist/session-auth.js';
import { iceRequestBytes, iceConfigurationBytes, iceResponseBytes, signIceRequest, verifyIceConfiguration, validateIceConfiguration, icePeerConfiguration, iceConfigurationSummary } from '../dist/ice-config.js';
const config = now => ({ version: 1, transport_policy: 'relay', issued_at: now, expires_at: now + 600, ice_servers: [{ urls: ['turn:127.0.0.1:3478?transport=udp'], username: 'temporary-user', credential: 'temporary-password' }] });
test('temporary loopback ICE configuration is bounded, copied and never reflected in diagnostic summaries/errors', () => {
  const value = config(1000); assert.equal(icePeerConfiguration(value, 1000).iceTransportPolicy, 'relay');
  const copy = validateIceConfiguration(value, 1000); copy.ice_servers[0].credential = 'changed'; assert.equal(value.ice_servers[0].credential, 'temporary-password');
  const summary = JSON.stringify(iceConfigurationSummary(value)); assert.doesNotMatch(summary, /temporary|127|3478|credential|username|turn:/);
  for (const change of [c => c.extra = true, c => c.version = 2, c => c.transport_policy = 'unknown', c => c.issued_at = 1001,
    c => c.expires_at = 1209, c => c.expires_at = 1601, c => c.expires_at = Number.MAX_SAFE_INTEGER + 1,
    c => c.ice_servers = [], c => c.ice_servers = Array(5).fill(c.ice_servers[0]), c => c.ice_servers[0].credential = '',
    c => c.ice_servers[0].credential = 'x'.repeat(1025), c => c.ice_servers[0].username = 'x'.repeat(257), c => c.ice_servers[0].username = '\nsecret', c => c.ice_servers[0].credential = 'secret\n',
    c => c.ice_servers[0].extra = true, c => c.ice_servers[0].urls.push(c.ice_servers[0].urls[0]),
    c => c.ice_servers[0].urls.push('stun:127.0.0.1:3478'), c => c.ice_servers = [{ urls: ['stun:127.0.0.1:3478'] }]]) {
    const changed = structuredClone(value); change(changed); assert.throws(() => validateIceConfiguration(changed, 1000), error => { assert.doesNotMatch(error.message, /temporary-user|temporary-password|127|3478|secret/); return true; });
  }
  for (const url of ['turn:localhost:3478?transport=udp', 'turn:127.0.0.2:3478?transport=udp', 'turn:127.0.0.1:03478?transport=udp', 'turn:127.0.0.1:65536?transport=udp', 'turn:127.0.0.1:3478?transport=tcp', 'turns:127.0.0.1:3478?transport=udp', 'turn:127.0.0.1:3478', 'https://secret@example.com', 'turn:127.0.0.1:3478?transport=udp\n', 'stun:127.0.0.1:3478?transport=udp']) {
    const changed = config(1000); changed.ice_servers[0].urls = [url]; assert.throws(() => validateIceConfiguration(changed, 1000));
  }
  const stun = { ...value, transport_policy: 'all', ice_servers: [{ urls: ['stun:127.0.0.1:3478'], username: '', credential: '' }] };
  validateIceConfiguration(stun, 1000); stun.ice_servers[0].credential = 'secret'; assert.throws(() => validateIceConfiguration(stun, 1000));
  const old = config(1000); old.expires_at = 1010; iceConfigurationBytes(old); assert.throws(() => validateIceConfiguration(old, 1000));
});
test('temporary ICE credential boundary: exactly 210 s remaining is accepted and 209 s is refused', () => {
  const at210 = { ...config(1000), expires_at: 1210 }, at209 = { ...config(1000), expires_at: 1209 };
  assert.equal(validateIceConfiguration(at210, 1000).expires_at, 1210);
  assert.equal(icePeerConfiguration(at210, 1000).iceTransportPolicy, 'relay');
  assert.throws(() => validateIceConfiguration(at209, 1000), /insufficient remaining lifetime/);
  assert.throws(() => icePeerConfiguration(at209, 1000), /insufficient remaining lifetime/);
  assert.throws(() => validateIceConfiguration(at210, 1001), /insufficient remaining lifetime/);
});
async function signedFixture() {
  const host = await DeviceIdentity.generate(), device = await DeviceIdentity.generate(), now = 1000;
  const grant = { host_id: host.id, device_id: device.id, device_public_key: device.publicKey, vm_id: 'ice-vm', scopes: ['observe'], version: 1 };
  const signedGrant = { grant, signature: await host.sign(await grantBytes(grant)) };
  const challenge = { host_id: host.id, device_id: device.id, vm_id: 'ice-vm', scopes: ['observe'], grant_version: 1, instance_nonce: '11'.repeat(32), nonce: '22'.repeat(32), issued_at: now, expires_at: now + 60 };
  const options = { vm_id: 'ice-vm', scopes: ['observe'], now };
  const signed = await signIceRequest(device, signedGrant, host.publicKey, challenge, options);
  const response = { request: signed.request, config: config(now), signature: await host.sign(await iceResponseBytes(signed.request, config(now))) };
  return { host, device, signedGrant, challenge, options, signed, response, now };
}
test('actual P256 signs exact grant-bound ICE request and host signs request/config hashes', async () => {
  const f = await signedFixture(); await verifySignature(f.device.publicKey, iceRequestBytes(f.signed.request), f.signed.signature);
  assert.deepEqual(await verifyIceConfiguration(f.response, f.signed.request, f.host.publicKey, f.now), config(f.now));
  for (const change of [r => r.request.request_nonce = '33'.repeat(32), r => r.request.challenge.vm_id = 'other', r => r.request.challenge.scopes = ['input'],
    r => r.config.transport_policy = 'all', r => r.config.ice_servers[0].credential = 'substituted', r => r.config.ice_servers[0].urls = ['turn:127.0.0.1:9999?transport=udp'],
    r => r.config.expires_at--, r => r.extra = true, r => r.signature = '00'.repeat(64)]) {
    const changed = structuredClone(f.response); change(changed); await assert.rejects(verifyIceConfiguration(changed, f.signed.request, f.host.publicKey, f.now));
  }
  await assert.rejects(verifyIceConfiguration(f.response, f.signed.request, f.host.publicKey, f.now + 60));
  for (const change of [c => c.host_id = '00'.repeat(32), c => c.device_id = '00'.repeat(32), c => c.vm_id = 'other', c => c.vm_id = 'ice-vm\n', c => c.nonce = '1'.repeat(63) + '\n', c => c.scopes = ['input'], c => c.grant_version++, c => c.expires_at = f.now, c => c.issued_at = f.now + 1, c => c.extra = true]) {
    const changed = structuredClone(f.challenge); change(changed); await assert.rejects(signIceRequest(f.device, f.signedGrant, f.host.publicKey, changed, f.options));
  }
});

import { AuthenticatedReceiver } from '../dist/native-auth.js';
import { answerBytes, sdpBinding, verifyOffer, verifyClose } from '../dist/session-auth.js';
const sdp = byte => `v=0\r\na=fingerprint:sha-256 ${Array(32).fill(byte).join(':')}\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n`;
test('production receiver gets signed temporary ICE on separate challenge, then offers with fresh challenge', async () => {
  const f = await signedFixture(), receiver = await AuthenticatedReceiver.fromIdentity(f.device, { pinnedHostPublicKey: f.host.publicKey, vmId: 'ice-vm' });
  await receiver.acceptGrant(f.signedGrant); const previous = globalThis.fetch;
  const paths = []; let challenge, firstNonce, transcript, malformed = true, count = 0;
  globalThis.fetch = async (path, options) => {
    paths.push(path); const body = JSON.parse(options.body), now = Math.floor(Date.now() / 1000);
    assert.equal(options.cache, 'no-store'); assert.equal(options.credentials, 'omit'); assert.equal(options.redirect, 'error');
    if (path === '/auth/challenge') { challenge = { ...f.challenge, nonce: (++count).toString(16).padStart(64, '0'), issued_at: now, expires_at: now + 60 }; return Response.json(challenge); }
    if (path === '/auth/ice-config') {
      assert.deepEqual(body.request.challenge, challenge); await verifySignature(f.device.publicKey, iceRequestBytes(body.request), body.signature); firstNonce = challenge.nonce;
      const response = { request: body.request, config: config(now), signature: await f.host.sign(await iceResponseBytes(body.request, config(now))) };
      if (malformed) response.config.ice_servers[0].credential = 'changed';
      return Response.json(response);
    }
    if (path === '/auth/offer') {
      assert.notEqual(body.signed_offer.intent.challenge.nonce, firstNonce);
      await verifyOffer(body.signed_offer, f.signedGrant.grant, challenge, body.sdp, now);
      transcript = { intent: body.signed_offer.intent, session_id: 'ice-session', answer: await sdpBinding(sdp('BB')) };
      return Response.json({ type: 'answer', sdp: sdp('BB'), signed_answer: { transcript, signature: await f.host.sign(answerBytes(transcript)) } });
    }
    if (path === '/auth/close') { await verifyClose(body, transcript, f.device.publicKey); return Response.json({ closed: true }); }
    throw Error('unexpected endpoint');
  };
  try {
    await assert.rejects(receiver.iceConfiguration(), /signature/); assert.equal(receiver.offerTransmissionState, 'not-sent'); await receiver.close();
    assert.equal(paths.includes('/auth/offer'), false); malformed = false;
    const value = await receiver.iceConfiguration(); assert.equal(value.ice_servers[0].credential, 'temporary-password');
    await receiver.offer(sdp('AA'), 'ice-offer', 'h264'); await assert.rejects(receiver.iceConfiguration(), /pending or admitted/);
    await receiver.close(); assert.equal(receiver.offerTransmissionState, 'not-sent');
    assert.deepEqual(paths, ['/auth/challenge', '/auth/ice-config', '/auth/challenge', '/auth/ice-config', '/auth/challenge', '/auth/offer', '/auth/close']);
  } finally { receiver.dispose(); globalThis.fetch = previous; }
});

import { readFile } from 'node:fs/promises';
test('Rust ICE golden canonical bytes and host/device signatures verify in WebCrypto', async () => {
  const golden = JSON.parse(await readFile(new URL('../../crates/session-auth/tests/ice-golden.json', import.meta.url), 'utf8'));
  assert.equal(hex(iceRequestBytes(golden.signed_request.request)), golden.request_canonical_hex);
  assert.equal(hex(iceConfigurationBytes(golden.response.config)), golden.config_canonical_hex);
  assert.equal(hex(await iceResponseBytes(golden.response.request, golden.response.config)), golden.response_canonical_hex);
  await verifySignature(golden.device_public_key, iceRequestBytes(golden.signed_request.request), golden.signed_request.signature);
  await verifyIceConfiguration(golden.response, golden.signed_request.request, golden.host_public_key, golden.now);
});

import { spawnSync } from 'node:child_process';
test('WebCrypto-generated ICE request and response signatures verify in real Rust CLI; mutations fail', async () => {
  const f = await signedFixture();
  const binary = process.env.VEYNODE_PAIR_BIN ?? new URL('../../target/debug/vm-pair', import.meta.url).pathname;
  const cases = [
    { kind: 'ice_request', device_public_key: f.device.publicKey, signed: f.signed },
    { kind: 'ice_config', host_public_key: f.host.publicKey, expected_request: f.signed.request, signed: f.response, now: f.now },
  ];
  for (const request of cases) {
    const result = spawnSync(binary, ['verify'], { input: JSON.stringify(request), encoding: 'utf8', timeout: 5000 });
    assert.equal(result.status, 0, result.error?.message ?? result.stderr); assert.equal(JSON.parse(result.stdout).verified, true); assert.equal(JSON.parse(result.stdout).authorization_checked, false);
    const changed = structuredClone(request);
    if (request.kind === 'ice_request') changed.signed.request.request_nonce = '00'.repeat(32); else changed.signed.config.ice_servers[0].credential = 'substituted';
    assert.notEqual(spawnSync(binary, ['verify'], { input: JSON.stringify(changed), encoding: 'utf8', timeout: 5000 }).status, 0);
  }
});

import { iceCandidateCounts, iceStatsCounts } from '../dist/ice-config.js';
test('candidate diagnostics expose bounded counts only, with no arbitrary SDP or stats strings', () => {
  const candidate = 'a=candidate:SECRET_FOUNDATION 1 udp 2122260223 SECRET_ADDRESS 54321 typ relay raddr SECRET_RELATED rport 9 ufrag SECRET_UFRAG';
  assert.deepEqual(iceCandidateCounts(`m=video SECRET_MEDIA\r\na=ice-pwd:SECRET_PASSWORD\r\n${candidate}\r\na=end-of-candidates\r\n`), { total: 1, host: 0, srflx: 0, prflx: 0, relay: 1, udp: 1, tcp: 0, invalid: 0, mediaSections: 1, endOfCandidates: 1 });
  const malformed = iceCandidateCounts('a=candidate:SECRET_BROKEN'); assert.equal(malformed.invalid, 1); assert.doesNotMatch(JSON.stringify(malformed), /SECRET/);
  const report = stats(); report.get('pair').requestsSent = 3; report.get('pair').responsesReceived = 2;
  const counts = iceStatsCounts(report); assert.equal(counts.localRelay, 1); assert.equal(counts.succeeded, 1); assert.equal(counts.requestsSent, 3); assert.equal(counts.responsesReceived, 2);
  assert.doesNotMatch(JSON.stringify(counts), /secret|private|address|credential/);
});

import { videoReceptionCounts } from '../dist/ice-config.js';
test('zero-frame media diagnostics distinguish absent counters from received undecodable video', () => {
  const absent = videoReceptionCounts(new Map()); assert.equal(absent.streams, 0); assert.equal(absent.framesDecoded, null); assert.equal(absent.packetsReceived, null);
  const report = new Map([['video', { type: 'inbound-rtp', kind: 'video', packetsReceived: 15, bytesReceived: 4096, framesReceived: 1, framesDecoded: 0, keyFramesDecoded: 0, pliCount: 3, nackCount: 0, privateField: 'SECRET' }], ['audio', { type: 'inbound-rtp', kind: 'audio', framesDecoded: 999 }]]);
  const counts = videoReceptionCounts(report); assert.equal(counts.streams, 1); assert.equal(counts.framesDecoded, 0); assert.equal(counts.packetsReceived, 15); assert.equal(counts.pliCount, 3); assert.equal(counts.framesDropped, null); assert.doesNotMatch(JSON.stringify(counts), /SECRET/);
});
