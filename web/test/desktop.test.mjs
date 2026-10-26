import assert from 'node:assert/strict'
import { test } from 'node:test'
import { FrameReader, RecordReader, acceptKey, frame } from '../desktop.mjs'

test('accept key matches RFC 6455 example', () => {
  assert.equal(acceptKey('dGhlIHNhbXBsZSBub25jZQ=='), 's3pPLMBiTxaQ9kYGzzhZRbK+xOo=')
})

test('server frames use the shortest length encoding', () => {
  assert.deepEqual([...frame(0x1, Buffer.from('hi')).subarray(0, 2)], [0x81, 2])
  const medium = frame(0x2, Buffer.alloc(300))
  assert.deepEqual([...medium.subarray(0, 4)], [0x82, 126, 1, 44])
  const large = frame(0x2, Buffer.alloc(70000))
  assert.equal(large[1], 127)
  assert.equal(large.readBigUInt64BE(2), 70000n)
})

function masked(opcode, payload, fin = true) {
  const mask = Buffer.from([1, 2, 3, 4])
  const body = Buffer.from(payload.map((b, i) => b ^ mask[i & 3]))
  return Buffer.concat([Buffer.from([(fin ? 0x80 : 0) | opcode, 0x80 | payload.length]), mask, body])
}

test('client frames are unmasked, split across chunks and reassembled', () => {
  const reader = new FrameReader()
  const text = masked(0x1, [...Buffer.from('{"events"')], false)
  const rest = masked(0x0, [...Buffer.from(':[]}')])
  const all = Buffer.concat([text, rest])
  assert.deepEqual(reader.push(all.subarray(0, 5)), [])
  const out = reader.push(all.subarray(5))
  assert.equal(out.length, 1)
  assert.equal(out[0].opcode, 0x1)
  assert.equal(out[0].payload.toString(), '{"events":[]}')
})

test('unmasked or oversized client frames are refused', () => {
  assert.throws(() => new FrameReader().push(Buffer.from([0x81, 2, 104, 105])), /masked/)
  const big = Buffer.alloc(10)
  big[0] = 0x82
  big[1] = 0x80 | 127
  big.writeBigUInt64BE(1n << 20n, 2)
  assert.throws(() => new FrameReader().push(big), /too large/)
})

test('relay records are split by length and kind', () => {
  const record = (kind, body) => {
    const head = Buffer.alloc(5)
    head.writeUInt32BE(body.length + 1)
    head[4] = kind
    return Buffer.concat([head, body])
  }
  const reader = new RecordReader()
  const stream = Buffer.concat([record(1, Buffer.from('{}')), record(2, Buffer.from([0, 1, 2]))])
  assert.deepEqual(reader.push(stream.subarray(0, 8)), [{ kind: 1, payload: Buffer.from('{}') }])
  const out = reader.push(stream.subarray(8))
  assert.equal(out.length, 1)
  assert.equal(out[0].kind, 2)
  assert.deepEqual([...out[0].payload], [0, 1, 2])
  assert.throws(() => new RecordReader().push(Buffer.from([0, 0, 0, 0, 1])), /outside bound/)
})
