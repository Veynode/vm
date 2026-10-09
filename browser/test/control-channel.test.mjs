import test from 'node:test';
import assert from 'node:assert/strict';
import { EncryptedControlChannel, CONTROL_MESSAGE_BYTES } from '../dist/control-channel.js';
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
