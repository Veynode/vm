import test from 'node:test';
import assert from 'node:assert/strict';
import { createCodecPlan, codecPreferences, negotiateCodec, CodecError, SourceFailureError, mediaFailureFromReceipt } from '../dist/codec-policy.js';
import { sdpCodecs } from '../dist/loopback.js';
const h264 = { mimeType: 'video/H264', clockRate: 90000, sdpFmtpLine: 'packetization-mode=1;profile-level-id=42e01f' };
const high264 = { ...h264, sdpFmtpLine: 'packetization-mode=1;profile-level-id=640c1f' };
const h265 = { mimeType: 'video/H265', clockRate: 90000, sdpFmtpLine: 'profile-id=1;level-id=93;tier-flag=0' };
const rtx = { mimeType: 'video/rtx', clockRate: 90000 };
const caps = [{ mimeType: 'video/VP8', clockRate: 90000 }, h264, rtx, high264, h265];
const host = { h264: 'verified-hardware', h265: 'verified-hardware' };
const plan = () => createCodecPlan(host, caps);
const success = codec => ({ codec: `video/${codec.toUpperCase()}`, framesDecoded: 1 });
test('H.265 preference retains original H.264 profiles and repair codecs, excludes other media', () => {
  const prefs = codecPreferences(caps, 'h265');
  assert.deepEqual(prefs, [h265, h264, high264, rtx]);
  assert.equal(prefs[0], h265); assert.equal(prefs[1], h264);
  assert.deepEqual(codecPreferences(caps, 'h264'), [h264, high264, rtx]);
});
test('codec absence cannot silently restore browser defaults', () => {
  assert.throws(() => codecPreferences([rtx], 'h264'), { code: 'codec_unavailable' });
  assert.throws(() => createCodecPlan(host, [h265]), { code: 'browser_h264_unavailable' });
});
test('hardware status is mandatory; HEVC file playback or UA cannot establish capability', () => {
  for (const status of ['unavailable', 'unverified']) {
    assert.throws(() => createCodecPlan({ ...host, h264: status }, caps), { code: 'hardware_h264_required' });
    assert.equal(createCodecPlan({ ...host, h265: status }, caps).preferred, 'h264');
  }
  assert.equal(createCodecPlan(host, [h264]).reason, 'browser_h265_unavailable');
  assert.equal(plan().preferred, 'h265');
});
test('H.265 actual decoded frame succeeds without retry', async () => {
  const calls = []; const result = await negotiateCodec(plan(), async codec => { calls.push(codec); return success(codec); });
  assert.deepEqual(calls, ['h265']); assert.equal(result.selected, 'h265'); assert.equal(result.fallbackReason, null);
});
test('actual H.264 selection during preferred offer is explicit fallback', async () => {
  const result = await negotiateCodec(plan(), async () => success('h264'));
  assert.equal(result.selected, 'h264'); assert.equal(result.fallbackReason, 'peer_selected_h264'); assert.equal(result.attempts.length, 1);
});
for (const failure of ['negotiation_failed', 'first_frame_timeout']) {
  test(`${failure} falls back once to H.264`, async () => {
    const calls = []; const result = await negotiateCodec(plan(), async codec => { calls.push(codec); if (codec === 'h265') throw new CodecError(failure, 'fixture failure'); return success(codec); });
    assert.deepEqual(calls, ['h265', 'h264']); assert.equal(result.selected, 'h264'); assert.equal(result.fallbackReason, failure);
  });
}
test('both codecs fail: bounded attempts, no VP8/software third attempt', async () => {
  const calls = [];
  await assert.rejects(negotiateCodec(plan(), async codec => { calls.push(codec); throw new Error('failed'); }), error => error.code === 'no_working_hardware_codec' && error.attempts.length === 2);
  assert.deepEqual(calls, ['h265', 'h264']);
});
test('deadline aborts a stuck attempt and advances exactly once', async () => {
  let aborted = false;
  const result = await negotiateCodec(plan(), async (codec, signal) => {
    if (codec === 'h264') { assert.equal(aborted, true); return success(codec); }
    signal.addEventListener('abort', () => { aborted = true; }); return new Promise(() => {});
  }, undefined, 15);
  assert.equal(result.fallbackReason, 'attempt_timeout');
});
test('cancel ends active attempt without fallback', async () => {
  const controller = new AbortController(); let calls = 0;
  await assert.rejects(negotiateCodec(plan(), async () => { calls++; queueMicrotask(() => controller.abort()); return new Promise(() => {}); }, controller.signal), { code: 'cancelled' });
  assert.equal(calls, 1);
});
test('zero decoded frames and forbidden media are not successes', async () => {
  for (const result of [{ codec: 'video/H264', framesDecoded: 0 }, { codec: 'video/VP8', framesDecoded: 10 }]) {
    await assert.rejects(negotiateCodec(createCodecPlan(host, [h264]), async () => result), { code: 'no_working_hardware_codec' });
  }
});
test('SDP summary records codec profiles without ICE credentials', () => {
  assert.deepEqual(sdpCodecs('a=ice-pwd:secret\r\na=rtpmap:100 H265/90000\r\na=fmtp:100 profile-id=1\r\n'), [{ payload: '100', name: 'H265/90000', fmtp: 'profile-id=1' }]);
});

