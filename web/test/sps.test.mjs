import assert from 'node:assert/strict'
import { test } from 'node:test'
import { escapeRbsp, rewriteAccessUnit, rewriteSps, unescapeRbsp } from '../src/desktop/sps.ts'

const hex = (s) => Uint8Array.from(Buffer.from(s, 'hex'))
// VideoToolbox H.264 Constrained Baseline 1280x800 SPS (no VUI), and the
// rewrite ffmpeg's trace_headers parses as max_num_reorder_frames = 0,
// max_dec_frame_buffering = 1.
const VT_SPS = '2742c028ab4028032c80'
const REWRITTEN = '2742c028ab4028032d00da08846a'

test('adds a no-reorder VUI to a VideoToolbox SPS', () => {
  assert.equal(Buffer.from(rewriteSps(hex(VT_SPS))).toString('hex'), REWRITTEN)
})

test('leaves an SPS that already declares no reordering', () => {
  assert.equal(rewriteSps(hex(REWRITTEN)), null)
})

test('ignores non-SPS and truncated NAL units', () => {
  assert.equal(rewriteSps(hex('68ce3c80')), null)
  assert.equal(rewriteSps(hex('2742c0')), null)
})

test('rewrites the SPS inside an access unit and caches it', () => {
  const au = hex(`00000001${VT_SPS}0000000168ce3c80000000016588`)
  const cache = new Map()
  const out = rewriteAccessUnit(au, cache)
  assert.equal(Buffer.from(out).toString('hex'), `00000001${REWRITTEN}0000000168ce3c80000000016588`)
  assert.equal(cache.size, 1)
  const delta = hex('00000001419a')
  assert.equal(rewriteAccessUnit(delta, cache), delta)
})

test('emulation prevention round-trips', () => {
  const raw = hex('0000000001000002000003ff')
  assert.deepEqual(unescapeRbsp(escapeRbsp(raw)), raw)
  assert.equal(Buffer.from(escapeRbsp(raw)).toString('hex'), '000003000003010000030200000303ff')
})
