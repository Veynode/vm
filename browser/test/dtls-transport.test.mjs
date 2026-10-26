import test from 'node:test';
import assert from 'node:assert/strict';
import { setTimeout as realTimeout } from 'node:timers';
import { AuthenticatedReceiver } from '../dist/native-auth.js';
import { DeviceIdentity, grantBytes, answerBytes, sdpBinding, verifyOffer, verifyConfirmation, verifyClose } from '../dist/session-auth.js';

const flush = async () => { for (let i = 0; i < 100; i++) await Promise.resolve(); };
const poll = async predicate => {
  for (let i = 0; i < 200; i++) { if (predicate()) return; await new Promise(resolve => realTimeout(resolve, 5)); }
  assert.fail('DTLS fixture did not settle');
};
const sdp = byte => `v=0\r\na=fingerprint:sha-256 ${Array(32).fill(byte).join(':')}\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n`;
let pageId = 0;
// Exercise the production page handlers with synthetic DOM, HTTP and WebRTC.
async function fixture(t, options = {}) {
  t.mock.timers.enable({ apis: ['setTimeout', 'setInterval'] });
  const saved = new Map();
  const replace = (key, value) => {
    saved.set(key, Object.getOwnPropertyDescriptor(globalThis, key));
    Object.defineProperty(globalThis, key, { value, writable: true, configurable: true });
  };
  let clock = 100, connectTask;
  const peers = [], calls = [], transports = [];
  let auth, transcript, confirmationSignal, resolveConfirmation;
  if (options.authenticated) {
    const host = await DeviceIdentity.generate(), device = await DeviceIdentity.generate();
    const grant = { host_id: host.id, device_id: device.id, device_public_key: device.publicKey, vm_id: 'dtls-fixture', scopes: ['observe'], version: 1 };
    const receiver = await AuthenticatedReceiver.fromIdentity(device, { pinnedHostPublicKey: host.publicKey, vmId: grant.vm_id });
    auth = { host, device, grant, receiver, signedGrant: { grant, signature: await host.sign(await grantBytes(grant)) } };
    t.mock.method(AuthenticatedReceiver, 'open', async () => receiver);
  }
  class Dtls extends EventTarget {
    state = 'connected'; listeners = new Set(); additions = 0;
    constructor() { super(); transports.push(this); }
    addEventListener(type, listener) {
      if (type === 'statechange') { this.listeners.add(listener); this.additions++; }
      super.addEventListener(type, listener);
    }
    removeEventListener(type, listener) {
      if (type === 'statechange') this.listeners.delete(listener);
      super.removeEventListener(type, listener);
    }
    change(state) { this.state = state; this.dispatchEvent(new Event('statechange')); }
  }
  class Element {
    listeners = new Map(); style = {}; textContent = ''; value = ''; checked = false;
    disabled = false; hidden = false; srcObject = null; videoWidth = 0; videoHeight = 0;
    addEventListener(type, callback) { this.listeners.set(type, callback); }
    click() { return this.listeners.get('click')?.(); }
    setAttribute() {}
    getContext() { return { drawImage() {}, getImageData: () => ({ data: new Uint8Array(16 * 9 * 4) }) }; }
    play() { return Promise.resolve(); }
    requestVideoFrameCallback(callback) { this.frame = callback; return 1; }
    cancelVideoFrameCallback() { this.frame = undefined; }
  }
  const nodes = new Map();
  const element = id => { if (!nodes.has(id)) nodes.set(id, new Element()); return nodes.get(id); };
  class Stream {
    constructor(tracks = []) { this.tracks = tracks; }
    getTracks() { return this.tracks; }
  }
  class Peer extends EventTarget {
    iceGatheringState = 'complete'; connectionState = 'new'; closeCount = 0;
    receiver = {}; sender = {}; extraReceiver = {}; mediaReady = false;
    constructor() { super(); peers.push(this); }
    addTransceiver() { return { receiver: this.receiver, sender: this.sender, setCodecPreferences() {} }; }
    getReceivers() { if (options.throwingMethod === 'getReceivers') throw Error('Unavailable receivers'); return options.throwingTransport ? [this.receiver, this.extraReceiver] : [this.receiver]; }
    getSenders() { if (options.throwingMethod === 'getSenders') throw Error('Unavailable senders'); return [this.sender]; }
    getTransceivers() { if (options.throwingMethod === 'getTransceivers') throw Error('Unavailable transceivers'); return [{ receiver: this.receiver, sender: this.sender }, { receiver: this.extraReceiver, sender: {} }]; }
    async createOffer() { return { type: 'offer', sdp: auth ? sdp('AA') : 'v=0\r\n' }; }
    async setLocalDescription(value) { this.localDescription = value; }
    async setRemoteDescription(value) {
      this.remoteDescription = value;
      if (!options.missingTransport) {
        this.receiver.transport = new Dtls(); this.sender.transport = this.receiver.transport;
        this.extraReceiver.transport = new Dtls();
        if (options.initialState) this.receiver.transport.state = options.initialState;
        if (options.throwingTransport) Object.defineProperty(this[options.throwingTransport], 'transport', { get() { throw Error('Unavailable transport'); } });
      }
      if (options.trackDuringAnswer) this.track();
      this.connectionState = 'connected';
      if (!options.noConnectionEvent) this.onconnectionstatechange();
    }
    track(receiver = this.receiver) { this.ontrack({ receiver, track: { stop() {} } }); }
    async getStats() {
      return this.mediaReady ? new Map([
        ['inbound', { type: 'inbound-rtp', kind: 'video', codecId: 'codec', framesDecoded: 5 }],
        ['codec', { mimeType: 'video/H264', clockRate: 90000, sdpFmtpLine: 'profile-level-id=42e01f;packetization-mode=1' }],
      ]) : new Map();
    }
    close() {
      this.closeCount++;
      // Deliberately emit DTLS closure before changing the aggregate state.
      for (const transport of transports) transport.change('closed');
      this.connectionState = 'closed'; this.onconnectionstatechange?.();
    }
  }
  t.after(async () => {
    try { resolveConfirmation?.(Response.json({ confirmed: true })); await element('#disconnect').click(); await flush(); await connectTask; }
    finally {
      auth?.receiver.dispose();
      for (const [key, descriptor] of saved) {
        if (descriptor) Object.defineProperty(globalThis, key, descriptor); else delete globalThis[key];
      }
    }
  });
  replace('document', { querySelector: element, createElement: () => new Element() });
  replace('window', new EventTarget()); replace('performance', { now: () => clock });
  replace('MediaStream', Stream); replace('RTCPeerConnection', Peer);
  replace('RTCRtpReceiver', { getCapabilities: () => ({ codecs: [{ mimeType: 'video/H264', clockRate: 90000, sdpFmtpLine: 'profile-level-id=42e01f;packetization-mode=1' }] }) });
  replace('fetch', async (path, request) => {
    calls.push(path);
    if (path === '/capabilities') return Response.json({ h264: 'verified-hardware', h265: 'unavailable', ...(auth ? { authentication: { required: true, vm_id: auth.grant.vm_id } } : {}) });
    if (auth) {
      const body = JSON.parse(request.body), now = Math.floor(Date.now() / 1000);
      if (path === '/auth/challenge') return Response.json({ host_id: auth.host.id, device_id: auth.device.id, vm_id: auth.grant.vm_id, scopes: ['observe'], grant_version: 1, instance_nonce: '11'.repeat(32), nonce: '22'.repeat(32), issued_at: now, expires_at: now + 60 });
      if (path === '/auth/offer') {
        await verifyOffer(body.signed_offer, auth.grant, body.signed_offer.intent.challenge, body.sdp, now);
        transcript = { intent: body.signed_offer.intent, session_id: 'dtls-test', answer: await sdpBinding(sdp('BB')) };
        return Response.json({ type: 'answer', sdp: sdp('BB'), signed_answer: { transcript, signature: await auth.host.sign(answerBytes(transcript)) } });
      }
      if (path === '/auth/confirm') {
        await verifyConfirmation(body.confirmation, transcript, auth.device.publicKey, now);
        return new Promise((resolve, reject) => {
          resolveConfirmation = resolve; confirmationSignal = request.signal;
          if (!options.ignoreConfirmationAbort) {
            const abort = () => reject(request.signal.reason);
            request.signal.addEventListener('abort', abort, { once: true });
            if (request.signal.aborted) abort();
          }
        });
      }
      if (path === '/auth/close') { await verifyClose(body, transcript, auth.device.publicKey); return Response.json({ closed: true }); }
    }
    if (path === '/offer') return Response.json({ type: 'answer', sdp: 'v=0\r\n', session_id: 'dtls-test' });
    if (path === '/close') return Response.json({ closed: true });
    throw new Error('Unexpected endpoint: ' + path);
  });
  await import(`../dist/native.js?dtls-test=${++pageId}`);
  await flush();
  if (auth) {
    element('#host-pin').value = auth.host.publicKey;
    element('#signed-grant').value = JSON.stringify(auth.signedGrant);
    await element('#prepare-pairing').click(); await poll(() => element('#pairing-request').textContent.includes('digest'));
    await element('#accept-grant').click(); await poll(() => element('#pairing-status').textContent.includes('Ready'));
  }
  assert.equal(element('#connect').disabled, false);
  connectTask = element('#connect').click();
  await flush();
  if (auth) await poll(() => confirmationSignal !== undefined);
  assert.equal(peers.length, 1);
  const report = () => JSON.parse(element('#result').textContent);
  const tick = async ms => { clock += ms; t.mock.timers.tick(ms); await flush(); };
  return { peer: peers[0], transports, calls, element, report, tick, connectTask, Dtls, confirmationSignal,
    confirm: () => resolveConfirmation(Response.json({ confirmed: true })),
    failConfirmation: () => resolveConfirmation(Response.json({ error: 'Unavailable' }, { status: 503 })),
  };
}

