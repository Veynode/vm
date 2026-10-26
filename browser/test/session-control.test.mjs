import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { spawnSync } from 'node:child_process';
import { DeviceIdentity, hex, grantBytes, answerBytes, sdpBinding, verifyOffer, verifyConfirmation, verifyClose } from '../dist/session-auth.js';
import { controlBytes, signControl, verifyControl, validateControlResponse, screenshotAnchor, verifiedScreenshotSource, SCREENSHOT_MAX_BYTES } from '../dist/session-control.js';
import { AuthenticatedReceiver } from '../dist/native-auth.js';

const pairBinary = process.env.VEYNODE_PAIR_BIN ?? new URL('../../target/debug/vm-pair', import.meta.url).pathname;
const frame = { frame_id: '12345678-1234-4234-8234-123456789abc', source_generation: 1, width: 1280, height: 800 };
const click = () => ({ kind: 'click', frame: structuredClone(frame), x: 25, y: 40, button: 'left' });
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
    if (path === '/auth/control') throw new Error('Plaintext HTTP control is forbidden');
    throw new Error('Unexpected endpoint');
  };
  receiver.bindControlTransport(async body => {
      await verifyControl(body, state.transcript, device.publicKey, Math.floor(Date.now() / 1000)); state.controls.push(body);
      if (state.controlReply) return (await state.controlReply(body)).json();
      const action = body.request.action;
      const result = action.kind === 'observe_frame' ? png : { dispatched: true, guest_delivery_confirmed: false };
      return { sequence: body.request.sequence, result };
  });
  return { host, device, receiver, state, calls, fetcher };
}
async function withSession(callback, scopes) {
  const f = await fixture(scopes), previous = globalThis.fetch; globalThis.fetch = f.fetcher;
  try { await f.receiver.offer(sdp('AA'), 'control-request', 'h264'); await callback(f); }
  finally { assert.equal(f.calls.includes('/auth/control'), false); f.state.close = true; await f.receiver.close(); f.receiver.dispose(); globalThis.fetch = previous; }
}

