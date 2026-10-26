import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { InputQueue, WheelAccumulator, ModifierTracker, containedBox, surfaceToSource, keyChord, sessionState, sameShape, INPUT_QUEUE_MAX } from '../dist/session-control.js';

// Pure rules of direct interaction. No DOM and no browser: real pointer/keyboard events are not exercised here.
const shape = { source_width: 1000, source_height: 500, source_generation: 4 };
const unscaled = { source_width: 1000, source_height: 500, frame_width: 1000, frame_height: 500, scale_divisor: 1 };
const scaled = { source_width: 2000, source_height: 1000, frame_width: 1000, frame_height: 500, scale_divisor: 2 };
const odd = { source_width: 1921, source_height: 1081, frame_width: 641, frame_height: 361, scale_divisor: 3 };
const key = (name, repeat = false) => ({ kind: 'key', at: 0, keys: [name], repeat });
const scroll = (direction, ticks, x = 5, y = 5) => ({ kind: 'scroll', at: 0, shape, x, y, direction, ticks });

test('contained picture box: exact fit, letterbox and pillarbox', () => {
  assert.deepEqual(containedBox({ left: 50, top: 50, width: 500, height: 250 }, 1000, 500), { left: 50, top: 50, width: 500, height: 250 });
  assert.deepEqual(containedBox({ left: 0, top: 0, width: 800, height: 600 }, 1000, 500), { left: 0, top: 100, width: 800, height: 400 });
  assert.deepEqual(containedBox({ left: 10, top: 20, width: 1200, height: 300 }, 1000, 500), { left: 310, top: 20, width: 600, height: 300 });
  for (const box of [{ left: 0, top: 0, width: 0, height: 10 }, { left: 0, top: 0, width: 10, height: 0 }, { left: 0, top: 0, width: NaN, height: 10 }]) assert.equal(containedBox(box, 1000, 500), null);
});

test('surface mapping: corners reach the source corners, bars and outside are ignored, for unscaled, scaled and partial-block frames', () => {
  for (const [geometry, last] of [[unscaled, [999, 499]], [scaled, [1999, 999]], [odd, [1920, 1080]]]) {
    // Letterbox (bars above and below), pillarbox (bars left and right) and exact fit, at fractional CSS positions.
    for (const box of [{ left: 3.25, top: 7.5, width: 800, height: 900 }, { left: 3.25, top: 7.5, width: 1900, height: 300 }, { left: 0, top: 0, width: geometry.frame_width / 2, height: geometry.frame_height / 2 }]) {
      const painted = containedBox(box, geometry.frame_width, geometry.frame_height), tiny = 1e-6;
      const right = painted.left + painted.width, bottom = painted.top + painted.height;
      assert.deepEqual(surfaceToSource(painted.left + tiny, painted.top + tiny, box, geometry), { x: 0, y: 0 });
      assert.deepEqual(Object.values(surfaceToSource(right - tiny, bottom - tiny, box, geometry)), last);
      assert.deepEqual(Object.values(surfaceToSource(right - tiny, painted.top + tiny, box, geometry)), [last[0], 0]);
      assert.deepEqual(Object.values(surfaceToSource(painted.left + tiny, bottom - tiny, box, geometry)), [0, last[1]]);
      for (const [x, y] of [[painted.left - 0.01, painted.top + 1], [right + 0.01, painted.top + 1], [painted.left + 1, painted.top - 0.01], [painted.left + 1, bottom + 0.01], [box.left - 5, box.top - 5]]) {
        assert.equal(surfaceToSource(x, y, box, geometry), null, 'outside the painted picture');
      }
      // Clamped (drag end only): pinned to the nearest edge pixel, never outside the source.
      assert.deepEqual(Object.values(surfaceToSource(right + 500, bottom + 500, box, geometry, true)), last);
      assert.deepEqual(surfaceToSource(painted.left - 500, painted.top - 500, box, geometry, true), { x: 0, y: 0 });
      // Every position inside maps to a valid source pixel, monotonically.
      let previous = -1;
      for (let step = 0; step < 200; step++) { const point = surfaceToSource(painted.left + (step + 0.5) * painted.width / 200, painted.top + painted.height / 2, box, geometry); assert.ok(point.x >= previous && point.x <= last[0]); previous = point.x; }
    }
  }
  assert.equal(surfaceToSource(NaN, 5, { left: 0, top: 0, width: 100, height: 50 }, unscaled), null);
});