for (const state of ['closed', 'failed']) test(`DTLS ${state} before first frame fails immediately while peer stays connected`, async t => {
  const f = await fixture(t);
  await f.tick(47);
  assert.equal(f.report().attemptFailure, undefined);
  assert.equal(f.peer.connectionState, 'connected');
  f.transports[0].change(state);
  assert.equal(f.peer.closeCount, 1);
  assert.deepEqual(f.report().dtlsTransportFailure, { code: 'dtls_transport_closed', state, elapsedMs: 47 });
  assert.match(f.report().attemptFailure, new RegExp(`DTLS transport ${state} at 47ms since connect`));
  await flush(); await f.connectTask;
  assert.deepEqual(f.report().attempts, [{ preferred: 'h264', outcome: 'failed', reason: 'dtls_transport_closed' }]);
  await f.tick(30000);
  assert.equal(f.calls.filter(path => path === '/close').length, 1);
  assert.ok(f.transports.every(transport => transport.listeners.size === 0));
});

test('intentional local close removes all deduplicated listeners before DTLS close events', async t => {
  const f = await fixture(t);
  f.peer.track(); f.peer.onconnectionstatechange();
  assert.equal(f.transports.length, 2);
  assert.ok(f.transports.every(transport => transport.additions === 1 && transport.listeners.size === 1));
  const queued = [...f.transports[0].listeners];
  await f.element('#disconnect').click(); await flush(); await f.connectTask;
  assert.ok(f.transports.every(transport => transport.listeners.size === 0));
  for (const transport of f.transports) transport.change('failed');
  for (const listener of queued) listener();
  assert.equal(f.report().attemptFailure, undefined);
  assert.equal(f.report().dtlsTransportFailure, undefined);
  assert.equal(f.peer.closeCount, 1);
});

