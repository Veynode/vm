// Remote desktop handshake, unit framing and host message shapes against the
// Rust contract (crates/session-auth/src/remote.rs, crates/media-local/src/
// remote/handshake.rs). The mock host below is written from that contract
// with node:crypto for ECDH/HKDF/AES, independently of web/src/remote.
import assert from 'node:assert/strict'
import crypto from 'node:crypto'
import { registerHooks } from 'node:module'
import { test } from 'node:test'

// web/src/remote imports siblings without an extension, as Vite and tsc
// expect; resolve those to .ts for Node's type stripping.
registerHooks({
  resolve(specifier, context, next) {
    if (/^\.\.?\//.test(specifier) && !/\.[cm]?[jt]sx?$/.test(specifier) && context.parentURL?.includes('/src/remote/'))
      return next(`${specifier}.ts`, context)
    return next(specifier, context)
  },
})
const hs = await import('../src/remote/handshake.ts')
const { classifyUnit, encodeRecord, CLOSE } = await import('../src/remote/proto.ts')
const { SecureSession } = await import('../src/remote/secure.ts')
const msg = await import('../src/remote/messages.ts')

const hex = (b) => Buffer.from(b).toString('hex')
const ORIGIN = 'https://desk.example.com'
const NOW = 1_790_000_000

// --- an independent Canon, straight from session-auth's Rust -----------------

function canon(domain, parts) {
  const out = [Buffer.from(`Veynode/${domain}/v1\0`)]
  const text = (s) => {
    const b = Buffer.from(s, 'utf8')
    const n = Buffer.alloc(4)
    n.writeUInt32BE(b.length)
    out.push(n, b)
  }
  for (const [type, v] of parts) {
    if (type === 'text') text(v)
    else if (type === 'int') {
      const n = Buffer.alloc(8)
      n.writeBigUInt64BE(BigInt(v))
      out.push(n)
    } else {
      const n = Buffer.alloc(4)
      n.writeUInt32BE(v.length)
      out.push(n)
      v.forEach(text)
    }
  }
  return Buffer.concat(out)
}
const challengeParts = (c) => [
  ['text', c.host_id],
  ['text', c.device_id],
  ['text', c.vm_id],
  ['list', c.scopes],
  ['int', c.grant_version],
  ['text', c.instance_nonce],
  ['text', c.nonce],
  ['int', c.issued_at],
  ['int', c.expires_at],
]
const intentParts = (i) => [...challengeParts(i.challenge), ['text', i.request_id], ['list', i.codecs], ['text', i.browser_ecdh], ['text', i.transport], ['text', i.origin]]

// --- keys -------------------------------------------------------------------

async function ecdsaPair() {
  const keys = await crypto.webcrypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, false, ['sign', 'verify'])
  const publicKey = hex(await crypto.webcrypto.subtle.exportKey('raw', keys.publicKey))
  const sign = async (bytes) => hex(await crypto.webcrypto.subtle.sign({ name: 'ECDSA', hash: 'SHA-256' }, keys.privateKey, bytes))
  const verify = async (bytes, sig) => crypto.webcrypto.subtle.verify({ name: 'ECDSA', hash: 'SHA-256' }, keys.publicKey, Buffer.from(sig, 'hex'), bytes)
  const id = crypto.createHash('sha256').update(Buffer.concat([Buffer.from('Veynode/identity/v1\0'), Buffer.from(publicKey, 'hex')])).digest('hex')
  return { publicKey, id, sign, verify }
}

