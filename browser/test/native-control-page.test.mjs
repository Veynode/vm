import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { deflateSync } from 'node:zlib';
import { AuthenticatedReceiver } from '../dist/native-auth.js';
import { DeviceIdentity, grantBytes, answerBytes, sdpBinding, verifyOffer, verifyConfirmation, verifyClose } from '../dist/session-auth.js';
import { verifyControl } from '../dist/session-control.js';
import { iceRequestBytes, iceResponseBytes } from '../dist/ice-config.js';
import { verifySignature } from '../dist/session-auth.js';
import { FakeChannel } from './control-channel-fixture.mjs';
const sdp = byte => `v=0\r\na=fingerprint:sha-256 ${Array(32).fill(byte).join(':')}\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n`;
function png(width, height) {
  const chunk = (name, body) => { const data = Buffer.concat([Buffer.from(name), body]); let crc = 0xffffffff; for (const b of data) { crc ^= b; for (let i = 0; i < 8; i++) crc = (crc >>> 1) ^ ((crc & 1) ? 0xedb88320 : 0); } const result = Buffer.alloc(body.length + 12); result.writeUInt32BE(body.length); data.copy(result, 4); result.writeUInt32BE((crc ^ 0xffffffff) >>> 0, result.length - 4); return result; };
  const header = Buffer.alloc(13); header.writeUInt32BE(width); header.writeUInt32BE(height, 4); header[8] = 8; header[9] = 6;
  return Buffer.concat([Buffer.from('89504e470d0a1a0a', 'hex'), chunk('IHDR', header), chunk('IDAT', deflateSync(Buffer.alloc(height * (width * 4 + 1)))), chunk('IEND', Buffer.alloc(0))]);
}
const image = png(1000, 500);
const jpeg = readFileSync(new URL('./fixtures/screenshot-1000x500.jpg', import.meta.url));
// These are production DOM event/crypto state tests, with synthetic browser
// graphics and HTTP. Actual image decoding and VM behavior require browser tests.
async function page(scopes, name, action) {
  const host = await DeviceIdentity.generate(), device = await DeviceIdentity.generate();
  const receiver = await AuthenticatedReceiver.fromIdentity(device, { pinnedHostPublicKey: host.publicKey, vmId: 'control-fixture', scopes });
  const grant = { host_id: host.id, device_id: device.id, device_public_key: device.publicKey, vm_id: 'control-fixture', scopes, version: 1 };
  const signedGrant = { grant, signature: await host.sign(await grantBytes(grant)) };
  const saved = new Map(); const replace = (key, value) => { saved.set(key, Object.getOwnPropertyDescriptor(globalThis, key)); Object.defineProperty(globalThis, key, { value, writable: true, configurable: true }); };
  let clock = 100, transcript, sequence = 0, epoch = 0, frameCounter = 0, pendingInput, releaseInput, leaseDeadline;
  let delayFile = false, releaseFile, uploadDeclaration, delayClose = false, releaseClose; const fileBytes = new Uint8Array([1, 2, 3]);
  let confirmed = false, denyNextInput = false, delayNextInput = false, offerCount = 0, failOffer = false, denyChallenge = false;
  const iceEnabled = name.startsWith('ice-');
  const calls = [], controls = [], peers = [], imageSources = []; let latePlayReject, diagnosticDeadline, closeRejected = false;
  class Element {
    listeners = new Map(); disabled = false; hidden = false; checked = false; value = ''; textContent = ''; style = {}; srcObject = null;
    addEventListener(type, callback) { const list = this.listeners.get(type) ?? []; list.push(callback); this.listeners.set(type, list); }
    emit(type, event = {}) { return Promise.all((this.listeners.get(type) ?? []).map(callback => callback(event))); }
    click(event = {}) { return this.emit('click', event); }
    setAttribute() {}
    getContext() { return { drawImage() {}, getImageData() { return { data: new Uint8Array(16 * 9 * 4) }; } }; }
    getBoundingClientRect() { return { left: 50, top: 50, right: 550, bottom: 300, width: 500, height: 250 }; }
    requestVideoFrameCallback(callback) { this.videoCallback = callback; return 1; } cancelVideoFrameCallback() {}
    play() { return new Promise((_, reject) => { latePlayReject = reject; }); }
  }
  const nodes = new Map(); const element = id => { if (!nodes.has(id)) nodes.set(id, new Element()); return nodes.get(id); };
  const poll = async predicate => { for (let i = 0; i < 200; i++) { if (predicate()) return; await new Promise(resolve => setTimeout(resolve, 5)); } throw Error('Page did not settle: ' + element('#control-status').textContent); };
  class Peer extends EventTarget {
    iceGatheringState = 'complete'; connectionState = 'new'; localDescription = null;
    constructor(configuration) { super(); this.configuration = structuredClone(configuration); peers.push(this); }
    addTransceiver() { return { setCodecPreferences() {} }; } async createOffer() { if (failOffer) { failOffer = false; throw Error('synthetic pre-offer failure'); } return { type: 'offer', sdp: sdp('AA') }; }
    createDataChannel(label, options) { assert.equal(label, 'veynode-control-v1'); assert.deepEqual(options, { ordered: true, protocol: 'veynode-control/v1' }); this.channel = new FakeChannel(handleControl); return this.channel; }
    async setLocalDescription(value) { this.localDescription = value; } async setRemoteDescription() {}
    async getStats() { return this.mediaReady ? new Map([['inbound', { type: 'inbound-rtp', kind: 'video', codecId: 'codec', framesDecoded: 5, frameWidth: 1000, frameHeight: 500 }], ['codec', { mimeType: 'video/H264', clockRate: 90000, sdpFmtpLine: 'profile-level-id=42e01f;packetization-mode=1' }]]) : new Map(); } close() { this.connectionState = 'closed'; }
  }
  class Image {
    naturalWidth = 0; naturalHeight = 0;
    set src(value) { imageSources.push(value); const bytes = Buffer.from(value.split(',')[1], 'base64'); if (value.startsWith('data:image/jpeg;')) { this.naturalWidth = name === 'jpeg-dimensions' ? 999 : 1000; this.naturalHeight = 500; } else { this.naturalWidth = bytes.readUInt32BE(16); this.naturalHeight = bytes.readUInt32BE(20); } }
    async decode() { if (name === 'jpeg-decode') throw new Error('Synthetic decoder rejection'); }
  }
  class Stream { constructor(tracks = []) { this.tracks = tracks; } getTracks() { return this.tracks; } }
  const handleControl = async body => {
    const now = Math.floor(Date.now() / 1000);
      assert.equal(confirmed, true); await verifyControl(body, transcript, device.publicKey, now);
      assert.ok(body.request.sequence > sequence); sequence = body.request.sequence; const op = body.request.action; controls.push(structuredClone(op)); let result;
      if (op.kind === 'file') {
        const a = op.operation;
        if (a.kind === 'upload_begin') { uploadDeclaration = a; result = {ok:true,value:{token:'a'.repeat(32),size:a.size,sha256:a.sha256,chunk_bytes:32768,idle_timeout_seconds:60}}; }
        else if (a.kind === 'upload_write') result = {ok:true,value:{offset:a.offset+Buffer.from(a.data,'base64').length}};
        else if (a.kind === 'upload_commit') { if(delayFile) { delayFile=false;await new Promise(r=>releaseFile=r); } result={ok:true,value:{published:true,size:uploadDeclaration.size,sha256:uploadDeclaration.sha256}}; }
        else if (a.kind === 'cancel') result={ok:true,value:{cancelled:true}};
        else throw new Error('Unexpected file fixture op');
      } else if (op.kind === 'acquire') result = { epoch: ++epoch, expires_in_seconds: 60 };
      else if (op.kind === 'release') result = { released: true };
      else if (op.kind === 'observe_frame') result = { frame_id: `00000000-0000-0000-0000-${String(++frameCounter).padStart(12, '0')}`, source_generation: 4, width: 1000, height: 500, coordinate_space: 'guest-physical-pixels', mime_type: 'image/png', byte_length: image.length, data_base64: image.toString('base64') };
      else { if (denyNextInput) { denyNextInput = false; return { sequence: body.request.sequence, error: 'control rejected' }; } if (delayNextInput) { delayNextInput = false; pendingInput?.(); await new Promise(resolve => { releaseInput = resolve; }); } result = { dispatched: true, steps: 1, guest_delivery_confirmed: false }; }
      if (op.kind === 'observe_frame' && name.startsWith('jpeg-')) {
        Object.assign(result, { mime_type: 'image/jpeg', byte_length: jpeg.length, data_base64: jpeg.toString('base64'), sha256: createHash('sha256').update(jpeg).digest('hex'), input_anchor_available: true });
        if (name === 'jpeg-hash') result.sha256 = '00'.repeat(32);
        if (name === 'jpeg-anchor') result.input_anchor_available = false;
        if (name === 'jpeg-generation') delete result.source_generation;
      }
      return { sequence: body.request.sequence, result };
  };
  const oldOpen = AuthenticatedReceiver.open;
  let connectTask;
  try {
    if (name.startsWith('files-')) { const timer = globalThis.setTimeout; replace('setTimeout', (callback, ms, ...args) => { if (ms === 60000) { leaseDeadline = callback; return timer(() => {}, 60000); } return timer(callback, ms, ...args); }); }
    if (name.startsWith('automatic-')) { const timer = globalThis.setTimeout; replace('setTimeout', (callback, ms, ...args) => { if (ms === 120000) { diagnosticDeadline = callback; return timer(() => {}, 120000); } return timer(callback, ms, ...args); }); }
    if (name === 'source-deadline') { const timer = globalThis.setTimeout; replace('setTimeout', (callback, ms, ...args) => timer(callback, ms === 12000 ? 120 : ms, ...args)); }
    replace('document', { querySelector: element, createElement: () => new Element() }); replace('window', new EventTarget()); replace('Image', Image);
    replace('performance', { now: () => clock }); replace('MediaStream', Stream); replace('RTCPeerConnection', Peer);
    replace('RTCRtpReceiver', { getCapabilities: () => ({ codecs: [{ mimeType: 'video/h264', clockRate: 90000, sdpFmtpLine: 'profile-level-id=42e01f;packetization-mode=1' }, ...(name === 'source-deadline' ? [{ mimeType: 'video/h265', clockRate: 90000, sdpFmtpLine: 'profile-id=1;tier-flag=0;level-id=120' }] : [])] }) });
    replace('fetch', async (path, options) => {
      calls.push(path); if (path === '/capabilities') return Response.json({ h264: 'verified-hardware', h265: name === 'source-deadline' ? 'verified-hardware' : 'unavailable', authentication: { required: true, vm_id: grant.vm_id, control_enabled: name !== 'files-only', files_enabled: scopes.includes('files'), control_transport: 'webrtc-datachannel-v1', ice_config_enabled: iceEnabled } });
      const body = JSON.parse(options.body), now = Math.floor(Date.now() / 1000);
      if (path === '/auth/challenge') { if (denyChallenge) { denyChallenge = false; return Response.json({ error: 'revoked' }, { status: 403 }); } confirmed = false; sequence = 0; offerCount++; return Response.json({ host_id: host.id, device_id: device.id, vm_id: grant.vm_id, scopes, grant_version: 1, instance_nonce: '01'.repeat(32), nonce: '02'.repeat(32), issued_at: now, expires_at: now + 60 }); }
      if (path === '/auth/ice-config') {
        if (name === 'ice-malformed') return new Response('credential=DO-NOT-LOG-ICE-PASSWORD (malformed JSON)', { status: 200 });
        await verifySignature(device.publicKey, iceRequestBytes(body.request), body.signature);
        const config = { version: 1, transport_policy: 'relay', issued_at: now, expires_at: now + 600, ice_servers: [{ urls: ['turn:127.0.0.1:3478?transport=udp'], username: 'sensitive-fixture-user', credential: 'sensitive-fixture-password' }] };
        return Response.json({ request: body.request, config, signature: await host.sign(await iceResponseBytes(body.request, config)) });
      }
      if (path === '/auth/offer') { await verifyOffer(body.signed_offer, grant, body.signed_offer.intent.challenge, body.sdp, now); transcript = { intent: body.signed_offer.intent, session_id: 'control-session-' + offerCount, answer: await sdpBinding(sdp('BB')) }; return Response.json({ type: 'answer', sdp: sdp('BB'), signed_answer: { transcript, signature: await host.sign(answerBytes(transcript)) } }); }
      if (path === '/auth/confirm') { await verifyConfirmation(body.confirmation, transcript, device.publicKey, now); confirmed = true; return Response.json({ confirmed: true }); }
      if (path === '/auth/close') { await verifyClose(body, transcript, device.publicKey); if (delayClose) { delayClose = false; await new Promise(resolve => { releaseClose = resolve; }); } if (name === 'automatic-rejected' && !closeRejected) { closeRejected = true; return Response.json({ closed: false }); } return Response.json({ closed: true, ...(name.startsWith('source-') ? { media_failure: { code: name === 'source-asleep' ? 'guest_display_asleep' : 'guest_capture_no_frames' } } : {}) }); }
      throw Error('Unexpected or plaintext endpoint: ' + path);
    });
    AuthenticatedReceiver.open = async options => { assert.deepEqual(options.scopes, scopes); return receiver; };
    element('#host-pin').value = host.publicKey; element('#signed-grant').value = JSON.stringify(signedGrant); element('#request-input').checked = scopes.includes('input');
    element('#request-files').checked = scopes.includes('files');
    element('#pointer-action').value = 'click'; element('#physical-key').value = 'ret';
    await import(`../dist/native.js?control-page=${name}`);
    await poll(() => element('#status').textContent.includes('verified pairing'));
    await element('#prepare-pairing').click(); await poll(() => element('#pairing-request').textContent.includes('digest'));
    assert.equal(element('#request-input').disabled, true);
    await element('#accept-grant').click(); await poll(() => element('#pairing-status').textContent.includes('Ready'));
    connectTask = element('#connect').click(); if (name === 'ice-malformed') await connectTask; else await poll(() => confirmed && !(name === 'files-only' ? element('#upload-file-button') : element('#refresh-control-frame')).disabled);
    const refresh = async () => { await element('#refresh-control-frame').click(); await poll(() => element('#control-status').textContent.startsWith('Screenshot ready')); };
    const acquire = async () => { await element('#acquire-control').click(); await poll(() => element('#control-status').textContent.startsWith('Control acquired')); };
    const reconnect = async (fail = false) => { if (connectTask) await connectTask; confirmed = false; failOffer = fail === true; denyChallenge = fail === 'challenge'; connectTask = element('#connect').click(); if (fail) { await connectTask; return; } await poll(() => confirmed && !(name === 'files-only' ? element('#upload-file-button') : element('#refresh-control-frame')).disabled); };
    const startMedia = async () => { const peer = peers.at(-1); peer.connectionState = 'connected'; peer.onconnectionstatechange(); peer.mediaReady = true; peer.ontrack({ track: { stop() {} } }); const video = element('#received'); video.videoWidth = 1000; video.videoHeight = 500; for (let i = 0; i < 5; i++) video.videoCallback(); await poll(() => element('#status').textContent.startsWith('Receiving native')); };
    await action({ delayClose: () => { delayClose = true; }, releaseClose: () => releaseClose?.(), fileBytes, expireLease: () => leaseDeadline(), delayFile: () => {delayFile=true;}, releaseFile: () => releaseFile?.(), startMedia, expire: () => { assert.equal(typeof diagnosticDeadline, 'function'); diagnosticDeadline(); }, imageSources, reconnect, element, poll, refresh, acquire, controls, calls, peers, receiver, advance: ms => { clock += ms; }, deny: () => { denyNextInput = true; }, delay: () => { delayNextInput = true; return new Promise(resolve => { pendingInput = resolve; }); }, release: () => releaseInput?.(), rejectPlay: () => latePlayReject?.(new Error('late old playback')) });
  } finally {
    releaseInput?.(); releaseFile?.(); releaseClose?.(); await element('#disconnect').click(); if (connectTask) await connectTask;
    assert.equal(calls.includes('/auth/control'), false);
    receiver.dispose(); AuthenticatedReceiver.open = oldOpen;
    for (const [key, descriptor] of saved) { if (descriptor) Object.defineProperty(globalThis, key, descriptor); else delete globalThis[key]; }
  }
}