for (const state of ['failed', 'closed']) test(`DTLS failed after connectionState ${state} does not double-report`, async t => {
  const f = await fixture(t);
  const queued = [...f.transports[0].listeners];
  f.peer.connectionState = state; f.peer.onconnectionstatechange();
  const failure = f.report().attemptFailure;
  assert.match(failure, /Native WebRTC transport failed/);
  f.transports[0].change('failed');
  for (const listener of queued) listener();
  await flush(); await f.connectTask;
  assert.equal(f.report().attemptFailure, failure);
  assert.equal(f.report().dtlsTransportFailure, undefined);
  assert.equal(f.report().attempts[0].reason, 'transport_failed');
  assert.equal(f.peer.closeCount, 1);
  assert.equal(f.calls.filter(path => path === '/close').length, 1);
});

test('missing receiver/sender transport does not throw or change the six-second first-frame deadline', async t => {
  const f = await fixture(t, { missingTransport: true });
  f.peer.track(); f.peer.onconnectionstatechange();
  await f.tick(5999);
  assert.equal(f.report().attemptFailure, undefined);
  await f.tick(1); await f.connectTask;
  assert.equal(f.report().attempts[0].reason, 'first_frame_timeout');
  assert.equal(f.report().dtlsTransportFailure, undefined);
  assert.equal(f.peer.closeCount, 1);
});