/** A host following handshake.rs and remote.rs, with node:crypto record keys. */
async function mockHost({ tamper } = {}) {
  const key = await ecdsaPair()
  const ecdh = crypto.createECDH('prime256v1')
  ecdh.generateKeys()
  const host = {
    key,
    challenge: undefined,
    keys: undefined,
    digest: undefined,
    async step(unit, device) {
      const u = classifyUnit(unit)
      assert.equal(u.type, 'handshake')
      const m = u.message
      if (m.t === 'hello') {
        assert.deepEqual(Object.keys(m).sort(), ['device_id', 'proto', 'scopes', 't', 'vm_id'])
        assert.equal(m.proto, 1)
        host.challenge = {
          host_id: key.id,
          device_id: m.device_id,
          vm_id: m.vm_id,
          scopes: m.scopes,
          grant_version: 3,
          instance_nonce: 'aa'.repeat(32),
          nonce: 'bb'.repeat(32),
          issued_at: NOW,
          expires_at: NOW + 60,
        }
        return { t: 'challenge', challenge: host.challenge }
      }
      if (m.t === 'offer') {
        const { intent, signature } = m.offer
        assert.deepEqual(Object.keys(intent).sort(), ['browser_ecdh', 'challenge', 'codecs', 'origin', 'request_id', 'transport'])
        assert.deepEqual(intent.challenge, host.challenge)
        assert.equal(intent.transport, 'wt')
        assert.equal(intent.origin, ORIGIN)
        assert.ok(await device.verify(canon('remote-offer', intentParts(intent)), signature), 'device signature over the remote-offer canon')
        const transcript = {
          intent: tamper === 'intent' ? { ...intent, origin: 'https://evil.example' } : intent,
          session_id: 'sess-1',
          host_ecdh: hex(ecdh.getPublicKey()),
          codec: intent.codecs.includes('h265') ? 'h265' : 'h264',
          expires_at: NOW + 8 * 3600,
        }
        const bytes = canon('remote-answer', [
          ...intentParts(transcript.intent),
          ['text', transcript.session_id],
          ['text', transcript.host_ecdh],
          ['text', transcript.codec],
          ['int', transcript.expires_at],
        ])
        host.digest = crypto.createHash('sha256').update(bytes).digest()
        const shared = ecdh.computeSecret(Buffer.from(intent.browser_ecdh, 'hex'))
        const k = (info) => Buffer.from(crypto.hkdfSync('sha256', shared, host.digest, Buffer.from(info), 16))
        host.keys = { h2b: k('Veynode/remote-key/v1\0h2b'), b2h: k('Veynode/remote-key/v1\0b2h') }
        let sig = await key.sign(bytes)
        if (tamper === 'signature') sig = (sig[0] === '0' ? '1' : '0') + sig.slice(1)
        return { t: 'answer', answer: { transcript, signature: sig } }
      }
      if (m.t === 'confirm') {
        assert.equal(m.session_id, 'sess-1')
        assert.equal(m.confirmation.transcript_sha256, hex(host.digest))
        assert.ok(await device.verify(canon('remote-confirm', [['text', hex(host.digest)]]), m.confirmation.signature))
        return null
      }
      throw new Error(`unexpected ${m.t}`)
    },
    /** A host → browser secure record sealed with node:crypto. */
    seal(kind, seq, payload) {
      const nonce = Buffer.concat([Buffer.from([0, 0, 0, 0]), Buffer.alloc(8)])
      nonce.writeBigUInt64BE(BigInt(seq), 4)
      const aad = Buffer.alloc(9)
      aad[0] = kind
      aad.writeBigUInt64BE(BigInt(seq), 1)
      const c = crypto.createCipheriv('aes-128-gcm', host.keys.h2b, nonce)
      c.setAAD(aad)
      return Buffer.concat([aad, c.update(payload), c.final(), c.getAuthTag()])
    },
  }
  return host
}

const unit = (message) => {
  const body = Buffer.from(JSON.stringify(message))
  return Buffer.concat([Buffer.from([0x07]), body])
}

async function run({ tamper, options = {} } = {}) {
  const device = await ecdsaPair()
  const host = await mockHost({ tamper })
  const client = new hs.RemoteHandshake({
    identity: device,
    hostPublicKey: host.key.publicKey,
    vmId: 'dev',
    scopes: ['input', 'observe'],
    codecs: ['h265', 'h264'],
    transport: 'wt',
    origin: ORIGIN,
    ...options,
  })
  let reply = await host.step(client.hello(), device)
  const offer = await client.receive(reply)
  reply = await host.step(offer.send, device)
  const answer = await client.receive(reply)
  await host.step(answer.send, device)
  return { host, device, established: answer.established }
}