test('Rust golden canonical bytes and P256 signatures match every browser control variant', async () => {
  const golden = JSON.parse(await readFile(new URL('../../crates/session-auth/tests/control-golden.json', import.meta.url), 'utf8'));
  assert.equal(golden.cases.length, 5);
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
      r => r.action.epoch = 1, r => r.action.x = 1280, r => r.action.y = -1, r => r.action.x = .5,
      r => r.action.button = 'back', r => r.action.frame.extra = true, r => r.action.frame.frame_id = frame.frame_id.toUpperCase(),
      r => r.action.frame.source_generation = -1, r => r.action.frame.width = 8193,
      r => r.action = { kind: 'release' }, r => r.action = { kind: 'acquire' },
      r => r.action = { kind: 'text', text: 'not-supported' },
      r => r.action = { kind: 'key', frame, keys: [''] },
      r => r.action = { kind: 'key', frame, keys: ['ctrl', 'ctrl'] },
      r => r.action = { kind: 'key', frame, keys: ['a'.repeat(17)] },
      r => r.action = { kind: 'key', frame, keys: ['A'] },
      r => r.action = { kind: 'scroll', frame, x: 0, y: 0, direction: 'up', ticks: 21 }]) {
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
test('control requires confirmed session, fixed scopes, and every action variant works', async () => {
  await withSession(async f => {
    await assert.rejects(f.receiver.control({ kind: 'observe_frame' }), /confirmed/);
    f.state.confirm = false; await assert.rejects(f.receiver.confirm(), /not acknowledged/);
    await assert.rejects(f.receiver.control({ kind: 'observe_frame' }), /confirmed/);
    f.state.confirm = true; await f.receiver.confirm();
    const actions = [{ kind: 'observe_frame' }, click(),
      { ...click(), kind: 'drag', end_x: 30, end_y: 50 },
      { kind: 'scroll', frame, x: 0, y: 0, direction: 'down', ticks: 2 },
      { kind: 'key', frame, keys: ['ctrl', 'a'] }];
    for (const action of actions) await f.receiver.control(action);
    assert.deepEqual(f.state.controls.map(s => s.request.sequence), [1, 2, 3, 4, 5]);
    assert.deepEqual(screenshotAnchor((await f.receiver.control({ kind: 'observe_frame' })).result), frame);
  });
  await withSession(async f => { await f.receiver.confirm(); await assert.rejects(f.receiver.control(click()), /scope/); assert.equal(f.state.controls.length, 0); await f.receiver.control({ kind: 'observe_frame' }); }, ['observe']);
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
    f.state.controlReply = undefined; await assert.rejects(f.receiver.control(click()), /confirmed/); assert.deepEqual(f.state.controls.map(s => s.request.sequence), [1]);
    f.state.close = false; await assert.rejects(f.receiver.close(), /not acknowledged/);
    await assert.rejects(f.receiver.control(click()), /confirmed/); await assert.rejects(f.receiver.confirm(), /No verified/);
    assert.equal(f.state.controls.length, 1);
  });
});
test('bound transport is mandatory, cannot be rebound during a session, and wrong sequence fences control', async () => {
  await withSession(async f => {
    await f.receiver.confirm(); assert.throws(() => f.receiver.bindControlTransport(async () => ({})), /before offering/);
    f.state.controlReply = body => Response.json({ sequence: body.request.sequence + 1, result: png });
    await assert.rejects(f.receiver.control({ kind: 'observe_frame' }), /sequence/);
    await assert.rejects(f.receiver.control({ kind: 'observe_frame' }), /confirmed/);
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

test('unbound confirmed session rejects control without HTTP; only genuinely unopened transports can clear', async () => {
  const f = await fixture(), previous = globalThis.fetch; globalThis.fetch = f.fetcher;
  try {
    f.receiver.clearUnopenedControlTransport();
    let closed = 0;
    f.receiver.bindControlTransport(async () => { throw Error('unused'); }, () => { closed++; });
    assert.throws(() => f.receiver.bindControlTransport(async () => ({})), /once before/);
    f.receiver.clearUnopenedControlTransport(); assert.equal(closed, 1);
    await f.receiver.offer(sdp('AA'), 'unbound-request', 'h264'); await f.receiver.confirm();
    await assert.rejects(f.receiver.control({ kind: 'observe_frame' }), /Encrypted control channel required; HTTP control is disabled/);
    assert.throws(() => f.receiver.clearUnopenedControlTransport(), /admission/);
    assert.throws(() => f.receiver.bindControlTransport(async () => ({})), /before offering/);
    assert.equal(f.calls.includes('/auth/control'), false); await f.receiver.close();
  } finally { await f.receiver.close(); f.receiver.dispose(); globalThis.fetch = previous; }
});
test('failed offer cannot discard its transport or bind another while admission is uncertain', async () => {
  // The old fixture threw during challenge, which is provably pre-transmission.
  // This negative case must reach /auth/offer before losing the answer.
  const f = await fixture(), previous = globalThis.fetch; globalThis.fetch = async (path, options) => {
    if (path === '/auth/offer') { await f.fetcher(path, options); throw Error('response lost'); }
    return f.fetcher(path, options);
  };
  try {
    await assert.rejects(f.receiver.offer(sdp('AA'), 'uncertain-request', 'h264'), /response lost/);
    assert.throws(() => f.receiver.clearUnopenedControlTransport(), /admission/);
    assert.throws(() => f.receiver.bindControlTransport(async () => ({})), /before offering/);
    assert.equal(f.calls.includes('/auth/control'), false);
  } finally { assert.throws(() => f.receiver.dispose(), /Close the session/); globalThis.fetch = previous; }
});

test('JPEG and PNG screenshot sources verify bytes and hashes without inventing anchors', async () => {
  const bytes = await readFile(new URL('./fixtures/screenshot-1000x500.jpg', import.meta.url));
  const jpeg = { ...frame, width: 1000, height: 500, input_anchor_available: true, mime_type: 'image/jpeg', byte_length: bytes.length, data_base64: bytes.toString('base64'), sha256: createHash('sha256').update(bytes).digest('hex') };
  const check = result => validateControlResponse({ sequence: 1, result }, 1, { kind: 'observe_frame' });
  check(jpeg); check(png);
  assert.equal(await verifiedScreenshotSource(jpeg), `data:image/jpeg;base64,${jpeg.data_base64}`);
  assert.equal(await verifiedScreenshotSource(png), `data:image/png;base64,${png.data_base64}`);
  const hashedPNG = { ...png, sha256: createHash('sha256').update(Buffer.from(png.data_base64, 'base64')).digest('hex') };
  await verifiedScreenshotSource(hashedPNG);
  for (const mutate of [
    r => r.mime_type = 'image/jpg', r => r.mime_type = 'image/png', r => delete r.sha256,
    r => r.sha256 = r.sha256.toUpperCase(), r => r.sha256 = 42,
    r => r.byte_length++, r => r.byte_length = SCREENSHOT_MAX_BYTES + 1,
    r => delete r.source_generation, r => r.input_anchor_available = false,
    r => r.input_anchor_available = 'true', r => r.data_base64 = 'a',
  ]) {
    const changed = structuredClone(jpeg); mutate(changed);
    assert.throws(() => check(changed)); await assert.rejects(verifiedScreenshotSource(changed));
  }
  for (const changedBytes of [bytes.subarray(1), bytes.subarray(0, -1), Buffer.concat([bytes, Buffer.from([0])])]) {
    const changed = { ...jpeg, byte_length: changedBytes.length, data_base64: changedBytes.toString('base64') };
    assert.throws(() => check(changed), /boundaries/);
  }
  for (const original of [jpeg, hashedPNG]) await assert.rejects(verifiedScreenshotSource({ ...original, sha256: '00'.repeat(32) }), /SHA-256 mismatch/);
  const mutable = structuredClone(jpeg), verifying = verifiedScreenshotSource(mutable);
  mutable.data_base64 = png.data_base64; mutable.mime_type = 'text/html';
  assert.equal(await verifying, `data:image/jpeg;base64,${jpeg.data_base64}`, 'Awaiting digest cannot swap the checked payload');
  assert.throws(() => screenshotAnchor({ ...jpeg, input_anchor_available: false }), /no usable input anchor/);
});
test('files-only signatures verify in Rust and semantic file refusal leaves the session usable',async()=>withSession(async f=>{
 const action={kind:'file',operation:{kind:'upload_begin',path:'澳洲.bin',size:3,sha256:createHash('sha256').update('abc').digest('hex')}};
 const signed=await signControl(f.device,f.state.transcript,action,{sequence:1,now:Math.floor(Date.now()/1000)});
 const request={kind:'control',device_public_key:f.device.publicKey,transcript:f.state.transcript,signed};
 const result=spawnSync(pairBinary,['verify'],{input:JSON.stringify(request),encoding:'utf8',timeout:5000});assert.equal(result.status,0,result.stderr);
 request.signed.request.action.operation.path='substituted';assert.notEqual(spawnSync(pairBinary,['verify'],{input:JSON.stringify(request),encoding:'utf8',timeout:5000}).status,0);
 await assert.rejects(signControl(f.device,f.state.transcript,click(),{sequence:2,now:Math.floor(Date.now()/1000)}),/scope/);
 await f.receiver.confirm();f.state.controlReply=body=>Response.json({sequence:body.request.sequence,result:{ok:false,error:{code:'forbidden'}}});
 assert.equal((await f.receiver.control({kind:'file',operation:{kind:'download_begin',path:'private'}})).result.ok,false);assert.equal(f.receiver.controlConfirmed,true);
 assert.equal((await f.receiver.control({kind:'file',operation:{kind:'download_begin',path:'private'}})).result.ok,false);
},['files','observe']));
