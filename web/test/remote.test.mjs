import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { test } from 'node:test'
import { HopStats, InputToPaint, percentile } from '../src/remote/hop-stats.ts'
import {
  CLOSE,
  LengthReader,
  MAX_FRAGMENT,
  decodeFragment,
  decodePointer,
  decodeRecord,
  decodeVideoV2,
  encodePointer,
  encodeRecord,
  encodeVideoV2,
  fragmentPayload,
  lengthPrefixed,
} from '../src/remote/proto.ts'
import { MAX_FRAMES, Reassembler } from '../src/remote/reassembly.ts'
import { AuthError, KEY_INFO, ReplayError, ReplayWindow, SecureSession, deriveKeys, importAesKey, open, seal } from '../src/remote/secure.ts'
import { LatestWins, attemptOrder, connect } from '../src/remote/transport.ts'
import { MAX_ZOOM, PanZoom, fullView, placement, toSource, viewControl } from '../src/remote/view.ts'

const V = JSON.parse(readFileSync(new URL('../src/remote/vectors.json', import.meta.url), 'utf8'))
const hex = (s) => Uint8Array.from(Buffer.from(s, 'hex'))
const toHex = (b) => Buffer.from(b).toString('hex')

// --- framing ----------------------------------------------------------------

test('records match the shared vectors both ways', () => {
  for (const r of V.records) {
    assert.equal(toHex(encodeRecord(r.kind, hex(r.payload))), r.record, r.name)
    const back = decodeRecord(hex(r.record))
    assert.equal(back.kind, r.kind)
    assert.equal(toHex(back.payload), r.payload)
  }
})

test('records outside bounds or with a wrong length are refused', () => {
  assert.throws(() => decodeRecord(hex('0000000204')), /length mismatch/)
  assert.throws(() => decodeRecord(hex('0000000000')), /outside bound/)
  assert.throws(() => decodeRecord(hex('0080000101')), /outside bound/)
  assert.throws(() => decodeRecord(hex('00000001')), /too short/)
  assert.throws(() => decodeRecord(hex('000000010a')), /unknown record kind/)
  assert.throws(() => encodeRecord(0, new Uint8Array()), /unknown record kind/)
  const json = new Uint8Array(64 * 1024 + 1)
  assert.throws(() => decodeRecord(encodeRecord(4, json)), /JSON record too large/)
  assert.equal(decodeRecord(encodeRecord(8, json)).payload.length, json.length)
})

test('length reader splits a byte stream however it is chunked', () => {
  const units = [hex('01'), hex('020304'), new Uint8Array(300).fill(7)]
  const stream = Buffer.concat(units.map(lengthPrefixed))
  for (const step of [1, 3, 7, stream.length]) {
    const reader = new LengthReader()
    const out = []
    for (let i = 0; i < stream.length; i += step) out.push(...reader.push(stream.subarray(i, i + step)))
    assert.deepEqual(out.map(toHex), units.map(toHex), `step ${step}`)
  }
})

test('video_v2 header with its View encodes and decodes the vector', () => {
  const v = V.video_v2
  const decoded = decodeVideoV2(hex(v.payload))
  assert.deepEqual(
    { ...decoded, au: toHex(decoded.au) },
    {
      epoch: v.epoch,
      frameSeq: v.frame_seq,
      key: v.key,
      spsChanged: v.sps_changed,
      ptsUs: BigInt(v.pts_us),
      encodeUs: v.encode_us,
      hostQueueUs: v.host_queue_us,
      inputAckSeq: v.input_ack_seq,
      view: { x: v.view.x, y: v.view.y, w: v.view.w, h: v.view.h, srcW: v.view.src_w, srcH: v.view.src_h },
      au: v.annex_b,
    },
  )
  assert.equal(toHex(encodeVideoV2(decoded)), v.payload)
  assert.throws(() => decodeVideoV2(new Uint8Array(40)), /too short/)
})

test('pointer and close codes match the vectors', () => {
  const p = V.pointer
  assert.deepEqual(decodePointer(hex(p.payload)), { seq: p.seq, x: p.x, y: p.y, w: p.w, h: p.h })
  assert.equal(toHex(encodePointer(p)), p.payload)
  assert.deepEqual(
    { unauthorized: CLOSE.unauthorized, replaced: CLOSE.replaced, revoked: CLOSE.revoked, rate_limited: CLOSE.rateLimited, invalid: CLOSE.invalid, retry: CLOSE.retry },
    V.close_codes,
  )
})

