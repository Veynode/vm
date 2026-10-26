import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { spawnSync } from 'node:child_process';
import { DeviceIdentity, hex, grantBytes, answerBytes, sdpBinding, verifyOffer, verifyConfirmation, verifyClose } from '../dist/session-auth.js';
import { controlBytes, signControl, verifyControl, validateControlResponse, screenshotAnchor, verifiedScreenshotSource, SCREENSHOT_MAX_BYTES, CONTROL_FRAME_MAX_DIVISOR, controlFrameGeometry, checkDecodedControlFrame, sourceToFrame, frameToSource, pointerToSource } from '../dist/session-control.js';
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

// Header-only JPEG: SOI, a baseline frame header with the given size, EOI. These are protocol/structure tests;
// no image decoder exists in node, and real decoding is the page's (and a real browser's) job.
const jpegOf = (width, height) => Buffer.from([0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0, 0xff, 0xc0, 0, 17, 8, height >> 8, height & 255, width >> 8, width & 255, 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1, 0xff, 0xd9]);
const scaledFrame = (width, height, divisor, real = [Math.ceil(width / divisor), Math.ceil(height / divisor)]) => {
  const bytes = jpegOf(...real);
  return { ...frame, width, height, frame_width: Math.ceil(width / divisor), frame_height: Math.ceil(height / divisor), scale_divisor: divisor, mime_type: 'image/jpeg', byte_length: bytes.length, data_base64: bytes.toString('base64'), sha256: createHash('sha256').update(bytes).digest('hex') };
};
const checkFrame = result => validateControlResponse({ sequence: 1, result }, 1, { kind: 'observe_frame' });

