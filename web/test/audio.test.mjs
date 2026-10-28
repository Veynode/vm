import assert from 'node:assert/strict'
import { test } from 'node:test'
import { MicChunker, PcmRing, decodePcm, pcmBase64 } from '../src/desktop/audio.ts'

const ramp = (n, from = 0) => Float32Array.from({ length: n }, (_, i) => (from + i) / 1000)

test('the jitter buffer primes to its target, plays in order and re-primes after running dry', () => {
  const ring = new PcmRing(64, 8)
  const left = new Float32Array(4)
  const right = new Float32Array(4)
  ring.push(ramp(6), ramp(6))
  ring.pull(left, right)
  assert.deepEqual([...left], [0, 0, 0, 0], 'silent until the target is buffered')
  ring.push(ramp(4, 6), ramp(4, 6))
  ring.pull(left, right)
  assert.deepEqual([...left], [...ramp(4)])
  ring.pull(left, right)
  ring.pull(left, right)
  assert.deepEqual([...left], [...ramp(2, 8), 0, 0], 'underrun pads with silence')
  ring.push(ramp(4, 20), ramp(4, 20))
  ring.pull(left, right)
  assert.deepEqual([...left], [0, 0, 0, 0], 'primes again after running dry')
})

test('a backlog skips back to the target instead of adding delay', () => {
  const ring = new PcmRing(1000, 10)
  ring.push(ramp(41), ramp(41))
  assert.equal(ring.size, 10)
  const left = new Float32Array(10)
  ring.pull(left, new Float32Array(10))
  assert.deepEqual([...left], [...ramp(10, 31)], 'the newest samples remain')
})

test('the microphone resamples to 48 kHz frames of 20 ms across calls', () => {
  const same = new MicChunker(48000)
  const frames = [...same.push(new Float32Array(500).fill(0.5)), ...same.push(new Float32Array(500).fill(0.5))]
  assert.equal(frames.length, 1)
  assert.equal(frames[0].length, 960)
  assert.ok(frames[0].every((v) => v === 16384))
  // 44.1 kHz in: 1 s of input is 50 frames of 48 kHz, give or take the last partial frame.
  const chunker = new MicChunker(44100)
  let count = 0
  for (let i = 0; i < 345; i++) count += chunker.push(new Float32Array(128).fill(-1)).length
  assert.ok(count >= 49 && count <= 50, `${count} frames`)
  assert.equal(new MicChunker(48000).push(new Float32Array(2000).fill(2))[0][0], 32767, 'clipped')
})

test('speaker records decode to per-channel floats and reject bad formats', () => {
  const body = new Uint8Array([0, 0, 0xbb, 0x80, 2, 0x00, 0x40, 0x00, 0xc0])
  const pcm = decodePcm(body)
  assert.equal(pcm.rate, 48000)
  assert.deepEqual([...pcm.left], [0.5])
  assert.deepEqual([...pcm.right], [-0.5])
  const mono = decodePcm(new Uint8Array([0, 0, 0xbb, 0x80, 1, 0x00, 0x40]))
  assert.equal(mono.left, mono.right)
  for (const bad of [
    [0, 0, 0xbb, 0x80, 3, 0, 0, 0, 0, 0, 0],
    [0, 0, 0xbb, 0x80, 2, 0, 0, 0],
    [0, 0, 0x0f, 0xa0, 1, 0, 0],
    [0, 0, 0xbb],
  ])
    assert.equal(decodePcm(new Uint8Array(bad)), null)
})

test('microphone samples are little-endian base64', () => {
  assert.equal(pcmBase64(Int16Array.from([1, -2])), Buffer.from([1, 0, 0xfe, 0xff]).toString('base64'))
})

test('the worklet classes stand alone as source text', () => {
  // The worklet module is built from their source; an outside reference would fail there.
  const Ring = new Function(`return (${PcmRing})`)()
  const ring = new Ring(16, 2)
  ring.push(ramp(2), ramp(2))
  const out = new Float32Array(2)
  ring.pull(out, new Float32Array(2))
  assert.deepEqual([...out], [...ramp(2)])
  const Chunker = new Function(`return (${MicChunker})`)()
  assert.equal(new Chunker(48000).push(new Float32Array(961)).length, 1)
})
