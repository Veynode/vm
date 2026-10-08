import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { spawnSync } from 'node:child_process';
import { DeviceIdentity, hex, grantBytes, answerBytes, sdpBinding, verifyOffer, verifyConfirmation, verifyClose } from '../dist/session-auth.js';
import { controlBytes, signControl, verifyControl, validateControlResponse, screenshotAnchor, SCREENSHOT_MAX_BYTES, CONTROL_MAX_RESPONSE_BYTES } from '../dist/session-control.js';
import { AuthenticatedReceiver } from '../dist/native-auth.js';

const pairBinary = process.env.VEYNODE_PAIR_BIN ?? new URL('../../target/debug/vm-pair', import.meta.url).pathname;
const frame = { frame_id: '12345678-1234-4234-8234-123456789abc', source_generation: 1, width: 1280, height: 800 };
const click = () => ({ kind: 'click', epoch: 1, frame: structuredClone(frame), x: 25, y: 40, button: 'left' });
const sdp = byte => `v=0\r\na=fingerprint:sha-256 ${Array(32).fill(byte).join(':')}\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n`;
const png = { ...frame, mime_type: 'image/png', byte_length: 8, data_base64: Buffer.from('\x89PNG\r\n\x1a\n', 'latin1').toString('base64') };

async function fixture(scopes = ['input', 'observe']) {
  const host = await DeviceIdentity.generate(), device = await DeviceIdentity.generate();
  const receiver = await AuthenticatedReceiver.fromIdentity(device, { pinnedHostPublicKey: host.publicKey, vmId: 'fixture-vm', scopes });
  const grant = { host_id: host.id, device_id: device.id, device_public_key: device.publicKey, vm_id: 'fixture-vm', scopes: [...scopes], version: 1 };
  await receiver.acceptGrant({ grant, signature: await host.sign(await grantBytes(grant)) });
  const now = Math.floor(Date.now() / 1000);
  const challenge = { host_id: host.id, device_id: device.id, vm_id: 'fixture-vm', scopes: [...scopes], grant_version: 1, instance_nonce: '11'.repeat(32), nonce: '22'.repeat(32), issued_at: now, expires_at: now + 60 };
  const state = { controls: [], transcript: undefined, confirm: true, close: true, controlReply: undefined };
  const calls = [];
  const fetcher = async (path, options) => {
    calls.push(path); const body = JSON.parse(options.body);
    assert.equal(options.redirect, 'error'); assert.equal(options.credentials, 'omit');
    if (path === '/auth/challenge') { assert.deepEqual(body.scopes, scopes); return Response.json(challenge); }
    if (path === '/auth/offer') {
      await verifyOffer(body.signed_offer, grant, challenge, body.sdp, now);
      state.transcript = { intent: body.signed_offer.intent, session_id: 'control-session', answer: await sdpBinding(sdp('BB')) };
      return Response.json({ type: 'answer', sdp: sdp('BB'), signed_answer: { transcript: state.transcript, signature: await host.sign(answerBytes(state.transcript)) } });
    }
    if (path === '/auth/confirm') { await verifyConfirmation(body.confirmation, state.transcript, device.publicKey, now); return Response.json({ confirmed: state.confirm }); }
    if (path === '/auth/close') { await verifyClose(body, state.transcript, device.publicKey); return Response.json({ closed: state.close }); }
    if (path === '/auth/control') {
      await verifyControl(body, state.transcript, device.publicKey, Math.floor(Date.now() / 1000)); state.controls.push(body);
      if (state.controlReply) return state.controlReply(body);
      const action = body.request.action;
      const result = action.kind === 'acquire' ? { epoch: 1, expires_in_seconds: 60 } : action.kind === 'release' ? { released: true, note: 'synthetic' } : action.kind === 'observe_frame' ? png : { dispatched: true, guest_delivery_confirmed: false };
      return Response.json({ sequence: body.request.sequence, result });
    }
    throw new Error('Unexpected endpoint');
  };
  return { host, device, receiver, state, calls, fetcher };
}
async function withSession(callback, scopes) {
  const f = await fixture(scopes), previous = globalThis.fetch; globalThis.fetch = f.fetcher;
  try { await f.receiver.offer(sdp('AA'), 'control-request', 'h264'); await callback(f); }
  finally { f.state.close = true; await f.receiver.close(); f.receiver.dispose(); globalThis.fetch = previous; }
}