test('scaled 2560x1440 and 3840x2160 control frames validate; the anchor stays the source geometry', async () => {
  for (const [width, height, divisor] of [[2560, 1440, 2], [3840, 2160, 2], [3840, 2160, 3], [3840, 2160, 16], [1366, 768, 4], [2560, 1440, 1]]) {
    const result = scaledFrame(width, height, divisor); checkFrame(result);
    assert.deepEqual(screenshotAnchor(result), { ...frame, width, height });
    assert.deepEqual(controlFrameGeometry(result), { source_width: width, source_height: height, frame_width: Math.ceil(width / divisor), frame_height: Math.ceil(height / divisor), scale_divisor: divisor });
    assert.equal(await verifiedScreenshotSource(result), `data:image/jpeg;base64,${result.data_base64}`);
    // A signed click at the far source corner is accepted against the source geometry; one past it is not.
    const anchor = screenshotAnchor(result), request = (x, y) => ({ session_id: 's', transcript_sha256: '00'.repeat(32), sequence: 1, expires_at: 1, action: { kind: 'click', frame: anchor, x, y, button: 'left' } });
    controlBytes(request(width - 1, height - 1)); assert.throws(() => controlBytes(request(width, 0))); assert.throws(() => controlBytes(request(0, height)));
  }
});
test('a frame whose declared scale disagrees with its geometry or real dimensions is refused', async () => {
  const good = scaledFrame(2560, 1440, 2); checkFrame(good);
  for (const mutate of [
    r => r.scale_divisor = 3, r => r.scale_divisor = 1, r => r.scale_divisor = 0, r => r.scale_divisor = CONTROL_FRAME_MAX_DIVISOR + 1,
    r => r.scale_divisor = 2.5, r => r.scale_divisor = '2', r => delete r.scale_divisor, r => delete r.frame_width, r => delete r.frame_height,
    r => r.frame_width++, r => r.frame_height--, r => r.frame_width = 0, r => r.width = 2562, r => r.height = 1442,
  ]) {
    const changed = structuredClone(good); mutate(changed);
    assert.throws(() => checkFrame(changed)); assert.throws(() => controlFrameGeometry(changed)); await assert.rejects(verifiedScreenshotSource(changed));
  }
  // Self-consistent declaration, but the image header really has other dimensions.
  for (const real of [[1280, 721], [1279, 720], [2560, 1440], [720, 1280]]) {
    const changed = scaledFrame(2560, 1440, 2, real);
    assert.throws(() => checkFrame(changed), /real dimensions/); await assert.rejects(verifiedScreenshotSource(changed), /real dimensions/);
  }
  { const changed = { ...good, width: 5120, height: 2880, frame_width: 2560, frame_height: 1440 }; controlFrameGeometry(changed); assert.throws(() => checkFrame(changed), /real dimensions/); await assert.rejects(verifiedScreenshotSource(changed), /real dimensions/); }
  // Same for an unscaled JPEG, and a JPEG with no frame header at all.
  assert.throws(() => checkFrame(scaledFrame(1000, 500, 1, [999, 500])), /real dimensions/);
  const headless = Buffer.from([0xff, 0xd8, 0xff, 0xd9]);
  assert.throws(() => checkFrame({ ...good, byte_length: headless.length, data_base64: headless.toString('base64'), sha256: createHash('sha256').update(headless).digest('hex') }), /frame header/);
  // A PNG is always unscaled.
  assert.throws(() => checkFrame({ ...png, frame_width: 640, frame_height: 400, scale_divisor: 2 }), /must not declare/);
  assert.throws(() => checkFrame({ ...png, frame_width: 1280, frame_height: 800, scale_divisor: 1 }), /must not declare/);
  // The decoder's verdict is checked against the declared frame, never against the source.
  const geometry = controlFrameGeometry(good); checkDecodedControlFrame(geometry, 1280, 720);
  for (const [w, h] of [[2560, 1440], [1280, 721], [1279, 720], [0, 0]]) assert.throws(() => checkDecodedControlFrame(geometry, w, h), /do not match/);
});
test('an unscaled small frame behaves as before', async () => {
  checkFrame(png);
  assert.deepEqual(controlFrameGeometry(png), { source_width: 1280, source_height: 800, frame_width: 1280, frame_height: 800, scale_divisor: 1 });
  checkDecodedControlFrame(controlFrameGeometry(png), 1280, 800); assert.throws(() => checkDecodedControlFrame(controlFrameGeometry(png), 640, 400));
  assert.equal(await verifiedScreenshotSource(png), `data:image/png;base64,${png.data_base64}`);
  for (const extent of [1280, 800]) for (let s = 0; s < extent; s++) { assert.equal(sourceToFrame(s, 1), s); assert.equal(frameToSource(s, 1, extent), s); assert.equal(pointerToSource(s, extent, extent, 1, extent), s); assert.equal(pointerToSource(s / 2, extent / 2, extent, 1, extent), s); }
});
test('coordinate round trip: exact from the frame, within floor(divisor / 2) from the source, corners and centre', () => {
  for (const [width, height] of [[2560, 1440], [3840, 2160], [1366, 768], [1, 1]]) for (let divisor = 1; divisor <= CONTROL_FRAME_MAX_DIVISOR; divisor++) {
    const tolerance = Math.floor(divisor / 2);
    for (const extent of [width, height]) {
      const frames = Math.ceil(extent / divisor);
      for (let f = 0; f < frames; f++) { const s = frameToSource(f, divisor, extent); if (s < 0 || s >= extent || sourceToFrame(s, divisor) !== f) assert.fail(`frame ${f} / ${divisor} of ${extent}`); }
      for (let s = 0; s < extent; s++) { const f = sourceToFrame(s, divisor); if (f >= frames || Math.abs(frameToSource(f, divisor, extent) - s) > tolerance) assert.fail(`source ${s} / ${divisor} of ${extent}`); }
      // A pointer anywhere inside displayed frame pixel f lands in f's own source block (partial last block
      // included), whatever CSS size the frame is shown at: exact registration with the picture.
      for (const zoom of [1, 0.5, 3]) for (let f = 0; f < frames; f++) for (const inside of [0, 0.5, 0.999]) { const s = pointerToSource((f + inside) * zoom, frames * zoom, frames, divisor, extent); if (s >= extent || sourceToFrame(s, divisor) !== f) assert.fail(`pointer in frame ${f} / ${divisor} of ${extent} at zoom ${zoom}`); }
    }
    for (const [sx, sy] of [[0, 0], [width - 1, 0], [0, height - 1], [width - 1, height - 1], [width >> 1, height >> 1]]) {
      const fx = sourceToFrame(sx, divisor), fy = sourceToFrame(sy, divisor);
      assert.ok(fx < Math.ceil(width / divisor) && fy < Math.ceil(height / divisor));
      const bx = frameToSource(fx, divisor, width), by = frameToSource(fy, divisor, height);
      assert.ok(bx < width && by < height && Math.abs(bx - sx) <= tolerance && Math.abs(by - sy) <= tolerance, `${width}x${height} / ${divisor} at ${sx},${sy}`);
      if (divisor === 1) assert.deepEqual([bx, by], [sx, sy]);
    }
  }
  // Pointer mapping over a displayed box of any size: corners and centre of a 3840x2160 source.
  for (const [displayedWidth, displayedHeight] of [[3840, 2160], [1920, 1080], [960, 540], [1234.5, 694.40625], [7680, 4320]]) {
    const x = offset => pointerToSource(offset, displayedWidth, 1920, 2, 3840), y = offset => pointerToSource(offset, displayedHeight, 1080, 2, 2160);
    assert.deepEqual([x(0), y(0)], [0, 0]);
    assert.deepEqual([x(displayedWidth - 1e-6), y(displayedHeight - 1e-6)], [3839, 2159]);
    assert.deepEqual([x(displayedWidth / 2), y(displayedHeight / 2)], [1920, 1080]);
    // One displayed pixel spans source / displayed source pixels: that is the pointing resolution, stated and bounded.
    const span = Math.ceil(3840 / displayedWidth);
    for (const offset of [0, 1, 17, Math.floor(displayedWidth) - 1]) { const s = x(offset); assert.ok(Math.abs(s - offset * 3840 / displayedWidth) < 1 && x(offset + 0.999) - s <= span); }
  }
  for (const [offset, extent] of [[-0.001, 100], [100, 100], [NaN, 100], [1, 0], [1, Infinity]]) assert.throws(() => pointerToSource(offset, extent, 1920, 2, 3840));
  // The pointer rule refuses a frame extent that is not ceil(source / divisor).
  assert.throws(() => pointerToSource(1, 100, 1920, 3, 3840)); assert.throws(() => pointerToSource(1, 100, 1921, 2, 3840)); assert.throws(() => pointerToSource(1, 100, 8193, 1, 8193));
  assert.throws(() => frameToSource(0, 0, 100)); assert.throws(() => sourceToFrame(1, 17));
});