test('observe-only page refuses acquire/key/pointer even with a fresh screenshot', async () => page(['observe'], 'observe', async f => {
  await f.refresh(); const count = f.controls.length;
  await f.element('#acquire-control').click(); await f.element('#send-physical-key').click(); await f.element('#control-frame').click({ clientX: 300, clientY: 175 });
  assert.equal(f.controls.length, count); assert.equal(f.element('#acquire-control').disabled, true); assert.equal(f.element('#send-physical-key').disabled, true);
}));

test('input uses displayed screenshot CSS mapping, explicit lease, fresh frame and physical-key allowlist', async () => page(['input', 'observe'], 'input', async f => {
  await f.refresh(); const before = f.controls.length; await f.element('#control-frame').click({ clientX: 300, clientY: 175 }); assert.equal(f.controls.length, before);
  await f.acquire(); await f.refresh();
  await f.element('#control-frame').click({ clientX: 550, clientY: 300 }); assert.equal(f.controls.at(-1).kind, 'observe_frame', 'Outside right/bottom boundary is rejected');
  await f.element('#control-frame').click({ clientX: 300, clientY: 175 }); await f.poll(() => f.controls.at(-1).kind === 'click' && !f.element('#refresh-control-frame').disabled);
  assert.deepEqual({ x: f.controls.at(-1).x, y: f.controls.at(-1).y }, { x: 500, y: 250 }); assert.equal(f.controls.at(-1).frame.width, 1000);
  await f.refresh(); f.advance(4001); const count = f.controls.length; await f.element('#send-physical-key').click(); assert.equal(f.controls.length, count); assert.match(f.element('#control-status').textContent, /expired/);
  await f.refresh(); f.element('#physical-key').value = 'arbitrary_text'; await f.element('#send-physical-key').click(); assert.equal(f.controls.at(-1).kind, 'observe_frame');
  f.element('#physical-key').value = 'ret'; await f.element('#send-physical-key').click(); await f.poll(() => f.controls.at(-1).kind === 'key' && !f.element('#refresh-control-frame').disabled); assert.deepEqual(f.controls.at(-1).keys, ['ret']);
  await f.refresh(); f.element('#pointer-action').value = 'drag';
  await f.element('#control-frame').click({ clientX: 50, clientY: 50 }); assert.equal(f.controls.at(-1).kind, 'observe_frame');
  await f.element('#control-frame').click({ clientX: 549.9, clientY: 299.9 }); await f.poll(() => f.controls.at(-1).kind === 'drag' && !f.element('#refresh-control-frame').disabled);
  assert.deepEqual([f.controls.at(-1).x, f.controls.at(-1).y, f.controls.at(-1).end_x, f.controls.at(-1).end_y], [0, 0, 999, 499]);
  await f.refresh(); f.element('#pointer-action').value = 'scroll-down'; await f.element('#control-frame').click({ clientX: 300, clientY: 175 });
  await f.poll(() => f.controls.at(-1).kind === 'scroll' && !f.element('#refresh-control-frame').disabled); assert.equal(f.controls.at(-1).direction, 'down'); assert.equal(f.controls.at(-1).ticks, 3);
  assert.equal(f.element('#result').textContent.includes('data_base64'), false); assert.equal(f.element('#result').textContent.includes(image.toString('base64')), false);
}));