test('Rust golden canonical bytes and P256 signatures match every browser control variant', async () => {
  const golden = JSON.parse(await readFile(new URL('../../crates/session-auth/tests/control-golden.json', import.meta.url), 'utf8'));
  assert.equal(golden.cases.length, 7);
  for (const item of golden.cases) {
    assert.equal(hex(controlBytes(item.signed.request)), item.canonical_hex, item.signed.request.action.kind);
    await verifyControl(item.signed, golden.transcript, golden.device_public_key, golden.now);
  }
});
test('WebCrypto control signature is verified by the actual Rust CLI; payload mutation fails', async () => {
  await withSession(async f => {
    const signed = await signControl(f.device, f.state.transcript, click(), { sequence: 7, now: Math.floor(Date.now() / 1000) });
    const request = { kind: 'control', device_public_key: f.device.publicKey, transcript: f.state.transcript, signed };
    const result = spawnSync(pairBinary, ['verify'], { input: JSON.stringify(request), encoding: 'utf8', timeout: 5000 });
    assert.equal(result.status, 0, result.error?.message ?? result.stderr);
    assert.equal(JSON.parse(result.stdout).verified, true);
    assert.equal(JSON.parse(result.stdout).authorization_checked, false, 'Crypto CLI is not a live-session authorization gate');
    request.signed.request.action.x++;
    assert.notEqual(spawnSync(pairBinary, ['verify'], { input: JSON.stringify(request), encoding: 'utf8', timeout: 5000 }).status, 0);
  });
});
test('strict action schema, safe integers, bounded coordinates, keys and short expiry', async () => {
  await withSession(async f => {
    const now = Math.floor(Date.now() / 1000), signed = await signControl(f.device, f.state.transcript, click(), { sequence: 1, now });
    for (const mutate of [r => r.extra = true, r => r.sequence = 0, r => r.sequence = 2 ** 53, r => r.sequence = true,
      r => r.session_id = 'bad/session', r => r.transcript_sha256 = 'AA'.repeat(32), r => r.action.extra = true,
      r => r.action.epoch = 0, r => r.action.x = 1280, r => r.action.y = -1, r => r.action.x = .5,
      r => r.action.button = 'back', r => r.action.frame.extra = true, r => r.action.frame.frame_id = frame.frame_id.toUpperCase(),
      r => r.action.frame.source_generation = -1, r => r.action.frame.width = 8193,
      r => r.action = { kind: 'release', epoch: 0 }, r => r.action = { kind: 'acquire', epoch: 1 },
      r => r.action = { kind: 'text', text: 'not-supported' },
      r => r.action = { kind: 'key', epoch: 1, frame, keys: [''] },
      r => r.action = { kind: 'key', epoch: 1, frame, keys: ['ctrl', 'ctrl'] },
      r => r.action = { kind: 'key', epoch: 1, frame, keys: ['a'.repeat(17)] },
      r => r.action = { kind: 'key', epoch: 1, frame, keys: ['A'] },
      r => r.action = { kind: 'scroll', epoch: 1, frame, x: 0, y: 0, direction: 'up', ticks: 21 }]) {
      const changed = structuredClone(signed.request); mutate(changed); assert.throws(() => controlBytes(changed));
    }
    for (const expires_at of [now, now + 6, Number.MAX_SAFE_INTEGER + 1]) await assert.rejects(signControl(f.device, f.state.transcript, click(), { sequence: 1, now, expires_at }));
    const foreign = await DeviceIdentity.generate(); await assert.rejects(signControl(foreign, f.state.transcript, click(), { sequence: 1, now }), /another device/);
    for (const mutate of [s => s.request.sequence++, s => s.request.action.x++, s => s.request.action.frame.source_generation++]) {
      const changed = structuredClone(signed); mutate(changed); await assert.rejects(verifyControl(changed, f.state.transcript, f.device.publicKey, now));
    }
    // An established session can outlive the setup challenge; each operation
    // gets its own fresh deadline, with host session expiry checked separately.
    await signControl(f.device, f.state.transcript, click(), { sequence: 2, now: now + 120 });
  });
});
test('control requires confirmed session, fixed scopes, and all seven action variants work', async () => {
  await withSession(async f => {
    await assert.rejects(f.receiver.control({ kind: 'acquire' }), /confirmed/);
    f.state.confirm = false; await assert.rejects(f.receiver.confirm(), /not acknowledged/);
    await assert.rejects(f.receiver.control({ kind: 'acquire' }), /confirmed/);
    f.state.confirm = true; await f.receiver.confirm();
    const actions = [{ kind: 'acquire' }, { kind: 'observe_frame' }, click(),
      { ...click(), kind: 'drag', end_x: 30, end_y: 50 },
      { kind: 'scroll', epoch: 1, frame, x: 0, y: 0, direction: 'down', ticks: 2 },
      { kind: 'key', epoch: 1, frame, keys: ['ctrl', 'a'] }, { kind: 'release', epoch: 1 }];
    for (const action of actions) await f.receiver.control(action);
    assert.deepEqual(f.state.controls.map(s => s.request.sequence), [1, 2, 3, 4, 5, 6, 7]);
    assert.deepEqual(screenshotAnchor((await f.receiver.control({ kind: 'observe_frame' })).result), frame);
  });
  await withSession(async f => { await f.receiver.confirm(); await assert.rejects(f.receiver.control({ kind: 'acquire' }), /scope/); assert.equal(f.state.controls.length, 0); await f.receiver.control({ kind: 'observe_frame' }); }, ['observe']);
});
test('pending control excludes close/second control; caller mutation cannot alter signed action', async () => {
  await withSession(async f => {
    await f.receiver.confirm(); let release, entered;
    const reached = new Promise(resolve => { entered = resolve; }), gate = new Promise(resolve => { release = resolve; });
    f.state.controlReply = async body => { entered(); await gate; return Response.json({ sequence: body.request.sequence, result: { dispatched: true, guest_delivery_confirmed: false } }); };
    const action = click(), pending = f.receiver.control(action); action.x = 999; action.frame.source_generation = 999;
    await reached;
    await assert.rejects(f.receiver.close(), /pending/); await assert.rejects(f.receiver.control(click()), /pending/);
    release(); await pending;
    assert.equal(f.state.controls[0].request.action.x, 25); assert.equal(f.state.controls[0].request.action.frame.source_generation, 1);
  });
});
test('uncertain control consumes sequence without retry; failed close permanently fences new control', async () => {
  await withSession(async f => {
    await f.receiver.confirm();
    f.state.controlReply = () => { throw new TypeError('response lost after dispatch'); };
    await assert.rejects(f.receiver.control(click()), /lost/); assert.equal(f.state.controls.length, 1);
    f.state.controlReply = undefined; await f.receiver.control(click()); assert.deepEqual(f.state.controls.map(s => s.request.sequence), [1, 2]);
    f.state.close = false; await assert.rejects(f.receiver.close(), /not acknowledged/);
    await assert.rejects(f.receiver.control(click()), /confirmed/); await assert.rejects(f.receiver.confirm(), /No verified/);
    assert.equal(f.state.controls.length, 2);
  });
});
test('control HTTP response has a1MiB streaming bound and matching sequence is mandatory', async () => {
  await withSession(async f => {
    await f.receiver.confirm(); let cancelled = false;
    f.state.controlReply = () => new Response(new ReadableStream({ start(controller) { controller.enqueue(new Uint8Array(CONTROL_MAX_RESPONSE_BYTES + 1)); }, cancel() { cancelled = true; } }));
    await assert.rejects(f.receiver.control({ kind: 'observe_frame' }), /too large/); assert.equal(cancelled, true);
    f.state.controlReply = body => Response.json({ sequence: body.request.sequence + 1, result: png });
    await assert.rejects(f.receiver.control({ kind: 'observe_frame' }), /sequence/);
  });
});
test('screenshot bounds/base64/type and dispatch/lease/release outcomes fail closed', () => {
  const response = { sequence: 1, result: structuredClone(png) };
  validateControlResponse(response, 1, { kind: 'observe_frame' });
  for (const mutate of [r => r.extra = true, r => r.sequence = true, r => r.result.byte_length++, r => r.result.byte_length = SCREENSHOT_MAX_BYTES + 1,
    r => r.result.data_base64 = r.result.data_base64.replace('=', '!'), r => r.result.mime_type = 'text/html',
    r => r.result.source_generation = -1, r => r.result.width = 0]) {
    const changed = structuredClone(response); mutate(changed); assert.throws(() => validateControlResponse(changed, 1, { kind: 'observe_frame' }));
  }
  assert.throws(() => validateControlResponse({ sequence: 1, result: { released: false } }, 1, { kind: 'release', epoch: 1 }));
  assert.throws(() => validateControlResponse({ sequence: 1, result: { epoch: 1, expires_in_seconds: 600 } }, 1, { kind: 'acquire' }));
  assert.throws(() => validateControlResponse({ sequence: 1, result: { dispatched: true, guest_delivery_confirmed: true } }, 1, click()));
  // Protocol size/header boundary, not a PNG-decoder or Guest acceptance test.
  const maximum = Buffer.alloc(SCREENSHOT_MAX_BYTES); Buffer.from('\x89PNG\r\n\x1a\n', 'latin1').copy(maximum);
  validateControlResponse({ sequence: 1, result: { ...png, byte_length: maximum.length, data_base64: maximum.toString('base64') } }, 1, { kind: 'observe_frame' });
});
test('configuration scopes and returned pairing objects cannot expand fixed authority', async () => {
  const host = await DeviceIdentity.generate(), device = await DeviceIdentity.generate();
  for (const scopes of [[], ['observe', 'input'], ['observe', 'observe'], ['unknown'], null]) await assert.rejects(AuthenticatedReceiver.fromIdentity(device, { pinnedHostPublicKey: host.publicKey, vmId: 'fixture-vm', scopes }));
  const options = { pinnedHostPublicKey: host.publicKey, vmId: 'fixture-vm', scopes: ['observe'] };
  const opening = AuthenticatedReceiver.fromIdentity(device, options); options.scopes.unshift('input');
  const receiver = await opening;
  const first = await receiver.pairingRequest(); first.pairing.scopes.unshift('input');
  assert.deepEqual((await receiver.pairingRequest()).pairing.scopes, ['observe']); receiver.dispose();
});