test('fragmenting a video_v2 payload gives the vector fragments', () => {
  const f = V.fragments
  const frags = fragmentPayload(hex(f.payload), f.epoch, f.frame_seq)
  assert.deepEqual(frags.map(toHex), f.fragments)
  for (const frag of frags) assert.ok(frag.length <= MAX_FRAGMENT)
  assert.throws(() => decodeFragment(hex('00000007000000010002000200')), /outside count/)
  assert.throws(() => decodeFragment(hex('000000070000000100000001')), /bad fragment size/)
  assert.throws(() => decodeFragment(new Uint8Array(MAX_FRAGMENT + 1)), /bad fragment size/)
})

// --- secure records ---------------------------------------------------------

async function vectorKeys() {
  const e = V.ecdh
  const pub = hex(e.browser_public)
  const b64 = (b) => Buffer.from(b).toString('base64url')
  const privateKey = await crypto.subtle.importKey(
    'jwk',
    { kty: 'EC', crv: 'P-256', d: b64(hex(e.browser_scalar)), x: b64(pub.subarray(1, 33)), y: b64(pub.subarray(33)) },
    { name: 'ECDH', namedCurve: 'P-256' },
    false,
    ['deriveBits'],
  )
  return deriveKeys(privateKey, hex(e.host_public), hex(e.transcript_sha256))
}

test('ECDH + HKDF derive the vector keys', async () => {
  assert.equal(toHex(new TextEncoder().encode(KEY_INFO.h2b)), V.ecdh.info_h2b)
  assert.equal(toHex(new TextEncoder().encode(KEY_INFO.b2h)), V.ecdh.info_b2h)
  const keys = await vectorKeys()
  const raw = { h2b: await importAesKey(hex(V.ecdh.k_h2b)), b2h: await importAesKey(hex(V.ecdh.k_b2h)) }
  // Derived keys are not extractable; equal keys open each other's records.
  for (const dir of ['h2b', 'b2h']) {
    const sealed = await seal(keys[dir], dir, 4, 3n, hex('7b7d'))
    assert.equal(toHex((await open(raw[dir], dir, sealed)).plain), '7b7d')
    await assert.rejects(open(raw[dir === 'h2b' ? 'b2h' : 'h2b'], dir, sealed), AuthError)
  }
})

test('sealing reproduces every vector ciphertext, and opening reverses it', async () => {
  const keys = { h2b: await importAesKey(hex(V.ecdh.k_h2b)), b2h: await importAesKey(hex(V.ecdh.k_b2h)) }
  for (const s of V.sealed) {
    const sealed = await seal(keys[s.direction], s.direction, s.kind, BigInt(s.seq), hex(s.plaintext))
    assert.equal(toHex(sealed), s.sealed, s.name)
    assert.equal(toHex(sealed.subarray(0, 9)), s.aad)
    const opened = await open(keys[s.direction], s.direction, hex(s.sealed))
    assert.equal(toHex(opened.plain), s.plaintext)
    assert.equal(opened.kind, s.kind)
  }
})

test('tampering with kind, seq, ciphertext, tag or direction fails authentication', async () => {
  const s = V.sealed[0]
  const key = await importAesKey(hex(V.ecdh.k_h2b))
  for (const at of [0, 8, 9, hex(s.sealed).length - 1]) {
    const bad = hex(s.sealed)
    bad[at] ^= at === 0 ? 3 : 1
    await assert.rejects(open(key, 'h2b', bad), AuthError, `byte ${at}`)
  }
  const unknown = hex(s.sealed)
  unknown[0] = 10
  await assert.rejects(open(key, 'h2b', unknown), /unknown record kind/)
  await assert.rejects(open(key, 'b2h', hex(s.sealed)), AuthError)
})

test('sessions round-trip in both directions with one counter per direction', async () => {
  const keys = await vectorKeys()
  const browser = new SecureSession(keys, 'b2h')
  const host = new SecureSession(keys, 'h2b')
  const r0 = await browser.seal(3, hex('7b7d'))
  const r1 = await browser.seal(9, encodePointer({ seq: 1, x: 1, y: 2, w: 3, h: 4 }))
  assert.equal(toHex(r0.subarray(1, 9)), '0000000000000000')
  assert.equal(toHex(r1.subarray(1, 9)), '0000000000000001')
  assert.deepEqual(await host.open(r0), { kind: 3, seq: 0n, plain: hex('7b7d') })
  assert.equal((await host.open(r1)).kind, 9)
  const video = await host.seal(8, hex(V.video_v2.payload))
  assert.equal(toHex((await browser.open(video)).plain), V.video_v2.payload)
  await assert.rejects(browser.open(r1), AuthError, 'own direction does not open')
})