test('semantic denial fences the session without reacquiring; explicit reconnect requires fresh confirmation', async () => page(['input', 'observe'], 'takeover', async f => {
  await f.acquire(); await f.refresh(); f.deny(); await f.element('#control-frame').click({ clientX: 300, clientY: 175 });
  await f.poll(() => f.calls.includes('/auth/close') && !f.element('#connect').disabled);
  const count = f.controls.length; await f.element('#control-frame').click({ clientX: 300, clientY: 175 }); await f.element('#acquire-control').click();
  assert.equal(f.controls.length, count); assert.equal(f.controls.filter(x => x.kind === 'acquire').length, 1);
  assert.equal(f.element('#acquire-control').disabled, true); assert.equal(f.peers[0].channel.readyState, 'closed');
  await f.reconnect(); assert.equal(f.calls.filter(x => x === '/auth/confirm').length, 2);
  await f.acquire(); assert.equal(f.controls.filter(x => x.kind === 'acquire').length, 2);
}));

test('disconnect fences pending control before signed cleanup and never retries the input', async () => page(['input', 'observe'], 'pending', async f => {
  await f.acquire(); await f.refresh(); const reached = f.delay(); await f.element('#control-frame').click({ clientX: 300, clientY: 175 }); await reached;
  const count = f.controls.length, closing = f.element('#disconnect').click();
  await f.poll(() => f.peers[0].channel.readyState === 'closed');
  assert.equal(f.element('#acquire-control').disabled, true); await f.element('#send-physical-key').click(); assert.equal(f.controls.length, count);
  f.release(); await closing; assert.equal(f.calls.filter(x => x === '/auth/close').length, 1);
  assert.equal(f.controls.filter(x => x.kind === 'click').length, 1);
}));