test('already cancelled never opens an attempt', async () => {
  const controller = new AbortController(); controller.abort(); let calls = 0;
  await assert.rejects(negotiateCodec(plan(), async () => { calls++; return success('h265'); }, controller.signal), { code: 'cancelled' });
  assert.equal(calls, 0);
});
test('cancel aborts owned resources; a late answer cannot turn cancellation into success', async () => {
  const controller = new AbortController(); let resolveLate; let cleaned = 0; let calls = 0;
  const pending = negotiateCodec(plan(), async (_, signal) => {
    calls++; signal.addEventListener('abort', () => { cleaned++; }, { once: true });
    return new Promise(resolve => { resolveLate = resolve; });
  }, controller.signal);
  controller.abort();
  await assert.rejects(pending, { code: 'cancelled' });
  resolveLate(success('h265'));
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(cleaned, 1); assert.equal(calls, 1);
});
test('late preferred answer after timeout does not replace the verified fallback', async () => {
  let resolveLate; let closed = false;
  const result = await negotiateCodec(plan(), async (codec, signal) => {
    if (codec === 'h264') { assert.equal(closed, true); return success(codec); }
    signal.addEventListener('abort', () => { closed = true; }, { once: true });
    return new Promise(resolve => { resolveLate = resolve; });
  }, undefined, 15);
  resolveLate(success('h265'));
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(result.selected, 'h264'); assert.equal(result.attempts.length, 2);
});

test('native H264 accepts RFC6184 equivalent CB profiles, mode1 and level>=3.1 only', async () => {
  const { nativeCodecPreferences } = await import('../dist/codec-policy.js');
  for (const profile of ['42c01f', '42e01f', '4d801f', '58c01f', '42e020', '42e028']) {
    const capability = { ...h264, sdpFmtpLine: `packetization-mode=1;profile-level-id=${profile}` };
    assert.equal(nativeCodecPreferences([capability], 'h264')[0], capability);
  }
  for (const fmtp of ['packetization-mode=1', 'profile-level-id=42e01f', 'packetization-mode=0;profile-level-id=42e01f', 'packetization-mode=1;profile-level-id=640c1f', 'packetization-mode=1;profile-level-id=4d001f', 'packetization-mode=1;profile-level-id=42001f', 'packetization-mode=1;profile-level-id=42e01e', 'packetization-mode=1;profile-level-id=42e4ff', 'packetization-mode=1;profile-level-id=42e01f;profile-level-id=640c1f']) {
    assert.throws(() => nativeCodecPreferences([{ ...h264, sdpFmtpLine: fmtp }], 'h264'), { code: 'unsupported_profile' }, fmtp);
  }
});
test('native HEVC filters Main10, tier1, other transmission modes and too-low level', async () => {
  const { nativeCodecPreferences, createNativeCodecPlan } = await import('../dist/codec-policy.js');
  const main = { ...h265, sdpFmtpLine: 'profile-id=1;tier-flag=0;tx-mode=SRST;level-id=120' };
  assert.deepEqual(nativeCodecPreferences([main, high264, h264, rtx], 'h265'), [main, h264, rtx]);
  const defaultMain = { ...h265, sdpFmtpLine: 'level-id=120' };
  assert.equal(nativeCodecPreferences([defaultMain, h264], 'h265')[0], defaultMain);
  for (const fmtp of ['profile-id=2;level-id=120', 'profile-id=1;tier-flag=1;level-id=120', 'profile-id=1;tx-mode=MRST;level-id=120', 'profile-id=1;level-id=93', 'profile-id=1', 'profile-space=1;level-id=120', 'level-id=255', 'level-id=120;level-id=93']) {
    const capability = { ...h265, sdpFmtpLine: fmtp };
    assert.throws(() => nativeCodecPreferences([capability, h264], 'h265'), { code: 'unsupported_profile' }, fmtp);
    const filteredPlan = createNativeCodecPlan(host, [capability, h264]);
    assert.equal(filteredPlan.preferred, 'h264'); assert.equal(filteredPlan.reason, 'browser_h265_profile_unavailable');
  }
});

test('source failure after acknowledged cleanup stops codec fallback; cleanup uncertainty takes precedence', async () => {
  for (const failure of [new SourceFailureError({ code: 'guest_display_asleep' }), new SourceFailureError({ code: 'guest_capture_no_frames' }), new CodecError('cleanup_unconfirmed', 'Cleanup not confirmed')]) {
    const calls = [];
    await assert.rejects(negotiateCodec(plan(), async codec => { calls.push(codec); throw failure; }), error => error.code === failure.code && error.attempts.length === 1);
    assert.deepEqual(calls, ['h265']);
  }
});
test('source diagnostics have only fixed messages and never infer lock from unknown code', () => {
  assert.equal(mediaFailureFromReceipt({ closed: true }), null);
  assert.deepEqual(mediaFailureFromReceipt({ closed: true, media_failure: { code: 'guest_display_asleep' } }), { code: 'guest_display_asleep' });
  const generic = mediaFailureFromReceipt({ closed: true, media_failure: { code: '/private/SECRET' } });
  assert.equal(generic.code, 'guest_capture_failed'); assert.doesNotMatch(new SourceFailureError(generic).message, /SECRET|asleep|lock/);
  for (const media_failure of [null, [], {}, { code: 1 }, { code: 'guest_display_asleep', extra: 'SECRET' }]) assert.throws(() => mediaFailureFromReceipt({ closed: true, media_failure }));
});