test('replayed records are refused; reordering inside the window is not; forgeries do not move it', async () => {
  const keys = await vectorKeys()
  const host = new SecureSession(keys, 'h2b')
  const browser = new SecureSession(keys, 'b2h')
  const sealed = []
  for (let i = 0; i < 12; i++) sealed.push(await host.seal(8, hex(V.video_v2.payload)))
  await browser.open(sealed[5])
  await assert.rejects(browser.open(sealed[5]), ReplayError)
  await browser.open(sealed[2])
  await browser.open(sealed[11])
  await assert.rejects(browser.open(sealed[2]), ReplayError)
  const forged = Uint8Array.from(sealed[0])
  forged.set(hex('0000000000ffffff'), 1)
  await assert.rejects(browser.open(forged), AuthError)
  await browser.open(sealed[10])
})

test('replay window: 4096 wide, as secure.rs', () => {
  const w = new ReplayWindow()
  const accept = (seq) => {
    assert.ok(w.check(BigInt(seq)), `accept ${seq}`)
    w.accept(BigInt(seq))
  }
  const reject = (seq, why) => assert.ok(!w.check(BigInt(seq)), why ?? `reject ${seq}`)
  for (const s of [5, 3, 4, 90, 20]) accept(s)
  reject(20)
  accept(4200)
  reject(100, '100 is stale (4199 - 100 >= 4096)')
  accept(200)
  reject(200)
  reject(90, "90's slot was reused, and it is stale anyway")
  // A jump of a whole window or more clears everything.
  accept(20_000)
  accept(20_000 - 4096 + 200)
  reject(20_000 - 4096, 'exactly 4096 behind the highest is outside')
  assert.ok(w.check(BigInt(20_000 - 4095)), '4095 behind is inside')
})

// --- reassembly -------------------------------------------------------------

const frame = (frameSeq, key, size = 2500, epoch = 7) => {
  const au = new Uint8Array(size - 41).fill(frameSeq & 0xff)
  const view = fullView(1920, 1080)
  const record = encodeVideoV2({ epoch, frameSeq, key, spsChanged: false, ptsUs: 0n, encodeUs: 0, hostQueueUs: 0, inputAckSeq: 0, view, au })
  return { record, frags: fragmentPayload(record, epoch, frameSeq).map(decodeFragment) }
}
const seqs = (payloads) => payloads.map((p) => decodeVideoV2(p).frameSeq)

test('reassembles in order and in any fragment order', () => {
  const r = new Reassembler(100)
  const k = frame(1, true)
  const d = frame(2, false)
  assert.equal(k.frags.length, 3)
  const out = []
  for (const f of [k.frags[2], k.frags[0], k.frags[1]]) out.push(...r.push(f, 0).frames)
  for (const f of [d.frags[1], d.frags[2], d.frags[0]]) out.push(...r.push(f, 1).frames)
  assert.deepEqual(out.map(toHex), [k.record, d.record].map(toHex))
  assert.equal(r.stats.delivered, 2)
})

test('out-of-order frames are delivered in frame order', () => {
  const r = new Reassembler(100)
  const [a, b, c] = [frame(1, true), frame(2, false), frame(3, false)]
  const out = []
  for (const f of a.frags) out.push(...r.push(f, 0).frames)
  for (const f of c.frags) out.push(...r.push(f, 1).frames)
  assert.deepEqual(seqs(out), [1])
  for (const f of b.frags) out.push(...r.push(f, 2).frames)
  assert.deepEqual(seqs(out), [1, 2, 3])
})

