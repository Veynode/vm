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
// Fixture note: a release requested before the delayed handler has reached its await is remembered (`*ReleasedEarly`);
// the poll that precedes a release returns as soon as the request is RECORDED, which can be before the handler (after an
// async signature check) has created the promise it waits on. Without this the release was a no-op and the test hung.
async function page(scopes, name, action) {
  const realSetTimeout = globalThis.setTimeout;
  const host = await DeviceIdentity.generate(), device = await DeviceIdentity.generate();
  const receiver = await AuthenticatedReceiver.fromIdentity(device, { pinnedHostPublicKey: host.publicKey, vmId: 'control-fixture', scopes });
  const grant = { host_id: host.id, device_id: device.id, device_public_key: device.publicKey, vm_id: 'control-fixture', scopes, version: 1 };
  const signedGrant = { grant, signature: await host.sign(await grantBytes(grant)) };
  const saved = new Map(); const replace = (key, value) => { saved.set(key, Object.getOwnPropertyDescriptor(globalThis, key)); Object.defineProperty(globalThis, key, { value, writable: true, configurable: true }); };
  let clock = 100, transcript, sequence = 0, frameCounter = 0, pendingInput, releaseInput;
  let delayFile = false, releaseFile, uploadDeclaration, delayClose = false, releaseClose, closeReleasedEarly = false, fileReleasedEarly = false; const fileBytes = new Uint8Array([1, 2, 3]);
  let slowFrames = 0, slowFrameMs = 0, holdNextDecode = false, decodeReached, releaseDecode;
  const unavailableFrames = []; let unavailableNext = null;
  let confirmed = false, denyNextInput = false, delayNextInput = false, offerCount = 0, failOffer = false, denyChallenge = false;
  const iceEnabled = name.startsWith('ice-');
  const calls = [], controls = [], peers = [], imageSources = [], graceTimers = [], frameTimers = [], intervals = [], pixelProbe = { reads: 0, posterDraws: 0, fill: 0 }; let latePlayReject, diagnosticDeadline, closeRejected = false;
  class Element {
    listeners = new Map(); disabled = false; hidden = false; checked = false; value = ''; textContent = ''; style = {}; srcObject = null;
    addEventListener(type, callback) { const list = this.listeners.get(type) ?? []; list.push(callback); this.listeners.set(type, list); }
    emit(type, event = {}) { return Promise.all((this.listeners.get(type) ?? []).map(callback => callback(event))); }
    click(event = {}) { return this.emit('click', event); }
    attributes = {}; setAttribute(key, value) { this.attributes[key] = value; }
    getContext() { return { drawImage(_, x, y, width) { if (width !== 16) pixelProbe.posterDraws++; }, getImageData() { pixelProbe.reads++; return { data: new Uint8Array(16 * 9 * 4).fill(pixelProbe.fill) }; } }; }
    getBoundingClientRect() { return { left: 50, top: 50, right: 550, bottom: 300, width: 500, height: 250 }; }
    requestVideoFrameCallback(callback) { this.videoCallback = callback; return 1; } cancelVideoFrameCallback() {}
    play() { return new Promise((_, reject) => { latePlayReject = reject; }); }
  }
  const nodes = new Map(); const element = id => { if (!nodes.has(id)) nodes.set(id, new Element()); return nodes.get(id); };
  // Time-bounded, not iteration-bounded: 200 polls of 5 ms was a one-second budget that a loaded host (parallel test files,
  // a build) could exceed with a step that was merely slow, which failed the test and, through the cleanup below, every
  // later test in this file. A genuinely stuck step still fails, after 20 s.
  const poll = async predicate => { const limit = Date.now() + 20000; for (;;) { if (predicate()) return; if (Date.now() > limit) break; await new Promise(resolve => setTimeout(resolve, 5)); } throw Error('Page did not settle: ' + element('#control-status').textContent); };
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
    set src(value) { imageSources.push(value); const bytes = Buffer.from(value.split(',')[1], 'base64'); if (value.startsWith('data:image/jpeg;')) { this.naturalWidth = name === 'jpeg-dimensions' || name === 'scaled-decoded' ? 999 : 1000; this.naturalHeight = 500; } else { this.naturalWidth = bytes.readUInt32BE(16); this.naturalHeight = bytes.readUInt32BE(20); } }
    async decode() { if (name === 'jpeg-decode') throw new Error('Synthetic decoder rejection'); if (holdNextDecode) { holdNextDecode = false; decodeReached?.(); await new Promise(resolve => { releaseDecode = resolve; }); } }
  }
  class Stream { constructor(tracks = []) { this.tracks = tracks; } getTracks() { return this.tracks; } }
  const handleControl = async body => {
    const now = Math.floor(Date.now() / 1000);
      assert.equal(confirmed, true); await verifyControl(body, transcript, device.publicKey, now);
      assert.ok(body.request.sequence > sequence); sequence = body.request.sequence; const op = body.request.action; controls.push(structuredClone(op)); let result;
      // The typed non-fencing reply of the media service: no picture this time. `unavailableNext` sends the same
      // reply to whatever request comes next, which the real service never does for anything but observe_frame.
      if (unavailableNext) { const reason = unavailableNext; unavailableNext = null; return { sequence: body.request.sequence, error: 'frame unavailable', reason }; }
      if (op.kind === 'observe_frame' && unavailableFrames.length) return { sequence: body.request.sequence, error: 'frame unavailable', reason: unavailableFrames.shift() };
      if (op.kind === 'file') {
        const a = op.operation;
        if (a.kind === 'upload_begin') { uploadDeclaration = a; result = {ok:true,value:{token:'a'.repeat(32),size:a.size,sha256:a.sha256,chunk_bytes:32768,idle_timeout_seconds:60}}; }
        else if (a.kind === 'upload_write') result = {ok:true,value:{offset:a.offset+Buffer.from(a.data,'base64').length}};
        else if (a.kind === 'upload_commit') { if(delayFile) { delayFile=false;await new Promise(r=>{ releaseFile=r; if (fileReleasedEarly) r(); }); } result={ok:true,value:{published:true,size:uploadDeclaration.size,sha256:uploadDeclaration.sha256}}; }
        else if (a.kind === 'cancel') result={ok:true,value:{cancelled:true}};
        else throw new Error('Unexpected file fixture op');
      } else if (op.kind === 'observe_frame') result = { frame_id: `00000000-0000-0000-0000-${String(++frameCounter).padStart(12, '0')}`, source_generation: 4, width: 1000, height: 500, coordinate_space: 'guest-physical-pixels', mime_type: 'image/png', byte_length: image.length, data_base64: image.toString('base64') };
      else { if (denyNextInput) { denyNextInput = false; return { sequence: body.request.sequence, error: 'control rejected' }; } if (delayNextInput) { delayNextInput = false; pendingInput?.(); await new Promise(resolve => { releaseInput = resolve; }); } result = { dispatched: true, steps: 1, guest_delivery_confirmed: false }; }
      if (op.kind === 'observe_frame' && name.startsWith('jpeg-')) {
        Object.assign(result, { mime_type: 'image/jpeg', byte_length: jpeg.length, data_base64: jpeg.toString('base64'), sha256: createHash('sha256').update(jpeg).digest('hex'), input_anchor_available: true });
        if (name === 'jpeg-hash') result.sha256 = '00'.repeat(32);
        if (name === 'jpeg-anchor') result.input_anchor_available = false;
        if (name === 'jpeg-generation') delete result.source_generation;
      }
      if (op.kind === 'observe_frame' && name.startsWith('scaled-')) {
        // A 2000x1000 Guest display shown through the real 1000x500 JPEG: integer divisor 2.
        Object.assign(result, { width: 2000, height: 1000, frame_width: 1000, frame_height: 500, scale_divisor: 2, mime_type: 'image/jpeg', byte_length: jpeg.length, data_base64: jpeg.toString('base64'), sha256: createHash('sha256').update(jpeg).digest('hex') });
        if (name === 'scaled-header') Object.assign(result, { width: 4000, height: 2000, frame_width: 2000, frame_height: 1000 });
        if (name === 'scaled-divisor') result.scale_divisor = 4;
        if (name === 'scaled-partial') delete result.frame_height;
      }
      // A slow host: the picture is answered only after this much of the page's clock has passed.
      if (op.kind === 'observe_frame' && slowFrames > 0) { slowFrames--; clock += slowFrameMs; }
      return { sequence: body.request.sequence, result };
  };
  const oldOpen = AuthenticatedReceiver.open;
  let connectTask;
  try {
    if (name.startsWith('automatic-')) { const timer = globalThis.setTimeout; replace('setTimeout', (callback, ms, ...args) => { if (ms === 120000) { diagnosticDeadline = callback; return timer(() => {}, 120000); } return timer(callback, ms, ...args); }); }
    if (name.startsWith('grace-')) { const timer = globalThis.setTimeout, clear = globalThis.clearTimeout; replace('setTimeout', (callback, ms, ...args) => { if (ms === 5000) { const entry = { callback, cancelled: false, id: timer(() => {}, 5000) }; graceTimers.push(entry); return entry.id; } return timer(callback, ms, ...args); }); replace('clearTimeout', id => { for (const entry of graceTimers) if (entry.id === id) entry.cancelled = true; return clear(id); }); }
    if (name.startsWith('keepfresh-')) { const timer = globalThis.setTimeout; replace('setTimeout', (callback, ms, ...args) => { if (ms === 4000) { frameTimers.push(callback); return timer(() => {}, 4000); } return timer(callback, ms, ...args); }); }
    if (name.startsWith('cadence-')) { const start = globalThis.setInterval, stop = globalThis.clearInterval; replace('setInterval', (callback, ms, ...args) => { const entry = { ms, callback, active: true, id: start(callback, ms, ...args) }; intervals.push(entry); return entry.id; }); replace('clearInterval', id => { for (const entry of intervals) if (entry.id === id) entry.active = false; return stop(id); }); }
    if (name === 'source-deadline') { const timer = globalThis.setTimeout; replace('setTimeout', (callback, ms, ...args) => timer(callback, ms === 12000 ? 120 : ms, ...args)); }
    const pageEvents = new Element(); replace('document', { querySelector: element, createElement: () => new Element(), visibilityState: 'visible', addEventListener: (type, callback) => pageEvents.addEventListener(type, callback), emit: (type, event) => pageEvents.emit(type, event) }); replace('window', new EventTarget()); replace('Image', Image);
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
      if (path === '/auth/close') { await verifyClose(body, transcript, device.publicKey); if (delayClose) { delayClose = false; await new Promise(resolve => { releaseClose = resolve; if (closeReleasedEarly) resolve(); }); } if (name === 'automatic-rejected' && !closeRejected) { closeRejected = true; return Response.json({ closed: false }); } return Response.json({ closed: true, ...(name.startsWith('source-') ? { media_failure: { code: name === 'source-asleep' ? 'guest_display_asleep' : 'guest_capture_no_frames' } } : {}) }); }
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
    const frames = () => controls.filter(control => control.kind === 'observe_frame').length;
    const refresh = async () => { const before = frames(); await element('#refresh-control-frame').click(); await poll(() => frames() > before && element('#control-status').textContent.startsWith('Screenshot ready') && !element('#refresh-control-frame').disabled); };
    // An input is followed by one automatic fresh frame; wait for it instead of clicking refresh.
    const inputSettled = async (kind, framesBefore) => { await poll(() => controls.some(control => control.kind === kind) && frames() > framesBefore && element('#control-status').textContent.startsWith('Screenshot ready') && !element('#refresh-control-frame').disabled); return controls.findLast(control => control.kind === kind); };
    const reconnect = async (fail = false) => { if (connectTask) await connectTask; confirmed = false; failOffer = fail === true; denyChallenge = fail === 'challenge'; connectTask = element('#connect').click(); if (fail) { await connectTask; return; } await poll(() => confirmed && !(name === 'files-only' ? element('#upload-file-button') : element('#refresh-control-frame')).disabled); };
    const startMedia = async () => { const peer = peers.at(-1); peer.connectionState = 'connected'; peer.onconnectionstatechange(); peer.mediaReady = true; peer.ontrack({ track: { stop() {} } }); const video = element('#received'); video.videoWidth = 1000; video.videoHeight = 500; for (let i = 0; i < 5; i++) video.videoCallback(); await poll(() => element('#status').textContent.startsWith('Receiving native')); };
    await action({ unavailable: (reason, count = 1) => { for (let i = 0; i < count; i++) unavailableFrames.push(reason); }, unavailableNext: reason => { unavailableNext = reason; }, slowFrames: (ms, count = 1) => { slowFrameMs = ms; slowFrames = count; }, holdDecode: () => { holdNextDecode = true; return new Promise(resolve => { decodeReached = resolve; }); }, releaseDecode: () => releaseDecode?.(), pixelProbe, frames, inputSettled, graceTimers, frameTimers, intervals, delayClose: () => { delayClose = true; closeReleasedEarly = false; }, releaseClose: () => { closeReleasedEarly = true; releaseClose?.(); }, fileBytes, delayFile: () => {delayFile=true; fileReleasedEarly = false;}, releaseFile: () => { fileReleasedEarly = true; releaseFile?.(); }, startMedia, expire: () => { assert.equal(typeof diagnosticDeadline, 'function'); diagnosticDeadline(); }, imageSources, reconnect, element, poll, refresh, controls, calls, peers, receiver, advance: ms => { clock += ms; }, deny: () => { denyNextInput = true; }, delay: () => { delayNextInput = true; return new Promise(resolve => { pendingInput = resolve; }); }, release: () => releaseInput?.(), rejectPlay: () => latePlayReject?.(new Error('late old playback')) });
  } finally {
    // The globals this fixture replaced are restored whatever happens below: an assertion or a hang in the cleanup used to
    // leave `document`, `window` and the timers replaced for every later test in the file (a failure then cascaded).
    try {
      releaseInput?.(); releaseFile?.(); releaseClose?.(); releaseDecode?.();
      // A cleanup step that never settles must fail this test, not stall the whole file (every later test then reports
      // as cancelled): bounded by the real timer captured before any replacement.
      const bounded = (promise, what) => Promise.race([promise, new Promise((_, reject) => realSetTimeout(() => reject(Error(`${what} did not settle within 15 s`)), 15000))]);
      await bounded(element('#disconnect').click(), 'disconnect');
      if (connectTask) await bounded(connectTask, 'connect');
      assert.equal(calls.includes('/auth/control'), false);
    } finally {
      receiver.dispose(); AuthenticatedReceiver.open = oldOpen;
      for (const [key, descriptor] of saved) { if (descriptor) Object.defineProperty(globalThis, key, descriptor); else delete globalThis[key]; }
    }
  }
}