for (const trackDuringAnswer of [true, false]) test(`already-closed DTLS during answer installation is preserved (track event: ${trackDuringAnswer})`, async t => {
  const f = await fixture(t, { initialState: 'closed', trackDuringAnswer, noConnectionEvent: true });
  await f.connectTask;
  assert.equal(f.report().attempts[0].reason, 'dtls_transport_closed');
  assert.equal(f.report().dtlsTransportFailure.state, 'closed');
  assert.equal(f.peer.closeCount, 1);
  assert.ok(f.transports.every(transport => transport.listeners.size === 0));
  await f.tick(30000);
  assert.equal(f.calls.filter(path => path === '/close').length, 1);
});

test('a distinct transport exposed only by a transceiver is also observed', async t => {
  const f = await fixture(t, { noConnectionEvent: true });
  f.transports[1].change('failed');
  await flush(); await f.connectTask;
  assert.equal(f.report().attempts[0].reason, 'dtls_transport_closed');
  assert.equal(f.report().dtlsTransportFailure.state, 'failed');
});

test('a transport first exposed by a later track event is observed', async t => {
  const f = await fixture(t, { missingTransport: true });
  const transport = new f.Dtls();
  f.peer.track({ transport });
  assert.equal(transport.listeners.size, 1);
  transport.change('closed');
  await flush(); await f.connectTask;
  assert.equal(f.report().attempts[0].reason, 'dtls_transport_closed');
  assert.equal(transport.listeners.size, 0);
});

test('DTLS failure after decoded/presented frames ends the live session immediately', async t => {
  const f = await fixture(t);
  f.peer.track(); f.peer.mediaReady = true;
  for (let i = 0; i < 5; i++) f.element('#received').frame();
  await f.tick(100); await f.connectTask;
  assert.equal(f.report().status, 'passed-live');
  f.transports[0].change('failed');
  assert.equal(f.report().status, 'stream-ended');
  assert.deepEqual(f.report().dtlsTransportFailure, { code: 'dtls_transport_closed', state: 'failed', elapsedMs: 100 });
  assert.equal(f.peer.closeCount, 1);
  await flush();
  assert.equal(f.element('#connect').disabled, false);
  assert.ok(f.transports.every(transport => transport.listeners.size === 0));
});

test('DTLS close aborts pending authenticated confirmation and fails without advancing timers', async t => {
  const f = await fixture(t, { authenticated: true });
  assert.equal(f.confirmationSignal.aborted, false);
  f.transports[0].change('closed');
  assert.equal(f.confirmationSignal.aborted, true);
  assert.equal(f.confirmationSignal.reason.code, 'dtls_transport_closed');
  await poll(() => f.report().status === 'failed'); await f.connectTask;
  assert.deepEqual(f.report().attempts, [{ preferred: 'h264', outcome: 'failed', reason: 'dtls_transport_closed' }]);
  assert.equal(f.calls.filter(path => path === '/auth/close').length, 1);
  assert.equal(f.element('#connect').disabled, false);
});