test('a missing fragment past the deadline drops the frame and later deltas until a key frame', () => {
  const r = new Reassembler(50)
  const out = []
  let needKey = false
  const take = (res) => {
    out.push(...res.frames)
    needKey ||= res.needKey
  }
  for (const f of frame(1, true).frags) take(r.push(f, 0))
  const lost = frame(2, false)
  take(r.push(lost.frags[0], 10))
  take(r.push(lost.frags[2], 10))
  take(r.tick(59))
  assert.equal(needKey, false)
  take(r.tick(60))
  assert.equal(needKey, true)
  needKey = false
  for (const f of frame(3, false).frags) take(r.push(f, 61))
  assert.equal(needKey, true, 'deltas while waiting re-ask')
  for (const f of frame(4, true).frags) take(r.push(f, 62))
  for (const f of frame(5, false).frags) take(r.push(f, 63))
  assert.deepEqual(seqs(out), [1, 4, 5])
  assert.equal(r.stats.lost, 1)
  assert.equal(r.stats.skipped, 1)
  // A fragment of the dropped frame arriving late is ignored.
  take(r.push(lost.frags[1], 64))
  assert.equal(r.stats.late, 1)
})

test('a frame lost entirely is noticed by the gap once the next frame times out', () => {
  const r = new Reassembler(50)
  const out = []
  for (const f of frame(1, true).frags) out.push(...r.push(f, 0).frames)
  for (const f of frame(3, false).frags) out.push(...r.push(f, 5).frames)
  assert.deepEqual(seqs(out), [1])
  assert.equal(r.tick(54).needKey, false)
  assert.equal(r.tick(55).needKey, true)
  assert.deepEqual(seqs(out), [1])
})

test('no more than three frames are buffered; the oldest incomplete is dropped', () => {
  const r = new Reassembler(1000)
  for (const f of frame(1, true).frags) r.push(f, 0)
  let needKey = false
  for (let s = 2; s <= 2 + MAX_FRAMES; s++) needKey ||= r.push(frame(s, false).frags[0], s).needKey
  assert.equal(needKey, true)
  assert.equal(r.stats.lost, 1)
})

test('a complete key frame supersedes incomplete older frames', () => {
  const r = new Reassembler(1000)
  for (const f of frame(1, true).frags) r.push(f, 0)
  r.push(frame(2, false).frags[0], 1)
  const out = []
  for (const f of frame(3, true).frags) out.push(...r.push(f, 2).frames)
  assert.deepEqual(seqs(out), [3])
})

test('a new epoch restarts from its key frame', () => {
  const r = new Reassembler(100)
  const out = []
  for (const f of frame(10, true).frags) out.push(...r.push(f, 0).frames)
  for (const f of frame(1, false, 2500, 8).frags) out.push(...r.push(f, 1).frames)
  for (const f of frame(2, true, 2500, 8).frags) out.push(...r.push(f, 2).frames)
  assert.deepEqual(seqs(out), [10, 2])
  r.push(frame(11, false).frags[0], 3)
  assert.equal(r.stats.late, 1, 'the old epoch is late')
})

// --- transport --------------------------------------------------------------

class MemoryStore {
  map = new Map()
  getItem(k) {
    return this.map.get(k) ?? null
  }
  setItem(k, v) {
    this.map.set(k, v)
  }
}

const handlers = { onUnit() {}, onVideoReset() {}, onClose() {} }
const link = (kind) => ({ kind, datagrams: kind === 'wt', send() {}, sendDatagram: () => kind === 'wt', close() {} })

test('transport order is WebTransport, WebSocket, WebRTC; the last success goes first', async () => {
  const store = new MemoryStore()
  const origin = 'https://desk.example'
  assert.deepEqual(attemptOrder(origin, store), ['wt', 'ws', 'webrtc'])
  const tried = []
  const opener = (kind, ok) => async () => {
    tried.push(kind)
    if (!ok) throw new Error(`${kind} refused`)
    return link(kind)
  }
  const { link: l, attempts } = await connect({
    origin,
    store,
    handlers,
    openers: { wt: opener('wt', false), ws: opener('ws', true), webrtc: opener('webrtc', true) },
  })
  assert.equal(l.kind, 'ws')
  assert.deepEqual(tried, ['wt', 'ws'])
  assert.deepEqual(attempts.map((a) => [a.kind, a.ok]), [['wt', false], ['ws', true]])
  assert.deepEqual(attemptOrder(origin, store), ['ws', 'wt', 'webrtc'])
  assert.deepEqual(attemptOrder('https://other.example', store), ['wt', 'ws', 'webrtc'], 'per origin')
})