test('observe-only page refuses key/pointer even with a fresh screenshot', async () => page(['observe'], 'observe', async f => {
  await f.refresh(); const count = f.controls.length;
  await f.element('#send-physical-key').click(); await f.element('#control-frame').click({ clientX: 300, clientY: 175 });
  assert.equal(f.controls.length, count); assert.equal(f.element('#send-physical-key').disabled, true);
}));

test('input uses displayed screenshot CSS mapping, fresh frame and physical-key allowlist', async () => page(['input', 'observe'], 'input', async f => {
  await f.refresh(); const before = f.controls.length; await f.element('#control-frame').click({ clientX: 300, clientY: 175 }); assert.equal(f.controls.length, before);
  await f.refresh();
  await f.element('#control-frame').click({ clientX: 550, clientY: 300 }); assert.equal(f.controls.at(-1).kind, 'observe_frame', 'Outside right/bottom boundary is rejected');
  let shown = f.frames(); await f.element('#control-frame').click({ clientX: 300, clientY: 175 }); const click = await f.inputSettled('click', shown);
  assert.deepEqual({ x: click.x, y: click.y }, { x: 500, y: 250 }); assert.equal(click.frame.width, 1000);
  assert.equal(f.frames(), shown + 1, 'Exactly one automatic refresh follows the input'); assert.equal(f.controls.at(-1).kind, 'observe_frame');
  assert.equal(f.element('#send-physical-key').disabled, false, 'The automatic frame is fresh and bound; no manual refresh is needed');
  await f.refresh(); f.advance(4001); const count = f.controls.length; await f.element('#send-physical-key').click(); assert.equal(f.controls.length, count); assert.match(f.element('#control-status').textContent, /expired/);
  await f.refresh(); f.element('#physical-key').value = 'arbitrary_text'; await f.element('#send-physical-key').click(); assert.equal(f.controls.at(-1).kind, 'observe_frame');
  f.element('#physical-key').value = 'ret'; shown = f.frames(); await f.element('#send-physical-key').click(); const key = await f.inputSettled('key', shown); assert.deepEqual(key.keys, ['ret']);
  assert.notEqual(key.frame.frame_id, click.frame.frame_id, 'Each input is bound to its own fresh frame');
  // No manual refresh: the drag uses the frame fetched automatically after the key.
  f.element('#pointer-action').value = 'drag'; const automatic = f.frames();
  await f.element('#control-frame').click({ clientX: 50, clientY: 50 }); assert.equal(f.controls.at(-1).kind, 'observe_frame');
  await f.element('#control-frame').click({ clientX: 549.9, clientY: 299.9 }); const drag = await f.inputSettled('drag', automatic);
  assert.deepEqual([drag.x, drag.y, drag.end_x, drag.end_y], [0, 0, 999, 499]);
  assert.equal(f.frames(), automatic + 1, 'The drag consumed the automatic frame and fetched exactly one more');
  await f.refresh(); f.element('#pointer-action').value = 'scroll-down'; shown = f.frames(); await f.element('#control-frame').click({ clientX: 300, clientY: 175 });
  const scroll = await f.inputSettled('scroll', shown); assert.equal(scroll.direction, 'down'); assert.equal(scroll.ticks, 3);
  assert.equal(f.element('#result').textContent.includes('data_base64'), false); assert.equal(f.element('#result').textContent.includes(image.toString('base64')), false);
}));

test('semantic denial fences the session; explicit reconnect requires fresh confirmation', async () => page(['input', 'observe'], 'denied', async f => {
  await f.refresh(); f.deny(); await f.element('#control-frame').click({ clientX: 300, clientY: 175 });
  await f.poll(() => f.calls.includes('/auth/close') && !f.element('#connect').disabled);
  const count = f.controls.length; await f.element('#control-frame').click({ clientX: 300, clientY: 175 }); await f.element('#refresh-control-frame').click();
  assert.equal(f.controls.length, count); assert.equal(f.element('#refresh-control-frame').disabled, true); assert.equal(f.peers[0].channel.readyState, 'closed');
  await f.reconnect(); assert.equal(f.calls.filter(x => x === '/auth/confirm').length, 2);
  await f.refresh(); assert.equal(f.controls.at(-1).kind, 'observe_frame');
}));

