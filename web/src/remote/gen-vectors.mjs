// Regenerates vectors.json: `node web/src/remote/gen-vectors.mjs web/src/remote/vectors.json`.
// node:crypto only, independent of both the TypeScript (web/src/remote) and
// the Rust (crates/remote-proto) code; both must reproduce every byte.
import crypto from 'node:crypto'
import fs from 'node:fs'
const hex = (b) => Buffer.from(b).toString('hex')
const be32 = (n) => { const b = Buffer.alloc(4); b.writeUInt32BE(n >>> 0); return b }
const be16 = (n) => { const b = Buffer.alloc(2); b.writeUInt16BE(n); return b }
const be64 = (n) => { const b = Buffer.alloc(8); b.writeBigUInt64BE(BigInt(n)); return b }
const i16 = (n) => { const b = Buffer.alloc(2); b.writeInt16BE(n); return b }
const record = (kind, payload) => Buffer.concat([be32(payload.length + 1), Buffer.from([kind]), payload])

const bd = crypto.createHash('sha256').update('veynode remote vector browser ephemeral').digest()
const hd = crypto.createHash('sha256').update('veynode remote vector host ephemeral').digest()
const b = crypto.createECDH('prime256v1'); b.setPrivateKey(bd)
const h = crypto.createECDH('prime256v1'); h.setPrivateKey(hd)
const shared = b.computeSecret(h.getPublicKey())
if (!shared.equals(h.computeSecret(b.getPublicKey()))) throw new Error('ecdh mismatch')
const transcript = crypto.createHash('sha256').update('veynode remote vector transcript').digest()
const INFO = { h2b: 'Veynode/remote-key/v1\0h2b', b2h: 'Veynode/remote-key/v1\0b2h' }
const k = (info) => Buffer.from(crypto.hkdfSync('sha256', shared, transcript, Buffer.from(info), 16))
const keys = { h2b: k(INFO.h2b), b2h: k(INFO.b2h) }

const DIR = { h2b: 0, b2h: 1 }
const seal = (name, direction, kind, seq, plaintext) => {
  const nonce = Buffer.concat([be32(DIR[direction]), be64(seq)])
  const aad = Buffer.concat([Buffer.from([kind]), be64(seq)])
  const c = crypto.createCipheriv('aes-128-gcm', keys[direction], nonce)
  c.setAAD(aad)
  const ct = Buffer.concat([c.update(plaintext), c.final(), c.getAuthTag()])
  return { name, direction, kind, seq: String(seq), plaintext: hex(plaintext), nonce: hex(nonce), aad: hex(aad), sealed: hex(Buffer.concat([aad, ct])) }
}

const view = (x, y, w, h, sw, sh) => Buffer.concat([be16(x), be16(y), be16(w), be16(h), be16(sw), be16(sh)])
const au = Buffer.from('00000001674d0028ab40280000000168ee3c8000000001658884', 'hex')
const v2 = Buffer.concat([be32(7), be32(1234), Buffer.from([3]), be64(16_666_667n), be32(4100), be32(250), be32(42), view(640, 360, 960, 540, 1920, 1080), au])
const pointer = Buffer.concat([be32(99), i16(-5), i16(812), be16(1920), be16(1080)])
const event = Buffer.from(JSON.stringify({ event: 'ready', vm: 'dev', backend: 'qemu', codec: 'hevc', w: 1920, h: 1080, epoch: 7 }))
const input = Buffer.from(JSON.stringify({ seq: 5, events: [{ t: 'key', code: 'KeyA', down: true }] }))
const control = Buffer.from(JSON.stringify({ view: { x: 640, y: 360, w: 960, h: 540, out_w: 1170, out_h: 658 } }))

// A 2500-byte video_v2 payload in fragments of at most 1088 data bytes.
const bigAu = Buffer.alloc(2500 - 41); for (let i = 0; i < bigAu.length; i++) bigAu[i] = i & 0xff
const bigPayload = Buffer.concat([be32(7), be32(1235), Buffer.from([0]), be64(33_333_333n), be32(3900), be32(200), be32(42), view(0, 0, 1920, 1080, 1920, 1080), bigAu])
const MAX_DATA = 1100 - 12
const frags = []
for (let i = 0, n = Math.ceil(bigPayload.length / MAX_DATA); i < n; i++)
  frags.push(Buffer.concat([be32(7), be32(1235), be16(i), be16(n), bigPayload.subarray(i * MAX_DATA, (i + 1) * MAX_DATA)]))