test('a transport that does not open within its timeout is aborted and the next is tried', async () => {
  const store = new MemoryStore()
  let aborted = false
  let lateClosed = false
  let release
  const { link: l, attempts } = await connect({
    origin: 'https://slow.example',
    store,
    handlers,
    timeoutMs: 20,
    openers: {
      wt: (_o, _h, signal) =>
        new Promise((resolve) => {
          signal.addEventListener('abort', () => (aborted = true))
          release = () => resolve({ ...link('wt'), close: () => (lateClosed = true) })
        }),
      ws: async () => link('ws'),
    },
  })
  assert.equal(l.kind, 'ws')
  assert.equal(aborted, true)
  assert.match(attempts[0].error, /timed out/)
  release()
  await new Promise((r) => setTimeout(r, 0))
  assert.equal(lateClosed, true, 'a late open is closed, not leaked')
})

test('missing openers and broken storage fall through; all failing throws with every attempt', async () => {
  const broken = {
    getItem() {
      throw new Error('SecurityError')
    },
    setItem() {
      throw new Error('QuotaExceededError')
    },
  }
  assert.deepEqual(attemptOrder('https://x.example', broken), ['wt', 'ws', 'webrtc'])
  const { link: l } = await connect({ origin: 'https://x.example', store: broken, handlers, openers: { ws: async () => link('ws') } })
  assert.equal(l.kind, 'ws')
  await assert.rejects(
    connect({ origin: 'https://x.example', store: broken, handlers, openers: { wt: async () => Promise.reject(new Error('no')) } }),
    (e) => e.attempts.map((a) => a.kind).join() === 'wt,ws,webrtc',
  )
})

test('the remembered kind is ignored when it is not a known transport', () => {
  const store = new MemoryStore()
  store.setItem('veynode.remote.transport:https://y.example', 'carrier-pigeon')
  assert.deepEqual(attemptOrder('https://y.example', store), ['wt', 'ws', 'webrtc'])
})

test('pointer moves are latest-wins, one send in flight', async () => {
  const ticks = []
  const sent = []
  let finish
  const lw = new LatestWins(
    (v) => {
      sent.push(v)
      return new Promise((r) => (finish = r))
    },
    (run) => ticks.push(run),
  )
  lw.offer(1)
  lw.offer(2)
  lw.offer(3)
  assert.equal(ticks.length, 1)
  ticks.shift()()
  assert.deepEqual(sent, [3])
  lw.offer(4)
  lw.offer(5)
  assert.equal(ticks.length, 0, 'nothing scheduled while sending')
  finish()
  await new Promise((r) => setTimeout(r, 0))
  assert.equal(ticks.length, 1)
  ticks.shift()()
  assert.deepEqual(sent, [3, 5])
  assert.equal(lw.replaced, 3)
  lw.offer(6)
  assert.equal(lw.take(), 6)
})

// --- hop stats --------------------------------------------------------------

test('hop percentiles over a 10 s window', () => {
  assert.equal(percentile([], 50), null)
  assert.equal(percentile([5, 1, 3, 2, 4], 50), 3)
  assert.equal(percentile(Array.from({ length: 100 }, (_, i) => i + 1), 95), 95)
  const s = new HopStats(0)
  for (let i = 1; i <= 20; i++) s.record('decode', i)
  s.record('decode', -1)
  s.record('paint', Number.NaN)
  assert.equal(s.tick(9_999), undefined)
  const report = s.tick(10_000)
  assert.deepEqual(report.hops.decode, { n: 20, p50: 10, p95: 19 })
  assert.deepEqual(report.hops.paint, { n: 0, p50: null, p95: null })
  assert.equal(s.summary(10_001).hops.decode.n, 0)
  assert.doesNotThrow(() => JSON.stringify(report))
})

test('key → paint completes inputs the painted frame acknowledges', () => {
  const k = new InputToPaint()
  k.input(1, 100)
  k.input(2, 110)
  k.input(3, 120)
  assert.deepEqual(k.painted(0, 130), [])
  assert.deepEqual(k.painted(2, 150), [50, 40])
  assert.deepEqual(k.painted(3, 160), [40])
})

// --- crop view --------------------------------------------------------------

test('input maps through the target view to source pixels', () => {
  const full = fullView(1920, 1080)
  assert.deepEqual(toSource(full, 195, 108, 390, 216), { x: 960, y: 540 })
  const crop = { x: 640, y: 360, w: 960, h: 540, srcW: 1920, srcH: 1080 }
  assert.deepEqual(toSource(crop, 0, 0, 390, 219.375), { x: 640, y: 360 })
  assert.deepEqual(toSource(crop, 195, 109.6875, 390, 219.375), { x: 1120, y: 630 })
  assert.deepEqual(toSource(crop, 9999, -5, 390, 219.375), { x: 1919, y: 347 }, 'clamped to the source, not the crop')
})