test('pre-offer failure closes and clears its channel so an explicit retry uses a fresh one', async () => page(['observe'], 'pre-offer', async f => {
  await f.element('#disconnect').click(); const before = f.calls.filter(x => x === '/auth/offer').length;
  await f.reconnect(true); assert.equal(f.peers[1].channel.readyState, 'closed');
  assert.equal(f.calls.filter(x => x === '/auth/offer').length, before); assert.equal(f.element('#connect').disabled, false);
  await f.reconnect(); await f.refresh(); assert.equal(f.peers.length, 3);
  assert.equal(f.calls.filter(x => x === '/auth/offer').length, before + 1);
}));

test('late old ontrack/playback failures cannot mutate video or restart old cleanup', async () => page(['input', 'observe'], 'late-track', async f => {
  const old = f.peers[0]; old.ontrack({ track: { stop() {} } });
  await f.element('#disconnect').click(); const before = f.element('#result').textContent, closeCount = f.calls.filter(x => x === '/auth/close').length;
  const nextVideo = { belongsTo: 'newer-render' }; f.element('#received').srcObject = nextVideo;
  old.ontrack({ track: { stop() {} } }); f.rejectPlay(); await new Promise(resolve => setTimeout(resolve, 10));
  assert.equal(f.element('#received').srcObject, nextVideo); assert.equal(f.element('#result').textContent, before); assert.equal(f.calls.filter(x => x === '/auth/close').length, closeCount);
}));