test('handshake: hello, challenge, offer, answer, confirm agree with the host contract and derive its keys', async () => {
  const { host, established } = await run()
  assert.equal(established.sessionId, 'sess-1')
  assert.equal(established.codec, 'h265')
  assert.equal(established.transcriptSha256, hex(host.digest))
  // The host's first secure record is the session event, under k_h2b.
  const session = new SecureSession(established.keys, 'b2h')
  const event = { event: 'session', session_id: 'sess-1', vm: 'dev', codec: 'h265', expires_at: NOW + 8 * 3600, input: true }
  const opened = await session.open(host.seal(1, 0, Buffer.from(JSON.stringify(event))))
  assert.equal(opened.kind, 1)
  assert.deepEqual(msg.parseHostEvent(JSON.parse(Buffer.from(opened.plain))), event)
  // And what the browser seals under k_b2h opens with the host's key.
  const sealed = await session.seal(4, Buffer.from('{"keyframe_req":true}'))
  const d = crypto.createDecipheriv('aes-128-gcm', host.keys.b2h, Buffer.concat([Buffer.from([0, 0, 0, 1]), sealed.subarray(1, 9)]))
  d.setAAD(sealed.subarray(0, 9))
  d.setAuthTag(sealed.subarray(sealed.length - 16))
  assert.equal(Buffer.concat([d.update(sealed.subarray(9, sealed.length - 16)), d.final()]).toString(), '{"keyframe_req":true}')
})

test('canonical bytes match an independent encoding of the Rust canon', () => {
  const intent = {
    challenge: {
      host_id: '11'.repeat(32),
      device_id: '22'.repeat(32),
      vm_id: 'dev',
      scopes: ['input', 'observe'],
      grant_version: 2,
      instance_nonce: '33'.repeat(32),
      nonce: '44'.repeat(32),
      issued_at: NOW,
      expires_at: NOW + 60,
    },
    request_id: 'req-1',
    codecs: ['h264', 'h265'],
    browser_ecdh: '04' + '55'.repeat(64),
    transport: 'wss',
    origin: 'https://[::1]:8443',
  }
  assert.equal(hex(hs.remoteOfferBytes(intent)), hex(canon('remote-offer', intentParts(intent))))
  const t = { intent, session_id: 'sess-1', host_ecdh: '04' + '66'.repeat(64), codec: 'h264', expires_at: NOW + 3600 }
  assert.equal(
    hex(hs.remoteAnswerBytes(t)),
    hex(canon('remote-answer', [...intentParts(intent), ['text', 'sess-1'], ['text', t.host_ecdh], ['text', 'h264'], ['int', NOW + 3600]])),
  )
  assert.equal(hex(hs.remoteConfirmationBytes('77'.repeat(32))), hex(canon('remote-confirm', [['text', '77'.repeat(32)]])))
  assert.throws(() => hs.remoteOfferBytes({ ...intent, codecs: ['h265', 'h264'] }), /sorted/)
  assert.throws(() => hs.remoteOfferBytes({ ...intent, origin: 'http://desk.example.com' }), /origin/)
  assert.throws(() => hs.remoteOfferBytes({ ...intent, transport: 'webrtc' }), /transport/)
  assert.throws(() => hs.remoteAnswerBytes({ ...t, codec: 'vp9' }), /not offered/)
  assert.throws(() => hs.remoteAnswerBytes({ ...t, expires_at: NOW + 8 * 3600 + 1 }), /lifetime/)
})

test('an answer whose intent was substituted is refused', async () => {
  await assert.rejects(run({ tamper: 'intent' }), /offer intent substituted/)
})

test('an answer with a bad host signature is refused', async () => {
  await assert.rejects(run({ tamper: 'signature' }), /invalid host signature/)
})

test('a challenge for another host, device, VM or scope set is refused before signing', async () => {
  const other = await ecdsaPair()
  await assert.rejects(run({ options: { hostPublicKey: other.publicKey } }), /another host/)
  const device = await ecdsaPair()
  const host = await ecdsaPair()
  let signed = 0
  const identity = { ...device, sign: async (b) => (signed++, device.sign(b)) }
  const good = {
    host_id: host.id,
    device_id: device.id,
    vm_id: 'dev',
    scopes: ['input', 'observe'],
    grant_version: 1,
    instance_nonce: 'aa'.repeat(32),
    nonce: 'bb'.repeat(32),
    issued_at: NOW,
    expires_at: NOW + 60,
  }
  for (const [change, error] of [
    [{ device_id: other.id }, /another device/],
    [{ vm_id: 'prod' }, /another VM/],
    [{ scopes: ['observe'] }, /scopes differ/],
    [{ expires_at: NOW + 61 }, /challenge lifetime/],
  ]) {
    const client = new hs.RemoteHandshake({ identity, hostPublicKey: host.publicKey, vmId: 'dev', scopes: ['input', 'observe'], codecs: ['h264'], transport: 'wt', origin: ORIGIN })
    client.hello()
    await assert.rejects(client.receive({ t: 'challenge', challenge: { ...good, ...change } }), error)
  }
  assert.equal(signed, 0)
})