const out = {
  _about: 'Shared byte vectors for web/src/remote (TypeScript) and crates/remote-proto (Rust); regenerate with gen-vectors.mjs. All integers big-endian; every hex string must be reproduced exactly.',
  conventions: {
    record: 'u32 len | u8 kind | payload, len = 1 + payload length; payload <= 4 MiB + 64, JSON kinds (all but 2, 8, 9) <= 64 KiB',
    secure_record: 'u8 kind | u64 seq | AES-128-GCM(payload) | 16-byte tag; the plaintext is the record payload (a fragment on the datagram video path)',
    nonce: 'u32 direction | u64 seq, direction h2b = 0, b2h = 1; one seq space per direction from 0',
    aad: 'u8 kind | u64 seq (the first 9 bytes of the secure record)',
    keys: 'shared = P-256 ECDH x-coordinate (32 bytes); k = HKDF-SHA256(ikm = shared, salt = transcript sha256, info = "Veynode/remote-key/v1\\0h2b" | "Veynode/remote-key/v1\\0b2h", L = 16)',
    video_v2: 'u32 epoch | u32 frame_seq | u8 flags (1 key, 2 sps_changed) | u64 pts_us | u32 encode_us | u32 host_queue_us | u32 input_ack_seq | u16 x, y, w, h, src_w, src_h | Annex B (41-byte header)',
    pointer: 'u32 seq | i16 x | i16 y | u16 w | u16 h',
    fragment: 'u32 epoch | u32 frame_seq | u16 idx | u16 count | data <= 1088; fragments of one frame concatenate to its video_v2 payload',
  },
  close_codes: { unauthorized: 4001, replaced: 4002, revoked: 4003, rate_limited: 4008, invalid: 4009, retry: 4503 },
  ecdh: {
    browser_scalar: hex(bd), browser_public: hex(b.getPublicKey()),
    host_scalar: hex(hd), host_public: hex(h.getPublicKey()),
    shared: hex(shared), transcript_sha256: hex(transcript),
    info_h2b: hex(Buffer.from(INFO.h2b)), info_b2h: hex(Buffer.from(INFO.b2h)),
    k_h2b: hex(keys.h2b), k_b2h: hex(keys.b2h),
  },
  records: [
    { name: 'event ready', kind: 1, payload: hex(event), record: hex(record(1, event)) },
    { name: 'input', kind: 3, payload: hex(input), record: hex(record(3, input)) },
    { name: 'control view', kind: 4, payload: hex(control), record: hex(record(4, control)) },
    { name: 'video_v2', kind: 8, payload: hex(v2), record: hex(record(8, v2)) },
    { name: 'pointer', kind: 9, payload: hex(pointer), record: hex(record(9, pointer)) },
  ],
  video_v2: {
    payload: hex(v2), epoch: 7, frame_seq: 1234, key: true, sps_changed: true, pts_us: '16666667', encode_us: 4100, host_queue_us: 250, input_ack_seq: 42,
    view: { x: 640, y: 360, w: 960, h: 540, src_w: 1920, src_h: 1080 }, annex_b: hex(au),
  },
  pointer: { payload: hex(pointer), seq: 99, x: -5, y: 812, w: 1920, h: 1080 },
  sealed: [
    seal('h2b event, seq 0', 'h2b', 1, 0n, event),
    seal('h2b video_v2, seq 1', 'h2b', 8, 1n, v2),
    seal('b2h input, seq 5', 'b2h', 3, 5n, input),
    seal('b2h pointer, large seq', 'b2h', 9, 0xfffffffffn, pointer),
    seal('h2b fragment 0 of the fragments vector', 'h2b', 8, 2n, frags[0]),
  ],
  fragments: { epoch: 7, frame_seq: 1235, payload: hex(bigPayload), fragments: frags.map(hex) },
}
fs.writeFileSync(process.argv[2], JSON.stringify(out, null, 2) + '\n')
