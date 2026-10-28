import assert from 'node:assert/strict'
import { test } from 'node:test'
import { BitrateGovernor, FrameReader, LinkDelay, RecordReader, acceptKey, frame } from '../desktop.mjs'

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
  big.writeBigUInt64BE(1n << 22n, 2)
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

test('a viewer that keeps up is stepped up every 5 s and a queue sends it back to 8 Mbps', () => {
  const governor = new BitrateGovernor(0)
  assert.equal(governor.observe(4999, 0), null)
  assert.equal(governor.observe(5000, 0), 12_000_000)
  assert.equal(governor.observe(9999, 0), null)
  assert.equal(governor.observe(10_000, 0), 16_000_000)
  assert.equal(governor.observe(15_000, 0), 24_000_000)
  assert.equal(governor.observe(60_000, 0), null)
  assert.equal(governor.observe(61_000, 512 * 1024), 8_000_000)
  assert.equal(governor.observe(61_500, 512 * 1024), null)
})

test('after a back-off the wait before the next step up doubles, to a minute', () => {
  const governor = new BitrateGovernor(0)
  let now = 5000
  assert.equal(governor.observe(now, 0), 12_000_000)
  const waits = []
  for (let i = 0; i < 6; i++) {
    assert.equal(governor.observe(now, 1 << 20), 8_000_000)
    const backedOff = now
    for (now += 1000; governor.observe(now, 0) === null; now += 1000);
    waits.push(now - backedOff)
  }
  assert.deepEqual(waits, [10_000, 20_000, 40_000, 60_000, 60_000, 60_000])
})

test('a queue at the default rate within a second of a change does nothing and small queues are not pressure', () => {
  const governor = new BitrateGovernor(0)
  assert.equal(governor.observe(999, 1 << 20), null)
  assert.equal(governor.observe(5998, 100 * 1024), null)
  assert.equal(governor.observe(5999, 100 * 1024), 12_000_000)
})

test('link delay is the lower of the last two ping round trips over the shortest recent one', () => {
  const link = new LinkDelay()
  assert.equal(link.delay(0), 0)
  for (const [sent, got] of [[0, 2], [250, 252], [500, 502]]) link.pong(got, link.ping(sent))
  assert.equal(link.delay(600), 0)
  link.pong(800, link.ping(500 + 250))
  assert.equal(link.delay(800), 0, 'one slow round trip is jitter')
  link.pong(1060, link.ping(1000))
  assert.equal(link.delay(1060), 48, 'two in a row are a queue')
  link.pong(1262, link.ping(1250))
  assert.equal(link.delay(1262), 10)
  link.pong(1512, link.ping(1500))
  assert.equal(link.delay(1512), 10)
})

test('an unanswered ping counts at its age, and pongs for unknown pings are ignored', () => {
  const link = new LinkDelay()
  link.pong(2, link.ping(0))
  link.ping(250)
  assert.equal(link.delay(300), 48)
  assert.equal(link.delay(1250), 998)
  link.pong(1300, Buffer.from('99'))
  assert.equal(link.latest, 2)
})

test('a slow link steps a viewer down below 8 Mbps, at most every 1 s, and back up once it keeps up', () => {
  const governor = new BitrateGovernor(0)
  assert.equal(governor.observe(5000, 0, 41), 6_000_000)
  assert.equal(governor.observe(5500, 0, 41), null)
  assert.equal(governor.observe(7000, 0, 200), 4_000_000)
  assert.equal(governor.observe(9000, 300 * 1024, 200), 2_000_000)
  assert.equal(governor.observe(11_000, 0, 41), null)
  assert.equal(governor.observe(15_000, 0, 39), null)
  assert.equal(governor.observe(16_000, 0, 39), 4_000_000)
  assert.equal(governor.observe(21_000, 0, 39), 6_000_000)
})

test('a long jittery link with no queue is not slowed down', () => {
  const link = new LinkDelay()
  const governor = new BitrateGovernor(0)
  const rtts = [100, 160, 100, 95, 150, 100, 170, 100, 100, 155]
  const targets = []
  for (let i = 0; i < 200; i += 1) {
    const sent = i * 250
    link.pong(sent + rtts[i % rtts.length], link.ping(sent))
    targets.push(governor.observe(sent + 250, 0, link.delay(sent + 250)))
  }
  assert.ok(!targets.some((t) => t !== null && t < 8_000_000), 'never below 8 Mbps')
  assert.ok(targets.includes(24_000_000), 'still climbs')
})

test('a deep delay drops a viewer two levels at once', () => {
  const governor = new BitrateGovernor(0)
  assert.equal(governor.observe(1000, 0, 400), 4_000_000)
  assert.equal(governor.observe(2000, 0, 400), 1_000_000)
  assert.equal(governor.observe(3000, 0, 400), null)
})

test('a viewer whose backlog is draining is not stepped down again', () => {
  const governor = new BitrateGovernor(0)
  assert.equal(governor.observe(1000, 0, 300), 4_000_000)
  assert.equal(governor.observe(2500, 0, 200), null, 'delay falling: wait')
  assert.equal(governor.observe(3500, 0, 290), 1_000_000, 'delay not falling: go on')
})