test('the host refusal carries its close code', async () => {
  const device = await ecdsaPair()
  const client = new hs.RemoteHandshake({
    identity: device,
    hostPublicKey: device.publicKey,
    vmId: 'dev',
    scopes: ['observe'],
    codecs: ['h264'],
    transport: 'wss',
    origin: ORIGIN,
  })
  client.hello()
  await assert.rejects(client.receive({ t: 'error', code: CLOSE.unauthorized, message: 'scope denied' }), (e) => e.code === 4001 && /scope denied/.test(e.message))
  const again = new hs.RemoteHandshake({ identity: device, hostPublicKey: device.publicKey, vmId: 'dev', scopes: ['observe'], codecs: ['h264'], transport: 'wss', origin: ORIGIN })
  again.hello()
  await assert.rejects(again.receive({ t: 'answer', answer: {} }), /unexpected handshake message answer/)
  assert.throws(() => new hs.RemoteHandshake({ identity: device, hostPublicKey: device.publicKey, vmId: 'dev', scopes: ['observe', 'input'], codecs: ['h264'], transport: 'wss', origin: ORIGIN }), /sorted/)
})

test('reliable units are told apart by their first byte', () => {
  assert.deepEqual(classifyUnit(unit({ t: 'challenge' })), { type: 'handshake', message: { t: 'challenge' } })
  const notice = Buffer.concat([Buffer.from([0xff]), Buffer.from('{"gateway":{"p50_us":800,"p95_us":2100,"samples":60}}')])
  assert.deepEqual(classifyUnit(notice), { type: 'gateway', notice: { gateway: { p50_us: 800, p95_us: 2100, samples: 60 } } })
  const record = Buffer.concat([Buffer.from([8]), Buffer.alloc(24)])
  assert.equal(classifyUnit(record).type, 'record')
  assert.equal(classifyUnit(Buffer.from([0x0a, 1])).type, 'invalid')
  assert.equal(classifyUnit(Buffer.from([0xff, 0x7b])).type, 'invalid')
  assert.equal(classifyUnit(new Uint8Array()).type, 'invalid')
  assert.equal(encodeRecord(1, new Uint8Array())[4], 1)
})

test('host messages, pings and capabilities', async () => {
  assert.deepEqual(msg.parseHostEvent({ event: 'close', code: 4003 }), { event: 'close', code: 4003 })
  assert.equal(msg.parseHostEvent({ nope: 1 }), undefined)
  assert.match(msg.closeReason(4003), /revoked/)
  assert.match(msg.closeReason(4009), /invalid/)
  assert.deepEqual(msg.statsMessage({ rtt: 31, loss: 1.5, fps: 60 }), { rtt: 31, loss: 1, fps: 60 })
  assert.deepEqual(msg.pingMessage(3, 1000), { id: 3, t_send_us: 1000 })
  // Sent at 1000 µs, host stamped 50_000 µs, back at 21_000 µs: rtt 20 ms, host 39 ms ahead.
  assert.deepEqual(msg.pongTiming({ id: 3, t_send_us: 1000, t_peer_us: 50_000 }, 21_000), { rttMs: 20, offsetMs: 39 })
  const fetcher = async (url, init) => {
    assert.equal(url, 'https://gw.example/v1/capabilities')
    assert.equal(init.credentials, 'omit')
    return { ok: true, json: async () => ({ protocol: 1, transports: ['wt', 'wss'], video: 'stream', hosts_online: 1 }) }
  }
  const caps = await msg.fetchCapabilities('https://gw.example', fetcher)
  assert.deepEqual(msg.offeredKinds(caps), ['wt', 'ws'])
  await assert.rejects(msg.fetchCapabilities('https://gw.example', async () => ({ ok: false, status: 503 })), /503/)
})
