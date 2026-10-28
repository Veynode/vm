import assert from 'node:assert/strict'
import { test } from 'node:test'
import { MAX_FPS, changed, refreshHz } from '../src/desktop/refresh.ts'

const gaps = (ms, n = 40) => Array.from({ length: n }, () => ms)

test('a steady animation-frame cadence gives the screen refresh', () => {
  assert.equal(refreshHz(gaps(1000 / 60)), 60)
  assert.equal(refreshHz(gaps(1000 / 120)), 120)
  assert.equal(refreshHz(gaps(1000 / 144)), 144)
})

test('frames a busy page skipped do not halve the rate', () => {
  const mixed = [...gaps(1000 / 120, 28), ...gaps(2000 / 120, 12)]
  assert.equal(refreshHz(mixed), 120)
})

test('too few or invalid samples measure nothing, and the rate stays within 1..240', () => {
  assert.equal(refreshHz(gaps(16, 10)), null)
  assert.equal(refreshHz([...gaps(0, 30), NaN, Infinity]), null)
  assert.equal(refreshHz(gaps(1)), MAX_FPS)
  assert.equal(refreshHz(gaps(5000)), 1)
})

test('rounding jitter is not a change worth sending', () => {
  assert.equal(changed(null, 60), true)
  assert.equal(changed(60, 61), false)
  assert.equal(changed(60, 120), true)
})