test('surface mapping is independent of devicePixelRatio: only CSS positions and the CSS box are used', () => {
  // The same element at the same CSS box yields the same source pixel whatever the device pixel density is;
  // zoom changes the CSS box and the position together, which leaves the ratio unchanged.
  const at = zoom => surfaceToSource((50 + 123.4) * zoom, (50 + 67.8) * zoom, { left: 50 * zoom, top: 50 * zoom, width: 500 * zoom, height: 250 * zoom }, scaled);
  for (const zoom of [0.5, 1, 1.25, 1.5, 2, 3]) assert.deepEqual(at(zoom), at(1));
  assert.deepEqual(at(1), { x: 493, y: 271 });
});

test('input queue: strict order, bounded, and nothing leaves it except by shift or clear', () => {
  const queue = new InputQueue(); assert.equal(queue.max, INPUT_QUEUE_MAX);
  for (let i = 0; i < 8; i++) assert.equal(queue.push(key(String.fromCharCode(97 + i))), 'queued');
  assert.equal(queue.full, true);
  assert.equal(queue.push(key('z')), 'full'); assert.equal(queue.push({ kind: 'click', at: 0, shape, x: 1, y: 1, button: 'left', second: false }), 'full');
  assert.equal(queue.push(scroll('down', 1)), 'full', 'A scroll that cannot merge is refused like anything else');
  assert.equal(queue.length, 8);
  assert.deepEqual(Array.from({ length: 8 }, () => queue.shift().keys[0]), ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h']);
  assert.equal(queue.shift(), undefined); assert.equal(queue.clear(), 0);
  queue.push(key('a')); queue.push(key('b')); assert.equal(queue.clear(), 2); assert.equal(queue.length, 0);
});

test('input queue merges only into the tail: wheel ticks of one direction and point, and auto-repeat of the waiting chord', () => {
  const queue = new InputQueue(3);
  assert.equal(queue.push(scroll('down', 3)), 'queued'); assert.equal(queue.push(scroll('down', 4)), 'merged'); assert.equal(queue.peek().ticks, 7);
  assert.equal(queue.push(scroll('down', 19)), 'capped'); assert.equal(queue.peek().ticks, 20, 'Never above the protocol limit of 20 ticks');
  assert.equal(queue.push(scroll('up', 1)), 'queued'); assert.equal(queue.push(scroll('up', 1, 6, 5)), 'queued', 'Another point is another action');
  assert.equal(queue.push(scroll('down', 1)), 'full', 'A scroll never merges past a different action: order is kept');
  assert.equal(queue.push(scroll('up', 2, 6, 5)), 'merged', 'A full queue still merges into its tail');
  assert.equal(queue.push({ ...scroll('up', 1, 6, 5), shape: { ...shape, source_generation: 5 } }), 'full', 'Another display generation never merges');
  const keys = new InputQueue(2);
  assert.equal(keys.push(key('a', true)), 'queued', 'A repeat with nothing waiting is a real press');
  assert.equal(keys.push(key('a', true)), 'merged'); assert.equal(keys.push(key('a')), 'queued', 'A deliberate second press is never merged');
  assert.equal(keys.push(key('b', true)), 'full'); assert.equal(keys.push({ kind: 'key', at: 0, keys: ['shift', 'a'], repeat: true }), 'full', 'Another chord is not a repeat');
  assert.equal(keys.length, 2);
  assert.equal(sameShape(shape, { ...shape }), true); assert.equal(sameShape(shape, { ...shape, source_width: 1001 }), false);
});

test('wheel accumulator: whole ticks on the dominant axis, sub-tick deltas accumulate, direction change discards the remainder', () => {
  const wheel = new WheelAccumulator();
  assert.deepEqual(wheel.add(0, 100), { direction: 'down', ticks: 1 }); assert.deepEqual(wheel.add(0, -250), { direction: 'up', ticks: 2 });
  assert.equal(wheel.add(0, 30), null); assert.equal(wheel.add(0, 30), null); assert.deepEqual(wheel.add(0, 45), { direction: 'down', ticks: 1 });
  assert.equal(wheel.add(0, 90), null); assert.equal(wheel.add(0, -90), null, 'Reversal does not inherit the opposite remainder');
  assert.deepEqual(wheel.add(0, -10), { direction: 'up', ticks: 1 });
  assert.deepEqual(wheel.add(-120, 10), { direction: 'left', ticks: 1 }); assert.deepEqual(wheel.add(300, 0), { direction: 'right', ticks: 3 });
  assert.deepEqual(wheel.add(0, 3, 1), { direction: 'down', ticks: 1 }, 'Three lines are one tick'); assert.deepEqual(wheel.add(0, -1, 2), { direction: 'up', ticks: 3 }, 'One page is three ticks');
  assert.deepEqual(wheel.add(0, 1e9), { direction: 'down', ticks: 20 }); assert.equal(wheel.add(0, 0), null); assert.equal(wheel.add(NaN, NaN), null);
  wheel.add(0, 90); wheel.reset(); assert.equal(wheel.add(0, 90), null);
});

test('key chords use only qcodes the controller allows; positions without one are refused', () => {
  const rust = readFileSync(new URL('../../crates/control/src/input.rs', import.meta.url), 'utf8');
  // Drift check against the controller's own allowlist (the body of `key_names`); a-z and 0-9 are generated there.
  const start = rust.indexOf('pub fn key_names'), body = rust.slice(start, rust.indexOf('\n}\n', start));
  assert.ok(start >= 0 && body.includes("b'a'..=b'z'") && body.includes("b'0'..=b'9'"), 'controller key_names not found where expected; update this drift check');
  const allowed = new Set([...'abcdefghijklmnopqrstuvwxyz0123456789', ...[...body.matchAll(/"([a-z0-9_]+)"/g)].map(match => match[1])]);
  assert.ok(allowed.has('grave_accent') && allowed.has('meta_r') && allowed.size === 36 + 46, `controller allowlist parsed (${allowed.size})`);
  const codes = ['Enter', 'Escape', 'Tab', 'Space', 'Backspace', 'Delete', 'Insert', 'Home', 'End', 'PageUp', 'PageDown', 'ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'Minus', 'Equal', 'BracketLeft', 'BracketRight', 'Semicolon', 'Quote', 'Backquote', 'Backslash', 'Comma', 'Period', 'Slash',
    ...Array.from({ length: 26 }, (_, i) => `Key${String.fromCharCode(65 + i)}`), ...Array.from({ length: 10 }, (_, i) => `Digit${i}`), ...Array.from({ length: 12 }, (_, i) => `F${i + 1}`)];
  const produced = new Set();
  for (const code of codes) { const chord = keyChord(code, []); assert.equal(chord.length, 1, code); assert.ok(allowed.has(chord[0]), `${code} -> ${chord[0]}`); produced.add(chord[0]); }
  const tracker = new ModifierTracker();
  for (const code of ['ControlLeft', 'ControlRight', 'AltLeft', 'AltRight', 'ShiftLeft', 'ShiftRight', 'MetaLeft', 'MetaRight']) { assert.equal(tracker.press(code), true); }
  for (const qcode of tracker.held()) { assert.ok(allowed.has(qcode), qcode); produced.add(qcode); }
  assert.deepEqual([...allowed].filter(name => !produced.has(name)), [], 'Every allowed physical key is reachable');
  assert.deepEqual(keyChord('KeyA', ['ctrl', 'shift']), ['ctrl', 'shift', 'a']); assert.deepEqual(keyChord('KeyA', ['ctrl', 'ctrl']), ['ctrl', 'a'], 'No duplicate qcode');
  assert.equal(keyChord('KeyA', tracker.held()), null, 'Eight modifiers plus a key exceed the 8-key chord');
  for (const code of ['CapsLock', 'NumLock', 'Numpad1', 'NumpadEnter', 'PrintScreen', 'IntlBackslash', 'ContextMenu', 'F13', '', 'ShiftLeft']) assert.equal(keyChord(code, []), null, code);
});

test('modifier tracker: chord order, left/right, missed releases, lone taps and clear', () => {
  const tracker = new ModifierTracker();
  assert.equal(tracker.press('KeyA'), false);
  tracker.press('ShiftRight'); tracker.press('ControlLeft');
  assert.deepEqual(tracker.held({ ctrlKey: true, shiftKey: true, altKey: false, metaKey: false }), ['ctrl', 'shift_r'], 'Fixed order: ctrl, alt, shift, meta');
  assert.deepEqual(tracker.held({ ctrlKey: true, shiftKey: false }), ['ctrl'], 'A release missed while focus was elsewhere is dropped');
  assert.deepEqual(tracker.held({ ctrlKey: true, shiftKey: true }), ['ctrl', 'shift'], 'A flag with no tracked key uses the left key');
  assert.deepEqual(tracker.release('ControlLeft'), ['ctrl'], 'Released with nothing since its press: a tap');
  assert.equal(tracker.release('ControlLeft'), null, 'A second release is nothing');
  assert.equal(tracker.press('MetaLeft'), true); assert.equal(tracker.press('MetaLeft', true), true); assert.deepEqual(tracker.release('MetaLeft'), ['meta_l']);
  tracker.press('AltLeft'); tracker.press('ShiftLeft'); assert.deepEqual(tracker.release('ShiftLeft'), ['alt', 'shift'], 'A tap keeps the modifiers still held');
  assert.equal(tracker.release('AltLeft'), null, 'The first modifier of that chord is not tapped again');
  tracker.press('AltLeft'); tracker.press('KeyF'); assert.equal(tracker.release('AltLeft'), null, 'Used in a chord: not a tap');
  tracker.press('AltLeft'); tracker.interrupt(); assert.equal(tracker.release('AltLeft'), null, 'Pointer use in between: not a tap');
  tracker.press('ShiftLeft'); tracker.press('AltRight'); tracker.clear(); assert.deepEqual(tracker.held(), []); assert.equal(tracker.release('ShiftLeft'), null);
});

test('session state: one visible state with a fixed priority and the reason for ending', () => {
  const base = { link: 'live', reason: '', sending: false, waiting: 0, queueMax: 8, inputAllowed: false, frameFresh: false, graceMs: 5000 };
  const of = changes => sessionState({ ...base, ...changes });
  assert.deepEqual(of({ link: 'idle' }), { state: 'idle', text: 'Not connected.' });
  assert.equal(of({ link: 'connecting' }).state, 'connecting'); assert.equal(of({}).state, 'live'); assert.match(of({}).text, /Viewing only/);
  assert.match(of({ inputAllowed: true, frameFresh: true }).text, /Input enabled/);
  assert.equal(of({ inputAllowed: true }).state, 'frame-stale'); assert.equal(of({ frameFresh: false }).state, 'live', 'Without input permission a stale picture is not an input state');
  assert.equal(of({ sending: true, inputAllowed: true }).state, 'input-pending'); assert.match(of({ waiting: 3 }).text, /none sending, 3 waiting\.$/);
  assert.match(of({ sending: true, waiting: 8 }).text, /queue full at 8; further input is refused/);
  assert.equal(of({ link: 'reconnecting', sending: true, waiting: 2 }).state, 'reconnecting'); assert.match(of({ link: 'reconnecting' }).text, /up to 5 seconds/);
  assert.deepEqual(of({ link: 'ended', reason: 'you disconnected.', sending: true }), { state: 'ended', text: 'Ended: you disconnected.' });
  assert.match(of({ link: 'ended' }).text, /^Ended: the session was closed\.$/);
});