test('challenge denial before offer clears only local resources and permits explicit pairing/retry', async () => page(['observe'], 'challenge-denied', async f => {
  await f.element('#disconnect').click();
  const offers = f.calls.filter(x => x === '/auth/offer').length, closes = f.calls.filter(x => x === '/auth/close').length;
  await f.reconnect('challenge');
  assert.equal(f.peers[1].channel.readyState, 'closed');
  assert.equal(f.calls.filter(x => x === '/auth/offer').length, offers);
  assert.equal(f.calls.filter(x => x === '/auth/close').length, closes);
  assert.equal(f.element('#connect').disabled, false);
  const report = JSON.parse(f.element('#result').textContent);
  assert.equal(report.cleanupBasis, 'local-offer-not-sent');
  assert.equal(report.cleanupError, undefined);
  await f.reconnect(); await f.refresh();
  assert.equal(f.calls.filter(x => x === '/auth/offer').length, offers + 1);
  assert.equal(f.peers.length, 3);
}));

test('native page passes only verified temporary ICE to peer and never exposes credentials in evidence', async () => page(['observe'], 'ice-relay', async f => {
  assert.equal(f.peers[0].configuration.iceTransportPolicy, 'relay');
  assert.equal(f.peers[0].configuration.iceServers[0].credential, 'sensitive-fixture-password');
  assert.equal(f.calls.filter(path => path === '/auth/challenge').length, 2);
  assert.equal(f.calls.filter(path => path === '/auth/ice-config').length, 1);
  assert.doesNotMatch(f.element('#result').textContent, /sensitive-fixture|turn:127|credential|ice_servers/);
  assert.equal(JSON.parse(f.element('#result').textContent).iceConfiguration.policy, 'relay');
}));