test('disconnect fences pending control before signed cleanup and never retries the input', async () => page(['input', 'observe'], 'pending', async f => {
  await f.refresh(); const reached = f.delay(); await f.element('#control-frame').click({ clientX: 300, clientY: 175 }); await reached;
  const count = f.controls.length, closing = f.element('#disconnect').click();
  await f.poll(() => f.peers[0].channel.readyState === 'closed');
  assert.equal(f.element('#refresh-control-frame').disabled, true); await f.element('#send-physical-key').click(); assert.equal(f.controls.length, count);
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
  await f.refresh();
  assert.ok(f.imageSources.at(-1).startsWith('data:image/jpeg;base64,'));
  const shown = f.frames(); await f.element('#control-frame').click({ clientX: 300, clientY: 175 });
  const click = await f.inputSettled('click', shown);
  assert.equal(click.x, 500); assert.equal(click.y, 250);
  assert.equal(f.element('#result').textContent.includes(jpeg.toString('base64')), false);
}));
for (const failure of ['hash', 'dimensions', 'decode', 'anchor', 'generation']) test(`JPEG ${failure} failure never enables screenshot input`, async () => page(['input', 'observe'], `jpeg-${failure}`, async f => {
  await f.element('#refresh-control-frame').click();
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
test('files-only page uploads without an input adapter', async()=>page(['files','observe'],'files-only',async f=>{
  f.element('#upload-file').files=[Object.assign(new Blob([f.fileBytes]),{name:'fixture.bin'})];
  await f.element('#upload-file-button').click();await f.poll(()=>f.element('#file-status').textContent.startsWith('Upload published'));
  assert.equal(f.controls.every(a=>a.kind==='file'),true);assert.equal(f.controls.filter(a=>a.kind==='file').length,3);
  assert.equal(f.element('#result').textContent.includes('token'),false);
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

test('scaled control frame is displayed at its own size and pointer input stays in source pixels', async () => page(['input', 'observe'], 'scaled-valid', async f => {
  await f.refresh();
  assert.ok(f.imageSources.at(-1).startsWith('data:image/jpeg;base64,'));
  const canvas = f.element('#control-frame');
  assert.deepEqual({ width: canvas.width, height: canvas.height, aspect: canvas.style.aspectRatio }, { width: 1000, height: 500, aspect: '1000 / 500' });
  // The displayed box is 500x250 CSS pixels at (50,50): its centre is source (1000,500), not frame (500,250).
  let shown = f.frames(); await f.element('#control-frame').click({ clientX: 300, clientY: 175 });
  let click = await f.inputSettled('click', shown);
  assert.deepEqual({ x: click.x, y: click.y, width: click.frame.width, height: click.frame.height }, { x: 1000, y: 500, width: 2000, height: 1000 });
  assert.deepEqual(Object.keys(click.frame).sort(), ['frame_id', 'height', 'source_generation', 'width'], 'The signed anchor carries source geometry only');
  // Corners: first displayed pixel and the last fraction before the right/bottom edge.
  shown = f.frames(); await f.element('#control-frame').click({ clientX: 50, clientY: 50 }); click = await f.inputSettled('click', shown);
  assert.deepEqual({ x: click.x, y: click.y }, { x: 0, y: 0 });
  const before = f.controls.filter(control => control.kind === 'click').length;
  shown = f.frames(); await f.element('#control-frame').click({ clientX: 549.99, clientY: 299.99 });
  await f.poll(() => f.controls.filter(control => control.kind === 'click').length === before + 1 && f.frames() > shown);
  click = f.controls.findLast(control => control.kind === 'click');
  assert.deepEqual({ x: click.x, y: click.y }, { x: 1999, y: 999 });
}));
for (const failure of ['header', 'divisor', 'partial', 'decoded']) test(`scaled control frame with inconsistent ${failure} never enables screenshot input`, async () => page(['input', 'observe'], `scaled-${failure}`, async f => {
  await f.element('#refresh-control-frame').click();
  await f.poll(() => f.element('#control-status').textContent.startsWith('Screenshot unavailable') || f.element('#connect').disabled === false);
  const count = f.controls.length;
  await f.element('#control-frame').click({ clientX: 300, clientY: 175 });
  await f.element('#send-physical-key').click();
  assert.equal(f.controls.length, count); assert.equal(f.element('#send-physical-key').disabled, true);
  // Declared scale against source geometry or against the JPEG header is refused before any decoder runs;
  // only a decoder that reports other dimensions gets as far as decoding.
  assert.equal(f.imageSources.length, failure === 'decoded' ? 1 : 0);
}));
test('input without a completed automatic frame stays fenced: a second click on the consumed screenshot sends nothing', async () => page(['input', 'observe'], 'auto-refresh-fence', async f => {
  await f.refresh(); const shown = f.frames(), reached = f.delay();
  await f.element('#control-frame').click({ clientX: 300, clientY: 175 }); await reached;
  const count = f.controls.length; await f.element('#control-frame').click({ clientX: 300, clientY: 175 }); await f.element('#send-physical-key').click();
  assert.equal(f.controls.length, count, 'Pending input admits no further input');
  f.release(); await f.inputSettled('click', shown);
  assert.equal(f.controls.filter(control => control.kind === 'click').length, 1);
  assert.equal(JSON.parse(f.element('#result').textContent).controlAction, 'observe_frame', 'The automatic refresh was a real request, not a no-op');
  f.advance(4001); const after = f.controls.length; await f.element('#send-physical-key').click();
  assert.equal(f.controls.length, after, 'The automatic frame expires like a manual one');
}));

test('transient disconnected keeps the live session; only 5 s without reconnect ends it', async () => page(['observe'], 'grace-transient', async f => {
  await f.startMedia(); const peer = f.peers.at(-1), report = () => JSON.parse(f.element('#result').textContent);
  const set = state => { peer.connectionState = state; peer.onconnectionstatechange(); };
  set('disconnected'); await new Promise(resolve => setTimeout(resolve, 30));
  assert.equal(f.graceTimers.length, 1); assert.equal(f.graceTimers[0].cancelled, false);
  assert.equal(report().status, 'passed-live'); assert.equal(f.calls.includes('/auth/close'), false); assert.equal(f.element('#disconnect').disabled, false);
  set('disconnected'); assert.equal(f.graceTimers.length, 1, 'A repeated disconnected never restarts the grace');
  set('connected'); assert.equal(f.graceTimers[0].cancelled, true, 'Reconnect cancels the timer');
  assert.deepEqual(report().iceAttempts.at(-1).phases.map(x => x.phase).filter(x => x.startsWith('disconnect-grace')), ['disconnect-grace-started', 'disconnect-grace-cancelled']);
  assert.equal(report().status, 'passed-live'); assert.equal(f.calls.includes('/auth/close'), false);
  set('disconnected'); assert.equal(f.graceTimers.length, 2); assert.equal(f.calls.includes('/auth/close'), false);
  f.graceTimers[1].callback();
  await f.poll(() => !f.element('#connect').disabled);
  assert.equal(report().status, 'stream-ended'); assert.match(report().error, /stayed disconnected for 5000ms/);
  assert.equal(f.calls.filter(path => path === '/auth/close').length, 1);
}));
test('failed during the disconnect grace ends at once and the stale grace timer cannot close again', async () => page(['observe'], 'grace-failed', async f => {
  await f.startMedia(); const peer = f.peers.at(-1);
  peer.connectionState = 'disconnected'; peer.onconnectionstatechange(); assert.equal(f.calls.includes('/auth/close'), false);
  peer.connectionState = 'failed'; peer.onconnectionstatechange();
  assert.equal(f.graceTimers[0].cancelled, true);
  await f.poll(() => !f.element('#connect').disabled);
  const before = f.element('#result').textContent; f.graceTimers[0].callback();
  assert.equal(f.element('#result').textContent, before); assert.equal(f.calls.filter(path => path === '/auth/close').length, 1);
}));
test('explicit disconnect during the grace cancels its timer', async () => page(['observe'], 'grace-closed', async f => {
  await f.startMedia(); const peer = f.peers.at(-1);
  peer.connectionState = 'disconnected'; peer.onconnectionstatechange();
  await f.element('#disconnect').click(); assert.equal(f.graceTimers[0].cancelled, true);
  assert.equal(JSON.parse(f.element('#result').textContent).status, 'disconnected');
}));

test('stats poll runs at 100 ms until success, then at 1000 ms, and stops at close', async () => page(['observe'], 'cadence-stats', async f => {
  const stats = () => f.intervals.filter(entry => entry.ms === 100 || entry.ms === 1000);
  await f.poll(() => stats().length === 1);
  assert.deepEqual(stats().map(entry => [entry.ms, entry.active]), [[100, true]]);
  await f.startMedia();
  assert.deepEqual(stats().map(entry => [entry.ms, entry.active]), [[100, false], [1000, true]]);
  const peer = f.peers.at(-1); let polls = 0; const getStats = peer.getStats.bind(peer); peer.getStats = async () => { polls++; return getStats(); };
  await new Promise(resolve => setTimeout(resolve, 450)); assert.equal(polls, 0, 'No 100 ms poll survives success');
  const latest = JSON.parse(f.element('#result').textContent).latestStats;
  assert.equal(latest.pixelChangeObserved, false); assert.equal(latest.pixelSample.width, 16); assert.equal(latest.distinctSampledPixelHashes.length, 1);
  await f.element('#disconnect').click();
  assert.deepEqual(stats().map(entry => [entry.ms, entry.active]), [[100, false], [1000, false]]);
}));

test('per-frame hashing stops after success once two distinct hashes exist; a static picture is sampled at most once per second', async () => page(['observe'], 'cadence-hash', async f => {
  const probe = f.pixelProbe, frame = () => f.element('#received').videoCallback();
  await f.startMedia();
  assert.equal(probe.reads, 5, 'Every frame is hashed until success'); assert.equal(probe.posterDraws, 5, 'Every frame is copied until success');
  frame(); frame(); assert.equal(probe.reads, 5, 'Static picture: no further sample within one second');
  f.advance(999); frame(); assert.equal(probe.reads, 5);
  f.advance(1); probe.fill = 7; frame(); assert.equal(probe.reads, 6, 'One sample once a second has passed');
  f.advance(5000); frame(); frame(); assert.equal(probe.reads, 6, 'Two distinct hashes collected: hashing stopped');
  assert.equal(probe.posterDraws, 5, 'No full-resolution poster copy per frame after success');
  // The next 1000 ms poll publishes the completed evidence; no asserted field is lost.
  f.intervals.find(entry => entry.ms === 1000 && entry.active).callback(); await f.poll(() => JSON.parse(f.element('#result').textContent).latestStats.pixelSamplingComplete === true);
  const latest = JSON.parse(f.element('#result').textContent).latestStats;
  assert.equal(latest.pixelChangeObserved, true); assert.equal(latest.distinctSampledPixelHashes.length, 2); assert.equal(latest.presentedFrames, 11);
  await f.element('#disconnect').click(); assert.equal(probe.posterDraws, 6, 'The final frame is copied once at close');
}));

test('video receiver gets minimal jitter-buffer hints only where the browser exposes them', async () => page(['observe'], 'latency-hints', async f => {
  const peer = f.peers.at(-1), report = () => JSON.parse(f.element('#result').textContent).receiverLatencyHints;
  const modern = { jitterBufferTarget: null }; peer.ontrack({ track: { stop() {} }, receiver: modern });
  assert.equal(modern.jitterBufferTarget, 0); assert.equal('playoutDelayHint' in modern, false);
  assert.deepEqual(report(), { requestedValue: 0, applied: ['jitterBufferTarget'] });
  const both = { jitterBufferTarget: null, playoutDelayHint: null }; peer.ontrack({ track: { stop() {} }, receiver: both });
  assert.deepEqual(both, { jitterBufferTarget: 0, playoutDelayHint: 0 }); assert.deepEqual(report().applied, ['jitterBufferTarget', 'playoutDelayHint']);
  const legacy = {}; peer.ontrack({ track: { stop() {} }, receiver: legacy });
  assert.deepEqual(legacy, {}); assert.deepEqual(report().applied, []);
  const frozen = Object.freeze({ jitterBufferTarget: null }); peer.ontrack({ track: { stop() {} }, receiver: frozen });
  assert.deepEqual(report().applied, [], 'A rejected assignment is not reported as applied');
  peer.ontrack({ track: { stop() {} } }); assert.deepEqual(report().applied, []);
}));

// ---- Direct interaction on the control picture --------------------------------------------------------------
// Same synthetic DOM, crypto and DataChannel as above: these prove the page's event, ordering and refusal logic,
// not a real browser's pointer/keyboard events, a real decoder, or Guest delivery.
function direct(f) {
  const surface = f.element('#control-frame'), notice = () => f.element('#input-notice').textContent, state = () => f.element('#session-state').textContent;
  const inputs = () => f.controls.filter(control => ['click', 'drag', 'scroll', 'key'].includes(control.kind));
  let prevented = 0; const event = fields => ({ button: 0, pointerId: 1, preventDefault() { prevented++; }, ...fields });
  return {
    surface, notice, state, inputs, prevented: () => prevented,
    // Refresh first, then switch: the starting picture is then deterministic.
    enable: async () => { await f.refresh(); f.element('#direct-control').checked = true; await f.element('#direct-control').emit('change'); },
    down: (x, y, extra = {}) => surface.emit('pointerdown', event({ clientX: x, clientY: y, ...extra })),
    up: (x, y, extra = {}) => surface.emit('pointerup', event({ clientX: x, clientY: y, ...extra })),
    tap(x, y, extra = {}) { this.down(x, y, extra); this.up(x, y, extra); },
    key: (code, extra = {}) => surface.emit('keydown', event({ code, key: code, ...extra })),
    keyUp: (code, extra = {}) => surface.emit('keyup', event({ code, key: code, ...extra })),
    wheel: (x, y, deltaY, extra = {}) => surface.emit('wheel', event({ clientX: x, clientY: y, deltaX: 0, deltaY, deltaMode: 0, ...extra })),
    // Every input sent and its automatic picture fetched, nothing waiting.
    settled: count => f.poll(() => inputs().length === count && f.controls.at(-1).kind === 'observe_frame' && f.element('#control-status').textContent.startsWith('Screenshot ready') && !/^Input pending/.test(state())),
    quiet: () => new Promise(resolve => setTimeout(resolve, 25)),
  };
}
// The fixture numbers its frames 1, 2, 3…: an input must carry the id of the observe_frame directly before it.
function assertEachBoundToPrecedingFrame(controls, allowShared = false) {
  let observed = 0, usedBy = null;
  for (const control of controls) {
    if (control.kind === 'observe_frame') { observed++; usedBy = null; continue; }
    if (!control.frame) continue;
    assert.equal(control.frame.frame_id, `00000000-0000-0000-0000-${String(observed).padStart(12, '0')}`, `${control.kind} is bound to the current frame`);
    if (!allowShared) assert.equal(usedBy, null, `${control.kind} does not share a frame with ${usedBy}`);
    usedBy = control.kind;
  }
}

test('direct input: inputs arriving while one is in flight are sent strictly in order, one at a time, each on its own fresh frame', async () => page(['input', 'observe'], 'direct-order', async f => {
  const d = direct(f); await d.enable();
  const reached = f.delay(); d.tap(300, 175); await reached;
  const count = f.controls.length;
  d.key('KeyA'); d.key('KeyB'); d.tap(100, 100); d.key('Enter');
  await d.quiet();
  assert.equal(f.controls.length, count, 'Nothing else reaches the host while one input is in flight');
  assert.match(d.state(), /^Input pending: 1 sending, 4 waiting\./);
  assert.equal(f.element('#session-state').attributes['data-state'], 'input-pending');
  f.release(); await d.settled(5);
  assert.deepEqual(d.inputs().map(input => input.kind === 'key' ? input.keys.join('+') : `${input.kind}@${input.x},${input.y}`), ['click@500,250', 'a', 'b', 'click@100,100', 'ret']);
  assert.deepEqual(f.controls.slice(count - 1).map(control => control.kind), ['click', 'observe_frame', 'key', 'observe_frame', 'key', 'observe_frame', 'click', 'observe_frame', 'key', 'observe_frame']);
  assertEachBoundToPrecedingFrame(f.controls);
  const report = JSON.parse(f.element('#result').textContent);
  assert.deepEqual({ queued: report.controlQueue.queued, sent: report.controlQueue.sent, waiting: report.controlQueue.waiting, refused: report.controlQueue.refused, discarded: report.controlQueue.discarded }, { queued: 5, sent: 5, waiting: 0, refused: 0, discarded: 0 });
  assert.equal(report.guestDeliveryConfirmed, false);
  assert.equal(d.notice(), '');
}));

test('direct input: the queue is bounded; a key or click that does not fit is refused visibly and never sent', async () => page(['input', 'observe'], 'direct-bounded', async f => {
  const d = direct(f); await d.enable();
  const reached = f.delay(); d.tap(300, 175); await reached;
  for (const letter of 'ABCDEFGH') d.key(`Key${letter}`);
  assert.match(d.state(), /^Input pending: 1 sending, 8 waiting \(queue full at 8; further input is refused\)\./);
  assert.equal(d.notice(), '');
  d.key('KeyI');
  assert.match(d.notice(), /input queue is full \(8 waiting\)\. This key was NOT sent/);
  d.tap(120, 120); assert.match(d.notice(), /This click was NOT sent/);
  d.wheel(300, 175, 300); assert.match(d.notice(), /This scroll was NOT sent/);
  // Auto-repeat of the chord already waiting at the tail is merged into it, not queued and not refused.
  const refusedBefore = JSON.parse(f.element('#result').textContent).controlQueue.refused;
  d.key('KeyH', { repeat: true }); d.key('KeyH', { repeat: true });
  assert.equal(JSON.parse(f.element('#result').textContent).controlQueue.refused, refusedBefore);
  assert.equal(JSON.parse(f.element('#result').textContent).controlQueue.merged, 2);
  f.release(); await d.settled(9);
  assert.deepEqual(d.inputs().map(input => input.kind === 'key' ? input.keys[0] : input.kind), ['click', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h']);
  assert.equal(d.inputs().some(input => input.keys?.includes('i') || input.kind === 'scroll' || input.x === 140), false, 'Refused inputs are never sent later');
  assertEachBoundToPrecedingFrame(f.controls);
  // Once it has drained, input is accepted again and the refusal notice is cleared.
  d.key('KeyI'); await d.settled(10); assert.deepEqual(d.inputs().at(-1).keys, ['i']); assert.equal(d.notice(), '');
}));

test('direct input: wheel ticks merge into the waiting scroll of the same direction and point; a new direction is a new action', async () => page(['input', 'observe'], 'direct-wheel', async f => {
  const d = direct(f); await d.enable();
  const reached = f.delay(); d.tap(100, 100); await reached;
  d.wheel(300, 175, 100); d.wheel(300, 175, 100); d.wheel(300, 175, 40); d.wheel(300, 175, 60);
  assert.match(d.state(), /1 sending, 1 waiting/);
  d.wheel(300, 175, -120); assert.match(d.state(), /1 sending, 2 waiting/);
  d.wheel(300, 200, -100, { deltaMode: 0 }); assert.match(d.state(), /1 sending, 3 waiting/, 'Another point is another action');
  f.release(); await d.settled(4);
  assert.deepEqual(d.inputs().slice(1).map(input => [input.direction, input.ticks, input.x, input.y]), [['down', 3, 500, 250], ['up', 1, 500, 250], ['up', 1, 500, 300]]);
  // Wheel on a letterbox-free picture but with Ctrl (browser zoom gesture) is refused, not sent as a plain scroll.
  d.wheel(300, 175, 100, { ctrlKey: true }); assert.match(d.notice(), /held with the pointer cannot be sent/);
  await d.quiet(); assert.equal(d.inputs().length, 4);
}));

test('direct input: blur discards waiting input and held modifiers visibly; the in-flight input is neither recalled nor retried', async () => page(['input', 'observe'], 'direct-blur', async f => {
  const d = direct(f); await d.enable(); d.surface.emit('focus');
  const reached = f.delay(); d.tap(300, 175); await reached;
  d.key('ShiftLeft', { shiftKey: true }); d.key('KeyA', { shiftKey: true }); d.key('KeyB', { shiftKey: true });
  assert.match(d.state(), /1 sending, 2 waiting/);
  d.surface.emit('blur');
  assert.match(d.notice(), /^2 waiting inputs were discarded and NOT sent: keyboard focus left the picture\./);
  assert.match(d.state(), /1 sending, 0 waiting/);
  f.release(); await d.settled(1);
  assert.deepEqual(d.inputs().map(input => input.kind), ['click'], 'Discarded keys are never sent; the click is sent exactly once');
  // Shift was down at blur and its release was never seen: the next chord must not inherit it.
  d.key('KeyC'); await d.settled(2); assert.deepEqual(d.inputs().at(-1).keys, ['c']);
  // The same release on window blur and on the page becoming hidden.
  let held = f.delay(); d.tap(100, 100); await held; d.key('KeyD'); globalThis.window.dispatchEvent(new Event('blur'));
  assert.match(d.notice(), /^1 waiting input was discarded and NOT sent: the window lost focus\./);
  f.release(); await d.settled(3);
  held = f.delay(); d.tap(200, 200); await held; d.key('KeyE'); d.key('KeyF');
  globalThis.document.visibilityState = 'hidden'; globalThis.document.emit('visibilitychange');
  assert.match(d.notice(), /^2 waiting inputs were discarded and NOT sent: the page was hidden\./);
  globalThis.document.visibilityState = 'visible'; f.release(); await d.settled(4);
  assert.equal(d.inputs().filter(input => input.kind === 'key').length, 1);
  assert.equal(JSON.parse(f.element('#result').textContent).controlQueue.discarded, 5);
}));

test('direct input: disconnect discards waiting input and nothing is sent afterwards', async () => page(['input', 'observe'], 'direct-disconnect', async f => {
  const d = direct(f); await d.enable();
  const reached = f.delay(); d.tap(300, 175); await reached; d.key('KeyA'); d.key('KeyB');
  const count = f.controls.length, closing = f.element('#disconnect').click();
  await f.poll(() => f.peers[0].channel.readyState === 'closed');
  assert.match(d.notice(), /^2 waiting inputs were discarded and NOT sent: control ended\./);
  assert.match(d.state(), /^Ended: you disconnected\./);
  d.key('KeyC'); d.tap(300, 175);
  f.release(); await closing; await d.quiet();
  assert.equal(f.controls.length, count); assert.equal(d.inputs().length, 1);
}));

test('direct input: letterbox and pillarbox bars are ignored; picture corners map to the source corners', async () => page(['input', 'observe'], 'direct-letterbox', async f => {
  const d = direct(f); await d.enable();
  // An 800x600 element showing the 1000x500 picture with object-fit: contain: 800x400 painted at y 100..500.
  d.surface.getBoundingClientRect = () => ({ left: 0, top: 0, right: 800, bottom: 600, width: 800, height: 600 });
  const count = f.controls.length;
  d.tap(400, 50); d.tap(400, 99.99); d.tap(400, 500); d.tap(400, 599); d.wheel(400, 50, 300);
  await d.quiet(); assert.equal(f.controls.length, count, 'Bars are not Guest pixels'); assert.equal(d.notice(), '');
  d.tap(0, 100); d.tap(799.99, 499.99); d.tap(400, 300); await d.settled(3);
  assert.deepEqual(d.inputs().map(input => [input.x, input.y]), [[0, 0], [999, 499], [500, 250]]);
  // A press that starts on a bar starts nothing; a drag that ends on a bar or outside is pinned to the edge.
  d.down(400, 50); d.up(400, 300); await d.quiet(); assert.equal(d.inputs().length, 3);
  d.down(400, 300); d.up(900, 700); await d.settled(4);
  const drag = d.inputs().at(-1); assert.deepEqual([drag.kind, drag.x, drag.y, drag.end_x, drag.end_y, drag.button], ['drag', 500, 250, 999, 499, 'left']);
  d.down(400, 300, { button: 2 }); d.up(-50, 20, { button: 2 }); await d.settled(5);
  const back = d.inputs().at(-1); assert.deepEqual([back.x, back.y, back.end_x, back.end_y, back.button], [500, 250, 0, 0, 'right']);
  // Pillarbox, at a fractional position as produced by devicePixelRatio 1.5 or page zoom: 1200x300 -> 600x300 at x 300.5.
  d.surface.getBoundingClientRect = () => ({ left: 0.5, top: 10.25, right: 1200.5, bottom: 310.25, width: 1200, height: 300 });
  d.tap(300.4, 150); d.tap(900.6, 150); await d.quiet(); assert.equal(d.inputs().length, 5);
  d.tap(300.5, 10.25); d.tap(900.49, 310.24); d.tap(600.5, 160.25); await d.settled(8);
  assert.deepEqual(d.inputs().slice(5).map(input => [input.x, input.y]), [[0, 0], [999, 499], [500, 250]]);
  // The step-by-step click path uses the same rule.
  f.element('#direct-control').checked = false; await f.element('#direct-control').emit('change');
  const before = f.controls.length; await d.surface.click({ clientX: 100, clientY: 150 }); assert.equal(f.controls.length, before, 'Legacy click on a pillarbox bar sends nothing');
  const shown = f.frames(); await d.surface.click({ clientX: 900.49, clientY: 10.25 }); const click = await f.inputSettled('click', shown);
  assert.equal(f.controls.filter(control => control.kind === 'click').length, 7); assert.deepEqual([click.x, click.y], [999, 0]);
}));

test('direct input on a scaled frame: bars ignored, corners map to SOURCE corners, anchor stays source geometry', async () => page(['input', 'observe'], 'scaled-direct', async f => {
  const d = direct(f); await d.enable();
  d.surface.getBoundingClientRect = () => ({ left: 0, top: 0, right: 800, bottom: 600, width: 800, height: 600 });
  d.tap(400, 99); await d.quiet(); assert.equal(d.inputs().length, 0);
  d.tap(0, 100); d.tap(799.99, 499.99); d.tap(400, 300); await d.settled(3);
  assert.deepEqual(d.inputs().map(input => [input.x, input.y, input.frame.width, input.frame.height]), [[0, 0, 2000, 1000], [1999, 999, 2000, 1000], [1000, 500, 2000, 1000]]);
}));

test('direct input: a picture older than 4 seconds refuses pointer and key input, says so, and fetches a fresh one', async () => page(['input', 'observe'], 'direct-stale', async f => {
  const d = direct(f); await d.enable();
  f.advance(4001); const frames = f.frames();
  d.tap(300, 175);
  assert.match(d.notice(), /picture was older than 4 seconds, so this input was NOT sent/);
  assert.match(d.state(), /^Picture stale/); assert.equal(f.element('#session-state').attributes['data-state'], 'frame-stale');
  await f.poll(() => f.frames() === frames + 1 && f.element('#control-status').textContent.startsWith('Screenshot ready'));
  assert.equal(d.inputs().length, 0, 'The refused click is not replayed on the fresh picture');
  f.advance(4001); d.key('KeyA'); assert.match(d.notice(), /NOT sent/);
  await f.poll(() => f.frames() === frames + 2 && f.element('#control-status').textContent.startsWith('Screenshot ready'));
  assert.equal(d.inputs().length, 0);
  d.tap(300, 175); await d.settled(1); assert.equal(d.notice(), ''); assert.doesNotMatch(d.state(), /stale/);
  // An input that waited too long behind a slow one is refused, visibly, instead of landing late.
  const reached = f.delay(); d.tap(100, 100); await reached; d.key('KeyB'); f.advance(6001); f.release();
  await f.poll(() => /waiting input was discarded and NOT sent: they had waited more than 6 seconds/.test(d.notice()));
  await d.quiet(); assert.equal(d.inputs().filter(input => input.kind === 'key').length, 0);
}));

test('visible session state: connecting, live, input pending, picture stale, reconnecting (grace) and ended with its reason', async () => page(['input', 'observe'], 'grace-states', async f => {
  const d = direct(f), tag = () => f.element('#session-state').attributes['data-state'];
  // Input is allowed from the start, so with no fresh picture yet the input state outranks connecting and live.
  assert.match(d.state(), /^Picture stale/); assert.equal(tag(), 'frame-stale');
  await f.startMedia(); assert.equal(tag(), 'frame-stale');
  await d.enable(); assert.match(d.state(), /^Live\. Input enabled\./);
  const reached = f.delay(); d.tap(300, 175); await reached;
  assert.match(d.state(), /^Input pending: 1 sending, 0 waiting\./); assert.equal(tag(), 'input-pending');
  f.release(); await d.settled(1); assert.equal(tag(), 'live');
  f.advance(4001); d.tap(300, 175); assert.equal(tag(), 'frame-stale');
  await f.poll(() => tag() === 'live');
  const peer = f.peers.at(-1), set = value => { peer.connectionState = value; peer.onconnectionstatechange(); };
  const held = f.delay(); d.tap(100, 100); await held; d.key('KeyA');
  set('disconnected');
  assert.match(d.state(), /^Reconnecting: the connection dropped; waiting up to 5 seconds\. Input is refused/); assert.equal(tag(), 'reconnecting');
  assert.match(d.notice(), /^1 waiting input was discarded and NOT sent: the connection dropped\./);
  const count = f.controls.length; d.tap(300, 175); d.key('KeyB');
  assert.match(d.notice(), /connection is recovering\. Nothing was sent/); assert.equal(f.controls.length, count);
  set('connected'); f.release(); await d.settled(2); assert.equal(tag(), 'live');
  assert.equal(d.inputs().some(input => input.kind === 'key'), false);
  set('disconnected'); assert.equal(tag(), 'reconnecting'); f.graceTimers.at(-1).callback();
  assert.equal(tag(), 'ended'); assert.match(d.state(), /^Ended: Native WebRTC transport stayed disconnected for 5000ms\./);
  await f.poll(() => !f.element('#connect').disabled);
  const after = f.controls.length; d.tap(300, 175); d.key('KeyC'); await d.quiet(); assert.equal(f.controls.length, after);
  assert.equal(tag(), 'ended', 'The reason stays visible after cleanup');
  await f.reconnect(); assert.equal(tag(), 'frame-stale');
  await f.element('#disconnect').click(); assert.match(d.state(), /^Ended: you disconnected\./);
}));

test('visible session state: the diagnostic session limit and a failed connection are shown as the reason for ending', async () => page(['observe'], 'automatic-state', async f => {
  const state = () => f.element('#session-state').textContent;
  await f.startMedia(); assert.match(state(), /^Live/);
  f.expire(); assert.match(state(), /^Ended: the 120-second diagnostic session limit was reached\./);
  await f.poll(() => !f.element('#connect').disabled);
  await f.reconnect('challenge'); assert.match(state(), /^Ended: the connection could not be established/);
}));

test('direct input: double-click is two clicks on the one picture the user double-clicked, with no screenshot between them', async () => page(['input', 'observe'], 'direct-double', async f => {
  const d = direct(f); await d.enable();
  const reached = f.delay(); d.tap(300, 175); await reached; d.tap(301, 176);
  f.release(); await d.settled(2);
  const [first, second] = d.inputs();
  assert.deepEqual(f.controls.slice(-3).map(control => control.kind), ['click', 'click', 'observe_frame']);
  assert.equal(second.frame.frame_id, first.frame.frame_id); assert.deepEqual([second.x, second.y], [first.x, first.y].map((value, index) => value + [2, 2][index]));
  assertEachBoundToPrecedingFrame(f.controls, true);
  // Clicks at different places, or with another button, are separate clicks with a picture between them.
  f.advance(1000); const held = f.delay(); d.tap(100, 100); await held; d.tap(400, 200); f.release(); await d.settled(4);
  assert.deepEqual(f.controls.slice(-4).map(control => control.kind), ['click', 'observe_frame', 'click', 'observe_frame']);
  // The same place after the double-click interval is a new single click.
  f.advance(501); d.tap(400, 200); await d.settled(5); f.advance(501);
  const again = f.delay(); d.tap(400, 200); await again; d.tap(400, 200, { button: 2 }); f.release(); await d.settled(7);
  assert.deepEqual(f.controls.slice(-4).map(control => [control.kind, control.button]), [['click', 'left'], ['observe_frame', undefined], ['click', 'right'], ['observe_frame', undefined]]);
}));

test('direct input keyboard: physical chords, lone modifier tap, unavailable keys and composition are explicit; Tab is only captured when sendable', async () => page(['input', 'observe'], 'direct-keys', async f => {
  const d = direct(f);
  // Not yet in direct mode: nothing is captured and nothing is sent.
  d.key('Tab'); d.tap(300, 175); assert.equal(d.prevented(), 0);
  await d.enable(); const count = f.controls.length;
  await d.surface.click({ clientX: 300, clientY: 175 }); await f.element('#send-physical-key').click();
  assert.equal(f.controls.length, count, 'The step-by-step click and key button are inert in direct mode'); assert.equal(f.element('#send-physical-key').disabled, true);
  d.key('ControlLeft', { ctrlKey: true }); d.key('KeyC', { ctrlKey: true }); d.keyUp('ControlLeft');
  d.key('Tab'); d.key('ShiftRight', { shiftKey: true }); d.key('Digit1', { shiftKey: true }); d.keyUp('ShiftRight');
  await d.settled(3);
  assert.deepEqual(d.inputs().map(input => input.keys), [['ctrl', 'c'], ['tab'], ['shift_r', '1']], 'A modifier used in a chord is not sent again as a tap');
  assert.equal(d.prevented(), 5, 'Every handled key press (two modifiers, three chords) is taken from the page');
  d.key('MetaLeft', { metaKey: true }); d.keyUp('MetaLeft'); await d.settled(4); assert.deepEqual(d.inputs().at(-1).keys, ['meta_l']);
  // A modifier flag whose key press was never seen (focus arrived with it held) uses the left key.
  d.key('KeyV', { ctrlKey: true, shiftKey: false, altKey: false, metaKey: false }); await d.settled(5); assert.deepEqual(d.inputs().at(-1).keys, ['ctrl', 'v']);
  const before = d.prevented();
  d.key('CapsLock'); assert.match(d.notice(), /The key CapsLock is not available on this connection/);
  d.key('Numpad1'); assert.match(d.notice(), /Numpad1 is not available/);
  d.key('KeyA', { isComposing: true }); assert.match(d.notice(), /Text composition \(IME\) cannot be sent/);
  assert.equal(d.prevented(), before, 'A refused key is left to the browser');
  d.tap(300, 175, { shiftKey: true }); assert.match(d.notice(), /held with the pointer cannot be sent/);
  // Ctrl held, then a pointer press: the Ctrl release afterwards is not a tap.
  d.key('ControlLeft', { ctrlKey: true }); d.tap(300, 175, { ctrlKey: true }); d.keyUp('ControlLeft');
  await d.quiet(); assert.equal(d.inputs().length, 5);
  // Ctrl+Alt+Escape leaves the surface and is not sent.
  d.key('Escape', { ctrlKey: true, altKey: true }); await d.quiet(); assert.equal(d.inputs().length, 5);
  d.key('Escape'); await d.settled(6); assert.deepEqual(d.inputs().at(-1).keys, ['esc']);
  for (const input of d.inputs()) for (const key of input.keys) assert.match(key, /^[a-z0-9_]{1,16}$/);
}));

test('direct input: the pointer handlers send nothing in step-by-step mode or without input permission', async () => page(['observe'], 'direct-observe', async f => {
  const d = direct(f); await f.refresh(); f.element('#direct-control').checked = true; await f.element('#direct-control').emit('change');
  const count = f.controls.length; d.tap(300, 175); d.key('KeyA'); d.wheel(300, 175, 300); await d.quiet();
  assert.equal(f.controls.length, count); assert.equal(d.prevented(), 0);
}));
test('direct input: a rejected input fences control and discards what was waiting; nothing is retried', async () => page(['input', 'observe'], 'direct-denied', async f => {
  const d = direct(f); await d.enable();
  f.deny(); d.tap(300, 175); d.key('KeyA'); d.key('KeyB');
  await f.poll(() => f.calls.includes('/auth/close') && !f.element('#connect').disabled);
  assert.deepEqual(d.inputs().map(input => input.kind), ['click']);
  assert.match(d.notice(), /^2 waiting inputs were discarded and NOT sent: control ended\./);
}));

test('direct input: the picture is kept fresh only while the surface has focus, control is held and the page is visible', async () => page(['input', 'observe'], 'keepfresh-focus', async f => {
  const d = direct(f); await d.enable(); const settle = count => f.poll(() => f.frames() === count && f.element('#control-status').textContent.startsWith('Screenshot ready'));
  let frames = f.frames();
  f.frameTimers.at(-1)(); await d.quiet(); assert.equal(f.frames(), frames, 'Not focused: an expired picture is not refetched');
  await f.refresh(); frames = f.frames(); d.surface.emit('focus');
  f.advance(4000); f.frameTimers.at(-1)(); await settle(frames + 1);
  globalThis.document.visibilityState = 'hidden'; f.advance(4000); f.frameTimers.at(-1)(); await d.quiet(); assert.equal(f.frames(), frames + 1, 'Hidden page: no refetch');
  globalThis.document.visibilityState = 'visible'; await f.refresh(); frames = f.frames();
  d.surface.emit('blur'); f.advance(4000); f.frameTimers.at(-1)(); await d.quiet(); assert.equal(f.frames(), frames, 'Blurred: no refetch');
  // Focusing a surface whose picture has gone stale fetches one.
  d.surface.emit('focus'); await settle(frames + 1);
}));

// ---- Slow control picture, single fetch, refusal counting and picture diagnostics ----------------------------
// Same synthetic page as above. The fixture's clock stands still unless a test advances it, so "slow" is exact.
const reportOf = f => JSON.parse(f.element('#result').textContent);

test('slow control picture (step-by-step): a picture that took over 4 s is not shown and anchors nothing, and the next picture is usable', async () => page(['input', 'observe'], 'slow-stepwise', async f => {
  await f.refresh();
  const frames = f.frames(), sources = f.imageSources.length; f.slowFrames(4001);
  await f.element('#refresh-control-frame').click();
  await f.poll(() => f.frames() === frames + 1 && /^The control picture took 4\.0 s to refresh/.test(f.element('#control-status').textContent) && !f.element('#refresh-control-frame').disabled);
  assert.equal(f.element('#control-status').textContent, 'The control picture took 4.0 s to refresh, longer than the 4 seconds a picture may be used for, so it was not shown. Input is paused. Press Refresh screenshot to try again.');
  assert.equal(f.imageSources.length, sources + 1, 'The late picture was verified and decoded, then discarded');
 
  // The freshness bound did not move: the late picture anchors no input.
  assert.equal(f.element('#send-physical-key').disabled, true); assert.equal(f.element('#control-frame').attributes['aria-disabled'], 'true');
  const count = f.controls.length; await f.element('#send-physical-key').click(); await f.element('#control-frame').click({ clientX: 300, clientY: 175 });
  assert.equal(f.controls.length, count, 'Nothing is sent against a picture that arrived too late');
  let report = reportOf(f);
  assert.deepEqual(report.controlFrameRefresh, { shown: 1, expired: 1, consecutiveExpired: 1, joined: 0, lastExpiredMs: 4001, unavailable: 0, consecutiveUnavailable: 0, lastUnavailableReason: null, freshForMs: 4000 });
  assert.equal(report.controlFrame.usable, false); assert.equal(report.controlFrame.requestToReadyMs, 4001);
  assert.equal(report.controlError, null, 'A late picture is not a control failure');
  // One picture in time and input works again.
  await f.refresh(); const shown = f.frames(); await f.element('#send-physical-key').click(); const key = await f.inputSettled('key', shown);
 
  report = reportOf(f); assert.equal(report.controlFrameRefresh.consecutiveExpired, 0); assert.equal(report.controlFrameRefresh.expired, 1); assert.equal(report.controlFrame.usable, true);
  // Under the bound is still a usable picture, exactly as before, for what is left of its 4 seconds.
  f.slowFrames(3000); await f.refresh(); assert.equal(f.element('#send-physical-key').disabled, false); assert.equal(reportOf(f).controlFrame.requestToReadyMs, 3000);
}));

test('slow control picture (direct): input is paused with a specific notice, refetching is bounded, and a picture in time resumes input', async () => page(['input', 'observe'], 'slow-direct', async f => {
  const d = direct(f); await d.enable(); d.surface.emit('focus');
  // Two late pictures in a row while the user is on the surface: one automatic retry, then it waits for the user.
  let frames = f.frames(); f.slowFrames(4500, 2);
  await f.element('#refresh-control-frame').click();
  await f.poll(() => f.frames() === frames + 2 && /Press Refresh screenshot to try again\.$/.test(f.element('#control-status').textContent) && !f.element('#refresh-control-frame').disabled);
  assert.match(f.element('#control-status').textContent, /^The control picture took 4\.5 s to refresh, longer than the 4 seconds a picture may be used for, so it was not shown\. Input is paused\./);
  await d.quiet(); assert.equal(f.frames(), frames + 2, 'No fetch loop: after one retry nothing more is requested without a user action');
  assert.deepEqual(f.controls.slice(-2).map(control => control.kind), ['observe_frame', 'observe_frame']);
  assert.match(d.state(), /^Picture stale/); assert.equal(f.element('#session-state').attributes['data-state'], 'frame-stale');
 
  assert.equal(reportOf(f).controlFrameRefresh.consecutiveExpired, 2); assert.equal(reportOf(f).controlFrameRefresh.expired, 2);
  // Input meanwhile is refused for the unchanged reason and is not replayed; it starts one fetch.
  frames = f.frames(); d.key('KeyA');
  assert.match(d.notice(), /picture was older than 4 seconds, so this input was NOT sent/);
  await f.poll(() => f.frames() === frames + 1 && f.element('#control-status').textContent.startsWith('Screenshot ready'));
  assert.equal(d.inputs().length, 0);
  // The picture arrived in time: input is sent.
  d.key('KeyB'); await d.settled(1);
  assert.deepEqual(d.inputs()[0].keys, ['b']);
  assert.equal(reportOf(f).controlFrameRefresh.consecutiveExpired, 0);
  // Late pictures after an input: the input that was waiting is discarded visibly by the unchanged 6 s rule,
  // never sent late, and control is kept.
  const held = f.delay(); d.tap(300, 175); await held; d.key('KeyC'); f.slowFrames(4500, 2); f.release();
  await f.poll(() => /^1 waiting input was discarded and NOT sent: they had waited more than 6 seconds\./.test(d.notice()));
  await f.poll(() => !f.element('#refresh-control-frame').disabled); await d.quiet();
  assert.deepEqual(d.inputs().map(input => input.kind), ['key', 'click']);
  // One late picture, then one in time: the automatic retry is enough and the waiting key is sent on it.
  await f.refresh(); const again = f.delay(); d.tap(100, 100); await again; d.key('KeyD'); f.slowFrames(4500, 1); f.release();
  await d.settled(4); assert.deepEqual(d.inputs().at(-1).keys, ['d']);
  assertEachBoundToPrecedingFrame(f.controls);
}));

test('slow control picture: not focused means no automatic retry', async () => page(['input', 'observe'], 'slow-nofocus', async f => {
  const d = direct(f); await d.enable();
  let frames = f.frames(); f.slowFrames(5000, 3);
  await f.element('#refresh-control-frame').click();
  await f.poll(() => /Press Refresh screenshot to try again\.$/.test(f.element('#control-status').textContent)); await d.quiet();
  assert.equal(f.frames(), frames + 1, 'Without focus on the picture a late picture is not refetched');
}));

test('control picture: one fetch at a time; refresh requests during verification and decoding join it instead of issuing another', async () => page(['input', 'observe'], 'single-fetch', async f => {
  const d = direct(f); await d.enable();
  const frames = f.frames(), joined = reportOf(f).controlFrameRefresh.joined;
  let decoding = f.holdDecode(); await f.element('#refresh-control-frame').click(); await decoding;
  // The request has been answered and the picture is being decoded: the host would admit another request now.
  assert.equal(f.element('#refresh-control-frame').disabled, false); assert.equal(f.frames(), frames + 1);
  await f.element('#refresh-control-frame').click(); await f.element('#refresh-control-frame').click();
  d.surface.emit('focus'); f.advance(100);
  await d.quiet(); assert.equal(f.frames(), frames + 1, 'No second request while the first picture is still being decoded');
  const shown = () => reportOf(f).controlFrameRefresh.shown, first = shown();
  f.releaseDecode(); await f.poll(() => shown() === first + 1);
  assert.equal(f.frames(), frames + 1); assert.equal(reportOf(f).controlFrameRefresh.joined, joined + 2);
  d.key('KeyA'); await d.settled(1);
  assert.equal(d.inputs()[0].frame.frame_id, `00000000-0000-0000-0000-${String(frames + 1).padStart(12, '0')}`, 'The joined fetch is the picture input is bound to');
  // Stale input while a fetch is decoding: every one is refused and counted, and still only one request is made.
  f.advance(4001); const before = f.frames(); decoding = f.holdDecode();
  d.key('KeyB'); await decoding; d.key('KeyC'); d.tap(300, 175); d.wheel(300, 175, 300);
  await d.quiet(); assert.equal(f.frames(), before + 1, 'A burst of refused input starts exactly one fetch');
  f.releaseDecode(); await f.poll(() => shown() === first + 3);
  assert.equal(f.frames(), before + 1); assert.equal(d.inputs().length, 1); assert.equal(reportOf(f).controlQueue.refused, 4);
}));

test('direct input: every refusal is counted in the report at once, identical ones too, and shown as a count on one bounded notice line', async () => page(['input', 'observe'], 'refusal-count', async f => {
  const d = direct(f); await d.enable();
  const refused = () => reportOf(f).controlQueue.refused, base = 'The picture was older than 4 seconds, so this input was NOT sent. A fresh picture is being fetched; repeat the input on it.';
  assert.equal(reportOf(f).controlQueue, undefined);
  // A stale picture: every input until the fresh one is shown is refused for the same reason, and only one is fetched.
  const shown = () => reportOf(f).controlFrameRefresh.shown, before = shown(); f.advance(4001);
  d.key('KeyA'); assert.equal(refused(), 1); assert.equal(d.notice(), base);
  d.key('KeyA'); assert.equal(refused(), 2); assert.equal(d.notice(), `${base} (2 inputs in a row were refused for this reason.)`);
  d.key('KeyB'); assert.equal(refused(), 3); assert.equal(d.notice(), `${base} (3 inputs in a row were refused for this reason.)`);
  for (let i = 0; i < 200; i++) d.wheel(300, 175, 300);
  assert.equal(refused(), 203); assert.equal(d.notice(), `${base} (203 inputs in a row were refused for this reason.)`);
  assert.ok(d.notice().length < 200, 'One line, whatever the burst');
  // Another reason starts a new count; the first reason afterwards starts at one again.
  d.key('CapsLock'); assert.equal(refused(), 204); assert.match(d.notice(), /^The key CapsLock is not available on this connection[^(]*\(or too many modifiers are held\)\. Nothing was sent\.$/);
  d.key('KeyA'); assert.equal(refused(), 205); assert.equal(d.notice(), base);
  assert.deepEqual(Object.keys(reportOf(f).controlQueue).sort(), ['discarded', 'max', 'merged', 'queued', 'refused', 'sent', 'waiting'], 'The published counter set is unchanged');
  assert.equal(f.controls.some(control => ['click', 'drag', 'scroll', 'key'].includes(control.kind)), false);
  // An accepted input clears the notice; the same refusal later is a first one again.
  await f.poll(() => shown() === before + 1 && !f.element('#refresh-control-frame').disabled);
  d.key('KeyA'); await d.settled(1); assert.equal(d.notice(), ''); assert.equal(refused(), 205);
  f.advance(4001); d.key('KeyB'); d.key('KeyB'); assert.equal(refused(), 207);
  assert.equal(d.notice(), `${base} (2 inputs in a row were refused for this reason.)`);
}));

for (const [name, expected, line] of [
  ['geometry-png', { source_width: 1000, source_height: 500, frame_width: 1000, frame_height: 500, scale_divisor: 1, path: 'full-png', mimeType: 'image/png', byteLength: image.length }, 'Picture: 1000x500 PNG, full size; fetched in 0.0 s.'],
  ['jpeg-geometry-full', { source_width: 1000, source_height: 500, frame_width: 1000, frame_height: 500, scale_divisor: 1, path: 'full-jpeg', mimeType: 'image/jpeg', byteLength: jpeg.length }, 'Picture: 1000x500 JPEG, full size; fetched in 0.0 s.'],
  ['scaled-geometry', { source_width: 2000, source_height: 1000, frame_width: 1000, frame_height: 500, scale_divisor: 2, path: 'scaled-jpeg', mimeType: 'image/jpeg', byteLength: jpeg.length }, 'Picture: 1000x500 JPEG, scaled 1/2 from the 2000x1000 display; fetched in 0.0 s.'],
]) test(`diagnostic report and status line state the control picture geometry, scale and encoding: ${expected.path}`, async () => page(['input', 'observe'], name, async f => {
  assert.equal(reportOf(f).controlFrame, undefined, 'Nothing is reported before a picture exists');
  await f.refresh();
  assert.deepEqual(reportOf(f).controlFrame, { ...expected, requestToReadyMs: 0, usable: true });
  assert.equal(f.element('#control-status').textContent, `Screenshot ready for 4 seconds. Choose an action and click the screenshot. ${line}`);
  f.slowFrames(1234); await f.refresh();
  assert.deepEqual(reportOf(f).controlFrame, { ...expected, requestToReadyMs: 1234, usable: true });
  assert.equal(f.element('#control-status').textContent, `Screenshot ready for 4 seconds. Choose an action and click the screenshot. ${line.replace('0.0 s', '1.2 s')}`);
  // A picture that arrived too late still says which path was slow.
  f.slowFrames(6000); await f.element('#refresh-control-frame').click();
  await f.poll(() => /^The control picture took 6\.0 s/.test(f.element('#control-status').textContent));
  assert.deepEqual(reportOf(f).controlFrame, { ...expected, requestToReadyMs: 6000, usable: false });
  assert.deepEqual(reportOf(f).controlFrameRefresh, { shown: 2, expired: 1, consecutiveExpired: 1, joined: 0, lastExpiredMs: 6000, unavailable: 0, consecutiveUnavailable: 0, lastUnavailableReason: null, freshForMs: 4000 });
  assert.equal(f.element('#result').textContent.includes('data_base64'), false);
  assert.equal(f.element('#result').textContent.includes(f.imageSources.at(-1).split(',')[1]), false, 'The report never contains picture bytes');
}));

// ---- Typed "frame unavailable" reply: no picture this time, nothing fenced ------------------------------------
const NO_PICTURE = words => `No control picture this time (${words}). Input is paused; press Refresh screenshot.`;
const noPicture = (f, frames) => f.poll(() => f.frames() === frames && /press Refresh screenshot\.$/.test(f.element('#control-status').textContent) && !f.element('#refresh-control-frame').disabled);

test('control picture unavailable (step-by-step): channel and session are kept, it is counted and named, and the next picture and input work', async () => page(['input', 'observe'], 'unavailable-stepwise', async f => {
  await f.refresh();
  const channel = f.peers.at(-1).channel, sources = f.imageSources.length; let frames = f.frames();
  f.unavailable('controller_refused'); await f.element('#refresh-control-frame').click(); await noPicture(f, ++frames);
  assert.equal(f.element('#control-status').textContent, NO_PICTURE('the host had no picture ready'));
  assert.equal(f.imageSources.length, sources, 'There was no picture to decode');
  // Nothing was fenced: the channel is open, the session was not closed, control is still held.
  assert.equal(channel.readyState, 'open'); assert.equal(f.calls.includes('/auth/close'), false); assert.equal(f.peers.length, 1);
  assert.equal(f.element('#disconnect').disabled, false);
  // The freshness rule is unchanged: with no fresh picture nothing can be sent.
  assert.equal(f.element('#send-physical-key').disabled, true); assert.equal(f.element('#control-frame').attributes['aria-disabled'], 'true');
  const count = f.controls.length; await f.element('#send-physical-key').click(); await f.element('#control-frame').click({ clientX: 300, clientY: 175 });
  assert.equal(f.controls.length, count, 'Nothing is sent without a fresh picture');
  let report = reportOf(f);
  assert.deepEqual(report.controlFrameRefresh, { shown: 1, expired: 0, consecutiveExpired: 0, joined: 0, lastExpiredMs: null, unavailable: 1, consecutiveUnavailable: 1, lastUnavailableReason: 'controller_refused', freshForMs: 4000 });
  assert.equal(report.controlError, null, 'An unavailable picture is not a control failure'); assert.equal(report.controlAction, 'observe_frame'); assert.equal(typeof report.controlRoundTripMs, 'number');
  // Each reason of the service in plain words; a word the page does not know is shown as the host's own word.
  for (const [reason, words] of [['controller_deadline', 'the host did not answer in time'], ['png_decode_unsupported', 'the host could not convert the picture'], ['scaled_frame_over_cap', 'the picture is too large to send'], ['some_new_word', 'host reason: some new word'], ['constructor', 'host reason: constructor']]) {
    f.unavailable(reason); await f.element('#refresh-control-frame').click(); await noPicture(f, ++frames);
    assert.equal(f.element('#control-status').textContent, NO_PICTURE(words)); assert.equal(reportOf(f).controlFrameRefresh.lastUnavailableReason, reason);
  }
  assert.equal(reportOf(f).controlFrameRefresh.unavailable, 6); assert.equal(reportOf(f).controlFrameRefresh.consecutiveUnavailable, 6);
  assert.equal(f.frames(), frames, 'Step-by-step mode never refetches on its own');
  // The next picture and the next input work on the same channel and session.
  await f.refresh(); const shown = f.frames(); await f.element('#send-physical-key').click(); const key = await f.inputSettled('key', shown);
  assert.equal(channel.readyState, 'open'); assert.equal(f.peers.length, 1);
  // The fixture numbers only the pictures it produced: this is the second one, fetched after the six without one.
  assert.equal(key.frame.frame_id, '00000000-0000-0000-0000-000000000002', 'The input is bound to the picture fetched after the unavailable ones');
  report = reportOf(f); assert.equal(report.controlFrameRefresh.consecutiveUnavailable, 0); assert.equal(report.controlFrameRefresh.unavailable, 6); assert.equal(report.controlFrameRefresh.lastUnavailableReason, 'constructor'); assert.equal(report.controlFrame.usable, true);
  // The page's sequence stayed in step with the host's: the fixture asserts a strictly increasing one on every request.
}));

test('control picture unavailable (observe-only): the session is kept and the next picture is shown', async () => page(['observe'], 'unavailable-observe', async f => {
  const frames = f.frames(); f.unavailable('controller_deadline'); await f.element('#refresh-control-frame').click(); await noPicture(f, frames + 1);
  assert.equal(f.peers.at(-1).channel.readyState, 'open'); assert.equal(f.calls.includes('/auth/close'), false);
  await f.refresh(); assert.match(f.element('#control-status').textContent, /^Screenshot ready for observation/);
  assert.equal(reportOf(f).controlFrameRefresh.unavailable, 1); assert.equal(reportOf(f).controlFrameRefresh.shown, 1);
}));

test('control picture unavailable (direct): one bounded automatic retry while on the surface, input paused with its reason, then input resumes', async () => page(['input', 'observe'], 'unavailable-direct', async f => {
  const d = direct(f); await d.enable(); d.surface.emit('focus');
  const channel = f.peers.at(-1).channel, counters = () => reportOf(f).controlFrameRefresh;
  // Two in a row while the user is on the surface: one automatic retry, then it waits for the user.
  let frames = f.frames(); f.unavailable('controller_refused', 2);
  await f.element('#refresh-control-frame').click(); await noPicture(f, frames + 2);
  assert.equal(f.element('#control-status').textContent, NO_PICTURE('the host had no picture ready'));
  await d.quiet(); assert.equal(f.frames(), frames + 2, 'No fetch loop: after one retry nothing more is requested without a user action');
  assert.equal(counters().unavailable, 2); assert.equal(counters().consecutiveUnavailable, 2);
  assert.equal(channel.readyState, 'open');
  // More unavailable replies are ready, yet a user action costs exactly one request: a failure does not refill the budget.
  f.unavailable('controller_refused', 2); await d.quiet(); assert.equal(f.frames(), frames + 2);
  await f.element('#refresh-control-frame').click(); await noPicture(f, frames + 3); await d.quiet(); assert.equal(f.frames(), frames + 3);
  // The old picture ages out by the unchanged 4 s rule; input is then refused, not sent and not replayed.
  f.advance(4001); frames = f.frames(); d.key('KeyA');
  assert.match(d.notice(), /picture was older than 4 seconds, so this input was NOT sent/); assert.match(d.state(), /^Picture stale/);
  await noPicture(f, frames + 1); await d.quiet(); assert.equal(f.frames(), frames + 1); assert.equal(d.inputs().length, 0);
  assert.equal(counters().unavailable, 4); assert.equal(counters().consecutiveUnavailable, 4);
  // A picture in time resets the budget. Then no picture followed by a late one: they share the one retry.
  await f.refresh(); assert.equal(counters().consecutiveUnavailable, 0);
  frames = f.frames(); f.unavailable('controller_deadline'); f.slowFrames(4500, 1);
  await f.element('#refresh-control-frame').click();
  await f.poll(() => f.frames() === frames + 2 && /Press Refresh screenshot to try again\.$/.test(f.element('#control-status').textContent) && !f.element('#refresh-control-frame').disabled);
  await d.quiet(); assert.equal(f.frames(), frames + 2, 'No picture, then a late picture: one retry in total');
  assert.equal(counters().consecutiveUnavailable, 1); assert.equal(counters().consecutiveExpired, 1); assert.equal(counters().lastUnavailableReason, 'controller_deadline');
  // A picture arrives: input is sent on the same channel.
  await f.refresh(); d.key('KeyB'); await d.settled(1);
  assert.deepEqual(d.inputs()[0].keys, ['b']); assert.equal(channel.readyState, 'open'); assert.equal(f.peers.length, 1);
  // One unavailable reply, then a picture: the single automatic retry is enough.
  frames = f.frames(); f.unavailable('scaled_frame_over_cap'); await f.element('#refresh-control-frame').click();
  await f.poll(() => f.frames() === frames + 2 && f.element('#control-status').textContent.startsWith('Screenshot ready')); await d.quiet(); assert.equal(f.frames(), frames + 2);
  // No picture after an input, none on its one retry, and none on the single fetch the queue makes for the input
  // that is waiting: that input is then discarded visibly, never sent late, and nothing more is requested.
  const held = f.delay(); d.tap(300, 175); await held; d.key('KeyC'); frames = f.frames(); f.unavailable('controller_deadline', 5); f.release();
  await f.poll(() => /^1 waiting input was discarded and NOT sent: no fresh picture could be obtained\./.test(d.notice()));
  await f.poll(() => !f.element('#refresh-control-frame').disabled); await d.quiet();
  assert.equal(f.frames(), frames + 3, 'Bounded: two more unavailable replies are ready and were not asked for'); assert.equal(f.element('#control-status').textContent, NO_PICTURE('the host did not answer in time'));
  assert.deepEqual(d.inputs().map(input => input.kind), ['key', 'click']); assert.equal(channel.readyState, 'open');
  assert.equal(reportOf(f).controlError, null); assert.equal(f.calls.includes('/auth/close'), false);
  // And once more a picture and an input, still on the same channel.
  await f.element('#refresh-control-frame').click(); await noPicture(f, frames + 4); await f.element('#refresh-control-frame').click(); await noPicture(f, frames + 5); await d.quiet(); assert.equal(f.frames(), frames + 5);
  await f.refresh(); d.key('KeyD'); await d.settled(3); assert.deepEqual(d.inputs().at(-1).keys, ['d']);
}));

test('control picture unavailable: without focus on the picture there is no automatic retry; with focus the status says the retry is running', async () => page(['input', 'observe'], 'unavailable-nofocus', async f => {
  const d = direct(f); await d.enable();
  let frames = f.frames(); f.unavailable('controller_refused');
  await f.element('#refresh-control-frame').click(); await noPicture(f, frames + 1); await d.quiet();
  assert.equal(f.frames(), frames + 1, 'Not on the surface: no refetch');
  await f.refresh(); d.surface.emit('focus'); await d.quiet();
  const status = f.element('#control-status'), seen = []; let text = status.textContent;
  Object.defineProperty(status, 'textContent', { get: () => text, set: value => { text = value; seen.push(value); }, configurable: true });
  frames = f.frames(); f.unavailable('controller_refused'); await f.element('#refresh-control-frame').click();
  await f.poll(() => f.frames() === frames + 2 && text.startsWith('Screenshot ready')); await d.quiet(); assert.equal(f.frames(), frames + 2);
  assert.deepEqual(seen.filter(value => value.startsWith('No control picture')), ['No control picture this time (the host had no picture ready). Input is paused; fetching it again…']);
}));

test('frame-unavailable reply to a request that is not observe_frame still ends the session', async () => page(['input', 'observe'], 'unavailable-violation', async f => {
  await f.refresh(); const channel = f.peers.at(-1).channel;
  f.unavailableNext('controller_refused'); await f.element('#send-physical-key').click();
  await f.poll(() => channel.readyState === 'closed' && f.calls.includes('/auth/close'));
  assert.equal(reportOf(f).controlFrameRefresh.unavailable, 0, 'It is not counted as an unavailable picture');
  assert.match(String(reportOf(f).controlError), /Invalid control response fields/);
  assert.equal(f.element('#send-physical-key').disabled, true); const count = f.controls.length; await f.element('#send-physical-key').click(); assert.equal(f.controls.length, count);
}));

test('malformed frame-unavailable reason to observe_frame still ends the session', async () => page(['input', 'observe'], 'unavailable-malformed', async f => {
  await f.refresh(); const channel = f.peers.at(-1).channel;
  f.unavailable('Not-A-Word'); await f.element('#refresh-control-frame').click();
  await f.poll(() => channel.readyState === 'closed' && f.calls.includes('/auth/close'));
  assert.equal(reportOf(f).controlFrameRefresh.unavailable, 0); assert.doesNotMatch(f.element('#control-status').textContent, /No control picture/);
}));