test('the canvas is placed so the element shows the target before the cropped frame arrives', () => {
  const full = fullView(1920, 1080)
  const crop = { x: 480, y: 270, w: 960, h: 540, srcW: 1920, srcH: 1080 }
  // A full frame shown for a 2x zoom on the middle: twice the element, shifted by half.
  assert.deepEqual(placement(full, crop, 400, 225), { x: -200, y: -112.5, w: 800, h: 450 })
  assert.deepEqual(placement(crop, crop, 400, 225), { x: 0, y: 0, w: 400, h: 225 })
})

test('view control payloads', () => {
  assert.deepEqual(viewControl(fullView(1920, 1080), 1170, 658), { view: null })
  assert.deepEqual(viewControl({ x: 640, y: 360, w: 960, h: 540, srcW: 1920, srcH: 1080 }, 1170.4, 658.2), {
    view: { x: 640, y: 360, w: 960, h: 540, out_w: 1170, out_h: 658 },
  })
  assert.deepEqual(JSON.parse(Buffer.from(V.records[2].payload, 'hex')), { view: { x: 640, y: 360, w: 960, h: 540, out_w: 1170, out_h: 658 } })
})

function panZoom() {
  const sent = []
  const timers = []
  const pz = new PanZoom(1920, 1080, {
    send: (c) => sent.push(c),
    setTimer: (run, ms) => (timers.push({ run, ms }), timers.length),
    clearTimer: (h) => (timers[h - 1].run = null),
  })
  const fire = () => {
    for (const t of timers.splice(0)) t.run?.()
  }
  pz.setOutput(390, 219.375, 3)
  return { pz, sent, timers, fire }
}

test('pinch zoom keeps the point under the fingers and sends one debounced view', () => {
  const { pz, sent, timers, fire } = panZoom()
  pz.zoomAt(1.5, 0.5, 0.5)
  pz.zoomAt(4 / 3, 0.5, 0.5)
  assert.deepEqual(pz.target, { x: 480, y: 270, w: 960, h: 540, srcW: 1920, srcH: 1080 })
  assert.equal(timers.filter((t) => t.run).length, 1)
  assert.equal(timers[0].ms, 150)
  assert.deepEqual(sent, [])
  fire()
  assert.deepEqual(sent, [{ view: { x: 480, y: 270, w: 960, h: 540, out_w: 1170, out_h: 658 } }])
  // The point under (0.25, 0.75) stays put through a zoom there.
  const before = pz.pointer(97.5, 164.53125, 390, 219.375)
  pz.zoomAt(2, 0.25, 0.75)
  assert.deepEqual(pz.pointer(97.5, 164.53125, 390, 219.375), before)
  assert.equal(before.w, 1920, 'pointer w/h stay the source size')
})

test('zoom and pan stay inside the source; reset sends view null once', () => {
  const { pz, sent, fire } = panZoom()
  pz.zoomAt(100, 0, 0)
  assert.equal(pz.target.w, 1920 / MAX_ZOOM)
  pz.panBy(1, 1)
  assert.deepEqual([pz.target.x, pz.target.y], [0, 0])
  pz.panBy(-100, -100)
  assert.deepEqual([pz.target.x, pz.target.y], [1920 - 240, 1080 - 135])
  fire()
  pz.reset()
  fire()
  pz.reset()
  fire()
  assert.deepEqual(sent.at(-1), { view: null })
  assert.equal(sent.length, 2)
})

test('a frame at a new source size drops the zoom', () => {
  const { pz, sent, fire } = panZoom()
  pz.zoomAt(2, 0.5, 0.5)
  fire()
  pz.painted({ x: 0, y: 0, w: 1280, h: 800, srcW: 1280, srcH: 800 })
  assert.deepEqual(pz.target, fullView(1280, 800))
  fire()
  assert.equal(sent.length, 1, 'already full: nothing more to ask')
  pz.painted({ x: 10, y: 10, w: 100, h: 100, srcW: 1440, srcH: 900 })
  fire()
  assert.deepEqual(sent.at(-1), { view: null })
})