test('malformed ICE response never leaks credential snippet and leaves pre-offer failure locally retryable', async () => page(['observe'], 'ice-malformed', async f => {
  assert.equal(f.peers.length, 0); assert.equal(f.calls.includes('/auth/offer'), false);
  assert.equal(f.receiver.offerTransmissionState, 'not-sent');
  assert.equal(f.element('#connect').disabled, false);
  assert.match(f.element('#result').textContent, /Malformed authenticated signaling response/);
  assert.doesNotMatch(f.element('#result').textContent, /DO-NOT-LOG-ICE-PASSWORD|credential=/);
}));

test('native ICE events report only type/count/error code and stop mutating after cleanup', async () => page(['observe'], 'ice-events', async f => {
  const peer = f.peers[0], candidate = new Event('icecandidate');
  candidate.candidate = { candidate: 'candidate:SECRET_FOUNDATION 1 udp 42 SECRET_ADDRESS 54321 typ relay ufrag SECRET_UFRAG' };
  peer.dispatchEvent(candidate);
  const error = new Event('icecandidateerror'); Object.assign(error, { errorCode: 701, address: 'SECRET_ADDRESS', url: 'turn:SECRET_URL', errorText: 'SECRET_TEXT' }); peer.dispatchEvent(error);
  const data = JSON.parse(f.element('#result').textContent), current = data.iceAttempts.at(-1);
  assert.equal(current.localCandidateEvents.relay, 1); assert.deepEqual(current.candidateErrorCodes, [701]);
  assert.doesNotMatch(f.element('#result').textContent, /SECRET/);
  await f.element('#disconnect').click(); const before = f.element('#result').textContent;
  peer.dispatchEvent(candidate); peer.dispatchEvent(error); assert.equal(f.element('#result').textContent, before);
}));

