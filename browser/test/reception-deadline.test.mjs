import test from 'node:test';
import assert from 'node:assert/strict';
import { ReceptionDeadline, negotiateCodec, CodecError, createCodecPlan } from '../dist/codec-policy.js';
const flush = async () => { for (let i = 0; i < 12; i++) await Promise.resolve(); };
function fixture(t, initiallyConnected = false, initiallyConfirmed = true) {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const errors = [], phases = []; let connected = initiallyConnected, confirmed = initiallyConfirmed;
  const deadline = new ReceptionDeadline({ connected: () => connected, confirmed: () => confirmed, onTimeout: code => errors.push(code), onPhase: phase => phases.push(phase) });
  return { deadline, errors, phases, connect: () => { connected = true; deadline.update(); }, confirm: () => { confirmed = true; deadline.update(); } };
}
test('6.3s connection no longer consumes first-frame budget; frame within next6s succeeds', t => {
  const f = fixture(t); t.mock.timers.tick(6300); assert.deepEqual(f.errors, []);
  f.connect(); t.mock.timers.tick(5900); assert.equal(f.deadline.firstFrame(), true);
  t.mock.timers.tick(30000); assert.deepEqual(f.errors, []);
  assert.deepEqual(f.phases, ['connection-deadline-started', 'first-frame-deadline-started']);
});
test('never-connected transport fails once at12s, repeated update cannot extend', t => {
  const f = fixture(t); t.mock.timers.tick(11999); f.deadline.update(); assert.equal(f.deadline.firstFrame(), false);
  t.mock.timers.tick(1); assert.deepEqual(f.errors, ['connection_timeout']); f.connect(); t.mock.timers.tick(30000); assert.equal(f.errors.length, 1);
});
test('connected transport without a decoded/presented frame fails unchanged6s after both gates', t => {
  const f = fixture(t, true, false); t.mock.timers.tick(3000); assert.equal(f.deadline.firstFrame(), false);
  f.confirm(); t.mock.timers.tick(5999); f.deadline.update(); assert.deepEqual(f.errors, []);
  t.mock.timers.tick(1); assert.deepEqual(f.errors, ['first_frame_timeout']); assert.equal(f.deadline.firstFrame(), false);
});
test('explicit close cancels connection and first-frame stages without late callbacks', t => {
  const f = fixture(t); f.deadline.close(); f.connect(); t.mock.timers.tick(30000); assert.deepEqual(f.errors, []);
  const errors = []; const afterConnection = new ReceptionDeadline({ connected: () => true, confirmed: () => true, onTimeout: e => errors.push(e) });
  afterConnection.close(); t.mock.timers.tick(30000); assert.deepEqual(errors, []);
});
const host = { h264: 'verified-hardware', h265: 'verified-hardware' };
const plan = () => createCodecPlan(host, [{ mimeType: 'video/H265', clockRate: 90000 }, { mimeType: 'video/H264', clockRate: 90000 }]);
test('configured30s attempt permits slow connection and frame; default12s total still aborts', async t => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  for (const budget of [30000, 12000]) {
    let closed = 0;
    const pending = negotiateCodec({ ...plan(), preferred: 'h264' }, async (_, signal) => new Promise((resolve, reject) => {
      let connected = false;
      const gate = new ReceptionDeadline({ connected: () => connected, confirmed: () => true, onTimeout: code => reject(new CodecError(code, code)) });
      signal.addEventListener('abort', () => { gate.close(); closed++; }, { once: true });
      setTimeout(() => { connected = true; gate.update(); }, 6300);
      setTimeout(() => { if (gate.firstFrame()) resolve({ codec: 'video/H264', framesDecoded: 5 }); }, 12200);
    }), undefined, budget);
    const observed = pending.then(value => ({ value }), error => ({ error }));
    t.mock.timers.tick(6300); await flush(); t.mock.timers.tick(5900); await flush();
    const outcome = await observed;
    if (budget === 30000) assert.equal(outcome.value.selected, 'h264'); else assert.equal(outcome.error.code, 'no_working_hardware_codec');
    assert.equal(closed, 1);
  }
});
test('connection timeout cleans preferred attempt before fallback; user cancellation never opens fallback', async t => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const events = []; let gate;
  const pending = negotiateCodec(plan(), async (codec, signal) => {
    events.push('open-' + codec);
    if (codec === 'h264') { assert.deepEqual(events, ['open-h265', 'close-h265', 'open-h264']); return { codec: 'video/H264', framesDecoded: 5 }; }
    return new Promise((_, reject) => {
      gate = new ReceptionDeadline({ connected: () => false, confirmed: () => true, onTimeout: code => reject(new CodecError(code, code)) });
      signal.addEventListener('abort', () => { gate.close(); events.push('close-' + codec); }, { once: true });
    });
  }, undefined, 30000);
  t.mock.timers.tick(12000); await flush(); assert.equal((await pending).selected, 'h264');
  const controller = new AbortController(); let calls = 0, closed = 0;
  const cancelled = negotiateCodec(plan(), async (_, signal) => { calls++; return new Promise(() => {
    const owned = new ReceptionDeadline({ connected: () => false, confirmed: () => true, onTimeout: () => assert.fail('late timeout') });
    signal.addEventListener('abort', () => { owned.close(); closed++; }, { once: true });
  }); }, controller.signal, 30000);
  controller.abort(); await assert.rejects(cancelled, { code: 'cancelled' }); t.mock.timers.tick(30000);
  assert.equal(calls, 1); assert.equal(closed, 1);
});
