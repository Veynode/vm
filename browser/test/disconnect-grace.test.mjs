import test from 'node:test';
import assert from 'node:assert/strict';
import { DisconnectGrace, DISCONNECT_GRACE_MS } from '../dist/codec-policy.js';
function fixture(t, enable = true) {
  if (enable) t.mock.timers.enable({ apis: ['setTimeout'] });
  const lost = []; return { lost, grace: new DisconnectGrace(reason => lost.push(reason)) };
}
test('disconnected is terminal exactly once after 5 s without reconnect, not before', t => {
  assert.equal(DISCONNECT_GRACE_MS, 5000);
  const f = fixture(t); f.grace.update('connected'); f.grace.update('disconnected');
  assert.deepEqual(f.lost, []); assert.equal(f.grace.pending, true);
  t.mock.timers.tick(4999); assert.deepEqual(f.lost, []);
  t.mock.timers.tick(1); assert.deepEqual(f.lost, ['disconnected_grace_expired']); assert.equal(f.grace.pending, false);
  f.grace.update('connected'); f.grace.update('disconnected'); f.grace.update('failed'); t.mock.timers.tick(60000);
  assert.equal(f.lost.length, 1, 'Nothing is reported after the terminal outcome');
});
test('returning to connected cancels the grace; a later disconnect gets a fresh 5 s', t => {
  const f = fixture(t); f.grace.update('disconnected'); t.mock.timers.tick(4999);
  f.grace.update('connected'); assert.equal(f.grace.pending, false);
  t.mock.timers.tick(60000); assert.deepEqual(f.lost, []);
  f.grace.update('disconnected'); t.mock.timers.tick(4999); assert.deepEqual(f.lost, []);
  t.mock.timers.tick(1); assert.deepEqual(f.lost, ['disconnected_grace_expired']);
});
test('repeated disconnected and non-connected states never extend the grace', t => {
  const f = fixture(t); f.grace.update('disconnected'); t.mock.timers.tick(3000);
  f.grace.update('disconnected'); f.grace.update('connecting'); f.grace.update('new');
  assert.equal(f.grace.pending, true); t.mock.timers.tick(1999); assert.deepEqual(f.lost, []);
  t.mock.timers.tick(1); assert.deepEqual(f.lost, ['disconnected_grace_expired']);
});
for (const state of ['failed', 'closed']) test(`${state} is terminal at once, also during the grace, and cancels its timer`, t => {
  const immediate = fixture(t); immediate.grace.update(state); assert.deepEqual(immediate.lost, [state]);
  const during = fixture(t, false); during.grace.update('disconnected'); t.mock.timers.tick(1000); during.grace.update(state);
  assert.deepEqual(during.lost, [state]); assert.equal(during.grace.pending, false);
  t.mock.timers.tick(60000); assert.deepEqual(during.lost, [state]);
});
test('close cancels a pending grace without a late callback', t => {
  const f = fixture(t); f.grace.update('disconnected'); f.grace.close();
  t.mock.timers.tick(60000); f.grace.update('disconnected'); t.mock.timers.tick(60000); assert.deepEqual(f.lost, []);
});
