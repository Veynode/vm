import test from 'node:test';
import assert from 'node:assert/strict';
import { EncryptedControlChannel, ControlChannelError, ControlFrameUnavailableError, CONTROL_MESSAGE_BYTES } from '../dist/control-channel.js';
import { FakeChannel, frames } from './control-channel-fixture.mjs';
const request = sequence => ({ request: { sequence }, signature: 'synthetic transport fixture only' });
const pause = () => new Promise(resolve => setTimeout(resolve, 5));
async function sent(channel) { for (let i = 0; i < 100; i++) { if (channel.sent.length) return; await pause(); } throw Error('No request frame'); }
test('fragmented request/near-limit screenshot response use exact bounded BE framing and no queued operations', async () => {
  const channel = new FakeChannel(async body => ({ sequence: body.request.sequence, result: { image: 'x'.repeat(950000) } }));
  const transport = new EncryptedControlChannel(channel, { isConfirmed: () => true });
  const signed = { ...request(1), padding: 'é'.repeat(60000) };
  const operation = transport.request(signed); await assert.rejects(transport.request(request(2)), /one control/);
  const value = await operation; assert.equal(value.result.image.length, 950000);
  assert.ok(channel.sent.length > 1); for (const frame of channel.sent) { assert.ok(frame.length <= 16384); assert.equal(frame[0], 1); assert.equal(frame.readBigUInt64BE(1), 1n); }
  await assert.rejects(transport.request(request(1)), /increase/); transport.close();
});
test('exact label/protocol/reliable ordering required and signing confirmation cannot be bypassed', async () => {
  for (const mutate of [c => c.label = 'other', c => c.protocol = '', c => c.ordered = false, c => c.negotiated = true, c => c.maxRetransmits = 0, c => c.maxPacketLifeTime = 1]) { const c = new FakeChannel(); mutate(c); assert.throws(() => new EncryptedControlChannel(c, { isConfirmed: () => true }), /reliable/); assert.equal(c.readyState, 'closed'); }
  const c = new FakeChannel(), t = new EncryptedControlChannel(c, { isConfirmed: () => false }); await assert.rejects(t.request(request(1)), /Confirmed/); assert.equal(c.sent.length, 0); t.close();
});
test('bad version/sequence/length/offset/UTF8/JSON/envelope and unsolicited data permanently fence channel', async () => {
  const invalid = [
    c => 'plaintext', c => new ArrayBuffer(17), c => new ArrayBuffer(16385),
    c => { const b = frames(1, { sequence: 1, result: {} })[0]; new DataView(b).setUint8(0, 2); return b; },
    c => frames(2, { sequence: 2, result: {} })[0],
    c => { const b = frames(1, { sequence: 1, result: {} })[0]; new DataView(b).setUint32(9, 1048577); return b; },
    c => { const b = frames(1, { sequence: 1, result: {} })[0]; new DataView(b).setUint32(13, 1); return b; },
    c => { const b = frames(1, 'x')[0]; new Uint8Array(b)[17] = 255; return b; },
    c => frames(1, '{bad')[0], c => frames(1, { sequence: 2, result: {} })[0],
    c => frames(1, { sequence: 1, result: {}, extra: true })[0], c => frames(1, { sequence: 1, error: 'sensitive arbitrary detail' })[0],
  ];
  for (const corrupt of invalid) { const c = new FakeChannel(), t = new EncryptedControlChannel(c, { isConfirmed: () => true }); const p = t.request(request(1)); await sent(c); c.emit(corrupt(c)); await assert.rejects(p); assert.equal(t.fenced, true); await assert.rejects(t.request(request(2))); }
  const c = new FakeChannel(), t = new EncryptedControlChannel(c, { isConfirmed: () => true }); c.emit(frames(1, { sequence: 1, result: {} })[0]); assert.equal(t.fenced, true);
});
test('changed fragment total/duplicate offset and response assembly deadline fail without returning partial image', async () => {
  for (const mode of ['total', 'duplicate', 'timeout']) {
    const c = new FakeChannel(), t = new EncryptedControlChannel(c, { isConfirmed: () => true, assemblyTimeoutMs: 20 }); const p = t.request(request(1)); await sent(c);
    const parts = frames(1, { sequence: 1, result: { value: 'x'.repeat(20000) } }); c.emit(parts[0]);
    if (mode === 'total') { new DataView(parts[1]).setUint32(9, new DataView(parts[1]).getUint32(9) + 1); c.emit(parts[1]); }
    if (mode === 'duplicate') c.emit(parts[0]);
    await assert.rejects(p); assert.equal(t.fenced, true);
  }
});
test('send highwater, open wait, total deadline, abort and authorization loss are bounded and fence', async () => {
  for (const mode of ['full', 'overflow', 'connecting', 'response', 'abort', 'revoke']) {
    let authorized = true; const controller = new AbortController(); const c = new FakeChannel();
    if (mode === 'full') c.bufferedAmount = 65536; if (mode === 'overflow') c.bufferedAmount = 65537; if (mode === 'connecting') c.readyState = 'connecting';
    const t = new EncryptedControlChannel(c, { isConfirmed: () => authorized, sendTimeoutMs: 20, requestTimeoutMs: 60 });
    const p = t.request(request(1), controller.signal); if (mode === 'abort') controller.abort(); if (mode === 'revoke') authorized = false;
    await assert.rejects(p); assert.equal(t.fenced, true); assert.equal(c.readyState, 'closed');
    if (['full', 'overflow', 'connecting'].includes(mode)) assert.equal(c.sent.length, 0);
  }
});
test('semantic rejection and idle transport close notify owner once, and no retry is sent', async () => {
  const c = new FakeChannel(body => ({ sequence: body.request.sequence, error: 'control rejected' })); let failures = 0;
  const t = new EncryptedControlChannel(c, { isConfirmed: () => true, onFailure: () => failures++ }); await assert.rejects(t.request(request(1)), /Control rejected/); await assert.rejects(t.request(request(2))); assert.equal(c.sent.length, 1); assert.equal(failures, 1); t.close(); assert.equal(failures, 1);
  const idle = new FakeChannel(body => ({ sequence: body.request.sequence, result: {} })); const owner = new EncryptedControlChannel(idle, { isConfirmed: () => true, onFailure: () => failures++ }); await owner.request(request(1)); idle.close(); assert.equal(owner.fenced, true); assert.equal(failures, 2);
});
test('oversized outgoing JSON is refused before allocating channel buffers or sending', async () => {
  const c = new FakeChannel(), t = new EncryptedControlChannel(c, { isConfirmed: () => true }); await assert.rejects(t.request({ ...request(1), data: 'x'.repeat(CONTROL_MESSAGE_BYTES) }), /too large/); assert.equal(c.sent.length, 0); t.close();
});
test('malformed response diagnostics never include file content from JSON parser errors',async()=>{let reported;const c=new FakeChannel(),t=new EncryptedControlChannel(c,{isConfirmed:()=>true,onFailure:error=>reported=error.message});const pending=t.request(request(1));await sent(c);c.emit(frames(1,'{"sensitive":"PRIVATE-FILE-CONTENT", trailing')[0]);await assert.rejects(pending,/^ControlChannelError: Malformed control response$/);assert.equal(reported,'Malformed control response');});
const action = (sequence, kind) => ({ request: { sequence, action: { kind } }, signature: 'synthetic transport fixture only' });
test('typed frame-unavailable reply to observe_frame rejects that one request, keeps the channel open and consumes the sequence', async () => {
  let failures = 0, answer = body => ({ sequence: body.request.sequence, error: 'frame unavailable', reason: 'controller_refused' });
  const c = new FakeChannel(body => answer(body)), t = new EncryptedControlChannel(c, { isConfirmed: () => true, onFailure: () => failures++ });
  await assert.rejects(t.request(action(1, 'observe_frame')), error => error instanceof ControlFrameUnavailableError && !(error instanceof ControlChannelError) && error.reason === 'controller_refused' && error.name === 'ControlFrameUnavailableError');
  assert.equal(t.fenced, false); assert.equal(c.readyState, 'open'); assert.equal(failures, 0);
  // The sequence was consumed, as on the host: it cannot be reused, and that refusal does not fence either.
  await assert.rejects(t.request(action(1, 'observe_frame')), /Control sequence must increase/); assert.equal(t.fenced, false); assert.equal(c.sent.length, 1);
  // Every reason word the service uses today, and the longest word the pattern admits.
  let sequence = 1;
  for (const reason of ['controller_deadline', 'png_decode_unsupported', 'scaled_frame_over_cap', 'a'.repeat(32)]) {
    answer = body => ({ sequence: body.request.sequence, error: 'frame unavailable', reason });
    await assert.rejects(t.request(action(++sequence, 'observe_frame')), error => error instanceof ControlFrameUnavailableError && error.reason === reason); assert.equal(t.fenced, false);
  }
  // The next picture and the next input use the same channel; no timer of the answered request is left behind.
  answer = body => ({ sequence: body.request.sequence, result: { kind: body.request.action.kind } });
  assert.deepEqual(await t.request(action(++sequence, 'observe_frame')), { sequence, result: { kind: 'observe_frame' } });
  assert.deepEqual(await t.request(action(++sequence, 'key')), { sequence, result: { kind: 'key' } });
  assert.equal(c.sent.length, 7); assert.equal(failures, 0); t.close(); assert.equal(failures, 1);
});
test('frame-unavailable reply to any request that is not observe_frame is a protocol violation and fences', async () => {
  for (const signed of [action(1, 'acquire'), action(1, 'release'), action(1, 'key'), action(1, 'click'), action(1, 'file'), request(1), { request: { sequence: 1, action: null }, signature: 'x' }]) {
    let failures = 0; const c = new FakeChannel(body => ({ sequence: body.request.sequence, error: 'frame unavailable', reason: 'controller_refused' }));
    const t = new EncryptedControlChannel(c, { isConfirmed: () => true, onFailure: () => failures++ });
    await assert.rejects(t.request(signed), error => error instanceof ControlChannelError && error.message === 'Invalid control response fields');
    assert.equal(t.fenced, true); assert.equal(c.readyState, 'closed'); assert.equal(failures, 1); await assert.rejects(t.request(action(2, 'observe_frame'))); assert.equal(c.sent.length, 1);
  }
});
test('malformed frame-unavailable replies still fence: reason, error text, key set and sequence are exact', async () => {
  const replies = [
    { error: 'frame unavailable', reason: 'Controller_refused' }, { error: 'frame unavailable', reason: 'controller refused' }, { error: 'frame unavailable', reason: '' },
    { error: 'frame unavailable', reason: 'a'.repeat(33) }, { error: 'frame unavailable', reason: 'png2' }, { error: 'frame unavailable', reason: 7 }, { error: 'frame unavailable', reason: null },
    { error: 'frame unavailable', reason: ['controller_refused'] }, { error: 'frame unavailable', reason: 'controller_refused\n' }, { error: 'control rejected', reason: 'controller_refused' },
    { error: 'Frame unavailable', reason: 'controller_refused' }, { error: 'frame unavailable', reason: 'controller_refused', result: {} }, { error: 'frame unavailable', reason: 'controller_refused', detail: 'x' },
    { error: 'frame unavailable' }, { result: {}, reason: 'controller_refused' },
  ];
  for (const reply of replies) {
    const c = new FakeChannel(body => ({ sequence: body.request.sequence, ...reply })), t = new EncryptedControlChannel(c, { isConfirmed: () => true });
    await assert.rejects(t.request(action(1, 'observe_frame')), error => error instanceof ControlChannelError, JSON.stringify(reply));
    assert.equal(t.fenced, true, JSON.stringify(reply)); assert.equal(c.readyState, 'closed');
  }
  const c = new FakeChannel(body => ({ sequence: body.request.sequence + 1, error: 'frame unavailable', reason: 'controller_refused' })), t = new EncryptedControlChannel(c, { isConfirmed: () => true });
  await assert.rejects(t.request(action(1, 'observe_frame')), /Invalid control response envelope/); assert.equal(t.fenced, true);
});