test('failed ICE retains only historical sanitized selected path without making it current or passing video', async () => page(['observe'], 'ice-path-history', async f => {
  const peer = f.peers[0];
  peer.getStats = async () => new Map([
    ['transport', { type: 'transport', dtlsState: 'connected', selectedCandidatePairId: 'pair' }],
    ['pair', { type: 'candidate-pair', state: 'succeeded', nominated: true, localCandidateId: 'local', remoteCandidateId: 'remote', currentRoundTripTime: .02 }],
    ['local', { type: 'local-candidate', candidateType: 'relay', protocol: 'udp', address: 'SECRET_ADDRESS', credential: 'SECRET_CREDENTIAL' }],
    ['remote', { type: 'remote-candidate', candidateType: 'relay', protocol: 'udp', url: 'SECRET_URL' }],
  ]);
  const report = () => JSON.parse(f.element('#result').textContent);
  await f.poll(() => report().iceAttempts.at(-1).lastSelectedPath !== undefined);
  const historical = report().iceAttempts.at(-1).lastSelectedPath;
  assert.equal(historical.path.relayed, true);
  assert.equal(typeof historical.observedElapsedMs, 'number');
  peer.getStats = async () => new Map();
  await f.poll(() => report().selectedIcePath === null);
  assert.deepEqual(report().iceAttempts.at(-1).lastSelectedPath, historical);
  assert.notEqual(report().status, 'passed-live');
  assert.equal(report().result, undefined);
  assert.doesNotMatch(JSON.stringify(report()), /SECRET/);
}));

test('JPEG is verified before display and retains physical screenshot coordinate mapping', async () => page(['input', 'observe'], 'jpeg-valid', async f => {
  await f.acquire(); await f.refresh();
  assert.ok(f.imageSources.at(-1).startsWith('data:image/jpeg;base64,'));
  await f.element('#control-frame').click({ clientX: 300, clientY: 175 });
  await f.poll(() => f.controls.at(-1).kind === 'click');
  assert.equal(f.controls.at(-1).x, 500); assert.equal(f.controls.at(-1).y, 250);
  assert.equal(f.element('#result').textContent.includes(jpeg.toString('base64')), false);
}));
for (const failure of ['hash', 'dimensions', 'decode', 'anchor', 'generation']) test(`JPEG ${failure} failure never enables screenshot input`, async () => page(['input', 'observe'], `jpeg-${failure}`, async f => {
  await f.acquire(); await f.element('#refresh-control-frame').click();
  await f.poll(() => f.element('#control-status').textContent.startsWith('Screenshot unavailable') || f.element('#connect').disabled === false);
  const count = f.controls.length;
  await f.element('#control-frame').click({ clientX: 300, clientY: 175 });
  await f.element('#send-physical-key').click();
  assert.equal(f.controls.length, count); assert.equal(f.element('#send-physical-key').disabled, true);
  if (failure === 'hash' || failure === 'anchor' || failure === 'generation') assert.equal(f.imageSources.length, 0, 'Invalid bytes or anchors never reach the decoder');
}));

for (const [name, message] of [['source-asleep', 'Guest display is asleep'], ['source-zero', 'cause is not established']]) test(`native page replaces generic transport failure with acknowledged ${name} diagnostic`, async () => page(['observe'], name, async f => {
  const peer = f.peers.at(-1); peer.connectionState = 'failed'; peer.onconnectionstatechange();
  await f.poll(() => f.element('#status').textContent.includes(message));
  const report = JSON.parse(f.element('#result').textContent);
  assert.equal(report.status, 'failed'); assert.equal(report.mediaFailure.code, name === 'source-asleep' ? 'guest_display_asleep' : 'guest_capture_no_frames');
  assert.equal(f.calls.filter(path => path === '/auth/offer').length, 1);
  assert.equal(f.element('#connect').disabled, false);
}));

test('source reason arriving during overall-deadline cleanup blocks a second codec offer', async () => page(['observe'], 'source-deadline', async f => {
  await f.poll(() => f.element('#status').textContent.includes('cause is not established'));
  assert.equal(f.peers.length, 1); assert.equal(f.calls.filter(path => path === '/auth/offer').length, 1);
  assert.equal(f.element('#connect').disabled, false);
}));