test('late HTTP 503 confirmation rejection preserves the DTLS close reason', async t => {
  const f = await fixture(t, { authenticated: true, ignoreConfirmationAbort: true });
  f.transports[0].change('closed');
  assert.equal(f.confirmationSignal.aborted, true);
  assert.equal(f.confirmationSignal.reason.code, 'dtls_transport_closed');
  f.failConfirmation();
  await poll(() => f.report().status === 'failed'); await f.connectTask;
  assert.deepEqual(f.report().attempts, [{ preferred: 'h264', outcome: 'failed', reason: 'dtls_transport_closed' }]);
  assert.equal(f.calls.filter(path => path === '/auth/close').length, 1);
});

test('authenticated confirmation succeeds normally without a DTLS event', async t => {
  const f = await fixture(t, { authenticated: true });
  f.confirm();
  await poll(() => f.report().iceAttempts[0].phases.some(entry => entry.phase === 'signed-confirmation-acknowledged'));
  f.peer.track(); f.peer.mediaReady = true;
  for (let i = 0; i < 5; i++) f.element('#received').frame();
  await f.tick(100); await f.connectTask;
  assert.equal(f.report().status, 'passed-live');
  assert.equal(f.report().dtlsTransportFailure, undefined);
  assert.equal(f.report().attemptFailure, undefined);
  assert.equal(f.confirmationSignal.aborted, false);
  await f.element('#disconnect').click();
  assert.equal(f.confirmationSignal.aborted, false, 'Settled confirmation no longer follows attempt cancellation');
});

test('HTTP 503 confirmation failure without DTLS retains its original reason', async t => {
  const f = await fixture(t, { authenticated: true });
  f.failConfirmation();
  await poll(() => f.report().status === 'failed'); await f.connectTask;
  assert.equal(f.report().attempts[0].reason, 'negotiation_failed');
  assert.equal(f.report().dtlsTransportFailure, undefined);
  assert.equal(f.confirmationSignal.aborted, false);
});

test('attempt cancellation still aborts pending authenticated confirmation', async t => {
  const f = await fixture(t, { authenticated: true });
  await f.element('#disconnect').click(); await f.connectTask;
  assert.equal(f.confirmationSignal.aborted, true);
  assert.equal(f.report().dtlsTransportFailure, undefined);
  assert.equal(f.calls.filter(path => path === '/auth/close').length, 1);
});

for (const throwingTransport of ['receiver', 'sender']) test(`throwing ${throwingTransport} transport getter does not prevent media attachment or other observation`, async t => {
  const f = await fixture(t, { throwingTransport, trackDuringAnswer: true });
  assert.ok(f.element('#received').srcObject instanceof MediaStream);
  assert.equal(f.transports[1].listeners.size, 1);
  assert.doesNotThrow(() => f.peer.track());
  assert.ok(f.element('#received').srcObject instanceof MediaStream);
  f.transports[1].change('closed');
  await flush(); await f.connectTask;
  assert.equal(f.report().attempts[0].reason, 'dtls_transport_closed');
});

for (const throwingMethod of ['getReceivers', 'getSenders', 'getTransceivers']) test(`throwing ${throwingMethod} does not prevent media attachment or other observation`, async t => {
  const f = await fixture(t, { throwingMethod, trackDuringAnswer: true });
  assert.ok(f.element('#received').srcObject instanceof MediaStream);
  assert.equal(f.transports[0].listeners.size, 1);
  assert.doesNotThrow(() => f.peer.track());
  f.transports[0].change('closed');
  await flush(); await f.connectTask;
  assert.equal(f.report().attempts[0].reason, 'dtls_transport_closed');
});