test('automatic live-session deadline restores Connect only after confirmed cleanup; old events cannot close the new session', async () => page(['files', 'observe'], 'automatic-acknowledged', async f => {
  await f.startMedia(); const oldPeer = f.peers.at(-1);
  f.expire(); assert.equal(f.element('#connect').disabled, true, 'Pending cleanup remains fenced');
  await f.poll(() => !f.element('#connect').disabled);
  assert.equal(f.element('#disconnect').disabled, true); assert.equal(f.element('#retry-cleanup').hidden, true);
  assert.equal(JSON.parse(f.element('#result').textContent).cleanupStatus, 'acknowledged');
  assert.equal(JSON.parse(f.element('#result').textContent).cleanupBasis, 'close-response-to-signed-request');
  assert.match(f.element('#file-status').textContent, /cleanup acknowledged/);
  await f.reconnect(); assert.equal(f.peers.length, 2);
  const readyFiles = f.element('#file-status').textContent;
  oldPeer.connectionState = 'failed'; oldPeer.onconnectionstatechange();
  assert.equal(f.element('#file-status').textContent, readyFiles);
  assert.match(readyFiles, /^Files ready/);
  assert.equal(f.element('#connect').disabled, true); assert.equal(f.element('#disconnect').disabled, false);
  assert.equal(f.calls.filter(path => path === '/auth/close').length, 1);
}));
test('automatic deadline with missing cleanup ACK stays fenced until explicit retry succeeds', async () => page(['files', 'observe'], 'automatic-rejected', async f => {
  await f.startMedia(); f.expire();
  await f.poll(() => !f.element('#retry-cleanup').hidden);
  assert.equal(f.element('#connect').disabled, true);
  assert.equal(JSON.parse(f.element('#result').textContent).cleanupStatus, 'unconfirmed');
  assert.match(f.element('#file-status').textContent, /cleanup is unconfirmed.*Reconnect remains blocked/);
  const offers = f.calls.filter(path => path === '/auth/offer').length;
  await f.element('#connect').click(); assert.equal(f.calls.filter(path => path === '/auth/offer').length, offers);
  await f.element('#retry-cleanup').click();
  assert.equal(f.element('#connect').disabled, false); assert.equal(f.element('#disconnect').disabled, true);
  assert.match(f.element('#file-status').textContent, /cleanup acknowledged/);
}));
test('post-acceptance transport failure also releases page ownership after cleanup', async () => page(['files', 'observe'], 'automatic-transport', async f => {
  await f.startMedia(); const peer = f.peers.at(-1); peer.connectionState = 'failed'; peer.onconnectionstatechange();
  await f.poll(() => !f.element('#connect').disabled);
  assert.equal(f.element('#disconnect').disabled, true);
  assert.equal(JSON.parse(f.element('#result').textContent).status, 'stream-ended');
  assert.match(f.element('#file-status').textContent, /cleanup acknowledged/);
}));

// Synthetic DOM/crypto fixtures exercise the actual page event path, not Guest acceptance.
test('files-only page uploads without an input adapter or lease', async()=>page(['files','observe'],'files-only',async f=>{
  f.element('#upload-file').files=[Object.assign(new Blob([f.fileBytes]),{name:'fixture.bin'})];
  await f.element('#upload-file-button').click();await f.poll(()=>f.element('#file-status').textContent.startsWith('Upload published'));
  assert.equal(f.controls.some(a=>a.kind==='acquire'),false);assert.equal(f.controls.filter(a=>a.kind==='file').length,3);
  assert.equal(f.element('#result').textContent.includes('token'),false);
}));
test('input lease expiry does not discard acknowledged file upload in the same session',async()=>page(['files','input','observe'],'files-lease',async f=>{
  await f.acquire();f.delayFile();f.element('#upload-file').files=[Object.assign(new Blob([f.fileBytes]),{name:'fixture.bin'})];
  await f.element('#upload-file-button').click();await f.poll(()=>f.controls.at(-1)?.operation?.kind==='upload_commit');
  f.expireLease();f.releaseFile();await f.poll(()=>f.element('#file-status').textContent.startsWith('Upload published'));
  assert.match(f.element('#control-status').textContent,/lease expired/);
}));


test('manual file-session close shows pending then acknowledged cleanup; late upload completion cannot overwrite a new session', async () => page(['files', 'observe'], 'files-manual-close', async f => {
  await f.startMedia(); f.delayFile();
  f.element('#upload-file').files = [Object.assign(new Blob([f.fileBytes]), { name: 'fixture.bin' })];
  await f.element('#upload-file-button').click();
  await f.poll(() => f.controls.at(-1)?.operation?.kind === 'upload_commit');
  f.delayClose(); const closing = f.element('#disconnect').click();
  await f.poll(() => f.calls.includes('/auth/close'));
  assert.match(f.element('#file-status').textContent, /requires the session close acknowledgment/);
  assert.equal(f.element('#connect').disabled, true);
  f.releaseClose(); await closing;
  assert.match(f.element('#file-status').textContent, /cleanup acknowledged/);
  assert.equal(f.element('#connect').disabled, false);
  await f.reconnect(); const ready = f.element('#file-status').textContent;
  assert.match(ready, /^Files ready/);
  f.releaseFile(); await new Promise(resolve => setTimeout(resolve, 20));
  assert.equal(f.element('#file-status').textContent, ready);
  assert.equal(f.element('#upload-file-button').disabled, false);
}));
